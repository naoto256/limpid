//! Authenticated LTP listener.

use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use prost::Message as _;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, SubjectPublicKeyInfoDer, UnixTime};
use rustls::server::AlwaysResolvesServerRawPublicKeys;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme,
    client::danger::HandshakeSignatureValid,
};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::dsl::ast::{ExprKind, Property};
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::ltp::{
    ED25519_SPKI_PREFIX, FRAME_MAGIC, FRAME_PREFIX_SIZE, FRAME_VERSION, HopStamp, LtpHello,
    LtpMeta, MAX_META_LEN, MAX_PAYLOAD_LEN, PAYLOAD_LEN_SIZE, ValidatedNodeKey,
};
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

const DEFAULT_LTP_PORT: u16 = 7514;
const DEFAULT_MAX_HOPS: u64 = 16;
const DEFAULT_MAX_CONNECTIONS: u64 = 1024;
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(not(test))]
const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const HELLO_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(50);

const LTP_PEER_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "node_id",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "pubkey",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
];

const LTP_INPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "bind",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "peer",
        required: true,
        repeatable: true,
        exclusive_group: None,
        kind: PropertyValueKind::Block(LTP_PEER_SCHEMA),
    },
    PropertySpec {
        name: "max_hops",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "max_connections",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
];

#[derive(Clone)]
struct PeerRegistry {
    node_by_spki: Arc<HashMap<Vec<u8>, Arc<str>>>,
}

impl PeerRegistry {
    fn node_for_certificate(&self, certificate: &[u8]) -> Option<Arc<str>> {
        self.node_by_spki.get(certificate).cloned()
    }
}

pub struct LtpInput {
    name: String,
    bind_addr: String,
    node_id: Arc<str>,
    peers: PeerRegistry,
    acceptor: TlsAcceptor,
    max_hops: usize,
    max_connections: usize,
    metrics: Arc<InputMetrics>,
    now: fn() -> DateTime<Utc>,
    #[cfg(test)]
    hello_started: Option<Arc<tokio::sync::Notify>>,
}

#[derive(Clone)]
struct ConnectionContext {
    acceptor: TlsAcceptor,
    peers: PeerRegistry,
    node_id: Arc<str>,
    max_hops: usize,
    now: fn() -> DateTime<Utc>,
    metrics: Arc<InputMetrics>,
    tx: tokio::sync::mpsc::Sender<Event>,
    #[cfg(test)]
    hello_started: Option<Arc<tokio::sync::Notify>>,
}

impl Module for LtpInput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(LTP_INPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let properties = properties.user_properties();
        let node_id =
            ctx.ltp_node_id.as_ref().cloned().ok_or_else(|| {
                anyhow::anyhow!("input '{name}': LTP node identity is unavailable")
            })?;
        let node_key = ctx
            .ltp_node_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("input '{name}': LTP node key is unavailable"))?;
        let peers = parse_peers(name, properties)?;
        let acceptor = build_rpk_acceptor(node_key, &peers)?;
        let bind_addr = props::get_string(properties, "bind")
            .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_LTP_PORT}"));
        let max_hops = props::get_positive_int(properties, "max_hops")?.unwrap_or(DEFAULT_MAX_HOPS);
        if max_hops > DEFAULT_MAX_HOPS {
            bail!("input '{name}': max_hops must be between 1 and {DEFAULT_MAX_HOPS}");
        }
        let max_connections = props::get_positive_int(properties, "max_connections")?
            .unwrap_or(DEFAULT_MAX_CONNECTIONS);

        Ok(Self {
            name: name.to_owned(),
            bind_addr,
            node_id,
            peers,
            acceptor,
            max_hops: max_hops as usize,
            max_connections: max_connections as usize,
            metrics: InputMetrics::register(&ctx.metrics, name)?,
            now: Utc::now,
            #[cfg(test)]
            hello_started: None,
        })
    }
}

impl HasMetrics for LtpInput {
    type Stats = InputMetrics;

    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

impl LtpInput {
    async fn run_on_listener(
        self,
        listener: TcpListener,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        info!("ltp input '{}' listening on {}", self.name, self.bind_addr);
        let mut connections = Vec::<tokio::task::JoinHandle<()>>::new();

        loop {
            connections.retain(|handle| !handle.is_finished());
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        for handle in &connections {
                            handle.abort();
                        }
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let (stream, address) = accepted?;
                    connections.retain(|handle| !handle.is_finished());
                    if connections.len() >= self.max_connections {
                        warn!("ltp input '{}': max connections reached; rejecting {}", self.name, address);
                        continue;
                    }
                    let context = ConnectionContext {
                        acceptor: self.acceptor.clone(),
                        peers: self.peers.clone(),
                        node_id: Arc::clone(&self.node_id),
                        max_hops: self.max_hops,
                        now: self.now,
                        metrics: Arc::clone(&self.metrics),
                        tx: tx.clone(),
                        #[cfg(test)]
                        hello_started: self.hello_started.clone(),
                    };
                    let name = self.name.clone();
                    connections.push(tokio::spawn(async move {
                        let result = handle_connection(stream, address, context).await;
                        if let Err(error) = result {
                            debug!("ltp input '{}': closing {}: {error:#}", name, address);
                        }
                    }));
                }
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Input for LtpInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let listener = TcpListener::bind(&self.bind_addr).await?;
        self.run_on_listener(listener, tx, shutdown).await
    }
}

#[derive(Debug)]
struct DeclaredClientVerifier {
    allowed_spki: Arc<HashSet<Vec<u8>>>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for DeclaredClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, TlsError> {
        if !intermediates.is_empty() || !self.allowed_spki.contains(end_entity.as_ref()) {
            return Err(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ));
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        Err(TlsError::General("LTP requires TLS 1.3".to_owned()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

fn build_rpk_acceptor(node_key: &ValidatedNodeKey, peers: &PeerRegistry) -> Result<TlsAcceptor> {
    crate::tls::install_default_crypto_provider();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let verifier = DeclaredClientVerifier {
        allowed_spki: Arc::new(peers.node_by_spki.keys().cloned().collect()),
        algorithms: provider.signature_verification_algorithms,
    };
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("configure TLS 1.3 for LTP input")?
        .with_client_cert_verifier(Arc::new(verifier))
        .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(
            node_key.certified_key(),
        )));
    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn parse_peers(name: &str, properties: &[Property]) -> Result<PeerRegistry> {
    let mut by_spki = HashMap::new();
    let mut node_ids = HashSet::new();
    for peer in properties.iter().filter_map(|property| match property {
        Property::Block {
            key, properties, ..
        } if key == "peer" => Some(properties.as_slice()),
        _ => None,
    }) {
        let node_id = required_literal_string(name, peer, "node_id")?;
        if node_id.is_empty() {
            bail!("input '{name}': peer node_id must be non-empty");
        }
        if !node_ids.insert(node_id.clone()) {
            bail!("input '{name}': duplicate peer node_id '{node_id}'");
        }
        let encoded = required_literal_string(name, peer, "pubkey")?;
        let spki = decode_ed25519_spki(name, &encoded)?;
        if by_spki.insert(spki, Arc::<str>::from(node_id)).is_some() {
            bail!("input '{name}': duplicate peer pubkey");
        }
    }
    if by_spki.is_empty() {
        bail!("input '{name}': at least one peer block is required");
    }
    Ok(PeerRegistry {
        node_by_spki: Arc::new(by_spki),
    })
}

fn required_literal_string(name: &str, properties: &[Property], key: &str) -> Result<String> {
    let values: Vec<&str> = properties
        .iter()
        .filter_map(|property| match property {
            Property::KeyValue {
                key: found, value, ..
            } if found == key => match &value.kind {
                ExprKind::StringLit(value) => Some(value.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    match values.as_slice() {
        [value] => Ok((*value).to_owned()),
        [] => bail!("input '{name}': peer {key} requires a string value"),
        _ => bail!("input '{name}': duplicate peer {key}"),
    }
}

fn decode_ed25519_spki(name: &str, encoded: &str) -> Result<Vec<u8>> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .with_context(|| format!("input '{name}': peer pubkey is not valid base64"))?;
    if decoded.len() != ED25519_SPKI_PREFIX.len() + 32 || !decoded.starts_with(&ED25519_SPKI_PREFIX)
    {
        bail!("input '{name}': peer pubkey must be an Ed25519 SPKI DER value");
    }
    Ok(decoded)
}

struct RawFrame {
    metadata: Bytes,
    payload: Bytes,
}

async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    metrics: &InputMetrics,
) -> Result<Option<RawFrame>> {
    let mut prefix = [0u8; FRAME_PREFIX_SIZE];
    match reader.read_exact(&mut prefix[..1]).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    reader
        .read_exact(&mut prefix[1..])
        .await
        .context("truncated LTP frame prefix")?;
    if &prefix[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        bail!("invalid LTP frame magic");
    }
    if prefix[FRAME_MAGIC.len()] != FRAME_VERSION {
        bail!(
            "unsupported LTP frame version {}",
            prefix[FRAME_MAGIC.len()]
        );
    }
    let meta_len = u32::from_be_bytes(prefix[5..9].try_into().unwrap()) as usize;
    if meta_len > MAX_META_LEN {
        bail!("LTP metadata length {meta_len} exceeds the limit");
    }
    let mut metadata = vec![0u8; meta_len];
    reader
        .read_exact(&mut metadata)
        .await
        .context("truncated LTP metadata")?;
    let mut payload_len_bytes = [0u8; PAYLOAD_LEN_SIZE];
    reader
        .read_exact(&mut payload_len_bytes)
        .await
        .context("truncated LTP payload length")?;
    let payload_len = u32::from_be_bytes(payload_len_bytes) as usize;
    if payload_len > MAX_PAYLOAD_LEN {
        bail!("LTP payload length {payload_len} exceeds the limit");
    }
    let mut payload = vec![0u8; payload_len];
    let mut consumed = 0;
    while consumed < payload_len {
        let read = reader
            .read(&mut payload[consumed..])
            .await
            .context("failed to read LTP payload")?;
        if read == 0 {
            bail!("truncated LTP payload");
        }
        metrics.bytes_received.inc_by(read as u64);
        consumed += read;
    }
    Ok(Some(RawFrame {
        metadata: Bytes::from(metadata),
        payload: Bytes::from(payload),
    }))
}

async fn handle_connection(
    stream: TcpStream,
    address: std::net::SocketAddr,
    context: ConnectionContext,
) -> Result<()> {
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, context.acceptor.accept(stream))
        .await
        .context("LTP TLS handshake timed out")??;
    let peer_certificate = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| anyhow::anyhow!("LTP peer did not present a raw public key"))?;
    let authenticated_node_id = context
        .peers
        .node_for_certificate(peer_certificate.as_ref())
        .ok_or_else(|| anyhow::anyhow!("LTP peer raw public key is not declared"))?;

    #[cfg(test)]
    if let Some(hello_started) = &context.hello_started {
        hello_started.notify_one();
    }
    let hello_frame =
        tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut stream, &context.metrics))
            .await
            .context("LTP hello timed out")??
            .ok_or_else(|| anyhow::anyhow!("LTP peer closed before hello"))?;
    if !hello_frame.payload.is_empty() {
        bail!("LTP hello payload must be empty");
    }
    let hello = LtpHello::decode(hello_frame.metadata).context("invalid LTP hello metadata")?;
    if hello.node_id != authenticated_node_id.as_ref() {
        bail!("LTP hello node_id does not match the authenticated peer key");
    }

    loop {
        let frame = match read_frame(&mut stream, &context.metrics).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                context.metrics.events_invalid.inc();
                return Err(error);
            }
        };
        let meta = match LtpMeta::decode(frame.metadata).context("invalid LTP event metadata") {
            Ok(meta) => meta,
            Err(error) => {
                context.metrics.events_invalid.inc();
                return Err(error);
            }
        };
        let decision = match event_from_frame(
            meta,
            frame.payload,
            address,
            &context.node_id,
            context.max_hops,
            context.now,
        ) {
            Ok(decision) => decision,
            Err(error) => {
                context.metrics.events_invalid.inc();
                return Err(error);
            }
        };
        match decision {
            FrameDecision::Forward(event) => {
                if context.tx.send(event).await.is_err() {
                    break;
                }
                context.metrics.events_received.inc();
            }
            FrameDecision::DropCycle
            | FrameDecision::DropMaxHops
            | FrameDecision::DropMetadataTooLarge => {
                context.metrics.events_invalid.inc();
                continue;
            }
        }
    }
    Ok(())
}

enum FrameDecision {
    Forward(Event),
    DropCycle,
    DropMaxHops,
    DropMetadataTooLarge,
}

fn event_from_frame(
    mut meta: LtpMeta,
    payload: Bytes,
    source: std::net::SocketAddr,
    node_id: &str,
    max_hops: usize,
    now: fn() -> DateTime<Utc>,
) -> Result<FrameDecision> {
    let key: [u8; 16] = meta
        .key
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("LTP event key must be exactly 16 bytes"))?;
    let key = uuid::Uuid::from_bytes(key);
    if key.get_variant() != uuid::Variant::RFC4122 || key.get_version_num() != 7 {
        bail!("LTP event key must be an RFC 4122 UUIDv7");
    }
    if meta.stamps.iter().any(|stamp| stamp.node_id == node_id) {
        warn!("LTP event dropped because its hop history already contains this node");
        return Ok(FrameDecision::DropCycle);
    }
    if meta.stamps.len() >= max_hops {
        warn!("LTP event dropped because its hop history reached max_hops");
        return Ok(FrameDecision::DropMaxHops);
    }
    let received_at = now();
    let arrival_unix_nano = received_at
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or(0);
    meta.stamps.push(HopStamp {
        node_id: node_id.to_owned(),
        arrival_unix_nano,
        departure_unix_nano: 1,
    });
    if meta.encoded_len() > MAX_META_LEN {
        warn!("LTP event dropped because appending the local hop exceeds the metadata limit");
        return Ok(FrameDecision::DropMetadataTooLarge);
    }
    meta.stamps.last_mut().unwrap().departure_unix_nano = 0;
    Ok(FrameDecision::Forward(Event::from_ltp_parts(
        key,
        received_at,
        source,
        payload,
        meta.stamps,
    )))
}

pub(crate) fn validate_static_properties(name: &str, properties: &[Property]) -> Result<()> {
    parse_peers(name, properties)?;
    let max_hops = props::get_positive_int(properties, "max_hops")?.unwrap_or(DEFAULT_MAX_HOPS);
    if max_hops > DEFAULT_MAX_HOPS {
        bail!("input '{name}': max_hops must be between 1 and {DEFAULT_MAX_HOPS}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone as _;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use std::os::unix::fs::PermissionsExt as _;
    use tokio::io::AsyncWriteExt as _;
    use tokio_rustls::TlsConnector;

    use crate::dsl::ast::Expr;

    fn string_property(key: &str, value: &str) -> Property {
        Property::KeyValue {
            key: key.to_owned(),
            key_span: None,
            value: Expr::spanless(ExprKind::StringLit(value.to_owned())),
            value_span: None,
        }
    }

    fn int_property(key: &str, value: i64) -> Property {
        Property::KeyValue {
            key: key.to_owned(),
            key_span: None,
            value: Expr::spanless(ExprKind::IntLit(value)),
            value_span: None,
        }
    }

    fn peer_property(node_id: &str, pubkey: &str) -> Property {
        Property::Block {
            key: "peer".to_owned(),
            key_span: None,
            properties: vec![
                string_property("node_id", node_id),
                string_property("pubkey", pubkey),
            ],
        }
    }

    fn encoded_spki() -> String {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
        let mut spki = ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        base64::engine::general_purpose::STANDARD.encode(spki)
    }

    fn generated_identity() -> (ValidatedNodeKey, Vec<u8>) {
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap();
        let mut spki = ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("node.pem");
        std::fs::write(
            &path,
            pem::encode(&pem::Pem::new("PRIVATE KEY", document.as_ref())),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        (crate::ltp::load_node_key(&path).unwrap(), spki)
    }

    fn peer_registry(node_id: &str, spki: Vec<u8>) -> PeerRegistry {
        PeerRegistry {
            node_by_spki: Arc::new(HashMap::from([(spki, Arc::<str>::from(node_id))])),
        }
    }

    async fn connect_client(
        address: std::net::SocketAddr,
        identity: &ValidatedNodeKey,
        server_spki: &[u8],
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        let connector: TlsConnector =
            crate::modules::output::ltp::build_rpk_connector(identity, server_spki).unwrap();
        let tcp = TcpStream::connect(address).await.unwrap();
        connector
            .connect("localhost".try_into().unwrap(), tcp)
            .await
            .unwrap()
    }

    async fn send_client_event(
        address: std::net::SocketAddr,
        identity: &ValidatedNodeKey,
        server_spki: &[u8],
        key: [u8; 16],
        payload: &'static [u8],
    ) {
        let mut client = connect_client(address, identity, server_spki).await;
        client
            .write_all(
                &crate::ltp::encode_hello_frame(&LtpHello {
                    node_id: "peer-a".to_owned(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        client
            .write_all(
                &crate::ltp::encode_frame(
                    &LtpMeta {
                        key: key.to_vec(),
                        stamps: Vec::new(),
                    },
                    payload,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        client.shutdown().await.unwrap();
    }

    fn fixed_now() -> DateTime<Utc> {
        Utc.timestamp_nanos(123)
    }

    fn v7_key(seed: u8) -> [u8; 16] {
        let mut key = [seed; 16];
        key[6] = (key[6] & 0x0f) | 0x70;
        key[8] = (key[8] & 0x3f) | 0x80;
        key
    }

    fn test_metrics() -> Arc<InputMetrics> {
        InputMetrics::for_testing()
    }

    fn node_id_for_departed_meta_len(target: usize) -> String {
        ((target - 128)..=target)
            .find_map(|len| {
                let node_id = "n".repeat(len);
                let meta = LtpMeta {
                    key: v7_key(9).to_vec(),
                    stamps: vec![HopStamp {
                        node_id: node_id.clone(),
                        arrival_unix_nano: 123,
                        departure_unix_nano: 1,
                    }],
                };
                (meta.encoded_len() == target).then_some(node_id)
            })
            .expect("a node_id length must realize the requested metadata boundary")
    }

    #[test]
    fn peer_registry_rejects_duplicate_node_ids_and_public_keys() {
        let first = encoded_spki();
        let second = encoded_spki();
        let duplicate_node = vec![
            peer_property("peer-a", &first),
            peer_property("peer-a", &second),
        ];
        assert!(
            parse_peers("in", &duplicate_node)
                .err()
                .unwrap()
                .to_string()
                .contains("duplicate peer node_id")
        );

        let duplicate_key = vec![
            peer_property("peer-a", &first),
            peer_property("peer-b", &first),
        ];
        assert!(
            parse_peers("in", &duplicate_key)
                .err()
                .unwrap()
                .to_string()
                .contains("duplicate peer pubkey")
        );
    }

    #[test]
    fn static_validation_bounds_max_hops_at_sixteen() {
        let peer = peer_property("peer-a", &encoded_spki());
        assert!(validate_static_properties("in", std::slice::from_ref(&peer)).is_ok());
        assert!(
            validate_static_properties("in", &[peer.clone(), int_property("max_hops", 16)]).is_ok()
        );
        assert!(validate_static_properties("in", &[peer, int_property("max_hops", 17)]).is_err());
    }

    #[test]
    fn event_validation_requires_uuid_v7_key_and_orders_cycle_before_hop_limit() {
        let source = "127.0.0.1:7514".parse().unwrap();
        for key_len in [0, 15, 17] {
            let meta = LtpMeta {
                key: vec![7; key_len],
                stamps: Vec::new(),
            };
            assert!(event_from_frame(meta, Bytes::new(), source, "self", 16, fixed_now).is_err());
        }

        for invalid_key in [
            [0; 16],
            {
                let mut key = v7_key(4);
                key[6] = (key[6] & 0x0f) | 0x40;
                key
            },
            {
                let mut key = v7_key(7);
                key[8] &= 0x3f;
                key
            },
        ] {
            assert!(
                event_from_frame(
                    LtpMeta {
                        key: invalid_key.to_vec(),
                        stamps: Vec::new(),
                    },
                    Bytes::new(),
                    source,
                    "self",
                    16,
                    fixed_now,
                )
                .is_err()
            );
        }

        let cycle = LtpMeta {
            key: v7_key(7).to_vec(),
            stamps: vec![HopStamp {
                node_id: "self".to_owned(),
                arrival_unix_nano: 1,
                departure_unix_nano: 2,
            }],
        };
        assert!(matches!(
            event_from_frame(cycle, Bytes::new(), source, "self", 1, fixed_now).unwrap(),
            FrameDecision::DropCycle
        ));

        let full = LtpMeta {
            key: v7_key(7).to_vec(),
            stamps: vec![HopStamp {
                node_id: "peer".to_owned(),
                arrival_unix_nano: 1,
                departure_unix_nano: 2,
            }],
        };
        assert!(matches!(
            event_from_frame(full, Bytes::new(), source, "self", 1, fixed_now).unwrap(),
            FrameDecision::DropMaxHops
        ));
    }

    #[test]
    fn accepted_event_preserves_key_payload_and_appends_local_arrival() {
        let key = v7_key(9);
        let peer_stamp = HopStamp {
            node_id: "peer".to_owned(),
            arrival_unix_nano: 10,
            departure_unix_nano: 11,
        };
        let source = "127.0.0.1:7514".parse().unwrap();
        let decision = event_from_frame(
            LtpMeta {
                key: key.to_vec(),
                stamps: vec![peer_stamp.clone()],
            },
            Bytes::from_static(b"payload"),
            source,
            "self",
            16,
            fixed_now,
        )
        .unwrap();
        let FrameDecision::Forward(event) = decision else {
            panic!("valid frame was dropped");
        };
        assert_eq!(event.key().as_bytes(), &key);
        assert_eq!(event.ingress, Bytes::from_static(b"payload"));
        assert_eq!(event.egress, Bytes::from_static(b"payload"));
        assert_eq!(event.ltp_stamps()[0], peer_stamp);
        assert_eq!(
            event.ltp_stamps()[1],
            HopStamp {
                node_id: "self".to_owned(),
                arrival_unix_nano: 123,
                departure_unix_nano: 0,
            }
        );
    }

    #[test]
    fn acceptance_reserves_nonzero_departure_and_exact_limit_encodes_for_output() {
        let source = "127.0.0.1:7514".parse().unwrap();
        let exact_node = node_id_for_departed_meta_len(MAX_META_LEN);
        let FrameDecision::Forward(event) = event_from_frame(
            LtpMeta {
                key: v7_key(9).to_vec(),
                stamps: Vec::new(),
            },
            Bytes::from_static(b"payload"),
            source,
            &exact_node,
            16,
            fixed_now,
        )
        .unwrap() else {
            panic!("metadata at the exact output boundary must be accepted");
        };
        assert_eq!(event.ltp_stamps().last().unwrap().departure_unix_nano, 0);

        let mut wire_meta = LtpMeta {
            key: event.key().as_bytes().to_vec(),
            stamps: event.ltp_stamps().to_vec(),
        };
        wire_meta.stamps.last_mut().unwrap().departure_unix_nano = 1;
        assert_eq!(wire_meta.encoded_len(), MAX_META_LEN);
        assert!(crate::ltp::encode_frame(&wire_meta, &event.egress).is_ok());

        let oversized_node = node_id_for_departed_meta_len(MAX_META_LEN + 1);
        assert!(matches!(
            event_from_frame(
                LtpMeta {
                    key: v7_key(9).to_vec(),
                    stamps: Vec::new(),
                },
                Bytes::new(),
                source,
                &oversized_node,
                16,
                fixed_now,
            )
            .unwrap(),
            FrameDecision::DropMetadataTooLarge
        ));
    }

    #[tokio::test]
    async fn frame_reader_rejects_lengths_before_reading_their_bodies() {
        let metrics = test_metrics();
        let mut metadata_prefix = Vec::from(FRAME_MAGIC);
        metadata_prefix.push(FRAME_VERSION);
        metadata_prefix.extend_from_slice(&((MAX_META_LEN + 1) as u32).to_be_bytes());
        let mut metadata_reader = &metadata_prefix[..];
        assert!(
            read_frame(&mut metadata_reader, &metrics)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("metadata length")
        );

        let mut payload_prefix = Vec::from(FRAME_MAGIC);
        payload_prefix.push(FRAME_VERSION);
        payload_prefix.extend_from_slice(&0u32.to_be_bytes());
        payload_prefix.extend_from_slice(&((MAX_PAYLOAD_LEN + 1) as u32).to_be_bytes());
        let mut payload_reader = &payload_prefix[..];
        assert!(
            read_frame(&mut payload_reader, &metrics)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("payload length")
        );

        let mut partial = Vec::from(FRAME_MAGIC);
        partial.push(FRAME_VERSION);
        partial.extend_from_slice(&0u32.to_be_bytes());
        partial.extend_from_slice(&5u32.to_be_bytes());
        partial.extend_from_slice(b"abc");
        let partial_metrics = test_metrics();
        assert!(
            read_frame(&mut &partial[..], &partial_metrics)
                .await
                .is_err()
        );
        assert_eq!(
            partial_metrics
                .bytes_received
                .load(std::sync::atomic::Ordering::Relaxed),
            3
        );
    }

    #[tokio::test]
    async fn frame_reader_consumes_multiple_frames_without_trailing_coalescence() {
        let metrics = test_metrics();
        let first = crate::ltp::encode_hello_frame(&LtpHello {
            node_id: "peer".to_owned(),
        })
        .unwrap();
        let second = crate::ltp::encode_frame(
            &LtpMeta {
                key: v7_key(3).to_vec(),
                stamps: Vec::new(),
            },
            b"payload",
        )
        .unwrap();
        let (mut writer, mut reader) = tokio::io::duplex(first.len() + second.len());
        writer.write_all(&first).await.unwrap();
        writer.write_all(&second).await.unwrap();
        drop(writer);

        let hello = read_frame(&mut reader, &metrics).await.unwrap().unwrap();
        assert_eq!(LtpHello::decode(hello.metadata).unwrap().node_id, "peer");
        assert!(hello.payload.is_empty());
        let event = read_frame(&mut reader, &metrics).await.unwrap().unwrap();
        assert_eq!(event.payload, Bytes::from_static(b"payload"));
        assert!(read_frame(&mut reader, &metrics).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn mutual_rpk_hello_binding_and_multi_frame_delivery_are_enforced() {
        let (server_identity, server_spki) = generated_identity();
        let (client_identity, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let metrics = test_metrics();
        let server_metrics = Arc::clone(&metrics);
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                ConnectionContext {
                    acceptor,
                    peers,
                    node_id: Arc::<str>::from("self"),
                    max_hops: 16,
                    now: fixed_now,
                    metrics: server_metrics,
                    tx,
                    hello_started: None,
                },
            )
            .await
        });

        let mut client = connect_client(address, &client_identity, &server_spki).await;
        client
            .write_all(
                &crate::ltp::encode_hello_frame(&LtpHello {
                    node_id: "peer-a".to_owned(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        for (key, payload) in [
            (v7_key(1), b"first".as_slice()),
            (v7_key(2), b"second".as_slice()),
        ] {
            client
                .write_all(
                    &crate::ltp::encode_frame(
                        &LtpMeta {
                            key: key.to_vec(),
                            stamps: Vec::new(),
                        },
                        payload,
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        client.flush().await.unwrap();
        client.shutdown().await.unwrap();

        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        assert_eq!(first.key().as_bytes(), &v7_key(1));
        assert_eq!(first.ingress, Bytes::from_static(b"first"));
        assert_eq!(second.key().as_bytes(), &v7_key(2));
        assert_eq!(second.ingress, Bytes::from_static(b"second"));
        assert!(server.await.unwrap().is_ok());
        assert_eq!(
            metrics
                .bytes_received
                .load(std::sync::atomic::Ordering::Relaxed),
            11
        );
        assert_eq!(
            metrics
                .events_received
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert_eq!(
            metrics
                .events_invalid
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn invalid_event_counts_consumed_payload_once_without_receipt() {
        let (server_identity, server_spki) = generated_identity();
        let (client_identity, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let metrics = test_metrics();
        let server_metrics = Arc::clone(&metrics);
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                ConnectionContext {
                    acceptor,
                    peers,
                    node_id: Arc::<str>::from("self"),
                    max_hops: 16,
                    now: fixed_now,
                    metrics: server_metrics,
                    tx,
                    hello_started: None,
                },
            )
            .await
        });

        let mut client = connect_client(address, &client_identity, &server_spki).await;
        client
            .write_all(
                &crate::ltp::encode_hello_frame(&LtpHello {
                    node_id: "peer-a".to_owned(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        client
            .write_all(
                &crate::ltp::encode_frame(
                    &LtpMeta {
                        key: vec![0; 16],
                        stamps: Vec::new(),
                    },
                    b"bad",
                )
                .unwrap(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();

        assert!(server.await.unwrap().is_err());
        assert!(rx.try_recv().is_err());
        assert_eq!(
            metrics
                .bytes_received
                .load(std::sync::atomic::Ordering::Relaxed),
            3
        );
        assert_eq!(
            metrics
                .events_invalid
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_eq!(
            metrics
                .events_received
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn closed_pipeline_channel_does_not_count_a_receipt_or_invalid_event() {
        let (server_identity, server_spki) = generated_identity();
        let (client_identity, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let metrics = test_metrics();
        let server_metrics = Arc::clone(&metrics);
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                ConnectionContext {
                    acceptor,
                    peers,
                    node_id: Arc::<str>::from("self"),
                    max_hops: 16,
                    now: fixed_now,
                    metrics: server_metrics,
                    tx,
                    hello_started: None,
                },
            )
            .await
        });

        send_client_event(
            address,
            &client_identity,
            &server_spki,
            v7_key(3),
            b"payload",
        )
        .await;
        assert!(server.await.unwrap().is_ok());
        assert_eq!(
            metrics
                .bytes_received
                .load(std::sync::atomic::Ordering::Relaxed),
            7
        );
        assert_eq!(
            metrics
                .events_received
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            metrics
                .events_invalid
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn hello_timeout_closes_stalled_connection_and_releases_the_slot() {
        let (server_identity, server_spki) = generated_identity();
        let (client_identity, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let metrics = test_metrics();
        let hello_started = Arc::new(tokio::sync::Notify::new());
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: address.to_string(),
            node_id: Arc::<str>::from("self"),
            peers,
            acceptor,
            max_hops: 16,
            max_connections: 1,
            metrics,
            now: fixed_now,
            hello_started: Some(Arc::clone(&hello_started)),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(input.run_on_listener(listener, tx, shutdown_rx));

        let mut stalled = connect_client(address, &client_identity, &server_spki).await;
        hello_started.notified().await;
        let mut closed_probe = [0u8; 1];
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            stalled.read(&mut closed_probe),
        )
        .await
        .expect("hello timeout must close the stalled connection");

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            send_client_event(
                address,
                &client_identity,
                &server_spki,
                v7_key(4),
                b"after-timeout",
            ),
        )
        .await
        .expect("the released slot must accept a new TLS connection");
        assert_eq!(
            rx.recv().await.unwrap().ingress,
            Bytes::from_static(b"after-timeout")
        );
        shutdown_tx.send(true).unwrap();
        assert!(server.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn hello_node_id_must_match_the_authenticated_public_key() {
        let (server_identity, server_spki) = generated_identity();
        let (client_identity, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let metrics = test_metrics();
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                ConnectionContext {
                    acceptor,
                    peers,
                    node_id: Arc::<str>::from("self"),
                    max_hops: 16,
                    now: fixed_now,
                    metrics,
                    tx,
                    hello_started: None,
                },
            )
            .await
        });

        let mut client = connect_client(address, &client_identity, &server_spki).await;
        client
            .write_all(
                &crate::ltp::encode_hello_frame(&LtpHello {
                    node_id: "different-peer".to_owned(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();
        drop(client);

        let error = server.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("hello node_id"));
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tls_handshake_rejects_an_undeclared_client_public_key() {
        let (server_identity, server_spki) = generated_identity();
        let (_, declared_spki) = generated_identity();
        let (unknown_identity, _) = generated_identity();
        let peers = peer_registry("peer-a", declared_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            acceptor.accept(stream).await
        });

        let connector =
            crate::modules::output::ltp::build_rpk_connector(&unknown_identity, &server_spki)
                .unwrap();
        let tcp = TcpStream::connect(address).await.unwrap();
        let client_result = connector
            .connect("localhost".try_into().unwrap(), tcp)
            .await;
        let server_result = server.await.unwrap();
        assert!(client_result.is_err() || server_result.is_err());
        assert!(server_result.is_err());
    }

    #[tokio::test]
    async fn parallel_connections_obey_channel_backpressure() {
        let (server_identity, server_spki) = generated_identity();
        let (client_identity, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let metrics = test_metrics();
        let server = tokio::spawn(async move {
            let mut connections = Vec::new();
            for _ in 0..2 {
                let (stream, peer_address) = listener.accept().await.unwrap();
                connections.push(tokio::spawn(handle_connection(
                    stream,
                    peer_address,
                    ConnectionContext {
                        acceptor: acceptor.clone(),
                        peers: peers.clone(),
                        node_id: Arc::<str>::from("self"),
                        max_hops: 16,
                        now: fixed_now,
                        metrics: Arc::clone(&metrics),
                        tx: tx.clone(),
                        hello_started: None,
                    },
                )));
            }
            let first = connections.remove(0).await.unwrap();
            let second = connections.remove(0).await.unwrap();
            (first, second)
        });

        let first = send_client_event(address, &client_identity, &server_spki, v7_key(1), b"one");
        let second = send_client_event(address, &client_identity, &server_spki, v7_key(2), b"two");
        tokio::join!(first, second);
        for _ in 0..100 {
            if rx.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(rx.len(), 1);
        assert!(
            !server.is_finished(),
            "one sender must wait for channel capacity"
        );

        let mut payloads = vec![
            rx.recv().await.unwrap().ingress,
            rx.recv().await.unwrap().ingress,
        ];
        payloads.sort();
        assert_eq!(
            payloads,
            [Bytes::from_static(b"one"), Bytes::from_static(b"two")]
        );
        let (first, second) = server.await.unwrap();
        assert!(first.is_ok());
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn listener_shutdown_aborts_the_accept_loop_without_waiting_for_connections() {
        let (server_identity, _) = generated_identity();
        let (_, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let acceptor = build_rpk_acceptor(&server_identity, &peers).unwrap();
        let context = crate::modules::BuildContext::for_testing();
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: "127.0.0.1:0".to_owned(),
            node_id: Arc::<str>::from("self"),
            peers,
            acceptor,
            max_hops: 16,
            max_connections: 2,
            metrics: InputMetrics::register(&context.metrics, "in").unwrap(),
            now: fixed_now,
            hello_started: None,
        };
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(input.run(event_tx, shutdown_rx));
        shutdown_tx.send(true).unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("listener must stop on shutdown")
            .unwrap();
        assert!(result.is_ok());
    }
}
