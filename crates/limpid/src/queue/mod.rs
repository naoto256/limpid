//! Output queue: async FIFO between pipeline and output modules.
//!
//! Two implementations:
//! - **Memory queue** (default): fast, events lost on process restart
//! - **Disk queue**: WAL-based, survives restarts, configurable max size

pub mod disk;
pub mod outcome;

use std::sync::Arc;

use tracing::{error, info, warn};

pub use outcome::{QueueSendError, WriteDisposition};

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
    /// Maximum number of events for memory queue / segment config for disk queue.
    pub capacity: usize,
    #[allow(dead_code)] // will be wired when backpressure config is exposed in DSL
    pub overflow: OverflowStrategy,
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
            overflow: OverflowStrategy::Block,
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
                ..Default::default()
            })
        } else {
            Ok(QueueConfig::default())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // DropNewest will be wirable via DSL queue config
pub enum OverflowStrategy {
    /// Block the pipeline until space is available (backpressure).
    Block,
    /// Drop the newest event (the one being sent).
    DropNewest,
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
    pub async fn recv(&mut self) -> Option<Event> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.recv().await,
            ReceiverInner::Disk(rx) => rx.recv().await,
        }
    }

    pub fn try_recv(&mut self) -> Option<Event> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.try_recv().ok(),
            ReceiverInner::Disk(rx) => rx.try_recv(),
        }
    }

    /// Commit the most recent `recv()` as processed. For the disk
    /// backend this advances the persisted cursor and reclaims
    /// fully-consumed segments — the actual durability hook. For the
    /// memory backend this is a no-op (mpsc removes events on
    /// `recv()`; there is no separate persistent cursor to advance).
    ///
    /// The consumer is expected to call `ack()` after every event's
    /// final disposition is decided — delivered or given up on
    /// (= "dropped" with retries exhausted). Both dispositions mean
    /// the event no longer needs to live in the queue. Skipping the
    /// call is safe (= the next call's progress covers it) but
    /// unnecessarily defers cursor commits.
    pub fn ack(&mut self) {
        if let ReceiverInner::Disk(rx) = &mut self.inner {
            rx.ack();
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

/// Run a queue consumer that drains events and writes them to an output.
///
/// Takes `Arc<dyn Output>` directly: after this change the `Output` trait is
/// already dyn-safe (the lifetime-bound `render` method was removed in
/// the trait collapse), so the earlier adapter trait that wrapped it
/// purely to hide that lifetime is no longer needed.
#[allow(clippy::too_many_arguments)]
pub async fn run_queue_consumer(
    mut receiver: QueueReceiver,
    writer: Arc<dyn crate::modules::Output>,
    retry_config: RetryConfig,
    tap: Option<crate::tap::TapRegistry>,
    metrics: Arc<crate::metrics::OutputMetrics>,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let name = Arc::clone(&receiver.name);
    info!("output '{}': queue consumer started", name);

    loop {
        tokio::select! {
            biased;

            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!("output '{}': shutting down, draining queue", name);
                    drain_remaining(&mut receiver, writer.as_ref(), &retry_config, &name, &metrics, tap.as_ref(), error_log.as_ref()).await;
                    break;
                }
            }

            input = receiver.recv() => {
                match input {
                    Some(event) => {
                        // `error_log` recovery is performed inside
                        // `write_with_retry`; the disposition is
                        // ignored here because the caller-side
                        // per-disposition metrics breakdown is not yet
                        // implemented (deferred to the 0.8 metrics
                        // rework).
                        let _ = write_with_retry(writer.as_ref(), event, &retry_config, &name, &metrics, tap.as_ref(), error_log.as_ref()).await;
                        // Acknowledge the event regardless of
                        // disposition (delivered or retries exhausted):
                        // from this queue's POV the event has been
                        // processed. For disk queues this is the
                        // durability commit — `recv()` returned the
                        // event with an in-memory cursor only, the
                        // persisted cursor advances here. Skipping
                        // the call on a crash gives at-least-once
                        // replay on restart.
                        receiver.ack();
                    }
                    None => {
                        info!("output '{}': queue closed", name);
                        break;
                    }
                }
            }
        }
    }

    // Batched sinks hold an in-memory buffer that queue-side `write()`
    // has already counted as delivered. Drop alone would abort the
    // flush timer and leak those events; tell the output to drain
    // itself before we exit. Both break paths above (shutdown signal
    // and queue-closed) come through here so the contract is uniform.
    if let Err(e) = writer.shutdown(error_log.as_ref()).await {
        warn!("output '{}': shutdown flush failed: {}", name, e);
    }

    info!("output '{}': queue consumer stopped", name);
}

#[allow(clippy::too_many_arguments)]
async fn drain_remaining(
    receiver: &mut QueueReceiver,
    writer: &dyn crate::modules::Output,
    retry_config: &RetryConfig,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
    tap: Option<&crate::tap::TapRegistry>,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
) {
    let mut count = 0u64;
    while let Some(event) = receiver.try_recv() {
        // Disposition ignored: same rationale as the steady-state
        // loop — drain just needs to ack each event.
        let _ = write_with_retry(
            writer,
            event,
            retry_config,
            name,
            metrics,
            tap,
            error_log,
        )
        .await;
        // Mirror the steady-state ack contract: each event's
        // disposition is committed to disk before we move on, so a
        // crash mid-drain still replays exactly the un-acked tail.
        receiver.ack();
        count += 1;
    }
    if count > 0 {
        info!(
            "output '{}': drained {} events during shutdown",
            name, count
        );
    }
}

/// Returns a [`WriteDisposition`] indicating whether the event was
/// delivered, persisted to `error_log` for recovery, or dropped.
///
/// Retry semantics (after this change): the queue always carries `Event` so
/// every attempt re-runs against the same event up to `max_attempts`.
/// Render failures (signalled by [`crate::modules::RenderError`])
/// bypass retries and go straight to DLQ — the render output is
/// deterministic on the event, so retrying would only repeat the same
/// failure. Transport / write failures continue to consume the retry
/// budget as before.
#[allow(clippy::too_many_arguments)]
async fn write_with_retry(
    writer: &dyn crate::modules::Output,
    event: Event,
    config: &RetryConfig,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
    tap: Option<&crate::tap::TapRegistry>,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
) -> WriteDisposition {
    use std::sync::atomic::Ordering;

    if let Some(tap) = tap {
        tap.emit(&format!("output {}", name), &event).await;
    }

    let mut attempt = 0u32;
    let mut wait = config.initial_wait;

    loop {
        match writer.consume(&event).await {
            Ok(()) => return WriteDisposition::Delivered,
            Err(e) => {
                // Render errors are deterministic on the event — no
                // amount of retrying will salvage a broken template.
                // Route straight to DLQ with a distinct `reason` so
                // operators can tell render failures apart from
                // transport exhaustion.
                let is_render_err = e.downcast_ref::<crate::modules::RenderError>().is_some();

                attempt += 1;
                if !is_render_err {
                    metrics.retries.fetch_add(1, Ordering::Relaxed);
                }

                if is_render_err || attempt >= config.max_attempts {
                    if is_render_err {
                        error!("output '{}': render failed: {}", name, e);
                    } else {
                        error!(
                            "output '{}': write failed after {} attempts: {}",
                            name, attempt, e
                        );
                    }
                    metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                    let reason = if is_render_err {
                        format!("render failed: {}", e)
                    } else {
                        format!("output write failed after {} attempts: {}", attempt, e)
                    };
                    if let Some(writer) = error_log {
                        let ctx = crate::pipeline::ErroredEventContext {
                            timestamp: chrono::Utc::now(),
                            pipeline: String::new(),
                            process: format!("(output {})", name),
                            reason,
                            event: event.clone(),
                        };
                        match writer.write(&ctx).await {
                            Ok(()) => return WriteDisposition::DroppedToRecovery,
                            Err(write_err) => {
                                warn!(
                                    "output '{}': error_log write failed: {} — dropping event",
                                    name, write_err
                                );
                            }
                        }
                    } else {
                        // No recovery path configured.
                        error!("output '{}': dropping event (no error_log)", name);
                    }
                    return WriteDisposition::Dropped;
                }
                warn!(
                    "output '{}': write failed (attempt {}/{}): {} — retrying in {:?}",
                    name, attempt, config.max_attempts, e, wait
                );
                tokio::time::sleep(wait).await;
                wait = match config.backoff {
                    BackoffStrategy::Exponential => (wait * 2).min(config.max_wait),
                    BackoffStrategy::Fixed => wait,
                };
            }
        }
    }
}

#[cfg(test)]
mod write_with_retry_tests {
    use super::*;
    use crate::modules::{HasMetrics, Output};
    use bytes::Bytes;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    /// Programmable mock: each call to `consume` pops the next result
    /// from `script`; if the script is empty, returns Ok. Records the
    /// number of consume invocations for assertions.
    struct ScriptedWriter {
        script: Mutex<Vec<anyhow::Result<()>>>,
        calls: std::sync::atomic::AtomicUsize,
        metrics: Arc<crate::metrics::OutputMetrics>,
    }

    impl ScriptedWriter {
        fn new(script: Vec<anyhow::Result<()>>) -> Self {
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
        async fn consume(&self, _event: &Event) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut s = self.script.lock().unwrap();
            if s.is_empty() { Ok(()) } else { s.remove(0) }
        }
    }

    fn fast_cfg(max_attempts: u32) -> RetryConfig {
        RetryConfig {
            max_attempts,
            initial_wait: std::time::Duration::from_millis(1),
            max_wait: std::time::Duration::from_millis(1),
            backoff: BackoffStrategy::Fixed,
        }
    }

    fn owned_event() -> Event {
        Event::new(
            Bytes::from_static(b"x"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn fresh_metrics() -> crate::metrics::OutputMetrics {
        crate::metrics::OutputMetrics::default()
    }

    #[tokio::test]
    async fn owned_succeeds_on_first_attempt_counts_no_retries() {
        let w = ScriptedWriter::new(vec![Ok(())]);
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &w,
            owned_event(),
            &fast_cfg(3),
            "test",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Delivered);
        assert_eq!(w.calls(), 1);
        assert_eq!(m.retries.load(Ordering::Relaxed), 0);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn owned_retries_then_succeeds() {
        let w = ScriptedWriter::new(vec![
            Err(anyhow::anyhow!("transient 1")),
            Err(anyhow::anyhow!("transient 2")),
            Ok(()),
        ]);
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &w,
            owned_event(),
            &fast_cfg(5),
            "test",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Delivered);
        assert_eq!(w.calls(), 3, "expected 1 + 2 retries");
        assert_eq!(m.retries.load(Ordering::Relaxed), 2);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn owned_exhausts_retries_bumps_events_failed() {
        let w = ScriptedWriter::new(vec![
            Err(anyhow::anyhow!("permanent")),
            Err(anyhow::anyhow!("permanent")),
            Err(anyhow::anyhow!("permanent")),
        ]);
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &w,
            owned_event(),
            &fast_cfg(3),
            "test",
            &m,
            None,
            None,
        )
        .await;
        // No error_log configured -> Dropped.
        assert_eq!(disposition, WriteDisposition::Dropped);
        assert_eq!(w.calls(), 3);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
        assert_eq!(m.retries.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn queue_sender_send_returns_channel_closed_when_receiver_dropped() {
        // Memory queue with a dropped receiver — send must surface
        // QueueSendError::ChannelClosed, not silently succeed.
        let (tx, rx) = create_queue(
            "mem".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 4,
                overflow: OverflowStrategy::Block,
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

    /// Writer that always returns a `RenderError`-wrapped error — used
    /// to drive the "render-err goes straight to DLQ" path.
    struct RenderFailWriter {
        metrics: Arc<crate::metrics::OutputMetrics>,
    }

    impl RenderFailWriter {
        fn new() -> Self {
            Self {
                metrics: Arc::new(crate::metrics::OutputMetrics::default()),
            }
        }
    }

    impl HasMetrics for RenderFailWriter {
        type Stats = crate::metrics::OutputMetrics;
        fn metrics(&self) -> Arc<crate::metrics::OutputMetrics> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for RenderFailWriter {
        async fn consume(&self, _event: &Event) -> anyhow::Result<()> {
            Err(crate::modules::RenderError::new(anyhow::anyhow!(
                "intentional render failure"
            ))
            .into())
        }
    }

    #[tokio::test]
    async fn render_err_goes_straight_to_dlq() {
        // Render errors bypass retry and route directly to recovery:
        // a RenderError from `consume` bypasses the
        // retry budget and lands directly in the DLQ. Exactly 1 call,
        // 0 retries counted, events_failed += 1, JSONL record's reason
        // contains "render failed".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("errored.jsonl");
        let el = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &RenderFailWriter::new(),
            owned_event(),
            &fast_cfg(5),
            "primary",
            &m,
            None,
            Some(&el),
        )
        .await;
        assert_eq!(disposition, WriteDisposition::DroppedToRecovery);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
        assert_eq!(
            m.retries.load(Ordering::Relaxed),
            0,
            "render error must not count as a retry"
        );
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["process"], "(output primary)");
        assert!(
            v["reason"].as_str().unwrap().contains("render failed"),
            "reason should mark render failure, got: {}",
            v["reason"]
        );
    }

    #[tokio::test]
    async fn render_err_with_no_error_log_returns_dropped() {
        // Same render-err shape but no DLQ configured: behave like the
        // legacy unrecoverable drop path — `Dropped`, events_failed += 1,
        // no retries.
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &RenderFailWriter::new(),
            owned_event(),
            &fast_cfg(5),
            "primary",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Dropped);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
        assert_eq!(m.retries.load(Ordering::Relaxed), 0);
    }

    // ---- retry-exhausted recovery routing ----

    /// Stub writer that *always* refuses the write — every test below
    /// drives the retry-exhaustion path, so the underlying writer just
    /// has to fail predictably without any per-test scripting.
    struct AlwaysFailWriter {
        metrics: Arc<crate::metrics::OutputMetrics>,
    }

    impl AlwaysFailWriter {
        fn new() -> Self {
            Self {
                metrics: Arc::new(crate::metrics::OutputMetrics::default()),
            }
        }
    }

    impl HasMetrics for AlwaysFailWriter {
        type Stats = crate::metrics::OutputMetrics;
        fn metrics(&self) -> Arc<crate::metrics::OutputMetrics> {
            Arc::clone(&self.metrics)
        }
    }

    #[async_trait::async_trait]
    impl Output for AlwaysFailWriter {
        async fn consume(&self, _event: &Event) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("permanent failure"))
        }
    }

    fn error_log_in(dir: &tempfile::TempDir) -> (Arc<crate::error_log::ErrorLogWriter>, PathBuf) {
        let path = dir.path().join("errored.jsonl");
        (
            Arc::new(crate::error_log::ErrorLogWriter::new(path.clone())),
            path,
        )
    }

    use std::path::PathBuf;

    #[tokio::test]
    async fn error_log_persists_and_returns_dropped_to_recovery() {
        // Retry-exhausted recovery happy path: retries exhaust, `error_log` captures the
        // payload. Must report `DroppedToRecovery` and the JSONL file
        // must contain one record carrying the original ingress.
        let dir = tempfile::tempdir().unwrap();
        let (el, path) = error_log_in(&dir);
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &AlwaysFailWriter::new(),
            owned_event(),
            &fast_cfg(2),
            "primary",
            &m,
            None,
            Some(&el),
        )
        .await;
        assert_eq!(disposition, WriteDisposition::DroppedToRecovery);
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 1, "expected exactly one JSONL record");
        let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["process"], "(output primary)");
        assert!(v["reason"].as_str().unwrap().contains("permanent failure"));
        assert!(v["event"].get("ingress").is_some());
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn no_error_log_preserves_existing_dropped_behavior() {
        // Boundary-contract regression anchor for the 0.7.7 baseline: with no
        // `error_log` configured, retry-exhaustion must still surface
        // `Dropped`. The warn line is observable in logs but not
        // asserted here.
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &AlwaysFailWriter::new(),
            owned_event(),
            &fast_cfg(2),
            "primary",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Dropped);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn error_log_write_failure_falls_back_to_dropped_without_recursion() {
        // Last-resort recovery path: error_log is configured but its
        // parent dir does not exist, so `write()` returns Err. The
        // function must warn and return `Dropped` (NOT
        // `DroppedToRecovery`), and must not retry the error_log write
        // or panic.
        let dir = tempfile::tempdir().unwrap();
        // Point at a path whose parent is missing — write() will fail
        // at open time. ErrorLogWriter::new doesn't validate eagerly.
        let bad_path = dir.path().join("missing-subdir/errored.jsonl");
        let el = Arc::new(crate::error_log::ErrorLogWriter::new(bad_path));
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &AlwaysFailWriter::new(),
            owned_event(),
            &fast_cfg(2),
            "primary",
            &m,
            None,
            Some(&el),
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Dropped);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
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
