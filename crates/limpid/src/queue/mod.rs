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
use crate::modules::RenderedPayload;

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
// Declarative schema for the per-output `retry { ... }` sub-block and
// the `secondary <name>` property
// ---------------------------------------------------------------------------
//
// Both surfaces are honored by `RetryConfig::from_output_properties`
// (below) for *every* output the queue consumer drives — the runtime
// has always read `retry` and `secondary` uniformly. Historically only
// the two OTLP outputs declared them in their `property_schema()`, so
// non-OTLP configs that set `retry { ... }` or `secondary <name>` got
// hard-failed at `--check` time as "unknown property" even though the
// runtime would happily accept them. Hoisting the schema here and
// splicing the two consts into every output's schema closes that gap
// without touching the runtime — schema acceptance now matches the
// runtime's existing intent.
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

/// `secondary <name>` property specification shared by every output
/// Module's property schema. Splice alongside [`QUEUE_PROPERTY_SPEC`].
///
/// The value is read by `props::get_ident` at runtime, which only
/// accepts a bare ident — `secondary "fallback"` (string literal) or
/// `secondary $tpl` (template) would silently return `None`, disabling
/// the fallback route without any schema-level signal (I-4 violation
/// caught by the boundary-contract audit). `PropertyValueKind::Ident`
/// rejects those shapes at `--check` time so the operator sees the
/// mismatch up front. The runtime still resolves the resulting ident
/// against the set of declared output names and rejects unknowns /
/// self-references / cycles in `runtime.rs`.
pub const SECONDARY_PROPERTY_SPEC: PropertySpec = PropertySpec {
    name: "secondary",
    required: false,
    repeatable: false,
    exclusive_group: None,
    kind: PropertyValueKind::Ident,
};

// ---------------------------------------------------------------------------
// SinkInput — what flows over the per-output queue
// ---------------------------------------------------------------------------
//
// The pipeline → output sink transport carries either a pre-rendered,
// sink-specific payload (memory-queue hot path) or an `OwnedEvent`
// (disk-queue persist, control-socket inject — cold paths where the
// event must be serializable). The pipeline picks at the output
// statement based on each output's queue type.

/// Item carried by an output queue.
pub enum SinkInput {
    /// Disk-queue persist / inject path. Serialisable, outlives the
    /// pipeline's per-event arena.
    Owned(Event),
    /// Memory-queue hot path. Type-erased payload built by
    /// `Output::render`; the matching `Output::write` downcasts it.
    Rendered(RenderedPayload),
}

/// Memory-vs-disk queue discriminator surfaced on `CompiledConfig` so
/// the pipeline can pick `SinkInput::Owned` (disk persist) vs
/// `SinkInput::Rendered` (memory hot path) at the `output` statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    Memory,
    Disk,
}

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
    /// Light-weight scan: peek at an output's properties and return
    /// the queue kind without parsing capacities or paths. Used at
    /// `CompiledConfig` build time to populate the per-output queue
    /// kind map driving pipeline output dispatch.
    pub fn kind_from_output_properties(output_props: &[Property]) -> QueueKind {
        if let Some(block) = props::get_block(output_props, "queue")
            && matches!(props::get_ident(block, "type").as_deref(), Some("disk"))
        {
            QueueKind::Disk
        } else {
            QueueKind::Memory
        }
    }

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
    name: Arc<String>,
    #[allow(dead_code)] // surfaced via `kind()` for future memory/disk-aware callers
    kind: QueueKind,
    /// Optional metrics — if set, `send()` increments `events_received` on success.
    /// Set by the runtime after the output module's metrics handle is available.
    metrics: Option<Arc<crate::metrics::OutputMetrics>>,
}

#[derive(Clone)]
enum SenderInner {
    Memory(tokio::sync::mpsc::Sender<SinkInput>),
    Disk(disk::DiskQueueSender),
}

impl QueueSender {
    /// Memory or disk discriminator. The pipeline reads this to decide
    /// between the render hot-path (memory) and the owned/serialise
    /// path (disk).
    #[allow(dead_code)] // currently consumed via the `kind` map on CompiledConfig
    pub fn kind(&self) -> QueueKind {
        self.kind
    }

    /// Send a `SinkInput` into the queue.
    ///
    /// Disk queues only accept `SinkInput::Owned(...)` because the
    /// `Rendered` variant holds a `Box<dyn Any>` payload which has no
    /// serialisable shape. Pipeline-output dispatch (`pipeline.rs`)
    /// already gates this by inspecting `kind()` at the output
    /// statement; the `Rendered`-on-Disk arm here is a defence-in-depth
    /// log+drop so a programmer mistake elsewhere doesn't silently
    /// corrupt the persist path.
    pub async fn send(&self, input: SinkInput) -> Result<(), QueueSendError> {
        let result: Result<(), QueueSendError> = match (&self.inner, input) {
            (SenderInner::Memory(tx), input) => {
                tx.send(input).await.map_err(|_| QueueSendError::ChannelClosed)
            }
            (SenderInner::Disk(tx), SinkInput::Owned(ev)) => tx.send(ev).await,
            (SenderInner::Disk(_), SinkInput::Rendered(_)) => {
                error!(
                    "queue '{}': pipeline routed a Rendered payload to a disk-persist queue \
                     — this is a programmer bug; dropping event",
                    self.name
                );
                Err(QueueSendError::RenderedOnDisk)
            }
        };
        if let Some(m) = &self.metrics {
            if result.is_ok() {
                m.events_received
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            } else {
                // Enqueue failure: memory-queue receiver dropped (=
                // consumer task gone, daemon usually shutting down),
                // disk-queue serialise/write error, or
                // Rendered-on-Disk routing bug above. From this
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

    /// Convenience: send an `OwnedEvent` regardless of queue kind.
    /// Used by the cold paths (control-socket inject, retry secondary)
    /// that already hold an owned event and don't go through the
    /// render path.
    pub async fn send_owned(&self, event: Event) -> Result<(), QueueSendError> {
        self.send(SinkInput::Owned(event)).await
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
    Memory(tokio::sync::mpsc::Receiver<SinkInput>),
    Disk(disk::DiskQueueReceiver),
}

impl QueueReceiver {
    pub async fn recv(&mut self) -> Option<SinkInput> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.recv().await,
            ReceiverInner::Disk(rx) => rx.recv().await.map(SinkInput::Owned),
        }
    }

    pub fn try_recv(&mut self) -> Option<SinkInput> {
        match &mut self.inner {
            ReceiverInner::Memory(rx) => rx.try_recv().ok(),
            ReceiverInner::Disk(rx) => rx.try_recv().map(SinkInput::Owned),
        }
    }

    /// Commit the most recent `recv()` as processed. For the disk
    /// backend this advances the persisted cursor and reclaims
    /// fully-consumed segments — the actual durability hook. For the
    /// memory backend this is a no-op (mpsc removes events on
    /// `recv()`; there is no separate persistent cursor to advance).
    ///
    /// The consumer is expected to call `ack()` after every event's
    /// final disposition is decided — delivered, routed to secondary,
    /// or given up on (= "dropped" with retries exhausted). All three
    /// dispositions mean the event no longer needs to live in the
    /// queue. Skipping the call is safe (= the next call's progress
    /// covers it) but unnecessarily defers cursor commits.
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
                    kind: QueueKind::Memory,
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
                    kind: QueueKind::Disk,
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
    /// Name of secondary output to send events that exhaust retries.
    pub secondary: Option<String>,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_wait: std::time::Duration::from_secs(1),
            max_wait: std::time::Duration::from_secs(60),
            backoff: BackoffStrategy::Exponential,
            secondary: None,
        }
    }
}

impl RetryConfig {
    /// Parse from an output definition's properties (retry block + secondary).
    pub fn from_output_properties(output_props: &[Property]) -> anyhow::Result<Self> {
        let mut config = Self {
            secondary: props::get_ident(output_props, "secondary"),
            ..Self::default()
        };

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

/// Narrow `dyn`-safe adapter the queue consumer holds onto an output
/// through. Lives separately from [`Output`] (in `modules::mod`) on
/// purpose: `Output` carries `render`, which takes a `BorrowedEvent`
/// tied to the per-event arena — that lifetime makes the trait object
/// non-object-safe for the queue's `Box<dyn _>` storage, and the
/// hot-path `render` isn't called from the queue side anyway.
///
/// Currently the only implementer is `modules::OutputWriterWrapper`,
/// which forwards `consume` to the underlying `Arc<dyn Output>`. The
/// trait is intentionally 1-impl: it exists as a **dyn-safety
/// boundary**, not as an extension point, and stays that shape until
/// a second writer kind appears (e.g. a side-channel sink that
/// consumes `SinkInput` without going through a full `Output`).
///
/// `SinkInput` discriminates between a pipeline-rendered payload
/// (memory-queue hot path) and an `OwnedEvent` (disk-queue replay,
/// control-socket inject). Implementors dispatch on the variant.
#[async_trait::async_trait]
pub trait OutputWriter: Send + Sync + 'static {
    async fn consume(&self, input: SinkInput) -> anyhow::Result<()>;

    /// Drain any buffered events before the consumer stops. Forwarded
    /// to the underlying `Output::shutdown` for `OutputWriterWrapper`;
    /// default no-op for any future writer kind that holds no
    /// internal buffer.
    ///
    /// `error_log` is the DLQ writer the consumer was launched with
    /// — batched outputs use it to persist buffer entries that
    /// survive a failed final flush (BC-4 / PR-P).
    async fn shutdown(
        &self,
        _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Run a queue consumer that drains events and writes them to an output.
#[allow(clippy::too_many_arguments)]
pub async fn run_queue_consumer(
    mut receiver: QueueReceiver,
    writer: Box<dyn OutputWriter>,
    retry_config: RetryConfig,
    secondary_sender: Option<QueueSender>,
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
                    drain_remaining(&mut receiver, writer.as_ref(), &retry_config, &secondary_sender, &name, &metrics, tap.as_ref(), error_log.as_ref()).await;
                    break;
                }
            }

            input = receiver.recv() => {
                match input {
                    Some(input) => {
                        // Fan-out (secondary routing + error_log
                        // recovery) is performed inside
                        // `write_with_retry`; the disposition is
                        // ignored here because the caller-side
                        // per-disposition metrics breakdown is not yet
                        // implemented (deferred to the 0.8 metrics
                        // rework).
                        let _ = write_with_retry(writer.as_ref(), input, &retry_config, &secondary_sender, &name, &metrics, tap.as_ref(), error_log.as_ref()).await;
                        // Acknowledge the event regardless of
                        // disposition (delivered, routed to
                        // secondary, or retries exhausted): from
                        // this queue's POV the event has been
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
    writer: &dyn OutputWriter,
    retry_config: &RetryConfig,
    secondary_sender: &Option<QueueSender>,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
    tap: Option<&crate::tap::TapRegistry>,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
) {
    let mut count = 0u64;
    while let Some(input) = receiver.try_recv() {
        // Disposition ignored: same rationale as the steady-state
        // loop — drain just needs to ack each event.
        let _ = write_with_retry(
            writer,
            input,
            retry_config,
            secondary_sender,
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
/// delivered, routed to the secondary queue, or dropped.
///
/// Retry semantics:
/// - `SinkInput::Owned(event)` is cloneable, so each attempt re-runs the
///   write with the same event up to `max_attempts`.
/// - `SinkInput::Rendered(payload)` consumes the (`Box<dyn Any>`)
///   payload on the first call into `OutputWriter::consume` and is not
///   re-buildable from the consumer, so on failure we fall through
///   to the secondary path immediately. Operators who need full retry
///   semantics on a sink should configure a disk queue (which always
///   carries `SinkInput::Owned`).
#[allow(clippy::too_many_arguments)]
async fn write_with_retry(
    writer: &dyn OutputWriter,
    input: SinkInput,
    config: &RetryConfig,
    secondary_sender: &Option<QueueSender>,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
    tap: Option<&crate::tap::TapRegistry>,
    error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
) -> WriteDisposition {
    use std::sync::atomic::Ordering;

    // Fast-split: extract the optional Owned event (used for tap emit
    // and retry/secondary fallback) without consuming the input we
    // hand to the writer on the first attempt.
    let mut owned_for_retry: Option<Event> = match &input {
        SinkInput::Owned(ev) => Some(ev.clone()),
        SinkInput::Rendered(_) => None,
    };

    if let Some(tap) = tap
        && let Some(ev) = &owned_for_retry
    {
        tap.emit(&format!("output {}", name), ev).await;
    }

    let mut next_attempt: Option<SinkInput> = Some(input);
    let mut attempt = 0u32;
    let mut wait = config.initial_wait;

    loop {
        let this = match next_attempt.take() {
            Some(i) => i,
            None => break,
        };
        let is_owned = matches!(this, SinkInput::Owned(_));
        match writer.consume(this).await {
            Ok(()) => return WriteDisposition::Delivered,
            Err(e) => {
                attempt += 1;
                metrics.retries.fetch_add(1, Ordering::Relaxed);
                if attempt >= config.max_attempts || !is_owned {
                    if !is_owned {
                        warn!(
                            "output '{}': write failed (rendered payload, no retry): {}",
                            name, e
                        );
                    } else {
                        error!(
                            "output '{}': write failed after {} attempts: {}",
                            name, attempt, e
                        );
                    }
                    metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                    // Recovery routing order (BC-3 / PR-O):
                    //   1. secondary configured + Owned available → try
                    //      to enqueue. Success returns
                    //      `RoutedToSecondary`; failure falls through
                    //      to the error_log step so the payload still
                    //      lands on disk.
                    //   2. error_log configured → write a JSONL record
                    //      mirroring the pipeline DLQ format. Success
                    //      returns `DroppedToRecovery`; failure warns
                    //      and falls through to `Dropped`.
                    //   3. neither path captured the payload → preserve
                    //      the existing 0.7.7 `Dropped` behaviour
                    //      (warn + give up). The original `Rendered`
                    //      case (no owned payload to capture) also
                    //      collapses here.
                    if let Some(secondary) = secondary_sender {
                        if let Some(ev) = owned_for_retry.as_ref() {
                            match secondary.send(SinkInput::Owned(ev.clone())).await {
                                Ok(()) => return WriteDisposition::RoutedToSecondary,
                                Err(err) => {
                                    error!(
                                        "output '{}': secondary output also failed: {}",
                                        name, err
                                    );
                                }
                            }
                        } else {
                            error!(
                                "output '{}': cannot route to secondary — original payload was Rendered (memory queue)",
                                name
                            );
                        }
                    }
                    if let (Some(writer), Some(ev)) = (error_log, owned_for_retry.take()) {
                        let ctx = crate::pipeline::ErroredEventContext {
                            timestamp: chrono::Utc::now(),
                            pipeline: String::new(),
                            process: format!("(output {})", name),
                            reason: format!("output write failed after {} attempts: {}", attempt, e),
                            event: ev,
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
                    } else if secondary_sender.is_none() {
                        // No recovery path of any kind configured.
                        error!(
                            "output '{}': dropping event (no secondary, no error_log)",
                            name
                        );
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
                // Rebuild the next-attempt input from the cloned owned
                // event we kept aside.
                if let Some(ev) = owned_for_retry.as_ref() {
                    next_attempt = Some(SinkInput::Owned(ev.clone()));
                } else {
                    break;
                }
            }
        }
    }
    // Loop ran out of next-attempt inputs without a return — treated
    // as dropped, matching the previous `false` fall-through.
    WriteDisposition::Dropped
}

#[cfg(test)]
mod write_with_retry_tests {
    use super::*;
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
    }

    impl ScriptedWriter {
        fn new(script: Vec<anyhow::Result<()>>) -> Self {
            Self {
                script: Mutex::new(script),
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl OutputWriter for ScriptedWriter {
        async fn consume(&self, _input: SinkInput) -> anyhow::Result<()> {
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
            secondary: None,
        }
    }

    fn owned_event() -> Event {
        Event::new(
            Bytes::from_static(b"x"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn rendered_event() -> SinkInput {
        SinkInput::Rendered(RenderedPayload::new(()))
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
            SinkInput::Owned(owned_event()),
            &fast_cfg(3),
            &None,
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
            SinkInput::Owned(owned_event()),
            &fast_cfg(5),
            &None,
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
            SinkInput::Owned(owned_event()),
            &fast_cfg(3),
            &None,
            "test",
            &m,
            None,
            None,
        )
        .await;
        // No secondary configured -> Dropped.
        assert_eq!(disposition, WriteDisposition::Dropped);
        assert_eq!(w.calls(), 3);
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
        assert_eq!(m.retries.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn rendered_failure_skips_retries_and_logs() {
        // Rendered payloads cannot be retried (the Box<dyn Any>
        // payload was consumed by the first consume call). The
        // retry-loop must take exactly one shot and then bump
        // events_failed once with the "rendered payload, no retry"
        // warn — never loop. A regression that mistakenly retries a
        // Rendered would panic or silently no-op since the closure
        // has nothing left to consume.
        let w = ScriptedWriter::new(vec![Err(anyhow::anyhow!("nope"))]);
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &w,
            rendered_event(),
            &fast_cfg(5), // 5 allowed but only 1 attempt should happen
            &None,
            "test",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Dropped);
        assert_eq!(
            w.calls(),
            1,
            "Rendered must NOT retry, got {} calls",
            w.calls()
        );
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn owned_exhausted_routes_to_secondary() {
        // After retries exhaust, the Owned event must reach the
        // secondary queue. A regression that dropped the secondary
        // routing or swapped the secondary->primary direction would
        // make the secondary silently dead.
        let w = ScriptedWriter::new(vec![Err(anyhow::anyhow!("e1")), Err(anyhow::anyhow!("e2"))]);
        let m = fresh_metrics();
        let (sec_tx, mut sec_rx) = create_queue(
            "secondary".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 8,
                overflow: OverflowStrategy::Block,
            },
        )
        .unwrap();
        let secondary = Some(sec_tx);
        let disposition = write_with_retry(
            &w,
            SinkInput::Owned(owned_event()),
            &fast_cfg(2),
            &secondary,
            "primary",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::RoutedToSecondary);
        let routed = sec_rx.try_recv();
        assert!(
            matches!(routed, Some(SinkInput::Owned(_))),
            "secondary did not receive the routed event"
        );
    }

    #[tokio::test]
    async fn rendered_exhausted_does_not_route_to_secondary() {
        // A Rendered failure with a configured secondary must NOT
        // route — there's no owned event to forward. The function
        // logs the "cannot route" error and drops. A regression that
        // sent SinkInput::Rendered to the secondary would corrupt the
        // secondary's invariants (only Owned is serialisable on disk
        // queues, and a Rendered on a memory secondary would still be
        // an orphan since the original payload was consumed).
        let w = ScriptedWriter::new(vec![Err(anyhow::anyhow!("rendered fail"))]);
        let m = fresh_metrics();
        let (sec_tx, mut sec_rx) = create_queue(
            "secondary".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 8,
                overflow: OverflowStrategy::Block,
            },
        )
        .unwrap();
        let secondary = Some(sec_tx);
        let _ = write_with_retry(
            &w,
            rendered_event(),
            &fast_cfg(5),
            &secondary,
            "primary",
            &m,
            None,
            None,
        )
        .await;
        assert!(sec_rx.try_recv().is_none(), "Rendered must NOT route");
        assert_eq!(m.events_failed.load(Ordering::Relaxed), 1);
    }

    // ---- type-encoded outcome contract (PR-L) ----

    #[tokio::test]
    async fn rendered_exhausted_with_secondary_yields_dropped() {
        // Rendered cannot be routed even when a secondary is
        // configured — the original payload was consumed. The
        // disposition must be `Dropped`, NOT `RoutedToSecondary`.
        let w = ScriptedWriter::new(vec![Err(anyhow::anyhow!("rendered fail"))]);
        let m = fresh_metrics();
        let (sec_tx, _sec_rx) = create_queue(
            "secondary".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 8,
                overflow: OverflowStrategy::Block,
            },
        )
        .unwrap();
        let disposition = write_with_retry(
            &w,
            rendered_event(),
            &fast_cfg(5),
            &Some(sec_tx),
            "primary",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Dropped);
    }

    #[tokio::test]
    async fn secondary_send_failure_yields_dropped() {
        // Owned event exhausts retries with a secondary configured —
        // but the secondary's receiver is dropped first, so the
        // secondary send itself fails. Disposition must collapse to
        // `Dropped` (not `RoutedToSecondary`), preserving the
        // historical bool=false behaviour while distinguishing it
        // from the happy fallback path.
        let w = ScriptedWriter::new(vec![Err(anyhow::anyhow!("e1")), Err(anyhow::anyhow!("e2"))]);
        let m = fresh_metrics();
        let (sec_tx, sec_rx) = create_queue(
            "secondary".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 8,
                overflow: OverflowStrategy::Block,
            },
        )
        .unwrap();
        drop(sec_rx); // close the secondary channel
        let disposition = write_with_retry(
            &w,
            SinkInput::Owned(owned_event()),
            &fast_cfg(2),
            &Some(sec_tx),
            "primary",
            &m,
            None,
            None,
        )
        .await;
        assert_eq!(disposition, WriteDisposition::Dropped);
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
            .send(SinkInput::Owned(owned_event()))
            .await
            .expect_err("send to closed memory channel must fail");
        assert!(
            matches!(err, QueueSendError::ChannelClosed),
            "expected ChannelClosed, got {:?}",
            err
        );
    }

    #[tokio::test]
    async fn queue_sender_send_returns_rendered_on_disk_when_misrouted() {
        // Routing a Rendered payload to a disk queue is a programmer
        // bug — the disk sink only accepts serialisable Owned events.
        // The send must return QueueSendError::RenderedOnDisk so the
        // pipeline-level DLQ path is taken.
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = create_queue(
            "disk".into(),
            QueueConfig {
                queue_type: QueueType::Disk {
                    path: dir.path().to_string_lossy().into_owned(),
                    max_size: 0,
                },
                capacity: 4,
                overflow: OverflowStrategy::Block,
            },
        )
        .unwrap();
        let err = tx
            .send(rendered_event())
            .await
            .expect_err("Rendered-on-Disk must fail");
        assert!(
            matches!(err, QueueSendError::RenderedOnDisk),
            "expected RenderedOnDisk, got {:?}",
            err
        );
    }

    // ---- BC-3 secondary-recovery routing (PR-O) ----

    /// Stub writer that *always* refuses the write — every test below
    /// drives the retry-exhaustion path, so the underlying writer just
    /// has to fail predictably without any per-test scripting.
    struct AlwaysFailWriter;

    #[async_trait::async_trait]
    impl OutputWriter for AlwaysFailWriter {
        async fn consume(&self, _input: SinkInput) -> anyhow::Result<()> {
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
    async fn no_secondary_with_error_log_persists_and_returns_dropped_to_recovery() {
        // BC-3 happy path: retries exhaust, no secondary configured,
        // but `error_log` captures the payload. Must report
        // `DroppedToRecovery` and the JSONL file must contain one
        // record carrying the original ingress.
        let dir = tempfile::tempdir().unwrap();
        let (el, path) = error_log_in(&dir);
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &AlwaysFailWriter,
            SinkInput::Owned(owned_event()),
            &fast_cfg(2),
            &None,
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
    async fn secondary_send_failure_with_error_log_falls_through_to_recovery() {
        // BC-3 fallback chain: secondary is configured but its receiver
        // is gone, so the secondary enqueue itself fails. The error_log
        // must catch the payload and the disposition must collapse to
        // `DroppedToRecovery` (not `Dropped`).
        let dir = tempfile::tempdir().unwrap();
        let (el, path) = error_log_in(&dir);
        let m = fresh_metrics();
        let (sec_tx, sec_rx) = create_queue(
            "secondary".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 8,
                overflow: OverflowStrategy::Block,
            },
        )
        .unwrap();
        drop(sec_rx);
        let disposition = write_with_retry(
            &AlwaysFailWriter,
            SinkInput::Owned(owned_event()),
            &fast_cfg(2),
            &Some(sec_tx),
            "primary",
            &m,
            None,
            Some(&el),
        )
        .await;
        assert_eq!(disposition, WriteDisposition::DroppedToRecovery);
        let body = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(body.lines().count(), 1);
    }

    #[tokio::test]
    async fn no_secondary_no_error_log_preserves_existing_dropped_behavior() {
        // BC-3 regression anchor for the 0.7.7 baseline: with neither
        // a secondary nor an error_log configured, retry-exhaustion
        // must still surface `Dropped`. The warn line is observable in
        // logs but not asserted here.
        let m = fresh_metrics();
        let disposition = write_with_retry(
            &AlwaysFailWriter,
            SinkInput::Owned(owned_event()),
            &fast_cfg(2),
            &None,
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
    async fn secondary_success_unchanged_when_error_log_present() {
        // Regression anchor: the recovery routing must not steal the
        // happy `RoutedToSecondary` path. When the secondary enqueue
        // succeeds, error_log must NOT receive a duplicate record even
        // if it is configured.
        let dir = tempfile::tempdir().unwrap();
        let (el, path) = error_log_in(&dir);
        let m = fresh_metrics();
        let (sec_tx, mut sec_rx) = create_queue(
            "secondary".into(),
            QueueConfig {
                queue_type: QueueType::Memory,
                capacity: 8,
                overflow: OverflowStrategy::Block,
            },
        )
        .unwrap();
        let disposition = write_with_retry(
            &AlwaysFailWriter,
            SinkInput::Owned(owned_event()),
            &fast_cfg(2),
            &Some(sec_tx),
            "primary",
            &m,
            None,
            Some(&el),
        )
        .await;
        assert_eq!(disposition, WriteDisposition::RoutedToSecondary);
        assert!(
            matches!(sec_rx.try_recv(), Some(SinkInput::Owned(_))),
            "secondary did not receive the routed event"
        );
        // error_log file must not exist (or be empty) — the secondary
        // captured the payload, recovery routing was a no-op.
        let exists = tokio::fs::try_exists(&path).await.unwrap_or(false);
        assert!(!exists, "error_log was written even though secondary succeeded");
    }

    #[tokio::test]
    async fn error_log_write_failure_falls_back_to_dropped_without_recursion() {
        // BC-3 last-resort path: error_log is configured but its
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
            &AlwaysFailWriter,
            SinkInput::Owned(owned_event()),
            &fast_cfg(2),
            &None,
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
// Pre-PR-T, only the two OTLP outputs declared `retry` and `secondary`
// in their `property_schema()` even though `RetryConfig::from_output_properties`
// reads both for *every* output. The tests below pin the post-PR-T
// invariant: every non-OTLP output's schema accepts a `retry { ... }`
// block and a `secondary <name>` property without raising
// `UnknownKey`. Adding a third output schema later is then a hard
// failure here if the splice is forgotten.
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

    fn secondary_prop() -> Property {
        kv("secondary", ExprKind::Ident(vec!["fallback".into()]))
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
        let props = vec![full_retry_block(), secondary_prop()];
        let errs = validate(&props, schema);
        let unknown = unknown_key_errs(&errs);
        assert!(
            unknown.is_empty(),
            "output '{}': retry / secondary should be accepted by schema, \
             got UnknownKey errors for: {:?}",
            output_name,
            unknown.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn every_output_schema_accepts_retry_and_secondary() {
        // The full matrix lives in one test so adding a new output
        // either splices the common consts into its schema or fails
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

    /// I-4 invariant guard: `secondary` is read at runtime by
    /// `props::get_ident`, which silently discards anything that
    /// isn't a bare ident. The schema must reject string literals
    /// and templates so the operator sees the mismatch at `--check`
    /// time rather than booting with a silently-disabled fallback
    /// route. Covers every output's shared schema in one pass.
    #[test]
    fn secondary_rejects_non_ident_shapes_in_all_outputs() {
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

        // Each bad shape must produce a TypeMismatch on the `secondary`
        // key — UnknownKey would mean the splice itself broke (covered
        // by the accept-test above), other errs are fine to ignore.
        let bad_shapes: Vec<(Property, &str)> = vec![
            (
                kv("secondary", ExprKind::StringLit("fallback".into())),
                "string literal",
            ),
            (
                kv(
                    "secondary",
                    ExprKind::Template(vec![
                        crate::dsl::ast::TemplateFragment::Literal("fall".into()),
                        crate::dsl::ast::TemplateFragment::Interp(Expr::spanless(
                            ExprKind::Ident(vec!["env".into(), "FB".into()]),
                        )),
                    ]),
                ),
                "template",
            ),
            (
                kv(
                    "secondary",
                    ExprKind::Ident(vec!["pkg".into(), "fallback".into()]),
                ),
                "multi-segment ident",
            ),
        ];

        for (schema, name) in &schemas {
            for (bad, shape_label) in &bad_shapes {
                let errs = validate(std::slice::from_ref(bad), schema);
                let mismatch = errs.iter().find(|e| {
                    e.key == "secondary"
                        && matches!(e.kind, SchemaErrorKind::TypeMismatch { .. })
                });
                assert!(
                    mismatch.is_some(),
                    "output '{}': secondary {} should be rejected by schema, \
                     got errs={:?}",
                    name,
                    shape_label,
                    errs.iter().map(|e| (&e.kind, &e.key)).collect::<Vec<_>>(),
                );
            }
        }
    }

    /// Companion to the negative test: bare-ident form (the only valid
    /// shape) and absence must both pass without a `secondary`-keyed
    /// error. Pins the post-fix accept range so the schema stays a
    /// faithful mirror of what `props::get_ident` reads at runtime.
    #[test]
    fn secondary_accepts_bare_ident_and_absence_in_all_outputs() {
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

        for (schema, name) in &schemas {
            // Bare ident — must pass.
            let with_ident = vec![secondary_prop()];
            let errs = validate(&with_ident, schema);
            let bad = errs.iter().find(|e| e.key == "secondary");
            assert!(
                bad.is_none(),
                "output '{}': bare-ident secondary should be accepted, got {:?}",
                name,
                bad,
            );

            // Absent — must pass.
            let empty: Vec<Property> = vec![];
            let errs = validate(&empty, schema);
            let bad = errs.iter().find(|e| e.key == "secondary");
            assert!(
                bad.is_none(),
                "output '{}': absent secondary should be accepted, got {:?}",
                name,
                bad,
            );
        }
    }

    /// `RetryConfig::from_output_properties` parses identical retry +
    /// secondary props on a non-OTLP output (kafka here) into the
    /// same fields the OTLP outputs have always populated. Anchors
    /// the "runtime behavior already matched, schema just caught up"
    /// invariant.
    #[test]
    fn retry_config_parser_reads_same_fields_for_non_otlp_outputs() {
        let props = vec![full_retry_block(), secondary_prop()];
        let cfg = RetryConfig::from_output_properties(&props)
            .expect("retry block parses for non-OTLP output");
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.initial_wait, std::time::Duration::from_millis(100));
        assert_eq!(cfg.max_wait, std::time::Duration::from_secs(5));
        assert!(matches!(cfg.backoff, BackoffStrategy::Exponential));
        assert_eq!(cfg.secondary.as_deref(), Some("fallback"));
    }
}
