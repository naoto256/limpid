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
//! 3. **`(pipeline body)`** — `if`/`switch`/`error <expr>` eval
//!    failed before reaching a process body.
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
//! Per-write open (not a persistent handle) is used by design —
//! failures are (hopefully) rare so the cost of a fresh open is
//! negligible, and it keeps the writer compatible with logrotate's
//! `copytruncate` / signal-less rotation flows without needing a
//! `SIGHUP`-handled file-handle reset. The open itself is a two-branch
//! contract: `create_new(true)` (`O_CREAT|O_EXCL`) with `O_NOFOLLOW`
//! and `mode(0o600)` for the fresh-inode path, falling back to a
//! non-create `O_NOFOLLOW | O_NONBLOCK` open plus an `fstat`
//! `S_ISREG` + mode verify on the `AlreadyExists` branch. Symlinks
//! are refused via `O_NOFOLLOW`; a FIFO at the DLQ path with no
//! reader is refused fast via `O_NONBLOCK` returning `ENXIO`, and
//! any other non-regular shape (FIFO with a reader, socket,
//! directory, device node) is refused by the `S_ISREG` fstat before
//! any bytes are written. An existing DLQ file whose mode isn't
//! exactly `0o600` is refused with a loud error rather than silently
//! appending a leak-prone record. See `ErrorLogWriter::write` for the
//! invariants.
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
    /// Checks:
    /// - The parent directory exists and is a directory.
    /// - If the DLQ file already exists on disk (leftover from a
    ///   prior run, logrotate output, or an operator tool), it is a
    ///   regular file (not a symlink, FIFO, socket, directory, or
    ///   device node) and its mode is exactly `0o600`. The runtime
    ///   `write` path refuses each of these loudly, so an operator
    ///   whose fresh deployment inherits a `0o644` file, a symlink,
    ///   or a mistyped path pointing at some other node type would
    ///   otherwise discover the mismatch only at the first real
    ///   failure — after the pipeline has already lost the
    ///   observability on that record. Surface it at startup so the
    ///   fix (remove or `chmod` the file, remove the wrong node)
    ///   happens before any events flow.
    ///
    /// The file itself does not need to exist. If it is absent,
    /// the preflight materialises it eagerly: `create_new` +
    /// `O_NOFOLLOW` + `mode(0o600)`, followed by `fchmod(0o600)`
    /// and an `fstat` re-verify on the fresh fd so a hostile
    /// umask (which would otherwise mask the `open(2)` mode
    /// argument) can't leave the file at anything but `0o600`.
    /// The resulting empty 0o600 file matches what the runtime
    /// would have produced on the first real failure.
    ///
    /// If the file already exists, the preflight opens it with
    /// `O_WRONLY|O_APPEND|O_NOFOLLOW|O_NONBLOCK` and `fstat`s the
    /// opened fd to re-check the S_ISREG + `0o600` contract —
    /// closing the TOCTOU gap between the earlier
    /// `symlink_metadata` and the `open(2)`.
    ///
    /// This function is called only from the daemon startup path
    /// (`Runtime::start`), never from `--check`. Configuration
    /// validation must not touch the filesystem beyond a
    /// read-only stat; the eager create on the absent-file path
    /// makes this preflight write-side and is deliberately gated
    /// on daemon startup.
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
        // Inspect the FINAL parent component with `symlink_metadata`
        // so a symlink parent can be named explicitly and rejected up
        // front. Otherwise `metadata` would follow the link and pass
        // the check against the (safe) target, letting an attacker
        // who can rename the symlink between here and the runtime
        // write path redirect DLQ writes into an attacker-controlled
        // directory. Final file is separately guarded by
        // `O_NOFOLLOW` + fstat on the fd, but that only covers the
        // socket path's last component; the parent path's identity
        // is the ancestor trust boundary. Ancestor components may
        // still be symlinks (`/var/run` → `/run`); ancestor path
        // identity is a deployment contract, not a runtime check.
        let link_meta = tokio::fs::symlink_metadata(parent).await.with_context(|| {
            format!(
                "error_log: parent directory '{}' is not accessible (does it exist?)",
                parent.display()
            )
        })?;
        if link_meta.file_type().is_symlink() {
            anyhow::bail!(
                "error_log: parent directory '{}' is a symlink — refusing to preflight. The \
                 final parent component must be a real directory: a symlink lets an attacker \
                 redirect DLQ writes between this check and the runtime `write` path. Point \
                 `control {{ error_log \"...\" }}` at a real directory (or leave the packaged \
                 default alone). Modern Linux ships `/var/run` as a symlink to `/run`; a \
                 `/run/limpid/...` path avoids the symlink parent shape.",
                parent.display()
            );
        }
        if !link_meta.is_dir() {
            anyhow::bail!(
                "error_log: '{}' exists but is not a directory",
                parent.display()
            );
        }

        // If the DLQ file already exists on disk, refuse anything that
        // isn't a regular 0o600 file. The runtime `write` path applies
        // the same contract (`O_NOFOLLOW`, fstat `S_ISREG`, mode
        // check), so an operator whose deployment inherited a
        // symlink, a FIFO, a socket, a directory, or a 0o644 leftover
        // would otherwise discover the mismatch only at the first
        // real failure — after the pipeline has already lost the
        // observability on that record. Surface it at startup so the
        // fix (remove or `chmod` the file, replace the node) happens
        // before any events flow. `symlink_metadata` inspects the
        // link itself rather than following it, so we can name the
        // symlink case explicitly instead of returning an `ELOOP`
        // wrapped in a stat error.
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileTypeExt, PermissionsExt};
            match tokio::fs::symlink_metadata(&self.path).await {
                Ok(meta) => {
                    let ft = meta.file_type();
                    if ft.is_symlink() {
                        anyhow::bail!(
                            "error_log: '{}' is a symlink; the runtime refuses to follow it. \
                             Remove the symlink before starting the daemon.",
                            self.path.display()
                        );
                    }
                    if !ft.is_file() {
                        // Any non-regular, non-symlink node: FIFO,
                        // socket, directory, block or character
                        // device. The write-path fstat would refuse
                        // this too, but the startup refusal names the
                        // offending shape directly.
                        let shape = if ft.is_dir() {
                            "directory"
                        } else if ft.is_fifo() {
                            "FIFO"
                        } else if ft.is_socket() {
                            "socket"
                        } else if ft.is_block_device() {
                            "block device"
                        } else if ft.is_char_device() {
                            "character device"
                        } else {
                            "non-regular file"
                        };
                        anyhow::bail!(
                            "error_log: '{}' is a {} rather than a regular file; the runtime \
                             refuses to write through it. Remove or move the node before \
                             starting the daemon.",
                            self.path.display(),
                            shape
                        );
                    }
                    let actual = meta.permissions().mode() & 0o7777;
                    if actual != 0o600 {
                        anyhow::bail!(
                            "error_log: existing file '{}' has mode 0o{:o}, but the DLQ \
                             writer requires exactly 0o600. `chmod 0600 <path>` or remove \
                             the file so the runtime recreates it.",
                            self.path.display(),
                            actual
                        );
                    }

                    // Preflight: open the same fd shape the runtime
                    // write path uses (write + append + `O_NOFOLLOW`
                    // + `O_NONBLOCK`) and re-verify shape / mode on
                    // that fd. The two-syscall shape (stat above,
                    // open here) leaves a TOCTOU window where the
                    // path could be swapped between checks;
                    // fstat-ing the opened fd forces the same-inode
                    // contract that the runtime write path also
                    // relies on. This also catches parent-directory
                    // execute/write denials and stale ACLs that a
                    // bare `symlink_metadata` stat cannot see — the
                    // parent may exist and be group-readable while
                    // `open(2)` for append is refused with
                    // `EACCES`. Docs promise the DLQ path is
                    // validated up front; without the probe, the
                    // failure would only surface at first DLQ
                    // write, after the pipeline has already lost the
                    // observability on that record.
                    let mut probe_opts = OpenOptions::new();
                    probe_opts.write(true).append(true);
                    probe_opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                    let probe = probe_opts.open(&self.path).await.map_err(|e| {
                        anyhow::Error::from(e).context(format!(
                            "error_log: existing DLQ file '{}' is not writable by the \
                             daemon user",
                            self.path.display()
                        ))
                    })?;
                    self.verify_existing_mode(&probe).await?;
                    drop(probe);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Absent file — probe the create path so a
                    // parent-directory permission miss or an SELinux/
                    // AppArmor denial surfaces at startup rather than
                    // at first DLQ write. Uses the same
                    // `create_new` + `O_NOFOLLOW` + `mode(0o600)`
                    // shape the runtime write path uses, so a success
                    // here is a real guarantee that the write path
                    // can create the inode.
                    //
                    // A successful preflight leaves an empty 0o600
                    // file at `self.path`. This is intentional and
                    // documented — daemon startup is the only caller
                    // of this function (never `--check`), and an
                    // empty DLQ file at the configured path matches
                    // what the runtime would have created on the
                    // first real failure anyway.
                    let mut create_opts = OpenOptions::new();
                    create_opts.write(true).create_new(true).append(true);
                    create_opts.custom_flags(libc::O_NOFOLLOW);
                    create_opts.mode(0o600);
                    let probe = create_opts.open(&self.path).await.map_err(|e| {
                        anyhow::Error::from(e).context(format!(
                            "error_log: cannot create DLQ file at '{}'; check parent \
                             directory permissions, ownership, and any MAC (SELinux / \
                             AppArmor) confinement",
                            self.path.display()
                        ))
                    })?;
                    // umask defense: `.mode(0o600)` at `open(2)`
                    // time is masked against the process umask, so
                    // an unusually restrictive umask (e.g. `0o277`,
                    // which strips owner write) or an unusually
                    // permissive one (a paranoid packager wrapper
                    // that leaves umask as inherited from the
                    // caller) could land the file at a different
                    // mode. `fchmod` on the just-opened fd forces
                    // exactly `0o600`; the subsequent
                    // `verify_existing_mode` fstats the same fd to
                    // confirm both S_ISREG and the 0o600 contract
                    // that the runtime write path checks.
                    self.fchmod_and_verify_dlq_mode(&probe).await?;
                    drop(probe);
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e).context(format!(
                        "error_log: failed to stat '{}'",
                        self.path.display()
                    )));
                }
            }
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
    ///
    /// The DLQ record carries the failed event's `ingress` / `egress`
    /// verbatim (UTF-8 payloads land as plain text, not base64), so a
    /// line that happened to contain a secret or PII is written through
    /// unchanged. Two invariants gate every write on Unix:
    ///
    /// - `O_NOFOLLOW`: refuse to write through a symlink at the DLQ
    ///   path. If an attacker (or a mis-configured operator tool)
    ///   plants a symlink where the DLQ file should be, we surface it
    ///   as `refusing to follow symlink at error_log path` rather than
    ///   silently redirecting failure records to whatever the symlink
    ///   points at. This mirrors the guard already in place on the
    ///   `file` output.
    /// - **Full 12-bit mode contract** (`0o600`): when we take the
    ///   `create_new` branch (fresh inode), the mode is set at
    ///   `open(2)` time via the `mode` option. When we take the
    ///   `AlreadyExists` branch (an inode was already there — logrotate,
    ///   a crash-leftover, or an operator-touched file), we `fstat` the
    ///   fd and refuse the write if the observed mode is not exactly
    ///   `0o600` (masking to 0o7777 so setuid/setgid/sticky mismatches
    ///   don't slip through with matching rwx bits). No silent chmod
    ///   on a file we didn't create; the operator sees a loud error
    ///   and either rotates the file or aligns the mode. Same shape as
    ///   the `file` output's `verify_existing_file_metadata` — a DLQ
    ///   that leaks its records at 0o644 is exactly as bad as a log
    ///   sink that does.
    pub async fn write(&self, ctx: &ErroredEventContext) -> Result<()> {
        let mut line = ctx.to_jsonl();
        line.push('\n');
        let _guard = self.write_lock.lock().await;

        // Fresh-inode branch: `create_new` (= `O_CREAT|O_EXCL`) plus
        // `O_NOFOLLOW`. Succeeds only if the path had nothing there,
        // which is our unambiguous "we own this inode from birth"
        // signal. The `mode(0o600)` on the OpenOptions lands the
        // permission at `open(2)` time, before the first payload byte
        // hits disk.
        let mut create_opts = OpenOptions::new();
        create_opts.write(true).create_new(true).append(true);
        #[cfg(unix)]
        {
            create_opts.custom_flags(libc::O_NOFOLLOW);
            create_opts.mode(0o600);
        }
        let create_res = create_opts.open(&self.path).await;

        let mut f = match create_res {
            Ok(f) => {
                // umask defense: `.mode(0o600)` at `open(2)` time
                // is masked by the process umask, so an unusual
                // umask can strip owner bits. Force the mode on
                // the just-created fd via `fchmod` (which is
                // umask-independent) and re-verify via `fstat`.
                #[cfg(unix)]
                self.fchmod_and_verify_dlq_mode(&f).await?;
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Fallback: open the existing inode non-create with
                // O_NOFOLLOW | O_NONBLOCK, then fstat-verify it is a
                // regular file with mode `0o600`. O_NONBLOCK stops a
                // FIFO-with-no-reader at the DLQ path from blocking
                // the writer indefinitely; on the failure side the
                // `verify_existing_mode` fstat catches the FIFO / any
                // non-regular node before we would append anything.
                let mut existing_opts = OpenOptions::new();
                existing_opts.write(true).append(true);
                #[cfg(unix)]
                existing_opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                let f = existing_opts.open(&self.path).await.map_err(|e| {
                    #[cfg(unix)]
                    if e.raw_os_error() == Some(libc::ELOOP) {
                        return anyhow::anyhow!(
                            "refusing to follow symlink at error_log path: {}",
                            self.path.display()
                        );
                    }
                    #[cfg(unix)]
                    if e.raw_os_error() == Some(libc::ENXIO) {
                        return anyhow::anyhow!(
                            "refusing to write to non-regular file at error_log path (looks \
                             like a FIFO with no reader): {}",
                            self.path.display()
                        );
                    }
                    anyhow::Error::from(e)
                        .context(format!("error_log: failed to open {}", self.path.display()))
                })?;
                #[cfg(unix)]
                self.verify_existing_mode(&f).await?;
                f
            }
            Err(e) => {
                #[cfg(unix)]
                if e.raw_os_error() == Some(libc::ELOOP) {
                    anyhow::bail!(
                        "refusing to follow symlink at error_log path: {}",
                        self.path.display()
                    );
                }
                return Err(anyhow::Error::from(e)
                    .context(format!("error_log: failed to open {}", self.path.display())));
            }
        };
        f.write_all(line.as_bytes())
            .await
            .with_context(|| format!("error_log: failed to write to {}", self.path.display()))?;
        f.shutdown()
            .await
            .with_context(|| format!("error_log: failed to close {}", self.path.display()))?;
        Ok(())
    }

    /// Verify that an existing DLQ file is a regular file with on-disk
    /// mode exactly `0o600`. Called only on the `AlreadyExists` branch
    /// of the write path — we did not create the inode, and the
    /// operator or logrotate may have left a file whose type or mode
    /// is different from what the DLQ is supposed to guarantee.
    ///
    /// The `S_ISREG` check is not defensive — an operator typo that
    /// pointed `error_log` at a FIFO, socket, directory, or device
    /// node would otherwise let this writer append records into a
    /// stream the operator didn't intend to be one. The mode check
    /// alone doesn't catch that shape mismatch (a FIFO can be `0o600`).
    ///
    /// Uses `dup(2)` + `spawn_blocking` so the `fstat` runs against a
    /// fd that stays live for the duration of the syscall even if the
    /// outer future is cancelled — a raw fd number is trivial for
    /// the kernel to reuse after the source `File` is dropped, and
    /// letting the syscall race that reuse would land on an unrelated
    /// inode. Same shape as `apply_file_metadata_to_fd` in the file
    /// output.
    #[cfg(unix)]
    async fn verify_existing_mode(&self, file: &tokio::fs::File) -> Result<()> {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::io::AsRawFd;

        let path = self.path.clone();
        let path_for_join = path.clone();

        let duped: i32 = unsafe { libc::dup(file.as_raw_fd()) };
        if duped < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("error_log '{}': dup failed: {}", path.display(), err);
        }
        // SAFETY: `duped >= 0` was just returned by `libc::dup` and no
        // other Rust type owns it yet, so we transfer exclusive
        // ownership to the `OwnedFd`.
        let owned_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(duped) };

        tokio::task::spawn_blocking(move || -> Result<()> {
            let fd = owned_fd.as_raw_fd();
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::fstat(fd, &mut stat) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                anyhow::bail!("error_log '{}': fstat failed: {}", path.display(), err);
            }
            // `stat.st_mode` and `libc::S_IF*` are both `mode_t` on
            // every target the crate builds for (u16 on macOS, u32
            // on Linux). Compare them directly — an explicit cast to
            // a fixed width is `unnecessary_cast` on the Linux
            // toolchain and trips `-D warnings` in CI.
            let ifmt = stat.st_mode & libc::S_IFMT;
            if ifmt != libc::S_IFREG {
                anyhow::bail!(
                    "error_log '{}': existing path is not a regular file (st_mode & S_IFMT = \
                     0o{:o}); refusing to write. Remove the node or point `error_log` at a real \
                     file.",
                    path.display(),
                    ifmt
                );
            }
            let actual = (stat.st_mode as u32) & 0o7777;
            if actual != 0o600 {
                anyhow::bail!(
                    "error_log '{}': existing file mode 0o{:o} does not match the required \
                     0o600 contract; refusing to write. Remove or rotate the file so a fresh \
                     inode is created with the required mode.",
                    path.display(),
                    actual
                );
            }
            Ok(())
        })
        .await
        .with_context(|| {
            format!(
                "error_log '{}': mode verify task failed to join",
                path_for_join.display()
            )
        })?
    }

    /// Force the DLQ file's mode to exactly `0o600` on the just-
    /// created fd, then re-verify via `fstat`. Called on the
    /// fresh-inode branch of the write path and on the absent-file
    /// branch of `validate_at_startup` — both use
    /// `OpenOptions::mode(0o600)` at `open(2)` time, but the mode
    /// argument to `open(2)` is masked by the process umask, so an
    /// unusually restrictive umask (e.g. `0o277` stripping owner
    /// write) can land the file at something other than `0o600`.
    /// `fchmod(2)` on a fd is umask-independent, so this call
    /// snaps the mode back to the DLQ contract regardless of the
    /// umask the daemon inherited. The `fstat` afterward is the
    /// same read-back-what-you-just-wrote pattern the runtime
    /// write path relies on for the `AlreadyExists` branch.
    ///
    /// Uses the same `dup(2)` + `spawn_blocking` cancel-safety
    /// pattern as `verify_existing_mode`.
    #[cfg(unix)]
    async fn fchmod_and_verify_dlq_mode(&self, file: &tokio::fs::File) -> Result<()> {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::io::AsRawFd;

        let path = self.path.clone();
        let path_for_join = path.clone();

        let duped: i32 = unsafe { libc::dup(file.as_raw_fd()) };
        if duped < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("error_log '{}': dup failed: {}", path.display(), err);
        }
        let owned_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(duped) };

        tokio::task::spawn_blocking(move || -> Result<()> {
            let fd = owned_fd.as_raw_fd();
            let rc = unsafe { libc::fchmod(fd, 0o600 as libc::mode_t) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                anyhow::bail!("error_log '{}': fchmod failed: {}", path.display(), err);
            }
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::fstat(fd, &mut stat) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                anyhow::bail!("error_log '{}': fstat failed: {}", path.display(), err);
            }
            let ifmt = stat.st_mode & libc::S_IFMT;
            if ifmt != libc::S_IFREG {
                anyhow::bail!(
                    "error_log '{}': freshly created path is not a regular file (st_mode & \
                     S_IFMT = 0o{:o}); refusing to write.",
                    path.display(),
                    ifmt
                );
            }
            let actual = (stat.st_mode as u32) & 0o7777;
            if actual != 0o600 {
                anyhow::bail!(
                    "error_log '{}': freshly created file mode is 0o{:o} after fchmod(0o600); \
                     the filesystem may not honour the mode contract, refusing to write.",
                    path.display(),
                    actual
                );
            }
            Ok(())
        })
        .await
        .with_context(|| {
            format!(
                "error_log '{}': fchmod+verify task failed to join",
                path_for_join.display()
            )
        })?
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
    #[cfg(unix)]
    async fn dlq_file_is_created_owner_only() {
        // The DLQ can carry secrets from a failed event's payload, so
        // a file this writer creates must not be world/group-readable
        // regardless of the daemon's umask.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let w = ErrorLogWriter::new(path.clone());
        w.write(&ctx()).await.unwrap();
        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "DLQ file must be created 0o600, got {mode:o}");
    }

    // Umask defense — pins both the runtime write path and the
    // startup preflight against a hostile process umask.
    //
    // The `.mode(0o600)` argument to `OpenOptions` is masked by
    // the process umask at `open(2)` time: a umask of `0o777` (or
    // any mask that strips owner bits, e.g. `0o600`) yields a
    // birth mode other than `0o600`. Both paths defend by calling
    // `fchmod(0o600)` on the just-created fd (umask-independent)
    // and re-verifying via `fstat`.
    //
    // `#[ignore]` because `umask(2)` is a process-global side
    // effect and this test needs to be run alone. Concurrent
    // tokio tests (the default `#[tokio::test]` shape) would race
    // on the flip and corrupt every other test's tempdirs. Run
    // manually via:
    //
    //   cargo test -p limpid --bin limpid \
    //       error_log::tests::dlq_mode_survives_hostile_umask \
    //       -- --ignored --test-threads=1
    //
    // A `serial_test`-style cross-module serialisation would let
    // this run in CI, but adding a dev-dep for one test is
    // heavier than the containment gain. Both paths are also
    // covered structurally via `fchmod_and_verify_dlq_mode`'s
    // fstat verify — a code-level regression that drops the
    // fchmod would surface at the fstat mismatch on the very
    // first hostile-umask deploy.
    #[cfg(unix)]
    struct RestoreUmask(libc::mode_t);
    #[cfg(unix)]
    impl Drop for RestoreUmask {
        fn drop(&mut self) {
            unsafe {
                libc::umask(self.0);
            }
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    #[ignore = "flips process-global umask; run with --test-threads=1"]
    async fn dlq_mode_survives_hostile_umask() {
        use std::os::unix::fs::PermissionsExt;

        // Two independent DLQ paths so we can exercise write()
        // and validate_at_startup() back-to-back inside one test.
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let write_path = dir_a.path().join("errored.jsonl");
        let preflight_path = dir_b.path().join("errored.jsonl");

        let saved_umask = unsafe { libc::umask(0o777) };
        let _restore = RestoreUmask(saved_umask);

        // 1. Runtime write path: fresh-inode create → fchmod defense.
        let w = ErrorLogWriter::new(write_path.clone());
        w.write(&ctx()).await.unwrap();
        let mode = tokio::fs::metadata(&write_path)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o600,
            "runtime-created DLQ file must land at 0o600 despite the hostile umask, got \
             0o{mode:o}"
        );

        // 2. Startup preflight: absent-file eager create → fchmod defense.
        let w = ErrorLogWriter::new(preflight_path.clone());
        w.validate_at_startup().await.unwrap();
        let mode = tokio::fs::metadata(&preflight_path)
            .await
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(
            mode, 0o600,
            "preflight-materialized DLQ file must land at 0o600 despite the hostile umask, got \
             0o{mode:o}"
        );
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
    #[cfg(unix)]
    async fn validate_at_startup_bails_when_final_parent_is_a_symlink() {
        // Parent-swap TOCTOU on the DLQ parent path is prevented by
        // inspecting the final parent component with
        // `symlink_metadata` and refusing a symlink. Ancestor
        // components may still be symlinks (`/var/run` → `/run`);
        // ancestor path identity is a deployment contract, not a
        // runtime check.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real-parent");
        std::fs::create_dir(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("symlink-parent");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let w = ErrorLogWriter::new(link.join("errored.jsonl"));
        let err = w.validate_at_startup().await.unwrap_err().to_string();
        assert!(
            err.contains("symlink") && err.contains("refusing to preflight"),
            "diagnostic must name the symlink shape: {err}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn validate_at_startup_accepts_ancestor_symlink_when_final_parent_is_real() {
        // /var/run → /run compatibility: an ancestor symlink with a
        // real final parent must not trip the check.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let real_ancestor = dir.path().join("real-ancestor");
        std::fs::create_dir(&real_ancestor).unwrap();
        std::fs::set_permissions(&real_ancestor, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let link_ancestor = dir.path().join("link-ancestor");
        std::os::unix::fs::symlink(&real_ancestor, &link_ancestor).unwrap();
        let final_parent = link_ancestor.join("dlq");
        std::fs::create_dir(&final_parent).unwrap();
        std::fs::set_permissions(&final_parent, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let w = ErrorLogWriter::new(final_parent.join("errored.jsonl"));
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
    #[cfg(unix)]
    async fn validate_at_startup_refuses_symlink_at_dlq_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let target = dir.path().join("_hijacked.jsonl");
        std::os::unix::fs::symlink(&target, &path).unwrap();

        let w = ErrorLogWriter::new(path.clone());
        let err = w.validate_at_startup().await.unwrap_err().to_string();
        assert!(err.contains("is a symlink"), "got: {}", err);
        assert!(
            !target.exists(),
            "symlink target must not have been touched"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn validate_at_startup_refuses_wrong_mode_existing_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        std::fs::write(&path, b"leftover\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let w = ErrorLogWriter::new(path.clone());
        let err = w.validate_at_startup().await.unwrap_err().to_string();
        assert!(err.contains("has mode 0o644"), "got: {}", err);
        assert!(err.contains("0o600"), "got: {}", err);

        // A file at the correct mode must pass.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        w.validate_at_startup().await.unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn validate_at_startup_refuses_fifo_at_dlq_path() {
        // A stale FIFO left at the DLQ path — from a debugging
        // session, an operator typo, or a leftover from another
        // daemon — must be refused. The FIFO can be 0o600 and would
        // pass the mode check alone; only the S_ISREG shape check
        // catches it.
        use std::ffi::CString;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let c_path = CString::new(path.as_os_str().to_str().unwrap()).unwrap();
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());

        let w = ErrorLogWriter::new(path.clone());
        let err = w.validate_at_startup().await.unwrap_err().to_string();
        assert!(err.contains("FIFO"), "got: {}", err);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn validate_at_startup_refuses_directory_at_dlq_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        std::fs::create_dir(&path).unwrap();

        let w = ErrorLogWriter::new(path.clone());
        let err = w.validate_at_startup().await.unwrap_err().to_string();
        assert!(err.contains("directory"), "got: {}", err);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn validate_at_startup_accepts_absent_file() {
        // A path whose parent exists but whose file itself does not
        // exist must pass — the runtime creates the file on first
        // failure with the correct mode. The preflight creates it
        // eagerly at 0o600 (see next test for that assertion).
        let dir = TempDir::new().unwrap();
        let w = ErrorLogWriter::new(dir.path().join("errored.jsonl"));
        w.validate_at_startup().await.unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn validate_at_startup_creates_absent_file_at_0o600() {
        // The absent-path preflight opens with `create_new` +
        // `mode(0o600)` and leaves the file in place. Pin this so a
        // future refactor that drops the preflight (or leaves a
        // wider-mode file behind) trips this test.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let w = ErrorLogWriter::new(path.clone());
        w.validate_at_startup().await.unwrap();

        let meta = tokio::fs::metadata(&path).await.unwrap();
        assert!(meta.is_file(), "preflight must have created the file");
        assert_eq!(meta.len(), 0, "preflight must leave the file empty");
        let mode = meta.permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o600,
            "preflight file must be 0o600 (matches the runtime write contract), got 0o{mode:o}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn validate_at_startup_fails_on_readonly_parent_absent_file() {
        // A parent directory the daemon user cannot write to is only
        // caught by the create-probe — a stat of the parent still
        // succeeds (it's readable). The docs promise this preflight
        // exists; pin it against a regression that reverts to
        // stat-only validation.
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("locked");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o555)).unwrap();

        let w = ErrorLogWriter::new(sub.join("errored.jsonl"));
        let result = w.validate_at_startup().await;

        // Restore write bit so TempDir can clean up.
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = result
            .expect_err("readonly parent must fail preflight")
            .to_string();
        assert!(
            err.contains("cannot create DLQ file"),
            "err must name the preflight failure, got: {err}"
        );
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

    #[tokio::test]
    #[cfg(unix)]
    async fn fresh_dlq_file_lands_with_0o600_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dlq.jsonl");
        let writer = ErrorLogWriter::new(path.clone());
        writer
            .write(&ctx())
            .await
            .expect("first DLQ write must succeed");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(
            mode, 0o600,
            "fresh DLQ file must land under 0o600, not umask default"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn preexisting_dlq_file_with_wrong_mode_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dlq.jsonl");
        // A stray DLQ file left by an operator tool or logrotate at
        // world-readable mode. The write must refuse rather than
        // silently appending secrets to a file exposed at 0o644.
        std::fs::write(&path, b"leftover\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let writer = ErrorLogWriter::new(path.clone());
        let err = writer
            .write(&ctx())
            .await
            .expect_err("wrong-mode preexisting DLQ file must be refused");
        let msg = err.to_string();
        assert!(
            msg.contains("existing file mode"),
            "diagnostic must name the observed mode: {msg}"
        );
        assert!(msg.contains("0o644"), "{msg}");
        assert!(msg.contains("0o600"), "{msg}");

        // The refusal must land before write_all, so the file
        // contents are unchanged.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"leftover\n",
            "record must not have been appended when verify refused"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn preexisting_dlq_file_with_correct_mode_accepts_writes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dlq.jsonl");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let writer = ErrorLogWriter::new(path.clone());
        writer
            .write(&ctx())
            .await
            .expect("existing 0o600 DLQ must accept writes");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("\"kind\":\"process\""),
            "record must have been appended, got: {contents}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn dlq_write_refuses_symlink_at_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dlq.jsonl");
        // A symlink at the DLQ path — the security concern is that
        // an attacker points it at a file they can otherwise read,
        // and every DLQ record leaks to that file. `O_NOFOLLOW`
        // guards both the create_new branch (symlink → EEXIST) and
        // the fallback existing-open (symlink → ELOOP).
        let bait = dir.path().join("_would_be_hijacked.jsonl");
        std::os::unix::fs::symlink(&bait, &path).unwrap();

        let writer = ErrorLogWriter::new(path.clone());
        let err = writer
            .write(&ctx())
            .await
            .expect_err("symlink at DLQ path must be refused");
        assert!(
            err.to_string().contains("refusing to follow symlink"),
            "diagnostic must name the refusal reason: {}",
            err
        );

        // The bait file must never have been created — writing to it
        // is exactly what the symlink was trying to achieve.
        assert!(
            !bait.exists(),
            "the symlink target must not have been touched"
        );
    }
}
