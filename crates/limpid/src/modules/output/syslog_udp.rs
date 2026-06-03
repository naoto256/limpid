//! Syslog UDP output: sends event messages to remote syslog UDP endpoints.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::output::syslog_peers::{
    PEER_CONNECT_TIMEOUT, PEER_WRITE_TIMEOUT, Peer, PeerList, SyslogPayload, iter_peers_block,
    parse_host_port,
};
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};

const SYSLOG_UDP_PEER_SCHEMA: &[PropertySpec] = &[
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

const SYSLOG_UDP_PEERS_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "peer",
    required: true,
    repeatable: true,
    exclusive_group: None,
    kind: PropertyValueKind::Block(SYSLOG_UDP_PEER_SCHEMA),
}];

const SYSLOG_UDP_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "peer",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_UDP_PEER_SCHEMA),
    },
    PropertySpec {
        name: "peers",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_UDP_PEERS_SCHEMA),
    },
    crate::queue::QUEUE_PROPERTY_SPEC,
];

pub struct SyslogUdpOutput {
    peers: PeerList<UdpSocket>,
    metrics: Arc<OutputMetrics>,
}

impl Module for SyslogUdpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SYSLOG_UDP_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
        let peers = parse_peers(name, properties)?;
        Ok(Self {
            peers: PeerList::new(peers),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

fn parse_peers(name: &str, properties: &[Property]) -> Result<Vec<Peer>> {
    if let Some(peer_block) = props::get_block(properties, "peer") {
        return Ok(vec![parse_peer(name, "peer", peer_block)?]);
    }

    if let Some(peers_block) = props::get_block(properties, "peers") {
        let label = format!("output '{}': peers", name);
        return iter_peers_block(peers_block, &label, |inner| {
            parse_peer(name, "peers.peer", inner)
        });
    }

    anyhow::bail!("output '{}': either 'peer' or 'peers' is required", name)
}

fn parse_peer(name: &str, label: &str, properties: &[Property]) -> Result<Peer> {
    let label = format!("output '{}': {}", name, label);
    let (host, port) = parse_host_port(properties, 514, &label)?;
    Ok(Peer {
        host,
        port,
        tls: None,
    })
}

impl HasMetrics for SyslogUdpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for SyslogUdpOutput {
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(SyslogPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: SyslogPayload = payload.downcast()?;
        let metrics = Arc::clone(&self.metrics);
        let result = self
            .peers
            .write_with_rotation_now(move |_idx, peer, state| {
                let egress = payload.egress.clone();
                let address = peer.address();
                Box::pin(async move {
                    if state.conn.is_none() {
                        let socket = UdpSocket::bind("0.0.0.0:0")
                            .await
                            .context("syslog_udp output: failed to bind ephemeral socket")?;
                        tokio::time::timeout(PEER_CONNECT_TIMEOUT, socket.connect(&address))
                            .await
                            .with_context(|| {
                                format!("syslog_udp connect to {} timed out", address)
                            })?
                            .with_context(|| format!("syslog_udp connect to {}", address))?;
                        state.conn = Some(socket);
                    }

                    let socket = state.conn.as_mut().expect("connection should be present");
                    let send_result =
                        tokio::time::timeout(PEER_WRITE_TIMEOUT, socket.send(&egress))
                            .await
                            .map_err(|_| {
                                anyhow::anyhow!("syslog_udp send to {} timed out", address)
                            })
                            .and_then(|res| {
                                res.with_context(|| format!("syslog_udp send to {}", address))
                            });
                    if send_result.is_err() {
                        state.conn = None;
                    }
                    send_result.map(|_| ())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::dsl::schema::SchemaErrorKind;

    /// Wrap a property list in a `ModuleProperties` shaped for this test module.
    /// Mirrors what the parser produces for `def input/output ... { type syslog_udp; ... }`
    /// without going through pest, so tests can drive `Module::{build,from_properties}`
    /// directly.
    fn mp(props: &[Property]) -> crate::modules::ModuleProperties {
        crate::modules::ModuleProperties::from_parts("syslog_udp", props.to_vec())
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
        let u = SyslogUdpOutput::build("u", &mp(&[peer("h", 1)])).expect("ok");
        assert_eq!(u.peers.len(), 1);
        assert_eq!(u.peers.peers()[0].address(), "h:1");
    }

    #[test]
    fn build_accepts_peer_with_default_port() {
        let u = SyslogUdpOutput::build(
            "u",
            &mp(&[block(
                "peer",
                vec![kv("host", ExprKind::StringLit("h".into()))],
            )]),
        )
        .expect("ok");
        assert_eq!(u.peers.peers()[0].address(), "h:514");
    }

    #[test]
    fn build_accepts_multiple_peers() {
        let u = SyslogUdpOutput::build(
            "u",
            &mp(&[block("peers", vec![peer("a", 1), peer("b", 2)])]),
        )
        .expect("ok");
        assert_eq!(u.peers.len(), 2);
        assert_eq!(u.peers.peers()[0].address(), "a:1");
        assert_eq!(u.peers.peers()[1].address(), "b:2");
    }

    #[test]
    fn build_rejects_missing_destination() {
        let err = SyslogUdpOutput::build("u", &mp(&[]))
            .err()
            .expect("missing destination");
        assert!(
            err.to_string()
                .contains("either 'peer' or 'peers' is required"),
            "{}",
            err
        );
    }

    #[test]
    fn build_rejects_unknown_key_with_did_you_mean() {
        let props = vec![block(
            "per",
            vec![kv("host", ExprKind::StringLit("h".into()))],
        )];
        let err = SyslogUdpOutput::build("u", &mp(&props))
            .err()
            .expect("typo");
        let msg = err.to_string();
        assert!(msg.contains("per") && msg.contains("peer"), "{}", msg);
    }

    #[test]
    fn build_rejects_peer_and_peers_together() {
        let props = vec![peer("a", 1), block("peers", vec![peer("b", 2)])];
        let err = SyslogUdpOutput::build("u", &mp(&props))
            .err()
            .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("exclusive group"), "{}", msg);
        assert!(msg.contains("peer") && msg.contains("peers"), "{}", msg);
    }

    #[test]
    fn build_rejects_empty_peers_block() {
        let err = SyslogUdpOutput::from_properties("u", &mp(&[block("peers", vec![])]))
            .err()
            .expect("should fail");
        assert!(
            err.to_string()
                .contains("peers block must contain at least one peer"),
            "{}",
            err
        );

        let schema_errs =
            crate::dsl::schema::validate(&[block("peers", vec![])], SYSLOG_UDP_OUTPUT_SCHEMA);
        assert!(
            schema_errs
                .iter()
                .any(|err| matches!(err.kind, SchemaErrorKind::MissingRequired))
        );
    }

    #[test]
    fn build_rejects_peer_missing_host() {
        let props = vec![block("peer", vec![kv("port", ExprKind::IntLit(514))])];
        let err = SyslogUdpOutput::build("u", &mp(&props))
            .err()
            .expect("should fail");
        assert!(err.to_string().contains("host"), "{}", err);
    }
}
