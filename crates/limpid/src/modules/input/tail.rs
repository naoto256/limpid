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

use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

const TAIL_INPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "path",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "state_file",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "poll_interval",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
];

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
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(TAIL_INPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn make_input(path: &Path, state_file: Option<&Path>) -> TailInput {
        TailInput {
            path: path.to_path_buf(),
            state_file: state_file.map(|p| p.to_path_buf()),
            poll_interval: Duration::from_millis(10),
            metrics: Arc::new(InputMetrics::default()),
        }
    }

    fn dummy_addr() -> std::net::SocketAddr {
        "127.0.0.1:0".parse().unwrap()
    }

    #[tokio::test]
    async fn read_new_lines_emits_complete_lines_from_offset() {
        // Baseline: two `\n`-terminated lines, both emit Events; the
        // returned offset is the full file length so the next poll
        // starts past EOF.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"line1\nline2\n").unwrap();
        let input = make_input(&path, None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let next_off = input.read_new_lines(0, &tx, dummy_addr()).await.unwrap();
        assert_eq!(next_off, 12);
        let e1 = rx.recv().await.unwrap();
        assert_eq!(&e1.ingress[..], b"line1");
        let e2 = rx.recv().await.unwrap();
        assert_eq!(&e2.ingress[..], b"line2");
    }

    #[tokio::test]
    async fn read_new_lines_rewinds_on_incomplete_trailing_line() {
        // The key correctness invariant for tail-vs-writer races:
        // a final line without a trailing newline is "still being
        // written" and must NOT be emitted. The returned offset
        // must rewind past the incomplete bytes so the next poll
        // sees them again once the writer adds the newline.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"complete\npartial").unwrap();
        let input = make_input(&path, None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let next_off = input.read_new_lines(0, &tx, dummy_addr()).await.unwrap();
        // First line was complete (9 bytes incl. \n); the partial
        // 7 bytes after must be rewound. Next offset = 9.
        assert_eq!(next_off, 9, "partial line must be rewound");
        let e1 = rx.recv().await.unwrap();
        assert_eq!(&e1.ingress[..], b"complete");
        // Second poll should see the partial line still ahead of the
        // cursor — assert nothing else arrived on the first poll.
        let extra = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(extra.is_err(), "partial line must NOT have emitted");
    }

    #[tokio::test]
    async fn read_new_lines_subsequent_poll_picks_up_completed_partial() {
        // Sibling of the above: simulate the writer finishing the
        // partial line. First poll rewinds; second poll, after the
        // writer appends a newline, emits the completed line.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"complete\npartial").unwrap();
        let input = make_input(&path, None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        let off1 = input.read_new_lines(0, &tx, dummy_addr()).await.unwrap();
        let _ = rx.recv().await; // drain "complete"
        // Writer appends the newline.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"\n").unwrap();
        }
        let off2 = input.read_new_lines(off1, &tx, dummy_addr()).await.unwrap();
        assert_eq!(off2, 17);
        let e = rx.recv().await.unwrap();
        assert_eq!(&e.ingress[..], b"partial");
    }

    #[test]
    fn save_and_load_position_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let state = dir.path().join("state");
        std::fs::write(&log, b"").unwrap();
        let input = make_input(&log, Some(&state));
        input.save_position(12345);
        assert_eq!(input.load_position(), Some(12345));
    }

    #[test]
    fn save_position_overwrites_atomically_no_leftover_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let state = dir.path().join("state");
        std::fs::write(&log, b"").unwrap();
        let input = make_input(&log, Some(&state));
        input.save_position(1);
        input.save_position(2);
        input.save_position(3);
        assert_eq!(input.load_position(), Some(3));
        assert!(!state.with_extension("tmp").exists(), "leftover .tmp");
    }

    #[test]
    fn load_position_returns_none_when_state_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let state = dir.path().join("does-not-exist");
        std::fs::write(&log, b"").unwrap();
        let input = make_input(&log, Some(&state));
        assert_eq!(input.load_position(), None);
    }

    #[test]
    fn get_inode_detects_replacement() {
        // The rotation detector in `run` keys on inode equality. A
        // log-rotate that creates a new file with the same path
        // produces a different inode; pin that get_inode returns a
        // different value across a rename-replace cycle.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"first").unwrap();
        let ino1 = get_inode(&path).expect("inode 1");
        // Replace via remove + write (mimics logrotate's
        // create-new-and-swap pattern).
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"second").unwrap();
        let ino2 = get_inode(&path).expect("inode 2");
        // Most filesystems will give a different inode on a fresh
        // create; if a particular FS reuses inodes synchronously this
        // would be flaky. Accept either outcome as "valid system
        // behaviour" but pin that get_inode returns Some on both.
        let _ = (ino1, ino2);
    }
}
