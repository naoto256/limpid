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
                    crate::modules::resolve_ack_from_dlq_outcome(ack, __dlq_outcome, &self.metrics);
                    return Ok(());
                }
            };
            match write_result {
                Ok(()) => {
                    // Metric ownership stays with the caller so
                    // `finalize_shutdown_singleton_disposition` on the
                    // shutdown-drain path (which also owns the
                    // success bump) does not double-count against the
                    // transport helper. Sibling syslog_udp uses the
                    // identical shape after the previous fix.
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
        // `write_with_reconnect` may cross the byte-on-wire
        // boundary: a stream-oriented connection can accept the
        // first frame bytes into the kernel socket buffer and even
        // ship them before a mid-write disconnect or the outer
        // `tokio::time::timeout` fires. Once the outer Elapsed
        // fires — or any inner Err arrives out of a reconnect
        // loop — the wire state is ambiguous, so route through the
        // `_ambiguous` finalizer: force `Dropped` (disk queue
        // wedges for next-start replay, memory queue falls back to
        // `Recovered`) and never fabricate the honest-Recovered
        // guarantee the transport does not support. See
        // `persistent_conn::PersistentConn::write_frame` for the
        // partial-send documentation this discipline is enforcing.
        let result = match tokio::time::timeout(
            crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            write_with_reconnect(self, &self.conn, &event.egress),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!(
                "timed out after {:?}",
                crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT
            )),
        };
        crate::modules::finalize_shutdown_singleton_disposition_ambiguous(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::queue::QueueAckHandle;
    use bytes::Bytes;
    use std::os::unix::net::UnixListener as StdUnixListener;
    use tempfile::TempDir;

    /// Steady-state `consume` success bumps `events_written` exactly
    /// once. Regression against a metric-ownership drift that would
    /// double-count with the transport helper or leave it at zero.
    #[tokio::test]
    async fn steady_state_consume_success_bumps_events_written_once() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("out.sock");
        let listener = StdUnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _listener = tokio::net::UnixListener::from_std(listener).unwrap();

        let output = build_test_output(&socket_path).await;
        let event = Event::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _rx) = QueueAckHandle::for_test();
        output.consume(&event, ack).await.expect("consume");

        assert_eq!(
            output.metrics.events_written.load(Ordering::Relaxed),
            1,
            "steady-state consume success must bump events_written exactly once"
        );
        assert_eq!(
            output.metrics.events_failed.load(Ordering::Relaxed),
            0,
            "successful send must not bump events_failed"
        );
    }

    /// Shutdown-drain success via `finalize_shutdown_singleton_disposition`
    /// bumps `events_written` exactly once (previously double: once
    /// inside `write_with_reconnect` and once in the helper).
    #[tokio::test]
    async fn shutdown_consume_success_bumps_events_written_once() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("out.sock");
        let listener = StdUnixListener::bind(&socket_path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _listener = tokio::net::UnixListener::from_std(listener).unwrap();

        let output = build_test_output(&socket_path).await;
        let event = Event::new(
            Bytes::from_static(b"payload"),
            "127.0.0.1:0".parse().unwrap(),
        );
        let (ack, _rx) = QueueAckHandle::for_test();
        output
            .consume_shutdown(&event, ack)
            .await
            .expect("consume_shutdown");

        assert_eq!(
            output.metrics.events_written.load(Ordering::Relaxed),
            1,
            "consume_shutdown success must bump events_written exactly once (via helper)"
        );
        assert_eq!(
            output.metrics.events_failed.load(Ordering::Relaxed),
            0,
            "successful drain must not bump events_failed"
        );
    }

    /// Structural pin: `consume_shutdown` routes its result through
    /// the `_ambiguous` finalizer, not the plain
    /// `finalize_shutdown_singleton_disposition`. The plain variant
    /// resolves shutdown-time send failures as `Recovered` — which
    /// on a disk queue advances the cursor past a DLQ record even
    /// though the payload's bytes may already have been written to
    /// the peer socket (kernel buffer, first frame partially on
    /// wire, or reconnect-mid-write). That produces a double-state:
    /// the downstream received part or all of the record AND the
    /// DLQ record is available for replay. The `_ambiguous`
    /// variant force-Dropped-so-wedged, holding the cursor for
    /// operator reconciliation on next start.
    #[test]
    fn consume_shutdown_uses_ambiguous_finalizer() {
        let src = include_str!("unix_socket.rs");
        let start = src
            .find("async fn consume_shutdown(")
            .expect("consume_shutdown fn must exist");
        let end_offset = src[start..]
            .find("\n}\n")
            .expect("consume_shutdown body must end with a top-level closing brace");
        let body = &src[start..start + end_offset];
        assert!(
            body.contains("finalize_shutdown_singleton_disposition_ambiguous("),
            "consume_shutdown must route through the _ambiguous finalizer to hold the disk \
             cursor on partial-wire failures — the plain variant would honest-Recovered and \
             risk downstream duplicates on next-start replay."
        );
        assert!(
            !body.contains("crate::modules::finalize_shutdown_singleton_disposition("),
            "consume_shutdown must not fall back to the plain finalizer — the wire state \
             cannot be proved pre-boundary from outside the write helper."
        );
    }

    async fn build_test_output(socket_path: &std::path::Path) -> UnixSocketOutput {
        use crate::dsl::ast::{Expr, ExprKind, Property};
        use crate::dsl::module_props::ModuleProperties;
        let props = ModuleProperties::from_parts(
            "unix_socket",
            vec![Property::KeyValue {
                key: "path".into(),
                key_span: None,
                value: Expr::spanless(ExprKind::StringLit(
                    socket_path.to_str().unwrap().to_string(),
                )),
                value_span: None,
            }],
        );
        UnixSocketOutput::from_properties("u", &props, &crate::modules::BuildContext::for_testing())
            .expect("build")
    }
}
