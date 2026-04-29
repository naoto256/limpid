//! Unix socket output: sends event messages to a Unix domain socket.
//! Maintains a persistent connection with automatic reconnection on failure.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::output::persistent_conn::{PersistentConn, write_with_reconnect};
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};

struct UnixSocketPayload {
    egress: Bytes,
}

pub struct UnixSocketOutput {
    pub path: PathBuf,
    conn: Mutex<Option<UnixStream>>,
    metrics: Arc<OutputMetrics>,
}

impl Module for UnixSocketOutput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let path = props::get_string(properties, "path")
            .ok_or_else(|| anyhow::anyhow!("output '{}': unix_socket requires 'path'", name))?;
        Ok(Self {
            path: PathBuf::from(path),
            conn: Mutex::new(None),
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
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(UnixSocketPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: UnixSocketPayload = payload.downcast()?;
        write_with_reconnect(self, &self.conn, &self.metrics, &payload.egress).await
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
