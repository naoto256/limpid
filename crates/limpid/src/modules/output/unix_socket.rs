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
use crate::modules::output::persistent_conn::{PersistentConn, write_with_reconnect};
use crate::modules::{HasMetrics, Module, Output, OutputBuilderWithErrorLog};
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
}

impl Module for UnixSocketOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(UNIX_SOCKET_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        Self::from_properties_with_error_log(name, properties, None)
    }
}

impl OutputBuilderWithErrorLog for UnixSocketOutput {
    fn from_properties_with_error_log(
        name: &str,
        properties: &crate::modules::ModuleProperties,
        error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
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
            error_log,
            metrics: Arc::new(OutputMetrics::default()),
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
        loop {
            match write_with_reconnect(self, &self.conn, &self.metrics, &event.egress).await {
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

#[async_trait::async_trait]
impl PersistentConn for UnixSocketOutput {
    type Stream = UnixStream;

    async fn connect(&self) -> Result<UnixStream> {
        UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("unix_socket connect to {}", self.path.display()))
    }

    async fn write_frame(&self, stream: &mut UnixStream, payload: &Bytes) -> Result<()> {
        let msg = String::from_utf8_lossy(payload);
        stream.write_all(msg.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        Ok(())
    }
}
