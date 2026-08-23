//! Authenticated LTP listener.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use bytes::Bytes;
#[cfg(test)]
use chrono::Utc;
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
use crate::metrics::{InputMetrics, LtpMetrics, LtpPeerMetrics};
use crate::modules::{HasMetrics, Input, Module};

const DEFAULT_LTP_PORT: u16 = 7514;
const DEFAULT_MAX_HOPS: u64 = 16;
const DEFAULT_MAX_CONNECTIONS: u64 = 1024;
const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const ACCEPT_RETRY_INITIAL: std::time::Duration = std::time::Duration::from_millis(100);
const ACCEPT_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(5);
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

pub struct LtpInput {
    name: String,
    bind_addr: String,
    node_id: Arc<str>,
    node_key: Arc<ValidatedNodeKey>,
    peers: PeerRegistry,
    max_hops: usize,
    max_connections: usize,
    metrics: Arc<InputMetrics>,
    ltp_metrics: Arc<LtpMetrics>,
    now: fn() -> crate::time::ClockSample,
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
        let ltp_metrics = ctx
            .ltp_metrics
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("input '{name}': LTP metrics are unavailable"))?;
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
            node_key: Arc::clone(node_key),
            peers,
            max_hops: max_hops as usize,
            max_connections: max_connections as usize,
            metrics: InputMetrics::register(&ctx.metrics, name)?,
            ltp_metrics,
            now: crate::time::ClockSample::now,
            #[cfg(test)]
            hello_started: None,
        })
    }
}

#[derive(Clone)]
struct LogicalInputContext {
    name: Arc<str>,
    node_id: Arc<str>,
    max_hops: usize,
    now: fn() -> crate::time::ClockSample,
    metrics: Arc<InputMetrics>,
    ltp_metrics: Arc<LtpMetrics>,
    tx: tokio::sync::mpsc::Sender<Event>,
    #[cfg(test)]
    hello_started: Option<Arc<tokio::sync::Notify>>,
}

#[derive(Clone)]
struct PeerRoute {
    expected_node_id: Arc<str>,
    input: LogicalInputContext,
}

struct SharedListenerGroup {
    bind_addr: String,
    member_names: Vec<String>,
    acceptor: TlsAcceptor,
    routes: Arc<HashMap<Vec<u8>, PeerRoute>>,
    max_connections: usize,
    ltp_metrics: Arc<LtpMetrics>,
}

#[async_trait::async_trait]
trait ListenerAccept {
    type Connection: Send;

    async fn accept(&self) -> io::Result<Self::Connection>;
}

#[async_trait::async_trait]
impl ListenerAccept for TcpListener {
    type Connection = (TcpStream, SocketAddr);

    async fn accept(&self) -> io::Result<Self::Connection> {
        TcpListener::accept(self).await
    }
}

async fn run_accept_loop<A, F>(
    accept_source: A,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    group_names: &str,
    mut on_accepted: F,
) where
    A: ListenerAccept + Sync,
    F: FnMut(A::Connection),
{
    let mut retry_delay = ACCEPT_RETRY_INITIAL;
    loop {
        tokio::select! {
            biased;
            _ = wait_for_shutdown(&mut shutdown) => {
                break;
            }
            accepted = accept_source.accept() => {
                match accepted {
                    Ok(connection) => {
                        retry_delay = ACCEPT_RETRY_INITIAL;
                        on_accepted(connection);
                    }
                    Err(error) => {
                        warn!(
                            "ltp listener group [{}]: accept failed: {error}; retrying in {} ms",
                            group_names,
                            retry_delay.as_millis()
                        );
                        tokio::select! {
                            biased;
                            _ = wait_for_shutdown(&mut shutdown) => {
                                break;
                            }
                            _ = tokio::time::sleep(retry_delay) => {}
                        }
                        retry_delay =
                            retry_delay.saturating_mul(2).min(ACCEPT_RETRY_MAX);
                    }
                }
            }
        }
    }
}

async fn wait_for_shutdown(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

impl HasMetrics for LtpInput {
    type Stats = InputMetrics;

    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

impl SharedListenerGroup {
    fn from_members(members: Vec<(LtpInput, tokio::sync::mpsc::Sender<Event>)>) -> Result<Self> {
        let first = members
            .first()
            .ok_or_else(|| anyhow::anyhow!("LTP listener group has no logical inputs"))?;
        let bind_addr = first.0.bind_addr.clone();
        let max_connections = first.0.max_connections;
        let node_key = Arc::clone(&first.0.node_key);
        let ltp_metrics = Arc::clone(&first.0.ltp_metrics);
        let mut member_names = Vec::with_capacity(members.len());
        let mut routes = HashMap::new();

        for (input, tx) in members {
            if input.bind_addr != bind_addr {
                bail!("internal error: LTP listener group contains different binds");
            }
            if input.max_connections != max_connections {
                bail!("internal error: LTP listener group contains different limits");
            }
            member_names.push(input.name.clone());
            let logical = LogicalInputContext {
                name: Arc::<str>::from(input.name),
                node_id: input.node_id,
                max_hops: input.max_hops,
                now: input.now,
                metrics: input.metrics,
                ltp_metrics: Arc::clone(&input.ltp_metrics),
                tx,
                #[cfg(test)]
                hello_started: input.hello_started,
            };
            for (spki, expected_node_id) in input.peers.node_by_spki.iter() {
                let route = PeerRoute {
                    expected_node_id: Arc::clone(expected_node_id),
                    input: logical.clone(),
                };
                if routes.insert(spki.clone(), route).is_some() {
                    bail!("internal error: duplicate LTP peer public key in listener group");
                }
            }
        }
        member_names.sort();
        let peers = PeerRegistry {
            node_by_spki: Arc::new(
                routes
                    .iter()
                    .map(|(spki, route)| (spki.clone(), Arc::clone(&route.expected_node_id)))
                    .collect(),
            ),
        };
        let acceptor = build_rpk_acceptor(&node_key, &peers, Arc::clone(&ltp_metrics))?;
        Ok(Self {
            bind_addr,
            member_names,
            acceptor,
            routes: Arc::new(routes),
            max_connections,
            ltp_metrics,
        })
    }

    async fn bind(self) -> Result<(Self, TcpListener)> {
        let listener = TcpListener::bind(&self.bind_addr).await.with_context(|| {
            format!(
                "LTP listener group [{}] failed to bind '{}'",
                self.member_names.join(", "),
                self.bind_addr
            )
        })?;
        Ok((self, listener))
    }

    async fn run_on_listener(
        self,
        listener: TcpListener,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        info!(
            "ltp listener group [{}] listening on {}",
            self.member_names.join(", "),
            self.bind_addr
        );
        let mut connections = Vec::<tokio::task::JoinHandle<()>>::new();
        let names = self.member_names.join(", ");
        run_accept_loop(listener, shutdown, &names, |(stream, address)| {
            connections.retain(|handle| !handle.is_finished());
            if connections.len() >= self.max_connections {
                warn!(
                    "ltp listener group [{}]: max connections reached; rejecting {}",
                    names, address
                );
                return;
            }
            let acceptor = self.acceptor.clone();
            let routes = Arc::clone(&self.routes);
            let ltp_metrics = Arc::clone(&self.ltp_metrics);
            let connection_names = names.clone();
            connections.push(tokio::spawn(async move {
                let result =
                    handle_connection(stream, address, acceptor, routes, ltp_metrics).await;
                if let Err(error) = result {
                    debug!(
                        "ltp listener group '[{}]': closing {}: {error:#}",
                        connection_names, address
                    );
                }
            }));
        })
        .await;
        for handle in &connections {
            handle.abort();
        }
        Ok(())
    }
}

/// Bind every distinct LTP address before spawning any listener task, then run
/// one listener for each exact bind string. This keeps bind failure fail-closed
/// and prevents same-address logical inputs from racing each other at startup.
pub(crate) async fn start_listener_groups(
    inputs: Vec<(LtpInput, tokio::sync::mpsc::Sender<Event>)>,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<Vec<tokio::task::JoinHandle<()>>> {
    let mut grouped = BTreeMap::<String, Vec<_>>::new();
    for (input, tx) in inputs {
        grouped
            .entry(input.bind_addr.clone())
            .or_default()
            .push((input, tx));
    }

    let mut bound = Vec::with_capacity(grouped.len());
    for (_, members) in grouped {
        let group = SharedListenerGroup::from_members(members)?;
        bound.push(group.bind().await?);
    }

    Ok(bound
        .into_iter()
        .map(|(group, listener)| {
            let group_names = group.member_names.join(", ");
            let group_shutdown = shutdown.clone();
            tokio::spawn(async move {
                report_listener_result(
                    &group_names,
                    group.run_on_listener(listener, group_shutdown).await,
                );
            })
        })
        .collect())
}

fn report_listener_result(group_names: &str, result: Result<()>) {
    if let Err(error) = result {
        warn!("ltp listener group [{}] failed: {error:#}", group_names);
    }
}

#[async_trait::async_trait]
impl Input for LtpInput {
    async fn run(
        self,
        tx: tokio::sync::mpsc::Sender<Event>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let (group, listener) = SharedListenerGroup::from_members(vec![(self, tx)])?
            .bind()
            .await?;
        group.run_on_listener(listener, shutdown).await
    }
}

struct DeclaredClientVerifier {
    allowed_spki: Arc<HashSet<Vec<u8>>>,
    algorithms: WebPkiSupportedAlgorithms,
    ltp_metrics: Arc<LtpMetrics>,
}

impl fmt::Debug for DeclaredClientVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeclaredClientVerifier")
            .field("allowed_spki", &self.allowed_spki)
            .field("algorithms", &self.algorithms)
            .finish_non_exhaustive()
    }
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
        if !intermediates.is_empty() {
            return Err(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ));
        }
        if !self.allowed_spki.contains(end_entity.as_ref()) {
            let candidate = end_entity.as_ref();
            if candidate.len() == ED25519_SPKI_PREFIX.len() + 32
                && candidate.starts_with(&ED25519_SPKI_PREFIX)
            {
                self.ltp_metrics.rejected_unknown_peer.inc();
            }
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

fn build_rpk_acceptor(
    node_key: &ValidatedNodeKey,
    peers: &PeerRegistry,
    ltp_metrics: Arc<LtpMetrics>,
) -> Result<TlsAcceptor> {
    crate::tls::install_default_crypto_provider();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let verifier = DeclaredClientVerifier {
        allowed_spki: Arc::new(peers.node_by_spki.keys().cloned().collect()),
        algorithms: provider.signature_verification_algorithms,
        ltp_metrics,
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
    read_frame_inner(reader, Some(metrics)).await
}

async fn read_frame_unmetered<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<RawFrame>> {
    read_frame_inner(reader, None).await
}

async fn read_frame_inner<R: AsyncRead + Unpin>(
    reader: &mut R,
    metrics: Option<&InputMetrics>,
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
        if let Some(metrics) = metrics {
            metrics.bytes_received.inc_by(read as u64);
        }
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
    acceptor: TlsAcceptor,
    routes: Arc<HashMap<Vec<u8>, PeerRoute>>,
    ltp_metrics: Arc<LtpMetrics>,
) -> Result<()> {
    let mut stream = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream))
        .await
        .context("LTP TLS handshake timed out")??;
    let peer_certificate = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certificates| certificates.first())
        .ok_or_else(|| anyhow::anyhow!("LTP peer did not present a raw public key"))?;
    let route = routes
        .get(peer_certificate.as_ref())
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("LTP peer raw public key is not declared"))?;

    #[cfg(test)]
    if let Some(hello_started) = &route.input.hello_started {
        hello_started.notify_one();
    }
    // The HELLO selects and verifies the logical input, but is not an event
    // payload and therefore must not contribute to that input's byte counter.
    let hello_frame = tokio::time::timeout(HELLO_TIMEOUT, read_frame_unmetered(&mut stream))
        .await
        .context("LTP hello timed out")??
        .ok_or_else(|| anyhow::anyhow!("LTP peer closed before hello"))?;
    if !hello_frame.payload.is_empty() {
        bail!("LTP hello payload must be empty");
    }
    let hello = LtpHello::decode(hello_frame.metadata).context("invalid LTP hello metadata")?;
    if hello.node_id != route.expected_node_id.as_ref() {
        ltp_metrics.rejected_unknown_peer.inc();
        bail!(
            "LTP hello node_id does not match the authenticated peer key for input '{}'",
            route.input.name
        );
    }
    let peer_metrics = route
        .input
        .ltp_metrics
        .peer(&route.expected_node_id)
        .ok_or_else(|| anyhow::anyhow!("LTP peer metrics were not registered"))?;

    // PR #200 decision: https://github.com/naoto256/limpid/pull/200#issuecomment-5355435303
    // After mutual-RPK authentication and the bound hello, an idle connection is a valid
    // persistent steady state. LTP has no application-level acknowledgement, so the output
    // caches this TLS stream and treats local write/flush success as delivery. If the server
    // closes an idle connection, the next local write can still be reported delivered while
    // its event is silently lost. HELLO_TIMEOUT therefore bounds only pre-protocol state;
    // max_connections bounds steady-state resources. An application-level acknowledgement
    // would invalidate this premise and require revisiting the idle-connection policy.
    loop {
        let frame = match read_frame(&mut stream, &route.input.metrics).await {
            Ok(Some(frame)) => frame,
            Ok(None) => break,
            Err(error) => {
                route.input.metrics.events_invalid.inc();
                return Err(error);
            }
        };
        let meta = match LtpMeta::decode(frame.metadata).context("invalid LTP event metadata") {
            Ok(meta) => meta,
            Err(error) => {
                route.input.metrics.events_invalid.inc();
                return Err(error);
            }
        };
        let decision = match event_from_frame(
            meta,
            frame.payload,
            address,
            &route.input.node_id,
            route.input.max_hops,
            route.input.now,
            &peer_metrics,
        ) {
            Ok(decision) => decision,
            Err(error) => {
                route.input.metrics.events_invalid.inc();
                return Err(error);
            }
        };
        match decision {
            FrameDecision::Forward(event) => {
                if route.input.tx.send(event).await.is_err() {
                    break;
                }
                route.input.metrics.events_received.inc();
            }
            FrameDecision::DropCycle
            | FrameDecision::DropMaxHops
            | FrameDecision::DropMetadataTooLarge => {
                route.input.metrics.events_invalid.inc();
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
    now: fn() -> crate::time::ClockSample,
    peer_metrics: &LtpPeerMetrics,
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
        peer_metrics.loop_dropped.inc();
        warn!("LTP event dropped because its hop history already contains this node");
        return Ok(FrameDecision::DropCycle);
    }
    if meta.stamps.len() >= max_hops {
        peer_metrics.loop_dropped.inc();
        warn!("LTP event dropped because its hop history reached max_hops");
        return Ok(FrameDecision::DropMaxHops);
    }
    let received = now();
    let received_at = received.utc;
    let arrival_unix_nano = received.unix_nanos.to_wire_u64();
    if let Some(previous) = meta.stamps.last()
        && previous.departure_unix_nano != 0
    {
        let elapsed =
            crate::time::ElapsedNanos::between_u64(arrival_unix_nano, previous.departure_unix_nano);
        if elapsed.reversed {
            peer_metrics.negative_delta.inc();
        }
        peer_metrics.network_latency.observe(elapsed.duration);
    }
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

struct ListenerSpec {
    name: String,
    bind_addr: String,
    max_connections: u64,
    peers: PeerRegistry,
}

/// Validate relationships that are only visible across logical LTP inputs.
/// Exact bind strings form a listener group; address normalization is
/// intentionally not used to merge operator declarations.
pub(crate) fn validate_listener_groups<'a>(
    inputs: impl Iterator<Item = (&'a str, &'a [Property])>,
) -> Result<()> {
    let mut specs = inputs
        .map(|(name, properties)| {
            Ok(ListenerSpec {
                name: name.to_owned(),
                bind_addr: props::get_string(properties, "bind")
                    .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_LTP_PORT}")),
                max_connections: props::get_positive_int(properties, "max_connections")?
                    .unwrap_or(DEFAULT_MAX_CONNECTIONS),
                peers: parse_peers(name, properties)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    specs.sort_by(|left, right| left.name.cmp(&right.name));

    let mut groups = BTreeMap::<&str, Vec<&ListenerSpec>>::new();
    for spec in &specs {
        groups.entry(&spec.bind_addr).or_default().push(spec);
    }
    for (bind_addr, members) in groups {
        let first_member = members[0];
        let expected_max = first_member.max_connections;
        let first_member_name = first_member.name.as_str();
        let mut node_ids = HashMap::<&str, &str>::new();
        let mut public_keys = HashMap::<&[u8], &str>::new();
        for member in members {
            if member.max_connections != expected_max {
                bail!(
                    "LTP listener max_connections mismatch on bind '{bind_addr}' between inputs '{}' and '{}'",
                    first_member_name,
                    member.name
                );
            }
            for (spki, node_id) in member.peers.node_by_spki.iter() {
                if let Some(previous) = node_ids.insert(node_id, &member.name) {
                    bail!(
                        "duplicate LTP peer node_id in listener group '{bind_addr}': inputs '{previous}' and '{}'",
                        member.name
                    );
                }
                if let Some(previous) = public_keys.insert(spki, &member.name) {
                    bail!(
                        "duplicate LTP peer public key in listener group '{bind_addr}': inputs '{previous}' and '{}'",
                        member.name
                    );
                }
            }
        }
    }

    for (index, left) in specs.iter().enumerate() {
        for right in &specs[index + 1..] {
            if left.bind_addr == right.bind_addr {
                continue;
            }
            let (Ok(left_addr), Ok(right_addr)) = (
                left.bind_addr.parse::<SocketAddr>(),
                right.bind_addr.parse::<SocketAddr>(),
            ) else {
                continue;
            };
            if listener_binds_overlap(left_addr, right_addr) {
                bail!(
                    "overlapping LTP listener binds '{}' (input '{}') and '{}' (input '{}')",
                    left.bind_addr,
                    left.name,
                    right.bind_addr,
                    right.name
                );
            }
        }
    }
    Ok(())
}

fn listener_binds_overlap(left: SocketAddr, right: SocketAddr) -> bool {
    if left.port() != right.port() {
        return false;
    }
    match (left, right) {
        (SocketAddr::V4(left), SocketAddr::V4(right)) => {
            left.ip() == right.ip() || left.ip().is_unspecified() || right.ip().is_unspecified()
        }
        (SocketAddr::V6(left), SocketAddr::V6(right)) => {
            left.ip() == right.ip() || left.ip().is_unspecified() || right.ip().is_unspecified()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::TimeZone as _;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::Mutex;
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
        send_client_event_as(address, identity, server_spki, "peer-a", key, payload).await;
    }

    async fn send_client_event_as(
        address: std::net::SocketAddr,
        identity: &ValidatedNodeKey,
        server_spki: &[u8],
        node_id: &str,
        key: [u8; 16],
        payload: &'static [u8],
    ) {
        let mut client = send_hello_as(address, identity, server_spki, node_id).await;
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

    fn fixed_now() -> crate::time::ClockSample {
        crate::time::ClockSample::from_datetime(Utc.timestamp_nanos(123))
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

    fn test_ltp_metrics(peer: &str) -> Arc<LtpMetrics> {
        test_ltp_metrics_for(&[peer])
    }

    fn test_ltp_metrics_for(peers: &[&str]) -> Arc<LtpMetrics> {
        LtpMetrics::register(
            &crate::metrics::Registry::new(),
            &peers.iter().map(|peer| (*peer).to_owned()).collect(),
        )
        .unwrap()
    }

    fn test_peer_metrics(peer: &str) -> LtpPeerMetrics {
        test_ltp_metrics(peer).peer(peer).unwrap()
    }

    struct SharedTestContext {
        bind_addr: String,
        node_key: Arc<ValidatedNodeKey>,
        max_connections: usize,
        ltp_metrics: Arc<LtpMetrics>,
    }

    fn shared_member(
        context: &SharedTestContext,
        name: &str,
        peer_id: &str,
        peer_spki: Vec<u8>,
        hello_started: Option<Arc<tokio::sync::Notify>>,
    ) -> (
        LtpInput,
        tokio::sync::mpsc::Sender<Event>,
        tokio::sync::mpsc::Receiver<Event>,
    ) {
        let peers = peer_registry(peer_id, peer_spki);
        let metrics = test_metrics();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let input = LtpInput {
            name: name.to_owned(),
            bind_addr: context.bind_addr.clone(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::clone(&context.node_key),
            peers,
            max_hops: 16,
            max_connections: context.max_connections,
            metrics,
            ltp_metrics: Arc::clone(&context.ltp_metrics),
            now: fixed_now,
            hello_started,
        };
        (input, tx, rx)
    }

    async fn send_hello_as(
        address: std::net::SocketAddr,
        identity: &ValidatedNodeKey,
        server_spki: &[u8],
        node_id: &str,
    ) -> tokio_rustls::client::TlsStream<TcpStream> {
        let mut client = connect_client(address, identity, server_spki).await;
        client
            .write_all(
                &crate::ltp::encode_hello_frame(&LtpHello {
                    node_id: node_id.to_owned(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();
        client
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
        let peer_metrics = test_peer_metrics("peer");
        for key_len in [0, 15, 17] {
            let meta = LtpMeta {
                key: vec![7; key_len],
                stamps: Vec::new(),
            };
            assert!(
                event_from_frame(
                    meta,
                    Bytes::new(),
                    source,
                    "self",
                    16,
                    fixed_now,
                    &peer_metrics,
                )
                .is_err()
            );
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
                    &peer_metrics,
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
            event_from_frame(
                cycle,
                Bytes::new(),
                source,
                "self",
                1,
                fixed_now,
                &peer_metrics,
            )
            .unwrap(),
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
            event_from_frame(
                full,
                Bytes::new(),
                source,
                "self",
                1,
                fixed_now,
                &peer_metrics,
            )
            .unwrap(),
            FrameDecision::DropMaxHops
        ));
        assert_eq!(
            peer_metrics
                .loop_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            2
        );
        assert_eq!(peer_metrics.network_latency.count(), 0);
        assert_eq!(
            peer_metrics
                .negative_delta
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
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
        let peer_metrics = test_peer_metrics("peer");
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
            &peer_metrics,
        )
        .unwrap();
        let FrameDecision::Forward(event) = decision else {
            panic!("valid frame was dropped");
        };
        assert_eq!(event.key().as_bytes(), &key);
        assert_eq!(event.received_at, fixed_now().utc);
        assert_eq!(event.ingress, Bytes::from_static(b"payload"));
        assert_eq!(event.egress, Bytes::from_static(b"payload"));
        assert_eq!(event.ltp_stamps()[0], peer_stamp);
        assert_eq!(peer_metrics.network_latency.count(), 1);
        assert_eq!(peer_metrics.network_latency.sum(), 112.0 / 1_000_000_000.0);
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
    fn network_latency_skips_unsealed_history_and_clamps_negative_deltas() {
        let source = "127.0.0.1:7514".parse().unwrap();
        let peer_metrics = test_peer_metrics("peer");
        for departure_unix_nano in [0, 124] {
            assert!(matches!(
                event_from_frame(
                    LtpMeta {
                        key: v7_key(departure_unix_nano as u8).to_vec(),
                        stamps: vec![HopStamp {
                            node_id: "upstream".to_owned(),
                            arrival_unix_nano: 1,
                            departure_unix_nano,
                        }],
                    },
                    Bytes::new(),
                    source,
                    "self",
                    16,
                    fixed_now,
                    &peer_metrics,
                )
                .unwrap(),
                FrameDecision::Forward(_)
            ));
        }
        assert_eq!(peer_metrics.network_latency.count(), 1);
        assert_eq!(peer_metrics.network_latency.sum(), 0.0);
        assert_eq!(
            peer_metrics
                .negative_delta
                .load(std::sync::atomic::Ordering::Relaxed),
            1
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
            &test_peer_metrics("peer"),
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
                &test_peer_metrics("peer"),
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
        let ltp_metrics = test_ltp_metrics("peer-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        let metrics = test_metrics();
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: address.to_string(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::new(server_identity),
            peers,
            max_hops: 16,
            max_connections: 1,
            metrics: Arc::clone(&metrics),
            ltp_metrics,
            now: fixed_now,
            hello_started: None,
        };
        let group = SharedListenerGroup::from_members(vec![(input, tx)]).unwrap();
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                group.acceptor,
                group.routes,
                group.ltp_metrics,
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
        let ltp_metrics = test_ltp_metrics("peer-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let metrics = test_metrics();
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: address.to_string(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::new(server_identity),
            peers,
            max_hops: 16,
            max_connections: 1,
            metrics: Arc::clone(&metrics),
            ltp_metrics,
            now: fixed_now,
            hello_started: None,
        };
        let group = SharedListenerGroup::from_members(vec![(input, tx)]).unwrap();
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                group.acceptor,
                group.routes,
                group.ltp_metrics,
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
        let ltp_metrics = test_ltp_metrics("peer-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        let metrics = test_metrics();
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: address.to_string(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::new(server_identity),
            peers,
            max_hops: 16,
            max_connections: 1,
            metrics: Arc::clone(&metrics),
            ltp_metrics,
            now: fixed_now,
            hello_started: None,
        };
        let group = SharedListenerGroup::from_members(vec![(input, tx)]).unwrap();
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                group.acceptor,
                group.routes,
                group.ltp_metrics,
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
        let ltp_metrics = test_ltp_metrics("peer-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let metrics = test_metrics();
        let hello_started = Arc::new(tokio::sync::Notify::new());
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: address.to_string(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::new(server_identity),
            peers,
            max_hops: 16,
            max_connections: 1,
            metrics,
            ltp_metrics,
            now: fixed_now,
            hello_started: Some(Arc::clone(&hello_started)),
        };
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let group = SharedListenerGroup::from_members(vec![(input, tx)]).unwrap();
        let server = tokio::spawn(group.run_on_listener(listener, shutdown_rx));

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
        let ltp_metrics = test_ltp_metrics("peer-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let metrics = test_metrics();
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: address.to_string(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::new(server_identity),
            peers,
            max_hops: 16,
            max_connections: 1,
            metrics,
            ltp_metrics: Arc::clone(&ltp_metrics),
            now: fixed_now,
            hello_started: None,
        };
        let group = SharedListenerGroup::from_members(vec![(input, tx)]).unwrap();
        let server = tokio::spawn(async move {
            let (stream, peer_address) = listener.accept().await.unwrap();
            handle_connection(
                stream,
                peer_address,
                group.acceptor,
                group.routes,
                group.ltp_metrics,
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
        assert_eq!(
            ltp_metrics
                .rejected_unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn tls_handshake_rejects_an_undeclared_client_public_key() {
        let (server_identity, server_spki) = generated_identity();
        let (_, declared_spki) = generated_identity();
        let (unknown_identity, _) = generated_identity();
        let peers = peer_registry("peer-a", declared_spki);
        let ltp_metrics = test_ltp_metrics("peer-a");
        let acceptor =
            build_rpk_acceptor(&server_identity, &peers, Arc::clone(&ltp_metrics)).unwrap();
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
        assert_eq!(
            ltp_metrics
                .rejected_unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
    }

    #[tokio::test]
    async fn shared_listener_rejects_unknown_spki_before_logical_dispatch() {
        let (server_identity, server_spki) = generated_identity();
        let server_identity = Arc::new(server_identity);
        let (_, declared_spki_a) = generated_identity();
        let (_, declared_spki_b) = generated_identity();
        let (unknown_identity, _) = generated_identity();
        let ltp_metrics = test_ltp_metrics_for(&["peer-a", "peer-b"]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let context = SharedTestContext {
            bind_addr: address.to_string(),
            node_key: Arc::clone(&server_identity),
            max_connections: 2,
            ltp_metrics: Arc::clone(&ltp_metrics),
        };
        let (input_a, tx_a, mut rx_a) =
            shared_member(&context, "from_a", "peer-a", declared_spki_a, None);
        let (input_b, tx_b, mut rx_b) =
            shared_member(&context, "from_b", "peer-b", declared_spki_b, None);
        let group =
            SharedListenerGroup::from_members(vec![(input_a, tx_a), (input_b, tx_b)]).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(group.run_on_listener(listener, shutdown_rx));

        let connector =
            crate::modules::output::ltp::build_rpk_connector(&unknown_identity, &server_spki)
                .unwrap();
        let tcp = TcpStream::connect(address).await.unwrap();
        let _ = connector
            .connect("localhost".try_into().unwrap(), tcp)
            .await;
        for _ in 0..100 {
            if ltp_metrics
                .rejected_unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed)
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            ltp_metrics
                .rejected_unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert!(rx_a.try_recv().is_err());
        assert!(rx_b.try_recv().is_err());
        shutdown_tx.send(true).unwrap();
        assert!(server.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn shared_listener_uses_spki_owner_and_isolates_protocol_errors() {
        let (server_identity, server_spki) = generated_identity();
        let server_identity = Arc::new(server_identity);
        let (peer_a, peer_a_spki) = generated_identity();
        let (peer_b, peer_b_spki) = generated_identity();
        let ltp_metrics = test_ltp_metrics_for(&["peer-a", "peer-b"]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let context = SharedTestContext {
            bind_addr: address.to_string(),
            node_key: Arc::clone(&server_identity),
            max_connections: 4,
            ltp_metrics: Arc::clone(&ltp_metrics),
        };
        let (input_a, tx_a, mut rx_a) =
            shared_member(&context, "from_a", "peer-a", peer_a_spki, None);
        let (input_b, tx_b, mut rx_b) =
            shared_member(&context, "from_b", "peer-b", peer_b_spki, None);
        let group =
            SharedListenerGroup::from_members(vec![(input_a, tx_a), (input_b, tx_b)]).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(group.run_on_listener(listener, shutdown_rx));

        let mismatched = send_hello_as(address, &peer_a, &server_spki, "peer-b").await;
        drop(mismatched);
        for _ in 0..100 {
            if ltp_metrics
                .rejected_unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed)
                == 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            ltp_metrics
                .rejected_unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed),
            1,
            "peer-a's key must not select peer-b's logical input"
        );

        let mut malformed = send_hello_as(address, &peer_a, &server_spki, "peer-a").await;
        malformed.write_all(b"BAD!").await.unwrap();
        malformed.flush().await.unwrap();
        drop(malformed);
        send_client_event_as(
            address,
            &peer_b,
            &server_spki,
            "peer-b",
            v7_key(8),
            b"peer-b-survives",
        )
        .await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx_b.recv())
                .await
                .unwrap()
                .unwrap()
                .ingress,
            Bytes::from_static(b"peer-b-survives")
        );
        assert!(rx_a.try_recv().is_err());
        shutdown_tx.send(true).unwrap();
        assert!(server.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn shared_listener_capacity_is_group_wide_and_timeout_releases_slot() {
        let (server_identity, server_spki) = generated_identity();
        let server_identity = Arc::new(server_identity);
        let (peer_a, peer_a_spki) = generated_identity();
        let (peer_b, peer_b_spki) = generated_identity();
        let ltp_metrics = test_ltp_metrics_for(&["peer-a", "peer-b"]);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hello_started = Arc::new(tokio::sync::Notify::new());
        let context = SharedTestContext {
            bind_addr: address.to_string(),
            node_key: Arc::clone(&server_identity),
            max_connections: 1,
            ltp_metrics: Arc::clone(&ltp_metrics),
        };
        let (input_a, tx_a, _rx_a) = shared_member(
            &context,
            "from_a",
            "peer-a",
            peer_a_spki,
            Some(Arc::clone(&hello_started)),
        );
        let (input_b, tx_b, mut rx_b) =
            shared_member(&context, "from_b", "peer-b", peer_b_spki, None);
        let group =
            SharedListenerGroup::from_members(vec![(input_a, tx_a), (input_b, tx_b)]).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(group.run_on_listener(listener, shutdown_rx));

        let mut stalled = connect_client(address, &peer_a, &server_spki).await;
        hello_started.notified().await;
        let connector =
            crate::modules::output::ltp::build_rpk_connector(&peer_b, &server_spki).unwrap();
        let tcp = TcpStream::connect(address).await.unwrap();
        assert!(
            connector
                .connect("localhost".try_into().unwrap(), tcp)
                .await
                .is_err(),
            "the second member must share the first member's connection cap"
        );

        let mut closed_probe = [0u8; 1];
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            stalled.read(&mut closed_probe),
        )
        .await
        .expect("HELLO timeout must close the stalled group connection");
        send_client_event_as(
            address,
            &peer_b,
            &server_spki,
            "peer-b",
            v7_key(10),
            b"released-group-slot",
        )
        .await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), rx_b.recv())
                .await
                .unwrap()
                .unwrap()
                .ingress,
            Bytes::from_static(b"released-group-slot")
        );
        shutdown_tx.send(true).unwrap();
        assert!(server.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn shared_listener_bind_failure_names_members_and_shutdown_closes_connections() {
        let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = occupied.local_addr().unwrap();
        let (server_identity, server_spki) = generated_identity();
        let server_identity = Arc::new(server_identity);
        let (peer_a, peer_a_spki) = generated_identity();
        let (_, peer_b_spki) = generated_identity();
        let ltp_metrics = test_ltp_metrics_for(&["peer-a", "peer-b"]);
        let context = SharedTestContext {
            bind_addr: address.to_string(),
            node_key: Arc::clone(&server_identity),
            max_connections: 2,
            ltp_metrics: Arc::clone(&ltp_metrics),
        };
        let (input_a, tx_a, _rx_a) =
            shared_member(&context, "from_a", "peer-a", peer_a_spki.clone(), None);
        let (input_b, tx_b, _rx_b) =
            shared_member(&context, "from_b", "peer-b", peer_b_spki.clone(), None);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let error = start_listener_groups(vec![(input_a, tx_a), (input_b, tx_b)], shutdown_rx)
            .await
            .expect_err("occupied group bind must fail before spawning");
        let error = format!("{error:#}");
        assert!(error.contains("from_a"));
        assert!(error.contains("from_b"));
        drop(occupied);

        let listener = TcpListener::bind(address).await.unwrap();
        let (input_a, tx_a, _rx_a) = shared_member(&context, "from_a", "peer-a", peer_a_spki, None);
        let (input_b, tx_b, _rx_b) = shared_member(&context, "from_b", "peer-b", peer_b_spki, None);
        let group =
            SharedListenerGroup::from_members(vec![(input_a, tx_a), (input_b, tx_b)]).unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(group.run_on_listener(listener, shutdown_rx));
        let _stalled = connect_client(address, &peer_a, &server_spki).await;
        shutdown_tx.send(true).unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), server)
                .await
                .unwrap()
                .unwrap()
                .is_ok()
        );
        assert!(TcpStream::connect(address).await.is_err());
    }

    #[test]
    fn non_rpk_and_intermediate_certificate_failures_do_not_count_unknown_peers() {
        let (_, declared_spki) = generated_identity();
        let ltp_metrics = test_ltp_metrics("peer-a");
        let verifier = DeclaredClientVerifier {
            allowed_spki: Arc::new(HashSet::from([declared_spki.clone()])),
            algorithms: rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms,
            ltp_metrics: Arc::clone(&ltp_metrics),
        };
        let now = UnixTime::since_unix_epoch(std::time::Duration::ZERO);

        assert!(
            verifier
                .verify_client_cert(&CertificateDer::from(vec![1, 2, 3]), &[], now)
                .is_err()
        );
        assert!(
            verifier
                .verify_client_cert(
                    &CertificateDer::from(declared_spki),
                    &[CertificateDer::from(vec![4, 5, 6])],
                    now,
                )
                .is_err()
        );
        assert_eq!(
            ltp_metrics
                .rejected_unknown_peer
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }

    #[tokio::test]
    async fn parallel_connections_obey_channel_backpressure() {
        let (server_identity, server_spki) = generated_identity();
        let (client_identity, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let ltp_metrics = test_ltp_metrics("peer-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let metrics = test_metrics();
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: address.to_string(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::new(server_identity),
            peers,
            max_hops: 16,
            max_connections: 2,
            metrics,
            ltp_metrics,
            now: fixed_now,
            hello_started: None,
        };
        let group = SharedListenerGroup::from_members(vec![(input, tx)]).unwrap();
        let server = tokio::spawn(async move {
            let mut connections = Vec::new();
            for _ in 0..2 {
                let (stream, peer_address) = listener.accept().await.unwrap();
                connections.push(tokio::spawn(handle_connection(
                    stream,
                    peer_address,
                    group.acceptor.clone(),
                    Arc::clone(&group.routes),
                    Arc::clone(&group.ltp_metrics),
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

    #[derive(Clone)]
    struct ScriptedAccept {
        state: Arc<Mutex<ScriptedAcceptState>>,
    }

    struct ScriptedAcceptState {
        outcomes: VecDeque<std::result::Result<usize, io::ErrorKind>>,
        fallback_error: Option<io::ErrorKind>,
        attempts: Vec<tokio::time::Instant>,
    }

    impl ScriptedAccept {
        fn finite(
            outcomes: impl IntoIterator<Item = std::result::Result<usize, io::ErrorKind>>,
        ) -> Self {
            Self::new(outcomes, None)
        }

        fn permanent(error: io::ErrorKind) -> Self {
            Self::new([], Some(error))
        }

        fn new(
            outcomes: impl IntoIterator<Item = std::result::Result<usize, io::ErrorKind>>,
            fallback_error: Option<io::ErrorKind>,
        ) -> Self {
            Self {
                state: Arc::new(Mutex::new(ScriptedAcceptState {
                    outcomes: outcomes.into_iter().collect(),
                    fallback_error,
                    attempts: Vec::new(),
                })),
            }
        }

        fn attempts(&self) -> Vec<tokio::time::Instant> {
            self.state.lock().unwrap().attempts.clone()
        }
    }

    #[async_trait::async_trait]
    impl ListenerAccept for ScriptedAccept {
        type Connection = usize;

        async fn accept(&self) -> io::Result<Self::Connection> {
            let outcome = {
                let mut state = self.state.lock().unwrap();
                let fallback_error = state.fallback_error;
                state
                    .outcomes
                    .pop_front()
                    .or_else(|| fallback_error.map(Err))
            };
            let Some(outcome) = outcome else {
                return std::future::pending().await;
            };
            self.state
                .lock()
                .unwrap()
                .attempts
                .push(tokio::time::Instant::now());
            outcome.map_err(|kind| io::Error::new(kind, "scripted accept failure"))
        }
    }

    async fn wait_for_attempts(acceptor: &ScriptedAccept, expected: usize) {
        for _ in 0..20 {
            if acceptor.attempts().len() >= expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(acceptor.attempts().len(), expected);
    }

    #[tokio::test(start_paused = true)]
    async fn accept_retry_errors_back_off_100_then_200_ms_and_process_connection() {
        let started = tokio::time::Instant::now();
        let acceptor = ScriptedAccept::finite([
            Err(io::ErrorKind::ConnectionAborted),
            Err(io::ErrorKind::ConnectionReset),
            Ok(7),
        ]);
        let observed = acceptor.clone();
        let processed = Arc::new(Mutex::new(Vec::new()));
        let processed_by_loop = Arc::clone(&processed);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            run_accept_loop(acceptor, shutdown_rx, "test", move |connection| {
                processed_by_loop.lock().unwrap().push(connection);
            })
            .await
        });

        wait_for_attempts(&observed, 1).await;
        tokio::time::advance(std::time::Duration::from_millis(99)).await;
        tokio::task::yield_now().await;
        assert_eq!(observed.attempts().len(), 1);
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        wait_for_attempts(&observed, 2).await;
        tokio::time::advance(std::time::Duration::from_millis(199)).await;
        tokio::task::yield_now().await;
        assert_eq!(observed.attempts().len(), 2);
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        wait_for_attempts(&observed, 3).await;
        assert_eq!(*processed.lock().unwrap(), [7]);
        assert_eq!(
            observed
                .attempts()
                .iter()
                .map(|attempt| attempt.duration_since(started))
                .collect::<Vec<_>>(),
            [
                std::time::Duration::ZERO,
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(300),
            ]
        );

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn accept_retry_permanent_errors_use_exact_bounded_backoff_without_busy_spin() {
        let acceptor = ScriptedAccept::permanent(io::ErrorKind::ConnectionAborted);
        let observed = acceptor.clone();
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_accept_loop(acceptor, shutdown_rx, "test", |_| {}));
        let waits = [
            std::time::Duration::from_millis(100),
            std::time::Duration::from_millis(200),
            std::time::Duration::from_millis(400),
            std::time::Duration::from_millis(800),
            std::time::Duration::from_millis(1_600),
            std::time::Duration::from_millis(3_200),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
        ];

        wait_for_attempts(&observed, 1).await;
        for (index, wait) in waits.iter().enumerate() {
            let attempts_before_wait = observed.attempts().len();
            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                observed.attempts().len(),
                attempts_before_wait,
                "accept loop must not spin while backoff time is paused"
            );
            tokio::time::advance(*wait).await;
            wait_for_attempts(&observed, index + 2).await;
        }

        let observed_waits = observed
            .attempts()
            .windows(2)
            .map(|attempts| attempts[1].duration_since(attempts[0]))
            .collect::<Vec<_>>();
        assert_eq!(observed_waits, waits);
        task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn accept_retry_shutdown_interrupts_backoff_immediately() {
        let acceptor = ScriptedAccept::permanent(io::ErrorKind::ConnectionAborted);
        let observed = acceptor.clone();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_accept_loop(acceptor, shutdown_rx, "test", |_| {}));

        wait_for_attempts(&observed, 1).await;
        assert!(!task.is_finished());
        let before_shutdown = tokio::time::Instant::now();
        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(tokio::time::Instant::now(), before_shutdown);
        assert_eq!(observed.attempts().len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn accept_retry_success_resets_delay_to_100_ms() {
        let started = tokio::time::Instant::now();
        let acceptor = ScriptedAccept::finite([
            Err(io::ErrorKind::ConnectionAborted),
            Ok(1),
            Err(io::ErrorKind::ConnectionReset),
            Ok(2),
        ]);
        let observed = acceptor.clone();
        let processed = Arc::new(Mutex::new(Vec::new()));
        let processed_by_loop = Arc::clone(&processed);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            run_accept_loop(acceptor, shutdown_rx, "test", move |connection| {
                processed_by_loop.lock().unwrap().push(connection);
            })
            .await
        });

        wait_for_attempts(&observed, 1).await;
        tokio::time::advance(std::time::Duration::from_millis(100)).await;
        wait_for_attempts(&observed, 3).await;
        assert_eq!(*processed.lock().unwrap(), [1]);
        tokio::time::advance(std::time::Duration::from_millis(99)).await;
        tokio::task::yield_now().await;
        assert_eq!(observed.attempts().len(), 3);
        tokio::time::advance(std::time::Duration::from_millis(1)).await;
        wait_for_attempts(&observed, 4).await;
        assert_eq!(*processed.lock().unwrap(), [1, 2]);
        assert_eq!(
            observed
                .attempts()
                .iter()
                .map(|attempt| attempt.duration_since(started))
                .collect::<Vec<_>>(),
            [
                std::time::Duration::ZERO,
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(100),
                std::time::Duration::from_millis(200),
            ]
        );

        shutdown_tx.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn listener_shutdown_aborts_the_accept_loop_without_waiting_for_connections() {
        let (server_identity, _) = generated_identity();
        let (_, client_spki) = generated_identity();
        let peers = peer_registry("peer-a", client_spki);
        let ltp_metrics = test_ltp_metrics("peer-a");
        let context = crate::modules::BuildContext::for_testing();
        let input = LtpInput {
            name: "in".to_owned(),
            bind_addr: "127.0.0.1:0".to_owned(),
            node_id: Arc::<str>::from("self"),
            node_key: Arc::new(server_identity),
            peers,
            max_hops: 16,
            max_connections: 2,
            metrics: InputMetrics::register(&context.metrics, "in").unwrap(),
            ltp_metrics,
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

    #[test]
    fn listener_task_error_does_not_broadcast_daemon_shutdown() {
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

        report_listener_result("in", Err(anyhow::anyhow!("deterministic listener failure")));

        assert!(!*shutdown_tx.borrow());
        assert!(!*shutdown_rx.borrow_and_update());
        assert!(!shutdown_rx.has_changed().unwrap());
    }
}
