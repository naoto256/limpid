//! Control socket: Unix domain socket server for limpidctl and
//! other management tools.
//!
//! Protocol: line-based over Unix stream socket.
//! Command-level responses are JSON, except `tap` (event-stream text)
//! and a small set of protocol-error responses (server-too-busy,
//! command-too-long) that fall back to a plain-text line.
//!
//! Commands:
//!   health                      — {"status":"ok","uptime_seconds":N}
//!   stats                       — pipeline/input/output metrics (JSON)
//!   list                        — pipeline structure with tap points (JSON)
//!   tap <kind> <name>           — stream event messages (LF-delimited text)
//!   tap <kind> <name> json      — stream full Event JSON (one per line)
//!   inject <kind> <name>        — push raw lines (read to EOF, reply {"injected":N})
//!   inject <kind> <name> json   — push full Event JSON lines (skip invalid lines)

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, error, info, warn};

use crate::dsl::ast::*;
use crate::event::Event;
use crate::metrics::MetricsRegistry;
use crate::pipeline::CompiledConfig;
use crate::queue::QueueSender;
use crate::tap::TapRegistry;

const DEFAULT_SOCKET_PATH: &str = "/var/run/limpid/control.sock";

/// Maximum command line length (bytes). Prevents OOM from malicious clients.
const MAX_COMMAND_LEN: usize = 4096;

/// Maximum concurrent control-socket connections.
///
/// The control socket is a local, root-equivalent trust boundary (mode 0o660
/// in a root-owned directory), but we still cap concurrent connections so
/// that a misbehaving or compromised peer in the limpid group cannot starve
/// the accept loop. 8 is ample for normal ops (`limpidctl` + a few taps).
const MAX_CONTROL_CONNECTIONS: usize = 8;

/// Maximum total bytes a single `inject` stream may consume before the
/// connection is dropped. Prevents a trusted-but-buggy client from growing
/// the downstream disk queue or memory channel without bound.
///
/// 16 MiB is large enough for reasonable replay batches (tens of thousands
/// of syslog lines) while bounding worst-case per-connection memory/disk
/// pressure.
const MAX_INJECT_BYTES: u64 = 16 * 1024 * 1024;

/// Per-input inject target: event channel + metrics handle (for events_injected).
pub type InputInjectTarget = (mpsc::Sender<Event>, Arc<crate::metrics::InputMetrics>);

/// Create the control socket's parent directory if absent. When *this
/// call* created it (bare / non-systemd runs), tighten it to 0o750 so
/// only the daemon user and its group can reach the socket inode at
/// all — this directory mode is the layer that covers the bind→chmod
/// window in [`ControlServer::run`]. Under systemd the directory comes
/// from `RuntimeDirectory=limpid` (mode pinned via
/// `RuntimeDirectoryMode` in the unit file) and already exists here; a
/// pre-existing directory's permissions are deliberately left alone
/// because they may be operator-managed.
///
/// Startup-time validation and (when absent) creation of the control
/// socket's parent directory. Called from `Runtime::start` before the
/// control task is spawned, so an unsafe pre-existing parent — or a
/// failure to safely create an absent parent — aborts the whole daemon
/// startup instead of letting a background task die silently with the
/// daemon still running under a broken trust boundary.
///
/// The trust boundary is enforced on the FINAL parent component:
///
/// - `symlink_metadata(parent)` inspects the final component itself
///   without following it. A symlink parent lets an attacker redirect
///   the bind target between validation and the daemon's actual `bind`
///   — the classic parent-swap TOCTOU — so a symlink final component
///   is rejected up front. Ancestor path components may be symlinks
///   (modern Linux ships `/var/run` → `/run` as a compatibility
///   symlink); ancestor path identity is a deployment contract.
/// - Parent exists but is not a directory → bail (config typo).
/// - Parent exists as a directory owned by an untrusted uid → bail.
///   Directory mode alone does not close the boundary: the owner
///   retains rename/unlink rights regardless of mode. A root-owned
///   parent is trusted only when the daemon runs as root; a non-root
///   daemon requires a daemon-owned parent.
/// - Parent exists as a directory with an unsafe mode → bail with the
///   observed mode and a remediation hint.
/// - Parent absent → verify the nearest existing ancestor is
///   trusted: owner must be the daemon's own euid or root, AND the
///   mode must not be group- or world-writable (`mode & 0o022 == 0`).
///   Ownership alone is not enough; a daemon-owned `0o777` ancestor
///   still lets any writer plant a node under the target name and
///   race the create+chmod, and root-owned `/tmp` at `0o1777` has
///   the same shape. Then create the parent under our control at
///   0o750, then `symlink_metadata` the created path to confirm it
///   is a real directory owned by us at the requested mode. Any
///   mismatch bails startup.
///
/// The custom-deploy contract this closes: an operator whose
/// `control { socket "..." }` points into a directory they have not
/// tightened (or that is owned by someone else, or that is a symlink,
/// or that lives under an attacker-writable ancestor) would previously
/// have seen only a `warn!` line or a silent bind failure. That
/// warn-only shape is now a fatal startup error, matching the DLQ
/// preflight in `error_log::validate_at_startup`.
///
/// Under packaged systemd units (`RuntimeDirectory=limpid` with
/// `RuntimeDirectoryMode=0750`) the parent is already safe and this
/// validation is a no-op except for the symlink check.
pub fn validate_control_socket_parent(socket_path_config: Option<&str>) -> anyhow::Result<()> {
    let socket_path = PathBuf::from(
        socket_path_config
            .map(str::to_string)
            .unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_string()),
    );
    let Some(parent) = socket_path.parent() else {
        return Ok(());
    };
    let parent = if parent.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        parent
    };
    #[cfg(unix)]
    {
        let self_euid = unsafe { libc::geteuid() };
        match std::fs::symlink_metadata(parent) {
            Ok(link_meta) => validate_existing_parent(parent, &link_meta, self_euid)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                create_parent_under_trusted_ancestor(parent, self_euid)?;
            }
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!(
                    "control socket: failed to stat parent {:?}",
                    parent
                )));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn validate_existing_parent(
    parent: &std::path::Path,
    link_meta: &std::fs::Metadata,
    self_euid: u32,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if link_meta.file_type().is_symlink() {
        anyhow::bail!(
            "control socket: parent {:?} is a symlink — refusing to bind. The final parent \
             component must be a real directory: a symlink lets an attacker redirect the bind \
             target between this preflight and the daemon's actual `bind`. Modern Linux ships \
             `/var/run` as a symlink to `/run`; point `control {{ socket \"...\" }}` at a \
             `/run/limpid/...` path, or leave the packaged default alone.",
            parent
        );
    }
    if !link_meta.is_dir() {
        anyhow::bail!(
            "control socket: parent {:?} exists but is not a directory; check the \
             `control {{ socket \"...\" }}` value",
            parent
        );
    }
    let uid = link_meta.uid();
    if parent_dir_owner_is_untrusted(uid, self_euid) {
        anyhow::bail!(
            "control socket: parent dir {:?} is owned by uid {}, but the daemon's effective \
             uid is {} — refusing to bind. Directory mode alone does not close the trust \
             boundary (an untrusted owner can rename or replace the socket inode inside the \
             parent), and a root-owned parent at the packaged mode (`0o750`) is not writable \
             by a non-root daemon so bind would fail post-validation anyway. Under systemd, \
             `RuntimeDirectory=limpid` combined with `User=limpid` creates a daemon-owned \
             parent at the requested mode — that is the intended shape. For custom deploys, \
             `chown <daemon-user>:<daemon-group> {:?}` and re-run.",
            parent,
            uid,
            self_euid,
            parent,
        );
    }
    let mode = link_meta.permissions().mode() & 0o777;
    if parent_dir_mode_is_unsafe(mode) {
        anyhow::bail!(
            "control socket: parent dir {:?} has mode 0o{:o} — refusing to bind. The control \
             socket is a root-equivalent trust boundary and the bind→chmod 0o660 window \
             assumes only the daemon's group can traverse to the socket inode. Tighten the \
             parent to 0o750 or stricter (`chmod 0750 {:?}`), or point \
             `control {{ socket \"...\" }}` at a packaged path (`/var/run/limpid/`, with \
             `RuntimeDirectoryMode=0750` under systemd).",
            parent,
            mode,
            parent,
        );
    }
    Ok(())
}

#[cfg(unix)]
fn create_parent_under_trusted_ancestor(
    parent: &std::path::Path,
    self_euid: u32,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    // Verify the nearest existing ancestor is a trusted owner so we
    // are not creating a fresh directory (and immediately chmod-ing
    // it to 0o750) inside an attacker-writable ancestor. Canonicalize
    // the ancestor so `/var/run` (symlink) → `/run` is checked at its
    // real identity; the check itself follows the canonical path.
    let ancestor = nearest_existing_ancestor(parent).with_context(|| {
        format!(
            "control socket: no existing ancestor for absent parent {:?}",
            parent
        )
    })?;
    let canonical = std::fs::canonicalize(&ancestor).with_context(|| {
        format!(
            "control socket: cannot canonicalize ancestor {:?} of parent {:?}",
            ancestor, parent
        )
    })?;
    let ancestor_meta = std::fs::metadata(&canonical).with_context(|| {
        format!(
            "control socket: cannot stat ancestor {:?} (canonical {:?})",
            ancestor, canonical
        )
    })?;
    let ancestor_uid = ancestor_meta.uid();
    let ancestor_mode = ancestor_meta.permissions().mode() & 0o777;
    if !ancestor_is_trusted_for_create(ancestor_uid, ancestor_mode, self_euid) {
        anyhow::bail!(
            "control socket: refusing to create parent {:?} — nearest existing ancestor \
             {:?} (canonical {:?}) is owned by uid {} at mode 0o{:o}, which does not satisfy \
             the create-ancestor trust contract (owner must be the daemon's euid or root, \
             AND the mode must have `mode & 0o022 == 0`). Owner alone is not enough: any \
             process with write permission on the ancestor could plant a node under the \
             target name or race the `create_dir_all` + `chmod` window. Move the socket \
             into a daemon-owned or root-owned parent that is not group- or world-writable \
             (e.g. `/run/limpid/`).",
            parent,
            ancestor,
            canonical,
            ancestor_uid,
            ancestor_mode,
        );
    }
    std::fs::create_dir_all(parent).with_context(|| {
        format!(
            "control socket: failed to create parent directory {:?}",
            parent
        )
    })?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750)).with_context(
        || {
            format!(
                "control socket: failed to chmod 0o750 on created parent {:?}",
                parent
            )
        },
    )?;
    // Post-create verify: symlink_metadata so we notice if a
    // concurrent attacker swapped the created path with a symlink
    // between our create and the check.
    let created = std::fs::symlink_metadata(parent).with_context(|| {
        format!(
            "control socket: failed to stat freshly-created parent {:?}",
            parent
        )
    })?;
    if created.file_type().is_symlink() {
        anyhow::bail!(
            "control socket: parent {:?} is a symlink after create — refusing to bind. A \
             concurrent process replaced the directory we just created.",
            parent
        );
    }
    if !created.is_dir() {
        anyhow::bail!(
            "control socket: parent {:?} is not a directory after create — refusing to bind",
            parent
        );
    }
    let created_uid = created.uid();
    if created_uid != self_euid {
        anyhow::bail!(
            "control socket: freshly-created parent {:?} is owned by uid {}, not the daemon's \
             effective uid {} — refusing to bind",
            parent,
            created_uid,
            self_euid,
        );
    }
    let created_mode = created.permissions().mode() & 0o777;
    if created_mode != 0o750 {
        anyhow::bail!(
            "control socket: freshly-created parent {:?} has mode 0o{:o}, not the requested \
             0o750 — refusing to bind",
            parent,
            created_mode,
        );
    }
    Ok(())
}

/// True when the deepest existing ancestor of an absent parent is
/// safe to `create_dir_all` into.
///
/// Two properties must both hold:
///
/// - Trusted owner: either the daemon's own effective uid or root.
///   Anyone else can `rename` or `unlink` inside a directory they
///   own regardless of its mode bits, so a non-daemon non-root owner
///   is out of scope for auto-create.
/// - Not group- or world-writable (`mode & 0o022 == 0`). Ownership
///   trusts the *owner*, but `create_dir_all` runs on the ancestor
///   from *any* process with write permission on that ancestor. A
///   daemon-owned ancestor at `0o777` still lets an outside user
///   `mkdir` the target name (or plant a node under it) before we
///   chmod, so the ownership check is not by itself enough. The
///   most common concrete example is sticky-bit `/tmp` at `0o1777`:
///   root owns it, but the sticky bit only stops `unlink` of files
///   the attacker doesn't own — an attacker can still create nodes,
///   and the daemon must not race with them.
///
/// Pure function so it can be exercised directly by unit tests
/// without requiring privilege escalation to construct an ancestor
/// owned by a different uid.
#[cfg(unix)]
fn ancestor_is_trusted_for_create(uid: u32, mode: u32, self_euid: u32) -> bool {
    (uid == self_euid || uid == 0) && mode & 0o022 == 0
}

/// Walk up `path` until a component exists on the filesystem (checked
/// with `symlink_metadata` so a broken symlink counts as "does not
/// exist" for the purpose of finding an ancestor we can trust). Used
/// by [`create_parent_under_trusted_ancestor`] to find the deepest
/// existing ancestor whose identity we can verify before creating a
/// new directory below it.
#[cfg(unix)]
fn nearest_existing_ancestor(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let mut current = path;
    loop {
        let Some(parent) = current.parent() else {
            anyhow::bail!("no existing ancestor for {:?}", path);
        };
        if std::fs::symlink_metadata(parent).is_ok() {
            return Ok(parent.to_path_buf());
        }
        current = parent;
    }
}

/// True when the parent dir's mode breaks either TOCTOU-mitigating
/// property the `ControlServer::run` bind->chmod comment relies on:
/// group/other write (symlink/file race before `bind`) or other
/// execute (an outside-the-group user can traverse to and connect the
/// socket inode during the bind->chmod window). Group execute is not
/// flagged — group is the socket's own trusted access group.
///
/// **Not shared with the unix_socket input predicate**: that sink
/// binds a world-writable (`0o666`) datagram socket at
/// `/dev/log`-style paths, where other-execute on the parent is a
/// requirement rather than a threat. Its own predicate lives in
/// `crates/limpid/src/modules/input/unix_socket.rs` and flags
/// group/other write only.
#[cfg(unix)]
fn parent_dir_mode_is_unsafe(mode: u32) -> bool {
    mode & 0o023 != 0
}

/// True when the parent dir's owner is not the daemon's own effective
/// uid. Directory mode alone does not close the trust boundary: an
/// untrusted owner retains rename/unlink rights inside the directory
/// regardless of mode bits, and can therefore replace the socket inode
/// between the daemon's `bind` and its follow-up `chmod`.
///
/// A root-owned parent is trusted **only when the daemon itself runs
/// as root** (`self_euid == 0`). A non-root daemon binding into a
/// root-owned parent at the packaged mode (`0o750`) has no write
/// permission on the parent, so `bind` would fail post-validation and
/// the fire-and-forget control task would die silently — same failure
/// shape the startup validation was introduced to prevent. Requiring
/// the parent owner to match the daemon's own euid covers both cases:
/// a root daemon runs against a root-owned parent, and a non-root
/// daemon runs against a daemon-owned parent (systemd's
/// `RuntimeDirectory=limpid` with `User=limpid` produces exactly this
/// shape).
///
/// Kept separate from the mode predicate so error diagnostics can name
/// the failing property (owner vs mode).
#[cfg(unix)]
fn parent_dir_owner_is_untrusted(uid: u32, self_euid: u32) -> bool {
    uid != self_euid
}

pub struct ControlServer {
    socket_path: PathBuf,
    tap: TapRegistry,
    metrics: Arc<MetricsRegistry>,
    config: Arc<CompiledConfig>,
    input_senders: Arc<HashMap<String, InputInjectTarget>>,
    output_senders: Arc<HashMap<String, QueueSender>>,
    started_at: Instant,
}

impl ControlServer {
    pub fn new(
        socket_path: Option<String>,
        tap: TapRegistry,
        metrics: Arc<MetricsRegistry>,
        config: Arc<CompiledConfig>,
        input_senders: HashMap<String, InputInjectTarget>,
        output_senders: Arc<HashMap<String, QueueSender>>,
        started_at: Instant,
    ) -> Self {
        Self {
            socket_path: PathBuf::from(
                socket_path.unwrap_or_else(|| DEFAULT_SOCKET_PATH.to_string()),
            ),
            tap,
            metrics,
            config,
            input_senders: Arc::new(input_senders),
            output_senders,
            started_at,
        }
    }

    pub async fn run(self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        // Parent directory has already been validated and (when absent)
        // created at 0o750 under a trusted ancestor by
        // `validate_control_socket_parent`, called from `Runtime::start`
        // before this task was spawned. No parent-preparation happens
        // here — moving it out was deliberate: a bail inside this
        // fire-and-forget task would kill the control server silently
        // with the daemon still running, so any trust-boundary decision
        // must be made at startup where its errors abort the daemon.

        // Remove stale socket — only if it's actually a socket. Any
        // other node type (regular file, directory, FIFO, device
        // node) is refused loudly. The previous shape ("non-symlink
        // → remove") was destructive by design: an operator typo that
        // set `socket` to a real file path would silently unlink the
        // target on daemon startup. The comment ("only if it's
        // actually a socket") is now the actual contract.
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            match std::fs::symlink_metadata(&self.socket_path) {
                Ok(meta) => {
                    let ft = meta.file_type();
                    if ft.is_symlink() {
                        error!(
                            "control socket: {:?} is a symlink — refusing to remove",
                            self.socket_path
                        );
                        return;
                    }
                    if !ft.is_socket() {
                        let shape = if ft.is_dir() {
                            "directory"
                        } else if ft.is_file() {
                            "regular file"
                        } else if ft.is_fifo() {
                            "FIFO"
                        } else if ft.is_block_device() {
                            "block device"
                        } else if ft.is_char_device() {
                            "character device"
                        } else {
                            "non-socket node"
                        };
                        error!(
                            "control socket: {:?} is a {} — refusing to remove. \
                             The stale-cleanup path only unlinks actual socket \
                             nodes; remove or move the node manually if this path \
                             is correct.",
                            self.socket_path, shape
                        );
                        return;
                    }
                    // Actual stale socket — safe to unlink.
                    let _ = std::fs::remove_file(&self.socket_path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // No node at the path — `bind(2)` will create it
                    // fresh.
                }
                Err(e) => {
                    warn!("control socket: cannot stat {:?}: {}", self.socket_path, e);
                }
            }
        }

        let listener = match UnixListener::bind(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                error!(
                    "control socket: failed to bind {:?}: {}",
                    self.socket_path, e
                );
                return;
            }
        };

        // Record (dev, ino) of the socket we just bound. Used at
        // shutdown as a defense-in-depth check: refuse to unlink
        // a node that has been swapped out from under us since
        // bind. Primary trust boundary is
        // `validate_control_socket_parent`'s fail-closed on
        // group/other-writable parents — on a safe parent no
        // outside-the-group writer can perform the swap. This
        // check is the extra ring of safety, not the load-bearing
        // guard.
        #[cfg(unix)]
        let bound_inode = {
            use std::os::unix::fs::MetadataExt;
            match std::fs::symlink_metadata(&self.socket_path) {
                Ok(meta) => Some((meta.dev(), meta.ino())),
                Err(e) => {
                    warn!(
                        "control socket: failed to stat bound socket for shutdown \
                         defense-in-depth: {}",
                        e
                    );
                    None
                }
            }
        };

        // Restrict socket permissions to owner + group (0o660).
        //
        // TOCTOU note (security audit Low 3-3): between `bind` above
        // and this chmod, the socket briefly carries umask-derived
        // permissions (typically 0o755 under umask 022) — a local
        // attacker who can reach the inode could connect in that
        // window. A process-wide `libc::umask(0o117)` around the bind
        // would close it, but was rejected: this function runs as a
        // spawned tokio task *after* every input / output / pipeline
        // task is already live (see `Runtime::start` — "start control
        // socket after all metrics are registered"), so a temporary
        // global umask races concurrent file creation in file outputs,
        // the disk queue, and the error_log, silently flipping *their*
        // modes instead. The window is instead covered structurally:
        // no await point separates bind from chmod (the gap is
        // microseconds of same-thread work), and the parent directory
        // gates who can reach the socket inode — daemon-created dirs are
        // 0o750 and packaged units pin `RuntimeDirectoryMode=0750`, so
        // an outside-the-group attacker cannot reach it during the
        // window. `validate_control_socket_parent` fails startup
        // when an inherited parent is too loose to hold that
        // line, so by the time we reach this point the parent
        // mode has been vetted.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o660);
            if let Err(e) = std::fs::set_permissions(&self.socket_path, perms) {
                warn!("control socket: failed to set permissions: {}", e);
            }
        }

        info!("control socket listening on {:?}", self.socket_path);

        let tap = Arc::new(self.tap);
        let config = self.config;
        let started_at = self.started_at;
        let metrics = self.metrics;
        let input_senders = self.input_senders;
        let output_senders = self.output_senders;

        let mut conn_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        let conn_sem = Arc::new(Semaphore::new(MAX_CONTROL_CONNECTIONS));

        loop {
            conn_handles.retain(|h| !h.is_finished());

            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("control socket: shutting down");
                        for h in &conn_handles {
                            h.abort();
                        }
                        break;
                    }
                }

                result = listener.accept() => {
                    match result {
                        Ok((mut stream, _addr)) => {
                            // Cap concurrent connections. We try_acquire so the
                            // accept loop never blocks; peers beyond the cap get
                            // a short error line and are dropped immediately.
                            let permit = match Arc::clone(&conn_sem).try_acquire_owned() {
                                Ok(p) => p,
                                Err(_) => {
                                    warn!(
                                        "control socket: rejecting connection — \
                                         {} concurrent connections already in flight",
                                        MAX_CONTROL_CONNECTIONS
                                    );
                                    let _ = stream
                                        .write_all(b"error: control socket busy (too many concurrent connections)\n")
                                        .await;
                                    continue;
                                }
                            };
                            let tap = Arc::clone(&tap);
                            let metrics_reg = Arc::clone(&metrics);
                            let config = Arc::clone(&config);
                            let input_senders = Arc::clone(&input_senders);
                            let output_senders = Arc::clone(&output_senders);
                            conn_handles.push(tokio::spawn(async move {
                                handle_connection(stream, tap, metrics_reg, config, input_senders, output_senders, started_at).await;
                                drop(permit);
                            }));
                        }
                        Err(e) => {
                            error!("control socket: accept error: {}", e);
                        }
                    }
                }
            }
        }

        // Clean up socket file. Defense-in-depth: only unlink
        // when the on-disk (dev, ino) still matches the socket
        // we bound. `validate_control_socket_parent`'s
        // fail-closed on writable parents is the load-bearing
        // guard; this check refuses to remove a foreign inode
        // even in the residual case where the parent contract
        // was somehow broken after startup validation ran.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            match (bound_inode, std::fs::symlink_metadata(&self.socket_path)) {
                (Some((dev, ino)), Ok(meta)) if meta.dev() == dev && meta.ino() == ino => {
                    let _ = std::fs::remove_file(&self.socket_path);
                }
                (Some((dev, ino)), Ok(meta)) => {
                    warn!(
                        "control socket: path swapped since bind (bound dev/ino {}/{}, now \
                         {}/{}); refusing to unlink foreign inode",
                        dev,
                        ino,
                        meta.dev(),
                        meta.ino()
                    );
                }
                (Some(_), Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Already gone — nothing to unlink.
                }
                (Some(_), Err(e)) => {
                    warn!(
                        "control socket: failed to re-stat before shutdown unlink: {}",
                        e
                    );
                }
                (None, _) => {
                    // We never recorded the bound inode (best-
                    // effort at startup); fall back to a
                    // path-based unlink under the safe-parent
                    // assumption enforced by
                    // `validate_control_socket_parent`.
                    let _ = std::fs::remove_file(&self.socket_path);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    tap: Arc<TapRegistry>,
    metrics: Arc<MetricsRegistry>,
    config: Arc<CompiledConfig>,
    input_senders: Arc<HashMap<String, InputInjectTarget>>,
    output_senders: Arc<HashMap<String, QueueSender>>,
    started_at: Instant,
) {
    let (reader, mut writer) = stream.into_split();
    // Limit the FIRST line read to MAX_COMMAND_LEN bytes to prevent OOM,
    // then unwrap for streaming commands (inject) that need unbounded reads.
    let limited = reader.take(MAX_COMMAND_LEN as u64);
    let mut reader = BufReader::new(limited);

    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => return,
        Ok(_) => {}
        Err(e) => {
            debug!("control socket: read error: {}", e);
            return;
        }
    }

    if !line.ends_with('\n') {
        let _ = writer.write_all(b"error: command too long\n").await;
        return;
    }

    let cmd = line.trim();
    debug!("control socket: received command: {}", cmd);

    if let Some(inject_args) = cmd.strip_prefix("inject ") {
        let parts: Vec<&str> = inject_args.split_whitespace().collect();
        let (kind, name, json_mode) = match parts.as_slice() {
            [kind, name] if matches!(*kind, "input" | "output") => {
                (*kind, (*name).to_string(), false)
            }
            [kind, name, "json"] if matches!(*kind, "input" | "output") => {
                (*kind, (*name).to_string(), true)
            }
            _ => {
                let _ = writer
                    .write_all(b"error: expected 'inject <input|output> <name> [json]'\n")
                    .await;
                return;
            }
        };
        // Raise the per-connection byte cap for the inject payload, but keep
        // a hard upper bound so a trusted-but-buggy client cannot grow the
        // downstream queue without limit. Any bytes buffered past the first
        // line remain intact inside the BufReader and count toward the cap.
        //
        // We add back the bytes already consumed by the command line so that
        // the *remaining* budget reflects the payload itself.
        let consumed = line.len() as u64;
        let remaining = MAX_INJECT_BYTES.saturating_add(consumed);
        reader.get_mut().set_limit(remaining);
        handle_inject(
            kind,
            &name,
            json_mode,
            reader,
            &mut writer,
            &input_senders,
            &output_senders,
        )
        .await;
        return;
    }

    if let Some(tap_args) = cmd.strip_prefix("tap ") {
        let tap_args = tap_args.trim();
        // Accept:
        //   "<kind> <name>"        → raw message mode
        //   "<kind> <name> json"   → full-Event JSON mode
        let parts: Vec<&str> = tap_args.split_whitespace().collect();
        let (tap_target, json_mode) = match parts.as_slice() {
            [kind, name] if matches!(*kind, "input" | "process" | "output") => {
                (format!("{} {}", kind, name), false)
            }
            [kind, name, "json"] if matches!(*kind, "input" | "process" | "output") => {
                (format!("{} {}", kind, name), true)
            }
            _ => {
                let _ = writer
                    .write_all(b"error: expected 'tap <input|process|output> <name> [json]'\n")
                    .await;
                return;
            }
        };
        match tap.subscribe(&tap_target).await {
            Some(subscription) => {
                handle_tap(&tap_target, subscription, &mut writer, json_mode).await;
            }
            None => {
                let _ = writer
                    .write_all(format!("error: unknown tap point '{}'\n", tap_target).as_bytes())
                    .await;
            }
        }
    } else {
        let response = match cmd {
            "health" => {
                let uptime = started_at.elapsed().as_secs();
                json!({"status": "ok", "uptime_seconds": uptime}).to_string()
            }
            "stats" => metrics.to_json(),
            "list" => build_list_json(&config),
            _ => json!({"error": format!("unknown command '{}'", cmd)}).to_string(),
        };
        let _ = writer.write_all(response.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
    }
}

/// Build JSON listing of pipelines with their tap points in flow order.
fn build_list_json(config: &CompiledConfig) -> String {
    let mut pipelines = Vec::new();

    let mut names: Vec<&String> = config.pipelines.keys().collect();
    names.sort();

    for name in names {
        let Some(pipeline_def) = config.pipelines.get(name) else {
            continue;
        };
        let mut inputs: Vec<String> = Vec::new();
        let mut processes = Vec::new();
        let mut outputs = Vec::new();

        collect_pipeline_tap_points(
            &pipeline_def.body,
            &mut inputs,
            &mut processes,
            &mut outputs,
        );

        let mut p = Map::new();
        p.insert("name".into(), Value::String(name.clone()));
        // Keep scalar `input` for single-input pipelines (backward-compatible payload),
        // emit `inputs` array when fan-in is in play.
        match inputs.len() {
            0 => {}
            1 => {
                p.insert("input".into(), Value::String(inputs.remove(0)));
            }
            _ => {
                p.insert(
                    "inputs".into(),
                    Value::Array(inputs.into_iter().map(Value::String).collect()),
                );
            }
        }
        p.insert(
            "processes".into(),
            Value::Array(processes.into_iter().map(Value::String).collect()),
        );
        p.insert(
            "outputs".into(),
            Value::Array(outputs.into_iter().map(Value::String).collect()),
        );
        pipelines.push(Value::Object(p));
    }

    json!({"pipelines": pipelines}).to_string()
}

/// Recursively walk pipeline statements to collect tap points in order.
fn collect_pipeline_tap_points(
    stmts: &[PipelineStatement],
    inputs: &mut Vec<String>,
    processes: &mut Vec<String>,
    outputs: &mut Vec<String>,
) {
    for stmt in stmts {
        match stmt {
            PipelineStatement::Input(names) => {
                for name in names {
                    if !inputs.contains(name) {
                        inputs.push(name.clone());
                    }
                }
            }
            PipelineStatement::ProcessChain(chain) => {
                for elem in chain {
                    match elem {
                        ProcessChainElement::Named(name) => {
                            if !processes.contains(name) {
                                processes.push(name.clone());
                            }
                        }
                        ProcessChainElement::Inline(_) => {
                            // Inline processes don't have tap points
                        }
                    }
                }
            }
            PipelineStatement::Output(name) => {
                if !outputs.contains(name) {
                    outputs.push(name.clone());
                }
            }
            PipelineStatement::If(chain) => {
                for (_, body) in &chain.branches {
                    let stmts: Vec<PipelineStatement> = body
                        .iter()
                        .filter_map(|b| match b {
                            BranchBody::Pipeline(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    collect_pipeline_tap_points(&stmts, inputs, processes, outputs);
                }
                if let Some(else_body) = &chain.else_body {
                    let stmts: Vec<PipelineStatement> = else_body
                        .iter()
                        .filter_map(|b| match b {
                            BranchBody::Pipeline(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    collect_pipeline_tap_points(&stmts, inputs, processes, outputs);
                }
            }
            PipelineStatement::Switch(_, arms) => {
                for arm in arms {
                    let stmts: Vec<PipelineStatement> = arm
                        .body
                        .iter()
                        .filter_map(|b| match b {
                            BranchBody::Pipeline(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect();
                    collect_pipeline_tap_points(&stmts, inputs, processes, outputs);
                }
            }
            PipelineStatement::Drop | PipelineStatement::Finish | PipelineStatement::Error(_) => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_inject(
    kind: &str,
    name: &str,
    json_mode: bool,
    mut reader: BufReader<tokio::io::Take<tokio::net::unix::OwnedReadHalf>>,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    input_senders: &HashMap<String, InputInjectTarget>,
    output_senders: &HashMap<String, QueueSender>,
) {
    enum Target {
        Input(mpsc::Sender<Event>, Arc<crate::metrics::InputMetrics>),
        Output(QueueSender),
    }

    let target = match kind {
        "input" => match input_senders.get(name) {
            Some((tx, metrics)) => Target::Input(tx.clone(), Arc::clone(metrics)),
            None => {
                let _ = writer
                    .write_all(format!("error: unknown input '{}'\n", name).as_bytes())
                    .await;
                return;
            }
        },
        "output" => match output_senders.get(name) {
            Some(tx) => Target::Output(tx.clone()),
            None => {
                let _ = writer
                    .write_all(format!("error: unknown output '{}'\n", name).as_bytes())
                    .await;
                return;
            }
        },
        _ => {
            let _ = writer
                .write_all(b"error: inject kind must be 'input' or 'output'\n")
                .await;
            return;
        }
    };

    let default_source: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut injected: u64 = 0;
    let mut line = String::new();
    let mut limit_exceeded = false;

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // Distinguish true EOF from byte-cap exhaustion. When the
                // underlying Take hits its limit, read_line also returns
                // Ok(0) — but the limit will be 0.
                if reader.get_ref().limit() == 0 {
                    limit_exceeded = true;
                }
                break;
            }
            Ok(_) => {}
            Err(e) => {
                debug!("control socket: inject read error: {}", e);
                break;
            }
        }

        // Strip trailing newline(s)
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }

        let event = if json_mode {
            match Event::from_json(trimmed) {
                Some(ev) => ev,
                None => {
                    warn!("inject {} '{}': skipping invalid JSON line", kind, name);
                    continue;
                }
            }
        } else {
            Event::new(Bytes::copy_from_slice(trimmed.as_bytes()), default_source)
        };

        let ok = match &target {
            Target::Input(tx, metrics) => {
                let sent = tx.send(event).await.is_ok();
                if sent {
                    metrics
                        .events_injected
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                sent
            }
            Target::Output(tx) => {
                // Inject is a cold path that hands an OwnedEvent
                // straight to the output's queue. After this change
                // every queue carries `Event` end-to-end, so there is no
                // longer a separate `send_owned` codepath — `send`
                // is the only entry.
                let sent = tx.send(event).await.is_ok();
                if sent && let Some(m) = tx.metrics() {
                    m.events_injected
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                sent
            }
        };
        if !ok {
            warn!("inject {} '{}': downstream channel closed", kind, name);
            break;
        }
        injected += 1;
    }

    if limit_exceeded {
        warn!(
            "inject {} '{}': stream exceeded {} byte cap after {} events — connection dropped",
            kind, name, MAX_INJECT_BYTES, injected
        );
    }

    let response = if limit_exceeded {
        json!({
            "injected": injected,
            "error": format!("inject payload exceeded {} byte cap", MAX_INJECT_BYTES),
        })
        .to_string()
    } else {
        json!({ "injected": injected }).to_string()
    };
    let _ = writer.write_all(response.as_bytes()).await;
    let _ = writer.write_all(b"\n").await;
}

async fn handle_tap(
    output_name: &str,
    mut subscription: crate::tap::TapSubscription,
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    json_mode: bool,
) {
    // Skip the human-readable header in JSON mode so output is pure NDJSON
    // (safe to pipe to `jq` or `limpidctl inject --json`).
    if !json_mode {
        let _ = writer
            .write_all(format!("tapping '{}' — events will stream below\n", output_name).as_bytes())
            .await;
    }

    loop {
        match subscription.recv().await {
            Ok(event) => {
                let line = if json_mode {
                    event.to_json_string()
                } else {
                    String::from_utf8_lossy(&event.egress).into_owned()
                };
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                if writer
                    .write_all(
                        format!("[warning: dropped {} events due to slow reader]\n", n).as_bytes(),
                    )
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let _ = writer.write_all(b"[output closed]\n").await;
                break;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Protocol round-trip tests
// ---------------------------------------------------------------------------
//
// `limpidctl` (crates/limpidctl/src/main.rs) and `limpid-prometheus`
// (crates/limpid-prometheus/src/main.rs) each hand-build the command
// strings this module's line parser above (`handle_connection`)
// expects — there is no shared crate defining the wire grammar.
//
// Scope: these tests pin the *daemon-parser side* of that protocol —
// each `#[test]` sends the exact string shape limpidctl builds today
// and asserts the parser accepts it. A future edit to the parser that
// tightens what it accepts (or renames a command) gets caught here.
// The client-side literals themselves are not imported (limpidctl is
// a separate crate), so a driver-side edit that starts sending a
// different literal is NOT caught here — that drift is limpidctl's
// own unit-test surface. A shared "protocol crate" was rejected as
// over-engineering for a two-writer, one-reader line protocol; these
// tests are the cheaper alternative that still pin the parser end.
#[cfg(test)]
mod protocol_round_trip_tests {
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    use super::*;
    use crate::dsl::parser::parse_config;

    /// Spins up a real `ControlServer` on a temp-dir socket with an
    /// empty config/metrics/tap registry (sufficient for `health` /
    /// `stats` / `list`, and for pinning that `tap` / `inject` command
    /// shapes reach their kind/name validation instead of being
    /// rejected as unparseable). Returns the socket path and a
    /// shutdown sender; the server task is aborted on drop via the
    /// watch channel going out of scope in the caller's control, so
    /// tests explicitly signal shutdown at the end.
    async fn spawn_server() -> (
        std::path::PathBuf,
        tempfile::TempDir,
        tokio::sync::watch::Sender<bool>,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket_path = dir.path().join("control.sock");

        let config = CompiledConfig::from_config(
            parse_config(
                r#"
def input i { type syslog_tcp bind "127.0.0.1:0" }
def output o { type stdout }
def pipeline p { input i; output o }
"#,
            )
            .expect("parse"),
        )
        .expect("compile");

        let server = ControlServer::new(
            Some(socket_path.to_string_lossy().into_owned()),
            TapRegistry::new(),
            Arc::new(MetricsRegistry::new()),
            Arc::new(config),
            HashMap::new(),
            Arc::new(HashMap::new()),
            Instant::now(),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(server.run(shutdown_rx));

        // Poll for the socket file to appear instead of a fixed sleep —
        // bind happens early in `run` but is still async relative to
        // this task's spawn.
        for _ in 0..100 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(socket_path.exists(), "control socket never appeared");

        (socket_path, dir, shutdown_tx)
    }

    /// Send `command` (already newline-terminated by the caller's
    /// format string, matching how `limpidctl` calls
    /// `writeln!(stream, "{}", command)`) and return the first
    /// response line.
    async fn send_command(socket_path: &std::path::Path, command: &str) -> String {
        let mut stream = UnixStream::connect(socket_path)
            .await
            .expect("connect to control socket");
        stream
            .write_all(format!("{}\n", command).as_bytes())
            .await
            .expect("write command");
        let (reader, _writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read response");
        line
    }

    #[tokio::test]
    async fn health_command_as_built_by_limpidctl_and_prometheus() {
        // Both callers send the bare command with no arguments.
        let (socket_path, _dir, shutdown_tx) = spawn_server().await;
        let resp = send_command(&socket_path, "health").await;
        assert!(
            resp.contains("\"status\":\"ok\""),
            "unexpected health response: {}",
            resp
        );
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn stats_command_as_built_by_limpidctl_and_prometheus() {
        let (socket_path, _dir, shutdown_tx) = spawn_server().await;
        let resp = send_command(&socket_path, "stats").await;
        // Any well-formed JSON object counts as "parsed", as opposed to
        // the `{"error":"unknown command '...'"}"` fallback.
        assert!(
            !resp.contains("unknown command"),
            "stats command was not recognised: {}",
            resp
        );
        let parsed: serde_json::Value = serde_json::from_str(resp.trim()).expect("valid JSON");
        assert!(parsed.is_object(), "stats response not an object: {}", resp);
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn list_command_as_built_by_limpidctl() {
        let (socket_path, _dir, shutdown_tx) = spawn_server().await;
        let resp = send_command(&socket_path, "list").await;
        assert!(
            !resp.contains("unknown command"),
            "list command was not recognised: {}",
            resp
        );
        let parsed: serde_json::Value = serde_json::from_str(resp.trim()).expect("valid JSON");
        assert!(parsed.is_object(), "list response not an object: {}", resp);
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn tap_command_shapes_as_built_by_limpidctl() {
        // `limpidctl tap <kind> <name> [json]` builds exactly these two
        // shapes (see `main()`'s `Command::Tap` arm). Both must clear
        // the parser's `strip_prefix("tap ")` + kind/name match and
        // reach `tap.subscribe`, which then reports "unknown tap
        // point" for a name this test never registered — a parser
        // rejection would instead read "expected 'tap ...'".
        let (socket_path, _dir, shutdown_tx) = spawn_server().await;
        for command in ["tap input i", "tap input i json"] {
            let resp = send_command(&socket_path, command).await;
            assert!(
                resp.contains("unknown tap point"),
                "command {:?} was rejected at the parser level: {}",
                command,
                resp
            );
        }
        let _ = shutdown_tx.send(true);
    }

    #[tokio::test]
    async fn inject_command_shapes_as_built_by_limpidctl() {
        // `limpidctl inject <kind> <name> [json]` builds exactly these
        // two shapes (see `main()`'s `Command::Inject` arm). Both must
        // clear `strip_prefix("inject ")` + kind/name match and reach
        // the sender-map lookup, which reports "unknown input" for a
        // name this test never registered — a parser rejection would
        // instead read "expected 'inject ...'".
        let (socket_path, _dir, shutdown_tx) = spawn_server().await;
        for command in ["inject input i", "inject input i json"] {
            let resp = send_command(&socket_path, command).await;
            assert!(
                resp.contains("unknown input"),
                "command {:?} was rejected at the parser level: {}",
                command,
                resp
            );
        }
        let _ = shutdown_tx.send(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn validate_creates_absent_parent_at_0o750_under_daemon_owned_ancestor() {
        // Absent parent + ancestor owned by the daemon's euid (the
        // tempdir default) → validate creates the parent at 0o750
        // and passes.
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::TempDir::new().unwrap();
        let parent = base.path().join("limpid-run");
        assert!(!parent.exists());
        let socket = parent.join("control.sock");
        validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap();
        assert!(parent.exists());
        let mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o750,
            "validate-created parent must not be world-traversable"
        );
    }

    #[test]
    #[cfg(unix)]
    fn parent_dir_mode_unsafe_flags_group_write_or_other_traverse() {
        // 0o750 (the daemon's own target mode: group rwx, no other bits)
        // and 0o700 (owner-only) uphold both TOCTOU-mitigating
        // properties: no group/other write (no symlink/file race), and
        // no other execute (an outside-the-group user can't even
        // traverse to the socket inode during the bind->chmod window).
        assert!(!parent_dir_mode_is_unsafe(0o750));
        assert!(!parent_dir_mode_is_unsafe(0o700));
        // 0o755 is world-traversable (other execute set) even though it
        // isn't world-writable — a user outside the daemon's group can
        // still reach the socket inode during the window, so this must
        // be flagged even though the earlier (narrower) write-only
        // check would have missed it.
        assert!(parent_dir_mode_is_unsafe(0o755));
        assert!(parent_dir_mode_is_unsafe(0o777)); // other write + traverse
        assert!(parent_dir_mode_is_unsafe(0o770)); // group write
        assert!(parent_dir_mode_is_unsafe(0o757)); // other write + traverse
        assert!(parent_dir_mode_is_unsafe(0o751)); // other execute only, no write
        // Group execute alone (no group/other write, no other execute)
        // is expected and not flagged — group is the socket's own
        // trusted access group per the 0o660 chmod.
        assert!(!parent_dir_mode_is_unsafe(0o710));
    }

    #[test]
    #[cfg(unix)]
    fn parent_dir_owner_untrusted_flags_non_matching_owner() {
        // Root daemon + root-owned parent is trusted (the packaged
        // shape when limpid runs as root).
        assert!(!parent_dir_owner_is_untrusted(0, 0));
        // Non-root daemon + daemon-owned parent is trusted (systemd's
        // `RuntimeDirectory=limpid` + `User=limpid` produces this
        // shape at 0o750).
        assert!(!parent_dir_owner_is_untrusted(1000, 1000));
        // Non-root daemon + root-owned parent is UNTRUSTED even though
        // root ownership sounds safer: at the packaged `0o750` mode
        // the daemon has no write permission on the parent, so `bind`
        // would fail post-validation and the fire-and-forget control
        // task would die silently — the exact failure shape this
        // check exists to prevent.
        assert!(parent_dir_owner_is_untrusted(0, 1000));
        // Root daemon + non-root parent is untrusted (someone else
        // owns the dir; even a root daemon should not bind into an
        // attacker-controlled directory).
        assert!(parent_dir_owner_is_untrusted(1000, 0));
        // Any unrelated uid is untrusted.
        assert!(parent_dir_owner_is_untrusted(1001, 1000));
        assert!(parent_dir_owner_is_untrusted(65534, 1000)); // nobody
    }

    #[test]
    #[cfg(unix)]
    fn validate_bails_when_final_parent_component_is_a_symlink() {
        // A symlink final parent lets an attacker redirect the bind
        // target between validation and the daemon's `bind`. The
        // check uses `symlink_metadata` so the symlink itself is
        // seen (not followed) and the daemon startup bails.
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::TempDir::new().unwrap();
        let real = base.path().join("real-parent");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o750)).unwrap();
        let link = base.path().join("symlink-parent");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let socket = link.join("control.sock");
        let err = validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("symlink") && msg.contains("refusing to bind"),
            "diagnostic must name the symlink shape: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn validate_accepts_ancestor_symlink_when_final_parent_is_a_real_dir() {
        // Modern Linux ships `/var/run` as a symlink to `/run` — the
        // default `/var/run/limpid/control.sock` has an ancestor
        // symlink but a real final parent `limpid`. Ancestor symlinks
        // must not trip the check.
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::TempDir::new().unwrap();
        let real_ancestor = base.path().join("real-ancestor");
        std::fs::create_dir_all(&real_ancestor).unwrap();
        std::fs::set_permissions(&real_ancestor, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link_ancestor = base.path().join("link-ancestor");
        std::os::unix::fs::symlink(&real_ancestor, &link_ancestor).unwrap();
        let final_parent = link_ancestor.join("limpid");
        std::fs::create_dir_all(&final_parent).unwrap();
        std::fs::set_permissions(&final_parent, std::fs::Permissions::from_mode(0o750)).unwrap();
        let socket = final_parent.join("control.sock");
        validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn nearest_existing_ancestor_walks_up_through_missing_components() {
        let base = tempfile::TempDir::new().unwrap();
        let deep = base.path().join("a/b/c/d");
        let found = super::nearest_existing_ancestor(&deep).unwrap();
        assert_eq!(found, base.path());
    }

    #[test]
    #[cfg(unix)]
    fn ancestor_trust_predicate_covers_all_shapes() {
        // Trusted: daemon-owned OR root-owned, AND not group- or
        // world-writable.
        assert!(ancestor_is_trusted_for_create(1000, 0o755, 1000));
        assert!(ancestor_is_trusted_for_create(1000, 0o750, 1000));
        assert!(ancestor_is_trusted_for_create(1000, 0o700, 1000));
        assert!(ancestor_is_trusted_for_create(0, 0o755, 1000));
        assert!(ancestor_is_trusted_for_create(0, 0o750, 1000));
        assert!(ancestor_is_trusted_for_create(0, 0o700, 0));
        // Untrusted: daemon-owned but group- or world-writable. Owner
        // trust does not override directory write permission — an
        // outside user with write access to the ancestor can plant a
        // node under the target name and race the create.
        assert!(!ancestor_is_trusted_for_create(1000, 0o777, 1000));
        assert!(!ancestor_is_trusted_for_create(1000, 0o775, 1000));
        assert!(!ancestor_is_trusted_for_create(1000, 0o757, 1000));
        // Untrusted: root-owned + world-writable (sticky-bit `/tmp`
        // shape at `0o1777`, mode masked to `0o777`) — sticky stops
        // unlink of other users' files but not create-in.
        assert!(!ancestor_is_trusted_for_create(0, 0o777, 1000));
        assert!(!ancestor_is_trusted_for_create(0, 0o1777 & 0o777, 1000));
        // Untrusted: non-root, non-self-owned ancestor regardless of
        // mode.
        assert!(!ancestor_is_trusted_for_create(1001, 0o755, 1000));
        assert!(!ancestor_is_trusted_for_create(1001, 0o700, 1000));
        assert!(!ancestor_is_trusted_for_create(65534, 0o755, 1000));
    }

    /// Fail-closed startup validation: a pre-existing parent
    /// whose mode fails `parent_dir_mode_is_unsafe` must bail
    /// with a diagnostic that names the observed mode and the
    /// remediation. This is the whole daemon's startup guard;
    /// the previous warn-only shape let the control socket
    /// die silently while the daemon ran on.
    #[test]
    #[cfg(unix)]
    fn validate_control_socket_parent_bails_on_unsafe_preexisting_mode() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::TempDir::new().unwrap();
        let parent = base.path().join("bad-run");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        let socket = parent.join("control.sock");

        let err = validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("0o777"),
            "diagnostic must name the mode: {msg}"
        );
        assert!(
            msg.contains("refusing to bind"),
            "diagnostic must state the refusal: {msg}"
        );
    }

    /// A pre-existing parent whose mode passes the predicate
    /// (systemd's `RuntimeDirectory=limpid` with
    /// `RuntimeDirectoryMode=0750`, or an operator that already
    /// tightened the path) must not trigger any warning or
    /// bail — this is the flagship packaged-deploy path and
    /// must stay a silent no-op.
    #[test]
    #[cfg(unix)]
    fn validate_control_socket_parent_accepts_safe_preexisting_mode() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::TempDir::new().unwrap();
        let parent = base.path().join("good-run");
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o750)).unwrap();
        let socket = parent.join("control.sock");

        validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap();
    }

    /// An absent parent is OK — the subsequent
    /// `ensure_socket_parent_dir` call in the control task
    /// will create it at 0o750, which passes the predicate by
    /// construction. This preserves the bare / non-systemd
    /// developer workflow of pointing `control { socket "..." }`
    /// at a path whose parent doesn't exist yet.
    #[test]
    fn validate_control_socket_parent_accepts_absent_parent() {
        let base = tempfile::TempDir::new().unwrap();
        let socket = base.path().join("missing-subdir").join("control.sock");
        validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap();
    }

    /// A parent path that exists but isn't a directory (a
    /// stray file left by a misconfigured operator) must bail
    /// with a diagnostic naming the config value — a bind
    /// attempt would otherwise fail with an opaque `ENOTDIR`
    /// on the socket create.
    #[test]
    #[cfg(unix)]
    fn validate_control_socket_parent_bails_when_parent_is_not_a_directory() {
        let base = tempfile::TempDir::new().unwrap();
        let file_at_parent = base.path().join("not-a-dir");
        std::fs::write(&file_at_parent, b"oops").unwrap();
        let socket = file_at_parent.join("control.sock");

        let err = validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a directory"),
            "diagnostic must name the shape: {msg}"
        );
    }
}
