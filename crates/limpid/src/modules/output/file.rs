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
//! Templates may reference event-intrinsic fields (`source.*`,
//! `received_at`, etc.) and pure functions. Pipeline-mutable state
//! (`workspace`, `egress`, `error`) is rejected by the analyzer in
//! `crates/limpid/src/check/outputs.rs`; daemon startup and reload
//! invoke the same analyzer via `compile_and_analyze` in `main.rs`,
//! so the generic expression evaluator only ever sees pre-validated
//! references at runtime. Path components are sanitised so
//! interpolated values can't introduce `/`, `\`, or `..` segments
//! that would escape into sibling directories.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use bytes::Bytes;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::{Expr, ExprKind, TemplateFragment};
use crate::dsl::eval::{eval_expr, value_to_string};
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::{BorrowedEvent, Event};
use crate::functions::FunctionRegistry;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output, RenderError};
use crate::queue::{QueueAckHandle, RetryConfig};

const FILE_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "path",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    // Octal mode string — `"0640"`, `"640"`. Parsed by from_properties.
    PropertySpec {
        name: "mode",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "owner",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "group",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

/// Per-event payload built by `FileOutput::render`. The expensive work
/// (template eval against the per-event arena) lives in `render`; the
/// async `write` path is left with a static `path` string and the
/// refcounted `egress` bytes.
struct FilePayload {
    egress: Bytes,
    path: String,
    is_dynamic: bool,
}

pub struct FileOutput {
    name: String,
    /// Parsed path expression. A plain `Expr::StringLit` means a static
    /// path; `Expr::Template` requires per-event evaluation.
    path: Expr,
    mode: Option<u32>,
    owner: Option<String>,
    group: Option<String>,
    funcs: Arc<FunctionRegistry>,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    metrics: Arc<OutputMetrics>,
    /// Runtime shutdown broadcast. Cloned inside the `consume` retry
    /// loop so the exponential backoff sleep races against it: a
    /// shutdown fired mid-sleep terminates the retry and routes the
    /// pending event to DLQ instead of extending past the runtime's
    /// shutdown budget and getting the consumer task-aborted.
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
}

impl Module for FileOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(FILE_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let error_log = ctx.error_log.as_ref().map(Arc::clone);
        let funcs = Arc::clone(&ctx.funcs);
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        let properties = properties.user_properties();
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
            .map(|raw| {
                // Parse as octal directly. Do NOT strip leading zeros
                // first: `"0"` and `"0000"` are both valid Unix modes
                // and must survive the parse; stripping to `""` used
                // to turn them into a load-time error.
                let parsed = u32::from_str_radix(&raw, 8).with_context(|| {
                    format!(
                        "output '{}': invalid mode (expected octal, e.g. \"0640\")",
                        name
                    )
                })?;
                // Enforce the same 12-bit mask the write path checks
                // via `fstat` (setuid / setgid / sticky + rwx). A
                // value above 0o7777 could never be honoured — the
                // `fchmod` on create silently masks the extras, and
                // the fstat verify on subsequent writes would report
                // a permanent mismatch. Reject at load time so the
                // operator sees the mistake in the daemon startup
                // log rather than at first write.
                if parsed > 0o7777 {
                    anyhow::bail!(
                        "output '{}': mode 0o{:o} exceeds the 12-bit permission range 0o7777",
                        name,
                        parsed
                    );
                }
                Ok(parsed)
            })
            .transpose()?;

        let owner = props::get_string(properties, "owner");
        let group = props::get_string(properties, "group");

        Ok(Self {
            name: name.to_string(),
            path,
            mode,
            owner,
            group,
            funcs,
            retry,
            error_log,
            metrics: Arc::new(OutputMetrics::default()),
            shutdown_signal: ctx.shutdown_signal.clone(),
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
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        // Per-event lifecycle: render → write (with internal retry) →
        // resolve. Render failures are deterministic on the event and
        // route straight to DLQ without burning the retry budget;
        // transport / I/O failures consume `retry.max_attempts` and
        // then land in DLQ on exhaust.
        let payload_res = {
            let bump = bumpalo::Bump::new();
            let arena = EventArena::new(&bump);
            let bevent = event.view_in(&arena);
            self.render_path_in(&bevent, &arena)
        };
        let payload = match payload_res {
            Ok((resolved, is_dynamic)) => FilePayload {
                egress: event.egress.clone(),
                path: resolved,
                is_dynamic,
            },
            Err(e) => {
                let reason = format!("render failed: {}", RenderError::new(e));
                crate::modules::route_event_to_dlq(
                    self.error_log.as_ref(),
                    &self.name,
                    event,
                    &reason,
                )
                .await;
                self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                ack.resolve_recovered();
                return Ok(());
            }
        };

        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            // Clone the payload's path/dynamic flag for each attempt;
            // `egress` is a refcounted `Bytes` so the actual buffer
            // isn't duplicated.
            let attempt_payload = FilePayload {
                egress: payload.egress.clone(),
                path: payload.path.clone(),
                is_dynamic: payload.is_dynamic,
            };
            // Note: unlike the network sinks, the file writer is
            // *not* wrapped in `attempt_or_shutdown` here. The file
            // output's contract is one line = one event, and
            // aborting `write_payload` mid-flight can leave a partial
            // append on disk (write_all is not atomic across
            // syscalls; a large payload can be split into several
            // `write(2)` calls). If we cancelled and DLQ-routed as
            // Recovered at the same time, replay would re-append the
            // event, producing a partial-then-full duplicate. Since
            // the file write is local disk I/O, the individual
            // attempt is typically <10 ms and finishes well inside
            // the shutdown budget; the retry backoff sleep
            // (`sleep_or_shutdown` below) is where the real
            // shutdown-awareness bound lives.
            match self.write_payload(attempt_payload).await {
                Ok(()) => {
                    ack.resolve_delivered();
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt >= self.retry.max_attempts {
                        let reason =
                            format!("output write failed after {} attempts: {}", attempt, e);
                        crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            &self.name,
                            event,
                            &reason,
                        )
                        .await;
                        self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        ack.resolve_recovered();
                        return Ok(());
                    }
                    tracing::warn!(
                        "output '{}': write failed (attempt {}/{}): {} — retrying in {:?}",
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        e,
                        wait
                    );
                    // Race the backoff sleep against shutdown. If the
                    // runtime signals shutdown mid-sleep, do NOT keep
                    // retrying — the retry budget (default 1+2+4+8 =
                    // 15 s) can outlast the runtime's 10 s shutdown
                    // budget, and if we don't return the queue
                    // consumer's select! never gets back to its
                    // shutdown arm. Route the pending event to DLQ,
                    // resolve `Recovered`, and return.
                    if crate::modules::sleep_or_shutdown(&mut shutdown, wait).await {
                        let reason = format!(
                            "output write failed and shutdown observed mid-retry \
                             after {} attempts: {}",
                            attempt, e
                        );
                        crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            &self.name,
                            event,
                            &reason,
                        )
                        .await;
                        self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        ack.resolve_recovered();
                        return Ok(());
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let payload_res = {
            let bump = bumpalo::Bump::new();
            let arena = EventArena::new(&bump);
            let bevent = event.view_in(&arena);
            self.render_path_in(&bevent, &arena)
        };
        let payload = match payload_res {
            Ok((resolved, is_dynamic)) => FilePayload {
                egress: event.egress.clone(),
                path: resolved,
                is_dynamic,
            },
            Err(e) => {
                let reason = format!("render failed: {}", RenderError::new(e));
                crate::modules::route_event_to_dlq(
                    self.error_log.as_ref(),
                    &self.name,
                    event,
                    &reason,
                )
                .await;
                self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                ack.resolve_recovered();
                return Ok(());
            }
        };
        match tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            self.write_payload(payload),
        )
        .await
        {
            Ok(Ok(())) => {
                ack.resolve_delivered();
            }
            Ok(Err(e)) => {
                let reason = format!("shutdown write failed: {}", e);
                crate::modules::route_event_to_dlq(
                    self.error_log.as_ref(),
                    &self.name,
                    event,
                    &reason,
                )
                .await;
                self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                ack.resolve_recovered();
            }
            Err(_) => {
                let reason = format!(
                    "shutdown write timed out after {:?}",
                    crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT
                );
                crate::modules::route_event_to_dlq(
                    self.error_log.as_ref(),
                    &self.name,
                    event,
                    &reason,
                )
                .await;
                self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                ack.resolve_recovered();
            }
        }
        Ok(())
    }
}

impl FileOutput {
    /// Append the rendered payload to its resolved path. Private —
    /// reached only from [`Output::consume`].
    async fn write_payload(&self, payload: FilePayload) -> Result<()> {
        let resolved = payload.path;
        let is_dynamic = payload.is_dynamic;
        let path = PathBuf::from(&resolved);

        // Catch-all `..` reject. For Template paths this is redundant
        // with `check_no_traversal` in `render_path_in`; for static
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

        // File-open strategy: distinguish "we just created this file"
        // from "the file was already there" at the syscall level, so
        // the metadata contract is enforced against the actual on-disk
        // inode instead of process-local memory. Two attempts:
        //
        //   1. `create_new(true)` (O_CREAT | O_EXCL). Succeeds only if
        //      no file exists at the path. That is our unambiguous
        //      signal for "this output owns the inode from birth" —
        //      apply mode/owner immediately, before any payload bytes
        //      reach disk.
        //   2. On `AlreadyExists` (EEXIST), the path is already
        //      populated — either by a prior successful run, by a
        //      logrotate that removed/recreated the file, by an
        //      operator, or by a pre-planted attacker file/symlink.
        //      Open non-create with O_NOFOLLOW, then if a metadata
        //      contract is configured, `fstat` the fd and refuse the
        //      write if the observed mode/owner/group don't match. We
        //      never silently chmod a file we didn't create — that
        //      would be a foreign-inode side effect. The operator gets
        //      a loud error and can rotate or fix the file.
        //
        // Compared with the previous path-keyed in-memory bookkeeping,
        // this survives daemon restart (state lives on the inode), and
        // logrotate-driven inode swaps re-run the apply because the
        // fresh inode is created via O_EXCL again.
        //
        // O_NOFOLLOW guards symlink attacks on the final path
        // component in both branches (create-new refuses symlinks by
        // its own semantics; the fallback open explicitly asks
        // O_NOFOLLOW so a symlink at the path is refused with ELOOP).
        let requires_metadata = self.requires_metadata();

        let mut create_options = OpenOptions::new();
        create_options.write(true).create_new(true).append(true);
        #[cfg(unix)]
        create_options.custom_flags(libc::O_NOFOLLOW);
        let create_res = create_options.open(&path).await;

        let mut file = match create_res {
            Ok(f) => {
                // Fresh inode. Apply mode/owner/group BEFORE payload
                // bytes reach disk so an intruder co-tenant can't
                // snapshot the file under process umask/default owner
                // in the window between write_all and fchmod.
                //
                // apply_file_metadata_to_fd is cancel-safe (dup fd +
                // spawn_blocking): if this await returns Ok, the
                // fchmod/fchown have run. If it returns Err, the file
                // stays empty on disk; subsequent writes fall into the
                // "already exists" branch, fstat it, see the umask
                // default, and refuse — the failure is surfaced loud
                // instead of degrading silently.
                if requires_metadata {
                    self.apply_file_metadata_to_fd(&f, &path).await?;
                }
                f
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut existing_options = OpenOptions::new();
                existing_options.write(true).append(true);
                #[cfg(unix)]
                existing_options.custom_flags(libc::O_NOFOLLOW);
                let f = existing_options.open(&path).await.map_err(|e| {
                    #[cfg(unix)]
                    if e.raw_os_error() == Some(libc::ELOOP) {
                        return anyhow::anyhow!(
                            "refusing to follow symlink at output path: {}",
                            resolved
                        );
                    }
                    anyhow::Error::from(e)
                })?;
                if requires_metadata {
                    self.verify_existing_file_metadata(&f, &path).await?;
                }
                f
            }
            Err(e) => {
                #[cfg(unix)]
                if e.raw_os_error() == Some(libc::ELOOP) {
                    anyhow::bail!("refusing to follow symlink at output path: {}", resolved);
                }
                return Err(anyhow::Error::from(e));
            }
        };

        let msg = String::from_utf8_lossy(&payload.egress);
        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(b'\n');
        file.write_all(&buf).await?;
        // Push the buffered bytes through tokio's fs::File to the OS
        // before we return "written". Without this, the data may still
        // sit in tokio's per-file buffer at drop time — Drop closes the
        // fd on a background thread, and if the pipeline advances
        // (metrics tick, ack, downstream reader) before that close
        // finishes flushing, the reader can observe a shorter file
        // than the event count claims. The syscall is cheap and turns
        // "write returned Ok" into an OS-visible commitment.
        file.flush().await?;
        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);

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
    /// Three safety passes — pass 1 normalises `/` and `\` to `_`
    /// inside each interpolation result so an injected value cannot
    /// introduce a directory boundary; passes 2 and 3 reject (per
    /// Principle 1) on traversal and trailing-slash shapes:
    ///
    /// 1. Per-interpolation: every `${...}` result has `/` and `\`
    ///    replaced with `_`, regardless of the wrapping expression
    ///    (`${source.ip}`, `${lower(received_at)}`, `${a + b}` —
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
    ///    so a stray template like `/var/log/${source.ip}/`
    ///    cannot create empty directories silently.
    fn render_path_in(
        &self,
        bevent: &BorrowedEvent<'_>,
        arena: &EventArena<'_>,
    ) -> Result<(String, bool)> {
        match &self.path.kind {
            ExprKind::StringLit(s) => Ok((s.clone(), false)),
            ExprKind::Template(fragments) => {
                let funcs = &self.funcs;
                let mut out = String::new();
                for frag in fragments {
                    match frag {
                        TemplateFragment::Literal(s) => out.push_str(s),
                        TemplateFragment::Interp(expr) => {
                            let rendered = value_to_string(&eval_expr(expr, bevent, funcs, arena)?);
                            // Pass 1: per-interp `/` `\` → `_` and reject empty.
                            // An empty interp would silently produce paths like
                            // `/foo//bar` or `/foo/.log` that almost never reflect
                            // operator intent — usually a null event-intrinsic
                            // value or a Pass-2 collapse of `${"..": something}`.
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

/// Pass 1: per-interpolation sanitisation. Replace `/` and `\` with
/// `_` so an interpolation cannot expand into multiple path components
/// or a Windows path separator. `.` is left alone — operators rely on
/// dots for FQDN-style filenames (`web01.example.com.log`).
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
    /// Apply the configured `mode`/`owner`/`group` to the just-created
    /// file via `fchmod(2)`/`fchown(2)` on the open fd — never through
    /// the path. The path-based `set_permissions`/`chown` we used
    /// before could be redirected by a symlink pre-planted between
    /// `open(O_NOFOLLOW)` and the metadata call; operating on the fd
    /// makes that impossible, and keeps the mode/ownership riding on
    /// the same inode `write_all` just touched. `path` here is only
    /// used to label warnings.
    ///
    /// The `spawn_blocking` future is *not* cancel-safe: if the caller
    /// (say `consume_shutdown` under a `timeout`) drops the awaiting
    /// future, the blocking task keeps running while the outer `File`
    /// gets dropped and closes its fd. A raw fd number is trivial for
    /// the kernel to reuse for an unrelated `open`, and the leftover
    /// `fchmod`/`fchown` would then land on that other inode. Guard
    /// against that by `dup(2)`-ing the fd into an `OwnedFd` that the
    /// blocking closure owns for its whole lifetime — the syscalls run
    /// against a fd that stays live even if the outer future is
    /// cancelled, and the `OwnedFd`'s `Drop` closes it deterministically
    /// on task exit (success, panic, or otherwise).
    async fn apply_file_metadata_to_fd(&self, file: &tokio::fs::File, path: &Path) -> Result<()> {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::io::AsRawFd;

        let mode = self.mode;
        let owner = self.owner.clone();
        let group = self.group.clone();
        debug_assert!(
            mode.is_some() || owner.is_some() || group.is_some(),
            "callers must gate this on requires_metadata()"
        );
        let path = path.to_path_buf();
        let path_for_join = path.clone();

        // `dup(2)` returns a fresh fd referring to the same open file
        // description. The `OwnedFd` moves into the closure so it stays
        // live for the duration of the blocking task even if the outer
        // future — and therefore the source `File` — is dropped.
        let duped: i32 = unsafe { libc::dup(file.as_raw_fd()) };
        if duped < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("output file '{}': dup failed: {}", path.display(), err);
        }
        // SAFETY: `duped >= 0` was just returned by `libc::dup` and no
        // other Rust type owns it yet, so this transfers exclusive
        // ownership to the `OwnedFd`.
        let owned_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(duped) };

        tokio::task::spawn_blocking(move || -> Result<()> {
            let fd = owned_fd.as_raw_fd();
            if let Some(mode) = mode {
                let rc = unsafe { libc::fchmod(fd, mode as libc::mode_t) };
                if rc != 0 {
                    let err = std::io::Error::last_os_error();
                    anyhow::bail!("output file '{}': fchmod failed: {}", path.display(), err);
                }
            }

            if owner.is_some() || group.is_some() {
                let uid = match owner.as_deref() {
                    Some(name) => Some(resolve_uid(name).with_context(|| {
                        format!(
                            "output file '{}': failed to resolve owner '{}'",
                            path.display(),
                            name
                        )
                    })?),
                    None => None,
                };
                let gid = match group.as_deref() {
                    Some(name) => Some(resolve_gid(name).with_context(|| {
                        format!(
                            "output file '{}': failed to resolve group '{}'",
                            path.display(),
                            name
                        )
                    })?),
                    None => None,
                };
                // `fchown(-1, -1)` is a no-op, so map "not configured"
                // to -1 and let libc decide which coordinate needs
                // changing. `resolve_*` failure is already an
                // early-return above; we never reach here after a
                // lookup miss with the wrong argument silently applied.
                let uid_arg = uid.unwrap_or(u32::MAX);
                let gid_arg = gid.unwrap_or(u32::MAX);
                let rc = unsafe { libc::fchown(fd, uid_arg, gid_arg) };
                if rc != 0 {
                    let err = std::io::Error::last_os_error();
                    anyhow::bail!("output file '{}': fchown failed: {}", path.display(), err);
                }
            }
            // `owned_fd` drops here at end-of-closure, so `close(2)`
            // runs deterministically whether the closure returned
            // normally or panicked — cancellation of the outer future
            // doesn't reach in here.
            Ok(())
        })
        .await
        .with_context(|| {
            format!(
                "output file '{}': metadata apply task failed to join",
                path_for_join.display()
            )
        })?
    }

    /// Verify that an existing file's on-disk mode/owner/group match
    /// the operator-configured values. Called only when this output
    /// took the "path already existed" branch of the write path — i.e.
    /// we did not create the inode, and the metadata contract must be
    /// re-checked rather than re-applied. On mismatch the write is
    /// refused with a loud error so an operator can rotate the file,
    /// fix ownership, or investigate an unexpected pre-existing inode
    /// (empty file left by a failed prior apply, logrotate copying
    /// instead of moving, unrelated producer sharing the path, etc.).
    ///
    /// Uses the same `dup(2)` + `spawn_blocking` pattern as
    /// `apply_file_metadata_to_fd`: the fd stays live for the entire
    /// blocking call even if the outer future is cancelled, so a
    /// concurrently-closed source `File` can't cause the fstat to hit
    /// an unrelated inode via fd reuse.
    async fn verify_existing_file_metadata(
        &self,
        file: &tokio::fs::File,
        path: &Path,
    ) -> Result<()> {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::io::AsRawFd;

        let mode = self.mode;
        let owner = self.owner.clone();
        let group = self.group.clone();
        debug_assert!(
            mode.is_some() || owner.is_some() || group.is_some(),
            "callers must gate this on requires_metadata()"
        );
        let path = path.to_path_buf();
        let path_for_join = path.clone();

        let duped: i32 = unsafe { libc::dup(file.as_raw_fd()) };
        if duped < 0 {
            let err = std::io::Error::last_os_error();
            anyhow::bail!("output file '{}': dup failed: {}", path.display(), err);
        }
        let owned_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(duped) };

        tokio::task::spawn_blocking(move || -> Result<()> {
            let fd = owned_fd.as_raw_fd();
            let mut stat: libc::stat = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::fstat(fd, &mut stat) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                anyhow::bail!("output file '{}': fstat failed: {}", path.display(), err);
            }
            if let Some(configured) = mode {
                // Compare the full permission mode including the
                // setuid / setgid / sticky bits (mask 0o7777). Masking
                // to 0o777 would let a file with an unexpected
                // setuid/setgid bit slip through as long as the rwx
                // triples matched — a trust-relevant gap on log paths
                // that are supposed to be plain regular files.
                let actual = (stat.st_mode as u32) & 0o7777;
                if actual != configured {
                    anyhow::bail!(
                        "output file '{}': existing file mode 0o{:o} does not match configured mode 0o{:o}; refusing to write. Remove or rotate the file to have this output recreate it with the configured mode.",
                        path.display(),
                        actual,
                        configured
                    );
                }
            }
            if let Some(name) = owner.as_deref() {
                let configured_uid = resolve_uid(name).with_context(|| {
                    format!(
                        "output file '{}': failed to resolve owner '{}'",
                        path.display(),
                        name
                    )
                })?;
                if stat.st_uid != configured_uid {
                    anyhow::bail!(
                        "output file '{}': existing file owner uid {} does not match configured owner '{}' (uid {}); refusing to write.",
                        path.display(),
                        stat.st_uid,
                        name,
                        configured_uid
                    );
                }
            }
            if let Some(name) = group.as_deref() {
                let configured_gid = resolve_gid(name).with_context(|| {
                    format!(
                        "output file '{}': failed to resolve group '{}'",
                        path.display(),
                        name
                    )
                })?;
                if stat.st_gid != configured_gid {
                    anyhow::bail!(
                        "output file '{}': existing file group gid {} does not match configured group '{}' (gid {}); refusing to write.",
                        path.display(),
                        stat.st_gid,
                        name,
                        configured_gid
                    );
                }
            }
            Ok(())
        })
        .await
        .with_context(|| {
            format!(
                "output file '{}': metadata verify task failed to join",
                path_for_join.display()
            )
        })?
    }

    /// Whether this output has a mode/owner/group contract to enforce.
    /// When false, an existing file's metadata is not verified and
    /// freshly-created files inherit the process umask / default
    /// ownership.
    fn requires_metadata(&self) -> bool {
        self.mode.is_some() || self.owner.is_some() || self.group.is_some()
    }
}

// Buffer size for the reentrant getpwnam_r / getgrnam_r calls below.
// POSIX recommends consulting `sysconf(_SC_GETPW_R_SIZE_MAX)` (typically
// 1024 on Linux, 4096 on macOS), but for user-name lookups during
// daemon startup a fixed 4 KiB buffer is comfortably larger than any
// realistic passwd/group record. Stack-allocated so there's no heap
// concern.
const NSS_RECORD_BUF: usize = 4096;

/// Resolve a username to its uid via `getpwnam_r`. The reentrant
/// variant is used in place of `getpwnam` so the call is safe to make
/// concurrently with other `getpw*` users in the process (the legacy
/// `getpwnam` returns a pointer into a static buffer shared across
/// threads). Called from the write path inside `spawn_blocking`
/// (`apply_file_metadata_to_fd` on create, `verify_existing_file_metadata`
/// on subsequent writes), so the thread-safety guarantee is
/// load-bearing, not defensive.
fn resolve_uid(name: &str) -> Result<u32> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let c_name = CString::new(name)?;
    let mut buf = [0u8; NSS_RECORD_BUF];
    let mut pwd: MaybeUninit<libc::passwd> = MaybeUninit::uninit();
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe {
        libc::getpwnam_r(
            c_name.as_ptr(),
            pwd.as_mut_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        anyhow::bail!("getpwnam_r failed for '{}': errno {}", name, rc);
    }
    if result.is_null() {
        anyhow::bail!("user '{}' not found", name);
    }
    // SAFETY: `result` is non-null and points into the `pwd` storage we
    // just initialised via `getpwnam_r`; the pw_uid field is a plain
    // numeric copy and outlives the borrow on `pwd`/`buf`.
    Ok(unsafe { (*result).pw_uid })
}

/// Resolve a group name to its gid via `getgrnam_r`. Same thread-
/// safety rationale as [`resolve_uid`].
fn resolve_gid(name: &str) -> Result<u32> {
    use std::ffi::CString;
    use std::mem::MaybeUninit;
    let c_name = CString::new(name)?;
    let mut buf = [0u8; NSS_RECORD_BUF];
    let mut grp: MaybeUninit<libc::group> = MaybeUninit::uninit();
    let mut result: *mut libc::group = std::ptr::null_mut();
    let rc = unsafe {
        libc::getgrnam_r(
            c_name.as_ptr(),
            grp.as_mut_ptr(),
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        anyhow::bail!("getgrnam_r failed for '{}': errno {}", name, rc);
    }
    if result.is_null() {
        anyhow::bail!("group '{}' not found", name);
    }
    // SAFETY: same as in `resolve_uid`.
    Ok(unsafe { (*result).gr_gid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::value::OwnedValue;
    use crate::event::Event;
    use crate::functions::table::TableStore;
    use std::net::SocketAddr;

    /// Test helper: resolve a path against an OwnedEvent without
    /// duplicating arena boilerplate at every call site. Mirrors what
    /// the previous `render_path(&Event)` signature (before the v0.6.0
    /// Output trait refactor) did.
    fn render_path_owned(out: &FileOutput, event: &Event) -> Result<(String, bool)> {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let bevent = event.view_in(&arena);
        out.render_path_in(&bevent, &arena)
    }

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
            .insert("host".into(), OwnedValue::String("web01".into()));
        // value containing a path separator — must be sanitised
        e.workspace
            .insert("ip".into(), OwnedValue::String("10.0.0.1/24".into()));
        e
    }

    fn make_output(path: Expr) -> FileOutput {
        make_output_with(path, None)
    }

    fn make_output_with(path: Expr, mode: Option<u32>) -> FileOutput {
        make_output_with_metadata(path, mode, None, None)
    }

    fn make_output_with_metadata(
        path: Expr,
        mode: Option<u32>,
        owner: Option<String>,
        group: Option<String>,
    ) -> FileOutput {
        FileOutput {
            name: "test".into(),
            path,
            mode,
            owner,
            group,
            funcs: funcs(),
            retry: RetryConfig::default(),
            error_log: None,
            metrics: Arc::new(OutputMetrics::default()),
            shutdown_signal: crate::modules::BuildContext::for_testing().shutdown_signal,
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
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = event_with_workspace();
        let bevent = owned.view_in(&arena);
        let (rendered, dynamic) = out.render_path_in(&bevent, &arena).unwrap();
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
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = event_with_workspace();
        let bevent = owned.view_in(&arena);
        let (rendered, dynamic) = out.render_path_in(&bevent, &arena).unwrap();
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
        let (rendered, _) = render_path_owned(&out, &event_with_workspace()).unwrap();
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
        let (rendered, _) = render_path_owned(&out, &event_with_workspace()).unwrap();
        // source.ip is "192.168.1.10" — no slashes, no change. Principle
        // holds for hypothetical slash-bearing values.
        assert_eq!(rendered, "a-192.168.1.10-b");
    }

    #[test]
    fn sanitize_path_component_replaces_unix_separator() {
        assert_eq!(sanitize_path_component("a/b/c"), "a_b_c");
    }

    #[test]
    fn sanitize_path_component_replaces_windows_separator() {
        // Backslash must be sanitised on every platform, not just
        // Windows — a Windows-style path leaking into the limpid
        // value pool would otherwise let an attacker (or a typo)
        // smuggle in a path-component break on Linux too. The
        // existing render_template_sanitises_every_interpolation case
        // used a value with neither `/` nor `\` and so did not pin
        // the backslash branch; pin it here.
        assert_eq!(sanitize_path_component("a\\b\\c"), "a_b_c");
    }

    #[test]
    fn sanitize_path_component_replaces_mixed_separators() {
        // A value containing both forward and back slashes (e.g. a
        // raw vendor field that mixes Windows + Unix paths) must
        // strip BOTH; a regression that narrowed replace() to a
        // single character would let the unfiltered side through.
        assert_eq!(sanitize_path_component("a/b\\c/d\\e"), "a_b_c_d_e");
    }

    #[test]
    fn sanitize_path_component_leaves_dots_alone() {
        // Operators rely on dots for FQDN-style filenames; the
        // sanitiser must NOT eat them. Regression guard against an
        // over-aggressive future "also strip `.`" change that would
        // silently corrupt `web01.example.com.log` into `web01_example_com_log`.
        assert_eq!(
            sanitize_path_component("web01.example.com"),
            "web01.example.com"
        );
    }

    #[test]
    fn render_template_sanitises_backslash_from_workspace() {
        // End-to-end pin: a workspace value containing `\` (which a
        // Windows-origin vendor field might carry) must be
        // sanitised in the rendered template, not pass through. The
        // existing workspace-reference test happens to use a value
        // with `/` only; this one closes the backslash branch.
        use crate::dsl::OwnedValue;
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("/var/log/".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec![
                "workspace".into(),
                "winpath".into(),
            ]))),
            TemplateFragment::Literal(".log".into()),
        ])));
        let mut event = Event::new(
            Bytes::from("x"),
            "192.168.1.10:514".parse::<SocketAddr>().unwrap(),
        );
        event.workspace.insert(
            "winpath".into(),
            OwnedValue::String("C:\\Users\\bob".into()),
        );
        let (rendered, _) = render_path_owned(&out, &event).unwrap();
        assert_eq!(rendered, "/var/log/C:_Users_bob.log");
    }

    #[test]
    fn render_template_errors_on_empty_interpolation() {
        // Template `/var/log/${workspace.empty}.log` with empty value would
        // produce `/var/log/.log` — almost never the operator's intent.
        let mut e = Event::new(
            Bytes::from("hello"),
            "192.168.1.10:514".parse::<SocketAddr>().unwrap(),
        );
        e.workspace
            .insert("empty".into(), OwnedValue::String("".into()));
        let out = make_output(ek(ExprKind::Template(vec![
            TemplateFragment::Literal("/var/log/".into()),
            TemplateFragment::Interp(ek(ExprKind::Ident(vec![
                "workspace".into(),
                "empty".into(),
            ]))),
            TemplateFragment::Literal(".log".into()),
        ])));
        let err = render_path_owned(&out, &e).unwrap_err();
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
        let err = render_path_owned(&out, &event_with_workspace()).unwrap_err();
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
        e.workspace
            .insert("v".into(), OwnedValue::String("..".into()));
        let out = make_output(ek(ExprKind::Template(vec![TemplateFragment::Interp(ek(
            ExprKind::Ident(vec!["workspace".into(), "v".into()]),
        ))])));
        let err = render_path_owned(&out, &e).unwrap_err();
        assert!(
            err.to_string().contains("traversal component"),
            "unexpected error: {}",
            err
        );
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

    #[tokio::test]
    #[cfg(unix)]
    async fn write_payload_refuses_symlink_at_final_component() {
        // Fourth safety pass (O_NOFOLLOW): a pre-planted symlink at
        // the output path must produce an explicit refusal, not a
        // silent append to the symlink's target. The rendered path is
        // clean — only the filesystem object at it is hostile — so
        // none of the three rendering passes can catch this.
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("target.log");
        let link = dir.path().join("out.log");
        std::fs::write(&target, b"pre-existing").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let out = make_output(ek(ExprKind::StringLit(link.display().to_string())));
        let err = out
            .write_payload(FilePayload {
                egress: Bytes::from("hello"),
                path: link.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("symlink at output path must be refused");
        assert!(
            err.to_string().contains("refusing to follow symlink"),
            "DLQ reason must name the refusal, got: {err}"
        );
        // The target file must be untouched.
        assert_eq!(std::fs::read(&target).unwrap(), b"pre-existing");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn write_payload_appends_to_regular_file() {
        // Companion to the symlink-refusal test: O_NOFOLLOW must not
        // disturb the normal create-and-append path.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("plain.log");

        let out = make_output(ek(ExprKind::StringLit(path.display().to_string())));
        for _ in 0..2 {
            out.write_payload(FilePayload {
                egress: Bytes::from("hello"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect("regular file write must succeed");
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"hello\nhello\n");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn write_payload_applies_mode_via_fchmod_to_created_file() {
        // Regression pin for the fd-based metadata application: the
        // configured mode must land on the file the writer just opened,
        // and the path used to label warnings must not participate in
        // the actual mode change (it goes through fchmod on the open
        // fd).
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("modeful.log");

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o640),
        );
        out.write_payload(FilePayload {
            egress: Bytes::from("hi"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("first write must succeed");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o640,
            "fchmod must land the configured mode on the created file"
        );

        // Second write from an operator who then externally tightened
        // the mode: the fstat verify refuses, so the write does not
        // silently override the operator's manual chmod, nor does it
        // silently proceed under a mode that no longer matches config.
        // The operator's next step is either to align the file with
        // config (chmod back) or to update the config to match the
        // file — both are explicit, neither is silent.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = out
            .write_payload(FilePayload {
                egress: Bytes::from("again"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("mismatched mode must be refused");
        assert!(err.to_string().contains("existing file mode"));
        let mode2 = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode2, 0o600, "refusal must not chmod the file back");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn dropping_never_polled_write_payload_leaves_no_side_effects() {
        // Narrow regression pin: constructing `write_payload`'s future
        // and dropping it without ever polling must not touch the
        // filesystem. This is not the same shape as "future was polled,
        // reached apply_file_metadata_to_fd, then got cancelled" — Rust
        // async futures are lazy, so a never-polled future runs no
        // code. Verifying the mid-await cancellation shape would need
        // a test-only barrier inside `write_payload` to hold the future
        // at a known suspension point; the fd-lifetime argument for the
        // cancel-safe path lives in the module-level comment and in
        // this commit's message. What this test *does* pin is that
        // wiring the payload alone has no observable side effect, so a
        // caller that speculatively builds a write and then abandons it
        // doesn't leak file/state.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("never_polled.log");

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
        );
        let fut = out.write_payload(FilePayload {
            egress: Bytes::from("never"),
            path: path.display().to_string(),
            is_dynamic: false,
        });
        drop(fut);

        // Nothing was polled, so no `open` happened. The file must not
        // exist and no other state must have changed.
        assert!(
            !path.exists(),
            "never-polled write_payload future must not create the target file"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn failed_first_write_does_not_skip_mode_on_the_retry() {
        // Regression pin: the `first_create` bookkeeping used to
        // record the path *before* `open`/`write_all`. That meant a
        // failed first attempt — e.g. O_NOFOLLOW hitting a
        // pre-planted symlink — still marked the path as "created",
        // so the operator's fix (remove the symlink) succeeded but
        // the subsequent write treated the file as not-first-create
        // and silently skipped mode/owner application. Pin that the
        // configured mode lands on the file that eventually gets
        // created.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("guarded.log");
        // Point a symlink at a target somewhere outside the tempdir
        // so `open(O_NOFOLLOW)` fails with ELOOP on the first try.
        let symlink_target = dir.path().join("_would_be_hijacked.log");
        std::os::unix::fs::symlink(&symlink_target, &path).unwrap();

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o640),
        );

        // First write must fail with the symlink refusal.
        let err = out
            .write_payload(FilePayload {
                egress: Bytes::from("first"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("symlink at output path must be refused on first write");
        assert!(err.to_string().contains("refusing to follow symlink"));

        // Operator removes the symlink and retries.
        std::fs::remove_file(&path).unwrap();
        out.write_payload(FilePayload {
            egress: Bytes::from("second"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("second write must succeed after removing the symlink");

        // The retry must be treated as first-create so the configured
        // mode lands on the newly opened file. Pre-fix this returned
        // 0644 (umask default) because `created_paths` already
        // contained the path from the failed attempt.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o640,
            "post-fix: metadata contract must apply on the first successful write, not on the first attempt"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn existing_file_with_matching_mode_accepts_subsequent_writes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("continuing.log");

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
        );
        for payload in ["first", "second", "third"] {
            out.write_payload(FilePayload {
                egress: Bytes::from(payload),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .unwrap_or_else(|e| panic!("{payload} write must succeed: {e:#}"));
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"first\nsecond\nthird\n");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode set on create must persist across writes");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn existing_file_with_mismatched_mode_is_refused_loudly() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mismatched.log");

        std::fs::write(&path, b"existing\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
        );

        let err = out
            .write_payload(FilePayload {
                egress: Bytes::from("payload"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("mismatched-mode preexisting file must be refused");
        let msg = err.to_string();
        assert!(msg.contains("existing file mode"), "{msg}");
        assert!(msg.contains("0o644"), "{msg}");
        assert!(msg.contains("0o600"), "{msg}");

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"existing\n",
            "payload must not have been appended when the mode check refused"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn existing_file_with_extra_special_mode_bits_is_refused() {
        // The permission mode comparison must cover the full 0o7777
        // mask (setuid / setgid / sticky + rwx), not just 0o777. If
        // an existing file has an unexpected setuid bit on top of
        // matching rwx bits, the write must still refuse — a
        // trust-relevant surface for log files that are supposed to
        // be plain regular files.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("suid.log");

        std::fs::write(&path, b"existing\n").unwrap();
        // Configured mode will be 0o640; on disk we set 0o4640 (adds
        // setuid). The low 9 bits match, but the full 12-bit mode
        // does not — so the write must refuse.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o4640)).unwrap();

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o640),
        );

        let err = out
            .write_payload(FilePayload {
                egress: Bytes::from("payload"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("preexisting file with an unexpected setuid bit must be refused");
        let msg = err.to_string();
        assert!(msg.contains("existing file mode"), "{msg}");
        assert!(
            msg.contains("0o4640"),
            "diagnostic must show the actual full mode: {msg}"
        );
        assert!(
            msg.contains("0o640"),
            "diagnostic must show the configured mode: {msg}"
        );

        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"existing\n",
            "payload must not have been appended"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn logrotate_style_inode_swap_reapplies_configured_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("rotated.log");

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
        );

        out.write_payload(FilePayload {
            egress: Bytes::from("pre-rotate"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("first write");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::remove_file(&path).unwrap();
        out.write_payload(FilePayload {
            egress: Bytes::from("post-rotate"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("write on freshly-rotated path");

        // The freshly-created file must contain only the post-rotate
        // payload — proving `create_new` was taken (not append-onto-
        // the-original), which is the branch that runs the apply.
        // Inode numbers are intentionally NOT compared: some
        // filesystems reuse inode numbers immediately after unlink,
        // and doing so is not a violation of what this test cares
        // about.
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"post-rotate\n",
            "rotation must have re-created the file (not appended to a leftover)"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600,
            "post-rotation inode must inherit the configured mode, not umask default"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn empty_file_left_by_failed_apply_blocks_next_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("crashed.log");

        let attempt1 = make_output_with_metadata(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
            Some("bad\0owner".into()),
            None,
        );
        let err1 = attempt1
            .write_payload(FilePayload {
                egress: Bytes::from("first"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("apply failure must refuse the first write");
        assert!(err1.to_string().contains("failed to resolve owner"));
        assert!(path.exists());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        // Force the leftover file into a mode that will not match the
        // configured value on the next attempt. On production Linux
        // (umask 0022) create_new produces 0o644 for free; setting it
        // explicitly makes the test invariant hold regardless of the
        // host's umask (macOS test runners can land at 0o600 by
        // chance, which would otherwise pass verify).
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        drop(attempt1);

        // Simulate daemon restart with a fixed config.
        let attempt2 = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
        );
        let err2 = attempt2
            .write_payload(FilePayload {
                egress: Bytes::from("second"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("post-restart write must refuse if leftover file mode does not match");
        assert!(err2.to_string().contains("existing file mode"));

        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

        // Operator remediation: remove the crash-leftover file.
        std::fs::remove_file(&path).unwrap();
        attempt2
            .write_payload(FilePayload {
                egress: Bytes::from("second"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect("write must succeed once the leftover file is cleared");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn metadata_apply_failure_leaves_empty_file_and_no_bytes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("apply_fail.log");

        let out = make_output_with_metadata(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
            Some("bad\0owner".into()),
            None,
        );

        let err = out
            .write_payload(FilePayload {
                egress: Bytes::from("payload"),
                path: path.display().to_string(),
                is_dynamic: false,
            })
            .await
            .expect_err("metadata contract must refuse the write when apply cannot land");
        assert!(err.to_string().contains("failed to resolve owner"));
        assert!(path.exists(), "create_new produced the file");
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "payload must not have been written when metadata apply failed"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn output_with_no_metadata_contract_accepts_existing_file_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("no_contract.log");
        std::fs::write(&path, b"pre-existing\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let out = make_output_with(ek(ExprKind::StringLit(path.display().to_string())), None);
        assert!(!out.requires_metadata());

        out.write_payload(FilePayload {
            egress: Bytes::from("appended"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("write must succeed with no metadata contract");

        assert_eq!(std::fs::read(&path).unwrap(), b"pre-existing\nappended\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644,
            "no metadata contract means the existing file's mode is untouched"
        );
    }

    fn mp(props: &[crate::dsl::ast::Property]) -> crate::dsl::module_props::ModuleProperties {
        crate::dsl::module_props::ModuleProperties::from_parts("file", props.to_vec())
    }

    fn prop_str(key: &str, val: &str) -> crate::dsl::ast::Property {
        crate::dsl::ast::Property::KeyValue {
            key: key.to_string(),
            key_span: None,
            value: Expr::spanless(ExprKind::StringLit(val.to_string())),
            value_span: None,
        }
    }

    #[test]
    fn mode_parser_accepts_all_zero_forms() {
        // `0o0000` is a legitimate Unix mode (no permission bits set).
        // The parser used to `trim_start_matches('0')` first, which
        // turned `"0"` and `"0000"` into an empty string and then a
        // parse error. Pin both spellings as accepted so future
        // refactors don't reintroduce the strip.
        for raw in ["0", "0000"] {
            let props = mp(&[prop_str("path", "/tmp/nowhere.log"), prop_str("mode", raw)]);
            let out = FileOutput::from_properties(
                "t",
                &props,
                &crate::modules::BuildContext::for_testing(),
            )
            .unwrap_or_else(|e| panic!("mode {raw:?} must parse: {e:#}"));
            assert_eq!(out.mode, Some(0o0000), "mode {raw:?}");
        }
    }

    #[test]
    fn mode_parser_rejects_values_above_the_permission_mask() {
        // A value above 0o7777 could never be honoured by the fstat
        // verify (which masks to 0o7777). Reject at load time so the
        // mistake surfaces in the startup log, not as a permanent
        // "mode does not match" refusal on every subsequent write.
        for raw in ["10000", "17777"] {
            let props = mp(&[prop_str("path", "/tmp/nowhere.log"), prop_str("mode", raw)]);
            let res = FileOutput::from_properties(
                "t",
                &props,
                &crate::modules::BuildContext::for_testing(),
            );
            let err = match res {
                Ok(_) => panic!("mode {raw:?} exceeds 0o7777 and must be rejected"),
                Err(e) => e,
            };
            let msg = err.to_string();
            assert!(
                msg.contains("exceeds the 12-bit permission range"),
                "diagnostic must name the constraint: {msg}"
            );
        }
    }

    /// Regression pin for the unbatched-sink shutdown race. A steady-
    /// state `consume` that fails and enters its retry backoff must
    /// not sleep past the runtime's shutdown budget: if the runtime
    /// signals shutdown mid-sleep, the sink breaks out, routes the
    /// pending event to DLQ, resolves `Recovered`, and returns
    /// promptly so the queue consumer's select! can proceed with the
    /// drain. Without this the retry sleep (default 1+2+4+8 = 15 s)
    /// held the consumer past the 10 s runtime shutdown budget and
    /// the task was aborted mid-flight — the exact class of loss
    /// PR #84 closed for batched sinks.
    #[tokio::test]
    #[cfg(unix)]
    async fn consume_short_circuits_retry_backoff_on_shutdown() {
        use crate::queue::{AckDisposition, BackoffStrategy, QueueAckHandle};

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let dir = tempfile::TempDir::new().unwrap();
        // Point at a non-existent parent so every write_payload fails
        // with ENOENT — the retry loop has to actually visit the
        // backoff sleep for the test to observe the race.
        let path = dir.path().join("does_not_exist").join("file.log");

        let out = FileOutput {
            name: "test".into(),
            path: ek(ExprKind::StringLit(path.display().to_string())),
            mode: None,
            owner: None,
            group: None,
            funcs: funcs(),
            // Long backoff floor so an untouched sleep would obviously
            // hold the consume past the assertion window.
            retry: crate::queue::RetryConfig {
                max_attempts: 5,
                initial_wait: std::time::Duration::from_secs(5),
                max_wait: std::time::Duration::from_secs(5),
                backoff: BackoffStrategy::Fixed,
            },
            error_log: None,
            metrics: Arc::new(OutputMetrics::default()),
            shutdown_signal: shutdown_rx,
        };

        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let event = event_with_workspace();
        let out = Arc::new(out);
        let out_clone = Arc::clone(&out);
        let started = std::time::Instant::now();
        let consume = tokio::spawn(async move { out_clone.consume(&event, ack).await });

        // Let the first attempt fail and the retry loop reach the sleep.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        shutdown_tx.send(true).unwrap();

        let res = consume.await.unwrap();
        let elapsed = started.elapsed();
        res.expect("consume must return Ok after shutdown-driven exit");
        assert!(
            elapsed < std::time::Duration::from_millis(1500),
            "consume must short-circuit the retry sleep — took {elapsed:?} against a 5s floor"
        );

        // The handle must resolve as `Recovered`, not `Dropped` — the
        // event was routed to DLQ, not silently lost.
        let (_pos, disposition) = ack_rx
            .try_recv()
            .expect("ack channel must have carried the resolution");
        assert!(
            matches!(disposition, AckDisposition::Recovered),
            "expected Recovered, got {disposition:?}"
        );
    }
}
