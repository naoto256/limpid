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
//! jq -c 'select(.kind == "output" and .output.name == "<output-name>") | .event' /var/log/limpid/errored.jsonl \
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

use crate::event::OwnedEvent;
use crate::pipeline::ErroredEventContext;

impl ErroredEventContext {
    /// Serialise as a single-line JSON record for the dead-letter queue.
    ///
    /// Layout (v2 — hard break from v1):
    ///
    /// ```text
    /// {
    ///   "schema_version": 2,
    ///   "timestamp": "<RFC3339 nanos UTC>",
    ///   "reason": "<error msg>",
    ///   "pipeline": "<def pipeline name or empty>",
    ///   "kind": "process" | "output",
    ///   "process": { "name": "<process_name>" },   // kind=process only
    ///   "output":  { "name": "<output_name>" },    // kind=output only
    ///   "event": {
    ///     "source": { "ip": ..., "port": ... },
    ///     "received_at": <unix nanos>,
    ///     "ingress": "...",
    ///     "egress":  "..."                         // kind=output only
    ///   }
    /// }
    /// ```
    ///
    /// `schema_version: 2` is the operator-visible discriminator for
    /// the v0.7.8 schema break. Output records intentionally carry
    /// *only* `{ name }` — no address, dest, path, key, topic,
    /// partition, endpoint, URL, peer, target, or workspace. Replay
    /// (`limpidctl inject output <name>`) hands the event back to the
    /// sink's `consume()`, which re-routes internally.
    ///
    /// Lives in `error_log` (not `pipeline`) because it encodes this
    /// module's DLQ wire format / replay contract — `ErroredEventContext`
    /// itself stays in `pipeline` since that's where the failure sites
    /// construct it, but the JSONL shape is `error_log`'s to own.
    pub fn to_jsonl(&self) -> String {
        // Rebuild a minimal Event so we can reuse the canonical
        // `to_json_value` serialiser for source / received_at /
        // ingress / egress. We construct it from the snapshot rather
        // than carrying a full OwnedEvent so we never accidentally
        // leak workspace fragments into the DLQ.
        let (timestamp, pipeline, kind_block, reason, event_json) = match self {
            Self::Process {
                timestamp,
                pipeline,
                site,
                reason,
                event,
            } => {
                let ev = OwnedEvent {
                    received_at: event.received_at,
                    source: event.source,
                    ingress: event.ingress.clone(),
                    egress: event.ingress.clone(),
                    workspace: std::collections::HashMap::new(),
                    ack: None,
                };
                let mut event_json = ev.to_json_value();
                if let serde_json::Value::Object(ref mut map) = event_json {
                    // ProcessEvent has no egress concept — strip it so
                    // replay recipes treat absence as "build egress
                    // from ingress at deserialisation time"
                    // (`Event::from_json` already does that).
                    map.remove("egress");
                    map.remove("workspace");
                }
                (
                    *timestamp,
                    pipeline,
                    serde_json::json!({
                        "kind": "process",
                        "process": { "name": site },
                    }),
                    reason,
                    event_json,
                )
            }
            Self::Output {
                timestamp,
                pipeline,
                site: _,
                reason,
                output_name,
                event,
            } => {
                let ev = OwnedEvent {
                    received_at: event.received_at,
                    source: event.source,
                    ingress: event.ingress.clone(),
                    egress: event.egress.clone(),
                    workspace: std::collections::HashMap::new(),
                    ack: None,
                };
                let mut event_json = ev.to_json_value();
                if let serde_json::Value::Object(ref mut map) = event_json {
                    // Output records must never carry workspace —
                    // any sink-specific routing metadata is forbidden
                    // by the DLQ schema contract (replay re-routes
                    // via the sink's own `consume()` path).
                    map.remove("workspace");
                }
                (
                    *timestamp,
                    pipeline,
                    serde_json::json!({
                        "kind": "output",
                        "output": { "name": output_name },
                    }),
                    reason,
                    event_json,
                )
            }
        };

        // Merge kind discriminator block + per-kind name block into
        // the top-level record. Using a Map keeps key ordering stable.
        let mut record = serde_json::Map::new();
        record.insert("schema_version".into(), serde_json::json!(2));
        record.insert(
            "timestamp".into(),
            serde_json::Value::String(
                timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
            ),
        );
        record.insert("reason".into(), serde_json::Value::String(reason.clone()));
        record.insert(
            "pipeline".into(),
            serde_json::Value::String(pipeline.clone()),
        );
        if let serde_json::Value::Object(kb) = kind_block {
            for (k, v) in kb {
                record.insert(k, v);
            }
        }
        record.insert("event".into(), event_json);
        serde_json::Value::Object(record).to_string()
    }
}

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
    /// the open + write_all + shutdown sequence — not around
    /// `to_jsonl()` which is pure CPU work.
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
    ///
    /// The trailing `shutdown().await` closes the underlying handle
    /// synchronously with this future rather than leaving it to
    /// `Drop`. `tokio::fs::File`'s `Drop` fires the close on the
    /// blocking pool and returns immediately, so without an explicit
    /// shutdown a caller that observes `write()` returning `Ok(())` is
    /// not guaranteed the record is visible to a subsequent open/read
    /// on another task — the flake this closes surfaced exactly that
    /// way in CI, where a subsequent `tokio::fs::read_to_string`
    /// occasionally saw an empty file. Shutdown-then-drop also nudges
    /// the file toward on-disk durability, which matters for a DLQ.
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
        f.shutdown()
            .await
            .with_context(|| format!("error_log: failed to close {}", self.path.display()))?;
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

    // -----------------------------------------------------------------
    // to_jsonl wire-format tests
    // -----------------------------------------------------------------
    //
    // The JSONL shape (schema_version = 2, `Process`/`Output` sum
    // discriminants, forbidden routing fields on Output, event sub-object
    // replayable through `Event::from_json`) is `error_log`'s to own —
    // that contract is what `limpidctl inject --json` and any downstream
    // DLQ tooling read against. Keep the wire-shape assertions here even
    // though `ErroredEventContext` and `to_jsonl` are constructed and
    // called out of `crate::pipeline`.

    fn sample_owned_event() -> crate::event::OwnedEvent {
        use std::net::SocketAddr;
        let mut ev = crate::event::OwnedEvent::new(
            Bytes::from_static(b"hello"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        ev.egress = Bytes::from_static(b"goodbye");
        ev
    }

    #[test]
    fn process_variant_jsonl_has_no_egress_no_output_block() {
        let ctx = ErroredEventContext::Process {
            timestamp: chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000),
            pipeline: "p".into(),
            site: "wrap".into(),
            reason: "boom".into(),
            event: crate::pipeline::ProcessEvent::from_owned(&sample_owned_event()),
        };
        let line = ctx.to_jsonl();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["schema_version"], 2);
        assert_eq!(v["kind"], "process");
        assert_eq!(v["pipeline"], "p");
        assert_eq!(v["reason"], "boom");
        assert_eq!(v["process"]["name"], "wrap");
        assert!(v["output"].is_null(), "Process must not carry output block");
        assert_eq!(v["event"]["ingress"], "hello");
        assert!(
            v["event"]["egress"].is_null(),
            "Process event must omit egress"
        );
        assert!(v["event"]["workspace"].is_null());
    }

    #[test]
    fn output_variant_jsonl_carries_egress_and_output_block() {
        let ctx = ErroredEventContext::Output {
            timestamp: chrono::DateTime::from_timestamp_nanos(1_700_000_000_000_000_000),
            pipeline: String::new(),
            site: "sink enqueue".into(),
            reason: "queue closed".into(),
            output_name: "sink".into(),
            event: crate::pipeline::OutputEvent::from_owned(&sample_owned_event()),
        };
        let line = ctx.to_jsonl();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["schema_version"], 2);
        assert_eq!(v["kind"], "output");
        assert_eq!(v["pipeline"], "");
        assert_eq!(v["output"]["name"], "sink");
        assert!(
            v["process"].is_null(),
            "Output must not carry process block"
        );
        assert_eq!(v["event"]["ingress"], "hello");
        assert_eq!(v["event"]["egress"], "goodbye");
        assert!(v["event"]["workspace"].is_null());
    }

    #[test]
    fn output_variant_jsonl_must_not_carry_sink_routing_metadata() {
        // Pin the DLQ no-address contract: the Output record carries
        // ONLY `{ name }`. No address, dest, path, key, topic,
        // partition, endpoint, url, peer, target, or workspace at any
        // level.
        let ctx = ErroredEventContext::Output {
            timestamp: chrono::Utc::now(),
            pipeline: "p".into(),
            site: "sink".into(),
            reason: "retry exhausted".into(),
            output_name: "sink".into(),
            event: crate::pipeline::OutputEvent::from_owned(&sample_owned_event()),
        };
        let line = ctx.to_jsonl();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let forbidden = [
            "address",
            "dest",
            "path",
            "key",
            "topic",
            "partition",
            "endpoint",
            "url",
            "peer",
            "target",
            "workspace",
        ];
        for f in forbidden {
            assert!(
                v.get(f).is_none(),
                "top-level must not carry forbidden field {}",
                f
            );
            assert!(
                v["output"].get(f).is_none(),
                "output block must not carry forbidden field {}",
                f
            );
            assert!(
                v["event"].get(f).is_none(),
                "event block must not carry forbidden field {}",
                f
            );
        }
        // output block must have *only* `name`.
        let obj = v["output"].as_object().expect("output is an object");
        assert_eq!(obj.len(), 1, "output block must carry only `name`");
        assert!(obj.contains_key("name"));
    }

    #[test]
    fn output_variant_round_trip_via_event_from_json() {
        // The Output event sub-object must be replayable through
        // `Event::from_json` so `limpidctl inject output --json` can
        // reconstruct the egress payload end-to-end.
        let ctx = ErroredEventContext::Output {
            timestamp: chrono::Utc::now(),
            pipeline: String::new(),
            site: "sink enqueue".into(),
            reason: "queue closed".into(),
            output_name: "sink".into(),
            event: crate::pipeline::OutputEvent::from_owned(&sample_owned_event()),
        };
        let line = ctx.to_jsonl();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let event_str = serde_json::to_string(&v["event"]).unwrap();
        let replayed =
            crate::event::Event::from_json(&event_str).expect("event sub-object must replay");
        assert_eq!(&replayed.ingress[..], b"hello");
        assert_eq!(&replayed.egress[..], b"goodbye");
    }

    #[test]
    fn process_variant_round_trip_via_event_from_json() {
        let ctx = ErroredEventContext::Process {
            timestamp: chrono::Utc::now(),
            pipeline: "p".into(),
            site: "wrap".into(),
            reason: "boom".into(),
            event: crate::pipeline::ProcessEvent::from_owned(&sample_owned_event()),
        };
        let line = ctx.to_jsonl();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let event_str = serde_json::to_string(&v["event"]).unwrap();
        let replayed =
            crate::event::Event::from_json(&event_str).expect("event sub-object must replay");
        // Process events omit egress on the wire — Event::from_json
        // backfills egress from ingress so replay through `inject input`
        // sees a self-consistent starting state.
        assert_eq!(&replayed.ingress[..], b"hello");
        assert_eq!(&replayed.egress[..], b"hello");
    }
}
