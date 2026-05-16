//! UDP output: sends event messages as UDP datagrams.
//!
//! Properties:
//!   address   "10.0.0.1:514"   — required (host:port)

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::OnceCell;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};

const UDP_OUTPUT_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "address",
    required: true,
    kind: PropertyValueKind::String,
}];

struct UdpPayload {
    egress: Bytes,
}

pub struct UdpOutput {
    address: String,
    /// Lazily bound socket (bound once on first write)
    socket: OnceCell<UdpSocket>,
    metrics: Arc<OutputMetrics>,
}

impl Module for UdpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(UDP_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        // Schema marks `address` required; this `ok_or_else` is the
        // defensive path for direct `from_properties` callers that
        // skip the registry / `build` validation step.
        let address = props::get_string(properties, "address")
            .ok_or_else(|| anyhow::anyhow!("output '{}': udp requires 'address'", name))?;
        Ok(Self {
            address,
            socket: OnceCell::new(),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for UdpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for UdpOutput {
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(UdpPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: UdpPayload = payload.downcast()?;
        let socket = self
            .socket
            .get_or_try_init(|| async {
                let sock = UdpSocket::bind("0.0.0.0:0")
                    .await
                    .context("udp output: failed to bind ephemeral socket")?;
                sock.connect(&self.address).await.with_context(|| {
                    format!("udp output: failed to connect to {}", self.address)
                })?;
                Ok::<_, anyhow::Error>(sock)
            })
            .await?;

        socket
            .send(&payload.egress)
            .await
            .with_context(|| format!("udp output: send to {}", self.address))?;

        self.metrics.events_written.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind};

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    #[test]
    fn build_accepts_address() {
        let props = vec![kv("address", ExprKind::StringLit("h:1".into()))];
        let u = UdpOutput::build("u", &props).expect("ok");
        assert_eq!(u.address, "h:1");
    }

    #[test]
    fn build_rejects_missing_address() {
        let err = UdpOutput::build("u", &[]).err().expect("missing address");
        assert!(err.to_string().contains("address"));
    }

    #[test]
    fn build_rejects_unknown_key_with_did_you_mean() {
        let props = vec![kv("adress", ExprKind::StringLit("h:1".into()))];
        let err = UdpOutput::build("u", &props).err().expect("typo");
        let msg = err.to_string();
        assert!(msg.contains("adress") && msg.contains("address"), "{}", msg);
    }
}
