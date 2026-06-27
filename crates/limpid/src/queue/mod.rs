//! Output queue: async FIFO between pipeline and output modules.
//!
//! Two implementations:
//! - **Memory queue** (default): fast, events lost on process restart
//! - **Disk queue**: WAL-based, survives restarts, configurable max size

pub mod disk;

use std::sync::Arc;

use tracing::{info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;

// ---------------------------------------------------------------------------
// Declarative schema for the per-output `queue { ... }` sub-block
// ---------------------------------------------------------------------------
//
// Every output supports the same `queue { type / path / max_size /
// capacity }` block, parsed by `QueueConfig::from_output_properties`
// below. We declare its shape once so every Module's
// `property_schema()` can splice in `QUEUE_PROPERTY_SPEC` instead of
// repeating the same four entries. Keeping the schema co-located with
// the parsing code is intentional — if a queue option is renamed
// here, both surfaces update in the same edit.

const QUEUE_BLOCK_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        name: "type",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["memory", "disk"]),
    },
    PropertySpec {
        name: "path",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "max_size",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Size,
    },
    PropertySpec {
        name: "capacity",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
];

/// `queue { ... }` sub-block specification shared by every output
/// Module's property schema. Spread into a Module schema as a single
/// `PropertySpec` value:
///
/// ```ignore
/// const SYSLOG_TCP_OUTPUT_SCHEMA: &[PropertySpec] = &[
///     PropertySpec { name: "address", ... },
///     crate::queue::QUEUE_PROPERTY_SPEC,
/// ];
/// ```
pub const QUEUE_PROPERTY_SPEC: PropertySpec = PropertySpec {
    name: "queue",
    required: false,
    repeatable: false,
    exclusive_group: None,
    kind: PropertyValueKind::Block(QUEUE_BLOCK_PROPERTIES),
};

// ---------------------------------------------------------------------------
// Declarative schema for the per-output `retry { ... }` sub-block
// ---------------------------------------------------------------------------
//
// Honored by `RetryConfig::from_output_properties` (below) for *every*
// output the queue consumer drives. Historically only the two OTLP
// outputs declared `retry` in their `property_schema()`, so non-OTLP
// configs that set `retry { ... }` got hard-failed at `--check` time
// as "unknown property" even though the runtime would happily accept
// them. Hoisting the schema here and splicing the const into every
// output's schema closes that gap without touching the runtime —
// schema acceptance now matches the runtime's existing intent.
//
// Sub-property names mirror the keys read by
// `RetryConfig::from_output_properties`; renames stay in lock-step
// with the parser because both definitions live in this file.

const RETRY_BLOCK_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        name: "max_attempts",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "initial_wait",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "max_wait",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "backoff",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["fixed", "exponential"]),
    },
];

/// `retry { ... }` sub-block specification shared by every output
/// Module's property schema. Splice alongside [`QUEUE_PROPERTY_SPEC`].
pub const RETRY_PROPERTY_SPEC: PropertySpec = PropertySpec {
    name: "retry",
    required: false,
    repeatable: false,
    exclusive_group: None,
    kind: PropertyValueKind::Block(RETRY_BLOCK_PROPERTIES),
};

// ---------------------------------------------------------------------------
// Queue item — every queue (memory + disk) now carries `Event` end-to-end
// ---------------------------------------------------------------------------
//
// Before this change the queue distinguished between a pre-rendered sink
// payload (memory hot path) and an `OwnedEvent` (disk persist / inject).
// After this change the queue uniformly carries `Event`; rendering moves from
// the pipeline-side (enqueue time) to the consumer-side (send time,
// inside each sink's `Output::consume`). Memory and disk queues are
// distinguished only by the `SenderInner` variant — there is no
// `QueueKind` tag because the pipeline never branches on it.

/// Configuration for an output queue.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub queue_type: QueueType,
    /// Maximum number of events for the memory queue. Ignored by the disk
    /// queue, which is sized in bytes via `QueueType::Disk { max_size }`.
    pub capacity: usize,
}

/// Why a queue enqueue failed. Each variant corresponds to a code
/// path inside `QueueSender::send` / `DiskQueueSender::send`.
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

#[derive(Debug, Clone)]
pub enum QueueType {
    Memory,
    Disk {
        path: String,
        max_size: u64, // bytes
    },
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            queue_type: QueueType::Memory,
            capacity: 65536,
        }
    }
}

impl QueueConfig {
    /// Parse from an output definition's `queue { ... }` block.
    pub fn from_output_properties(
        output_name: &str,
        output_props: &[Property],
    ) -> anyhow::Result<Self> {
        if let Some(block) = props::get_block(output_props, "queue") {
            let queue_type = match props::get_ident(block, "type").as_deref() {
                Some("disk") => {
                    let path = props::get_string(block, "path")
                        .unwrap_or_else(|| format!("/var/lib/limpid/queues/{}", output_name));
                    let max_size = match props::get_string(block, "max_size") {
                        Some(s) => props::parse_size(&s)?,
                        None => 0, // 0 = unlimited
                    };
                    QueueType::Disk { path, max_size }
                }
                _ => QueueType::Memory,
            };
            let capacity = props::get_positive_int(block, "capacity")?.unwrap_or(65536) as usize;
            Ok(QueueConfig {
                queue_type,
                capacity,
            })
        } else {
            Ok(QueueConfig::default())
        }
    }
}

/// Handle for sending events into a queue. Cheaply cloneable.
#[derive(Clone)]
pub struct QueueSender {
    inner: SenderInner,
    // 0.8 metrics will surface queue-level counters (depth, enqueue
    // pressure) that need a label distinct from the output name.
    // Holding the Arc<String> avoids re-plumbing it then; the per-queue
    // allocation is trivial.
    #[allow(dead_code)]
    name: Arc<String>,
    /// Optional metrics — if set, `send()` increments `events_received` on success.
    /// Set by the runtime after the output module's metrics handle is available.
    metrics: Option<Arc<crate::metrics::OutputMetrics>>,
}

#[derive(Clone)]
enum SenderInner {
    Memory(tokio::sync::mpsc::Sender<Event>),
    Disk(disk::DiskQueueSender),
}

impl QueueSender {
    /// Send an `Event` into the queue.
    ///
    /// Both queue kinds carry `Event` end-to-end after this change: memory
    /// queues forward the event to the consumer where the configured
    /// output renders it inside `Output::consume`, and disk queues
    /// serialise the same `Event` to JSON for replay. There is no
    /// longer a `Rendered`-vs-`Owned` discriminator at this level
    /// because the queue does not see sink-specific payloads anymore.
    pub async fn send(&self, event: Event) -> Result<(), QueueSendError> {
        let result: Result<(), QueueSendError> = match &self.inner {
            SenderInner::Memory(tx) => tx
                .send(event)
                .await
                .map_err(|_| QueueSendError::ChannelClosed),
            SenderInner::Disk(tx) => tx.send(event).await,
        };
        if let Some(m) = &self.metrics {
            if result.is_ok() {
                m.events_received
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                // Enqueue failure: memory-queue receiver dropped (=
                // consumer task gone, daemon usually shutting down),
                // or disk-queue serialise/write error. From this
                // sender's POV the event never made it into the
                // queue and can't be retried by the consumer (the
                // consumer never sees it). Bump `events_failed` so
                // the per-output enqueue failure is visible in
                // metrics; the pipeline-side caller additionally
                // routes the lost event through the dead-letter
                // path.
                m.events_failed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        result
    }

    /// Attach output metrics so subsequent `send()` calls count `events_received`.
    pub fn attach_metrics(&mut self, metrics: Arc<crate::metrics::OutputMetrics>) {
        self.metrics = Some(metrics);
    }

    /// Access the attached metrics (e.g. to increment `events_injected` on inject).
    pub fn metrics(&self) -> Option<&Arc<crate::metrics::OutputMetrics>> {
        self.metrics.as_ref()
    }
}

/// Position of an event in its queue, captured at `recv()` time and
/// carried through the event's lifetime on a [`QueueAckHandle`]. The
/// consumer feeds it back to the receiver via `ack_to`, which uses it
/// to advance the on-disk cursor only through the contiguous prefix of
/// acked positions — so out-of-order acks from a batched output do not
/// silently advance past still-in-flight events.
///
/// Memory queues have no persistent cursor, so the [`AckPosition::Memory`]
/// variant carries no data — it exists only so the handle's type does
/// not need to know which backend produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AckPosition {
    Memory,
    Disk { seq: u64, offset: u64 },
}

/// Handle for receiving events from a queue.
pub struct QueueReceiver {
    inner: ReceiverInner,
    name: Arc<String>,
}

enum ReceiverInner {
    Memory(tokio::sync::mpsc::Receiver<Event>),
    Disk(disk::DiskQueueReceiver),
}

impl QueueReceiver {
    /// Receive the next event, paired with the position that must be
    /// fed back via `ack_to` once the event reaches a terminal
    /// disposition. The position is captured at the moment of read,
    /// not at ack time — that distinction is what makes the disk
    /// cursor correct under batched, out-of-order acks.
    pub async fn recv(&mut self) -> Option<(Event, AckPosition)> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.recv().await.map(|e| (e, AckPosition::Memory)),
            ReceiverInner::Disk(rx) => rx.recv().await,
        }
    }

    pub fn try_recv(&mut self) -> Option<(Event, AckPosition)> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.try_recv().ok().map(|e| (e, AckPosition::Memory)),
            ReceiverInner::Disk(rx) => rx.try_recv(),
        }
    }

    /// Commit a specific event's position as processed. For the disk
    /// backend this records the ack against the in-flight position
    /// queue and, when the position is the front of the queue (or
    /// completes a contiguous acked prefix from the front), advances
    /// the persisted cursor through that prefix and reclaims
    /// fully-consumed segments. For the memory backend this is a
    /// no-op (mpsc removes events on `recv()`; there is no separate
    /// persistent cursor to advance).
    ///
    /// The consumer is expected to call `ack_to(position)` after
    /// every event's final disposition is decided — delivered, routed
    /// to recovery, or dropped. Calling out of order is safe: the
    /// disk receiver only persists through positions that are
    /// contiguously acked from the front, so a late-arriving early
    /// ack will still hold the cursor back correctly.
    pub fn ack_to(&mut self, position: AckPosition) {
        match (&mut self.inner, position) {
            (ReceiverInner::Memory(_), AckPosition::Memory) => {}
            (ReceiverInner::Disk(rx), AckPosition::Disk { seq, offset }) => {
                rx.ack_to(seq, offset);
            }
            (ReceiverInner::Memory(_), AckPosition::Disk { .. }) => {
                debug_assert!(
                    false,
                    "ack_to: Disk position fed to memory receiver — backend mismatch",
                );
                warn!("queue: ack_to received Disk position on a memory receiver (ignored)");
            }
            (ReceiverInner::Disk(_), AckPosition::Memory) => {
                debug_assert!(
                    false,
                    "ack_to: Memory position fed to disk receiver — backend mismatch",
                );
                warn!("queue: ack_to received Memory position on a disk receiver (ignored)");
            }
        }
    }
}

/// Create a sender/receiver pair.
pub fn create_queue(
    name: String,
    config: QueueConfig,
) -> anyhow::Result<(QueueSender, QueueReceiver)> {
    let name = Arc::new(name);

    match config.queue_type {
        QueueType::Memory => {
            let (tx, rx) = tokio::sync::mpsc::channel(config.capacity);
            Ok((
                QueueSender {
                    inner: SenderInner::Memory(tx),
                    name: Arc::clone(&name),
                    metrics: None,
                },
                QueueReceiver {
                    inner: ReceiverInner::Memory(rx),
                    name: Arc::clone(&name),
                },
            ))
        }
        QueueType::Disk { ref path, max_size } => {
            let (tx, rx) = disk::create_disk_queue(path, max_size)?;
            Ok((
                QueueSender {
                    inner: SenderInner::Disk(tx),
                    name: Arc::clone(&name),
                    metrics: None,
                },
                QueueReceiver {
                    inner: ReceiverInner::Disk(rx),
                    name: Arc::clone(&name),
                },
            ))
        }
    }
}

/// Retry configuration for output writes.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_wait: std::time::Duration,
    pub max_wait: std::time::Duration,
    pub backoff: BackoffStrategy,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_wait: std::time::Duration::from_secs(1),
            max_wait: std::time::Duration::from_secs(60),
            backoff: BackoffStrategy::Exponential,
        }
    }
}

impl RetryConfig {
    /// Compute the next sleep duration given the current one, applying
    /// the configured backoff. Shared by every output's internal retry
    /// loop so the doubling-then-clamp policy is defined once.
    pub fn next_wait(&self, current: std::time::Duration) -> std::time::Duration {
        match self.backoff {
            BackoffStrategy::Exponential => current.saturating_mul(2).min(self.max_wait),
            BackoffStrategy::Fixed => current,
        }
    }

    /// Parse from an output definition's properties (retry block).
    pub fn from_output_properties(output_props: &[Property]) -> anyhow::Result<Self> {
        let mut config = Self::default();

        if let Some(block) = props::get_block(output_props, "retry") {
            if let Some(n) = props::get_positive_int(block, "max_attempts")? {
                config.max_attempts = n.min(u32::MAX as u64) as u32;
            }
            if let Some(s) = props::get_string(block, "initial_wait") {
                config.initial_wait = props::parse_duration(&s)?;
            }
            if let Some(s) = props::get_string(block, "max_wait") {
                config.max_wait = props::parse_duration(&s)?;
            }
            match props::get_ident(block, "backoff").as_deref() {
                Some("fixed") => config.backoff = BackoffStrategy::Fixed,
                _ => config.backoff = BackoffStrategy::Exponential,
            }
        }

        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackoffStrategy {
    Exponential,
    Fixed,
}

// ---------------------------------------------------------------------------
// Ack lifecycle
// ---------------------------------------------------------------------------
//
// Previously the queue consumer advanced the disk-queue cursor (`ack`)
// immediately after `Output::consume` returned `Ok` — which for batched
// outputs only means "event accepted into the in-memory buffer", not
// "event shipped". A daemon crash between accept and flush silently
// lost every event in the batch (the cursor said "delivered", the
// buffer was gone).
//
// The current flow gives each event a `QueueAckHandle`. Outputs hold
// the handle until the event's final disposition is known and call
// `resolve_delivered()` or `resolve_recovered()` (= "routed to DLQ").
// The handle's destruction sends one [`AckDisposition`] back to the
// consumer, which then advances the queue cursor. A handle that drops
// without an explicit resolve sends `Dropped` and (in debug builds)
// fires a `debug_assert!` — that path is reserved for bugs / panics /
// shutdown fall-through, never the steady-state contract.

/// Final disposition of an event as far as the queue is concerned.
/// Reported back from the output via [`QueueAckHandle`] so the consumer
/// can advance the cursor and update per-disposition metrics breakdowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDisposition {
    /// Event was durably shipped to its destination.
    Delivered,
    /// Event was routed to the DLQ (`error_log`).
    Recovered,
    /// Event was dropped without explicit disposition (bug / panic /
    /// shutdown fallthrough). Should not occur on healthy paths.
    Dropped,
}

/// Handle handed to an [`crate::modules::Output`] alongside each event.
/// The output is responsible for calling `resolve_delivered` or
/// `resolve_recovered` once the event's final disposition is decided —
/// including across batched flushes that may resolve a handle long
/// after `consume` returned `Ok`. The handle's `Drop` impl falls back
/// to [`AckDisposition::Dropped`] for the unhealthy paths, and
/// `debug_assert!`s the contract was met.
#[derive(Debug)]
pub struct QueueAckHandle {
    tx: Option<tokio::sync::mpsc::UnboundedSender<(AckPosition, AckDisposition)>>,
    /// Position the handle's event occupied in the source queue,
    /// captured at `recv()` time. Pre-fix the disk receiver advanced
    /// its cursor to `self.read_*` at ack time, which under batched
    /// outputs meant "all in-flight events", not "this event" — a
    /// single ack from anywhere in the batch could advance the cursor
    /// past still-in-flight events and silently lose them on crash.
    /// Carrying the position on the handle decouples cursor commit
    /// from in-flight order.
    position: AckPosition,
    /// True once an explicit `resolve_*` ran. Read in `Drop` to
    /// distinguish "resolved cleanly" (silence the debug_assert) from
    /// "dropped without resolve" (fire the assert + send `Dropped`).
    resolved: bool,
}

impl QueueAckHandle {
    pub fn new(
        tx: tokio::sync::mpsc::UnboundedSender<(AckPosition, AckDisposition)>,
        position: AckPosition,
    ) -> Self {
        Self {
            tx: Some(tx),
            position,
            resolved: false,
        }
    }

    /// The position this handle's event occupies in the source queue.
    /// Returned alongside the [`AckDisposition`] when the handle
    /// resolves, so the queue consumer can drive `ack_to(position)`
    /// on the matching receiver. Currently only used in tests; kept
    /// public so a future output that wants to log / route on
    /// position (e.g. structured tracing of disk-cursor pressure)
    /// does not have to add a new accessor.
    #[allow(dead_code)]
    pub fn position(&self) -> AckPosition {
        self.position
    }

    /// Signal that the event was durably delivered. Consumes the handle.
    pub fn resolve_delivered(mut self) {
        self.resolved = true;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.position, AckDisposition::Delivered));
        }
    }

    /// Signal that the event was routed to the DLQ (retry exhausted,
    /// render error, shutdown leftover). Consumes the handle.
    pub fn resolve_recovered(mut self) {
        self.resolved = true;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.position, AckDisposition::Recovered));
        }
    }

    /// Test-only constructor returning the handle and the receiving
    /// half of its ack channel, so tests can assert on the disposition.
    #[cfg(test)]
    pub fn for_test() -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<(AckPosition, AckDisposition)>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(tx, AckPosition::Memory), rx)
    }

    /// Test-only constructor that also sets the carried position.
    #[cfg(test)]
    pub fn for_test_with_position(
        position: AckPosition,
    ) -> (
        Self,
        tokio::sync::mpsc::UnboundedReceiver<(AckPosition, AckDisposition)>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::new(tx, position), rx)
    }
}

impl Drop for QueueAckHandle {
    fn drop(&mut self) {
        debug_assert!(
            self.resolved,
            "QueueAckHandle dropped without explicit resolve_delivered / resolve_recovered \
             — bug: outputs MUST explicitly resolve their disposition"
        );
        if let Some(tx) = self.tx.take() {
            let _ = tx.send((self.position, AckDisposition::Dropped));
        }
    }
}

/// Run a queue consumer that drains events and writes them to an output.
///
/// The consumer does not own retry / DLQ logic — each `Output` runs
/// its own per-event retry loop and resolves a [`QueueAckHandle`]
/// when the event's final disposition is decided. The consumer's job
/// is purely "hand each event + handle to the output, then advance
/// the queue cursor when its disposition comes back". The cursor
/// only advances after the output's handle resolves, which for
/// batched outputs happens at flush time, not at buffer-accept time.
#[allow(clippy::too_many_arguments)]
pub async fn run_queue_consumer(
    mut receiver: QueueReceiver,
    writer: Arc<dyn crate::modules::Output>,
    tap: Option<crate::tap::TapRegistry>,
    metrics: Arc<crate::metrics::OutputMetrics>,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use std::sync::atomic::Ordering;

    let name = Arc::clone(&receiver.name);
    info!("output '{}': queue consumer started", name);
    let (ack_tx, mut ack_rx) =
        tokio::sync::mpsc::unbounded_channel::<(AckPosition, AckDisposition)>();
    let mut in_flight: usize = 0;
    let mut accepting = true;

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed(), if accepting => {
                if *shutdown.borrow() {
                    info!("output '{}': shutting down, draining queue", name);
                    // Feed any events already buffered on the receiver
                    // into the output one last time so they don't
                    // survive the restart with the queue still pointing
                    // at them. The output owns the per-event lifecycle
                    // from here; we exit the select loop and let the
                    // shutdown phase below resolve every in-flight
                    // handle by calling `writer.shutdown()` first
                    // (which drains batched buffers and resolves the
                    // handles parked inside them) and only then
                    // draining the ack channel.
                    while let Some((event, position)) = receiver.try_recv() {
                        if let Some(tap) = &tap {
                            tap.emit(&format!("output {}", name), &event).await;
                        }
                        let handle = QueueAckHandle::new(ack_tx.clone(), position);
                        in_flight += 1;
                        // `consume_shutdown` (not `consume`) — the
                        // shutdown contract forbids the steady-state
                        // retry path. Unbatched outputs ship once
                        // bounded then DLQ; batched outputs buffer
                        // only and let the post-loop `writer.shutdown()`
                        // drain bounded. See `Output::consume_shutdown`.
                        if let Err(e) = writer.consume_shutdown(&event, handle).await {
                            tracing::error!(
                                "output '{}': consume_shutdown returned Err during drain: {} \
                                 (bug — disposition signalled via handle)",
                                name,
                                e
                            );
                            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    break;
                }
            }

            Some((position, disposition)) = ack_rx.recv() => {
                handle_ack_disposition(disposition, &name, &metrics);
                // Dropped disposition advances the cursor the same as Delivered:
                // panic-mid-handle event loss is the pre-0.7.7 semantic and is
                // out of scope for this regression fix; full panic recovery is
                // a separate cycle.
                receiver.ack_to(position);
                in_flight = in_flight.saturating_sub(1);
                // Natural queue-closure exit: the receiver returned
                // None, we stopped accepting, and the last in-flight
                // handle just resolved. Shutdown does NOT exit here —
                // it breaks straight out of the select arm above so
                // the post-loop `writer.shutdown()` can drain batched
                // buffers (which is the only thing that can resolve
                // their parked handles).
                if !accepting && in_flight == 0 {
                    break;
                }
            }

            input = receiver.recv(), if accepting => {
                match input {
                    Some((event, position)) => {
                        if let Some(tap) = &tap {
                            tap.emit(&format!("output {}", name), &event).await;
                        }
                        let handle = QueueAckHandle::new(ack_tx.clone(), position);
                        in_flight += 1;
                        if let Err(e) = writer.consume(&event, handle).await {
                            // Reaching here means the output returned an
                            // error from `consume` itself — by the
                            // ack-handle contract that signals a bug
                            // (the output failed to take ownership of
                            // the lifecycle). The handle's Drop impl
                            // fires `Dropped` via the channel; we just
                            // log so operators can investigate.
                            tracing::error!(
                                "output '{}': consume returned Err: {} \
                                 (bug — disposition signalled via handle)",
                                name,
                                e
                            );
                            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    None => {
                        info!("output '{}': queue closed", name);
                        accepting = false;
                        if in_flight == 0 {
                            break;
                        }
                    }
                }
            }
        }
    }

    // Shutdown phase. Ordering matters: call `writer.shutdown()` FIRST
    // so batched outputs drain their internal `(Event, QueueAckHandle)`
    // buffers (final flush + per-event DLQ routing as needed),
    // resolving every still-held handle. Only then drain the ack
    // channel for cursor advancement. The reversed ordering (wait for
    // in_flight == 0 before calling shutdown) deadlocks: the buffered
    // handles are exactly what `writer.shutdown()` resolves, so the
    // wait can never make progress on its own.
    if let Err(e) = writer.shutdown(error_log.as_ref()).await {
        warn!("output '{}': shutdown flush failed: {}", name, e);
    }
    drop(ack_tx);
    while let Some((position, disposition)) = ack_rx.recv().await {
        handle_ack_disposition(disposition, &name, &metrics);
        receiver.ack_to(position);
        in_flight = in_flight.saturating_sub(1);
    }
    if in_flight != 0 {
        tracing::error!(
            "output '{}': consumer exiting with {} unresolved handle(s) — bug",
            name,
            in_flight,
        );
    }

    info!("output '{}': queue consumer stopped", name);
}

fn handle_ack_disposition(
    disposition: AckDisposition,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
) {
    use std::sync::atomic::Ordering;
    match disposition {
        AckDisposition::Delivered => {
            // The output bumped `events_written` itself on the success
            // path; nothing to do here. Kept explicit so the
            // per-disposition metrics breakdown has an obvious hook
            // when 0.8 lands.
        }
        AckDisposition::Recovered => {
            // The output bumped `events_failed` on the recovery path
            // (retry exhausted / render error / shutdown leftover).
            // Same rationale as `Delivered`.
        }
        AckDisposition::Dropped => {
            tracing::error!(
                "output '{}': event dropped without explicit disposition (bug)",
                name
            );
            metrics.events_failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod consumer_lifecycle_tests {
    use super::*;
    use crate::modules::{HasMetrics, Output};
    use bytes::Bytes;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    fn owned_event() -> Event {
        Event::new(
            Bytes::from_static(b"x"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    /// Programmable mock: each call to `consume` pops the next outcome
    /// from `script` and resolves the handle accordingly. The script
    /// vocabulary mirrors the per-event ack-lifecycle a real output
    /// reaches: `Delivered` resolves the handle as delivered, and
    /// `Bug` returns Err WITHOUT resolving the handle (exercise the
    /// consumer's fallthrough).
    #[derive(Clone, Copy)]
    enum Outcome {
        Delivered,
        Bug,
    }

    struct ScriptedWriter {
        script: Mutex<Vec<Outcome>>,
        calls: std::sync::atomic::AtomicUsize,
        metrics: Arc<crate::metrics::OutputMetrics>,
    }

    impl ScriptedWriter {
        fn new(script: Vec<Outcome>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: std::sync::atomic::AtomicUsize::new(0),
                metrics: Arc::new(crate::metrics::OutputMetrics::default()),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl HasMetrics for ScriptedWriter {
        type Stats = crate::metrics::OutputMetrics;
        fn metrics(&self) -> Arc<crate::metrics::OutputMetrics> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for ScriptedWriter {
        async fn consume(&self, _event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let next = {
                let mut s = self.script.lock().unwrap();
                if s.is_empty() {
                    Outcome::Delivered
                } else {
                    s.remove(0)
                }
            };
            match next {
                Outcome::Delivered => {
                    ack.resolve_delivered();
                    Ok(())
                }
                Outcome::Bug => {
                    // Drop the handle without resolve — exercises the
                    // consumer's Dropped-fallthrough path. Returning
                    // Err signals the bug.
                    drop(ack);
                    Err(anyhow::anyhow!("scripted bug"))
                }
            }
        }

        async fn consume_shutdown(
            &self,
            event: &Event,
            ack: QueueAckHandle,
        ) -> anyhow::Result<()> {
            self.consume(event, ack).await
        }
    }

    // ---- queue boundary error tests ----

    #[tokio::test]
    async fn queue_sender_send_returns_channel_closed_when_receiver_dropped() {
        let (tx, rx) = create_queue(
            "mem".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        drop(rx);
        let err = tx
            .send(owned_event())
            .await
            .expect_err("send to closed memory channel must fail");
        assert!(
            matches!(err, QueueSendError::ChannelClosed),
            "expected ChannelClosed, got {:?}",
            err
        );
    }

    // ---- QueueAckHandle unit tests ----

    #[tokio::test]
    async fn ack_handle_resolve_delivered_sends_delivered() {
        let (handle, mut rx) = QueueAckHandle::for_test();
        handle.resolve_delivered();
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Delivered))
        );
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn ack_handle_resolve_recovered_sends_recovered() {
        let (handle, mut rx) = QueueAckHandle::for_test();
        handle.resolve_recovered();
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Recovered))
        );
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn ack_handle_carries_position_through_resolve() {
        // Positions captured at recv time must travel back unchanged
        // through `resolve_delivered` — this is the core of the
        // positional-ack fix.
        let position = AckPosition::Disk {
            seq: 7,
            offset: 4242,
        };
        let (handle, mut rx) = QueueAckHandle::for_test_with_position(position);
        assert_eq!(handle.position(), position);
        handle.resolve_delivered();
        assert_eq!(rx.recv().await, Some((position, AckDisposition::Delivered)));
    }

    /// Dropping a handle without resolving sends `Dropped` so the
    /// consumer can still advance the cursor (avoiding queue stall on
    /// a buggy output) and bumps `events_failed`. The debug_assert
    /// path is exercised by `debug_assert_panics_on_handle_drop_without_resolve`.
    #[cfg(not(debug_assertions))]
    #[tokio::test]
    async fn ack_handle_drop_without_resolve_sends_dropped_in_release() {
        let (handle, mut rx) = QueueAckHandle::for_test();
        drop(handle);
        assert_eq!(
            rx.recv().await,
            Some((AckPosition::Memory, AckDisposition::Dropped))
        );
    }

    #[cfg(debug_assertions)]
    #[tokio::test]
    #[should_panic(expected = "QueueAckHandle dropped without explicit resolve")]
    async fn debug_assert_panics_on_handle_drop_without_resolve() {
        let (handle, _rx) = QueueAckHandle::for_test();
        drop(handle);
    }

    // ---- consumer loop ack-bookkeeping ----

    /// Build a queue + spawn the consumer driving `writer`. Returns the
    /// sender, a shutdown signal, and the join handle.
    async fn spawn_consumer(
        writer: Arc<dyn Output>,
        metrics: Arc<crate::metrics::OutputMetrics>,
    ) -> (
        QueueSender,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = create_queue(
            "test".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 16,
            },
        )
        .unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(run_queue_consumer(
            receiver,
            writer,
            None,
            metrics,
            None,
            shutdown_rx,
        ));
        (sender, shutdown_tx, handle)
    }

    #[tokio::test]
    async fn consumer_acks_each_delivered_event() {
        let writer = Arc::new(ScriptedWriter::new(vec![
            Outcome::Delivered,
            Outcome::Delivered,
            Outcome::Delivered,
        ]));
        let metrics = Arc::new(crate::metrics::OutputMetrics::default());
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        for _ in 0..3 {
            sender.send(owned_event()).await.unwrap();
        }
        // Tickle shutdown so the consumer wraps up; it must still
        // process the events already on the queue.
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let _ = shutdown.send(true);
        handle.await.unwrap();
        assert_eq!(writer.calls(), 3);
        // Delivered dispositions do not bump `events_failed`.
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn consumer_handles_bug_path_via_drop_fallthrough() {
        // The bug path drops the handle without resolving — Drop sends
        // `Dropped`, and Drop's debug_assert fires on debug builds. We
        // only run this assertion in release builds (panics in debug
        // would make the test fail). The path still exists and is
        // counted; we just don't materialise the panic here.
        if cfg!(debug_assertions) {
            return;
        }
        let writer = Arc::new(ScriptedWriter::new(vec![Outcome::Bug]));
        let metrics = Arc::new(crate::metrics::OutputMetrics::default());
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        sender.send(owned_event()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let _ = shutdown.send(true);
        handle.await.unwrap();
        assert!(metrics.events_failed.load(Ordering::Relaxed) >= 1);
    }

    // ---- batched-output shutdown ordering ----

    /// A batched-output mock: `consume` parks the ack handle in an
    /// internal buffer without resolving it (= the steady-state
    /// contract for a real batched output below `batch_size`). Only
    /// `shutdown()` drains the buffer and resolves every handle. This
    /// is exactly the shape that deadlocked the consumer when the
    /// previous ordering waited for `in_flight == 0` BEFORE calling
    /// `shutdown()`.
    struct BatchedMockWriter {
        buffer: tokio::sync::Mutex<Vec<QueueAckHandle>>,
        shutdown_mode: ShutdownMode,
        shutdown_called: std::sync::atomic::AtomicBool,
        consume_calls: std::sync::atomic::AtomicUsize,
        metrics: Arc<crate::metrics::OutputMetrics>,
    }

    #[derive(Clone, Copy)]
    enum ShutdownMode {
        /// Final flush succeeds: resolve every parked handle as Delivered.
        DeliverAll,
        /// Final flush fails: route every parked event to DLQ — but we
        /// only have handles here (no Event captured), so we resolve as
        /// Recovered to mirror the real shutdown DLQ path.
        FailAll,
    }

    impl BatchedMockWriter {
        fn new(mode: ShutdownMode) -> Self {
            Self {
                buffer: tokio::sync::Mutex::new(Vec::new()),
                shutdown_mode: mode,
                shutdown_called: std::sync::atomic::AtomicBool::new(false),
                consume_calls: std::sync::atomic::AtomicUsize::new(0),
                metrics: Arc::new(crate::metrics::OutputMetrics::default()),
            }
        }
    }

    impl HasMetrics for BatchedMockWriter {
        type Stats = crate::metrics::OutputMetrics;
        fn metrics(&self) -> Arc<crate::metrics::OutputMetrics> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for BatchedMockWriter {
        async fn consume(&self, _event: &Event, ack: QueueAckHandle) -> anyhow::Result<()> {
            self.consume_calls.fetch_add(1, Ordering::Relaxed);
            self.buffer.lock().await.push(ack);
            Ok(())
        }

        async fn consume_shutdown(
            &self,
            _event: &Event,
            ack: QueueAckHandle,
        ) -> anyhow::Result<()> {
            // Batched mock: park into the same buffer that shutdown()
            // will drain. No flush trigger — mirrors the real batched
            // outputs' shutdown contract.
            self.consume_calls.fetch_add(1, Ordering::Relaxed);
            self.buffer.lock().await.push(ack);
            Ok(())
        }

        async fn shutdown(
            &self,
            _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
        ) -> anyhow::Result<()> {
            self.shutdown_called.store(true, Ordering::Relaxed);
            let leftovers = std::mem::take(&mut *self.buffer.lock().await);
            match self.shutdown_mode {
                ShutdownMode::DeliverAll => {
                    for ack in leftovers {
                        ack.resolve_delivered();
                    }
                }
                ShutdownMode::FailAll => {
                    for ack in leftovers {
                        // Mirror the real shutdown DLQ recovery path:
                        // failed final flush → resolve Recovered after
                        // routing each event to error_log.
                        self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        ack.resolve_recovered();
                    }
                }
            }
            Ok(())
        }
    }

    /// The ordering fix: outputs that hold handles inside an internal
    /// buffer until `shutdown()` runs must see `writer.shutdown()`
    /// called BEFORE the consumer waits for in-flight to drain. The
    /// old code reversed this and would have hung on a batched output.
    /// Here we verify the consumer exits cleanly and every event's ack
    /// disposition was reported (= the `Delivered` count on the
    /// metrics path is bumped by the output itself, but the consumer
    /// must have processed each disposition in the post-loop drain).
    #[tokio::test]
    async fn shutdown_drains_batched_buffer_before_consumer_exits() {
        let writer = Arc::new(BatchedMockWriter::new(ShutdownMode::DeliverAll));
        let metrics = Arc::new(crate::metrics::OutputMetrics::default());
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        for _ in 0..5 {
            sender.send(owned_event()).await.unwrap();
        }
        // Give the consumer time to pull every event into the
        // writer's internal buffer (no flush — `consume` parks the
        // handles unresolved).
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(writer.consume_calls.load(Ordering::Relaxed), 5);

        let _ = shutdown.send(true);

        // The consumer must terminate without external help. If the
        // ordering reverts to "wait for in_flight == 0 before calling
        // shutdown()", this await blocks forever and the test times
        // out — that is the deadlock the fix prevents.
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit within 2s after shutdown")
            .expect("consumer task panicked");

        assert!(
            writer.shutdown_called.load(Ordering::Relaxed),
            "writer.shutdown() must have been called"
        );
        // Delivered dispositions: no failures bumped by the consumer.
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
    }

    /// Shutdown DLQ routing: when the writer's `shutdown()` body fails
    /// its final flush and routes each buffered event to the error
    /// log, every handle resolves as `Recovered`. The consumer must
    /// drain those dispositions and exit cleanly.
    #[tokio::test]
    async fn shutdown_routes_failed_batch_to_dlq() {
        let writer = Arc::new(BatchedMockWriter::new(ShutdownMode::FailAll));
        let metrics = Arc::new(crate::metrics::OutputMetrics::default());
        let (sender, shutdown, handle) =
            spawn_consumer(writer.clone() as Arc<dyn Output>, Arc::clone(&metrics)).await;
        for _ in 0..3 {
            sender.send(owned_event()).await.unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(writer.consume_calls.load(Ordering::Relaxed), 3);

        let _ = shutdown.send(true);
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("consumer must exit within 2s")
            .expect("consumer task panicked");

        assert!(writer.shutdown_called.load(Ordering::Relaxed));
        // Each event's failure is bumped on the writer's metrics
        // (where real outputs track it). The consumer's per-event
        // metrics object is the same struct, but bumped on a
        // different path: `Recovered` does NOT touch the consumer's
        // counter (see `handle_ack_disposition`). So the consumer's
        // metrics stay at 0 and the writer's is at 3.
        assert_eq!(writer.metrics.events_failed.load(Ordering::Relaxed), 3);
        assert_eq!(metrics.events_failed.load(Ordering::Relaxed), 0);
    }

    /// `ack_to(AckPosition::Memory)` on a memory-backed receiver is a
    /// silent no-op: the mpsc channel already consumed the event on
    /// recv, there is no persistent cursor to advance. This is the
    /// contract every memory-queue consumer relies on; a regression
    /// that, e.g., panicked here would break every non-disk output
    /// the moment the consumer loop tried to commit an ack.
    #[tokio::test]
    async fn memory_queue_ack_to_is_noop() {
        let (sender, mut receiver) = create_queue(
            "mem".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
            },
        )
        .unwrap();
        sender.send(owned_event()).await.unwrap();
        let (_e, position) = receiver.recv().await.unwrap();
        assert_eq!(position, AckPosition::Memory);
        receiver.ack_to(position);
        // A second ack on the same position is still a clean no-op.
        receiver.ack_to(AckPosition::Memory);
    }
}

// ---------------------------------------------------------------------------
// Schema-splice regression tests
// ---------------------------------------------------------------------------
//
// Before the retry-schema splice, only the two OTLP outputs declared
// `retry` in their `property_schema()` even though
// `RetryConfig::from_output_properties` reads it for *every* output.
// The tests below pin the invariant: every non-OTLP output's schema
// accepts a `retry { ... }`
// block without raising `UnknownKey`. Adding a third output schema
// later is then a hard failure here if the splice is forgotten.
#[cfg(test)]
mod schema_splice_tests {
    use super::{BackoffStrategy, RetryConfig};
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::dsl::schema::{PropertySpec, SchemaError, SchemaErrorKind, validate};
    use crate::modules::Module;
    use crate::modules::output::file::FileOutput;
    use crate::modules::output::http::HttpOutput;
    use crate::modules::output::otlp::grpc::OtlpGrpcOutput;
    use crate::modules::output::otlp::http::OtlpHttpOutput;
    use crate::modules::output::stdout::StdoutOutput;
    use crate::modules::output::syslog_tcp::SyslogTcpOutput;
    use crate::modules::output::syslog_udp::SyslogUdpOutput;
    use crate::modules::output::unix_socket::UnixSocketOutput;

    #[cfg(feature = "kafka")]
    use crate::modules::output::kafka::KafkaOutput;

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str, properties: Vec<Property>) -> Property {
        Property::Block {
            key: key.into(),
            key_span: None,
            properties,
        }
    }

    /// `retry { max_attempts 3 initial_wait "100ms" max_wait "5s" backoff exponential }`
    fn full_retry_block() -> Property {
        block(
            "retry",
            vec![
                kv("max_attempts", ExprKind::IntLit(3)),
                kv("initial_wait", ExprKind::StringLit("100ms".into())),
                kv("max_wait", ExprKind::StringLit("5s".into())),
                kv("backoff", ExprKind::Ident(vec!["exponential".into()])),
            ],
        )
    }

    /// Filter out `UnknownKey` errors for the splice contract — other
    /// errors (e.g. missing required keys we didn't supply for the
    /// schema under test) are off-topic for this test.
    fn unknown_key_errs(errs: &[SchemaError]) -> Vec<&SchemaError> {
        errs.iter()
            .filter(|e| matches!(e.kind, SchemaErrorKind::UnknownKey))
            .collect()
    }

    fn assert_accepts(schema: &[PropertySpec], output_name: &str) {
        let props = vec![full_retry_block()];
        let errs = validate(&props, schema);
        let unknown = unknown_key_errs(&errs);
        assert!(
            unknown.is_empty(),
            "output '{}': retry should be accepted by schema, \
             got UnknownKey errors for: {:?}",
            output_name,
            unknown.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn every_output_schema_accepts_retry() {
        // The full matrix lives in one test so adding a new output
        // either splices the common const into its schema or fails
        // this test loudly.
        assert_accepts(StdoutOutput::property_schema().unwrap(), "stdout");
        assert_accepts(FileOutput::property_schema().unwrap(), "file");
        assert_accepts(HttpOutput::property_schema().unwrap(), "http");
        assert_accepts(SyslogTcpOutput::property_schema().unwrap(), "syslog_tcp");
        assert_accepts(SyslogUdpOutput::property_schema().unwrap(), "syslog_udp");
        assert_accepts(UnixSocketOutput::property_schema().unwrap(), "unix_socket");
        assert_accepts(OtlpGrpcOutput::property_schema().unwrap(), "otlp_grpc");
        assert_accepts(OtlpHttpOutput::property_schema().unwrap(), "otlp_http");
        #[cfg(feature = "kafka")]
        assert_accepts(KafkaOutput::property_schema().unwrap(), "kafka");
    }

    /// Inside the `retry { ... }` block, the four documented keys must
    /// be the only accepted ones across every output schema —
    /// confirms the hoist into `crate::queue` keeps the OTLP-era block
    /// shape exactly (no key drift).
    #[test]
    fn retry_block_rejects_unknown_inner_key_in_all_outputs() {
        let bad_retry = block("retry", vec![kv("typo", ExprKind::IntLit(1))]);
        let props = vec![bad_retry];
        #[allow(unused_mut)]
        let mut schemas: Vec<(&[PropertySpec], &str)> = vec![
            (StdoutOutput::property_schema().unwrap(), "stdout"),
            (FileOutput::property_schema().unwrap(), "file"),
            (HttpOutput::property_schema().unwrap(), "http"),
            (SyslogTcpOutput::property_schema().unwrap(), "syslog_tcp"),
            (SyslogUdpOutput::property_schema().unwrap(), "syslog_udp"),
            (UnixSocketOutput::property_schema().unwrap(), "unix_socket"),
            (OtlpGrpcOutput::property_schema().unwrap(), "otlp_grpc"),
            (OtlpHttpOutput::property_schema().unwrap(), "otlp_http"),
        ];
        #[cfg(feature = "kafka")]
        schemas.push((KafkaOutput::property_schema().unwrap(), "kafka"));
        for (schema, name) in schemas {
            let errs = validate(&props, schema);
            let typo_err = errs
                .iter()
                .find(|e| matches!(e.kind, SchemaErrorKind::UnknownKey) && e.key == "typo");
            assert!(
                typo_err.is_some(),
                "output '{}': retry block should reject unknown inner key 'typo', got errs={:?}",
                name,
                errs.iter().map(|e| (&e.kind, &e.key)).collect::<Vec<_>>(),
            );
        }
    }

    /// `RetryConfig::from_output_properties` parses retry props on a
    /// non-OTLP output (kafka here) into the same fields the OTLP
    /// outputs have always populated. Anchors the "runtime behavior
    /// already matched, schema just caught up" invariant.
    #[test]
    fn retry_config_parser_reads_same_fields_for_non_otlp_outputs() {
        let props = vec![full_retry_block()];
        let cfg = RetryConfig::from_output_properties(&props)
            .expect("retry block parses for non-OTLP output");
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.initial_wait, std::time::Duration::from_millis(100));
        assert_eq!(cfg.max_wait, std::time::Duration::from_secs(5));
        assert!(matches!(cfg.backoff, BackoffStrategy::Exponential));
    }
}
