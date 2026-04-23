//! File output: appends event messages to a local file.
//!
//! Properties:
//!   path   "/var/log/limpid/fw.log"   — required (supports templates)
//!   mode   "0640"                      — octal file permissions (applied on create)
//!   owner  "syslog"                    — file owner (requires CAP_CHOWN)
//!   group  "adm"                       — file group (requires CAP_CHOWN or membership)
//!
//! Dynamic path templates:
//!   ${source}         — source IP address
//!   ${facility}       — facility number
//!   ${severity}       — severity number
//!   ${date}           — YYYY-MM-DD
//!   ${year}           — 4-digit year
//!   ${month}          — 2-digit month
//!   ${day}            — 2-digit day
//!   ${fields.xxx}     — value of fields.xxx (nested: ${fields.geo.country})
//!
//! Example:
//!   path "/var/log/limpid/${source}/${date}.log"

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{FromProperties, HasMetrics, Output};

pub struct FileOutput {
    path_template: String,
    is_dynamic: bool,
    mode: Option<u32>,
    owner: Option<String>,
    group: Option<String>,
    /// Tracks which paths have been created (for applying mode/owner/group once)
    created_paths: Mutex<HashSet<PathBuf>>,
    metrics: Arc<OutputMetrics>,
}

impl FromProperties for FileOutput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let path = props::get_string(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("output '{}': file requires 'path'", name))?;

        let is_dynamic = path.contains("${");

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
            path_template: path,
            is_dynamic,
            mode,
            owner,
            group,
            created_paths: Mutex::new(HashSet::new()),
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
    async fn write(&self, event: &Event) -> Result<()> {
        let path = if self.is_dynamic {
            let resolved = resolve_template(&self.path_template, event);
            let path = PathBuf::from(&resolved);
            // Sanitize: reject path traversal components
            for component in path.components() {
                if matches!(component, std::path::Component::ParentDir) {
                    anyhow::bail!("path traversal rejected: {}", resolved);
                }
            }
            path
        } else {
            PathBuf::from(&self.path_template)
        };

        // Ensure parent directory exists (needed for dynamic paths)
        if self.is_dynamic
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

        let msg = String::from_utf8_lossy(&event.message);
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
// Template resolution
// ---------------------------------------------------------------------------

fn resolve_template(template: &str, event: &Event) -> String {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var.push(c);
            }
            result.push_str(&resolve_variable(&var, event));
        } else {
            result.push(ch);
        }
    }

    result
}

/// Sanitize a template variable value: remove path separators and traversal components.
fn sanitize_path_component(s: &str) -> String {
    s.replace(['/', '\\'], "_").replace("..", "_")
}

fn resolve_variable(var: &str, event: &Event) -> String {
    match var {
        "source" => event.source.ip().to_string(), // IP addresses are safe
        "facility" => event.facility.map(|f| f.to_string()).unwrap_or_default(),
        "severity" => event.severity.map(|s| s.to_string()).unwrap_or_default(),
        "date" => event.timestamp.format("%Y-%m-%d").to_string(),
        "year" => event.timestamp.format("%Y").to_string(),
        "month" => event.timestamp.format("%m").to_string(),
        "day" => event.timestamp.format("%d").to_string(),
        v if v.starts_with("fields.") => {
            let path: Vec<&str> = v["fields.".len()..].split('.').collect();
            sanitize_path_component(&resolve_fields_path(&path, &event.fields))
        }
        _ => String::new(),
    }
}

fn resolve_fields_path(
    path: &[&str],
    fields: &std::collections::HashMap<String, serde_json::Value>,
) -> String {
    use serde_json::Value;

    let first = match fields.get(path[0]) {
        Some(v) => v,
        None => return String::new(),
    };

    let mut current = first;
    for &segment in &path[1..] {
        match current {
            Value::Object(map) => {
                current = match map.get(segment) {
                    Some(v) => v,
                    None => return String::new(),
                };
            }
            _ => return String::new(),
        }
    }

    match current {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
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
