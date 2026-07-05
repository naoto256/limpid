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
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
}

impl Module for StdoutOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(STDOUT_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        Ok(Self {
            name: name.to_string(),
            retry,
            error_log: ctx.error_log.as_ref().map(Arc::clone),
            metrics: Arc::new(OutputMetrics::default()),
            shutdown_signal: ctx.shutdown_signal.clone(),
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
    /// `io::stdout().lock()` + `write_all` rather than the
    /// `println!` macro: `println!` panics on a broken pipe (the
    /// canonical case is `limpid | head` where `head` exits before
    /// draining stdout), which would tear down the daemon mid-run.
    /// The locked-write path surfaces the I/O error here instead so
    /// the retry / DLQ path in `consume` decides the disposition.
    ///
    /// The bytes are written verbatim; using `writeln!` on a lossy
    /// `String::from_utf8_lossy` view would silently replace
    /// non-UTF-8 payload bytes with U+FFFD (`\xEF\xBF\xBD`) — a
    /// silent corruption on a security telemetry pipeline that can
    /// carry binary payloads.
    ///
    /// Payload bytes and the trailing `\n` are concatenated into a
    /// single buffer and written with one `write_all` call. This
    /// does NOT make the frame atomic — `write_all` internally loops
    /// over `write(2)` and can still leave partial bytes if the
    /// underlying writer errors mid-frame — but it removes the
    /// second `write_all` boundary that could silently succeed
    /// on the payload and then fail on the delimiter, leaving an
    /// unterminated line that a retry would double.
    fn write_event(&self, event: &Event) -> Result<()> {
        use std::io::Write;
        let buf = super::frame_with_newline(&event.egress);
        let mut out = std::io::stdout().lock();
        out.write_all(&buf).context("stdout write failed")?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl Output for StdoutOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
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
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            &self.metrics,
                            &self.name,
                            event,
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
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
                    // Race the backoff sleep against shutdown. If the runtime
                    // signals shutdown mid-sleep, do NOT keep retrying — the
                    // retry budget (default 1+2+4+8 = 15 s) can outlast the
                    // runtime's 10 s shutdown budget, and if we don't return
                    // the queue consumer's select! never gets back to its
                    // shutdown arm. Route the pending event to DLQ, resolve
                    // `Recovered`, and return.
                    if crate::modules::sleep_or_shutdown(&mut shutdown, wait).await {
                        let reason = format!(
                            "output write failed and shutdown observed mid-retry \
                             after {} attempts: {}",
                            attempt, e
                        );
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            &self.metrics,
                            &self.name,
                            event,
                            &reason,
                        )
                        .await;
                        crate::modules::resolve_ack_from_dlq_outcome(
                            ack,
                            __dlq_outcome,
                            &self.metrics,
                        );
                        return Ok(());
                    }
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
                let __dlq_outcome = crate::modules::route_event_to_dlq(
                    self.error_log.as_ref(),
                    &self.metrics,
                    &self.name,
                    event,
                    &reason,
                )
                .await;
                crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
            }
        }
        Ok(())
    }
}
