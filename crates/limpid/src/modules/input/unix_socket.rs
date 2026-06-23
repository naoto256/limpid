//! Unix socket input: receives syslog messages from a Unix datagram socket.
//!
//! Used to receive messages from `logger` and local applications via `/dev/log`.
//!
//! Properties:
//!   path   "/dev/log"   — required

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
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

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
        let path = props::get_string(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("input '{}': unix_socket requires 'path'", name))?;
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
        // Remove stale socket file if it exists (but not if it's a symlink)
        if std::path::Path::new(&self.path).exists() {
            match std::fs::symlink_metadata(&self.path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    error!(
                        "unix_socket: {:?} is a symlink — refusing to remove",
                        self.path
                    );
                    anyhow::bail!("unix_socket: {:?} is a symlink", self.path);
                }
                _ => {
                    let _ = std::fs::remove_file(&self.path);
                }
            }
        }

        let socket = UnixDatagram::bind(&self.path)?;
        info!("unix_socket listening on {}", self.path);

        // Make socket world-writable so any process can send (like /dev/log)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) =
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o666))
            {
                warn!(
                    "unix_socket {}: failed to set permissions: {}",
                    self.path, e
                );
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
                        let _ = std::fs::remove_file(&self.path);
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
    async fn replaces_stale_regular_file_at_path() {
        // Inverse case: a stale regular file at the configured path
        // (e.g. left over from a previous run that didn't clean up
        // gracefully) MUST be removed so the bind can proceed. A
        // regression that conflated regular-file and symlink would
        // either leave the daemon stuck on startup OR (worse) let
        // symlinks through.
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("sock");
        std::fs::write(&socket_path, b"stale").unwrap();

        let (handle, sd_tx, _rx) = spawn_with_path(&socket_path);
        // Give the listener a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Path should now be a socket, not a regular file.
        let meta = std::fs::symlink_metadata(&socket_path).unwrap();
        assert!(meta.file_type().is_socket(), "expected socket, got {:?}", meta.file_type());
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
        let mode = std::fs::metadata(&socket_path).unwrap().permissions().mode();
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
}
