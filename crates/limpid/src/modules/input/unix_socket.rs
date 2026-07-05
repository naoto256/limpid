//! Unix socket input: receives syslog messages from a Unix datagram socket.
//!
//! Used to receive messages from `logger` and local applications via `/dev/log`.
//!
//! Properties:
//!   path   "/dev/log"   — required

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::net::UnixDatagram;
use tracing::{error, info, warn};

use super::validate::validate_pri;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

const UNIX_SOURCE: &str = "127.0.0.1:0";

/// True when the parent dir's mode lets an outside-the-owner
/// party plant or swap a node at the socket path.
///
/// The unix_socket input binds an intentionally world-writable
/// (`0o666`) datagram socket at `/dev/log`-style paths — the
/// standard `/dev` mode is `0o755`, and any local process must
/// be able to `sendto` the inode, so **other-execute is a
/// requirement, not a threat**. What we must refuse is a parent
/// that grants **write** access outside the owner: a
/// group-writable or world-writable parent lets an attacker
/// swap the socket path for a symlink between shutdown and
/// next bind, or between the stale-cleanup stat and the
/// following `remove_file`.
///
/// Distinct predicate from `control.rs::parent_dir_mode_is_unsafe`,
/// which additionally flags other-execute — the control
/// socket's `0o660` bind→chmod window relies on non-group
/// traversal that the input's `0o666` bind by design does not.
///
/// **Sticky bit + world-writable (`/tmp` at `0o1777`) is
/// deliberately rejected**: the sticky bit only protects
/// against non-owner unlink of files whose owner matches, but
/// under a swap attack the attacker owns the replacement node,
/// so sticky offers no protection here. `/tmp/foo.sock` is
/// unsupported for this input; use `/dev/log` (path in a
/// non-writable parent) or a packaged runtime directory
/// instead.
#[cfg(unix)]
fn parent_dir_mode_is_unsafe_for_input(mode: u32) -> bool {
    mode & 0o022 != 0
}

/// True when the parent dir's owner is neither root (uid 0) nor the
/// daemon's own effective uid. Symmetric with the control-socket
/// helper: an untrusted owner keeps rename/unlink rights inside the
/// directory regardless of mode bits, and can swap the socket inode
/// between the stale-cleanup stat and the follow-up unlink or between
/// shutdown and next bind. `/dev` (root-owned) and packaged runtime
/// directories owned by the daemon's uid pass; a user-created parent
/// owned by an unrelated uid fails even at `0o755`.
#[cfg(unix)]
fn parent_dir_owner_is_untrusted_for_input(uid: u32, self_euid: u32) -> bool {
    uid != 0 && uid != self_euid
}

/// Startup-time validation of the unix_socket input's parent
/// directory. Called from `UnixSocketInput::from_properties`
/// so a fail-closed bail aborts daemon startup via
/// `create_input` — matching the pipeline-side fatal shape of
/// `control::validate_control_socket_parent` and
/// `ErrorLogWriter::validate_at_startup`.
///
/// - Parent absent → bail. Unlike control, this input does
///   not create its own parent (`/dev` is expected to exist);
///   an absent parent almost always means a config typo.
/// - Parent exists but is not a directory → bail.
/// - Parent exists as a directory owned by an untrusted uid
///   (neither root nor the daemon's own effective uid) →
///   bail. `/dev` (root-owned) and daemon-owned runtime dirs
///   pass; a user-created dir owned by an unrelated uid fails
///   independent of mode.
/// - Parent exists as a directory whose mode is group-writable
///   or world-writable → bail. `/dev` at `0o755` passes.
///   `/tmp` at `0o1777` does **not** — see the predicate doc
///   for why sticky is not treated as protective here.
fn validate_unix_socket_input_parent(path: &str) -> Result<()> {
    let path = Path::new(path);
    let Some(parent) = path.parent() else {
        anyhow::bail!("input unix_socket: path {:?} has no parent directory", path);
    };
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::metadata(parent).with_context(|| {
            format!(
                "input unix_socket: parent directory {:?} is not accessible (does it exist?)",
                parent
            )
        })?;
        if !meta.is_dir() {
            anyhow::bail!(
                "input unix_socket: parent {:?} exists but is not a directory; check the `path` value",
                parent
            );
        }
        let uid = meta.uid();
        let self_euid = unsafe { libc::geteuid() };
        if parent_dir_owner_is_untrusted_for_input(uid, self_euid) {
            anyhow::bail!(
                "input unix_socket: parent dir {:?} is owned by uid {} — refusing to bind. \
                 The unix_socket input expects its parent directory to be owned by root \
                 (uid 0) or by the daemon's own effective uid ({}); any other owner can \
                 rename or replace the socket inode between the stale-cleanup stat and the \
                 follow-up unlink, or between shutdown and next bind, even when the mode \
                 looks safe. Point `path` at `/dev/log` (`/dev` is root-owned) or move it \
                 into a daemon-owned runtime directory.",
                parent,
                uid,
                self_euid,
            );
        }
        let mode = meta.permissions().mode() & 0o7777;
        if parent_dir_mode_is_unsafe_for_input(mode & 0o777) {
            anyhow::bail!(
                "input unix_socket: parent dir {:?} has mode 0o{:o} — refusing to bind. \
                 The unix_socket input binds a world-writable (0o666) datagram socket, so a \
                 group- or world-writable parent lets an outside-the-owner process swap the \
                 socket path between shutdown and next bind (or between the stale-cleanup \
                 stat and the follow-up unlink). Tighten the parent to 0o755 or stricter, or \
                 point `path` at `/dev/log` (parent `/dev` is `0o755` on standard POSIX \
                 systems). `/tmp` (`0o1777`) is **not** supported — sticky protects unlink \
                 of files the attacker doesn't own, but a swap attack plants an attacker-owned \
                 node.",
                parent,
                mode & 0o777,
            );
        }
    }
    Ok(())
}

const UNIX_SOCKET_INPUT_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "path",
    required: true,
    repeatable: false,
    exclusive_group: None,
    kind: PropertyValueKind::String,
}];

pub struct UnixSocketInput {
    path: String,
    metrics: Arc<InputMetrics>,
}

impl Module for UnixSocketInput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(UNIX_SOCKET_INPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        _ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let properties = properties.user_properties();
        let path = props::get_string(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("input '{}': unix_socket requires 'path'", name))?;
        // Parent safety fail-closed: refuse startup when the
        // configured path's parent is owned by an untrusted uid
        // or is group-/world-writable. Symmetric with
        // `control::validate_control_socket_parent`; the mode
        // predicate diverges (this sink binds a world-writable
        // datagram socket where other-execute on the parent is a
        // use-case requirement, not a threat), but the owner
        // predicate is identical: only root or the daemon's own
        // effective uid may own the parent.
        validate_unix_socket_input_parent(&path)
            .with_context(|| format!("input '{}': unix_socket startup validation failed", name))?;
        Ok(Self {
            path,
            metrics: Arc::new(InputMetrics::default()),
        })
    }
}

impl HasMetrics for UnixSocketInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for UnixSocketInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        // Stale socket cleanup: unlink the previous socket inode so
        // `bind(2)` can succeed. The removal is narrowed to actual
        // socket nodes — anything else at the path (regular file,
        // directory, FIFO, device node) is refused loudly. The
        // previous shape (`_ => remove_file(...)`) was destructive by
        // design: an operator typo that pointed `path` at
        // `/etc/passwd` would silently unlink the target at daemon
        // startup. This narrowing rejects that shape instead.
        //
        // `symlink_metadata` inspects the link itself rather than
        // following it, so a symlink at the path is caught before we
        // consider `remove_file`.
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            match std::fs::symlink_metadata(&self.path) {
                Ok(meta) => {
                    let ft = meta.file_type();
                    if ft.is_symlink() {
                        error!(
                            "unix_socket: {:?} is a symlink — refusing to remove",
                            self.path
                        );
                        anyhow::bail!("unix_socket: {:?} is a symlink", self.path);
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
                            "unix_socket: {:?} is a {} — refusing to remove",
                            self.path, shape
                        );
                        anyhow::bail!(
                            "unix_socket: {:?} is a {}; refusing to remove. The stale-cleanup \
                             path is intended for actual socket nodes only. Remove or move the \
                             node manually if this path is correct.",
                            self.path,
                            shape
                        );
                    }
                    // Actual stale socket — safe to unlink.
                    let _ = std::fs::remove_file(&self.path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // No node at the path — `bind(2)` will create the
                    // socket fresh.
                }
                Err(e) => {
                    error!("unix_socket: failed to stat {:?}: {}", self.path, e);
                    return Err(anyhow::Error::from(e)
                        .context(format!("unix_socket: failed to stat {:?}", self.path)));
                }
            }
        }

        let socket = UnixDatagram::bind(&self.path)?;
        info!("unix_socket listening on {}", self.path);

        // Record the (dev, ino) of the socket we just bound. Used
        // at shutdown as a defense-in-depth check so the unlink
        // path can refuse to remove a node that has been swapped
        // out from under us since bind. This is best-effort: the
        // primary trust boundary is
        // `validate_unix_socket_input_parent`'s fail-closed on
        // group/other-writable parents. On a safe parent no
        // outside-the-owner writer exists; on an unsafe parent
        // startup would have bailed and we would not be here.
        // If the stat fails we log and continue with the same
        // path-based unlink as before.
        #[cfg(unix)]
        let bound_inode = {
            use std::os::unix::fs::MetadataExt;
            match std::fs::symlink_metadata(&self.path) {
                Ok(meta) => Some((meta.dev(), meta.ino())),
                Err(e) => {
                    warn!(
                        "unix_socket {}: failed to stat bound socket for shutdown \
                         defense-in-depth: {}",
                        self.path, e
                    );
                    None
                }
            }
        };

        // Make socket world-writable so any process can send
        // (like `/dev/log`). Failure here is fatal: the input's
        // operator-facing contract is a `0o666` datagram
        // socket, and running on with an umask-derived mode
        // silently changes who can `sendto` the inode — an
        // operator alarm signal, not a warn-and-continue. The
        // just-bound socket inode is unlinked before we bail
        // so the daemon can be restarted cleanly (the parent is
        // safe by `validate_unix_socket_input_parent` and the
        // inode is the one we just created, so `remove_file`
        // cannot touch a foreign node).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o666))
            {
                error!(
                    "unix_socket {}: failed to set permissions to 0o666: {} — refusing to \
                     listen on a socket whose mode does not match the operator-facing contract",
                    self.path, e
                );
                let _ = std::fs::remove_file(&self.path);
                anyhow::bail!("unix_socket {}: chmod 0o666 failed: {}", self.path, e);
            }
        }

        let source_addr = UNIX_SOURCE.parse().unwrap();
        let mut buf = vec![0u8; 65536];

        loop {
            tokio::select! {
                biased;

                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("unix_socket {}: shutting down", self.path);
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            match (bound_inode, std::fs::symlink_metadata(&self.path)) {
                                (Some((dev, ino)), Ok(meta))
                                    if meta.dev() == dev && meta.ino() == ino =>
                                {
                                    // Same inode we bound at startup —
                                    // safe to unlink.
                                    let _ = std::fs::remove_file(&self.path);
                                }
                                (Some((dev, ino)), Ok(meta)) => {
                                    warn!(
                                        "unix_socket {}: path swapped since bind (bound dev/ino \
                                         {}/{}, now {}/{}); refusing to unlink foreign inode",
                                        self.path,
                                        dev,
                                        ino,
                                        meta.dev(),
                                        meta.ino()
                                    );
                                }
                                (Some(_), Err(e))
                                    if e.kind() == std::io::ErrorKind::NotFound =>
                                {
                                    // Already gone — nothing to unlink.
                                }
                                (Some(_), Err(e)) => {
                                    warn!(
                                        "unix_socket {}: failed to re-stat before shutdown \
                                         unlink: {}",
                                        self.path, e
                                    );
                                }
                                (None, _) => {
                                    // We never recorded the bound
                                    // inode (best-effort at startup);
                                    // fall back to a path-based
                                    // unlink under the safe-parent
                                    // assumption enforced by
                                    // `validate_unix_socket_input_parent`.
                                    let _ = std::fs::remove_file(&self.path);
                                }
                            }
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = std::fs::remove_file(&self.path);
                        }
                        break;
                    }
                }

                result = socket.recv(&mut buf) => {
                    match result {
                        Ok(len) => {
                            let data = &buf[..len];

                            if let Err(e) = validate_pri(data) {
                                warn!("unix_socket: dropping invalid message ({})", e);
                                self.metrics.events_invalid.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }

                            self.metrics.events_received.fetch_add(1, Ordering::Relaxed);

                            let event = Event::new(Bytes::copy_from_slice(data), source_addr);
                            if tx.send(event).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => {
                            error!("unix_socket recv error: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    use std::os::unix::net::UnixDatagram as StdUnixDatagram;

    /// Tiny helper: build the input with a path and a shutdown channel,
    /// spawn `run`, return the join handle + the shutdown sender. The
    /// caller drives shutdown to terminate the listener cleanly.
    fn spawn_with_path(
        path: &std::path::Path,
    ) -> (
        tokio::task::JoinHandle<Result<()>>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::mpsc::Receiver<Event>,
    ) {
        let input = UnixSocketInput {
            path: path.display().to_string(),
            metrics: Arc::new(InputMetrics::default()),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let (sd_tx, sd_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move { input.run(tx, sd_rx).await });
        (handle, sd_tx, rx)
    }

    #[tokio::test]
    async fn refuses_to_run_when_path_is_a_symlink() {
        // The accept loop is supposed to refuse to remove a symlink at
        // the configured socket path. A regression that followed the
        // symlink (via `remove_file` directly, no symlink_metadata
        // check) could let a malicious user pre-place a symlink and
        // make limpid clobber the target file when binding. Pin the
        // refusal behaviour: pre-create the path as a symlink, run
        // the input, assert `run` returns an Err whose message
        // mentions "symlink", and the symlink target is untouched.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"do not clobber").unwrap();
        let socket_path = dir.path().join("sock");
        std::os::unix::fs::symlink(&target, &socket_path).unwrap();

        let (handle, _sd_tx, _rx) = spawn_with_path(&socket_path);
        let result = handle.await.expect("task join");
        let err = result.expect_err("run must error on symlink");
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink-refusal error, got: {err}"
        );
        // Target must be unchanged.
        let body = std::fs::read(&target).unwrap();
        assert_eq!(body, b"do not clobber");
    }

    #[tokio::test]
    async fn refuses_regular_file_at_socket_path() {
        // Contract narrowing (v0.7.9): a regular file at the
        // configured socket path is NOT treated as a stale-socket
        // leftover. The previous implementation removed anything
        // non-symlink; an operator typo that pointed `path` at
        // `/etc/passwd` (or any real file) would silently unlink the
        // target at daemon startup. Cleanup is now scoped to actual
        // socket nodes — anything else is refused.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sock");
        std::fs::write(&socket_path, b"important-content").unwrap();

        let (handle, _sd_tx, _rx) = spawn_with_path(&socket_path);
        let result = handle.await.expect("task join");
        let err = result.expect_err("regular file at socket path must be refused");
        assert!(
            err.to_string().contains("regular file"),
            "refusal must name the shape, got: {err}"
        );
        // The regular file must be untouched — nothing removed.
        let body = std::fs::read(&socket_path).unwrap();
        assert_eq!(body, b"important-content");
    }

    #[tokio::test]
    async fn refuses_directory_at_socket_path() {
        // A directory at the configured path is likewise refused.
        // The old cleanup path would have attempted `remove_file`,
        // which would fail with EISDIR, but the failure would be
        // silent (`let _ = ...`) and then bind would fail with an
        // opaque EADDRINUSE-style error. The narrowed cleanup surfaces
        // the actual misconfiguration up front.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sock");
        std::fs::create_dir(&socket_path).unwrap();

        let (handle, _sd_tx, _rx) = spawn_with_path(&socket_path);
        let result = handle.await.expect("task join");
        let err = result.expect_err("directory at socket path must be refused");
        assert!(
            err.to_string().contains("directory"),
            "refusal must name the shape, got: {err}"
        );
        // The directory must still exist.
        assert!(socket_path.is_dir(), "directory must not be removed");
    }

    #[tokio::test]
    async fn replaces_stale_socket_at_path() {
        // The actual stale-socket case: a leftover socket inode from
        // a previous run (crash, ungraceful shutdown, forgotten
        // manual bind) must be cleared so `bind(2)` can proceed. This
        // is the contract the cleanup path is intended to serve — and
        // now the only shape it accepts.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sock");

        // Pre-bind a socket at the path, then drop it without
        // unlinking, mirroring a crashed previous run that left the
        // socket inode behind.
        {
            let stale = StdUnixDatagram::bind(&socket_path).unwrap();
            drop(stale);
        }
        // Sanity: the socket inode is still there.
        assert!(
            std::fs::symlink_metadata(&socket_path)
                .unwrap()
                .file_type()
                .is_socket(),
            "test setup: stale socket must be present before daemon starts"
        );

        let (handle, sd_tx, _rx) = spawn_with_path(&socket_path);
        // Give the listener a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Path should be a fresh, live socket bound by the daemon.
        let meta = std::fs::symlink_metadata(&socket_path).unwrap();
        assert!(
            meta.file_type().is_socket(),
            "expected socket, got {:?}",
            meta.file_type()
        );
        let _ = sd_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn binds_with_world_writable_permissions() {
        // Documented behaviour: the socket is set to 0o666 after bind
        // so every local process can write (mirrors /dev/log). A
        // regression that dropped the chmod would silently lock out
        // unprivileged senders. The set_permissions call uses warn-
        // -on-error rather than bail, so a missing chmod doesn't
        // even surface as an Err — pin the bits explicitly.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sock");

        let (handle, sd_tx, _rx) = spawn_with_path(&socket_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mode = std::fs::metadata(&socket_path)
            .unwrap()
            .permissions()
            .mode();
        // mode() returns the full st_mode including file-type bits;
        // mask to the permission portion (0o7777).
        assert_eq!(
            mode & 0o7777,
            0o666,
            "expected 0o666, got 0o{:o} (full mode 0o{:o})",
            mode & 0o7777,
            mode
        );
        let _ = sd_tx.send(true);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn receives_valid_syslog_datagram() {
        // End-to-end sanity: a valid `<PRI>` syslog datagram sent to
        // the bound socket arrives as an Event via the mpsc channel
        // with events_received counted exactly once. Guards against
        // a regression in the validate_pri / recv / events_received
        // wiring.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sock");

        let (handle, sd_tx, mut rx) = spawn_with_path(&socket_path);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let sender = StdUnixDatagram::unbound().unwrap();
        sender.send_to(b"<13>test message", &socket_path).unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
            .await
            .expect("recv timed out")
            .expect("channel closed");
        assert_eq!(&event.ingress[..], b"<13>test message");
        let _ = sd_tx.send(true);
        let _ = handle.await;
    }

    // ---- parent-safety validation ----

    /// Predicate scope: the input predicate treats parent write
    /// access as unsafe (group/other write flag `0o022`), but
    /// leaves other-execute alone — `/dev` at `0o755` is the
    /// flagship `/dev/log` deploy shape and must pass.
    #[test]
    #[cfg(unix)]
    fn parent_dir_mode_is_unsafe_for_input_flags_write_only() {
        // Safe: no group/other write bit.
        assert!(!parent_dir_mode_is_unsafe_for_input(0o755)); // `/dev` shape — flagship
        assert!(!parent_dir_mode_is_unsafe_for_input(0o750));
        assert!(!parent_dir_mode_is_unsafe_for_input(0o700));
        assert!(!parent_dir_mode_is_unsafe_for_input(0o711)); // owner + traverse
        // Unsafe: group or world writable.
        assert!(parent_dir_mode_is_unsafe_for_input(0o775)); // group write
        assert!(parent_dir_mode_is_unsafe_for_input(0o757)); // world write
        assert!(parent_dir_mode_is_unsafe_for_input(0o777)); // both
        assert!(parent_dir_mode_is_unsafe_for_input(0o770)); // group rwx
    }

    /// Sticky bit + world-writable (`/tmp` at `0o1777`) is
    /// **not** an escape hatch: sticky prevents non-owner
    /// unlink of files whose owner does not match, but a swap
    /// attack plants an attacker-owned replacement node, so
    /// sticky offers no protection here. `/tmp/foo.sock` is
    /// unsupported for this input.
    #[test]
    #[cfg(unix)]
    fn parent_dir_mode_sticky_world_writable_is_unsafe() {
        // Callers pass the low 9 permission bits, so `/tmp`'s
        // 0o1777 arrives as 0o777 through
        // `validate_unix_socket_input_parent`. Pin the low-bits
        // shape here so a future caller that forgot to mask
        // still trips the predicate.
        assert!(parent_dir_mode_is_unsafe_for_input(0o777));
    }

    #[test]
    #[cfg(unix)]
    fn parent_dir_owner_untrusted_for_input_flags_non_root_non_self() {
        // Root-owned (`/dev` shape) is always trusted.
        assert!(!parent_dir_owner_is_untrusted_for_input(0, 0));
        assert!(!parent_dir_owner_is_untrusted_for_input(0, 1000));
        // Daemon's own euid is trusted (custom deploys, systemd
        // `User=` matching the runtime dir owner).
        assert!(!parent_dir_owner_is_untrusted_for_input(1000, 1000));
        // Any other owner is untrusted independent of mode: rename
        // rights alone let them swap the socket inode.
        assert!(parent_dir_owner_is_untrusted_for_input(1000, 0));
        assert!(parent_dir_owner_is_untrusted_for_input(1001, 1000));
        assert!(parent_dir_owner_is_untrusted_for_input(65534, 1000));
    }

    #[test]
    #[cfg(unix)]
    fn from_properties_bails_on_group_writable_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("group-writable");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o775)).unwrap();
        let socket_path = parent.join("sock");
        let err = validate_unix_socket_input_parent(socket_path.to_str().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("0o775"),
            "diagnostic must name the mode: {msg}"
        );
        assert!(
            msg.contains("refusing to bind"),
            "diagnostic must state the refusal: {msg}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn from_properties_accepts_dev_shaped_parent() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("dev-shaped");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket_path = parent.join("log");
        validate_unix_socket_input_parent(socket_path.to_str().unwrap()).unwrap();
    }

    /// End-to-end: the run() shutdown path records the bound
    /// (dev, ino) at bind time and refuses to unlink the path
    /// if the on-disk node has been swapped out from under us.
    /// The test simulates the swap by rebinding a fresh
    /// standalone datagram socket at the same path before
    /// triggering shutdown; the swap survives (input refuses
    /// to unlink it).
    #[tokio::test]
    #[cfg(unix)]
    async fn shutdown_refuses_to_unlink_swapped_socket() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sock");

        let (handle, sd_tx, _rx) = spawn_with_path(&socket_path);
        // Give the input a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let bound_meta_before = std::fs::symlink_metadata(&socket_path).unwrap();

        // Simulate a swap: unlink and rebind a fresh socket
        // (different inode) at the same path.
        std::fs::remove_file(&socket_path).unwrap();
        let squatter = StdUnixDatagram::bind(&socket_path).unwrap();
        drop(squatter);
        let swapped_meta = std::fs::symlink_metadata(&socket_path).unwrap();
        assert!(
            swapped_meta.ino() != bound_meta_before.ino(),
            "test setup: swap must produce a fresh inode",
        );

        // Trigger shutdown. The input's dev/ino check should
        // observe the inode mismatch and refuse to unlink.
        let _ = sd_tx.send(true);
        let _ = handle.await;

        let post_meta =
            std::fs::symlink_metadata(&socket_path).expect("swapped socket must survive shutdown");
        assert_eq!(
            post_meta.ino(),
            swapped_meta.ino(),
            "swapped inode must not have been unlinked by shutdown",
        );
    }
}
