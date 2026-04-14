//! Disk-persistent queue using a simple WAL (Write-Ahead Log) approach.
//!
//! Design:
//! - Events are serialized to JSON and appended to segment files
//! - Each segment is a file named `seg-{sequence}.wal`
//! - A cursor file (`cursor`) tracks the current read position
//! - Old segments are deleted once fully consumed
//! - Max total size is enforced; oldest unread segments are dropped if exceeded
//!
//! This survives process restarts: on startup, the consumer resumes
//! from the cursor position.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{warn, error, debug};

use crate::event::Event;

const SEGMENT_MAX_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB per segment
const NEWLINE: u8 = b'\n';

/// Shared state for disk queue.
struct DiskQueueState {
    dir: PathBuf,
    max_size: u64,
    /// Current write segment sequence number.
    write_seq: u64,
    /// Current write segment file (append mode).
    write_file: Option<fs::File>,
    /// Current write segment size.
    write_size: u64,
    /// Current read segment sequence (updated by receiver to protect unread segments).
    read_seq: u64,
}

pub struct DiskQueueSender {
    state: Arc<Mutex<DiskQueueState>>,
    notify: Arc<tokio::sync::Notify>,
    closed: Arc<AtomicBool>,
    sender_count: Arc<std::sync::atomic::AtomicUsize>,
}

impl Clone for DiskQueueSender {
    fn clone(&self) -> Self {
        self.sender_count.fetch_add(1, Ordering::AcqRel);
        Self {
            state: Arc::clone(&self.state),
            notify: Arc::clone(&self.notify),
            closed: Arc::clone(&self.closed),
            sender_count: Arc::clone(&self.sender_count),
        }
    }
}

impl Drop for DiskQueueSender {
    fn drop(&mut self) {
        // If this is the last sender (only receiver holds the other Arc),
        // signal closed. We use sender_count (AtomicUsize) for accurate tracking
        // instead of Arc::strong_count which has TOCTOU issues.
        if self.sender_count.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.closed.store(true, Ordering::Release);
            self.notify.notify_one();
        }
    }
}

pub struct DiskQueueReceiver {
    state: Arc<Mutex<DiskQueueState>>,
    notify: Arc<tokio::sync::Notify>,
    closed: Arc<AtomicBool>,
    /// Current read segment sequence.
    read_seq: u64,
    /// Current byte offset within the read segment.
    read_offset: u64,
    dir: PathBuf,
}

pub fn create_disk_queue(path: &str, max_size: u64) -> anyhow::Result<(DiskQueueSender, DiskQueueReceiver)> {
    let dir = PathBuf::from(path);
    fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("failed to create disk queue directory '{}': {}", path, e))?;

    // Find existing segments
    let (write_seq, read_seq, read_offset) = recover_state(&dir);

    let notify = Arc::new(tokio::sync::Notify::new());
    let closed = Arc::new(AtomicBool::new(false));

    let state = Arc::new(Mutex::new(DiskQueueState {
        dir: dir.clone(),
        max_size,
        write_seq,
        write_file: None,
        write_size: 0,
        read_seq,
    }));

    Ok((
        DiskQueueSender {
            state: Arc::clone(&state),
            notify: Arc::clone(&notify),
            closed: Arc::clone(&closed),
            sender_count: Arc::new(std::sync::atomic::AtomicUsize::new(1)),
        },
        DiskQueueReceiver {
            state,
            notify,
            closed,
            read_seq,
            read_offset,
            dir,
        },
    ))
}

impl DiskQueueSender {
    pub async fn send(&self, event: Event) -> bool {
        let serialized = match serde_json::to_string(&event.to_json_value()) {
            Ok(s) => s,
            Err(e) => {
                error!("disk queue: failed to serialize event: {}", e);
                return false;
            }
        };

        // Use spawn_blocking to avoid blocking the tokio worker thread
        let state = Arc::clone(&self.state);
        let result = match tokio::task::spawn_blocking(move || {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            write_to_segment(&mut state, serialized.as_bytes())
        })
        .await
        {
            Ok(ok) => ok,
            Err(e) => {
                error!("disk queue: write task failed: {}", e);
                false
            }
        };

        if result {
            self.notify.notify_one();
        }
        result
    }
}

impl DiskQueueReceiver {
    pub async fn recv(&mut self) -> Option<Event> {
        loop {
            // Register for notification BEFORE checking — prevents missed-wakeup race.
            // Clone the Arc to avoid borrowing self across the await.
            let notify = Arc::clone(&self.notify);
            let notified = notify.notified();
            tokio::pin!(notified);

            if let Some(event) = self.try_read_next() {
                return Some(event);
            }
            if self.closed.load(Ordering::Acquire) {
                return self.try_read_next();
            }
            notified.await;
        }
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        self.try_read_next()
    }

    fn try_read_next(&mut self) -> Option<Event> {
        loop {
        let seg_path = segment_path(&self.dir, self.read_seq);
        if !seg_path.exists() {
            let write_seq = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                state.write_seq
            };
            if self.read_seq < write_seq {
                self.read_seq += 1;
                self.read_offset = 0;
                self.sync_read_seq();
                continue; // loop instead of recurse
            }
            return None;
        }

        let mut file = match fs::File::open(&seg_path) {
            Ok(f) => f,
            Err(_) => return None,
        };

        // Seek to byte offset instead of scanning lines
        use std::io::Seek;
        if self.read_offset > 0
            && file.seek(std::io::SeekFrom::Start(self.read_offset)).is_err() {
                return None;
            }

        let mut reader = BufReader::new(file);
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = match reader.read_line(&mut line) {
                Ok(n) => n,
                Err(_) => break,
            };
            if bytes_read == 0 {
                break; // EOF
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                self.read_offset += bytes_read as u64;
                continue;
            }

            self.read_offset += bytes_read as u64;
            save_cursor(&self.dir, self.read_seq, self.read_offset);

            if let Some(event) = Event::from_json(trimmed) {
                return Some(event);
            }

            warn!(
                "disk queue: skipping corrupted line in segment {} at byte offset {}",
                self.read_seq, self.read_offset
            );
        }

        // Finished this segment — try next
        let write_seq = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            state.write_seq
        };

        if self.read_seq < write_seq {
            // Delete consumed segment
            if let Err(e) = fs::remove_file(&seg_path) {
                warn!("disk queue: failed to remove consumed segment {}: {}", self.read_seq, e);
            } else {
                debug!("disk queue: removed consumed segment {}", self.read_seq);
            }
            self.read_seq += 1;
            self.read_offset = 0;
            self.sync_read_seq();
            save_cursor(&self.dir, self.read_seq, self.read_offset);
            continue; // loop instead of recurse
        }

        return None;
        } // end loop
    }

    /// Update the shared state's read_seq so enforce_max_size can protect unread segments.
    fn sync_read_seq(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.read_seq = self.read_seq;
        }
    }
}

// ---------------------------------------------------------------------------
// Segment I/O
// ---------------------------------------------------------------------------

fn segment_path(dir: &Path, seq: u64) -> PathBuf {
    dir.join(format!("seg-{:08}.wal", seq))
}

fn cursor_path(dir: &Path) -> PathBuf {
    dir.join("cursor")
}

fn write_to_segment(state: &mut DiskQueueState, data: &[u8]) -> bool {
    // Rotate segment if needed
    if state.write_size + data.len() as u64 + 1 > SEGMENT_MAX_BYTES {
        state.write_file = None;
        state.write_seq += 1;
        state.write_size = 0;
    }

    // Open or create segment file
    if state.write_file.is_none() {
        let path = segment_path(&state.dir, state.write_seq);
        match fs::OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                state.write_file = Some(f);
                state.write_size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
            Err(e) => {
                error!("disk queue: failed to open segment: {}", e);
                return false;
            }
        }
    }

    let file = state.write_file.as_mut().unwrap();
    // Combine data + newline into a single write to avoid partial writes
    let mut buf = Vec::with_capacity(data.len() + 1);
    buf.extend_from_slice(data);
    buf.push(NEWLINE);
    if let Err(e) = file.write_all(&buf) {
        error!("disk queue: write failed: {}", e);
        return false;
    }
    if let Err(e) = file.flush() {
        error!("disk queue: flush failed: {}", e);
        return false;
    }
    state.write_size += buf.len() as u64;

    // Enforce max size
    enforce_max_size(&state.dir, state.max_size, state.read_seq, state.write_seq);

    true
}

fn enforce_max_size(dir: &Path, max_size: u64, current_read_seq: u64, _current_write_seq: u64) {
    if max_size == 0 {
        return;
    }

    let mut total: u64 = 0;
    let mut segments: Vec<(u64, u64)> = Vec::new(); // (seq, size)

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("seg-") && name.ends_with(".wal")
                && let Ok(meta) = entry.metadata() {
                    let size = meta.len();
                    let seq_str = &name[4..name.len() - 4];
                    if let Ok(seq) = seq_str.parse::<u64>() {
                        segments.push((seq, size));
                        total += size;
                    }
                }
        }
    }

    if total <= max_size {
        return;
    }

    // Sort by sequence (oldest first) and remove oldest until under limit
    segments.sort_by_key(|&(seq, _)| seq);
    for (seq, size) in segments {
        if total <= max_size {
            break;
        }
        if seq >= current_read_seq {
            break; // don't delete unread or current segments
        }
        let path = segment_path(dir, seq);
        if fs::remove_file(&path).is_ok() {
            warn!("disk queue: removed old segment {} to enforce max size", seq);
            total -= size;
        }
    }
}

// ---------------------------------------------------------------------------
// Recovery
// ---------------------------------------------------------------------------

fn recover_state(dir: &Path) -> (u64, u64, u64) {
    // Find highest segment number
    let mut max_seq = 0u64;
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("seg-") && name.ends_with(".wal") {
                let seq_str = &name[4..name.len() - 4];
                if let Ok(seq) = seq_str.parse::<u64>() {
                    max_seq = max_seq.max(seq);
                }
            }
        }
    }

    // Read cursor
    let (read_seq, read_offset) = load_cursor(dir);

    (max_seq, read_seq, read_offset)
}

fn load_cursor(dir: &Path) -> (u64, u64) {
    let path = cursor_path(dir);
    if let Ok(content) = fs::read_to_string(&path) {
        let parts: Vec<&str> = content.trim().split(':').collect();
        if parts.len() == 2 {
            let seq = parts[0].parse().unwrap_or(0);
            let offset = parts[1].parse().unwrap_or(0);
            return (seq, offset);
        }
    }
    (0, 0)
}

fn save_cursor(dir: &Path, seq: u64, offset: u64) {
    let path = cursor_path(dir);
    let tmp_path = path.with_extension("tmp");
    let data = format!("{}:{}", seq, offset);
    if let Err(e) = fs::write(&tmp_path, &data).and_then(|_| fs::rename(&tmp_path, &path)) {
        error!("disk queue: failed to save cursor: {} — events may be re-delivered on restart", e);
        let _ = fs::remove_file(&tmp_path);
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn make_event(msg: &str) -> Event {
        Event::new(Bytes::from(msg.to_string()), "127.0.0.1:0".parse().unwrap())
    }

    #[test]
    fn test_event_roundtrip() {
        let mut event = make_event("<134>test");
        event.facility = Some(16);
        event.severity = Some(6);
        event.fields.insert("key".into(), serde_json::Value::String("val".into()));

        let json = event.to_json_value();
        let json_str = serde_json::to_string(&json).unwrap();
        let recovered = Event::from_json(&json_str).unwrap();

        assert_eq!(String::from_utf8_lossy(&recovered.raw), "<134>test");
        assert_eq!(recovered.facility, Some(16));
        assert_eq!(recovered.severity, Some(6));
        assert_eq!(recovered.fields["key"], serde_json::Value::String("val".into()));
    }

    #[tokio::test]
    async fn test_disk_queue_basic() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = create_disk_queue(dir.path().to_str().unwrap(), 0).unwrap();

        tx.send(make_event("<134>msg1")).await;
        tx.send(make_event("<134>msg2")).await;

        let e1 = rx.recv().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&e1.raw), "<134>msg1");

        let e2 = rx.recv().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&e2.raw), "<134>msg2");
    }

    #[test]
    fn test_disk_queue_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        // Write some events
        {
            let (tx, _rx) = create_disk_queue(path, 0).unwrap();
            // Use blocking send via try approach
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                tx.send(make_event("<134>persist1")).await;
                tx.send(make_event("<134>persist2")).await;
            });
        }

        // Re-open and read
        {
            let (_tx, mut rx) = create_disk_queue(path, 0).unwrap();
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let e1 = rx.recv().await.unwrap();
                assert_eq!(String::from_utf8_lossy(&e1.raw), "<134>persist1");
                let e2 = rx.recv().await.unwrap();
                assert_eq!(String::from_utf8_lossy(&e2.raw), "<134>persist2");
            });
        }
    }
}
