//! Unix socket output: sends event messages to a Unix domain socket.
//! Maintains a persistent connection with automatic reconnection on failure.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{FromProperties, HasMetrics, Output};

pub struct UnixSocketOutput {
    pub path: PathBuf,
    conn: Mutex<Option<UnixStream>>,
    metrics: Arc<OutputMetrics>,
}

impl FromProperties for UnixSocketOutput {
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
    async fn write(&self, event: &Event) -> Result<()> {
        let mut guard = self.conn.lock().await;

        // Try existing connection
        if guard.is_some() {
            match self.write_to(guard.as_mut().unwrap(), event).await {
                Ok(()) => {
                    self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
                Err(_) => {
                    *guard = None;
                }
            }
        }

        // (Re)connect and write
        let stream = UnixStream::connect(&self.path)
            .await
            .with_context(|| format!("unix_socket connect to {}", self.path.display()))?;
        *guard = Some(stream);

        self.write_to(guard.as_mut().unwrap(), event).await?;
        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

impl UnixSocketOutput {
    async fn write_to(&self, stream: &mut UnixStream, event: &Event) -> Result<()> {
        let msg = String::from_utf8_lossy(&event.message);
        stream.write_all(msg.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;
        Ok(())
    }
}
