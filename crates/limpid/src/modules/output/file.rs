//! File output: appends event messages to a local file.
//!
//! Properties:
//!   path   "/var/log/limpid/fw.log"   — required (supports templates)
//!   mode   "0640"                      — octal file permissions (applied on create)
//!   owner  "syslog"                    — file owner (requires CAP_CHOWN)
//!   group  "adm"                       — file group (requires CAP_CHOWN or membership)
//!
//! Dynamic path templates use the DSL's native `${expr}` interpolation,
//! e.g. `path "/var/log/${source}/${strftime(timestamp, "%Y-%m-%d")}.log"`.
//! Any DSL expression works (identifiers, function calls, string concat).
//! Interpolations that dereference `workspace.*` are sanitised to strip
//! `/`, `\`, and `..` so untrusted event data can't escape into sibling
//! directories; other interpolations render verbatim.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::dsl::ast::{Expr, ExprKind, Property, TemplateFragment};
use crate::dsl::eval::{eval_expr, value_to_string};
use crate::dsl::props;
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};

pub struct FileOutput {
    /// Parsed path expression. A plain `Expr::StringLit` means a static
    /// path; `Expr::Template` requires per-event evaluation.
    path: Expr,
    mode: Option<u32>,
    owner: Option<String>,
    group: Option<String>,
    /// Tracks which paths have been created (for applying mode/owner/group once)
    created_paths: Mutex<HashSet<PathBuf>>,
    funcs: Option<Arc<FunctionRegistry>>,
    metrics: Arc<OutputMetrics>,
}

impl Module for FileOutput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let path = props::get_expr(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("output '{}': file requires 'path'", name))?
            .clone();

        // `path` must eventually render to a string. Allow StringLit and
        // Template at config-load time; other shapes (e.g. bare integer)
        // would be a user error so we reject here rather than at write.
        match &path.kind {
            ExprKind::StringLit(_) | ExprKind::Template(_) => {}
            other => anyhow::bail!(
                "output '{}': file 'path' must be a string, got {:?}",
                name,
                other
            ),
        }

        let mode = props::get_string(properties, "mode")
            .map(|s| {
                let s = s.trim_start_matches('0');
                u32::from_str_radix(s, 8).with_context(|| {
                    format!(
                        "output '{}': invalid mode (expected octal, e.g. \"0640\")",
                        name
                    )
                })
            })
            .transpose()?;

        let owner = props::get_string(properties, "owner");
        let group = props::get_string(properties, "group");

        Ok(Self {
            path,
            mode,
            owner,
            group,
            created_paths: Mutex::new(HashSet::new()),
            funcs: None,
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for FileOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for FileOutput {
    fn attach_funcs(&mut self, funcs: Arc<FunctionRegistry>) {
        self.funcs = Some(funcs);
    }

    async fn write(&self, event: &Event) -> Result<()> {
        let (resolved, is_dynamic) = self.render_path(event)?;
        let path = PathBuf::from(&resolved);

        // Defence in depth: reject path traversal components even after
        // per-fragment sanitisation.
        for component in path.components() {
            if matches!(component, std::path::Component::ParentDir) {
                anyhow::bail!("path traversal rejected: {}", resolved);
            }
        }

        // Ensure parent directory exists (needed for dynamic paths)
        if is_dynamic
            && let Some(parent) = path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            tracing::warn!(
                "output file: failed to create directory '{}': {}",
                parent.display(),
                e
            );
        }

        let first_create = {
            let mut guard = self.created_paths.lock().await;
            if !path.exists() && !guard.contains(&path) {
                guard.insert(path.clone());
                true
            } else {
                false
            }
        };

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;

        let msg = String::from_utf8_lossy(&event.egress);
        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(b'\n');
        file.write_all(&buf).await?;
        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);

        if first_create {
            self.apply_file_metadata(&path).await;
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Path rendering
// ---------------------------------------------------------------------------

impl FileOutput {
    /// Render `self.path` against `event`. Returns `(rendered, is_dynamic)`
    /// where `is_dynamic` is true when the template had any interpolated
    /// fragments (used to decide whether to `mkdir -p` the parent).
    ///
    /// For `Template`, each `Interp` fragment is evaluated separately so
    /// that values derived from `workspace.*` can be sanitised without
    /// affecting literal path separators or server-owned interpolations
    /// like `${source}`.
    fn render_path(&self, event: &Event) -> Result<(String, bool)> {
        match &self.path.kind {
            ExprKind::StringLit(s) => Ok((s.clone(), false)),
            ExprKind::Template(fragments) => {
                let funcs = self.funcs.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "output file: FunctionRegistry not attached — \
                         dynamic path template requires attach_funcs() before write"
                    )
                })?;
                let mut out = String::new();
                for frag in fragments {
                    match frag {
                        TemplateFragment::Literal(s) => out.push_str(s),
                        TemplateFragment::Interp(expr) => {
                            let rendered = value_to_string(&eval_expr(expr, event, funcs)?);
                            if is_workspace_reference(expr) {
                                out.push_str(&sanitize_path_component(&rendered));
                            } else {
                                out.push_str(&rendered);
                            }
                        }
                    }
                }
                Ok((out, true))
            }
            other => anyhow::bail!(
                "output file: unsupported path expression shape: {:?}",
                other
            ),
        }
    }
}

/// Does `expr` dereference `workspace.*` (i.e. user-controlled event data)?
/// Conservative: only flags literal `workspace.xxx[.yyy]` identifiers. Other
/// expressions whose results happen to originate from the workspace (e.g.
/// `lower(workspace.host)`) are rendered without sanitisation — users who
/// care should write their sanitisation explicitly.
fn is_workspace_reference(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Ident(parts) => {
            parts.first().is_some_and(|s| s == "workspace") && parts.len() > 1
        }
        ExprKind::PropertyAccess(base, _) => is_workspace_reference(base),
        _ => false,
    }
}

/// Sanitize a path component: strip `/`, `\`, and `..`.
fn sanitize_path_component(s: &str) -> String {
    s.replace(['/', '\\'], "_").replace("..", "_")
}

// ---------------------------------------------------------------------------
// File metadata (permissions / ownership)
// ---------------------------------------------------------------------------

impl FileOutput {
    async fn apply_file_metadata(&self, path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        if let Some(mode) = self.mode
            && let Err(e) =
                tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).await
        {
            tracing::warn!(
                "output file '{}': failed to set mode: {}",
                path.display(),
                e
            );
        }

        if self.owner.is_some() || self.group.is_some() {
            let owner = self.owner.clone();
            let group = self.group.clone();
            let path = path.to_path_buf();

            tokio::task::spawn_blocking(move || {
                let uid = owner.as_deref().and_then(|name| {
                    resolve_uid(name)
                        .inspect_err(|e| {
                            tracing::warn!(
                                "output file '{}': failed to resolve owner '{}': {}",
                                path.display(),
                                name,
                                e
                            );
                        })
                        .ok()
                });
                let gid = group.as_deref().and_then(|name| {
                    resolve_gid(name)
                        .inspect_err(|e| {
                            tracing::warn!(
                                "output file '{}': failed to resolve group '{}': {}",
                                path.display(),
                                name,
                                e
                            );
                        })
                        .ok()
                });
                if (uid.is_some() || gid.is_some())
                    && let Err(e) = std::os::unix::fs::chown(&path, uid, gid)
                {
                    tracing::warn!("output file '{}': failed to chown: {}", path.display(), e);
                }
            })
            .await
            .ok();
        }
    }
}

fn resolve_uid(name: &str) -> Result<u32> {
    use std::ffi::CString;
    let c_name = CString::new(name)?;
    let pw = unsafe { libc::getpwnam(c_name.as_ptr()) };
    if pw.is_null() {
        anyhow::bail!("user '{}' not found", name);
    }
    Ok(unsafe { (*pw).pw_uid })
}

fn resolve_gid(name: &str) -> Result<u32> {
    use std::ffi::CString;
    let c_name = CString::new(name)?;
    let gr = unsafe { libc::getgrnam(c_name.as_ptr()) };
    if gr.is_null() {
        anyhow::bail!("group '{}' not found", name);
    }
    Ok(unsafe { (*gr).gr_gid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::table::TableStore;
    use bytes::Bytes;
    use crate::dsl::value::Value;
    use std::net::SocketAddr;

    fn funcs() -> Arc<FunctionRegistry> {
        let mut reg = FunctionRegistry::new();
        let store = TableStore::from_configs(vec![]).unwrap();
        crate::functions::register_builtins(&mut reg, store);
        Arc::new(reg)
    }

    fn event_with_workspace() -> Event {
        let mut e = Event::new(
            Bytes::from("hello"),
            "192.168.1.10:514".parse::<SocketAddr>().unwrap(),
        );
        e.workspace
            .insert("host".into(), Value::String("web01".into()));
        // value containing a path separator — must be sanitised
        e.workspace
            .insert("ip".into(), Value::String("10.0.0.1/24".into()));
        e
    }

    fn make_output(path: Expr) -> FileOutput {
        FileOutput {
            path,
            mode: None,
            owner: None,
            group: None,
            created_paths: Mutex::new(HashSet::new()),
            funcs: Some(funcs()),
            metrics: Arc::new(OutputMetrics::default()),
        }
    }

    /// Spanless [`Expr`] shortcut — test fixtures aren't anchored to
    /// real source spans.
    fn ek(kind: ExprKind) -> Expr {
        Expr::spanless(kind)
    }

    #[test]
    fn render_static_path() {
        let out = make_output(ek(ExprKind::StringLit("/var/log/app.log".into())));
        let (rendered, dynamic) = out.render_path(&event_with_workspace()).unwrap();
        assert_eq!(rendered, "/var/log/app.log");
        assert!(!dynamic);
    }

    #[test]
    fn render_template_with_ident_interp() {
        // "/var/log/${source}.log"
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("/var/log/".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec!["source".into()]))),
            TemplateFragment::Literal(".log".into()),
        ])));
        let (rendered, dynamic) = out.render_path(&event_with_workspace()).unwrap();
        assert_eq!(rendered, "/var/log/192.168.1.10.log");
        assert!(dynamic);
    }

    #[test]
    fn render_template_sanitizes_workspace_reference() {
        // "/var/log/${workspace.ip}.log" — workspace.ip contains "10.0.0.1/24",
        // the `/` must be replaced with `_`.
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("/var/log/".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec!["workspace".into(), "ip".into()]))),
            TemplateFragment::Literal(".log".into()),
        ])));
        let (rendered, _) = out.render_path(&event_with_workspace()).unwrap();
        assert_eq!(rendered, "/var/log/10.0.0.1_24.log");
    }

    #[test]
    fn render_template_does_not_sanitize_non_workspace_interp() {
        // `${source}` is a top-level ident (not workspace.*), so its value
        // is passed through even if it contains separators (IPv4 won't,
        // but the principle holds).
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("a-".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec!["source".into()]))),
            TemplateFragment::Literal("-b".into()),
        ])));
        let (rendered, _) = out.render_path(&event_with_workspace()).unwrap();
        assert_eq!(rendered, "a-192.168.1.10-b");
    }

    #[test]
    fn render_template_errors_without_attached_funcs() {
        let mut out = make_output(ek(ExprKind::Template(vec![TemplateFragment::Interp(ek(
            ExprKind::Ident(vec!["source".into()]),
        ))])));
        out.funcs = None;
        let err = out.render_path(&event_with_workspace()).unwrap_err();
        assert!(err.to_string().contains("FunctionRegistry not attached"));
    }

    #[test]
    fn is_workspace_reference_detects_nested_paths() {
        assert!(is_workspace_reference(&ek(ExprKind::Ident(vec![
            "workspace".into(),
            "x".into()
        ]))));
        assert!(is_workspace_reference(&ek(ExprKind::Ident(vec![
            "workspace".into(),
            "x".into(),
            "y".into()
        ]))));
        // `workspace` alone is not a reference to a specific key
        assert!(!is_workspace_reference(&ek(ExprKind::Ident(vec![
            "workspace".into()
        ]))));
        // non-workspace idents don't count
        assert!(!is_workspace_reference(&ek(ExprKind::Ident(vec![
            "source".into()
        ]))));
        // function calls always render without sanitisation (users
        // opt-in to their own sanitisation)
        assert!(!is_workspace_reference(&ek(ExprKind::FuncCall {
            namespace: None,
            name: "lower".into(),
            args: vec![ek(ExprKind::Ident(
                vec!["workspace".into(), "host".into(),]
            ))],
        })));
    }
}
