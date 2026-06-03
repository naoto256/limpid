//! Syslog TCP+TLS output: sends event messages to remote syslog TLS endpoints.
//! Supports octet counting (RFC 6587) and non-transparent framing.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use bytes::Bytes;
use tokio::io::AsyncWriteExt;
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
};
use crate::modules::output::syslog_tcp::SyslogTcpFraming;
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};
use crate::tls::{ClientTlsConfig, build_client_config_sync};

const SYSLOG_TLS_BLOCK_PROPERTIES: &[PropertySpec] = &[
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

const SYSLOG_TLS_PEER_TLS_KINDS: &[PropertyValueKind] = &[
    PropertyValueKind::Block(SYSLOG_TLS_BLOCK_PROPERTIES),
    PropertyValueKind::String,
];

const SYSLOG_TLS_PEER_SCHEMA: &[PropertySpec] = &[
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
        kind: PropertyValueKind::OneOf(SYSLOG_TLS_PEER_TLS_KINDS),
    },
];

const SYSLOG_TLS_PEERS_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "peer",
    required: true,
    repeatable: true,
    exclusive_group: None,
    kind: PropertyValueKind::Block(SYSLOG_TLS_PEER_SCHEMA),
}];

const SYSLOG_TLS_OUTPUT_SCHEMA: &[PropertySpec] = &[
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
        kind: PropertyValueKind::BlockMap(SYSLOG_TLS_BLOCK_PROPERTIES),
    },
    PropertySpec {
        name: "peer",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_TLS_PEER_SCHEMA),
    },
    PropertySpec {
        name: "peers",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(SYSLOG_TLS_PEERS_SCHEMA),
    },
    crate::queue::QUEUE_PROPERTY_SPEC,
];

struct SyslogTlsPayload {
    egress: Bytes,
}

pub struct SyslogTlsOutput {
    pub framing: SyslogTcpFraming,
    peers: PeerList<TlsStream<TcpStream>>,
    connectors: Vec<TlsConnector>,
    metrics: Arc<OutputMetrics>,
}

impl Module for SyslogTlsOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(SYSLOG_TLS_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        crate::tls::install_default_crypto_provider();
        let properties = properties.user_properties();
        let framing = parse_framing(name, properties)?;
        let profiles = parse_tls_profiles(name, properties)?;
        let peers = parse_peers(name, properties, &profiles)?;
        let connectors = peers
            .iter()
            .map(|peer| {
                let tls = peer.tls.clone().unwrap_or(ClientTlsConfig {
                    ca_path: None,
                    cert_path: None,
                    key_path: None,
                });
                build_client_config_sync(&tls).map(TlsConnector::from)
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
        let mut out = Vec::new();
        for prop in peers_block {
            if let Property::Block {
                key,
                properties: inner,
                ..
            } = prop
                && key == "peer"
            {
                out.push(parse_peer(name, "peers.peer", inner, profiles)?);
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

fn parse_peer(
    name: &str,
    label: &str,
    properties: &[Property],
    profiles: &HashMap<String, ClientTlsConfig>,
) -> Result<Peer> {
    let host = props::get_string(properties, "host")
        .ok_or_else(|| anyhow::anyhow!("output '{}': {} requires 'host'", name, label))?;
    let port = match props::get_int(properties, "port") {
        Some(port) => u16::try_from(port)
            .with_context(|| format!("output '{}': {} port must be 0..=65535", name, label))?,
        None => 6514,
    };
    let tls = parse_peer_tls(name, label, properties, profiles)?;
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

impl HasMetrics for SyslogTlsOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for SyslogTlsOutput {
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(SyslogTlsPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: SyslogTlsPayload = payload.downcast()?;
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
                        .with_context(|| {
                            format!("syslog_tls TCP connect to {} timed out", address)
                        })?
                        .with_context(|| format!("syslog_tls TCP connect to {}", address))?;
                        let server_name = ServerName::try_from(server_name.as_str())
                            .with_context(|| {
                                format!("syslog_tls invalid server name: {}", server_name)
                            })?
                            .to_owned();
                        let stream = tokio::time::timeout(
                            PEER_HANDSHAKE_TIMEOUT,
                            connector.connect(server_name, tcp),
                        )
                        .await
                        .with_context(|| {
                            format!("syslog_tls TLS handshake to {} timed out", address)
                        })?
                        .with_context(|| format!("syslog_tls TLS handshake to {}", address))?;
                        state.conn = Some(stream);
                    }

                    let stream = state.conn.as_mut().expect("connection should be present");
                    let write_result = tokio::time::timeout(
                        PEER_WRITE_TIMEOUT,
                        write_syslog_tls_framed(stream, framing, &egress),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("syslog_tls write to {} timed out", address))
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

async fn write_syslog_tls_framed(
    stream: &mut TlsStream<TcpStream>,
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
    use crate::dsl::ast::{ExprKind, Property};
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
        crate::modules::ModuleProperties::from_parts("syslog_tls", props.to_vec())
    }

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: crate::dsl::ast::Expr::spanless(kind),
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

    fn peer(host: &str, port: i64, tls: Option<Property>) -> Property {
        let mut props = vec![
            kv("host", ExprKind::StringLit(host.into())),
            kv("port", ExprKind::IntLit(port)),
        ];
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

    #[test]
    fn build_accepts_single_peer_no_tls() {
        let output = SyslogTlsOutput::build("relay", &mp(&[peer("example.com", 6514, None)]))
            .expect("should build");
        assert_eq!(output.peers.len(), 1);
        assert_eq!(output.connectors.len(), 1);
        assert_eq!(output.peers.peers()[0].address(), "example.com:6514");
    }

    #[test]
    fn build_accepts_single_peer_inline_tls_ca_only() {
        let files = pem_files();
        let output = SyslogTlsOutput::build(
            "relay",
            &mp(&[peer("example.com", 6514, Some(ca_block(&files.cert)))]),
        )
        .expect("should build");
        assert_eq!(
            output.peers.peers()[0].tls.as_ref().unwrap().ca_path,
            Some(files.cert)
        );
    }

    #[test]
    fn build_accepts_single_peer_inline_tls_mtls() {
        let files = pem_files();
        let output = SyslogTlsOutput::build(
            "relay",
            &mp(&[peer(
                "example.com",
                6514,
                Some(mtls_block(&files.cert, &files.key, &files.cert)),
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
        let output = SyslogTlsOutput::build(
            "relay",
            &mp(&[
                block(
                    "tls",
                    vec![block(
                        "my_p",
                        vec![kv("ca", ExprKind::StringLit(files.cert.clone()))],
                    )],
                ),
                peer(
                    "example.com",
                    6514,
                    Some(kv("tls", ExprKind::Ident(vec!["my_p".into()]))),
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
        let err = SyslogTlsOutput::build(
            "relay",
            &mp(&[peer(
                "example.com",
                6514,
                Some(kv("tls", ExprKind::Ident(vec!["missing".into()]))),
            )]),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().contains("unknown tls profile"), "{}", err);
    }

    #[test]
    fn build_rejects_cert_without_key() {
        let err = SyslogTlsOutput::build(
            "relay",
            &mp(&[peer(
                "example.com",
                6514,
                Some(tls_block(vec![kv(
                    "cert",
                    ExprKind::StringLit("x.crt".into()),
                )])),
            )]),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().contains("cert and key"), "{}", err);
    }

    #[test]
    fn build_rejects_key_without_cert() {
        let err = SyslogTlsOutput::build(
            "relay",
            &mp(&[peer(
                "example.com",
                6514,
                Some(tls_block(vec![kv(
                    "key",
                    ExprKind::StringLit("x.key".into()),
                )])),
            )]),
        )
        .err()
        .expect("should fail");
        assert!(err.to_string().contains("cert and key"), "{}", err);
    }

    #[test]
    fn build_accepts_multiple_peers_mixed_tls_forms() {
        let files = pem_files();
        let output = SyslogTlsOutput::build(
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
                        peer(
                            "a.example.com",
                            6514,
                            Some(kv("tls", ExprKind::Ident(vec!["named".into()]))),
                        ),
                        peer("b.example.com", 6514, Some(ca_block(&files.cert))),
                        peer("c.example.com", 6514, None),
                    ],
                ),
            ]),
        )
        .expect("should build");
        assert_eq!(output.peers.len(), 3);
        assert!(output.peers.peers()[0].tls.is_some());
        assert!(output.peers.peers()[1].tls.is_some());
        assert!(output.peers.peers()[2].tls.is_none());
    }

    #[test]
    fn build_rejects_peer_and_peers_together() {
        let err = SyslogTlsOutput::build(
            "relay",
            &mp(&[
                peer("a.example.com", 6514, None),
                block("peers", vec![peer("b.example.com", 6514, None)]),
            ]),
        )
        .err()
        .expect("should fail");
        let msg = err.to_string();
        assert!(msg.contains("exclusive group"), "{}", msg);
        assert!(msg.contains("peer") && msg.contains("peers"), "{}", msg);
    }

    #[test]
    fn build_rejects_missing_destination() {
        let err = SyslogTlsOutput::build("relay", &mp(&[]))
            .err()
            .expect("should fail");
        assert!(
            err.to_string()
                .contains("either 'peer' or 'peers' is required"),
            "{}",
            err
        );
    }
}
