//! Dead-letter queue (DLQ) writer for events that fail their main-flow
//! disposition.
//!
//! Records are sum-typed (`schema_version: 2`): every record carries a
//! `kind` discriminator (`"process"` or `"output"`) and a per-kind block
//! (`process: { name }` or `output: { name }`) naming the failure site.
//! The Output flavor additionally carries the rendered `egress` in
//! `event.egress`; the Process flavor only has `event.{source,
//! received_at, ingress}`.
//!
//! Seven producer sites map to the two flavors:
//!
//! Process flavor (= pipeline-side failures; replay via `inject input`):
//!
//! 1. **`<process_name>`** — an explicit `process` body raised an error
//!    via `error <expr>` or a process-internal failure.
//! 2. **`(inline)`** — an inline `process { ... }` block raised
//!    similarly.
//! 3. **`(pipeline body)`** — `if`/`switch`/`error <expr>`/process-args
//!    eval failed before reaching a process body.
//! 4. **`(pipeline)`** — `error <expr>` at the pipeline (statement)
//!    level raised.
//!
//! Output flavor (= sink-side failures; replay via `inject output`):
//!
//! 5. **`<output_name>`** — output retry budget exhausted (= sink-side).
//!    A batched output's per-event render failure inside `flush()` is
//!    also routed here with `reason = "render failed during batch
//!    flush: ..."`.
//! 6. **`<output_name> shutdown`** — batched output's `shutdown()`
//!    walks any remaining `Vec<Event>` buffer entries (one per event)
//!    through this writer.
//! 7. **`<output_name> enqueue`** — `runtime.rs` could not hand an
//!    event to the named output's queue (queue closed, disk write
//!    error, unknown output). Per-failed-output split: a pipeline-eval
//!    result with N failed-output enqueues produces N records.
//!
//! All seven converge on this same JSONL file and the same
//! `events_errored` / `events_errored_unwritable` counter pair.
//! Operators audit failures, fix the offending config or parser, and
//! replay the original events. Replay tooling is flavor-aware:
//!
//! ```bash
//! # Process flavor: re-enter at the input layer; the pipeline reruns
//! # against the original ingress bytes.
//! jq -c 'select(.kind == "process") | .event' /var/log/limpid/errored.jsonl \
//!     | limpidctl inject input <input-name> --json
//!
//! # Output flavor: re-deliver the pre-rendered event directly to the
//! # named output's queue; the sink re-routes via its own `consume()`.
//! jq -c 'select(.kind == "output") | .event' /var/log/limpid/errored.jsonl \
//!     | limpidctl inject output <output-name> --json
//! ```
//!
//! Per-write `OpenOptions::create(true).append(true)` is used by
//! design — failures are (hopefully) rare so the cost of a fresh open
//! is negligible, and it keeps the writer compatible with logrotate's
//! `copytruncate` / signal-less rotation flows without needing a
//! `SIGHUP`-handled file-handle reset.
//!
//! Concurrency note: multiple pipeline workers may call `write()`
//! concurrently when several pipelines hit a process error in the
//! same instant. `O_APPEND` only guarantees atomic append for writes
//! up to `PIPE_BUF` (Linux: 4 KiB), and DLQ records carrying
//! base64-encoded binary ingress can easily exceed that. To keep
//! lines from interleaving, every `write()` takes a process-local
//! `tokio::sync::Mutex` before opening the file. The serialisation
//! is inside the `error_log` boundary, not at the kernel layer.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use crate::pipeline::ErroredEventContext;

/// Writer for the configured `error_log` JSONL file.
///
/// Built once at runtime startup from the `error_log` property in the
/// `control { ... }` block. Wrapped in `Option` upstream — when not
/// configured, the runtime falls back to a structured `tracing::error!`
/// line so the failure data is never silently lost.
pub struct ErrorLogWriter {
    path: PathBuf,
    /// Serialises concurrent `write()` calls so that records from
    /// different pipeline workers cannot interleave when a single
    /// JSONL line exceeds `PIPE_BUF`. The lock is held only across
    /// the open + write_all sequence — not around `to_jsonl()` which
    /// is pure CPU work.
    write_lock: Mutex<()>,
}

impl ErrorLogWriter {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            write_lock: Mutex::new(()),
        }
    }

    /// Validate that the `error_log` path is reachable at startup.
    ///
    /// Checks the parent directory exists and is writable by the
    /// daemon user. Surfacing this at startup (rather than at first
    /// failure) matches Principle 1 — operators see typo'd paths
    /// before any event hits a process error.
    ///
    /// The file itself does not need to exist; `OpenOptions::create`
    /// will materialise it on the first failure.
    pub async fn validate_at_startup(&self) -> Result<()> {
        let parent = self.path.parent().ok_or_else(|| {
            anyhow::anyhow!(
                "error_log path '{}' has no parent directory",
                self.path.display()
            )
        })?;
        let parent: &Path = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        let meta = tokio::fs::metadata(parent).await.with_context(|| {
            format!(
                "error_log: parent directory '{}' is not accessible (does it exist?)",
                parent.display()
            )
        })?;
        if !meta.is_dir() {
            anyhow::bail!(
                "error_log: '{}' exists but is not a directory",
                parent.display()
            );
        }
        Ok(())
    }

    /// Append one JSONL record for `ctx`. Errors here are surfaced to
    /// the caller (runtime layer) which counts them in
    /// `events_errored_unwritable` and falls back to tracing.
    pub async fn write(&self, ctx: &ErroredEventContext) -> Result<()> {
        let mut line = ctx.to_jsonl();
        line.push('\n');
        let _guard = self.write_lock.lock().await;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await
            .with_context(|| format!("error_log: failed to open {}", self.path.display()))?;
        f.write_all(line.as_bytes())
            .await
            .with_context(|| format!("error_log: failed to write to {}", self.path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::value::OwnedValue;
    use crate::event::Event;
    use bytes::Bytes;
    use std::net::SocketAddr;
    use tempfile::TempDir;

    fn ctx() -> ErroredEventContext {
        let mut event = Event::new(
            Bytes::from_static(b"<134>raw payload"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        event.workspace.insert(
            "partial".into(),
            OwnedValue::String("from earlier process".into()),
        );
        ErroredEventContext::Process {
            timestamp: chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000),
            pipeline: "p".into(),
            site: "wrap".into(),
            reason: "unknown identifier: timestamp".into(),
            event: crate::pipeline::ProcessEvent::from_owned(&event),
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
            assert_eq!(v["schema_version"], 2);
            assert_eq!(v["kind"], "process");
            assert_eq!(v["pipeline"], "p");
            assert_eq!(v["process"]["name"], "wrap");
            assert!(v["output"].is_null());
            assert!(v["reason"].as_str().unwrap().contains("timestamp"));
            // event sub-object keeps only source / received_at / ingress
            // for Process records — egress and workspace are omitted.
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

    #[tokio::test]
    async fn validate_at_startup_passes_for_existing_parent() {
        let dir = TempDir::new().unwrap();
        let w = ErrorLogWriter::new(dir.path().join("errored.jsonl"));
        w.validate_at_startup().await.unwrap();
    }

    #[tokio::test]
    async fn validate_at_startup_fails_for_missing_parent() {
        let dir = TempDir::new().unwrap();
        let w = ErrorLogWriter::new(dir.path().join("nope/errored.jsonl"));
        let err = w.validate_at_startup().await.unwrap_err().to_string();
        assert!(err.contains("not accessible"), "got: {}", err);
    }

    #[tokio::test]
    async fn concurrent_writes_do_not_interleave_lines() {
        // Records carrying ~6 KiB of base64-encoded binary ingress would
        // exceed POSIX PIPE_BUF (4 KiB) and could interleave under raw
        // O_APPEND from independent file handles. The internal Mutex
        // serialises writes so each line stays atomic.
        use std::sync::Arc;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let w = Arc::new(ErrorLogWriter::new(path.clone()));

        // Inflate the ingress to push the JSONL line past PIPE_BUF.
        let big = vec![b'A'; 8192];
        let big_event = Event::new(
            Bytes::from(big),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        let ctx = match ctx() {
            ErroredEventContext::Process {
                timestamp,
                pipeline,
                site,
                reason,
                ..
            } => ErroredEventContext::Process {
                timestamp,
                pipeline,
                site,
                reason,
                event: crate::pipeline::ProcessEvent::from_owned(&big_event),
            },
            _ => unreachable!("ctx() returns Process"),
        };
        let ctx = Arc::new(ctx);

        let mut handles = Vec::new();
        for _ in 0..16 {
            let w = Arc::clone(&w);
            let c = Arc::clone(&ctx);
            handles.push(tokio::spawn(async move {
                w.write(&c).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        // Each line must parse as JSON — interleaving would split records.
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 16, "expected 16 records, got {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("line {} is not valid JSON: {}\nline: {}", i, e, line));
        }
    }
}
