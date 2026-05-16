//! TCP output: sends event messages to a remote TCP endpoint.
//! Supports octet counting (RFC 6587) and non-transparent framing.
//!
//! Maintains a persistent connection with automatic reconnection on failure.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::output::persistent_conn::{PersistentConn, write_with_reconnect};
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};

/// Declared property surface for `output tcp`. Either `address` (in
/// `host:port` form) or `host` + optional `port` is required, but the
/// schema layer can only enforce shape — the cross-field "one of the
/// two paths" rule lives in `from_properties` below.
const TCP_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "address",
        required: false,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "host",
        required: false,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "port",
        required: false,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "framing",
        required: false,
        kind: PropertyValueKind::Enum(&["octet_counting", "non_transparent"]),
    },
];

struct TcpPayload {
    egress: Bytes,
}

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
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(TCP_OUTPUT_SCHEMA)
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
        // After schema validation, `framing` is guaranteed to be one of
        // the declared enum values (or absent). The match is exhaustive
        // on the legal set; we still default to the documented default
        // for the `None` case.
        let framing = match props::get_ident(properties, "framing").as_deref() {
            Some("non_transparent") => TcpOutputFraming::NonTransparent,
            Some("octet_counting") | None => TcpOutputFraming::OctetCounting,
            Some(other) => {
                // Unreachable when reached through the registry, which
                // validates the schema first. Kept as a defensive
                // fallback for direct `from_properties` callers (tests,
                // snippet libs); upgrades the previous silent fallback
                // to an explicit error.
                anyhow::bail!(
                    "output '{}': unknown framing '{}' (expected octet_counting | non_transparent)",
                    name,
                    other
                );
            }
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
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(TcpPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: TcpPayload = payload.downcast()?;
        write_with_reconnect(self, &self.conn, &self.metrics, &payload.egress).await
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

    async fn write_frame(&self, stream: &mut TcpStream, payload: &Bytes) -> Result<()> {
        match self.framing {
            TcpOutputFraming::OctetCounting => {
                let header = format!("{} ", payload.len());
                stream.write_all(header.as_bytes()).await?;
                stream.write_all(payload).await?;
            }
            TcpOutputFraming::NonTransparent => {
                stream.write_all(payload).await?;
                stream.write_all(b"\n").await?;
            }
        }

        stream.flush().await?;
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
    fn build_accepts_minimal_valid_config() {
        let props = vec![kv("address", ExprKind::StringLit("127.0.0.1:514".into()))];
        let tcp = TcpOutput::build("relay", &props).expect("should build");
        assert_eq!(tcp.address, "127.0.0.1:514");
        assert_eq!(tcp.framing, TcpOutputFraming::OctetCounting);
    }

    #[test]
    fn build_accepts_correct_framing_enum_value() {
        let props = vec![
            kv("address", ExprKind::StringLit("h:1".into())),
            kv("framing", ExprKind::Ident(vec!["non_transparent".into()])),
        ];
        let tcp = TcpOutput::build("relay", &props).expect("should build");
        assert_eq!(tcp.framing, TcpOutputFraming::NonTransparent);
    }

    #[test]
    fn build_rejects_typoed_framing_with_did_you_mean() {
        let props = vec![
            kv("address", ExprKind::StringLit("h:1".into())),
            kv("framing", ExprKind::Ident(vec!["non_trasnaprent".into()])),
        ];
        let err = TcpOutput::build("relay", &props).err().expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("framing"), "{}", msg);
        assert!(msg.contains("non_transparent"), "did-you-mean missing: {}", msg);
    }

    #[test]
    fn build_rejects_unknown_key_with_did_you_mean() {
        let props = vec![
            kv("address", ExprKind::StringLit("h:1".into())),
            // typo of `framing` → should suggest `framing`
            kv("framming", ExprKind::Ident(vec!["octet_counting".into()])),
        ];
        let err = TcpOutput::build("relay", &props).err().expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("unknown property 'framming'"), "{}", msg);
        assert!(msg.contains("framing"), "did-you-mean missing: {}", msg);
    }

    #[test]
    fn build_rejects_wrong_value_type() {
        // `port` is Int — pass a string and we should get a type
        // mismatch finding rather than a silent fallback.
        let props = vec![
            kv("host", ExprKind::StringLit("h".into())),
            kv("port", ExprKind::StringLit("five-fourteen".into())),
        ];
        let err = TcpOutput::build("relay", &props).err().expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("port"), "{}", msg);
        assert!(msg.contains("integer"), "{}", msg);
    }

    #[test]
    fn build_collects_multiple_errors_in_one_message() {
        let props = vec![
            kv("portt", ExprKind::IntLit(514)),
            kv("framing", ExprKind::Ident(vec!["xx".into()])),
        ];
        let err = TcpOutput::build("relay", &props).err().expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("portt"), "{}", msg);
        assert!(msg.contains("framing"), "{}", msg);
    }

    #[test]
    fn from_properties_directly_still_works_for_existing_call_sites() {
        // The trait's `from_properties` is the no-validation entry the
        // factory closure uses (validation happens at the registry
        // boundary). Confirm legacy direct callers — e.g. modules in
        // their own tests — still work.
        let props = vec![kv("address", ExprKind::StringLit("h:1".into()))];
        let tcp = TcpOutput::from_properties("relay", &props).expect("should build");
        assert_eq!(tcp.address, "h:1");
    }
}
