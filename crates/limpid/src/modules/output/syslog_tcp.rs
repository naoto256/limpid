//! Syslog TCP output: ships event messages to remote syslog endpoints
//! over TCP, with optional per-peer TLS.
//!
//! A single output may hold a mixed peer list: plaintext and TLS
//! destinations coexist on the same rotation. The decision is made
//! per-peer — a `tls` block (inline or named-profile reference) flips
//! that peer into a TLS handshake before framing; peers without `tls`
//! stay on plaintext TCP.
//!
//! Default port is per-peer: 6514 when TLS is configured for that
//! peer (RFC 5425), 514 otherwise (RFC 6587). Framing
//! (`octet_counting` / `non_transparent`) is output-wide and applies
//! uniformly to every peer.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result};
use tokio::io::AsyncWrite;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::{ExprKind, Property};
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::output::syslog_peers::{
    PEER_CONNECT_TIMEOUT, PEER_HANDSHAKE_TIMEOUT, PEER_WRITE_TIMEOUT, Peer, PeerList,
    SyslogFraming, SyslogPayload, iter_peers_block, parse_host_port, write_framed,
};
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};
use crate::tls::{ClientTlsConfig, build_client_config_sync};

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const TLS_BLOCK_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        name: "ca",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "cert",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "key",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
];

/// `peer.tls` may be either an inline block or a profile name (ident
/// or string). `OneOf` lets the schema validator accept either shape
/// without flagging the other as a type error.
const PEER_TLS_KINDS: &[PropertyValueKind] = &[
    PropertyValueKind::Block(TLS_BLOCK_PROPERTIES),
    PropertyValueKind::String,
];

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
    PropertySpec {
        name: "tls",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::OneOf(PEER_TLS_KINDS),
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
        name: "tls",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::BlockMap(TLS_BLOCK_PROPERTIES),
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

// ---------------------------------------------------------------------------
// Conn — plaintext or TLS, behind one AsyncWrite façade
// ---------------------------------------------------------------------------

/// Per-peer connection. Plain and TLS variants both implement
/// [`AsyncWrite`] so `write_framed` doesn't branch on the peer type.
///
/// The TLS variant is boxed because `TlsStream` carries an internal
/// rustls connection state much larger than a bare `TcpStream`, and
/// leaving it inline would force every `PeerState::conn` slot (TLS or
/// not) to pay that footprint. One heap allocation per peer per
/// reconnect is negligible.
enum Conn {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncWrite for Conn {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

pub struct SyslogTcpOutput {
    pub framing: SyslogFraming,
    peers: PeerList<Conn>,
    /// Same index as `peers.peers()`. `None` for plaintext peers, so
    /// the per-event hot path branches on a cheap option check rather
    /// than rebuilding a `ClientConfig`.
    connectors: Vec<Option<TlsConnector>>,
    metrics: Arc<OutputMetrics>,
}

impl Module for SyslogTcpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SYSLOG_TCP_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
        let framing = parse_framing(name, properties)?;
        let profiles = parse_tls_profiles(name, properties)?;
        let peers = parse_peers(name, properties, &profiles)?;

        // Only pay the crypto-provider install cost if the output
        // actually has at least one TLS peer.
        if peers.iter().any(|p| p.tls.is_some()) {
            crate::tls::install_default_crypto_provider();
        }

        let connectors = peers
            .iter()
            .map(|peer| match &peer.tls {
                Some(tls) => build_client_config_sync(tls).map(TlsConnector::from).map(Some),
                None => Ok(None),
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            framing,
            peers: PeerList::new(peers),
            connectors,
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

fn parse_framing(name: &str, properties: &[Property]) -> Result<SyslogFraming> {
    match props::get_ident(properties, "framing").as_deref() {
        Some("non_transparent") => Ok(SyslogFraming::NonTransparent),
        Some("octet_counting") | None => Ok(SyslogFraming::OctetCounting),
        Some(other) => anyhow::bail!(
            "output '{}': unknown framing '{}' (expected octet_counting | non_transparent)",
            name,
            other
        ),
    }
}

fn parse_tls_profiles(
    name: &str,
    properties: &[Property],
) -> Result<HashMap<String, ClientTlsConfig>> {
    let mut out = HashMap::new();
    let Some(block) = props::get_block(properties, "tls") else {
        return Ok(out);
    };

    for prop in block {
        if let Property::Block {
            key,
            properties: inner,
            ..
        } = prop
        {
            let label = format!("output '{}': tls profile '{}'", name, key);
            let tls = parse_tls_block(&label, inner)?;
            out.insert(key.clone(), tls);
        }
    }

    Ok(out)
}

fn parse_peers(
    name: &str,
    properties: &[Property],
    profiles: &HashMap<String, ClientTlsConfig>,
) -> Result<Vec<Peer>> {
    if let Some(peer_block) = props::get_block(properties, "peer") {
        return Ok(vec![parse_peer(name, "peer", peer_block, profiles)?]);
    }

    if let Some(peers_block) = props::get_block(properties, "peers") {
        let label = format!("output '{}': peers", name);
        return iter_peers_block(peers_block, &label, |inner| {
            parse_peer(name, "peers.peer", inner, profiles)
        });
    }

    anyhow::bail!("output '{}': either 'peer' or 'peers' is required", name)
}

fn parse_peer(
    name: &str,
    label: &str,
    properties: &[Property],
    profiles: &HashMap<String, ClientTlsConfig>,
) -> Result<Peer> {
    let tls = parse_peer_tls(name, label, properties, profiles)?;
    // RFC 5425 (TLS) → 6514, RFC 6587 (TCP plain) → 514. The default
    // is per-peer, not per-output, so a mixed list does the right
    // thing without anyone writing port numbers explicitly.
    let default_port = if tls.is_some() { 6514 } else { 514 };
    let host_port_label = format!("output '{}': {}", name, label);
    let (host, port) = parse_host_port(properties, default_port, &host_port_label)?;
    Ok(Peer { host, port, tls })
}

fn parse_peer_tls(
    name: &str,
    label: &str,
    properties: &[Property],
    profiles: &HashMap<String, ClientTlsConfig>,
) -> Result<Option<ClientTlsConfig>> {
    let Some(prop) = properties.iter().find(|prop| match prop {
        Property::Block { key, .. } | Property::KeyValue { key, .. } => key == "tls",
    }) else {
        return Ok(None);
    };

    match prop {
        Property::Block {
            properties: inner, ..
        } => {
            let label = format!("output '{}': {} tls", name, label);
            parse_tls_block(&label, inner).map(Some)
        }
        Property::KeyValue { value, .. } => {
            let profile = match &value.kind {
                ExprKind::Ident(parts) if !parts.is_empty() => Some(parts.join(".")),
                ExprKind::StringLit(s) => Some(s.clone()),
                _ => None,
            }
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "output '{}': {} tls must be a profile name or inline block",
                    name,
                    label
                )
            })?;
            profiles.get(&profile).cloned().map(Some).ok_or_else(|| {
                anyhow::anyhow!("output '{}': unknown tls profile '{}'", name, profile)
            })
        }
    }
}

fn parse_tls_block(label: &str, properties: &[Property]) -> Result<ClientTlsConfig> {
    let tls = ClientTlsConfig {
        ca_path: props::get_string(properties, "ca"),
        cert_path: props::get_string(properties, "cert"),
        key_path: props::get_string(properties, "key"),
    };
    tls.validate(label)?;
    Ok(tls)
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
        Ok(RenderedPayload::new(SyslogPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: SyslogPayload = payload.downcast()?;
        let framing = self.framing;
        let metrics = Arc::clone(&self.metrics);
        let connectors = self.connectors.clone();
        let result = self
            .peers
            .write_with_rotation_now(move |idx, peer, state| {
                let egress = payload.egress.clone();
                let address = peer.address();
                let server_name = peer.host.clone();
                let connector = connectors[idx].clone();
                Box::pin(async move {
                    if state.conn.is_none() {
                        let tcp = tokio::time::timeout(
                            PEER_CONNECT_TIMEOUT,
                            TcpStream::connect(&address),
                        )
                        .await
                        .with_context(|| format!("syslog_tcp connect to {} timed out", address))?
                        .with_context(|| format!("syslog_tcp connect to {}", address))?;

                        let conn = match connector {
                            Some(connector) => {
                                let server_name = ServerName::try_from(server_name.as_str())
                                    .with_context(|| {
                                        format!("syslog_tcp invalid server name: {}", server_name)
                                    })?
                                    .to_owned();
                                let tls = tokio::time::timeout(
                                    PEER_HANDSHAKE_TIMEOUT,
                                    connector.connect(server_name, tcp),
                                )
                                .await
                                .with_context(|| {
                                    format!("syslog_tcp TLS handshake to {} timed out", address)
                                })?
                                .with_context(|| {
                                    format!("syslog_tcp TLS handshake to {}", address)
                                })?;
                                Conn::Tls(Box::new(tls))
                            }
                            None => Conn::Plain(tcp),
                        };
                        state.conn = Some(conn);
                    }

                    let stream = state.conn.as_mut().expect("connection should be present");
                    let write_result = tokio::time::timeout(
                        PEER_WRITE_TIMEOUT,
                        write_framed(stream, framing, &egress),
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::dsl::schema::SchemaErrorKind;
    use tempfile::TempDir;

    struct PemFiles {
        _dir: TempDir,
        cert: String,
        key: String,
    }

    fn pem_files() -> PemFiles {
        let dir = TempDir::new().unwrap();
        let cert_params =
            rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("valid CN");
        let key_pair = rcgen::KeyPair::generate().expect("key gen");
        let cert = cert_params.self_signed(&key_pair).expect("self-sign");
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();
        PemFiles {
            _dir: dir,
            cert: cert_path.display().to_string(),
            key: key_path.display().to_string(),
        }
    }

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

    fn peer_plain(host: &str, port: i64) -> Property {
        block(
            "peer",
            vec![
                kv("host", ExprKind::StringLit(host.into())),
                kv("port", ExprKind::IntLit(port)),
            ],
        )
    }

    fn peer_with_tls(host: &str, port: i64, tls: Property) -> Property {
        block(
            "peer",
            vec![
                kv("host", ExprKind::StringLit(host.into())),
                kv("port", ExprKind::IntLit(port)),
                tls,
            ],
        )
    }

    fn peer_with_tls_default_port(host: &str, tls: Option<Property>) -> Property {
        let mut props = vec![kv("host", ExprKind::StringLit(host.into()))];
        if let Some(tls) = tls {
            props.push(tls);
        }
        block("peer", props)
    }

    fn tls_block(props: Vec<Property>) -> Property {
        block("tls", props)
    }

    fn ca_block(path: &str) -> Property {
        tls_block(vec![kv("ca", ExprKind::StringLit(path.into()))])
    }

    fn mtls_block(cert: &str, key: &str, ca: &str) -> Property {
        tls_block(vec![
            kv("ca", ExprKind::StringLit(ca.into())),
            kv("cert", ExprKind::StringLit(cert.into())),
            kv("key", ExprKind::StringLit(key.into())),
        ])
    }

    // -------------------------------------------------------------------
    // Plain-only behavior (carried over from the original syslog_tcp tests)
    // -------------------------------------------------------------------

    #[test]
    fn build_accepts_single_peer() {
        let props = vec![peer_plain("127.0.0.1", 514)];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.len(), 1);
        assert_eq!(tcp.peers.peers()[0].address(), "127.0.0.1:514");
        assert_eq!(tcp.framing, SyslogFraming::OctetCounting);
        assert_eq!(tcp.connectors.len(), 1);
        assert!(tcp.connectors[0].is_none());
    }

    #[test]
    fn build_accepts_peer_with_default_port_plain() {
        let props = vec![block(
            "peer",
            vec![kv("host", ExprKind::StringLit("127.0.0.1".into()))],
        )];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.peers()[0].address(), "127.0.0.1:514");
    }

    #[test]
    fn build_accepts_multiple_peers() {
        let props = vec![block("peers", vec![peer_plain("a", 514), peer_plain("b", 1514)])];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.len(), 2);
        assert_eq!(tcp.peers.peers()[0].address(), "a:514");
        assert_eq!(tcp.peers.peers()[1].address(), "b:1514");
    }

    #[test]
    fn build_accepts_correct_framing_enum_value() {
        let props = vec![
            peer_plain("h", 1),
            kv("framing", ExprKind::Ident(vec!["non_transparent".into()])),
        ];
        let tcp = SyslogTcpOutput::build("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.framing, SyslogFraming::NonTransparent);
    }

    #[test]
    fn build_rejects_typoed_framing_with_did_you_mean() {
        let props = vec![
            peer_plain("h", 1),
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
        let props = vec![peer_plain("a", 514), block("peers", vec![peer_plain("b", 514)])];
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
        let props = vec![peer_plain("h", 1)];
        let tcp = SyslogTcpOutput::from_properties("relay", &mp(&props)).expect("should build");
        assert_eq!(tcp.peers.peers()[0].address(), "h:1");
    }

    // -------------------------------------------------------------------
    // TLS-on-syslog_tcp behavior (carried over from syslog_tls tests)
    // -------------------------------------------------------------------

    #[test]
    fn build_accepts_single_peer_inline_tls_ca_only() {
        let files = pem_files();
        let output = SyslogTcpOutput::build(
            "relay",
            &mp(&[peer_with_tls("example.com", 6514, ca_block(&files.cert))]),
        )
        .expect("should build");
        let peer0 = &output.peers.peers()[0];
        assert_eq!(peer0.tls.as_ref().unwrap().ca_path, Some(files.cert));
        assert!(output.connectors[0].is_some());
    }

    #[test]
    fn build_accepts_single_peer_inline_tls_mtls() {
        let files = pem_files();
        let output = SyslogTcpOutput::build(
            "relay",
            &mp(&[peer_with_tls(
                "example.com",
                6514,
                mtls_block(&files.cert, &files.key, &files.cert),
            )]),
        )
        .expect("should build");
        let tls = output.peers.peers()[0].tls.as_ref().unwrap();
        assert_eq!(tls.cert_path.as_deref(), Some(files.cert.as_str()));
        assert_eq!(tls.key_path.as_deref(), Some(files.key.as_str()));
    }

    #[test]
    fn build_accepts_peer_referencing_named_profile() {
        let files = pem_files();
        let output = SyslogTcpOutput::build(
            "relay",
            &mp(&[
                block(
                    "tls",
                    vec![block(
                        "my_p",
                        vec![kv("ca", ExprKind::StringLit(files.cert.clone()))],
                    )],
                ),
                peer_with_tls(
                    "example.com",
                    6514,
                    kv("tls", ExprKind::Ident(vec!["my_p".into()])),
                ),
            ]),
        )
        .expect("should build");
        assert_eq!(
            output.peers.peers()[0]
                .tls
                .as_ref()
                .unwrap()
                .ca_path
                .as_deref(),
            Some(files.cert.as_str())
        );
    }

    #[test]
    fn build_rejects_peer_referencing_undefined_profile() {
        let err = SyslogTcpOutput::build(
            "relay",
            &mp(&[peer_with_tls(
                "example.com",
                6514,
                kv("tls", ExprKind::Ident(vec!["missing".into()])),
            )]),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().contains("unknown tls profile"), "{}", err);
    }

    #[test]
    fn build_rejects_cert_without_key() {
        let err = SyslogTcpOutput::build(
            "relay",
            &mp(&[peer_with_tls(
                "example.com",
                6514,
                tls_block(vec![kv("cert", ExprKind::StringLit("x.crt".into()))]),
            )]),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().contains("cert and key"), "{}", err);
    }

    #[test]
    fn build_rejects_key_without_cert() {
        let err = SyslogTcpOutput::build(
            "relay",
            &mp(&[peer_with_tls(
                "example.com",
                6514,
                tls_block(vec![kv("key", ExprKind::StringLit("x.key".into()))]),
            )]),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().contains("cert and key"), "{}", err);
    }

    // -------------------------------------------------------------------
    // Mixed plain + TLS peers — the shape this merge unlocks
    // -------------------------------------------------------------------

    #[test]
    fn build_accepts_mixed_plain_and_tls_peers() {
        let files = pem_files();
        let output = SyslogTcpOutput::build(
            "relay",
            &mp(&[
                block(
                    "tls",
                    vec![block(
                        "named",
                        vec![kv("ca", ExprKind::StringLit(files.cert.clone()))],
                    )],
                ),
                block(
                    "peers",
                    vec![
                        peer_with_tls(
                            "a.example.com",
                            6514,
                            kv("tls", ExprKind::Ident(vec!["named".into()])),
                        ),
                        peer_with_tls("b.example.com", 6514, ca_block(&files.cert)),
                        peer_plain("c.example.com", 514),
                    ],
                ),
            ]),
        )
        .expect("should build");
        assert_eq!(output.peers.len(), 3);
        assert_eq!(output.connectors.len(), 3);
        assert!(output.peers.peers()[0].tls.is_some() && output.connectors[0].is_some());
        assert!(output.peers.peers()[1].tls.is_some() && output.connectors[1].is_some());
        assert!(output.peers.peers()[2].tls.is_none() && output.connectors[2].is_none());
    }

    #[test]
    fn default_port_is_per_peer_based_on_tls_presence() {
        let files = pem_files();
        let output = SyslogTcpOutput::build(
            "relay",
            &mp(&[block(
                "peers",
                vec![
                    peer_with_tls_default_port("tls.example.com", Some(ca_block(&files.cert))),
                    peer_with_tls_default_port("plain.example.com", None),
                ],
            )]),
        )
        .expect("should build");
        // TLS peer → 6514 (RFC 5425), plain peer → 514 (RFC 6587).
        assert_eq!(output.peers.peers()[0].address(), "tls.example.com:6514");
        assert_eq!(output.peers.peers()[1].address(), "plain.example.com:514");
    }
}
