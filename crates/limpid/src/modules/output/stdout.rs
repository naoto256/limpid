//! Stdout output: prints event messages to standard output (debugging/testing).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};

use crate::dsl::schema::PropertySpec;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};

/// `output stdout` has no module-specific properties; only the
/// common `retry { ... } / queue { ... }` sub-blocks apply.
const STDOUT_OUTPUT_SCHEMA: &[PropertySpec] = &[
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

pub struct StdoutOutput {
    name: String,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    metrics: Arc<OutputMetrics>,
}

impl Module for StdoutOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(STDOUT_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::modules::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        Ok(Self {
            name: name.to_string(),
            retry,
            error_log: ctx.error_log.as_ref().map(Arc::clone),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for StdoutOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

impl StdoutOutput {
    /// Pure write step — extracted so `consume` can drive the retry
    /// loop without duplicating the print. Goes through
    /// `io::stdout().lock()` + `writeln!` rather than the `println!`
    /// macro: `println!` panics on a broken pipe (the canonical case
    /// is `limpid | head` where `head` exits before draining stdout),
    /// which would tear down the daemon mid-run. The locked-write
    /// path surfaces the I/O error here instead so the retry / DLQ
    /// path in `consume` decides the disposition.
    fn write_event(&self, event: &Event) -> Result<()> {
        use std::io::Write;
        let msg = String::from_utf8_lossy(&event.egress);
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", msg).context("stdout write failed")?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Output for StdoutOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        loop {
            match self.write_event(event) {
                Ok(()) => {
                    self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
                    ack.resolve_delivered();
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.fetch_add(1, Ordering::Relaxed);
                    if attempt >= self.retry.max_attempts {
                        let reason =
                            format!("output write failed after {} attempts: {}", attempt, e);
                        crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            &self.name,
                            event,
                            &reason,
                        )
                        .await;
                        self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        ack.resolve_recovered();
                        return Ok(());
                    }
                    tracing::warn!(
                        "output '{}': write failed (attempt {}/{}): {} — retrying in {:?}",
                        self.name,
                        attempt,
                        self.retry.max_attempts,
                        e,
                        wait
                    );
                    tokio::time::sleep(wait).await;
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        match self.write_event(event) {
            Ok(()) => {
                self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
                ack.resolve_delivered();
            }
            Err(e) => {
                let reason = format!("shutdown write failed: {}", e);
                crate::modules::route_event_to_dlq(
                    self.error_log.as_ref(),
                    &self.name,
                    event,
                    &reason,
                )
                .await;
                self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                ack.resolve_recovered();
            }
        }
        Ok(())
    }
}
