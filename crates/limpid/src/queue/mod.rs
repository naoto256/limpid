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
    pub async fn send(&self, event: Event) -> bool {
        let ok = match &self.inner {
            SenderInner::Memory(tx) => tx.send(event).await.is_ok(),
            SenderInner::Disk(tx) => tx.send(event).await,
        };
        if ok && let Some(m) = &self.metrics {
            m.events_received
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        ok
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
/// Uses `async_trait` for dyn compatibility (required by plugin registry).
#[async_trait::async_trait]
pub trait OutputWriter: Send + Sync + 'static {
    async fn write(&self, event: &Event) -> anyhow::Result<()>;
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
                    drain_remaining(&mut receiver, writer.as_ref(), &retry_config, &secondary_sender, &name, &metrics).await;
                    break;
                }
            }

            event = receiver.recv() => {
                match event {
                    Some(evt) => {
                        if let Some(ref tap) = tap {
                            tap.emit(&format!("output {}", name), &evt).await;
                        }
                        write_with_retry(writer.as_ref(), &evt, &retry_config, &secondary_sender, &name, &metrics).await;
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
) {
    let mut count = 0u64;
    while let Some(event) = receiver.try_recv() {
        write_with_retry(
            writer,
            &event,
            retry_config,
            secondary_sender,
            name,
            metrics,
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
async fn write_with_retry(
    writer: &dyn OutputWriter,
    event: &Event,
    config: &RetryConfig,
    secondary_sender: &Option<QueueSender>,
    name: &str,
    metrics: &crate::metrics::OutputMetrics,
) -> bool {
    use std::sync::atomic::Ordering;

    let mut attempt = 0u32;
    let mut wait = config.initial_wait;

    loop {
        match writer.write(event).await {
            Ok(()) => return true,
            Err(e) => {
                attempt += 1;
                metrics.retries.fetch_add(1, Ordering::Relaxed);
                if attempt >= config.max_attempts {
                    error!(
                        "output '{}': write failed after {} attempts: {}",
                        name, attempt, e
                    );
                    metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                    // Send to secondary output if configured
                    if let Some(secondary) = secondary_sender {
                        if !secondary.send(event.clone()).await {
                            error!("output '{}': secondary output also failed", name);
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
            }
        }
    }
}
