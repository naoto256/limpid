//! Dead-letter queue (DLQ) writer for events that fail in `process`.
//!
//! When a pipeline `process` raises a runtime error, the event is
//! pulled out of the main flow and routed here as a JSONL record.
//! Operators can then audit failures, fix the offending config or
//! parser, and replay the original events via:
//!
//! ```bash
//! jq -c '.event' /var/log/limpid/errored.jsonl \
//!     | limpidctl inject input <name> --json
//! ```
//!
//! Per-write `OpenOptions::create(true).append(true)` is used by
//! design — failures are (hopefully) rare so the cost of a fresh open
//! is negligible, and it keeps the writer compatible with logrotate's
//! `copytruncate` / signal-less rotation flows without needing a
//! `SIGHUP`-handled file-handle reset.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::pipeline::ErroredEventContext;

/// Writer for the configured `error_log` JSONL file.
///
/// Built once at runtime startup from the `error_log` property in the
/// `control { ... }` block. Wrapped in `Option` upstream — when not
/// configured, the runtime falls back to a structured `tracing::error!`
/// line so the failure data is never silently lost.
pub struct ErrorLogWriter {
    path: PathBuf,
}

impl ErrorLogWriter {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Append one JSONL record for `ctx`. Errors here are surfaced to
    /// the caller (runtime layer) which counts them in
    /// `events_errored_unwritable` and falls back to tracing.
    pub async fn write(&self, ctx: &ErroredEventContext) -> Result<()> {
        let mut line = ctx.to_jsonl();
        line.push('\n');
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| {
                format!("error_log: failed to open {}", self.path.display())
            })?;
        f.write_all(line.as_bytes())
            .await
            .with_context(|| format!("error_log: failed to write to {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::value::Value;
    use crate::event::Event;
    use bytes::Bytes;
    use std::net::SocketAddr;
    use tempfile::TempDir;

    fn ctx() -> ErroredEventContext {
        let mut event = Event::new(
            Bytes::from_static(b"<134>raw payload"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        event
            .workspace
            .insert("partial".into(), Value::String("from earlier process".into()));
        ErroredEventContext {
            timestamp: chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000),
            pipeline: "p".into(),
            process: "wrap".into(),
            reason: "unknown identifier: timestamp".into(),
            event,
        }
    }

    #[tokio::test]
    async fn appends_jsonl_record() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let w = ErrorLogWriter::new(path.clone());
        w.write(&ctx()).await.unwrap();
        w.write(&ctx()).await.unwrap();
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["pipeline"], "p");
            assert_eq!(v["process"], "wrap");
            assert!(v["reason"].as_str().unwrap().contains("timestamp"));
            // event sub-object keeps only source / received_at / ingress —
            // egress and workspace are intentionally omitted.
            let event = &v["event"];
            assert!(event.get("source").is_some());
            assert!(event.get("received_at").is_some());
            assert!(event.get("ingress").is_some());
            assert!(event.get("egress").is_none());
            assert!(event.get("workspace").is_none());
        }
    }

    #[tokio::test]
    async fn parent_dir_must_exist() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing-subdir/errored.jsonl");
        let w = ErrorLogWriter::new(path);
        let err = w.write(&ctx()).await.unwrap_err().to_string();
        assert!(err.contains("error_log"), "got: {}", err);
    }
}
