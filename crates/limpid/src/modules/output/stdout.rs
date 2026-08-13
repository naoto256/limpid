//! Stdout output: prints event messages to standard output (debugging/testing).

use std::sync::Arc;

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
    error_log_fallback: crate::error_log::ErrorLogFallback,
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
            error_log_fallback: ctx.error_log_fallback,
            metrics: OutputMetrics::register(&ctx.metrics, name)?,
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
        let mut out = std::io::stdout().lock();
        self.write_event_to(&mut out, event)
    }

    /// Private writer seam: `write_event` locks stdout and delegates
    /// here so the confirmed-bytes contract stays testable against
    /// an arbitrary `impl Write`. The counter bump follows the
    /// `write_all` `?`, so a partial or failing write leaves the
    /// counter untouched.
    fn write_event_to(&self, out: &mut impl std::io::Write, event: &Event) -> Result<()> {
        let buf = super::frame_with_newline(&event.egress);
        out.write_all(&buf).context("stdout write failed")?;
        self.metrics.bytes_written.inc_by(buf.len() as u64);
        Ok(())
    }

    /// Retry-loop seam shared by `consume` and its test scaffolding.
    /// Production callers pass a closure that delegates to
    /// `write_event`; test callers pass a scripted closure so
    /// retry / shutdown / gauge state can be exercised without an
    /// actual stdout write.
    async fn consume_with_write<F>(
        &self,
        event: &Event,
        ack: QueueAckHandle,
        mut write: F,
    ) -> Result<()>
    where
        F: FnMut(&Event) -> Result<()> + Send,
    {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            match write(event) {
                Ok(()) => {
                    self.metrics.in_retry.set(0);
                    self.metrics.events_written.inc();
                    ack.resolve_delivered();
                    return Ok(());
                }
                Err(e) => {
                    attempt += 1;
                    self.metrics.retries.inc();
                    if attempt >= self.retry.max_attempts {
                        self.metrics.in_retry.set(0);
                        let reason =
                            format!("output write failed after {} attempts: {}", attempt, e);
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
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
                    self.metrics.in_retry.set(1);
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
                        self.metrics.in_retry.set(0);
                        let reason = format!(
                            "output write failed and shutdown observed mid-retry \
                             after {} attempts: {}",
                            attempt, e
                        );
                        let __dlq_outcome = crate::modules::route_event_to_dlq(
                            self.error_log.as_ref(),
                            self.error_log_fallback,
                            &self.metrics,
                            &self.name,
                            event,
                            ack.position(),
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
}

#[async_trait::async_trait]
impl Output for StdoutOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        self.consume_with_write(event, ack, |event| self.write_event(event))
            .await
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        // `write_event` is a synchronous write with no metric side
        // effects, so the success + failure paths reduce to the
        // canonical shape `finalize_shutdown_singleton_disposition`
        // handles: bump events_written on Ok, route to DLQ on Err.
        // Delegating removes the inline `route_event_to_dlq` +
        // `resolve_ack_from_dlq_outcome` pair and lines up with the
        // sibling sinks (syslog_tcp, syslog_udp, unix_socket, kafka)
        // that already use the helper.
        crate::modules::finalize_shutdown_singleton_disposition(
            self.write_event(event),
            self.error_log.as_ref(),
            self.error_log_fallback,
            &self.metrics,
            &self.name,
            event,
            ack,
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod metrics_registration_tests {
    use super::*;
    use crate::dsl::module_props::ModuleProperties;
    use crate::metrics::{MetricsError, OutputMetrics, Registry};

    fn assert_output_duplicate(error: &MetricsError, label_value: &str, diagnostic: &str) {
        let (name, labelset) = match error {
            MetricsError::DuplicateSeries { name, labelset } => (name, labelset),
            other => panic!("expected DuplicateSeries, got {other:?}"),
        };
        assert!(
            [
                "limpid_output_events_received_total",
                "limpid_output_events_injected_total",
                "limpid_output_events_written_total",
                "limpid_output_events_failed_total",
                "limpid_output_retries_total",
                "limpid_output_events_wedged_total",
                "limpid_output_events_errored_unwritable_total",
                "limpid_output_bytes_written_total",
            ]
            .contains(&name.as_str())
        );
        assert_eq!(labelset, &[("output".to_owned(), label_value.to_owned())]);
        assert!(diagnostic.contains(&format!("name={name:?}")));
        assert!(diagnostic.contains(&format!("labelset={labelset:?}")));
    }

    #[test]
    fn factory_uses_the_shared_registry_and_propagates_registration_conflicts() {
        let registry = Arc::new(Registry::new());
        OutputMetrics::register(&registry, "conflicting")
            .expect("preseeded output metrics must register");
        let mut ctx = crate::modules::BuildContext::for_testing();
        ctx.metrics = Arc::clone(&registry);
        let properties = ModuleProperties::from_parts("stdout", Vec::new());

        let error = match StdoutOutput::from_properties("conflicting", &properties, &ctx) {
            Ok(_) => panic!("factory unexpectedly swallowed the registration conflict"),
            Err(error) => error,
        };
        let diagnostic = format!("{error:#}");
        let metrics_error = error
            .chain()
            .find_map(|source| source.downcast_ref::<MetricsError>())
            .unwrap_or_else(|| {
                panic!(
                    "MetricsError must remain downcastable in the anyhow source chain: {error:#}"
                )
            });
        assert_output_duplicate(metrics_error, "conflicting", &diagnostic);
    }

    #[tokio::test]
    async fn successful_write_counts_payload_and_adapter_newline_once() {
        let ctx = crate::modules::BuildContext::for_testing();
        let properties = ModuleProperties::from_parts("stdout", Vec::new());
        let output = StdoutOutput::from_properties("stdout-bytes", &properties, &ctx).unwrap();
        let payload = bytes::Bytes::from_static(b"stdout-payload");
        let event = crate::event::Event::new(payload.clone(), "127.0.0.1:0".parse().unwrap());
        let (ack, _ack_rx) = crate::queue::QueueAckHandle::for_test();

        output.consume(&event, ack).await.unwrap();

        assert_eq!(
            output
                .metrics
                .bytes_written
                .load(std::sync::atomic::Ordering::Relaxed),
            (payload.len() + 1) as u64
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "event and byte counters remain independent"
        );
    }

    #[test]
    fn writer_seam_counts_only_a_fully_confirmed_stdout_frame() {
        use std::io::{self, Write};

        struct AlwaysFails;

        impl Write for AlwaysFails {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "test failure"))
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let ctx = crate::modules::BuildContext::for_testing();
        let properties = ModuleProperties::from_parts("stdout", Vec::new());
        let output = StdoutOutput::from_properties("stdout-writer", &properties, &ctx).unwrap();
        let event = crate::event::Event::new(
            bytes::Bytes::from_static(b"stdout-payload"),
            "127.0.0.1:0".parse().unwrap(),
        );

        let mut written = Vec::new();
        output.write_event_to(&mut written, &event).unwrap();
        assert_eq!(written, b"stdout-payload\n");
        assert_eq!(
            output
                .metrics
                .bytes_written
                .load(std::sync::atomic::Ordering::Relaxed),
            written.len() as u64
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            0,
            "the private write seam owns bytes, not event disposition"
        );

        let mut failing = AlwaysFails;
        output
            .write_event_to(&mut failing, &event)
            .expect_err("an unconfirmed stdout write must fail");
        assert_eq!(
            output
                .metrics
                .bytes_written
                .load(std::sync::atomic::Ordering::Relaxed),
            written.len() as u64,
            "a failed write must not add any bytes"
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_retry_exposes_backoff_then_clears_on_success() {
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = Arc::new(StdoutOutput {
            name: "stdout-retry".to_owned(),
            retry: RetryConfig {
                max_attempts: 2,
                initial_wait: std::time::Duration::from_millis(50),
                max_wait: std::time::Duration::from_millis(50),
                backoff: crate::queue::BackoffStrategy::Fixed,
            },
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            metrics: OutputMetrics::for_testing(),
            shutdown_signal: shutdown_rx,
        });
        let event = crate::event::Event::new(
            bytes::Bytes::from_static(b"retry"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (failed_tx, failed_rx) = tokio::sync::oneshot::channel();
        let failed_tx = Arc::new(std::sync::Mutex::new(Some(failed_tx)));
        let task_output = Arc::clone(&output);
        let task_attempts = Arc::clone(&attempts);
        let task = tokio::spawn(async move {
            task_output
                .consume_with_write(&event, ack, move |_| {
                    let attempt = task_attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if attempt == 0 {
                        failed_tx.lock().unwrap().take().unwrap().send(()).unwrap();
                        anyhow::bail!("scripted first failure");
                    }
                    Ok(())
                })
                .await
        });

        failed_rx.await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        tokio::time::advance(output.retry.initial_wait).await;
        task.await.unwrap().unwrap();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            output
                .metrics
                .retries
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, crate::queue::AckDisposition::Delivered))
        ));
    }

    #[tokio::test]
    async fn active_shutdown_clears_scripted_retry_state() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let output = Arc::new(StdoutOutput {
            name: "stdout-shutdown".to_owned(),
            retry: RetryConfig {
                max_attempts: 3,
                initial_wait: std::time::Duration::from_secs(5),
                max_wait: std::time::Duration::from_secs(5),
                backoff: crate::queue::BackoffStrategy::Fixed,
            },
            error_log: None,
            error_log_fallback: crate::error_log::ErrorLogFallback::default(),
            metrics: OutputMetrics::for_testing(),
            shutdown_signal: shutdown_rx,
        });
        let event = crate::event::Event::new(
            bytes::Bytes::from_static(b"shutdown"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _ack_rx) = QueueAckHandle::for_test();
        let (failed_tx, failed_rx) = tokio::sync::oneshot::channel();
        let failed_tx = Arc::new(std::sync::Mutex::new(Some(failed_tx)));
        let task_output = Arc::clone(&output);
        let task = tokio::spawn(async move {
            task_output
                .consume_with_write(&event, ack, move |_| {
                    if let Some(tx) = failed_tx.lock().unwrap().take() {
                        tx.send(()).unwrap();
                    }
                    anyhow::bail!("scripted persistent failure")
                })
                .await
        });
        failed_rx.await.unwrap();
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        shutdown_tx.send(true).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .expect("shutdown must stop retry")
            .unwrap()
            .unwrap();
        assert_eq!(
            output
                .metrics
                .in_retry
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            output
                .metrics
                .events_written
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}
