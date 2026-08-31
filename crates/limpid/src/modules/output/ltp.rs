//! Unbatched LTP output over mutually authenticated TLS 1.3 raw public keys.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use bytes::Bytes;
#[cfg(test)]
use chrono::Utc;
use prost::Message as _;
use rustls::client::AlwaysResolvesClientRawPublicKeys;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme,
};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::dsl::ast::{ExprKind, Property};
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::ltp::{
    ED25519_SPKI_PREFIX, HopStamp, LtpHello, LtpMeta, MAX_META_LEN, MAX_PAYLOAD_LEN,
    ValidatedNodeKey, encode_frame, encode_hello_frame,
};
use crate::metrics::{LtpPeerMetrics, OutputMetrics};
use crate::modules::output::syslog_peers::{
    PEER_CONNECT_TIMEOUT, PEER_HANDSHAKE_TIMEOUT, PEER_WRITE_TIMEOUT,
};
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};

const DEFAULT_LTP_PORT: u16 = 7514;

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
    PropertySpec {
        name: "endpoint",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
];

const LTP_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "peer",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(LTP_PEER_SCHEMA),
    },
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

#[derive(Clone, Debug)]
struct PeerConfig {
    node_id: String,
    address: String,
    server_name: ServerName<'static>,
    public_key_spki: Vec<u8>,
}

pub struct LtpOutput {
    name: String,
    node_id: Arc<str>,
    peer: PeerConfig,
    connector: TlsConnector,
    connection: Mutex<Option<TlsStream<TcpStream>>>,
    hello_frame: Bytes,
    retry: RetryConfig,
    error_log: Option<Arc<crate::error_log::ErrorLogWriter>>,
    error_log_fallback: crate::error_log::ErrorLogFallback,
    metrics: Arc<OutputMetrics>,
    peer_metrics: LtpPeerMetrics,
    shutdown_signal: tokio::sync::watch::Receiver<bool>,
    now: fn() -> crate::time::UnixNanos,
}

impl Module for LtpOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(LTP_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let peer = parse_peer(name, properties.user_properties())?;
        let peer_metrics = ctx
            .ltp_metrics
            .as_ref()
            .and_then(|metrics| metrics.peer(&peer.node_id))
            .ok_or_else(|| anyhow::anyhow!("output '{name}': LTP peer metrics are unavailable"))?;
        let node_id =
            ctx.ltp_node_id.as_ref().cloned().ok_or_else(|| {
                anyhow::anyhow!("output '{name}': LTP node identity is unavailable")
            })?;
        let event_meta_len = event_meta(&node_id, &[0; 16], 1, 1).encoded_len();
        if event_meta_len > MAX_META_LEN {
            bail!(
                "output '{name}': node_id makes LTP event metadata {event_meta_len} bytes (maximum {MAX_META_LEN})"
            );
        }
        let node_key = ctx
            .ltp_node_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("output '{name}': LTP node key is unavailable"))?;
        let connector = build_rpk_connector(node_key, &peer.public_key_spki)?;
        let hello_frame = Bytes::from(encode_hello_frame(&LtpHello {
            node_id: node_id.to_string(),
        })?);

        Ok(Self {
            name: name.to_owned(),
            node_id,
            peer,
            connector,
            connection: Mutex::new(None),
            hello_frame,
            retry: RetryConfig::from_output_properties(properties.user_properties())?,
            error_log: ctx.error_log.as_ref().map(Arc::clone),
            error_log_fallback: ctx.error_log_fallback,
            metrics: OutputMetrics::register(&ctx.metrics, name)?,
            peer_metrics,
            shutdown_signal: ctx.shutdown_signal.clone(),
            now: crate::time::UnixNanos::now,
        })
    }
}

impl HasMetrics for LtpOutput {
    type Stats = OutputMetrics;

    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[derive(Debug)]
struct PinnedRpkVerifier {
    expected_spki: Vec<u8>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedRpkVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        if !intermediates.is_empty() || end_entity.as_ref() != self.expected_spki {
            return Err(TlsError::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ));
        }
        Ok(ServerCertVerified::assertion())
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

pub(crate) fn build_rpk_connector(
    node_key: &ValidatedNodeKey,
    expected_peer_spki: &[u8],
) -> Result<TlsConnector> {
    crate::tls::install_default_crypto_provider();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let verifier = PinnedRpkVerifier {
        expected_spki: expected_peer_spki.to_vec(),
        algorithms: provider.signature_verification_algorithms,
    };
    let config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("configure TLS 1.3 for LTP")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_client_cert_resolver(Arc::new(AlwaysResolvesClientRawPublicKeys::new(
            node_key.certified_key(),
        )));
    Ok(TlsConnector::from(Arc::new(config)))
}

pub(crate) fn validate_static_properties(name: &str, properties: &[Property]) -> Result<()> {
    parse_peer(name, properties).map(|_| ())
}

fn parse_peer(name: &str, properties: &[Property]) -> Result<PeerConfig> {
    let peer_blocks: Vec<&[Property]> = properties
        .iter()
        .filter_map(|property| match property {
            Property::Block {
                key, properties, ..
            } if key == "peer" => Some(properties.as_slice()),
            _ => None,
        })
        .collect();
    let [peer] = peer_blocks.as_slice() else {
        bail!("output '{name}': exactly one peer block is required");
    };

    let node_id = required_literal_string(name, peer, "node_id")?;
    if node_id.is_empty() {
        bail!("output '{name}': peer node_id must be non-empty");
    }
    let pubkey = required_literal_string(name, peer, "pubkey")?;
    let public_key_spki = decode_ed25519_spki(name, &pubkey)?;
    let endpoint = required_literal_string(name, peer, "endpoint")?;
    let (host, address) = parse_endpoint(name, &endpoint)?;
    let server_name = ServerName::try_from(host.to_owned())
        .with_context(|| format!("output '{name}': invalid peer endpoint host '{host}'"))?;

    Ok(PeerConfig {
        node_id,
        address,
        server_name,
        public_key_spki,
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
        [] => bail!("output '{name}': peer {key} requires a string value"),
        _ => bail!("output '{name}': duplicate peer {key}"),
    }
}

fn decode_ed25519_spki(name: &str, encoded: &str) -> Result<Vec<u8>> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .with_context(|| format!("output '{name}': peer pubkey is not valid base64"))?;
    if decoded.len() != ED25519_SPKI_PREFIX.len() + 32 || !decoded.starts_with(&ED25519_SPKI_PREFIX)
    {
        bail!("output '{name}': peer pubkey must be an Ed25519 SPKI DER value");
    }
    Ok(decoded)
}

fn parse_endpoint(name: &str, endpoint: &str) -> Result<(String, String)> {
    if endpoint.is_empty() {
        bail!("output '{name}': peer endpoint must be non-empty");
    }

    if let Some(rest) = endpoint.strip_prefix('[') {
        let close = rest
            .find(']')
            .ok_or_else(|| anyhow::anyhow!("output '{name}': invalid bracketed peer endpoint"))?;
        let host = &rest[..close];
        host.parse::<std::net::Ipv6Addr>()
            .with_context(|| format!("output '{name}': invalid IPv6 peer endpoint"))?;
        let suffix = &rest[close + 1..];
        let port = if suffix.is_empty() {
            DEFAULT_LTP_PORT
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(|| anyhow::anyhow!("output '{name}': invalid peer endpoint"))?
                .parse::<u16>()
                .with_context(|| format!("output '{name}': invalid peer endpoint port"))?
        };
        return Ok((host.to_owned(), format!("[{host}]:{port}")));
    }

    let colon_count = endpoint.bytes().filter(|byte| *byte == b':').count();
    if colon_count > 1 {
        endpoint
            .parse::<std::net::Ipv6Addr>()
            .with_context(|| format!("output '{name}': invalid peer endpoint"))?;
        return Ok((
            endpoint.to_owned(),
            format!("[{endpoint}]:{DEFAULT_LTP_PORT}"),
        ));
    }
    if let Some((host, port)) = endpoint.rsplit_once(':') {
        if host.is_empty() {
            bail!("output '{name}': peer endpoint host must be non-empty");
        }
        let port = port
            .parse::<u16>()
            .with_context(|| format!("output '{name}': invalid peer endpoint port"))?;
        return Ok((host.to_owned(), format!("{host}:{port}")));
    }
    Ok((
        endpoint.to_owned(),
        format!("{endpoint}:{DEFAULT_LTP_PORT}"),
    ))
}

enum WriteOutcome {
    Delivered,
    PreSendShutdown,
    Err(anyhow::Error),
}

fn event_meta(
    node_id: &str,
    key: &[u8; 16],
    arrival_unix_nano: u64,
    departure_unix_nano: u64,
) -> LtpMeta {
    LtpMeta {
        key: key.to_vec(),
        stamps: vec![HopStamp {
            node_id: node_id.to_owned(),
            arrival_unix_nano,
            departure_unix_nano,
        }],
    }
}

struct WorkingEventMeta {
    meta: LtpMeta,
    local_stamp: usize,
    #[cfg(test)]
    requested_capacity: usize,
}

#[cfg(test)]
std::thread_local! {
    static STAMP_CLONE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl LtpOutput {
    fn working_event_meta(&self, event: &Event) -> WorkingEventMeta {
        let arrival_unix_nano =
            crate::time::UnixNanos::from_datetime(event.received_at).to_wire_u64();
        #[cfg(test)]
        STAMP_CLONE_COUNT.set(STAMP_CLONE_COUNT.get() + 1);
        let prefix = event.ltp_stamps();
        let reuses_pending_local = matches!(
            prefix.last(),
            Some(stamp)
                if stamp.node_id == self.node_id.as_ref() && stamp.departure_unix_nano == 0
        );
        let append_count = usize::from(!reuses_pending_local);
        let requested_capacity = prefix.len() + append_count;
        let mut stamps = Vec::with_capacity(requested_capacity);
        stamps.extend_from_slice(prefix);
        if !reuses_pending_local {
            stamps.push(HopStamp {
                node_id: self.node_id.to_string(),
                arrival_unix_nano,
                departure_unix_nano: 0,
            });
        }
        let local_stamp = stamps.len() - 1;
        WorkingEventMeta {
            meta: LtpMeta {
                key: event.key().as_bytes().to_vec(),
                stamps,
            },
            local_stamp,
            #[cfg(test)]
            requested_capacity,
        }
    }

    fn event_frame(&self, working: &mut WorkingEventMeta, payload: &[u8]) -> Result<Bytes> {
        let departure_unix_nano = (self.now)().to_wire_u64();
        working.meta.stamps[working.local_stamp].departure_unix_nano = departure_unix_nano;
        Ok(Bytes::from(encode_frame(&working.meta, payload)?))
    }

    async fn connect(&self) -> Result<TlsStream<TcpStream>> {
        let tcp =
            tokio::time::timeout(PEER_CONNECT_TIMEOUT, TcpStream::connect(&self.peer.address))
                .await
                .with_context(|| format!("LTP connect to {} timed out", self.peer.address))?
                .with_context(|| format!("LTP connect to {}", self.peer.address))?;
        tokio::time::timeout(
            PEER_HANDSHAKE_TIMEOUT,
            self.connector.connect(self.peer.server_name.clone(), tcp),
        )
        .await
        .with_context(|| format!("LTP handshake with {} timed out", self.peer.node_id))?
        .with_context(|| format!("LTP handshake with {}", self.peer.node_id))
    }

    async fn write_bytes<W>(stream: &mut W, bytes: &[u8]) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        Self::write_bytes_with_timeout(stream, bytes, PEER_WRITE_TIMEOUT).await
    }

    async fn write_bytes_with_timeout<W>(
        stream: &mut W,
        bytes: &[u8],
        timeout: std::time::Duration,
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        tokio::time::timeout(timeout, async {
            stream.write_all(bytes).await?;
            stream.flush().await
        })
        .await
        .context("LTP write timed out")?
        .context("LTP write failed")?;
        Ok(())
    }

    async fn write_event<W>(
        &self,
        stream: &mut W,
        working: &mut WorkingEventMeta,
        payload: &[u8],
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        let frame = self.event_frame(working, payload)?;
        Self::write_bytes(stream, &frame).await?;
        self.metrics.bytes_written.inc_by(frame.len() as u64);
        let stamp = &working.meta.stamps[working.local_stamp];
        let delta = stamp
            .departure_unix_nano
            .saturating_sub(stamp.arrival_unix_nano);
        self.peer_metrics
            .intra_latency
            .observe(crate::time::DurationNanos::new(delta));
        Ok(())
    }

    async fn write_hello_and_event<W>(
        &self,
        stream: &mut W,
        working: &mut WorkingEventMeta,
        payload: &[u8],
    ) -> Result<()>
    where
        W: AsyncWrite + Unpin,
    {
        Self::write_bytes(stream, &self.hello_frame).await?;
        self.write_event(stream, working, payload).await
    }

    async fn write_attempt(
        &self,
        working: &mut WorkingEventMeta,
        payload: &[u8],
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
    ) -> WriteOutcome {
        let mut guard = self.connection.lock().await;
        if *shutdown.borrow() {
            return WriteOutcome::PreSendShutdown;
        }
        if guard.is_none() {
            let stream = match crate::modules::pre_send_or_shutdown(shutdown, self.connect()).await
            {
                Some(Ok(stream)) => stream,
                Some(Err(error)) => return WriteOutcome::Err(error),
                None => return WriteOutcome::PreSendShutdown,
            };
            if *shutdown.borrow() {
                return WriteOutcome::PreSendShutdown;
            }
            let mut stream = stream;
            return match self
                .write_hello_and_event(&mut stream, working, payload)
                .await
            {
                Ok(()) => {
                    *guard = Some(stream);
                    WriteOutcome::Delivered
                }
                Err(error) => WriteOutcome::Err(error),
            };
        }
        match self
            .write_event(guard.as_mut().unwrap(), working, payload)
            .await
        {
            Ok(()) => WriteOutcome::Delivered,
            Err(error) => {
                *guard = None;
                WriteOutcome::Err(error)
            }
        }
    }

    async fn write_shutdown_attempt(
        &self,
        working: &mut WorkingEventMeta,
        payload: &[u8],
    ) -> Result<()> {
        let mut guard = self.connection.lock().await;
        let Some(mut stream) = guard.take() else {
            let mut stream = self.connect().await?;
            self.write_hello_and_event(&mut stream, working, payload)
                .await?;
            *guard = Some(stream);
            return Ok(());
        };
        self.write_event(&mut stream, working, payload).await?;
        *guard = Some(stream);
        Ok(())
    }

    async fn write_shutdown_with_timeout(
        &self,
        working: &mut WorkingEventMeta,
        payload: &[u8],
        timeout: std::time::Duration,
    ) -> Result<()> {
        tokio::time::timeout(timeout, self.write_shutdown_attempt(working, payload))
            .await
            .map_err(|_| anyhow::anyhow!("LTP shutdown write timed out"))?
    }

    async fn route_failure(&self, event: &Event, ack: QueueAckHandle, reason: &str) {
        let outcome = crate::modules::route_event_to_dlq(
            self.error_log.as_ref(),
            self.error_log_fallback,
            &self.metrics,
            &self.name,
            event,
            ack.position(),
            reason,
        )
        .await;
        crate::modules::resolve_ack_from_dlq_outcome(ack, outcome, &self.metrics);
    }
}

#[async_trait::async_trait]
impl Output for LtpOutput {
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        if event.egress.len() > MAX_PAYLOAD_LEN {
            self.route_failure(
                event,
                ack,
                &format!(
                    "LTP payload length {} exceeds the {} byte limit",
                    event.egress.len(),
                    MAX_PAYLOAD_LEN
                ),
            )
            .await;
            return Ok(());
        }

        let mut working = self.working_event_meta(event);
        let mut attempt = 0u32;
        let mut wait = self.retry.initial_wait;
        let mut shutdown = self.shutdown_signal.clone();
        loop {
            match self
                .write_attempt(&mut working, &event.egress, &mut shutdown)
                .await
            {
                WriteOutcome::Delivered => {
                    self.metrics.in_retry.set(0);
                    self.metrics.events_written.inc();
                    ack.resolve_delivered();
                    return Ok(());
                }
                WriteOutcome::PreSendShutdown => {
                    self.metrics.in_retry.set(0);
                    self.route_failure(
                        event,
                        ack,
                        "LTP write abandoned on shutdown before event bytes",
                    )
                    .await;
                    return Ok(());
                }
                WriteOutcome::Err(error) => {
                    attempt += 1;
                    self.metrics.retries.inc();
                    if attempt >= self.retry.max_attempts {
                        self.metrics.in_retry.set(0);
                        self.route_failure(
                            event,
                            ack,
                            &format!("LTP write failed after {attempt} attempts: {error}"),
                        )
                        .await;
                        return Ok(());
                    }
                    self.metrics.in_retry.set(1);
                    if crate::modules::sleep_or_shutdown(&mut shutdown, wait).await {
                        self.metrics.in_retry.set(0);
                        self.route_failure(
                            event,
                            ack,
                            &format!("LTP write failed and shutdown interrupted retry: {error}"),
                        )
                        .await;
                        return Ok(());
                    }
                    wait = self.retry.next_wait(wait);
                }
            }
        }
    }

    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        if event.egress.len() > MAX_PAYLOAD_LEN {
            self.route_failure(
                event,
                ack,
                &format!(
                    "LTP payload length {} exceeds the {} byte limit",
                    event.egress.len(),
                    MAX_PAYLOAD_LEN
                ),
            )
            .await;
            return Ok(());
        }
        let mut working = self.working_event_meta(event);
        let result = self
            .write_shutdown_with_timeout(
                &mut working,
                &event.egress,
                crate::modules::SHUTDOWN_FLUSH_ATTEMPT_TIMEOUT,
            )
            .await;
        crate::modules::finalize_shutdown_singleton_disposition_ambiguous(
            result,
            self.error_log.as_ref(),
            self.error_log_fallback,
            &self.metrics,
            &self.name,
            event,
            ack,
        )
        .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::pin::Pin;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::AtomicI64;
    use std::sync::atomic::Ordering;
    use std::task::{Context as TaskContext, Poll};

    use chrono::TimeZone as _;
    use ring::rand::SystemRandom;
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use rustls::DistinguishedName;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
    use rustls::server::AlwaysResolvesServerRawPublicKeys;
    use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
    use rustls::sign::CertifiedKey;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use crate::dsl::ast::{Expr, Property};
    use crate::dsl::module_props::ModuleProperties;
    use crate::queue::AckDisposition;

    fn string_property(key: &str, value: &str) -> Property {
        Property::KeyValue {
            key: key.to_owned(),
            key_span: None,
            value: Expr::spanless(ExprKind::StringLit(value.to_owned())),
            value_span: None,
        }
    }

    fn peer_property(node_id: &str, pubkey: &str, endpoint: &str) -> Property {
        Property::Block {
            key: "peer".to_owned(),
            key_span: None,
            properties: vec![
                string_property("node_id", node_id),
                string_property("pubkey", pubkey),
                string_property("endpoint", endpoint),
            ],
        }
    }

    fn retry_once_property() -> Property {
        Property::Block {
            key: "retry".to_owned(),
            key_span: None,
            properties: vec![Property::KeyValue {
                key: "max_attempts".to_owned(),
                key_span: None,
                value: Expr::spanless(ExprKind::IntLit(1)),
                value_span: None,
            }],
        }
    }

    fn spki_for(pair: &Ed25519KeyPair) -> Vec<u8> {
        let mut spki = ED25519_SPKI_PREFIX.to_vec();
        spki.extend_from_slice(pair.public_key().as_ref());
        spki
    }

    fn generated_pair() -> (ring::pkcs8::Document, Ed25519KeyPair) {
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        (pkcs8, pair)
    }

    fn encoded_spki(pair: &Ed25519KeyPair) -> String {
        base64::engine::general_purpose::STANDARD.encode(spki_for(pair))
    }

    fn build_context_and_spki(node_id: &str) -> (crate::modules::BuildContext, Vec<u8>) {
        let (pkcs8, pair) = generated_pair();
        let spki = spki_for(&pair);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.pem");
        std::fs::write(
            &path,
            pem::encode(&pem::Pem::new("PRIVATE KEY", pkcs8.as_ref())),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut context = crate::modules::BuildContext {
            ltp_node_id: Some(Arc::<str>::from(node_id)),
            ltp_node_key: Some(Arc::new(crate::ltp::load_node_key(&path).unwrap())),
            ..crate::modules::BuildContext::for_testing()
        };
        context.ltp_metrics = Some(
            crate::metrics::LtpMetrics::register(
                &context.metrics,
                &std::collections::BTreeSet::from(["peer-a".to_owned()]),
            )
            .unwrap(),
        );
        (context, spki)
    }

    fn build_context(node_id: &str) -> crate::modules::BuildContext {
        build_context_and_spki(node_id).0
    }

    #[test]
    fn multiple_outputs_share_the_startup_parsed_certified_key() {
        let (ctx, _) = build_context_and_spki("node-a");
        let identity = ctx.ltp_node_key.as_ref().unwrap();
        let initial = identity.certified_key_strong_count();
        let (_, peer_key) = generated_pair();
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &encoded_spki(&peer_key),
                "127.0.0.1:7514",
            )],
        );

        let first = LtpOutput::build("out-a", &properties, &ctx).unwrap();
        assert_eq!(identity.certified_key_strong_count(), initial + 1);
        let second = LtpOutput::build("out-b", &properties, &ctx).unwrap();
        assert_eq!(identity.certified_key_strong_count(), initial + 2);

        drop((first, second));
        assert_eq!(identity.certified_key_strong_count(), initial);
    }

    fn output_for(endpoint: &str) -> LtpOutput {
        let (_, peer_key) = generated_pair();
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property("peer-a", &encoded_spki(&peer_key), endpoint)],
        );
        LtpOutput::build("out", &properties, &build_context("node-a")).unwrap()
    }

    #[derive(Clone, Copy)]
    enum FlushAction {
        Ok,
        Error,
        Pending,
    }

    struct ScriptedWriter {
        bytes: Vec<u8>,
        max_write: usize,
        flush_action: FlushAction,
        flushes: usize,
        mark_first_flush: bool,
    }

    impl ScriptedWriter {
        fn new(max_write: usize, flush_action: FlushAction) -> Self {
            Self {
                bytes: Vec::new(),
                max_write,
                flush_action,
                flushes: 0,
                mark_first_flush: false,
            }
        }
    }

    impl AsyncWrite for ScriptedWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let written = buf.len().min(self.max_write);
            self.bytes.extend_from_slice(&buf[..written]);
            Poll::Ready(Ok(written))
        }

        fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            self.flushes += 1;
            if self.mark_first_flush && self.flushes == 1 {
                HELLO_FLUSHED.store(true, Ordering::SeqCst);
            }
            match self.flush_action {
                FlushAction::Ok => Poll::Ready(Ok(())),
                FlushAction::Error => Poll::Ready(Err(io::Error::other("scripted flush failure"))),
                FlushAction::Pending => Poll::Pending,
            }
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Debug)]
    struct PinnedClientRpkVerifier {
        expected_spki: Vec<u8>,
        algorithms: WebPkiSupportedAlgorithms,
    }

    impl ClientCertVerifier for PinnedClientRpkVerifier {
        fn root_hint_subjects(&self) -> &[DistinguishedName] {
            &[]
        }

        fn verify_client_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            intermediates: &[CertificateDer<'_>],
            _now: UnixTime,
        ) -> std::result::Result<ClientCertVerified, TlsError> {
            if !intermediates.is_empty() || end_entity.as_ref() != self.expected_spki {
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

    fn server_config(
        server_pkcs8: &[u8],
        server_spki: &[u8],
        expected_client_spki: &[u8],
    ) -> rustls::ServerConfig {
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let signing_key = provider
            .key_provider
            .load_private_key(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                server_pkcs8.to_vec(),
            )))
            .unwrap();
        let certified_key = CertifiedKey::new(
            vec![CertificateDer::from(server_spki.to_vec())],
            signing_key,
        );
        let client_verifier = PinnedClientRpkVerifier {
            expected_spki: expected_client_spki.to_vec(),
            algorithms: provider.signature_verification_algorithms,
        };
        rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(Arc::new(client_verifier))
            .with_cert_resolver(Arc::new(AlwaysResolvesServerRawPublicKeys::new(Arc::new(
                certified_key,
            ))))
    }

    async fn read_outer_frame<S>(stream: &mut S) -> Bytes
    where
        S: tokio::io::AsyncRead + Unpin,
    {
        let mut prefix = [0u8; crate::ltp::FRAME_PREFIX_SIZE];
        stream.read_exact(&mut prefix).await.unwrap();
        assert_eq!(&prefix[..4], crate::ltp::FRAME_MAGIC);
        assert_eq!(prefix[4], crate::ltp::FRAME_VERSION);
        let meta_len = u32::from_be_bytes(prefix[5..9].try_into().unwrap()) as usize;
        let mut meta_and_payload_len = vec![0u8; meta_len + crate::ltp::PAYLOAD_LEN_SIZE];
        stream.read_exact(&mut meta_and_payload_len).await.unwrap();
        let payload_len = u32::from_be_bytes(
            meta_and_payload_len[meta_len..meta_len + 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let mut payload = vec![0u8; payload_len];
        stream.read_exact(&mut payload).await.unwrap();
        Bytes::from([prefix.to_vec(), meta_and_payload_len, payload].concat())
    }

    fn raw_meta(frame: &Bytes) -> &[u8] {
        let meta_len = u32::from_be_bytes(frame[5..9].try_into().unwrap()) as usize;
        &frame[9..9 + meta_len]
    }

    #[test]
    fn config_accepts_one_peer_and_defaults_port_7514() {
        let (_, peer_key) = generated_pair();
        let peer = parse_peer(
            "out",
            &[peer_property(
                "peer-a",
                &encoded_spki(&peer_key),
                "collector.example",
            )],
        )
        .unwrap();
        assert_eq!(peer.node_id, "peer-a");
        assert_eq!(peer.address, "collector.example:7514");
        assert_eq!(peer.public_key_spki, spki_for(&peer_key));
    }

    #[test]
    fn config_rejects_peer_cardinality_fields_spki_and_endpoint_errors() {
        let (_, peer_key) = generated_pair();
        let pubkey = encoded_spki(&peer_key);
        assert!(
            parse_peer("out", &[])
                .unwrap_err()
                .to_string()
                .contains("exactly one")
        );
        assert!(
            parse_peer(
                "out",
                &[
                    peer_property("a", &pubkey, "one.example"),
                    peer_property("b", &pubkey, "two.example"),
                ],
            )
            .unwrap_err()
            .to_string()
            .contains("exactly one")
        );
        for (key, expected) in [
            ("node_id", "node_id requires a string value"),
            ("pubkey", "pubkey requires a string value"),
            ("endpoint", "endpoint requires a string value"),
        ] {
            let properties = vec![Property::Block {
                key: "peer".to_owned(),
                key_span: None,
                properties: vec![
                    (key != "node_id").then(|| string_property("node_id", "peer-a")),
                    (key != "pubkey").then(|| string_property("pubkey", &pubkey)),
                    (key != "endpoint").then(|| string_property("endpoint", "host")),
                ]
                .into_iter()
                .flatten()
                .collect(),
            }];
            let error = parse_peer("out", &properties).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }
        let bad_spki = base64::engine::general_purpose::STANDARD.encode([0u8; 44]);
        assert!(
            parse_peer("out", &[peer_property("peer-a", &bad_spki, "host")])
                .unwrap_err()
                .to_string()
                .contains("Ed25519 SPKI")
        );
        assert!(
            parse_peer("out", &[peer_property("peer-a", &pubkey, "host:70000")])
                .unwrap_err()
                .to_string()
                .contains("port")
        );
    }

    #[test]
    fn build_accepts_the_exact_event_metadata_limit_and_rejects_limit_plus_one() {
        let key = [0; 16];
        let mut low = 0;
        let mut high = MAX_META_LEN + 1;
        while low < high {
            let middle = (low + high).div_ceil(2);
            if event_meta(&"n".repeat(middle), &key, 1, 1).encoded_len() <= MAX_META_LEN {
                low = middle;
            } else {
                high = middle - 1;
            }
        }

        let exact_node_id = "n".repeat(low);
        let oversized_node_id = "n".repeat(low + 1);
        assert_eq!(
            event_meta(&exact_node_id, &key, 1, 1).encoded_len(),
            MAX_META_LEN
        );
        assert_eq!(
            event_meta(&oversized_node_id, &key, 1, 1).encoded_len(),
            MAX_META_LEN + 1
        );
        assert!(
            encode_hello_frame(&LtpHello {
                node_id: oversized_node_id.clone(),
            })
            .is_ok(),
            "the hello fits even though the real event metadata does not"
        );

        let (_, peer_key) = generated_pair();
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &encoded_spki(&peer_key),
                "127.0.0.1:7514",
            )],
        );
        let (mut context, _) = build_context_and_spki(&exact_node_id);
        LtpOutput::build("exact", &properties, &context).unwrap();

        context.ltp_node_id = Some(Arc::<str>::from(oversized_node_id));
        let error = match LtpOutput::build("oversized", &properties, &context) {
            Ok(_) => panic!("event metadata above the wire limit was accepted"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("LTP event metadata 65537 bytes"));
    }

    fn now_200() -> crate::time::UnixNanos {
        crate::time::UnixNanos::new(200)
    }

    fn now_300() -> crate::time::UnixNanos {
        crate::time::UnixNanos::new(300)
    }

    fn now_negative() -> crate::time::UnixNanos {
        crate::time::UnixNanos::new(-1)
    }

    static HELLO_FLUSHED: AtomicBool = AtomicBool::new(false);

    fn now_after_hello_flush() -> crate::time::UnixNanos {
        assert!(
            HELLO_FLUSHED.load(Ordering::SeqCst),
            "departure time must be sampled after the hello has flushed"
        );
        crate::time::UnixNanos::new(200)
    }

    #[test]
    fn event_frame_carries_only_key_own_stamp_and_egress() {
        let mut output = output_for("127.0.0.1:1");
        output.now = now_200;
        let mut event = Event::new(
            Bytes::from_static(b"ingress"),
            "127.0.0.1:1".parse().unwrap(),
        );
        event.received_at = Utc.timestamp_nanos(100);
        event.egress = Bytes::from_static(b"egress");
        event.workspace.insert(
            "secret".to_owned(),
            crate::dsl::value::OwnedValue::String("no".into()),
        );

        let mut working = output.working_event_meta(&event);
        assert_eq!(working.requested_capacity, 1);
        let frame = output.event_frame(&mut working, &event.egress).unwrap();
        let (meta, payload) = crate::ltp::decode_frame(&frame).unwrap();
        assert_eq!(meta.key, event.key().as_bytes());
        assert_eq!(meta.stamps.len(), 1);
        assert_eq!(meta.stamps[0].node_id, "node-a");
        assert_eq!(meta.stamps[0].arrival_unix_nano, 100);
        assert_eq!(meta.stamps[0].departure_unix_nano, 200);
        assert_eq!(payload, Bytes::from_static(b"egress"));
        assert!(!frame.windows(6).any(|window| window == b"secret"));
        assert!(!frame.windows(7).any(|window| window == b"ingress"));
    }

    #[test]
    fn sixteen_hop_delivery_clones_once_and_reuses_working_metadata_across_attempts() {
        let output = output_for("127.0.0.1:1");
        let mut stamps: Vec<HopStamp> = (0..15)
            .map(|index| HopStamp {
                node_id: format!("peer-{index}"),
                arrival_unix_nano: index,
                departure_unix_nano: index + 1,
            })
            .collect();
        stamps.push(HopStamp {
            node_id: "node-a".to_owned(),
            arrival_unix_nano: 100,
            departure_unix_nano: 0,
        });
        let event = Event::from_ltp_parts(
            uuid::Uuid::now_v7(),
            Utc.timestamp_nanos(100),
            "127.0.0.1:7514".parse().unwrap(),
            Bytes::from_static(b"payload"),
            stamps.clone(),
        );

        STAMP_CLONE_COUNT.set(0);
        let mut working = output.working_event_meta(&event);
        let first = output.event_frame(&mut working, &event.egress).unwrap();
        let second = output.event_frame(&mut working, &event.egress).unwrap();

        assert_eq!(STAMP_CLONE_COUNT.get(), 1);
        assert_eq!(working.meta.stamps.len(), 16);
        assert_eq!(working.local_stamp, 15);
        assert_eq!(event.ltp_stamps(), stamps);
        for frame in [first, second] {
            let (meta, payload) = crate::ltp::decode_frame(&frame).unwrap();
            assert_eq!(meta.stamps.len(), 16);
            assert_eq!(meta.stamps[..15], stamps[..15]);
            assert_eq!(meta.stamps[15].node_id, "node-a");
            assert_ne!(meta.stamps[15].departure_unix_nano, 0);
            assert_eq!(payload, Bytes::from_static(b"payload"));
        }
    }

    #[test]
    fn fanout_outputs_build_independent_exact_capacity_metadata_without_mutating_prefix() {
        let stamps = vec![HopStamp {
            node_id: "peer".to_owned(),
            arrival_unix_nano: 1,
            departure_unix_nano: 2,
        }];
        let event = Event::from_ltp_parts(
            uuid::Uuid::now_v7(),
            Utc.timestamp_nanos(100),
            "127.0.0.1:7514".parse().unwrap(),
            Bytes::from_static(b"payload"),
            stamps.clone(),
        );
        let mut output_a = output_for("127.0.0.1:1");
        output_a.now = now_200;
        let mut output_b = output_for("127.0.0.1:2");
        output_b.node_id = Arc::from("node-b");
        output_b.now = now_300;

        let mut working_a = output_a.working_event_meta(&event);
        let mut working_b = output_b.working_event_meta(&event);

        assert_eq!(working_b.meta.stamps[0], stamps[0]);
        assert_eq!(working_a.requested_capacity, stamps.len() + 1);
        assert_eq!(working_b.requested_capacity, stamps.len() + 1);
        let frame_a = output_a.event_frame(&mut working_a, &event.egress).unwrap();
        let frame_b = output_b.event_frame(&mut working_b, &event.egress).unwrap();
        let (meta_a, _) = crate::ltp::decode_frame(&frame_a).unwrap();
        let (meta_b, _) = crate::ltp::decode_frame(&frame_b).unwrap();
        assert_eq!(meta_a.stamps[1].node_id, "node-a");
        assert_eq!(meta_a.stamps[1].departure_unix_nano, 200);
        assert_eq!(meta_b.stamps[1].node_id, "node-b");
        assert_eq!(meta_b.stamps[1].departure_unix_nano, 300);
        assert_eq!(event.ltp_stamps(), stamps);

        let source = include_str!("ltp.rs");
        let body = source
            .split("fn working_event_meta")
            .nth(1)
            .and_then(|tail| tail.split("fn event_frame").next())
            .expect("working_event_meta source");
        assert!(!body.contains("ltp_stamps().to_vec()"));
        assert_eq!(body.matches("Vec::with_capacity").count(), 1);
        assert!(body.contains("let mut stamps = Vec::with_capacity(requested_capacity);"));
        assert!(body.contains("extend_from_slice"));
    }

    #[test]
    fn exact_metadata_limit_from_input_encodes_through_the_real_output_path() {
        let key = uuid::Uuid::now_v7();
        let node_id = ((MAX_META_LEN - 128)..=MAX_META_LEN)
            .find_map(|len| {
                let node_id = "n".repeat(len);
                let meta = LtpMeta {
                    key: key.as_bytes().to_vec(),
                    stamps: vec![HopStamp {
                        node_id: node_id.clone(),
                        arrival_unix_nano: 123,
                        departure_unix_nano: 1,
                    }],
                };
                (meta.encoded_len() == MAX_META_LEN).then_some(node_id)
            })
            .unwrap();
        let mut output = output_for("127.0.0.1:1");
        output.node_id = Arc::from(node_id.clone());
        output.now = now_200;
        let event = Event::from_ltp_parts(
            key,
            Utc.timestamp_nanos(123),
            "127.0.0.1:7514".parse().unwrap(),
            Bytes::from_static(b"payload"),
            vec![HopStamp {
                node_id,
                arrival_unix_nano: 123,
                departure_unix_nano: 0,
            }],
        );

        let mut working = output.working_event_meta(&event);
        let frame = output.event_frame(&mut working, &event.egress).unwrap();
        let (meta, payload) = crate::ltp::decode_frame(&frame).unwrap();
        assert_eq!(meta.encoded_len(), MAX_META_LEN);
        assert_eq!(meta.stamps.last().unwrap().departure_unix_nano, 200);
        assert_eq!(payload, Bytes::from_static(b"payload"));
    }

    #[test]
    fn output_appends_one_local_stamp_when_input_history_ends_at_a_peer() {
        let output = output_for("127.0.0.1:1");
        let stamps: Vec<HopStamp> = (0..15)
            .map(|index| HopStamp {
                node_id: format!("peer-{index}"),
                arrival_unix_nano: index,
                departure_unix_nano: index + 1,
            })
            .collect();
        let event = Event::from_ltp_parts(
            uuid::Uuid::now_v7(),
            Utc.timestamp_nanos(100),
            "127.0.0.1:7514".parse().unwrap(),
            Bytes::from_static(b"payload"),
            stamps,
        );

        let working = output.working_event_meta(&event);
        assert_eq!(working.meta.stamps.len(), 16);
        assert_eq!(working.local_stamp, 15);
        assert_eq!(working.meta.stamps[15].node_id, "node-a");
        assert_eq!(working.meta.stamps[15].departure_unix_nano, 0);
    }

    #[test]
    fn negative_or_unrepresentable_timestamps_map_to_zero() {
        let mut output = output_for("127.0.0.1:1");
        output.now = now_negative;
        let mut event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        event.received_at = Utc.timestamp_nanos(-1);
        let mut working = output.working_event_meta(&event);
        let frame = output.event_frame(&mut working, &event.egress).unwrap();
        let (meta, _) = crate::ltp::decode_frame(&frame).unwrap();
        assert_eq!(meta.stamps[0].arrival_unix_nano, 0);
        assert_eq!(meta.stamps[0].departure_unix_nano, 0);
    }

    #[tokio::test]
    async fn event_delivery_requires_flush_and_counts_only_a_flushed_outer_frame() {
        let mut output = output_for("127.0.0.1:1");
        output.now = now_200;
        let mut event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        event.received_at = Utc.timestamp_nanos(100);
        event.egress = Bytes::from_static(b"payload");
        let mut working = output.working_event_meta(&event);
        let expected_len = output
            .event_frame(&mut working, &event.egress)
            .unwrap()
            .len() as u64;

        let mut flush_failure = ScriptedWriter::new(2, FlushAction::Error);
        assert!(
            output
                .write_event(&mut flush_failure, &mut working, &event.egress)
                .await
                .is_err()
        );
        assert_eq!(flush_failure.bytes.len(), expected_len as usize);
        assert_eq!(flush_failure.flushes, 1);
        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert_eq!(output.peer_metrics.intra_latency.count(), 0);

        let mut success = ScriptedWriter::new(3, FlushAction::Ok);
        output
            .write_event(&mut success, &mut working, &event.egress)
            .await
            .unwrap();
        assert_eq!(success.bytes.len(), expected_len as usize);
        assert_eq!(success.flushes, 1);
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            expected_len,
            "a retry success owns exactly one event-frame byte increment"
        );
        assert_eq!(output.peer_metrics.intra_latency.count(), 1);
        assert_eq!(
            output.peer_metrics.intra_latency.sum(),
            100.0 / 1_000_000_000.0
        );
    }

    #[tokio::test]
    async fn flush_timeout_is_a_write_failure() {
        let mut writer = ScriptedWriter::new(usize::MAX, FlushAction::Pending);
        let error = LtpOutput::write_bytes_with_timeout(
            &mut writer,
            b"frame",
            std::time::Duration::from_millis(10),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert_eq!(writer.bytes, b"frame");
        assert!(writer.flushes >= 1);
    }

    #[tokio::test]
    async fn departure_is_sampled_after_the_hello_flush() {
        HELLO_FLUSHED.store(false, Ordering::SeqCst);
        let mut output = output_for("127.0.0.1:1");
        output.now = now_after_hello_flush;
        let event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        let mut writer = ScriptedWriter::new(usize::MAX, FlushAction::Ok);
        writer.mark_first_flush = true;

        let mut working = output.working_event_meta(&event);
        output
            .write_hello_and_event(&mut writer, &mut working, &event.egress)
            .await
            .unwrap();

        assert_eq!(writer.flushes, 2);
        assert!(HELLO_FLUSHED.load(Ordering::SeqCst));
    }

    #[test]
    fn rpk_verifier_pins_exact_spki_and_requires_rpk_negotiation() {
        let (_, expected_key) = generated_pair();
        let (_, wrong_key) = generated_pair();
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let verifier = PinnedRpkVerifier {
            expected_spki: spki_for(&expected_key),
            algorithms: provider.signature_verification_algorithms,
        };
        let server_name = ServerName::try_from("peer.example").unwrap();
        assert!(verifier.requires_raw_public_keys());
        assert!(
            verifier
                .verify_server_cert(
                    &CertificateDer::from(spki_for(&expected_key)),
                    &[],
                    &server_name,
                    &[],
                    UnixTime::since_unix_epoch(std::time::Duration::ZERO),
                )
                .is_ok()
        );
        assert!(
            verifier
                .verify_server_cert(
                    &CertificateDer::from(spki_for(&wrong_key)),
                    &[],
                    &server_name,
                    &[],
                    UnixTime::since_unix_epoch(std::time::Duration::ZERO),
                )
                .is_err()
        );
        assert!(
            verifier
                .verify_server_cert(
                    &CertificateDer::from(vec![0x30, 0x00]),
                    &[],
                    &server_name,
                    &[],
                    UnixTime::since_unix_epoch(std::time::Duration::ZERO),
                )
                .is_err(),
            "an X.509-shaped certificate must not bypass the SPKI pin"
        );
    }

    #[tokio::test]
    async fn oversize_payload_routes_directly_to_dlq_without_connect_or_retry() {
        let output = output_for("127.0.0.1:1");
        let mut event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        event.egress = Bytes::from(vec![0; MAX_PAYLOAD_LEN + 1]);
        let (ack, mut ack_rx) = QueueAckHandle::for_test();

        output.consume(&event, ack).await.unwrap();

        assert!(output.connection.lock().await.is_none());
        assert_eq!(output.metrics.retries.load(Ordering::Relaxed), 0);
        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Recovered))
        ));
    }

    #[tokio::test]
    async fn payload_at_the_exact_limit_enters_the_send_path() {
        let (_, peer_key) = generated_pair();
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![
                peer_property("peer-a", &encoded_spki(&peer_key), "127.0.0.1:1"),
                retry_once_property(),
            ],
        );
        let output = LtpOutput::build("out", &properties, &build_context("node-a")).unwrap();
        let mut event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        event.egress = Bytes::from(vec![0; MAX_PAYLOAD_LEN]);
        let (ack, mut ack_rx) = QueueAckHandle::for_test();

        output.consume(&event, ack).await.unwrap();

        assert_eq!(output.metrics.retries.load(Ordering::Relaxed), 1);
        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Recovered))
        ));
    }

    #[tokio::test]
    async fn mutual_rpk_connection_sends_hello_before_event_and_preserves_projection() {
        let (server_pkcs8, server_pair) = generated_pair();
        let server_spki = spki_for(&server_pair);
        let (ctx, client_spki) = build_context_and_spki("node-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config(
            server_pkcs8.as_ref(),
            &server_spki,
            &client_spki,
        )));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            let hello = read_outer_frame(&mut tls).await;
            let event = read_outer_frame(&mut tls).await;
            (hello, event)
        });

        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &base64::engine::general_purpose::STANDARD.encode(&server_spki),
                &address.to_string(),
            )],
        );
        let mut output = LtpOutput::build("out", &properties, &ctx).unwrap();
        output.now = now_200;
        let mut event = Event::new(
            Bytes::from_static(b"ingress"),
            "127.0.0.1:1".parse().unwrap(),
        );
        event.received_at = Utc.timestamp_nanos(100);
        event.egress = Bytes::from_static(b"payload");
        let event_key = event.key();
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        output.consume(&event, ack).await.unwrap();
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Delivered))
        ));

        let (hello_frame, event_frame) = server.await.unwrap();
        let hello = LtpHello::decode(raw_meta(&hello_frame)).unwrap();
        assert_eq!(hello.node_id, "node-a");
        assert_eq!(hello_frame[hello_frame.len() - 4..], [0, 0, 0, 0]);
        let (meta, payload) = crate::ltp::decode_frame(&event_frame).unwrap();
        assert_eq!(meta.key, event_key.as_bytes());
        assert_eq!(meta.stamps.len(), 1);
        assert_eq!(meta.stamps[0].arrival_unix_nano, 100);
        assert_eq!(meta.stamps[0].departure_unix_nano, 200);
        assert_eq!(payload, Bytes::from_static(b"payload"));
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            event_frame.len() as u64,
            "only the flushed event outer frame contributes to bytes_written"
        );
    }

    #[tokio::test]
    async fn shutdown_success_counts_the_flushed_event_frame_once() {
        let (server_pkcs8, server_pair) = generated_pair();
        let server_spki = spki_for(&server_pair);
        let (ctx, client_spki) = build_context_and_spki("node-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config(
            server_pkcs8.as_ref(),
            &server_spki,
            &client_spki,
        )));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            let hello = read_outer_frame(&mut tls).await;
            let event = read_outer_frame(&mut tls).await;
            (hello, event)
        });
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &base64::engine::general_purpose::STANDARD.encode(&server_spki),
                &address.to_string(),
            )],
        );
        let output = LtpOutput::build("out", &properties, &ctx).unwrap();
        let mut event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        event.egress = Bytes::from_static(b"shutdown");
        let (ack, mut ack_rx) = QueueAckHandle::for_test();

        output.consume_shutdown(&event, ack).await.unwrap();

        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Delivered))
        ));
        let (hello_frame, event_frame) = server.await.unwrap();
        assert_eq!(
            LtpHello::decode(raw_meta(&hello_frame)).unwrap().node_id,
            "node-a"
        );
        assert_eq!(
            crate::ltp::decode_frame(&event_frame).unwrap().1,
            Bytes::from_static(b"shutdown")
        );
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            event_frame.len() as u64,
            "shutdown success owns exactly one event-frame byte increment"
        );
        assert_eq!(output.peer_metrics.intra_latency.count(), 1);
    }

    #[tokio::test]
    async fn cancelled_cached_shutdown_write_drops_stream_and_reconnects_with_hello() {
        let (server_pkcs8, server_pair) = generated_pair();
        let server_spki = spki_for(&server_pair);
        let (ctx, client_spki) = build_context_and_spki("node-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config(
            server_pkcs8.as_ref(),
            &server_spki,
            &client_spki,
        )));
        let server = tokio::spawn(async move {
            let (first_tcp, _) = listener.accept().await.unwrap();
            let mut first_tls = acceptor.accept(first_tcp).await.unwrap();
            let first_hello = read_outer_frame(&mut first_tls).await;
            let first_event = read_outer_frame(&mut first_tls).await;

            // Keep the first connection open without reading again. The
            // client's maximum-sized cached write must remain pending until
            // the outer shutdown timeout cancels and drops that stream.
            let (second_tcp, _) = listener.accept().await.unwrap();
            let mut second_tls = acceptor.accept(second_tcp).await.unwrap();
            let second_hello = read_outer_frame(&mut second_tls).await;
            let second_event = read_outer_frame(&mut second_tls).await;
            (first_hello, first_event, second_hello, second_event)
        });
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &base64::engine::general_purpose::STANDARD.encode(&server_spki),
                &address.to_string(),
            )],
        );
        let mut output = LtpOutput::build("out", &properties, &ctx).unwrap();
        output.now = now_200;

        let first = Event::new(Bytes::from_static(b"first"), "127.0.0.1:1".parse().unwrap());
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        output.consume(&first, ack).await.unwrap();
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Delivered))
        ));
        assert!(output.connection.lock().await.is_some());

        let mut pending = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        pending.egress = Bytes::from(vec![0; MAX_PAYLOAD_LEN]);
        let mut pending_working = output.working_event_meta(&pending);
        assert!(
            output
                .write_shutdown_with_timeout(
                    &mut pending_working,
                    &pending.egress,
                    std::time::Duration::from_millis(100),
                )
                .await
                .is_err()
        );
        assert!(
            output.connection.lock().await.is_none(),
            "cancellation must drop the taken cached stream"
        );

        let next = Event::new(Bytes::from_static(b"next"), "127.0.0.1:1".parse().unwrap());
        let mut next_working = output.working_event_meta(&next);
        output
            .write_shutdown_attempt(&mut next_working, &next.egress)
            .await
            .unwrap();

        let (first_hello, first_event, second_hello, second_event) = server.await.unwrap();
        for hello in [&first_hello, &second_hello] {
            assert_eq!(LtpHello::decode(raw_meta(hello)).unwrap().node_id, "node-a");
        }
        assert_eq!(
            crate::ltp::decode_frame(&second_event).unwrap().1,
            Bytes::from_static(b"next")
        );
        assert_eq!(
            output.metrics.bytes_written.load(Ordering::Relaxed),
            (first_event.len() + second_event.len()) as u64,
            "the cancelled partial frame must not be counted"
        );
    }

    #[tokio::test]
    async fn every_new_connection_sends_hello_before_its_event() {
        let (server_pkcs8, server_pair) = generated_pair();
        let server_spki = spki_for(&server_pair);
        let (ctx, client_spki) = build_context_and_spki("node-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config(
            server_pkcs8.as_ref(),
            &server_spki,
            &client_spki,
        )));
        let server = tokio::spawn(async move {
            let mut connections = Vec::new();
            for _ in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut tls = acceptor.accept(tcp).await.unwrap();
                connections.push((
                    read_outer_frame(&mut tls).await,
                    read_outer_frame(&mut tls).await,
                ));
            }
            connections
        });

        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &base64::engine::general_purpose::STANDARD.encode(&server_spki),
                &address.to_string(),
            )],
        );
        let output = LtpOutput::build("out", &properties, &ctx).unwrap();
        for payload in [b"one".as_slice(), b"two".as_slice()] {
            let mut event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
            event.egress = Bytes::copy_from_slice(payload);
            let (ack, mut ack_rx) = QueueAckHandle::for_test();
            output.consume(&event, ack).await.unwrap();
            assert!(matches!(
                ack_rx.recv().await,
                Some((_, AckDisposition::Delivered))
            ));
            *output.connection.lock().await = None;
        }

        let connections = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(connections.len(), 2);
        for (index, (hello_frame, event_frame)) in connections.into_iter().enumerate() {
            assert_eq!(
                LtpHello::decode(raw_meta(&hello_frame)).unwrap().node_id,
                "node-a"
            );
            assert_eq!(
                crate::ltp::decode_frame(&event_frame).unwrap().1,
                Bytes::from_static(if index == 0 { b"one" } else { b"two" })
            );
        }
    }

    #[tokio::test]
    async fn tls_handshake_rejects_a_server_outside_the_spki_pin() {
        let (server_pkcs8, server_pair) = generated_pair();
        let server_spki = spki_for(&server_pair);
        let (_, wrong_pair) = generated_pair();
        let (ctx, client_spki) = build_context_and_spki("node-a");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(server_config(
            server_pkcs8.as_ref(),
            &server_spki,
            &client_spki,
        )));
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            acceptor.accept(tcp).await
        });
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &encoded_spki(&wrong_pair),
                &address.to_string(),
            )],
        );
        let output = LtpOutput::build("out", &properties, &ctx).unwrap();
        assert!(output.connect().await.is_err());
        assert!(server.await.unwrap().is_err());
    }

    static RETRY_NOW: AtomicI64 = AtomicI64::new(300);

    fn retry_now() -> crate::time::UnixNanos {
        crate::time::UnixNanos::new(RETRY_NOW.fetch_add(1, Ordering::SeqCst))
    }

    #[test]
    fn rebuilding_an_attempt_refreshes_only_departure_time() {
        RETRY_NOW.store(300, Ordering::SeqCst);
        let mut output = output_for("127.0.0.1:1");
        output.now = retry_now;
        let mut event = Event::new(Bytes::from_static(b"x"), "127.0.0.1:1".parse().unwrap());
        event.received_at = Utc.timestamp_nanos(100);
        let mut working = output.working_event_meta(&event);
        let first = output.event_frame(&mut working, &event.egress).unwrap();
        let second = output.event_frame(&mut working, &event.egress).unwrap();
        let first_meta = crate::ltp::decode_frame(&first).unwrap().0;
        let second_meta = crate::ltp::decode_frame(&second).unwrap().0;
        assert_eq!(first_meta.key, second_meta.key);
        assert_eq!(first_meta.stamps[0].arrival_unix_nano, 100);
        assert_eq!(second_meta.stamps[0].arrival_unix_nano, 100);
        assert_eq!(first_meta.stamps[0].departure_unix_nano, 300);
        assert_eq!(second_meta.stamps[0].departure_unix_nano, 301);
    }

    #[tokio::test]
    async fn connect_failure_uses_the_existing_retry_and_dlq_contract() {
        let (_, peer_key) = generated_pair();
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![
                peer_property("peer-a", &encoded_spki(&peer_key), "127.0.0.1:1"),
                retry_once_property(),
            ],
        );
        let output = LtpOutput::build("out", &properties, &build_context("node-a")).unwrap();
        let event = Event::new(Bytes::from_static(b"x"), "127.0.0.1:1".parse().unwrap());
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        output.consume(&event, ack).await.unwrap();
        assert_eq!(output.metrics.retries.load(Ordering::Relaxed), 1);
        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert_eq!(output.peer_metrics.intra_latency.count(), 0);
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Recovered))
        ));
    }

    #[tokio::test]
    async fn pre_send_shutdown_cancels_without_connect_or_retry() {
        let (_, peer_key) = generated_pair();
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(true);
        let mut ctx = build_context("node-a");
        ctx.shutdown_signal = shutdown_rx;
        let properties = ModuleProperties::from_parts(
            "ltp",
            vec![peer_property(
                "peer-a",
                &encoded_spki(&peer_key),
                "127.0.0.1:1",
            )],
        );
        let output = LtpOutput::build("out", &properties, &ctx).unwrap();
        let event = Event::new(Bytes::from_static(b"x"), "127.0.0.1:1".parse().unwrap());
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        output.consume(&event, ack).await.unwrap();
        assert!(output.connection.lock().await.is_none());
        assert_eq!(output.metrics.retries.load(Ordering::Relaxed), 0);
        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Recovered))
        ));
        drop(shutdown_tx);
    }

    #[tokio::test]
    async fn shutdown_drain_routes_oversize_without_connecting() {
        let output = output_for("127.0.0.1:1");
        let mut event = Event::new(Bytes::new(), "127.0.0.1:1".parse().unwrap());
        event.egress = Bytes::from(vec![0; MAX_PAYLOAD_LEN + 1]);
        let (ack, mut ack_rx) = QueueAckHandle::for_test();
        output.consume_shutdown(&event, ack).await.unwrap();
        assert!(output.connection.lock().await.is_none());
        assert_eq!(output.metrics.retries.load(Ordering::Relaxed), 0);
        assert_eq!(output.metrics.bytes_written.load(Ordering::Relaxed), 0);
        assert!(matches!(
            ack_rx.recv().await,
            Some((_, AckDisposition::Recovered))
        ));
    }
}
