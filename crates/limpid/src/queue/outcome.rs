//! Type-encoded outcomes for the queue I/O boundary.
//!
//! Replaces the previous `bool` returns on [`QueueSender::send`],
//! [`disk::DiskQueueSender::send`], and `write_with_retry`. Behavior
//! is unchanged — every former `false` path maps to a specific variant
//! here so callers can pattern-match the failure mode instead of
//! collapsing it. Currently every caller treats all failure variants
//! equivalently (log + DLQ), so the split is purely for visibility and
//! to give future recovery-routing PRs a place to dispatch.
//!
//! [`QueueSender::send`]: super::QueueSender::send
//! [`disk::DiskQueueSender::send`]: super::disk::DiskQueueSender::send

use std::io;

/// Why a queue enqueue failed. Every variant corresponds to a code
/// path inside `QueueSender::send` / `DiskQueueSender::send` that was
/// previously returning `bool` = `false`.
///
/// `#[non_exhaustive]` so future recovery-routing work can add
/// variants without breaking external matches; downstream code should
/// use `Result` patterns that accept new variants forward-compatibly.
/// Current callers in-tree match exhaustively.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QueueSendError {
    /// Memory queue's receiving end was dropped (consumer task gone,
    /// daemon usually shutting down). Comes from
    /// `tokio::sync::mpsc::Sender::send().await.is_err()`.
    #[error("queue receiver dropped (channel closed)")]
    ChannelClosed,

    /// The pipeline routed a `SinkInput::Rendered` payload onto a
    /// disk-persist queue. Disk queues only carry serialisable
    /// `Owned` events; `Rendered` carries a non-serialisable
    /// `Box<dyn Any>`. Pipeline dispatch already gates this — hitting
    /// this variant means a programmer mistake elsewhere routed a
    /// rendered payload to a disk sink.
    #[error("rendered payload routed to a disk-persist queue (programmer bug)")]
    RenderedOnDisk,

    /// Disk queue failed to serialise the event to JSON before
    /// writing. The underlying error is preserved for logging.
    #[error("disk queue: failed to serialize event: {0}")]
    Serialize(#[from] serde_json::Error),

    /// `tokio::task::spawn_blocking` returned a `JoinError` while the
    /// disk queue was performing the synchronous segment write.
    #[error("disk queue: write task failed: {0}")]
    JoinError(#[from] tokio::task::JoinError),

    /// Disk queue segment write failed — covers open/append/flush
    /// errors inside `write_to_segment`. The helper currently
    /// collapses the specific I/O error onto its own error log; this
    /// variant signals only that the write didn't land.
    #[error("disk queue: segment write failed")]
    DiskWrite,
}

impl QueueSendError {
    /// Construct from a raw `io::Error` produced by the disk-segment
    /// write path. Kept as a constructor (not a `From` impl) because
    /// the underlying error is already logged inside the helper; the
    /// outer error carries only the disposition.
    #[allow(dead_code)] // reserved for future disk-write paths that surface io::Error
    pub fn from_disk_io(_e: io::Error) -> Self {
        QueueSendError::DiskWrite
    }
}

/// Disposition of an event after `write_with_retry`. Type-encoded
/// version of the function's previous doc comment ("true on success,
/// false if event was dropped/sent to secondary").
///
/// `#[must_use]` so a caller can't silently throw the disposition
/// away — that's exactly the bug shape this refactor is meant to
/// prevent. PR-O added [`DroppedToRecovery`] to distinguish payloads
/// persisted to `error_log` for replay from unrecoverable drops;
/// `#[non_exhaustive]` remains so future routing work can extend
/// without churning match sites.
///
/// [`DroppedToRecovery`]: WriteDisposition::DroppedToRecovery
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
#[non_exhaustive]
pub enum WriteDisposition {
    /// `OutputWriter::consume` returned `Ok(())` on some attempt.
    Delivered,

    /// `consume` ultimately failed but the original `Owned` event was
    /// successfully forwarded to the configured secondary queue.
    RoutedToSecondary,

    /// `consume` ultimately failed and the event was not handed off:
    /// retries exhausted with no secondary configured, the secondary
    /// send itself failed, or the payload was `Rendered` (not
    /// re-routable). The consumer treats all three as "done from the
    /// queue's POV" and acks anyway.
    Dropped,

    /// `consume` ultimately failed, the event could not be routed to a
    /// secondary queue (none configured, or the secondary enqueue
    /// itself failed), and the payload was persisted to the configured
    /// `error_log` JSONL file for manual recovery. Distinguishes the
    /// "payload survives on disk" outcome from the unrecoverable
    /// [`Dropped`] case so PR-Q metrics can count recoverable losses
    /// separately.
    ///
    /// [`Dropped`]: WriteDisposition::Dropped
    DroppedToRecovery,
}
