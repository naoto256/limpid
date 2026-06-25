//! Stdout output: prints event messages to standard output (debugging/testing).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;

use crate::dsl::schema::PropertySpec;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output, OutputBuilderWithErrorLog};
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
    ) -> Result<Self> {
        Self::from_properties_with_error_log(name, properties, None)
    }
}

impl OutputBuilderWithErrorLog for StdoutOutput {
    fn from_properties_with_error_log(
        name: &str,
        properties: &crate::modules::ModuleProperties,
        error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<Self> {
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        Ok(Self {
            name: name.to_string(),
            retry,
            error_log,
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
    /// loop without duplicating the print. Returns `Err` only for
    /// genuine I/O failures; stdout is unbuffered for our purposes so
    /// this almost always succeeds.
    fn write_event(&self, event: &Event) -> Result<()> {
        let msg = String::from_utf8_lossy(&event.egress);
        println!("{}", msg);
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
}
