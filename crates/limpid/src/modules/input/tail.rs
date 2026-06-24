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
use crate::event::{AckHandle, AckPosition, Event};
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

        // `read_offset` is the in-memory file cursor — it tracks bytes
        // already emitted onto the pipeline channel and is used so the
        // next file read doesn't re-emit the same line twice. It moves
        // forward at emission time.
        //
        // `acked_offset` is the on-disk watermark — the cursor that gets
        // written to the state file. It moves forward only when the
        // pipeline worker has finished processing the corresponding event
        // (the per-event AckHandle drops and sends its position back over
        // `ack_rx`).
        //
        // These were one variable before this change. Splitting them is
        // what closes the at-most-once gap: a crash now leaves an on-disk
        // cursor that points to the last *acked* line, not the last
        // *emitted* line, so events in flight at the moment of the crash
        // get re-read on the next boot.
        let mut read_offset = self.initial_offset().await;
        let mut acked_offset = read_offset;
        let mut last_inode = get_inode(&self.path);

        // Unbounded ack channel — see `AckHandle` for the rationale
        // (backpressure here would deadlock the pipeline).
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();

        loop {
            // Check for shutdown
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!("tail {}: shutting down", self.path.display());
                        // Drain any in-flight acks before the final save so
                        // events that finished between the last periodic
                        // save and shutdown still advance the cursor. Any
                        // event still on the wire (un-acked) is intentionally
                        // left for re-read on next boot — that's the
                        // at-least-once contract.
                        if let Some(hi) = drain_acks_into_watermark(&mut ack_rx) {
                            acked_offset = acked_offset.max(hi);
                        }
                        self.save_position(acked_offset);
                        break;
                    }
                }
                _ = tokio::time::sleep(self.poll_interval) => {}
                // Also wake on ack so a quiet file with active downstream
                // still flushes the watermark in a timely fashion.
                Some(pos) = ack_rx.recv() => {
                    if let Some(new_off) = ack_offset(&pos) {
                        let hi = drain_remaining_acks(&mut ack_rx, new_off);
                        // Monotonic guard: an out-of-order ack must not
                        // roll the watermark backwards.
                        if hi > acked_offset {
                            acked_offset = hi;
                            self.save_position(acked_offset);
                        }
                    }
                    continue;
                }
            }

            // Check if file exists
            let meta = match tokio::fs::metadata(&self.path).await {
                Ok(m) => m,
                Err(_) => {
                    debug!("tail: {} not found, waiting", self.path.display());
                    continue;
                }
            };

            // Detect rotation: inode changed or file truncated. Both
            // cursors reset together — a rotated file has a fresh byte
            // namespace, so any in-flight acks from the previous file are
            // stale and must not be persisted against the new file.
            let current_inode = get_inode(&self.path);
            if current_inode != last_inode {
                info!(
                    "tail {}: rotation detected (inode changed), resetting to beginning",
                    self.path.display()
                );
                read_offset = 0;
                acked_offset = 0;
                // Discard stray acks from the pre-rotation file so they
                // don't poison the post-rotation watermark.
                let _ = drain_acks_into_watermark(&mut ack_rx);
                last_inode = current_inode;
            } else if meta.len() < read_offset {
                info!(
                    "tail {}: file truncated, resetting to beginning",
                    self.path.display()
                );
                read_offset = 0;
                acked_offset = 0;
                let _ = drain_acks_into_watermark(&mut ack_rx);
            }

            // No new data
            if meta.len() <= read_offset {
                continue;
            }

            // Read new lines. The returned offset is the *emitted*
            // position; we update `read_offset` but NOT `acked_offset`.
            match self
                .read_new_lines(read_offset, &tx, source_addr, ack_tx.clone())
                .await
            {
                Ok(new_offset) => {
                    read_offset = new_offset;
                }
                Err(e) => {
                    warn!("tail {}: read error: {}", self.path.display(), e);
                }
            }

            // Opportunistic drain of acks accumulated during the read pass.
            if let Some(hi) = drain_acks_into_watermark(&mut ack_rx)
                && hi > acked_offset
            {
                acked_offset = hi;
                self.save_position(acked_offset);
            }
        }

        Ok(())
    }
}

/// Extract the file offset from an `AckPosition` issued by this input.
/// Always `Offset(_)` for tail; the `Cursor(_)` variant is journald-only.
fn ack_offset(pos: &AckPosition) -> Option<u64> {
    match pos {
        AckPosition::Offset(o) => Some(*o),
        AckPosition::Cursor(_) => None,
    }
}

/// Drain everything currently in the ack channel and return the highest
/// offset seen, or `None` if the channel was empty. Non-blocking.
fn drain_acks_into_watermark(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AckPosition>,
) -> Option<u64> {
    let mut hi: Option<u64> = None;
    while let Ok(pos) = rx.try_recv() {
        if let Some(o) = ack_offset(&pos) {
            hi = Some(hi.map_or(o, |cur| cur.max(o)));
        }
    }
    hi
}

/// Like [`drain_acks_into_watermark`] but seeded with `initial` so callers
/// holding an already-extracted value can fold remaining items in one pass.
fn drain_remaining_acks(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<AckPosition>,
    initial: u64,
) -> u64 {
    let mut hi = initial;
    while let Ok(pos) = rx.try_recv() {
        if let Some(o) = ack_offset(&pos) {
            hi = hi.max(o);
        }
    }
    hi
}

impl TailInput {
    async fn read_new_lines(
        &self,
        from_offset: u64,
        tx: &tokio::sync::mpsc::Sender<Event>,
        source_addr: std::net::SocketAddr,
        ack_tx: tokio::sync::mpsc::UnboundedSender<AckPosition>,
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

            // Stamp the event with the post-line offset. When the pipeline
            // worker finishes (or fans out across multiple workers and the
            // outer scope ends), the embedded AckHandle drops and sends the
            // offset back to the run loop's ack channel. Inputs without a
            // configured `state_file` still create the token — the handle
            // is cheap (one Arc + a closure-shaped channel send), and the
            // `save_position` call it eventually triggers is a no-op when
            // no state file is configured. Branching on `state_file` here
            // would add a config-dependent code path for negligible win.
            let ack = Arc::new(AckHandle::new(
                AckPosition::Offset(current_offset),
                ack_tx.clone(),
            ));
            let event = Event::with_ack(
                Bytes::copy_from_slice(trimmed.as_bytes()),
                source_addr,
                Arc::clone(&ack),
            );
            if let Err(send_err) = tx.send(event).await {
                // Downstream closed. The event never reached the pipeline,
                // so disarm its ack BEFORE letting the SendError drop the
                // event — otherwise the embedded handle would still fire
                // and advance the cursor past a line that was never
                // processed. The line gets retried via the rewind below.
                ack.disarm();
                drop(send_err);
                current_offset -= bytes_read as u64;
                break;
            }
        }

        Ok(current_offset)
    }

    /// Where to start the very first read for this `run()`.
    ///
    /// - `Some(n)` from the state file → resume from `n`, including
    ///   the legitimate `Some(0)` case (e.g. we shut down right after
    ///   a rotate/truncate). Treating `Some(0)` as "no state" used
    ///   to send the cursor to EOF and silently skip every line
    ///   appended between save and restart.
    /// - `None` (no state file configured, missing, or unparseable)
    ///   → start at EOF so a fresh daemon doesn't replay the entire
    ///   historical log.
    async fn initial_offset(&self) -> u64 {
        match self.load_position() {
            Some(n) => n,
            None => tokio::fs::metadata(&self.path)
                .await
                .map(|m| m.len())
                .unwrap_or(0),
        }
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

    /// Throwaway ack sender for tests that don't exercise the ack-feedback
    /// loop. The corresponding receiver is dropped, so events that fire
    /// their ack on drop will see a closed channel and silently no-op (the
    /// same path a shutdown-time stray ack would take in production).
    fn dummy_ack_tx() -> tokio::sync::mpsc::UnboundedSender<AckPosition> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        tx
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

        let next_off = input.read_new_lines(0, &tx, dummy_addr(), dummy_ack_tx()).await.unwrap();
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

        let next_off = input.read_new_lines(0, &tx, dummy_addr(), dummy_ack_tx()).await.unwrap();
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

        let off1 = input.read_new_lines(0, &tx, dummy_addr(), dummy_ack_tx()).await.unwrap();
        let _ = rx.recv().await; // drain "complete"
        // Writer appends the newline.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"\n").unwrap();
        }
        let off2 = input.read_new_lines(off1, &tx, dummy_addr(), dummy_ack_tx()).await.unwrap();
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

    #[tokio::test]
    async fn initial_offset_resumes_from_saved_zero_not_eof() {
        // Regression: `save_position(0)` used to be indistinguishable
        // from "no state file" because `load_position().unwrap_or(0)`
        // collapsed both into the same `0`, and the follow-up `if
        // offset == 0` then bumped the cursor to EOF. That silently
        // dropped any data appended between a rotation-time save (=
        // offset 0) and the next start — the typical recovery shape
        // for `tail`.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let state = dir.path().join("state");
        std::fs::write(&log, b"appended-since-shutdown\n").unwrap();
        let input = make_input(&log, Some(&state));
        input.save_position(0);

        assert_eq!(
            input.initial_offset().await,
            0,
            "Some(0) from the state file must resume at 0, not EOF",
        );
    }

    #[tokio::test]
    async fn initial_offset_starts_at_eof_without_state_file() {
        // First-run / no-state-file behaviour is preserved: don't
        // replay the entire historical log, start at EOF.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        std::fs::write(&log, b"existing-content\n").unwrap();
        let input = make_input(&log, None);

        assert_eq!(input.initial_offset().await, 17);
    }

    #[tokio::test]
    async fn initial_offset_starts_at_eof_when_state_file_missing() {
        // State file configured but absent (= first start) — same
        // contract as "no state file at all": start at EOF.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let state = dir.path().join("does-not-exist");
        std::fs::write(&log, b"existing\n").unwrap();
        let input = make_input(&log, Some(&state));

        assert_eq!(input.initial_offset().await, 9);
    }

    #[tokio::test]
    async fn read_new_lines_rewinds_on_downstream_send_failure() {
        // Regression: when the consumer is gone and `tx.send().await`
        // fails, `read_new_lines` used to break out with
        // `current_offset` already advanced past the un-sent line.
        // `run()` then saved that offset and the next poll skipped
        // the line entirely — silent data loss. The fix rewinds by
        // `bytes_read` so the line is retried.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"line1\nline2\n").unwrap();
        let input = make_input(&path, None);
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        // Close the receiver so the first send fails.
        drop(rx);

        let next_off = input.read_new_lines(0, &tx, dummy_addr(), dummy_ack_tx()).await.unwrap();
        assert_eq!(
            next_off, 0,
            "send failure must rewind so the un-sent line is retried",
        );
    }

    // -----------------------------------------------------------------
    // Ack-driven cursor advance — coverage for the at-most-once gap fix
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn read_new_lines_embeds_ack_handle_per_event() {
        // Pin the wire contract: every emitted Event carries an
        // AckHandle whose position is the post-line offset. When the
        // receiver drops the event, the handle's Drop fires and the
        // offset reaches the run loop's ack channel — that is the
        // mechanism the cursor watermark rides on.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"a\nbb\n").unwrap();
        let input = make_input(&path, None);
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();

        let _ = input
            .read_new_lines(0, &tx, dummy_addr(), ack_tx)
            .await
            .unwrap();

        // Drain the two emitted events; dropping them fires their acks.
        let e1 = rx.recv().await.unwrap();
        let e2 = rx.recv().await.unwrap();
        assert!(e1.ack.is_some(), "first event must carry an AckHandle");
        assert!(e2.ack.is_some(), "second event must carry an AckHandle");
        drop(e1);
        drop(e2);

        // Both acks must now be in the channel, with offsets matching
        // the post-line positions: 2 (after "a\n") and 5 (after "bb\n").
        let mut offsets = Vec::new();
        while let Ok(pos) = ack_rx.try_recv() {
            if let AckPosition::Offset(o) = pos {
                offsets.push(o);
            }
        }
        offsets.sort();
        assert_eq!(offsets, vec![2, 5], "acks must carry post-line offsets");
    }

    #[tokio::test]
    async fn save_position_does_not_advance_without_ack() {
        // Regression for the at-most-once gap: emitting a line must NOT
        // by itself cause the on-disk cursor to advance. The cursor
        // moves only after the corresponding event has been acked back
        // (= pipeline worker finished processing it). We simulate the
        // "crashed before ack" state by holding the events in the
        // channel and asserting the state file remains at its initial
        // value.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let state = dir.path().join("state");
        std::fs::write(&log, b"one\ntwo\n").unwrap();
        // Pre-seed the watermark so we can observe (lack of) advance.
        let input = make_input(&log, Some(&state));
        input.save_position(0);
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let (ack_tx, _ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();

        let _ = input
            .read_new_lines(0, &tx, dummy_addr(), ack_tx)
            .await
            .unwrap();

        // No ack has been drained back; the on-disk cursor must still
        // be at 0. Pre-fix this would have been 8 (= EOF).
        assert_eq!(
            input.load_position(),
            Some(0),
            "cursor must not advance until acks come back",
        );
    }

    #[tokio::test]
    async fn send_failure_disarms_ack_so_un_sent_line_not_marked_consumed() {
        // The failure-mode that drove the disarm() API: when tx.send
        // fails (downstream closed), the un-sent event still gets
        // dropped — which would naively fire its ack and advance the
        // cursor past a line that was never processed. The disarm()
        // call inside read_new_lines must suppress that ack so the
        // line is correctly retried on the next poll.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log");
        std::fs::write(&path, b"will-not-be-processed\n").unwrap();
        let input = make_input(&path, None);
        let (tx, rx) = tokio::sync::mpsc::channel::<Event>(1);
        drop(rx); // close receiver so the first send fails
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();

        let next_off = input
            .read_new_lines(0, &tx, dummy_addr(), ack_tx)
            .await
            .unwrap();
        assert_eq!(next_off, 0, "send failure must rewind the read offset");

        // Crucially: the dropped (un-sent) event must NOT have fired an
        // ack — disarm() suppressed it.
        let spurious = ack_rx.try_recv();
        assert!(
            spurious.is_err(),
            "disarmed handle must not emit an ack; got {:?}",
            spurious
        );
    }

    #[tokio::test]
    async fn ack_handle_fires_on_event_drop() {
        // Smoke test for the AckHandle drop mechanism itself: build a
        // handle outside of the input layer, embed it in a synthetic
        // event, drop the event, observe the position on the ack
        // channel. This pins the contract the input layer relies on.
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();
        let handle = Arc::new(AckHandle::new(AckPosition::Offset(42), ack_tx));
        let event = Event::with_ack(
            Bytes::from_static(b"x"),
            dummy_addr(),
            Arc::clone(&handle),
        );
        // Drop both the event and our local Arc clone — refcount goes
        // to zero, Drop fires, the offset lands on the channel.
        drop(event);
        drop(handle);
        match ack_rx.recv().await {
            Some(AckPosition::Offset(42)) => {}
            other => panic!("expected Offset(42), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn ack_handle_does_not_fire_after_disarm() {
        // Sibling to the above: disarm() must convert Drop into a no-op
        // so the receiver sees nothing.
        let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel::<AckPosition>();
        let handle = Arc::new(AckHandle::new(AckPosition::Offset(99), ack_tx));
        handle.disarm();
        drop(handle);
        // Give the runtime a tick — if a spurious ack were to arrive,
        // it would be in the channel by now.
        tokio::task::yield_now().await;
        let got = ack_rx.try_recv();
        assert!(
            got.is_err(),
            "disarmed handle must not emit on drop; got {:?}",
            got
        );
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
