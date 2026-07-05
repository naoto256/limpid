//! Unix socket output: sends event messages to a Unix domain socket.
//! Maintains a persistent connection with automatic reconnection on failure.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::output::persistent_conn::{
    PersistentConn, WriteReconnectOutcome, write_with_reconnect,
    write_with_reconnect_shutdown_aware,
};
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};

const UNIX_SOCKET_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "path",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

pub struct UnixSocketOutput {
    name: String,
    pub path: PathBuf,
    conn: Mutex<Option<UnixStream>>,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    metrics: Arc<OutputMetrics>,
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
}

impl Module for UnixSocketOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(UNIX_SOCKET_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let retry = RetryConfig::from_output_properties(properties.user_properties())?;
        let properties = properties.user_properties();
        let path = props::get_string(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("output '{}': unix_socket requires 'path'", name))?;
        Ok(Self {
            name: name.to_string(),
            path: PathBuf::from(path),
            conn: Mutex::new(None),
            retry,
            error_log: ctx.error_log.as_ref().map(Arc::clone),
            metrics: Arc::new(OutputMetrics::default()),
            shutdown_signal: ctx.shutdown_signal.clone(),
        })
    }
}

impl HasMetrics for UnixSocketOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for UnixSocketOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            // Split the attempt into a pre-send phase (mutex lock +
            // reconnect + pre-write shutdown check) and a send phase
            // (`write_frame`). Shutdown only cancels the pre-send
            // side; a partial write that had already reached the
            // wire would otherwise be masked as `Recovered` and
            // double-sent on the next start's retry.
            let write_result = match write_with_reconnect_shutdown_aware(
                self,
                &self.conn,
                &self.metrics,
                &event.egress,
                &mut shutdown,
            )
            .await
            {
                WriteReconnectOutcome::Delivered => Ok(()),
                WriteReconnectOutcome::Err(e) => Err(e),
                WriteReconnectOutcome::PreSendShutdown => {
                    let reason = format!(
                        "output '{}': write attempt abandoned on shutdown (pre-send)",
                        self.name
                    );
                    let __dlq_outcome = crate::modules::route_event_to_dlq(
                        self.error_log.as_ref(),
                        &self.metrics,
                        &self.name,
                        event,
                        &reason,
                    )
                    .await;
                    self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                    crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome);
                    return Ok(());
                }
            };
            match write_result {
                Ok(()) => {
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
                        self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome);
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
                        self.metrics.events_failed.fetch_add(1, Ordering::Relaxed);
                        crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome);
                        return Ok(());
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        let result = match tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            write_with_reconnect(self, &self.conn, &self.metrics, &event.egress),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "timed out after {:?}",
                crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT
            )),
        };
        crate::modules::finalize_shutdown_singleton_disposition(
            result,
            self.error_log.as_ref(),
            &self.metrics,
            &self.name,
            event,
            ack,
        )
        .await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl PersistentConn for UnixSocketOutput {
    type Stream = UnixStream;

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("unix_socket connect to {}", self.path.display()))
    }

    async fn write_frame(&self, stream: &mut UnixStream, payload: &Bytes) -> Result<()> {
        // Write the payload verbatim. `String::from_utf8_lossy`
        // would silently replace non-UTF-8 bytes with U+FFFD
        // (`\xEF\xBF\xBD`), which is exactly the wrong default for
        // a security telemetry pipeline shipping opaque payloads
        // to a local collector.
        //
        // Payload bytes and the trailing `\n` are concatenated into
        // one buffer and handed to a single `write_all`, so an I/O
        // error between the payload and the delimiter — which the
        // previous two-`write_all` shape could produce as
        // `Ok(payload) → Err(delimiter)` — no longer leaves an
        // unterminated line for the retry / DLQ path to double.
        // `write_all` still loops internally, so this is a boundary
        // narrowing, not an atomicity guarantee.
        let buf = super::frame_with_newline(payload);
        stream.write_all(&buf).await?;
        stream.flush().await?;
        Ok(())
    }
}
