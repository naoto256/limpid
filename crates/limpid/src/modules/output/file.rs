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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

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
    /// Paths this output has finished applying mode/owner/group to.
    /// Membership is only inserted *after* `apply_file_metadata_to_fd`
    /// returns, so it is authoritative: presence == metadata obligation
    /// satisfied.
    created_paths: Mutex<HashSet<PathBuf>>,
    /// Paths where this output has successfully opened the file at
    /// least once but has not yet promoted them to `created_paths`.
    /// Used to survive a write_all failure or a cancellation between
    /// write_all and apply — either case leaves an on-disk file this
    /// output owes metadata to. Presence here forces the next
    /// successful write to re-apply mode/owner even though
    /// `path.exists()` is now true.
    metadata_obligations: Mutex<HashSet<PathBuf>>,
    funcs: Arc<FunctionRegistry>,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    metrics: Arc<OutputMetrics>,
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
            name: name.to_string(),
            path,
            mode,
            owner,
            group,
            created_paths: Mutex::new(HashSet::new()),
            metadata_obligations: Mutex::new(HashSet::new()),
            funcs,
            retry,
            error_log,
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
        loop {
            // Clone the payload's path/dynamic flag for each attempt;
            // `egress` is a refcounted `Bytes` so the actual buffer
            // isn't duplicated.
            let attempt_payload = FilePayload {
                egress: payload.egress.clone(),
                path: payload.path.clone(),
                is_dynamic: payload.is_dynamic,
            };
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
                    tokio::time::sleep(wait).await;
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

        // Decide whether this write owes metadata to the target
        // path, sampled *before* the open. Two orthogonal signals:
        //
        //   * `already_applied` — a prior write for this path ran
        //     apply_file_metadata_to_fd to completion. Nothing more
        //     to do.
        //   * `has_prior_obligation` — a prior write got as far as a
        //     successful `open` for this path but did not finish the
        //     apply (write_all failed, or the apply await was
        //     cancelled). We still owe metadata; the on-disk file
        //     exists but the obligation is outstanding.
        //
        // For "path exists but neither flag is set" — a pre-existing
        // file some other producer created — we deliberately skip
        // metadata: this output has never touched it, so overriding
        // its mode/owner would surprise operators. The obligation
        // path only kicks in once we've opened the file at least once
        // ourselves.
        let (path_preexisted, has_prior_obligation, already_applied) = {
            let created = self.created_paths.lock().await;
            let obligations = self.metadata_obligations.lock().await;
            (
                path.exists(),
                obligations.contains(&path),
                created.contains(&path),
            )
        };

        // Fourth safety pass, complementing the three path-rendering
        // passes in `render_path_in` (interpolation sanitising, `..`
        // reject, trailing-slash reject): refuse to write through a
        // symlink at the final path component. The rendering passes
        // stop an event from *composing* an escaping path; O_NOFOLLOW
        // stops a pre-planted symlink at a legitimately-composed path
        // from redirecting the append to an arbitrary file (classic
        // local symlink attack on a writable log directory). Scope is
        // the final component only — that is the file this output
        // creates and owns; symlinked *parent directories* are a
        // deployment topology choice and stay under the operator's
        // control.
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(&path).await.map_err(|e| {
            // ELOOP is how open(2) reports O_NOFOLLOW hitting a
            // symlink; surface it as an explicit refusal so the DLQ
            // reason names the attack instead of a cryptic errno.
            #[cfg(unix)]
            if e.raw_os_error() == Some(libc::ELOOP) {
                return anyhow::anyhow!("refusing to follow symlink at output path: {}", resolved);
            }
            anyhow::Error::from(e)
        })?;

        // We now hold an fd for the target. If this write is going to
        // owe metadata, record the obligation *before* write_all — the
        // open may have just created a fresh file on disk, and if
        // write_all fails or is cancelled we must remember to finish
        // the apply on the next attempt (otherwise `path.exists()`
        // above would silently drop us into the "pre-existing" arm on
        // the next call).
        let should_apply = !already_applied && (has_prior_obligation || !path_preexisted);
        if should_apply && !has_prior_obligation {
            let mut obligations = self.metadata_obligations.lock().await;
            obligations.insert(path.clone());
        }

        let msg = String::from_utf8_lossy(&payload.egress);
        let mut buf = Vec::with_capacity(msg.len() + 1);
        buf.extend_from_slice(msg.as_bytes());
        buf.push(b'\n');
        file.write_all(&buf).await?;
        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);

        if should_apply {
            // Apply mode/owner through the open fd rather than the path.
            // The path is already guarded by O_NOFOLLOW at open time, but
            // set_permissions()/chown() would re-resolve the path and
            // could follow a symlink pre-planted between open and the
            // metadata call — closing that TOCTOU window means the mode
            // and ownership always land on the same inode we just wrote
            // to, never a different one.
            //
            // apply_file_metadata_to_fd is cancel-safe (dup fd +
            // spawn_blocking): if this await returns at all, the
            // fchmod/fchown have run. So once we get past it we can
            // promote the path from "obligation outstanding" to
            // "obligation satisfied". A concurrent second writer may
            // have observed should_apply=true and re-applied in
            // parallel — fchmod/fchown are idempotent so the extra
            // call is harmless, and the insert below de-duplicates
            // future callers.
            self.apply_file_metadata_to_fd(&file, &path).await;
            let mut created = self.created_paths.lock().await;
            let mut obligations = self.metadata_obligations.lock().await;
            created.insert(path.clone());
            obligations.remove(&path);
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
    async fn apply_file_metadata_to_fd(&self, file: &tokio::fs::File, path: &Path) {
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::io::AsRawFd;

        let mode = self.mode;
        let owner = self.owner.clone();
        let group = self.group.clone();
        if mode.is_none() && owner.is_none() && group.is_none() {
            return;
        }
        let path = path.to_path_buf();

        // `dup(2)` returns a fresh fd referring to the same open file
        // description. The `OwnedFd` moves into the closure so it stays
        // live for the duration of the blocking task even if the outer
        // future — and therefore the source `File` — is dropped.
        let duped: i32 = unsafe { libc::dup(file.as_raw_fd()) };
        if duped < 0 {
            tracing::warn!(
                "output file '{}': dup failed, skipping metadata apply: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
            return;
        }
        // SAFETY: `duped >= 0` was just returned by `libc::dup` and no
        // other Rust type owns it yet, so this transfers exclusive
        // ownership to the `OwnedFd`.
        let owned_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(duped) };

        tokio::task::spawn_blocking(move || {
            let fd = owned_fd.as_raw_fd();
            if let Some(mode) = mode {
                let rc = unsafe { libc::fchmod(fd, mode as libc::mode_t) };
                if rc != 0 {
                    tracing::warn!(
                        "output file '{}': fchmod failed: {}",
                        path.display(),
                        std::io::Error::last_os_error()
                    );
                }
            }

            if owner.is_some() || group.is_some() {
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
                if uid.is_some() || gid.is_some() {
                    // `fchown(-1, -1)` is a no-op, so map "not
                    // configured" or "lookup failed" to -1 and let
                    // libc decide whether either coordinate needs
                    // changing.
                    let uid_arg = uid.unwrap_or(u32::MAX);
                    let gid_arg = gid.unwrap_or(u32::MAX);
                    let rc = unsafe { libc::fchown(fd, uid_arg, gid_arg) };
                    if rc != 0 {
                        tracing::warn!(
                            "output file '{}': fchown failed: {}",
                            path.display(),
                            std::io::Error::last_os_error()
                        );
                    }
                }
            }
            // `owned_fd` drops here at end-of-closure, so `close(2)`
            // runs deterministically whether the closure returned
            // normally or panicked — cancellation of the outer future
            // doesn't reach in here.
        })
        .await
        .ok();
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
/// threads). Called only at module construction time today, but the
/// hard guarantee removes a hazard for any future caller that wires
/// this onto a hot path.
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
        FileOutput {
            name: "test".into(),
            path,
            mode,
            owner: None,
            group: None,
            created_paths: Mutex::new(HashSet::new()),
            metadata_obligations: Mutex::new(HashSet::new()),
            funcs: funcs(),
            retry: RetryConfig::default(),
            error_log: None,
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
        // smuggle in a path-component break on Linux too. Pre-fix
        // audits flagged that the existing
        // render_template_sanitises_every_interpolation case used a
        // value with neither `/` nor `\` and so didn't actually pin
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

        // A second write must not re-apply the mode — apply_file_metadata
        // is gated on first_create. Externally tightening the mode
        // between writes stays sticky, which is the operator-friendly
        // behaviour.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        out.write_payload(FilePayload {
            egress: Bytes::from("again"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("second write must succeed");
        let mode2 = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode2, 0o600,
            "second write must not re-apply mode; operator tightening stays sticky"
        );
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
    async fn outstanding_metadata_obligation_reapplies_on_next_write() {
        // Regression pin for the "write_all failed or the apply await
        // was cancelled between open and apply_file_metadata_to_fd"
        // shape (audit round-2, boundary-contract / security). Before
        // the fix, the metadata obligation was tracked only through
        // `path.exists()` — once the file existed on disk (which
        // `open(create=true)` guarantees the moment it returns), any
        // subsequent call landed in the "not first create" arm and
        // silently skipped mode/owner. We simulate that shape by
        // seeding `metadata_obligations` directly: it is what a prior
        // aborted attempt would have left behind.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("obligation.log");

        // Pretend a previous attempt got as far as `open` (creating
        // the file) but was cancelled before apply. On disk that
        // looks like: file exists with umask-default mode; obligation
        // still registered.
        std::fs::write(&path, b"partial\n").unwrap();
        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
        );
        out.metadata_obligations.lock().await.insert(path.clone());

        // Next successful write must honour the obligation.
        out.write_payload(FilePayload {
            egress: Bytes::from("retry"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("retry write must succeed");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "outstanding metadata obligation must trigger apply on the next successful write"
        );
        assert!(
            out.metadata_obligations.lock().await.is_empty(),
            "obligation must be cleared once apply has landed"
        );
        assert!(
            out.created_paths.lock().await.contains(&path),
            "successful apply must promote the path to created_paths"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn preexisting_file_from_another_producer_keeps_its_mode() {
        // Semantic pin, complementing the obligation test above: a
        // file that existed *before* this output ever touched it, and
        // therefore carries no obligation, must not have its mode
        // overwritten on our first append. The mode contract is
        // scoped to files this output creates (or has an outstanding
        // obligation on); rewriting an unrelated producer's file
        // would surprise operators.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("foreign.log");

        std::fs::write(&path, b"not ours\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let out = make_output_with(
            ek(ExprKind::StringLit(path.display().to_string())),
            Some(0o600),
        );

        out.write_payload(FilePayload {
            egress: Bytes::from("append"),
            path: path.display().to_string(),
            is_dynamic: false,
        })
        .await
        .expect("append to a preexisting file must succeed");

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "pre-existing file with no obligation must keep its mode"
        );
        assert!(
            out.metadata_obligations.lock().await.is_empty(),
            "no obligation should have been recorded for a preexisting file"
        );
        assert!(
            !out.created_paths.lock().await.contains(&path),
            "no metadata was applied, so nothing should have promoted to created_paths"
        );
    }
}
