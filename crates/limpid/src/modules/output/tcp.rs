//! TCP output: sends event messages to a remote TCP endpoint.
//! Supports octet counting (RFC 6587) and non-transparent framing.
//!
//! Maintains a persistent connection with automatic reconnection on failure.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::output::persistent_conn::{PersistentConn, write_with_reconnect};
use crate::modules::{HasMetrics, Module, ModuleSchema, Output};

pub struct TcpOutput {
    pub address: String,
    pub framing: TcpOutputFraming,
    conn: Mutex<Option<TcpStream>>,
    metrics: Arc<OutputMetrics>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpOutputFraming {
    OctetCounting,
    NonTransparent,
}

impl Module for TcpOutput {
    fn schema() -> ModuleSchema {
        ModuleSchema::default()
    }

    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let address = props::get_string(properties, "address")
            .or_else(|| {
                let host = props::get_string(properties, "host")?;
                let port = props::get_int(properties, "port").unwrap_or(514);
                Some(format!("{}:{}", host, port))
            })
            .ok_or_else(|| {
                anyhow::anyhow!("output '{}': tcp requires 'address' or 'host'+'port'", name)
            })?;
        let framing = match props::get_ident(properties, "framing").as_deref() {
            Some("non_transparent") => TcpOutputFraming::NonTransparent,
            _ => TcpOutputFraming::OctetCounting,
        };
        Ok(Self {
            address,
            framing,
            conn: Mutex::new(None),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for TcpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for TcpOutput {
    async fn write(&self, event: &Event) -> Result<()> {
        write_with_reconnect(self, &self.conn, &self.metrics, event).await
    }
}

#[async_trait::async_trait]
impl PersistentConn for TcpOutput {
    type Stream = TcpStream;

    async fn connect(&self) -> Result<TcpStream> {
        TcpStream::connect(&self.address)
            .await
            .with_context(|| format!("tcp connect to {}", self.address))
    }

    async fn write_frame(&self, stream: &mut TcpStream, event: &Event) -> Result<()> {
        let msg = &event.egress;

        match self.framing {
            TcpOutputFraming::OctetCounting => {
                let header = format!("{} ", msg.len());
                stream.write_all(header.as_bytes()).await?;
                stream.write_all(msg).await?;
            }
            TcpOutputFraming::NonTransparent => {
                stream.write_all(msg).await?;
                stream.write_all(b"\n").await?;
            }
        }

        stream.flush().await?;
        Ok(())
    }
}
