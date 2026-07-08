//! Disk-persistent queue using a simple WAL (Write-Ahead Log) approach.
//!
//! Design:
//! - Events are serialized to JSON and appended to segment files
//! - Each segment is a file named `seg-{sequence}.wal`
//! - A cursor file (`cursor`) persists the acked position — the last
//!   byte the consumer explicitly acknowledged, not the in-flight read
//!   position. Restart replays from this cursor for at-least-once.
//! - Segments below the acked position are deleted once `ack_to`
//!   advances through them; unread segments are never deleted.
//! - Max total size is enforced by dropping the oldest acked
//!   segments (= those below the acked cursor); segments at or above
//!   the acked cursor are protected (they may still hold in-flight
//!   events whose ack handles have not resolved), so an undersized
//!   `max_size` cannot silently drop pending events.
//!
//! This survives process restarts: on startup, the consumer resumes
//! from the acked cursor position.

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, error, warn};

use crate::event::Event;
use crate::queue::AckPosition;

const SEGMENT_MAX_BYTES: u64 = 16 * 1024 * 1024; // 16 MiB per segment
const NEWLINE: u8 = b'\n';

/// One in-flight (read but not yet acked) event tracked by
/// [`DiskQueueReceiver`]. `start_*` is the position the
/// [`AckPosition::Disk`] handle quotes; `end_*` is the position the
/// persisted cursor lands on once this event reaches the front of the
/// contiguous acked prefix and is popped.
#[derive(Debug, Clone, Copy)]
struct InFlight {
    start_seq: u64,
    start_offset: u64,
    end_seq: u64,
    end_offset: u64,
    acked: bool,
}

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
    /// Current read segment sequence (updated by receiver). Tracked
    /// for observability; the GC boundary is `acked_seq` below — the
    /// reader can advance `read_seq` past events that have not yet
    /// been acked, so `read_seq` alone would let `enforce_max_size`
    /// delete a segment whose events are still in-flight.
    read_seq: u64,
    /// Last acked segment sequence (updated by the receiver after
    /// `ack_to` advances its acked cursor). This is the protective
    /// boundary for `enforce_max_size`: segments below `acked_seq`
    /// have no in-flight handles and are safe to delete; segments at
    /// or above `acked_seq` may still hold unacked events that the
    /// at-least-once contract guarantees to replay on restart.
    acked_seq: u64,
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
    /// In-memory read cursor — the next byte to read. Advances as
    /// `recv()` returns events; NOT persisted on disk between recv
    /// and ack. A crash between recv and ack rolls back to the
    /// acked cursor below, so the unacked event replays on restart
    /// (at-least-once contract — matches Kafka/RabbitMQ).
    read_seq: u64,
    read_offset: u64,
    /// Persisted cursor — the last byte the consumer has explicitly
    /// acknowledged as processed. `save_cursor` is only called after
    /// `ack_to` advances this; segment files below `acked_seq` are
    /// also deleted by `ack_to`. An earlier version saved the read
    /// cursor immediately on each `recv()`, which made the disk
    /// queue at-most-once: a crash mid-write lost the in-flight
    /// event because the cursor said it had been consumed.
    acked_seq: u64,
    acked_offset: u64,
    /// In-flight position tracker. Each entry carries the event's
    /// `(start_seq, start_offset)` — the position the
    /// [`AckPosition::Disk`] handle quotes back to identify *which*
    /// event is being acked — alongside the `(end_seq, end_offset)`
    /// the persisted cursor must land on once that event is done
    /// (= the position of the NEXT event after it). Cursor only
    /// advances through the contiguous `acked=true` prefix popped
    /// from the front, so an out-of-order ack (= batched output
    /// resolving handles in flush order, not in receive order) holds
    /// the cursor back until the earlier in-flight events also
    /// resolve. Pre-fix this state did not exist; ack saved
    /// `self.read_seq` / `self.read_offset` — the position of the
    /// NEXT event to read — and any single ack from anywhere in the
    /// in-flight batch silently advanced past every still-pending
    /// event.
    in_flight_positions: VecDeque<InFlight>,
    dir: PathBuf,
}

pub fn create_disk_queue(
    path: &str,
    max_size: u64,
) -> anyhow::Result<(DiskQueueSender, DiskQueueReceiver)> {
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
        // Boot with acked_seq == read_seq: anything at recover time is
        // either already acked (cursor file points past it) or never
        // existed. Receiver's `ack_to` will advance this as the new
        // session progresses.
        acked_seq: read_seq,
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
            // Boot up with acked == read: anything before
            // `read_offset` was acked by a previous run (that's
            // what `recover_state` returned).
            acked_seq: read_seq,
            acked_offset: read_offset,
            in_flight_positions: VecDeque::new(),
            dir,
        },
    ))
}

impl DiskQueueSender {
    pub async fn send(&self, event: Event) -> Result<(), super::QueueSendError> {
        let serialized = match serde_json::to_string(&event.to_json_value()) {
            Ok(s) => s,
            Err(e) => {
                error!("disk queue: failed to serialize event: {}", e);
                return Err(super::QueueSendError::Serialize(e));
            }
        };

        // Use spawn_blocking to avoid blocking the tokio worker thread
        let state = Arc::clone(&self.state);
        let wrote = match tokio::task::spawn_blocking(move || {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            write_to_segment(&mut state, serialized.as_bytes())
        })
        .await
        {
            Ok(ok) => ok,
            Err(e) => {
                error!("disk queue: write task failed: {}", e);
                return Err(super::QueueSendError::JoinError(e));
            }
        };

        if wrote {
            self.notify.notify_one();
            Ok(())
        } else {
            // `write_to_segment` already logged the specific I/O
            // error (open/append/flush). Surface a single
            // `DiskWrite` outcome to the caller.
            Err(super::QueueSendError::DiskWrite)
        }
    }
}

impl DiskQueueReceiver {
    pub async fn recv(&mut self) -> Option<(Event, AckPosition)> {
        loop {
            // Register for notification BEFORE checking — prevents missed-wakeup race.
            // Clone the Arc to avoid borrowing self across the await.
            let notify = Arc::clone(&self.notify);
            let notified = notify.notified();
            tokio::pin!(notified);

            if let Some(pair) = self.try_read_next() {
                return Some(pair);
            }
            if self.closed.load(Ordering::Acquire) {
                return self.try_read_next();
            }
            notified.await;
        }
    }

    /// Non-blocking read of the next available event. Returns `None`
    /// on empty (regardless of whether the queue has been closed) —
    /// closure observation is left to a subsequent `recv().await`.
    /// Used as the greedy-drain step inside `QueueReceiver::recv_many`.
    pub fn try_recv(&mut self) -> Option<(Event, AckPosition)> {
        self.try_read_next()
    }

    /// Commit a specific event's position to the in-flight tracker
    /// and, if it completes the contiguous acked prefix from the
    /// front, advance the persisted cursor through that prefix and
    /// reclaim fully-consumed segments.
    ///
    /// Pre-fix the receiver had `ack(&mut self)` that just saved
    /// `self.read_seq` / `self.read_offset` — but those fields point
    /// at the NEXT event to read, not the event being acked. With
    /// batched outputs holding multiple `(Event, QueueAckHandle)`
    /// pairs in flight, a single event's ack would advance the
    /// persisted cursor past every still-in-flight event in the
    /// buffer; a crash before flush silently lost the rest, defeating
    /// the at-least-once contract. Position is now captured at recv
    /// time and threaded through the handle, so ack_to commits the
    /// right position regardless of resolve order.
    pub fn ack_to(&mut self, seq: u64, offset: u64) {
        // Look up the position in the in-flight queue and mark it
        // acked. A miss means the consumer fed us a position we never
        // handed out (or one we have already advanced past) — both
        // shapes are bugs; warn and refuse to advance the cursor so
        // we cannot accidentally widen a cursor jump on bad input.
        let mut found = false;
        for entry in self.in_flight_positions.iter_mut() {
            if entry.start_seq == seq && entry.start_offset == offset {
                entry.acked = true;
                found = true;
                break;
            }
        }
        if !found {
            warn!(
                "disk queue: ack_to for unknown position seq={} offset={} \
                 (already advanced past, or never issued); cursor unchanged",
                seq, offset,
            );
            return;
        }

        // Pop the contiguous acked prefix from the front; the last
        // entry popped wins the cursor advance — and we land on its
        // *end* position (= the next byte to read after that event),
        // not its start position. Landing on the start would replay
        // the acked event on next open.
        let mut advanced = None;
        while let Some(front) = self.in_flight_positions.front() {
            if !front.acked {
                break;
            }
            let popped = self.in_flight_positions.pop_front().unwrap();
            advanced = Some((popped.end_seq, popped.end_offset));
        }

        let Some((new_seq, new_offset)) = advanced else {
            // The acked position was not at the front and no
            // contiguous prefix completed; cursor stays put until
            // the earlier in-flight entries also ack.
            return;
        };

        if new_seq == self.acked_seq && new_offset == self.acked_offset {
            // Idempotent — defensive no-op (matches the pre-fix
            // contract for drain-loop polling against the empty tail).
            return;
        }

        // Delete every fully-consumed segment whose seq is strictly
        // below the new acked seq. We held them through recv() so
        // crash-replay could find the in-flight event; once ack
        // advances past them they're permanently consumed.
        for stale_seq in self.acked_seq..new_seq {
            let seg_path = segment_path(&self.dir, stale_seq);
            if let Err(e) = fs::remove_file(&seg_path) {
                // Missing file is acceptable (segment may have been
                // GC'd already); other errors are operator-visible
                // but non-fatal — leaving the segment around just
                // wastes disk, not correctness.
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        "disk queue: failed to remove consumed segment {}: {}",
                        stale_seq, e
                    );
                } else {
                    debug!("disk queue: removed consumed segment {}", stale_seq);
                }
            } else {
                debug!("disk queue: removed consumed segment {}", stale_seq);
            }
        }

        self.acked_seq = new_seq;
        self.acked_offset = new_offset;
        save_cursor(&self.dir, self.acked_seq, self.acked_offset);
        // Propagate to shared state so `enforce_max_size` honours the
        // at-least-once boundary on the next write-side GC pass.
        self.sync_acked_seq();
    }

    fn try_read_next(&mut self) -> Option<(Event, AckPosition)> {
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
                && file
                    .seek(std::io::SeekFrom::Start(self.read_offset))
                    .is_err()
            {
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

                // Capture the position of THIS event (= the position
                // BEFORE we advance past it) so the ack handle can be
                // matched back to the contiguous-prefix tracker.
                // Pre-fix the receiver acked using `read_seq` /
                // `read_offset` AFTER the advance, which named the
                // NEXT event's position, not this one's — and under
                // batched outputs that mismatch silently advanced the
                // cursor past in-flight events.
                let start_seq = self.read_seq;
                let start_offset = self.read_offset;
                self.read_offset += bytes_read as u64;
                let end_seq = self.read_seq;
                let end_offset = self.read_offset;
                // Do NOT save_cursor here. The cursor on disk only
                // advances when the consumer calls `ack_to`, after
                // the event has been successfully handed off
                // downstream. Returning here with the in-memory
                // `read_offset` already moved gives the recv side an
                // in-flight position; if the process crashes before
                // ack, restart re-reads from `acked_offset` and
                // replays this event.

                if let Some(event) = Event::from_json(trimmed) {
                    self.in_flight_positions.push_back(InFlight {
                        start_seq,
                        start_offset,
                        end_seq,
                        end_offset,
                        acked: false,
                    });
                    return Some((
                        event,
                        AckPosition::Disk {
                            seq: start_seq,
                            offset: start_offset,
                        },
                    ));
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
                // Advance the in-memory read cursor to the next
                // segment, but do NOT delete the current one or save
                // the cursor yet — both are tied to `ack_to`. The
                // current segment may still hold the in-flight event
                // (the one most recently returned by `recv()` but not
                // yet acked); deleting it now would lose that event
                // on crash-and-replay.
                self.read_seq += 1;
                self.read_offset = 0;
                self.sync_read_seq();
                continue; // loop instead of recurse
            }

            return None;
        } // end loop
    }

    /// Sync the receiver's read cursor to shared state for
    /// observability. The GC boundary in `enforce_max_size` is
    /// `acked_seq`, not `read_seq` — see `sync_acked_seq` and the
    /// `DiskQueueState::acked_seq` doc.
    fn sync_read_seq(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.read_seq = self.read_seq;
        }
    }

    /// Sync the receiver's acked cursor to shared state. Called
    /// after `ack_to` advances `self.acked_seq` so the next
    /// `enforce_max_size` invocation (from `write_to_segment`)
    /// honours the at-least-once boundary: segments at or above
    /// `acked_seq` may still hold in-flight events that must replay
    /// on restart.
    fn sync_acked_seq(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.acked_seq = self.acked_seq;
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

    // Enforce max size. The protective boundary is `acked_seq` —
    // segments below it have no in-flight unacked events and are
    // safe to delete. Using `read_seq` would let a slow consumer's
    // unacked segment vanish: `read_seq` advances as `recv()` moves
    // past events that have not yet been acked.
    enforce_max_size(&state.dir, state.max_size, state.acked_seq, state.write_seq);

    true
}

fn enforce_max_size(dir: &Path, max_size: u64, current_acked_seq: u64, _current_write_seq: u64) {
    if max_size == 0 {
        return;
    }

    let mut total: u64 = 0;
    let mut segments: Vec<(u64, u64)> = Vec::new(); // (seq, size)

    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("seg-")
                && name.ends_with(".wal")
                && let Ok(meta) = entry.metadata()
            {
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
        if seq >= current_acked_seq {
            break; // don't delete segments that may still hold unacked events
        }
        let path = segment_path(dir, seq);
        if fs::remove_file(&path).is_ok() {
            warn!(
                "disk queue: removed old segment {} to enforce max size",
                seq
            );
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
        error!(
            "disk queue: failed to save cursor: {} — events may be re-delivered on restart",
            e
        );
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
        event
            .workspace
            .insert("key".into(), crate::dsl::OwnedValue::String("val".into()));

        let json = event.to_json_value();
        let json_str = serde_json::to_string(&json).unwrap();
        let recovered = Event::from_json(&json_str).unwrap();

        assert_eq!(String::from_utf8_lossy(&recovered.ingress), "<134>test");
        assert_eq!(
            recovered.workspace["key"],
            crate::dsl::OwnedValue::String("val".into())
        );
    }

    #[test]
    fn source_with_nondefault_port_round_trips() {
        // Source IP+port must survive `to_json_value` → `from_json`
        // intact; otherwise compose_replayable / DLQ replay loses the
        // port discriminator that distinguishes co-located originators
        // (multi-tenant: same host, different bind ports).
        let event = Event::new(
            Bytes::from_static(b"<134>test"),
            "192.0.2.10:5140".parse().unwrap(),
        );
        let json_str = serde_json::to_string(&event.to_json_value()).unwrap();
        // Wire form is `{ip, port}` since v0.5.6.
        assert!(
            json_str.contains(r#""source":{"ip":"192.0.2.10","port":5140}"#),
            "expected v0.5.6+ object form, got: {}",
            json_str
        );
        let recovered = Event::from_json(&json_str).unwrap();
        assert_eq!(recovered.source.ip().to_string(), "192.0.2.10");
        assert_eq!(recovered.source.port(), 5140);
    }

    #[test]
    fn from_json_rejects_legacy_string_source() {
        // The 0.5.5 flat-string form is intentionally not accepted —
        // breaking change documented in CHANGELOG. Operators with old
        // captures must `jq` migrate before replay.
        let json_str = r#"{"received_at":1234,"source":"192.0.2.10:5140","ingress":"x"}"#;
        assert!(Event::from_json(json_str).is_none());
    }

    #[tokio::test]
    async fn test_disk_queue_basic() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = create_disk_queue(dir.path().to_str().unwrap(), 0).unwrap();

        tx.send(make_event("<134>msg1")).await.unwrap();
        tx.send(make_event("<134>msg2")).await.unwrap();

        let (e1, _p1) = rx.recv().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&e1.ingress), "<134>msg1");

        let (e2, _p2) = rx.recv().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&e2.ingress), "<134>msg2");
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
                tx.send(make_event("<134>persist1")).await.unwrap();
                tx.send(make_event("<134>persist2")).await.unwrap();
            });
        }

        // Re-open and read
        {
            let (_tx, mut rx) = create_disk_queue(path, 0).unwrap();
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (e1, _) = rx.recv().await.unwrap();
                assert_eq!(String::from_utf8_lossy(&e1.ingress), "<134>persist1");
                let (e2, _) = rx.recv().await.unwrap();
                assert_eq!(String::from_utf8_lossy(&e2.ingress), "<134>persist2");
            });
        }
    }

    // ---------- enforce_max_size unread-protection invariant ----------

    fn make_segment(dir: &Path, seq: u64, byte_len: usize) {
        let path = segment_path(dir, seq);
        std::fs::write(&path, vec![b'x'; byte_len]).unwrap();
    }

    fn segment_exists(dir: &Path, seq: u64) -> bool {
        segment_path(dir, seq).exists()
    }

    #[test]
    fn enforce_max_size_deletes_oldest_consumed_segments_until_under_cap() {
        // 5 segments seq=0..4, each 1024 bytes; cap = 2048, acked
        // cursor at seq=4 (everything below has been acked). Caller
        // must delete seq=0, 1, 2 to fit; seq=3 might or might not be
        // removed depending on order, but seq=4 (== current_acked_seq)
        // must stay because it may still contain data at or after
        // the persisted acked cursor.
        let dir = tempfile::tempdir().unwrap();
        for s in 0..5u64 {
            make_segment(dir.path(), s, 1024);
        }
        enforce_max_size(dir.path(), 2048, /* current_acked_seq */ 4, 4);
        // seq < 4 may be deleted; seq=4 MUST stay (boundary segment).
        assert!(
            segment_exists(dir.path(), 4),
            "acked-cursor boundary segment must not be deleted"
        );
    }

    #[test]
    fn enforce_max_size_never_deletes_unacked_segments_even_when_over_cap() {
        // 4 segments seq=0..3, each 1 MiB. Cap = 100 KiB, acked
        // cursor is at seq=0 (nothing has been acked yet). The
        // boundary `seq >= current_acked_seq` forbids the function
        // from deleting any segment that may still hold in-flight
        // or unread events. This is the at-least-once invariant: an
        // operator setting a too-small max_size must not silently
        // lose events whose ack handles have not resolved.
        let dir = tempfile::tempdir().unwrap();
        for s in 0..4u64 {
            make_segment(dir.path(), s, 1024 * 1024);
        }
        enforce_max_size(dir.path(), 100 * 1024, /* current_acked_seq */ 0, 3);
        // All 4 segments must still exist: nothing is below
        // acked_seq, so NONE are deletable.
        for s in 0..4u64 {
            assert!(
                segment_exists(dir.path(), s),
                "seq={s} was deleted but is at or past the acked cursor"
            );
        }
    }

    #[test]
    fn enforce_max_size_protects_read_but_unacked_segments() {
        // Regression for the at-least-once boundary: an event may be
        // `recv()`-ed (advances `read_seq`) but not yet acked. The
        // GC boundary must use `acked_seq`, not `read_seq` — using
        // `read_seq` would let `enforce_max_size` delete a segment
        // whose events are still in-flight under the contract, and
        // the daemon would silently lose them on the next restart.
        //
        // 4 segments seq=0..3, each 1 MiB. Cap = 100 KiB. Reader has
        // advanced to seq=3 (read everything), but acked_seq is
        // still 1 (only segment 0 fully resolved). seq=1..2 must be
        // protected even though the read cursor is past them.
        let dir = tempfile::tempdir().unwrap();
        for s in 0..4u64 {
            make_segment(dir.path(), s, 1024 * 1024);
        }
        enforce_max_size(dir.path(), 100 * 1024, /* current_acked_seq */ 1, 3);
        // seq=0 < acked_seq=1 → may be deleted to free space.
        assert!(
            !segment_exists(dir.path(), 0),
            "seq=0 is below acked_seq, must be deletable to enforce the cap"
        );
        // seq=1..3 >= acked_seq=1 → MUST be preserved.
        for s in 1..4u64 {
            assert!(
                segment_exists(dir.path(), s),
                "seq={s} is at or above acked_seq=1; in-flight events must replay on restart"
            );
        }
    }

    #[test]
    fn enforce_max_size_no_op_when_total_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        for s in 0..3u64 {
            make_segment(dir.path(), s, 1024);
        }
        enforce_max_size(dir.path(), 1024 * 1024, 0, 2);
        for s in 0..3u64 {
            assert!(segment_exists(dir.path(), s));
        }
    }

    #[test]
    fn enforce_max_size_zero_cap_disables_enforcement() {
        // max_size = 0 is the "no cap" sentinel; enforce_max_size
        // must early-return without scanning the dir. Documented via
        // the early-return branch; pin it so a refactor that flips
        // the predicate doesn't accidentally turn this into "cap at
        // 0 bytes → delete everything".
        let dir = tempfile::tempdir().unwrap();
        for s in 0..3u64 {
            make_segment(dir.path(), s, 1024 * 1024);
        }
        enforce_max_size(dir.path(), 0, 1, 2);
        for s in 0..3u64 {
            assert!(segment_exists(dir.path(), s));
        }
    }

    // ---------- save_cursor / load_cursor ----------

    #[test]
    fn save_cursor_round_trips_through_load_cursor() {
        let dir = tempfile::tempdir().unwrap();
        save_cursor(dir.path(), 42, 12345);
        let (seq, off) = load_cursor(dir.path());
        assert_eq!((seq, off), (42, 12345));
    }

    #[test]
    fn save_cursor_overwrites_previous_value_atomically() {
        // The save_cursor path is "write tmp, rename atomically". A
        // regression that, e.g., writes directly to the cursor file
        // would leave the file truncated mid-write on a crash. We
        // can't simulate a crash easily, but we can verify that
        // repeated overwrites land cleanly and that no leftover .tmp
        // remains after a successful save.
        let dir = tempfile::tempdir().unwrap();
        save_cursor(dir.path(), 1, 10);
        save_cursor(dir.path(), 2, 20);
        save_cursor(dir.path(), 3, 30);
        let (seq, off) = load_cursor(dir.path());
        assert_eq!((seq, off), (3, 30));
        let tmp = cursor_path(dir.path()).with_extension("tmp");
        assert!(
            !tmp.exists(),
            "leftover .tmp after successful save: {}",
            tmp.display()
        );
    }

    #[test]
    fn load_cursor_returns_zero_zero_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let (seq, off) = load_cursor(dir.path());
        assert_eq!((seq, off), (0, 0));
    }

    #[test]
    fn load_cursor_returns_zero_zero_for_malformed_file() {
        // A cursor file from a future format, or one that got
        // truncated, must NOT panic. The function falls back to (0,0)
        // — pessimistic but safe (= replays everything since the
        // start of the available segments).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(cursor_path(dir.path()), "not-a-cursor").unwrap();
        let (seq, off) = load_cursor(dir.path());
        assert_eq!((seq, off), (0, 0));
    }

    // ---------- ack / replay invariants ----------

    #[tokio::test]
    async fn unacked_recv_replays_on_reopen() {
        // Regression: an earlier disk-queue version saved the cursor
        // inside `recv()` immediately on each event, so the queue
        // claimed events as consumed before the consumer had a chance
        // to ship them downstream. A crash between recv and write
        // lost the event because the on-disk cursor was already past
        // it. Cursor persistence is now deferred to `ack_to`, so the
        // un-acked event is replayed on the next open.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        {
            let (tx, mut rx) = create_disk_queue(path, 0).unwrap();
            tx.send(make_event("<134>e1")).await.unwrap();
            tx.send(make_event("<134>e2")).await.unwrap();
            // Receive e1 but don't ack — simulates a process that
            // started shipping it and crashed before completing.
            let (e1, _) = rx.recv().await.unwrap();
            assert_eq!(String::from_utf8_lossy(&e1.ingress), "<134>e1");
            // rx is dropped here without `ack_to`.
        }

        {
            let (_tx, mut rx) = create_disk_queue(path, 0).unwrap();
            let (e1, _) = rx.recv().await.unwrap();
            assert_eq!(
                String::from_utf8_lossy(&e1.ingress),
                "<134>e1",
                "unacked event must replay on reopen (at-least-once contract)",
            );
            // And e2 follows in order.
            let (e2, _) = rx.recv().await.unwrap();
            assert_eq!(String::from_utf8_lossy(&e2.ingress), "<134>e2");
        }
    }

    fn unwrap_disk_position(p: AckPosition) -> (u64, u64) {
        match p {
            AckPosition::Disk { seq, offset } => (seq, offset),
            AckPosition::Memory => panic!("expected Disk position, got Memory"),
        }
    }

    #[tokio::test]
    async fn ack_persists_cursor_so_acked_event_does_not_replay() {
        // Baseline of the contract above: once ack runs, the
        // persisted cursor is past the acked event, so reopen
        // resumes from the next un-acked event.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        {
            let (tx, mut rx) = create_disk_queue(path, 0).unwrap();
            tx.send(make_event("<134>e1")).await.unwrap();
            tx.send(make_event("<134>e2")).await.unwrap();
            let (e1, p1) = rx.recv().await.unwrap();
            assert_eq!(String::from_utf8_lossy(&e1.ingress), "<134>e1");
            let (seq, off) = unwrap_disk_position(p1);
            rx.ack_to(seq, off);
            // Now drop without acking e2.
        }

        {
            let (_tx, mut rx) = create_disk_queue(path, 0).unwrap();
            // Cursor is past e1, so the next recv starts at e2.
            let (next, _) = rx.recv().await.unwrap();
            assert_eq!(
                String::from_utf8_lossy(&next.ingress),
                "<134>e2",
                "ack must persist the cursor; acked event must not replay",
            );
        }
    }

    #[tokio::test]
    async fn ack_is_idempotent_when_nothing_new_to_commit() {
        // ack_to is a no-op when acked == read. The drain loop and
        // any defensive consumer-side double-ack should incur no
        // disk write and no error.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let (tx, mut rx) = create_disk_queue(path, 0).unwrap();
        tx.send(make_event("<134>only")).await.unwrap();
        let (_e, p) = rx.recv().await.unwrap();
        let (seq, off) = unwrap_disk_position(p);
        rx.ack_to(seq, off);
        // Second ack with the same position is unknown (already
        // popped from in_flight), warns, and does not advance.
        rx.ack_to(seq, off);
        rx.ack_to(seq, off);
    }

    // ---------- positional ack regression tests ----------

    #[tokio::test]
    async fn disk_ack_out_of_order_advances_only_contiguous_prefix() {
        // Receive A, B, C, then ack C, then B, then A.
        // Cursor must stay put after C-ack and B-ack (front not yet
        // acked), then jump straight past C once A — the front — is
        // acked. This is the regression: pre-fix any single ack
        // would have saved `read_seq`/`read_offset`, which after
        // three recvs already pointed past C.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        let (tx, mut rx) = create_disk_queue(path, 0).unwrap();
        tx.send(make_event("<134>A")).await.unwrap();
        tx.send(make_event("<134>B")).await.unwrap();
        tx.send(make_event("<134>C")).await.unwrap();

        let (_a, pa) = rx.recv().await.unwrap();
        let (_b, pb) = rx.recv().await.unwrap();
        let (_c, pc) = rx.recv().await.unwrap();

        let (a_seq, a_off) = unwrap_disk_position(pa);
        let (b_seq, b_off) = unwrap_disk_position(pb);
        let (c_seq, c_off) = unwrap_disk_position(pc);

        // Snapshot the end position of C (= the read cursor after
        // recv returned C) so we can compare the persisted cursor
        // after A is acked.
        let c_end_seq = rx.read_seq;
        let c_end_offset = rx.read_offset;

        // ack C → cursor stays at boot value (front is A, not acked).
        rx.ack_to(c_seq, c_off);
        let (seq, off) = load_cursor(dir.path());
        assert_eq!(
            (seq, off),
            (0, 0),
            "ack C with A,B still in flight must not advance cursor",
        );

        // ack B → still no advance.
        rx.ack_to(b_seq, b_off);
        let (seq, off) = load_cursor(dir.path());
        assert_eq!(
            (seq, off),
            (0, 0),
            "ack B with A still in flight must not advance cursor",
        );

        // ack A → cursor jumps past C (the end of the contiguous
        // acked prefix), so a reopen would resume past C.
        rx.ack_to(a_seq, a_off);
        let (seq, off) = load_cursor(dir.path());
        assert_eq!(
            (seq, off),
            (c_end_seq, c_end_offset),
            "front ack must advance through the full contiguous prefix",
        );
        assert!(rx.in_flight_positions.is_empty());
    }

    #[tokio::test]
    async fn disk_ack_in_order_advances_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let (tx, mut rx) = create_disk_queue(path, 0).unwrap();

        for i in 0..3 {
            tx.send(make_event(&format!("<134>e{i}"))).await.unwrap();
        }

        for _ in 0..3 {
            let (_e, p) = rx.recv().await.unwrap();
            let (seq, off) = unwrap_disk_position(p);
            // Cursor must land on the END of the just-acked event
            // (= where the next read would resume), not its start.
            let expected_end = (rx.read_seq, rx.read_offset);
            rx.ack_to(seq, off);
            let (cseq, coff) = load_cursor(dir.path());
            assert_eq!((cseq, coff), expected_end);
        }
        assert!(rx.in_flight_positions.is_empty());
    }

    #[tokio::test]
    async fn pending_acks_bounded_under_sustained_load() {
        // After every event is acked, the in-flight tracker must be
        // empty — a leak there would grow O(N) with sustained load.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let (tx, mut rx) = create_disk_queue(path, 0).unwrap();

        for i in 0..1000 {
            tx.send(make_event(&format!("<134>e{i}"))).await.unwrap();
        }
        let mut positions = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let (_e, p) = rx.recv().await.unwrap();
            positions.push(unwrap_disk_position(p));
        }
        // Ack in receive order — the simplest fast path.
        for (seq, off) in positions {
            rx.ack_to(seq, off);
        }
        assert!(
            rx.in_flight_positions.is_empty(),
            "in_flight_positions leaked: {} entries left",
            rx.in_flight_positions.len(),
        );
    }

    #[tokio::test]
    async fn ack_for_unknown_position_warns_not_advances() {
        // A position the receiver never handed out (or already
        // popped) must NOT advance the cursor — it's a no-op + warn.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        let (tx, mut rx) = create_disk_queue(path, 0).unwrap();

        tx.send(make_event("<134>e1")).await.unwrap();
        let (_e, p) = rx.recv().await.unwrap();
        let (real_seq, real_off) = unwrap_disk_position(p);
        let expected_end = (rx.read_seq, rx.read_offset);

        // Ack a position no recv ever produced.
        rx.ack_to(real_seq + 999, 42);
        let (seq, off) = load_cursor(dir.path());
        assert_eq!(
            (seq, off),
            (0, 0),
            "unknown-position ack must not advance the cursor",
        );

        // The real one still works — and lands on the END of the
        // event, not its start.
        rx.ack_to(real_seq, real_off);
        let (seq, off) = load_cursor(dir.path());
        assert_eq!((seq, off), expected_end);
    }
}
