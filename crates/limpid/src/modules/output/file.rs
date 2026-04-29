//! File output: appends event messages to a local file.
//!
//! Properties:
//!   path   "/var/log/limpid/fw.log"   — required (supports templates)
//!   mode   "0640"                      — octal file permissions (applied on create)
//!   owner  "syslog"                    — file owner (requires CAP_CHOWN)
//!   group  "adm"                       — file group (requires CAP_CHOWN or membership)
//!
//! Dynamic path templates use the DSL's native `${expr}` interpolation,
//! e.g. `path "/var/log/${source.ip}/${strftime(timestamp, "%Y-%m-%d")}.log"`.
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

use crate::dsl::arena::EventArena;
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

        // Catch-all `..` reject. For Template paths this is redundant
        // with `check_no_traversal` in `render_path`; for static
        // `StringLit` paths (which skip render_path's safety passes)
        // this is the sole defence.
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
    /// Three safety passes (each rejects rather than silently rewrites,
    /// per Principle 1):
    ///
    /// 1. Per-interpolation: every `${...}` result has `/` and `\`
    ///    replaced with `_`, regardless of the wrapping expression
    ///    (`${workspace.x}`, `${lower(workspace.x)}`, `${a + b}` —
    ///    all treated alike). An empty interpolation result is
    ///    rejected up front. The invariant is "one interpolation =
    ///    one path component"; directory structure must be expressed
    ///    in the literal parts of the template.
    ///
    /// 2. Post-evaluation traversal reject: the fully-rendered path
    ///    is split on `/` and any component exactly equal to `..`
    ///    causes the write to error. Combined with pass 1, no
    ///    interpolation value can introduce a directory escape
    ///    regardless of how it is composed.
    ///
    /// 3. Trailing-slash reject: a rendered path that ends in `/`
    ///    (no filename component) errors before the auto-mkdir runs,
    ///    so a stray template like `/var/log/${workspace.host}/`
    ///    cannot create empty directories silently.
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
                // The pipeline's per-event arena has already dropped by
                // the time the output runs. Templating against `event`
                // here just evaluates a small expression and produces
                // owned `Value`s; a local arena is all we need.
                let bump = bumpalo::Bump::new();
                let arena = EventArena::new(&bump);
                let mut out = String::new();
                for frag in fragments {
                    match frag {
                        TemplateFragment::Literal(s) => out.push_str(s),
                        TemplateFragment::Interp(expr) => {
                            let rendered = value_to_string(&eval_expr(expr, event, funcs, &arena)?);
                            // Pass 1: per-interp `/` `\` → `_` and reject empty.
                            // An empty interp would silently produce paths like
                            // `/foo//bar` or `/foo/.log` that almost never reflect
                            // operator intent — usually a null workspace value or
                            // a Pass-2 collapse of `${"..": something}`.
                            if rendered.is_empty() {
                                anyhow::bail!(
                                    "output file: interpolation evaluated to empty string \
                                     (would create surprise path like `/foo//bar` or `/foo/.log`)"
                                );
                            }
                            out.push_str(&sanitize_path_component(&rendered));
                        }
                    }
                }
                // Pass 2: reject (do not silently strip) any directory
                // traversal sequence in the fully-rendered path. `..` in
                // a path almost always reflects a config or data bug,
                // and silently rewriting it to "the target one level up"
                // would be the kind of "helpful" hidden behaviour
                // limpid Principle 1 forbids.
                check_no_traversal(&out)?;
                // Pass 3: reject empty results and trailing-slash
                // results before the write attempt. Trailing slash is
                // not just a "the OS will catch it" case — the parent-
                // dir auto-mkdir runs before open(), so a path like
                // `/foo/bar/` would silently create `/foo/bar` as a
                // directory and *then* fail at open with `EISDIR`.
                // Catching it here avoids the spurious mkdir side
                // effect and gives a clear diagnostic.
                if out.is_empty() {
                    anyhow::bail!(
                        "output file: rendered path is empty (template produced no content)"
                    );
                }
                if out.ends_with('/') {
                    anyhow::bail!(
                        "output file: rendered path ends with `/` (no filename component): {:?}",
                        out
                    );
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

/// Pass 1: per-interpolation sanitisation. Strip `/` and `\` so an
/// interpolation cannot expand into multiple path components or a
/// Windows path separator. `.` is left alone — operators rely on dots
/// for FQDN-style filenames (`web01.example.com.log`).
fn sanitize_path_component(s: &str) -> String {
    s.replace(['/', '\\'], "_")
}

/// Pass 2: error if any path component (slash-separated segment) is
/// exactly `..`. Per limpid Principle 1 (zero hidden behaviour), `..`
/// in a path is loud-rejected rather than silently rewritten —
/// almost always a config / data bug, and a silent collapse would
/// quietly redirect writes to a different file.
///
/// The check is component-wise (`split('/')`) so unusual but harmless
/// dirnames like `...` or `..foo` pass through cleanly — only the
/// exact `..` token, in any path position, is rejected.
fn check_no_traversal(s: &str) -> Result<()> {
    if s.split('/').any(|c| c == "..") {
        anyhow::bail!(
            "output file: rendered path contains a `..` traversal component: {:?}. \
             `..` is rejected rather than silently rewritten — sanitise upstream \
             (regex_replace, a process body) or pin the value before interpolation.",
            s
        );
    }
    Ok(())
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
    use crate::dsl::value::Value;
    use crate::functions::table::TableStore;
    use bytes::Bytes;
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
        // "/var/log/${source.ip}.log" — source is an Object since v0.5.6,
        // `source.ip` is the canonical accessor for the peer IP string.
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("/var/log/".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec!["source".into(), "ip".into()]))),
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
    fn render_template_sanitises_every_interpolation() {
        // Pass 1: every interpolation result has `/` `\` → `_`,
        // regardless of expression shape. `source.ip` (a non-workspace
        // path) gets the same treatment as `workspace.x`.
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("a-".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec!["source".into(), "ip".into()]))),
            TemplateFragment::Literal("-b".into()),
        ])));
        let (rendered, _) = out.render_path(&event_with_workspace()).unwrap();
        // source.ip is "192.168.1.10" — no slashes, no change. Principle
        // holds for hypothetical slash-bearing values.
        assert_eq!(rendered, "a-192.168.1.10-b");
    }

    #[test]
    fn render_template_errors_on_empty_interpolation() {
        // Template `/var/log/${workspace.empty}.log` with empty value would
        // produce `/var/log/.log` — almost never the operator's intent.
        let mut e = Event::new(
            Bytes::from("hello"),
            "192.168.1.10:514".parse::<SocketAddr>().unwrap(),
        );
        e.workspace.insert("empty".into(), Value::String("".into()));
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("/var/log/".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec![
                "workspace".into(),
                "empty".into(),
            ]))),
            TemplateFragment::Literal(".log".into()),
        ])));
        let err = out.render_path(&e).unwrap_err();
        assert!(
            err.to_string().contains("evaluated to empty"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn render_template_errors_on_trailing_slash() {
        // Template "/var/log/${workspace.empty}/" — empty value alone
        // would already trip Pass 1b; here the template has trailing
        // literal slash on a non-empty interp, producing a path that
        // ends in `/`. Without Pass 3 catching this, the write path's
        // `create_dir_all(parent)` would silently materialise an empty
        // directory before open() fails with EISDIR.
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("/var/log/".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec!["workspace".into(), "host".into()]))),
            TemplateFragment::Literal("/".into()),
        ])));
        let err = out.render_path(&event_with_workspace()).unwrap_err();
        assert!(
            err.to_string().contains("ends with `/`"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn render_template_errors_when_traversal_appears_in_path() {
        // Template `${workspace.v}` with v=".." → Pass 1 leaves ".." as-is
        // (no slash to strip) → Pass 2 rejects rather than silently
        // collapsing.
        let mut e = Event::new(
            Bytes::from("hello"),
            "192.168.1.10:514".parse::<SocketAddr>().unwrap(),
        );
        e.workspace.insert("v".into(), Value::String("..".into()));
        let out = make_output(ek(ExprKind::Template(vec![TemplateFragment::Interp(ek(
            ExprKind::Ident(vec!["workspace".into(), "v".into()]),
        ))])));
        let err = out.render_path(&e).unwrap_err();
        assert!(
            err.to_string().contains("traversal component"),
            "unexpected error: {}",
            err
        );
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
    fn check_no_traversal_accepts_clean_paths() {
        assert!(check_no_traversal("/var/log/foo.log").is_ok());
        assert!(check_no_traversal("/var/log/web01.example.com.log").is_ok());
        assert!(check_no_traversal("/var/log/.hidden.log").is_ok());
        // Multi-dot dirnames are NOT `..` — `....` is just an unusual
        // filename, not a traversal.
        assert!(check_no_traversal("/var/log/.../foo.log").is_ok());
        assert!(check_no_traversal("a/..../b").is_ok());
    }

    #[test]
    fn check_no_traversal_rejects_dot_dot_sequences() {
        // Single ../ in the middle
        assert!(check_no_traversal("/var/log/../etc/passwd").is_err());
        // Multiple ../ chained
        assert!(check_no_traversal("/var/log/../../etc/passwd").is_err());
        // Concatenation traversal: literal "/x/../" via interp+literal
        assert!(check_no_traversal("/var/log/x/../etc/passwd").is_err());
        // Trailing /..
        assert!(check_no_traversal("/var/log/..").is_err());
        // Standalone ..
        assert!(check_no_traversal("..").is_err());
    }
}
