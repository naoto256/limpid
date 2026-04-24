//! Tail input: follows a log file, emitting each new line as an event.
//!
//! Features:
//! - Follows file appends (poll-based, no inotify dependency)
//! - Detects log rotation (inode change or file truncation)
//! - Persists read position to a state file for restart recovery
//!
//! Properties:
//!   path        "/var/log/auth.log"           — required
//!   state_file  "/var/lib/limpid/tail/auth"   — optional (default: no persistence)
//!   poll_interval "1s"                         — optional (default: 1s)

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader};
use tracing::{debug, error, info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

/// Default poll interval.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Source address used for tail-generated events (no network source).
const TAIL_SOURCE: &str = "127.0.0.1:0";

pub struct TailInput {
    path: PathBuf,
    state_file: Option<PathBuf>,
    poll_interval: Duration,
    metrics: Arc<InputMetrics>,
}

impl Module for TailInput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let path = props::get_string(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("input '{}': tail requires 'path'", name))?;
        let state_file = props::get_string(properties, "state_file").map(PathBuf::from);
        let poll_interval = match props::get_string(properties, "poll_interval") {
            Some(s) => props::parse_duration(&s)?,
            None => DEFAULT_POLL_INTERVAL,
        };
        Ok(Self {
            path: PathBuf::from(path),
            state_file,
            poll_interval,
            metrics: Arc::new(InputMetrics::default()),
        })
    }
}

impl HasMetrics for TailInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Input for TailInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        info!("tail watching {}", self.path.display());

        let source_addr = TAIL_SOURCE.parse().unwrap();

        // Load saved position or start from end of file
        let mut offset = self.load_position().unwrap_or(0);
        let mut last_inode = get_inode(&self.path);

        // If no state file or first run, start from end of file
        if (self.state_file.is_none() || offset == 0)
            && let Ok(meta) = tokio::fs::metadata(&self.path).await
        {
            offset = meta.len();
        }

        loop {
            // Check for shutdown
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("tail {}: shutting down", self.path.display());
                        self.save_position(offset);
                        break;
                    }
                }
                _ = tokio::time::sleep(self.poll_interval) => {}
            }

            // Check if file exists
            let meta = match tokio::fs::metadata(&self.path).await {
                Ok(m) => m,
                Err(_) => {
                    debug!("tail: {} not found, waiting", self.path.display());
                    continue;
                }
            };

            // Detect rotation: inode changed or file truncated
            let current_inode = get_inode(&self.path);
            if current_inode != last_inode {
                info!(
                    "tail {}: rotation detected (inode changed), resetting to beginning",
                    self.path.display()
                );
                offset = 0;
                last_inode = current_inode;
            } else if meta.len() < offset {
                info!(
                    "tail {}: file truncated, resetting to beginning",
                    self.path.display()
                );
                offset = 0;
            }

            // No new data
            if meta.len() <= offset {
                continue;
            }

            // Read new lines
            match self.read_new_lines(offset, &tx, source_addr).await {
                Ok(new_offset) => {
                    offset = new_offset;
                    self.save_position(offset);
                }
                Err(e) => {
                    warn!("tail {}: read error: {}", self.path.display(), e);
                }
            }
        }

        Ok(())
    }
}

impl TailInput {
    async fn read_new_lines(
        &self,
        from_offset: u64,
        tx: &tokio::sync::mpsc::Sender<Event>,
        source_addr: std::net::SocketAddr,
    ) -> Result<u64> {
        let file = tokio::fs::File::open(&self.path)
            .await
            .with_context(|| format!("tail: failed to open {}", self.path.display()))?;
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(from_offset)).await?;

        let mut line = String::new();
        let mut current_offset = from_offset;

        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                break; // EOF
            }

            current_offset += bytes_read as u64;

            // Skip incomplete lines (no trailing newline = still being written)
            if !line.ends_with('\n') {
                current_offset -= bytes_read as u64; // rewind, retry next poll
                break;
            }

            // Trim trailing newline
            let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
            if trimmed.is_empty() {
                continue;
            }

            self.metrics.events_received.fetch_add(1, Ordering::Relaxed);

            let event = Event::new(Bytes::copy_from_slice(trimmed.as_bytes()), source_addr);
            if tx.send(event).await.is_err() {
                break;
            }
        }

        Ok(current_offset)
    }

    fn load_position(&self) -> Option<u64> {
        let state_file = self.state_file.as_ref()?;
        let content = std::fs::read_to_string(state_file).ok()?;
        content.trim().parse().ok()
    }

    fn save_position(&self, offset: u64) {
        if let Some(ref state_file) = self.state_file {
            if let Some(parent) = state_file.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let tmp_path = state_file.with_extension("tmp");
            if let Err(e) = std::fs::write(&tmp_path, offset.to_string())
                .and_then(|_| std::fs::rename(&tmp_path, state_file))
            {
                error!(
                    "tail: failed to save position to {}: {}",
                    state_file.display(),
                    e
                );
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn get_inode(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
}

#[cfg(not(target_os = "linux"))]
fn get_inode(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.ino())
}
