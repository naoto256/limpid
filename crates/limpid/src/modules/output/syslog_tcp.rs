//! Syslog TCP output: sends event messages to remote syslog TCP endpoints.
//! Supports octet counting (RFC 6587) and non-transparent framing.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::output::syslog_peers::{
    PEER_CONNECT_TIMEOUT, PEER_WRITE_TIMEOUT, Peer, PeerList,
};
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};

const SYSLOG_TCP_PEER_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "host",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "port",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
];

const SYSLOG_TCP_PEERS_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "peer",
    required: true,
    repeatable: true,
    exclusive_group: None,
    kind: PropertyValueKind::Block(SYSLOG_TCP_PEER_SCHEMA),
}];

const SYSLOG_TCP_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "framing",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["octet_counting", "non_transparent"]),
    },
    PropertySpec {
        name: "peer",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_TCP_PEER_SCHEMA),
    },
    PropertySpec {
        name: "peers",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_TCP_PEERS_SCHEMA),
    },
    crate::queue::QUEUE_PROPERTY_SPEC,
];

struct SyslogTcpPayload {
    egress: Bytes,
}

pub struct SyslogTcpOutput {
    pub framing: SyslogTcpFraming,
    peers: PeerList<TcpStream>,
    metrics: Arc<OutputMetrics>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyslogTcpFraming {
    OctetCounting,
    NonTransparent,
}

impl Module for SyslogTcpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SYSLOG_TCP_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
        let framing = parse_framing(name, properties)?;
        let peers = parse_peers(name, properties)?;
        Ok(Self {
            framing,
            peers: PeerList::new(peers),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

fn parse_framing(name: &str, properties: &[Property]) -> Result<SyslogTcpFraming> {
    match props::get_ident(properties, "framing").as_deref() {
        Some("non_transparent") => Ok(SyslogTcpFraming::NonTransparent),
        Some("octet_counting") | None => Ok(SyslogTcpFraming::OctetCounting),
        Some(other) => anyhow::bail!(
            "output '{}': unknown framing '{}' (expected octet_counting | non_transparent)",
            name,
            other
        ),
    }
}

fn parse_peers(name: &str, properties: &[Property]) -> Result<Vec<Peer>> {
    if let Some(peer_block) = props::get_block(properties, "peer") {
        return Ok(vec![parse_peer(name, "peer", peer_block)?]);
    }

    if let Some(peers_block) = props::get_block(properties, "peers") {
        let mut out = Vec::new();
        for prop in peers_block {
            if let Property::Block {
                key,
                properties: inner,
                ..
            } = prop
                && key == "peer"
            {
                out.push(parse_peer(name, "peers.peer", inner)?);
            }
        }
        if out.is_empty() {
            anyhow::bail!(
                "output '{}': peers block must contain at least one peer",
                name
            );
        }
        return Ok(out);
    }

    anyhow::bail!("output '{}': either 'peer' or 'peers' is required", name)
}

fn parse_peer(name: &str, label: &str, properties: &[Property]) -> Result<Peer> {
    let host = props::get_string(properties, "host")
        .ok_or_else(|| anyhow::anyhow!("output '{}': {} requires 'host'", name, label))?;
    let port = match props::get_int(properties, "port") {
        Some(port) => u16::try_from(port)
            .with_context(|| format!("output '{}': {} port must be 0..=65535", name, label))?,
        None => 514,
    };
    Ok(Peer {
        host,
        port,
        tls: None,
    })
}

impl HasMetrics for SyslogTcpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for SyslogTcpOutput {
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(SyslogTcpPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: SyslogTcpPayload = payload.downcast()?;
        let framing = self.framing;
        let metrics = Arc::clone(&self.metrics);
        let result = self
            .peers
            .write_with_rotation_now(move |_idx, peer, state| {
                let egress = payload.egress.clone();
                let address = peer.address();
                Box::pin(async move {
                    if state.conn.is_none() {
                        let stream = tokio::time::timeout(
                            PEER_CONNECT_TIMEOUT,
                            TcpStream::connect(&address),
                        )
                        .await
                        .with_context(|| format!("syslog_tcp connect to {} timed out", address))?
                        .with_context(|| format!("syslog_tcp connect to {}", address))?;
                        state.conn = Some(stream);
                    }

                    let stream = state.conn.as_mut().expect("connection should be present");
                    let write_result = tokio::time::timeout(
                        PEER_WRITE_TIMEOUT,
                        write_syslog_tcp_framed(stream, framing, &egress),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("syslog_tcp write to {} timed out", address))
                    .and_then(|res| res);
                    if write_result.is_err() {
                        state.conn = None;
                    }
                    write_result
                })
            })
            .await;

        match result {
            Ok(()) => {
                metrics.events_written.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(err) => Err(anyhow::anyhow!("{}", err)),
        }
    }
}

async fn write_syslog_tcp_framed(
    stream: &mut TcpStream,
    framing: SyslogTcpFraming,
    payload: &Bytes,
) -> Result<()> {
    match framing {
        SyslogTcpFraming::OctetCounting => {
            let header = format!("{} ", payload.len());
            stream.write_all(header.as_bytes()).await?;
            stream.write_all(payload).await?;
        }
        SyslogTcpFraming::NonTransparent => {
            stream.write_all(payload).await?;
            stream.write_all(b"\n").await?;
        }
    }

    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::dsl::schema::SchemaErrorKind;

    /// Wrap a property list in a `ModuleProperties` shaped for this test module.
    /// Mirrors what the parser produces for `def input/output ... { type syslog_tcp; ... }`
    /// without going through pest, so tests can drive `Module::{build,from_properties}`
    /// directly.
    fn mp(props: &[Property]) -> crate::modules::ModuleProperties {
        crate::modules::ModuleProperties::from_parts("syslog_tcp", props.to_vec())
    }

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str, properties: Vec<Property>) -> Property {
        Property::Block {
            key: key.into(),
            key_span: None,
            properties,
        }
    }

    fn peer(host: &str, port: i64) -> Property {
        block(
            "peer",
            vec![
                kv("host", ExprKind::StringLit(host.into())),
                kv("port", ExprKind::IntLit(port)),
            ],
        )
    }

    #[test]
    fn build_accepts_single_peer() {
        let props = vec![peer("127.0.0.1", 514)];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.len(), 1);
        assert_eq!(tcp.peers.peers()[0].address(), "127.0.0.1:514");
        assert_eq!(tcp.framing, SyslogTcpFraming::OctetCounting);
    }

    #[test]
    fn build_accepts_peer_with_default_port() {
        let props = vec![block(
            "peer",
            vec![kv("host", ExprKind::StringLit("127.0.0.1".into()))],
        )];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.peers()[0].address(), "127.0.0.1:514");
    }

    #[test]
    fn build_accepts_multiple_peers() {
        let props = vec![block("peers", vec![peer("a", 514), peer("b", 1514)])];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.len(), 2);
        assert_eq!(tcp.peers.peers()[0].address(), "a:514");
        assert_eq!(tcp.peers.peers()[1].address(), "b:1514");
    }

    #[test]
    fn build_accepts_correct_framing_enum_value() {
        let props = vec![
            peer("h", 1),
            kv("framing", ExprKind::Ident(vec!["non_transparent".into()])),
        ];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.framing, SyslogTcpFraming::NonTransparent);
    }

    #[test]
    fn build_rejects_typoed_framing_with_did_you_mean() {
        let props = vec![
            peer("h", 1),
            kv("framing", ExprKind::Ident(vec!["non_trasnaprent".into()])),
        ];
        let err = SyslogTcpOutput::build("relay", &mp(&props))
            .err()
            .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("framing"), "{}", msg);
        assert!(
            msg.contains("non_transparent"),
            "did-you-mean missing: {}",
            msg
        );
    }

    #[test]
    fn build_rejects_unknown_key_with_did_you_mean() {
        let props = vec![block(
            "per",
            vec![kv("host", ExprKind::StringLit("h".into()))],
        )];
        let err = SyslogTcpOutput::build("relay", &mp(&props))
            .err()
            .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("unknown property 'per'"), "{}", msg);
        assert!(msg.contains("peer"), "did-you-mean missing: {}", msg);
    }

    #[test]
    fn build_rejects_wrong_value_type() {
        let props = vec![block(
            "peer",
            vec![
                kv("host", ExprKind::StringLit("h".into())),
                kv("port", ExprKind::StringLit("five-fourteen".into())),
            ],
        )];
        let err = SyslogTcpOutput::build("relay", &mp(&props))
            .err()
            .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("port"), "{}", msg);
        assert!(msg.contains("integer"), "{}", msg);
    }

    #[test]
    fn build_collects_multiple_errors_in_one_message() {
        let props = vec![
            block("per", vec![kv("host", ExprKind::StringLit("h".into()))]),
            kv("framing", ExprKind::Ident(vec!["xx".into()])),
        ];
        let err = SyslogTcpOutput::build("relay", &mp(&props))
            .err()
            .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("per"), "{}", msg);
        assert!(msg.contains("framing"), "{}", msg);
    }

    #[test]
    fn build_rejects_peer_and_peers_together() {
        let props = vec![peer("a", 514), block("peers", vec![peer("b", 514)])];
        let err = SyslogTcpOutput::build("relay", &mp(&props))
            .err()
            .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("exclusive group"), "{}", msg);
        assert!(msg.contains("peer") && msg.contains("peers"), "{}", msg);
    }

    #[test]
    fn build_rejects_missing_destination() {
        let err = SyslogTcpOutput::build("relay", &mp(&[]))
            .err()
            .expect("should fail");
        assert!(
            err.to_string()
                .contains("either 'peer' or 'peers' is required"),
            "{}",
            err
        );
    }

    #[test]
    fn build_rejects_empty_peers_block() {
        let err = SyslogTcpOutput::from_properties("relay", &mp(&[block("peers", vec![])]))
            .err()
            .expect("should fail");
        assert!(
            err.to_string()
                .contains("peers block must contain at least one peer"),
            "{}",
            err
        );

        let schema_errs =
            crate::dsl::schema::validate(&[block("peers", vec![])], SYSLOG_TCP_OUTPUT_SCHEMA);
        assert!(
            schema_errs
                .iter()
                .any(|err| matches!(err.kind, SchemaErrorKind::MissingRequired))
        );
    }

    #[test]
    fn build_rejects_peer_missing_host() {
        let props = vec![block("peer", vec![kv("port", ExprKind::IntLit(514))])];
        let err = SyslogTcpOutput::build("relay", &mp(&props))
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("host"), "{}", err);
    }

    #[test]
    fn from_properties_directly_still_works_for_existing_call_sites() {
        let props = vec![peer("h", 1)];
        let tcp = SyslogTcpOutput::from_properties("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.peers()[0].address(), "h:1");
    }
}
