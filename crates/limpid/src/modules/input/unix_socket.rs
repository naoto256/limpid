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
use tracing::{info, warn, error};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{FromProperties, HasMetrics, Input};
use super::validate::validate_pri;

const UNIX_SOURCE: &str = "127.0.0.1:0";

pub struct UnixSocketInput {
    path: String,
    metrics: Arc<InputMetrics>,
}

impl FromProperties for UnixSocketInput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
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
    fn metrics(&self) -> Arc<InputMetrics> { Arc::clone(&self.metrics) }
}

#[async_trait::async_trait]
impl Input for UnixSocketInput {
    async fn run(self, tx: tokio::sync::mpsc::Sender<Event>, mut shutdown: tokio::sync::watch::Receiver<bool>) -> Result<()> {
        // Remove stale socket file if it exists (but not if it's a symlink)
        if std::path::Path::new(&self.path).exists() {
            match std::fs::symlink_metadata(&self.path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    error!("unix_socket: {:?} is a symlink — refusing to remove", self.path);
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
            if let Err(e) = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o666)) {
                warn!("unix_socket {}: failed to set permissions: {}", self.path, e);
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
