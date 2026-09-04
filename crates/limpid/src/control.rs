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

#[cfg(test)]
use crate::dsl::ast::*;
use crate::event::Event;
use crate::metrics::Registry;
#[cfg(test)]
use crate::pipeline::CompiledConfig;
use crate::pipeline::RuntimeBlueprint;
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

async fn shutdown_change_is_terminal(shutdown: &mut tokio::sync::watch::Receiver<bool>) -> bool {
    match shutdown.changed().await {
        Ok(()) => *shutdown.borrow(),
        Err(_) => true,
    }
}

/// Per-input inject target: event channel + metrics handle (for events_injected).
pub type InputInjectTarget = (mpsc::Sender<Event>, Arc<crate::metrics::InputMetrics>);

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
             `RuntimeDirectory=limpid` combined with `User=<daemon-user>` (the packaged \
             unit ships with `User=syslog`) creates a daemon-owned parent at the requested \
             mode — that is the intended shape. For custom deploys, \
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
///
/// Relative paths anchor to the current working directory. On Unix,
/// `Path::new("foo").parent()` returns `Some("")`, and
/// `symlink_metadata` on an empty path fails with `ENOENT`, so the
/// naive walk would then hit `Path::new("").parent() == None` and
/// bail. Treating an empty candidate as `.` (the cwd) keeps a
/// relative config like `control { socket "missing/control.sock" }`
/// walking up to a real ancestor instead of raising a spurious
/// "no existing ancestor" at startup.
#[cfg(unix)]
fn nearest_existing_ancestor(path: &std::path::Path) -> anyhow::Result<PathBuf> {
    let mut current = path;
    loop {
        let Some(parent) = current.parent() else {
            anyhow::bail!("no existing ancestor for {:?}", path);
        };
        let candidate: &std::path::Path = if parent.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            parent
        };
        if std::fs::symlink_metadata(candidate).is_ok() {
            return Ok(candidate.to_path_buf());
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
/// `RuntimeDirectory=limpid` with `User=<daemon-user>` — the packaged
/// unit ships with `User=syslog` — produces exactly this shape).
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
    metrics: Arc<Registry>,
    blueprint: Arc<RuntimeBlueprint>,
    input_senders: Arc<HashMap<String, InputInjectTarget>>,
    output_senders: Arc<HashMap<String, QueueSender>>,
    started_at: Instant,
}

impl ControlServer {
    pub fn new(
        socket_path: Option<String>,
        tap: TapRegistry,
        metrics: Arc<Registry>,
        blueprint: Arc<RuntimeBlueprint>,
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
            blueprint,
            input_senders: Arc::new(input_senders),
            output_senders,
            started_at,
        }
    }

    pub async fn run(
        self,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
        mut startup: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    ) {
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
                        let diagnostic = format!(
                            "control socket: {:?} is a symlink — refusing to remove",
                            self.socket_path
                        );
                        error!(
                            "control socket: {:?} is a symlink — refusing to remove",
                            self.socket_path
                        );
                        send_control_startup(&mut startup, Err(diagnostic));
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
                        send_control_startup(
                            &mut startup,
                            Err(format!(
                                "control socket: {:?} is a {shape}; refusing to remove",
                                self.socket_path
                            )),
                        );
                        return;
                    }
                    // Do not steal a live daemon's control path. A successful
                    // stream connection proves an active owner; only the
                    // connection-refused/not-found stale shapes may be
                    // unlinked under the already-validated parent boundary.
                    match std::os::unix::net::UnixStream::connect(&self.socket_path) {
                        Ok(stream) => {
                            drop(stream);
                            let diagnostic = format!(
                                "control socket: {:?} is already owned by an active listener",
                                self.socket_path
                            );
                            error!("{diagnostic}");
                            send_control_startup(&mut startup, Err(diagnostic));
                            return;
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionRefused
                                    | std::io::ErrorKind::NotFound
                            ) =>
                        {
                            if let Err(diagnostic) = classify_stale_socket_removal(
                                &self.socket_path,
                                std::fs::remove_file(&self.socket_path),
                            ) {
                                error!("{diagnostic}");
                                send_control_startup(&mut startup, Err(diagnostic));
                                return;
                            }
                        }
                        Err(error) => {
                            let diagnostic = format!(
                                "control socket: cannot prove existing socket {:?} is stale: {error}",
                                self.socket_path
                            );
                            error!("{diagnostic}");
                            send_control_startup(&mut startup, Err(diagnostic));
                            return;
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // No node at the path — `bind(2)` will create it
                    // fresh.
                }
                Err(e) => {
                    let diagnostic =
                        format!("control socket: cannot stat {:?}: {e}", self.socket_path);
                    error!("{diagnostic}");
                    send_control_startup(&mut startup, Err(diagnostic));
                    return;
                }
            }
        }

        let listener = match UnixListener::bind(&self.socket_path) {
            Ok(l) => l,
            Err(e) => {
                let diagnostic =
                    format!("control socket: failed to bind {:?}: {e}", self.socket_path);
                error!(
                    "control socket: failed to bind {:?}: {}",
                    self.socket_path, e
                );
                send_control_startup(&mut startup, Err(diagnostic));
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
        // chmod failure is fatal to the control task. A successful
        // `bind` with a subsequent chmod failure would leave a socket
        // whose mode is umask-derived (typically 0o755) instead of
        // the contract-required 0o660 — root-equivalent traffic could
        // reach a socket group-writable to `other`, silently
        // widening the trust boundary. So on chmod failure:
        //
        //   1. record an error diagnostic naming the observed error;
        //   2. best-effort inode-bound unlink the socket we bound
        //      (`(dev, ino)` recorded above must still match — the
        //      swap-check is defensive against a concurrent replace,
        //      which the parent-safety preflight already gates
        //      against);
        //   3. return without entering the accept loop so no client
        //      connects to a mis-moded socket.
        //
        // The startup readiness channel makes this fatal to the daemon
        // transaction as well as to the control task: Runtime::start rolls
        // back every earlier owner and returns the chmod error.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o660);
            if let Err(e) = std::fs::set_permissions(&self.socket_path, perms) {
                error!(
                    "control socket: chmod 0o660 failed on bound socket {:?}: {} — refusing \
                     to listen on a socket whose mode does not match the operator-facing \
                     contract (0o660, root-equivalent); the control task exits without \
                     accepting connections. Fix the filesystem / packaging issue and \
                     restart the daemon.",
                    self.socket_path, e
                );
                if let Some((bound_dev, bound_ino)) = bound_inode {
                    use std::os::unix::fs::MetadataExt;
                    match std::fs::symlink_metadata(&self.socket_path) {
                        Ok(m) if (m.dev(), m.ino()) == (bound_dev, bound_ino) => {
                            if let Err(ue) = std::fs::remove_file(&self.socket_path) {
                                warn!(
                                    "control socket: cleanup unlink after chmod failure \
                                     also failed: {}",
                                    ue
                                );
                            }
                        }
                        Ok(_) => warn!(
                            "control socket: inode at {:?} changed between bind and chmod \
                             failure; refusing to unlink an entry we no longer own",
                            self.socket_path
                        ),
                        Err(se) => warn!(
                            "control socket: failed to re-stat {:?} for cleanup after \
                             chmod failure: {}",
                            self.socket_path, se
                        ),
                    }
                }
                send_control_startup(
                    &mut startup,
                    Err(format!(
                        "control socket: chmod 0o660 failed on {:?}: {e}",
                        self.socket_path
                    )),
                );
                return;
            }
        }

        send_control_startup(&mut startup, Ok(()));

        info!("control socket listening on {:?}", self.socket_path);

        let tap = Arc::new(self.tap);
        let blueprint = self.blueprint;
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

                terminal = shutdown_change_is_terminal(&mut shutdown) => {
                    if terminal {
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
                            let blueprint = Arc::clone(&blueprint);
                            let input_senders = Arc::clone(&input_senders);
                            let output_senders = Arc::clone(&output_senders);
                            conn_handles.push(tokio::spawn(async move {
                                handle_connection(stream, tap, metrics_reg, blueprint, input_senders, output_senders, started_at).await;
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

fn send_control_startup(
    startup: &mut Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    result: std::result::Result<(), String>,
) {
    if let Some(sender) = startup.take() {
        let _ = sender.send(result);
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    tap: Arc<TapRegistry>,
    metrics: Arc<Registry>,
    blueprint: Arc<RuntimeBlueprint>,
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
                // Output-flavor tap projects `workspace` out of its JSON
                // shape unconditionally (0.7.10 contract): the pipeline's
                // `output` statement drops workspace from the memory-
                // queue snapshot but not the disk-queue one, and
                // projecting here as well is what makes the operator-
                // facing tap output shape independent of the queue kind
                // the operator happened to configure. `process` and
                // `input` taps keep workspace — process tap is exactly
                // where operators debug workspace state, and input tap
                // fires before any process has populated it.
                let strip_workspace_json = tap_target.starts_with("output ");
                handle_tap(
                    &tap_target,
                    subscription,
                    &mut writer,
                    json_mode,
                    strip_workspace_json,
                )
                .await;
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
            "stats" => serde_json::to_string(&metrics.snapshot())
                .expect("MetricsSnapshot serialization must remain infallible"),
            "list" => build_list_json(&blueprint),
            _ => json!({"error": format!("unknown command '{}'", cmd)}).to_string(),
        };
        let _ = writer.write_all(response.as_bytes()).await;
        let _ = writer.write_all(b"\n").await;
    }
}

/// Build JSON listing of pipelines with their tap points in flow order.
fn build_list_json(blueprint: &RuntimeBlueprint) -> String {
    let pipeline_defs = blueprint
        .pipelines()
        .map(|(_, pipeline)| pipeline)
        .collect();

    build_list_json_from_pipelines(pipeline_defs)
}

fn build_list_json_from_pipelines(
    mut pipeline_defs: Vec<&crate::pipeline::PipelineBlueprint>,
) -> String {
    let mut pipelines = Vec::new();
    pipeline_defs.sort_unstable_by(|left, right| left.name.cmp(&right.name));

    for pipeline in pipeline_defs {
        let mut inputs = pipeline.flow.inputs.clone();

        let mut p = Map::new();
        p.insert("name".into(), Value::String(pipeline.name.clone()));
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
            Value::Array(
                pipeline
                    .flow
                    .processes
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        p.insert(
            "outputs".into(),
            Value::Array(
                pipeline
                    .flow
                    .outputs
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        pipelines.push(Value::Object(p));
    }

    json!({"pipelines": pipelines}).to_string()
}

/// Recursively walk pipeline statements to collect tap points in order.
#[cfg(test)]
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

#[cfg(test)]
fn build_list_json_legacy(config: &CompiledConfig) -> String {
    let mut pipelines = Vec::new();
    let mut names: Vec<&String> = config.pipelines.keys().collect();
    names.sort();
    for name in names {
        let pipeline = &config.pipelines[name];
        let mut inputs = Vec::new();
        let mut processes = Vec::new();
        let mut outputs = Vec::new();
        collect_pipeline_tap_points(&pipeline.body, &mut inputs, &mut processes, &mut outputs);
        let mut value = Map::new();
        value.insert("name".into(), Value::String(name.clone()));
        match inputs.len() {
            0 => {}
            1 => {
                value.insert("input".into(), Value::String(inputs.remove(0)));
            }
            _ => {
                value.insert(
                    "inputs".into(),
                    Value::Array(inputs.into_iter().map(Value::String).collect()),
                );
            }
        }
        value.insert(
            "processes".into(),
            Value::Array(processes.into_iter().map(Value::String).collect()),
        );
        value.insert(
            "outputs".into(),
            Value::Array(outputs.into_iter().map(Value::String).collect()),
        );
        pipelines.push(Value::Object(value));
    }
    json!({"pipelines": pipelines}).to_string()
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
                    metrics.events_injected.inc();
                }
                sent
            }
            Target::Output(tx) => {
                // A direct output injection has no pipeline Output statement,
                // so its queue-entry boundary is stamped here, immediately
                // before the potentially blocking enqueue.
                let sent = tx
                    .send(crate::event::QueuedEvent::new(
                        event,
                        crate::time::UnixNanos::now(),
                    ))
                    .await
                    .is_ok();
                if sent && let Some(m) = tx.metrics() {
                    m.events_injected.inc();
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
    strip_workspace_json: bool,
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
                    if strip_workspace_json {
                        serde_json::to_string(&event.to_json_value_without_workspace())
                            .unwrap_or_default()
                    } else {
                        event.to_json_string()
                    }
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

        let metrics = Arc::new(crate::metrics::Registry::new());
        let received = metrics
            .counter("limpid_input_events_received_total")
            .help("Total events received by an input.")
            .label("input", "control_test")
            .build()
            .expect("test metric registration must succeed");
        received.inc();
        crate::metrics::register_build_info(&metrics, "0.8.1", "control-node")
            .expect("build info registration must succeed");

        let server = ControlServer::new(
            Some(socket_path.to_string_lossy().into_owned()),
            TapRegistry::new(),
            metrics,
            crate::pipeline::compile_runtime_blueprint(&config).expect("compile blueprint"),
            HashMap::new(),
            Arc::new(HashMap::new()),
            Instant::now(),
        );

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        tokio::spawn(server.run(shutdown_rx, None));

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
        assert!(
            !resp.contains("unknown command"),
            "stats command was not recognised: {}",
            resp
        );
        let parsed: serde_json::Value = serde_json::from_str(resp.trim()).expect("valid JSON");
        assert_eq!(parsed["schema"], 1);
        let metrics = parsed["metrics"]
            .as_array()
            .expect("stats metrics must be an array");
        let family = metrics
            .iter()
            .find(|metric| metric["name"] == "limpid_input_events_received_total")
            .expect("stats must serialize the typed registry snapshot");
        assert_eq!(family["type"], "counter");
        assert_eq!(family["series"][0]["labels"]["input"], "control_test");
        assert_eq!(family["series"][0]["value"], 1);
        let build_info = metrics
            .iter()
            .find(|metric| metric["name"] == "limpid_build_info")
            .expect("stats must include build info");
        assert_eq!(build_info["type"], "gauge");
        assert_eq!(
            build_info["help"],
            "Build information for the running limpid node."
        );
        assert_eq!(
            build_info["series"],
            serde_json::json!([{
                "labels": {"node_id": "control-node", "version": "0.8.1"},
                "value": 1
            }])
        );
        assert!(parsed.get("inputs").is_none());
        assert!(parsed.get("pipelines").is_none());
        assert!(parsed.get("outputs").is_none());
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

#[cfg(unix)]
fn classify_stale_socket_removal(
    socket_path: &std::path::Path,
    removal: std::io::Result<()>,
) -> Result<(), String> {
    match removal {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "control socket: failed to remove stale socket {socket_path:?}: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_blueprint_list_json_preserves_flow_order_and_input_shape() {
        let config = CompiledConfig::from_config(
            crate::dsl::parser::parse_config(
                r#"
def process first { egress = ingress }
def process nested { egress = ingress }
def input one { type syslog_udp bind "127.0.0.1:0" }
def input two { type syslog_udp bind "127.0.0.1:0" }
def output alpha { type stdout }
def output omega { type stdout }
def pipeline fan_in {
    input one, two
    process first
    process { egress = ingress }
    if true {
        process { egress = ingress }
        process nested
        process { egress = ingress }
        output omega
    } else { output alpha }
    output alpha
}
def pipeline scalar { input one; output omega }
"#,
            )
            .expect("parse flow fixture"),
        )
        .expect("compile flow fixture");
        let blueprint =
            crate::pipeline::compile_runtime_blueprint(&config).expect("compile flow blueprint");
        assert_eq!(
            build_list_json(&blueprint),
            build_list_json_legacy(&config),
            "control list JSON bytes, named-only process flow, and scalar/array input shape drifted"
        );

        let routing_split = CompiledConfig::from_config(
            crate::dsl::parser::parse_config(
                r#"
def input a { type syslog_udp bind "127.0.0.1:0" }
def input b { type syslog_udp bind "127.0.0.1:0" }
def input c { type syslog_udp bind "127.0.0.1:0" }
def pipeline p {
    input a
    input b
    if true { input c; finish } else { finish }
}
"#,
            )
            .expect("parse routing/control fixture"),
        )
        .expect("compile routing/control fixture");
        let routing_blueprint = crate::pipeline::compile_runtime_blueprint(&routing_split)
            .expect("compile routing/control blueprint");
        assert_eq!(
            build_list_json(&routing_blueprint),
            build_list_json_legacy(&routing_split),
            "recursive control input union must remain byte-exact while routing stays top-level-first"
        );
    }

    #[test]
    fn sealed_blueprint_list_json_preserves_lexical_pipeline_order() {
        let config = CompiledConfig::from_config(
            crate::dsl::parser::parse_config(
                r#"
def input one { type syslog_udp bind "127.0.0.1:0" }
def output omega { type stdout }
def pipeline zeta { input one; output omega }
def pipeline alpha { input one; output omega }
"#,
            )
            .expect("parse lexical-order fixture"),
        )
        .expect("compile lexical-order fixture");
        let blueprint = crate::pipeline::compile_runtime_blueprint(&config)
            .expect("compile lexical-order blueprint");
        let legacy = build_list_json_legacy(&config);
        let mut reversed_pipelines: Vec<_> = blueprint
            .pipelines()
            .map(|(_, pipeline)| pipeline)
            .collect();
        reversed_pipelines.reverse();

        assert_eq!(
            legacy,
            r#"{"pipelines":[{"name":"alpha","input":"one","processes":[],"outputs":["omega"]},{"name":"zeta","input":"one","processes":[],"outputs":["omega"]}]}"#,
            "legacy control list JSON must remain lexical by pipeline name"
        );
        assert_eq!(
            build_list_json_from_pipelines(reversed_pipelines),
            legacy,
            "control list serialization must enforce lexical order at its own boundary"
        );
    }

    #[test]
    #[cfg(unix)]
    fn stale_socket_removal_accepts_not_found_race_but_preserves_other_errors() {
        let socket = std::path::Path::new("/run/limpid/control.sock");
        assert!(
            classify_stale_socket_removal(
                socket,
                Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
            )
            .is_ok()
        );

        let error = classify_stale_socket_removal(
            socket,
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        )
        .expect_err("non-NotFound removal failures must remain fatal");
        assert!(error.contains("failed to remove stale socket"));
        assert!(error.to_lowercase().contains("permission denied"));
    }

    #[test]
    #[cfg(unix)]
    fn validate_creates_absent_parent_at_0o750_under_daemon_owned_ancestor() {
        // Absent parent + ancestor owned by the daemon's euid → validate
        // creates the parent at 0o750 and passes. The tempdir is
        // explicitly chmod'd to 0o755 so the test runs the same way
        // regardless of the caller's umask; on Linux systems where the
        // default umask is 0o002 the raw tempdir would land at 0o775
        // (group-write set) and fail the `mode & 0o022 == 0` ancestor
        // trust predicate — the operator-facing behaviour, not a bug
        // the test wants to reproduce.
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
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
        // `RuntimeDirectory=limpid` + `User=<daemon-user>` (the
        // packaged unit ships with `User=syslog`) produces this
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

    /// An absent parent is OK — `validate_control_socket_parent`
    /// creates it at 0o750 under the daemon's uid after verifying
    /// the nearest existing ancestor is trusted, then re-verifies
    /// via `symlink_metadata`. Tempdir is chmod'd to 0o755 so the
    /// ancestor trust check doesn't reject it under a Linux
    /// 0o002-umask default that would leave the raw tempdir at
    /// 0o775 (group-write bit set).
    #[test]
    #[cfg(unix)]
    fn validate_control_socket_parent_accepts_absent_parent() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = base.path().join("missing-subdir").join("control.sock");
        validate_control_socket_parent(Some(socket.to_str().unwrap())).unwrap();
    }

    /// A relative socket path with an absent nested parent must not
    /// bail with "no existing ancestor" — the walk anchors to `.`
    /// when named ancestors run out. Regression against the shape
    /// where `Path::new("foo").parent()` returns `Some("")` and
    /// `symlink_metadata("")` fails with `ENOENT`, leaving the naive
    /// walk with no candidate. Uses `set_current_dir` inside a
    /// tempdir (chmod 0o755 so the ancestor trust predicate accepts
    /// it independent of the caller's umask) so the test does not
    /// litter the developer cwd.
    #[test]
    #[cfg(unix)]
    fn validate_control_socket_parent_handles_relative_absent_parent() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;
        static CWD_LOCK: Mutex<()> = Mutex::new(());
        let _guard = CWD_LOCK.lock().unwrap();
        let base = tempfile::TempDir::new().unwrap();
        std::fs::set_permissions(base.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(base.path()).unwrap();
        let result = validate_control_socket_parent(Some("missing-subdir/control.sock"));
        // Restore cwd before asserting so a panic-on-fail leaves a
        // recoverable state for later tests running in the same
        // process.
        std::env::set_current_dir(&prev).unwrap();
        result.unwrap();
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

    /// Structural regression pin for the control-socket chmod
    /// fatalize contract. `ControlServer::run` must:
    ///
    /// - emit an `error!` (not `warn!`) on the chmod-failure arm so
    ///   operators reading `journalctl -u limpid` see the failure
    ///   at the right severity;
    /// - `return` from that arm without entering the accept loop
    ///   (this is what "fatal to the control task" means — the
    ///   daemon as a whole stays up, but no client connects to a
    ///   socket carrying the wrong mode);
    /// - perform an inode-bound cleanup via `remove_file` gated by
    ///   the `(dev, ino)` match on the bound inode, so a
    ///   concurrent replace of the socket entry is not blindly
    ///   unlinked.
    ///
    /// The direct behavioural test (inject a `chmod` failure at
    /// runtime) requires a test-harness plumbing we do not yet
    /// share; a source-level pin is honest for a shape (contract)
    /// rather than a value, and catches the regression that would
    /// reintroduce warn-and-continue.
    #[test]
    fn chmod_failure_arm_is_fatal_and_inode_bound() {
        let src = include_str!("control.rs");
        // Bound to the body of `ControlServer::run` — start at the
        // fn signature and stop at the module-scope closer that
        // follows the `impl ControlServer` block.
        let run_start = src
            .find("pub async fn run(\n        self,")
            .expect("ControlServer::run must exist");
        // The body is not enormous; take a generous slice.
        let slice_end = (run_start + 20_000).min(src.len());
        let body = &src[run_start..slice_end];

        // The chmod-failure arm must emit `error!` and contain the
        // fatalization intent (a `return;` inside the same arm).
        assert!(
            body.contains("chmod 0o660 failed on bound socket"),
            "control socket chmod failure diagnostic must be present"
        );
        assert!(
            body.contains("error!(") && body.contains("refusing \\\n"),
            "chmod failure must use `error!` (not `warn!`) so severity matches the trust \
             boundary the mode is meant to enforce"
        );
        assert!(
            body.contains("(bound_dev, bound_ino)"),
            "chmod-failure cleanup must be gated by a bound (dev, ino) match so a swapped \
             entry is not blindly unlinked"
        );
    }

    #[tokio::test]
    async fn closed_shutdown_watch_is_terminal_for_control_accept_loop() {
        let (sender, mut receiver) = tokio::sync::watch::channel(false);
        drop(sender);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                shutdown_change_is_terminal(&mut receiver),
            )
            .await
            .expect("closed watch must resolve without spinning")
        );
        let marker = ["shutdown_change_is_terminal", "(&mut shutdown)"].concat();
        assert!(include_str!("control.rs").contains(&marker));

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("closed-watch.sock");
        let config = CompiledConfig::from_config(
            crate::dsl::parser::parse_config("").expect("empty config parses"),
        )
        .expect("empty config compiles");
        let server = ControlServer::new(
            Some(socket.to_string_lossy().into_owned()),
            TapRegistry::new(),
            Arc::new(crate::metrics::Registry::new()),
            crate::pipeline::compile_runtime_blueprint(&config).expect("compile blueprint"),
            HashMap::new(),
            Arc::new(HashMap::new()),
            Instant::now(),
        );
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        drop(shutdown_tx);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            server.run(shutdown_rx, None),
        )
        .await
        .expect("actual control accept loop must terminate on a closed watch");
        assert!(
            !socket.exists(),
            "control cleanup must unlink the owned socket"
        );
    }
}
