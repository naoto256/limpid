//! Output queue: async FIFO between pipeline and output modules.
//!
//! Two implementations:
//! - **Memory queue** (default): fast, events lost on process restart
//! - **Disk queue**: WAL-based, survives restarts, configurable max size

pub mod disk;

use std::sync::Arc;

use tracing::{error, info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::modules::RenderedPayload;

// ---------------------------------------------------------------------------
// SinkInput — what flows over the per-output queue
// ---------------------------------------------------------------------------
//
// v0.6.0 Step B: pipeline → output sink transport carries either a
// pre-rendered, sink-specific payload (memory-queue hot path) or an
// `OwnedEvent` (disk-queue persist, control-socket inject — cold paths
// where the event must be serializable). The pipeline picks at the
// output statement based on each output's queue type.

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
    pub async fn send(&self, input: SinkInput) -> bool {
        let ok = match (&self.inner, input) {
            (SenderInner::Memory(tx), input) => tx.send(input).await.is_ok(),
            (SenderInner::Disk(tx), SinkInput::Owned(ev)) => tx.send(ev).await,
            (SenderInner::Disk(_), SinkInput::Rendered(_)) => {
                error!(
                    "queue '{}': pipeline routed a Rendered payload to a disk-persist queue \
                     — this is a programmer bug; dropping event",
                    self.name
                );
                false
            }
        };
        if ok && let Some(m) = &self.metrics {
            m.events_received
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        ok
    }

    /// Convenience: send an `OwnedEvent` regardless of queue kind.
    /// Used by the cold paths (control-socket inject, retry secondary)
    /// that already hold an owned event and don't go through the
    /// render path.
    pub async fn send_owned(&self, event: Event) -> bool {
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

/// Trait for output writers usable in queue consumers.
///
/// The queue carries `SinkInput`, which is either a `Rendered` payload
/// (built by the pipeline via `Output::render`) or an `Owned` event
/// (disk-queue replay, control-socket inject — paths that bypass
/// pipeline rendering). Implementors dispatch on the variant.
///
/// Uses `async_trait` for dyn compatibility (required by plugin registry).
#[async_trait::async_trait]
pub trait OutputWriter: Send + Sync + 'static {
    async fn consume(&self, input: SinkInput) -> anyhow::Result<()>;
}

/// Run a queue consumer that drains events and writes them to an output.
pub async fn run_queue_consumer(
    mut receiver: QueueReceiver,
    writer: Box<dyn OutputWriter>,
    retry_config: RetryConfig,
    secondary_sender: Option<QueueSender>,
    tap: Option<crate::tap::TapRegistry>,
    metrics: Arc<crate::metrics::OutputMetrics>,
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
                    drain_remaining(&mut receiver, writer.as_ref(), &retry_config, &secondary_sender, &name, &metrics, tap.as_ref()).await;
                    break;
                }
            }

            input = receiver.recv() => {
                match input {
                    Some(input) => {
                        write_with_retry(writer.as_ref(), input, &retry_config, &secondary_sender, &name, &metrics, tap.as_ref()).await;
                    }
                    None => {
                        info!("output '{}': queue closed", name);
                        break;
                    }
                }
            }
        }
    }

    info!("output '{}': queue consumer stopped", name);
}

async fn drain_remaining(
    receiver: &mut QueueReceiver,
    writer: &dyn OutputWriter,
    retry_config: &RetryConfig,
    secondary_sender: &Option<QueueSender>,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
    tap: Option<&crate::tap::TapRegistry>,
) {
    let mut count = 0u64;
    while let Some(input) = receiver.try_recv() {
        write_with_retry(
            writer,
            input,
            retry_config,
            secondary_sender,
            name,
            metrics,
            tap,
        )
        .await;
        count += 1;
    }
    if count > 0 {
        info!(
            "output '{}': drained {} events during shutdown",
            name, count
        );
    }
}

/// Returns true on success, false if event was dropped/sent to secondary.
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
async fn write_with_retry(
    writer: &dyn OutputWriter,
    input: SinkInput,
    config: &RetryConfig,
    secondary_sender: &Option<QueueSender>,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
    tap: Option<&crate::tap::TapRegistry>,
) -> bool {
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
            Ok(()) => return true,
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
                    if let Some(secondary) = secondary_sender {
                        if let Some(ev) = owned_for_retry.take() {
                            if !secondary.send(SinkInput::Owned(ev)).await {
                                error!("output '{}': secondary output also failed", name);
                            }
                        } else {
                            error!(
                                "output '{}': cannot route to secondary — original payload was Rendered (memory queue)",
                                name
                            );
                        }
                    } else {
                        error!("output '{}': dropping event (no secondary)", name);
                    }
                    return false;
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
    false
}
