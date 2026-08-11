//! OTLP/gRPC output: forwards Events to one or more OpenTelemetry
//! collectors / SaaS backends via OTLP over gRPC.
//!
//! ```text
//! def output otlp_out {
//!     type otlp_grpc
//!     peers {
//!         peer {
//!             endpoint "https://collector-a.example.com:4317"
//!             tls { ca "/etc/limpid/ca.crt" }
//!         }
//!         peer {
//!             endpoint "https://collector-b.example.com:4317"
//!             tls {
//!                 ca   "/etc/limpid/ca.crt"
//!                 cert "/etc/limpid/client.crt"
//!                 key  "/etc/limpid/client.key"
//!             }
//!         }
//!     }
//!     batch_size 512
//!     batch_timeout "5s"
//!     headers {
//!         Authorization "Bearer ${env.OTLP_TOKEN}"
//!     }
//! }
//! ```
//!
//! ### Endpoint conventions
//!
//! Each `peer.endpoint` is the gRPC server URL (typically `:4317`).
//! The service name
//! (`opentelemetry.proto.collector.logs.v1.LogsService`) is implicit
//! in the generated client. `https://` and `http://` schemes select
//! TLS / plaintext respectively. Headers translate to gRPC metadata.
//!
//! ### Round-robin + cooldown
//!
//! On each flush, peers are tried in round-robin order. A peer that
//! fails the request is marked cooled-down for `PEER_COOLDOWN` (5s,
//! shared with the syslog outputs) and skipped on subsequent flushes
//! until the cooldown expires. The `retry { … }` block controls the
//! per-flush retry budget; within one budget the rotation transparently
//! picks the next available peer. If every peer is currently cooled
//! the rotation falls back to the cursor start — the retry budget,
//! not the cooldown, protects the single-peer-just-failed case.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, logs_service_client::LogsServiceClient,
};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::output::batched::{BatchSinkPolicy, BatchedSink, SendOutcome};
use crate::modules::output::syslog_peers::{RotatingPeers, iter_peers_block};
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{QueueAckHandle, RetryConfig};

use super::{BatchLevel, decode_drained_to_request};

/// Upper bound on a single gRPC export. A stalled collector (TCP
/// connection accepted but no HEADERS frame returned) would otherwise
/// hold the flush future open indefinitely and starve the
/// rotation/retry path. 30s is loose enough for normal collector
/// latency including TLS handshake; flushes that take longer are
/// almost certainly hung.
const GRPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct GrpcPeer {
    endpoint: String,
    channel: Channel,
}

/// Transport policy plugged into the shared [`BatchedSink`] skeleton:
/// render = refcount bump on the per-event ResourceLogs proto bytes,
/// prepare = decode + `batch_level` merge into one
/// `ExportLogsServiceRequest`, send = one attempt against the next
/// rotation candidate. Buffering, retry (the shared `RetryConfig`
/// vocabulary spliced in by the queue layer), and the shutdown
/// lifecycle live in `crate::modules::output::batched`.
struct OtlpGrpcSinkPolicy {
    peers: Vec<GrpcPeer>,
    /// Round-robin cursor + per-peer failure cooldown; one candidate
    /// per send attempt. See `RotatingPeers` for the selection and
    /// cooldown contract.
    rotation: RotatingPeers,
    batch_level: BatchLevel,
    headers: Vec<(String, String)>,
}

pub struct OtlpGrpcOutput {
    sink: BatchedSink<OtlpGrpcSinkPolicy>,
    metrics: Arc<OutputMetrics>,
}

const GRPC_PEER_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "endpoint",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
    },
    PropertySpec {
        name: "tls",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(crate::tls::TLS_CLIENT_BLOCK_PROPERTIES),
    },
];

const GRPC_PEERS_SCHEMA: &[PropertySpec] = &[PropertySpec {
    name: "peer",
    required: true,
    repeatable: true,
    exclusive_group: None,
    kind: PropertyValueKind::Block(GRPC_PEER_SCHEMA),
}];

const OTLP_GRPC_OUTPUT_SCHEMA: &[PropertySpec] = &[
    // Shorthand for the common single-collector case; mirrors the
    // syslog_tcp ergonomics. One of `peer` (single) or `peers` (multi)
    // is required; both at once is rejected by the schema layer.
    PropertySpec {
        name: "peer",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(GRPC_PEER_SCHEMA),
    },
    PropertySpec {
        name: "peers",
        required: false,
        repeatable: false,
        exclusive_group: Some("destination"),
        kind: PropertyValueKind::Block(GRPC_PEERS_SCHEMA),
    },
    PropertySpec {
        name: "batch_size",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "batch_timeout",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "batch_level",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["none", "resource", "scope"]),
    },
    PropertySpec {
        name: "headers",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::StringMap,
    },
    crate::queue::RETRY_PROPERTY_SPEC,
    crate::queue::QUEUE_PROPERTY_SPEC,
];

fn parse_peer(name: &str, peer_props: &[Property]) -> Result<GrpcPeer> {
    let endpoint = props::get_string(peer_props, "endpoint")
        .ok_or_else(|| anyhow!("output '{}': otlp_grpc peer requires 'endpoint'", name))?;

    let tls_block = props::get_block(peer_props, "tls");
    if tls_block.is_some() && !endpoint.to_ascii_lowercase().starts_with("https://") {
        // A `tls { ... }` block on a plaintext (http:// or
        // scheme-less) endpoint is almost always an operator
        // error: tonic only engages the TLS layer when the URI
        // scheme is https, so the configured CA / client identity
        // is silently dropped and the daemon ships gRPC in clear
        // text. Refuse at parse time so the misconfiguration is
        // visible. Mirrors the matching guard in `output otlp_http`.
        bail!(
            "output '{}': otlp_grpc peer endpoint '{}' uses a plaintext scheme but a tls {{ ... }} block was supplied — switch the endpoint to https:// or drop the tls block",
            name,
            endpoint
        );
    }
    let tls_cfg = tls_block
        .map(|block| {
            let cfg = crate::tls::ClientTlsConfig {
                ca_path: props::get_string(block, "ca"),
                cert_path: props::get_string(block, "cert"),
                key_path: props::get_string(block, "key"),
            };
            cfg.validate(&format!("output '{}'", name))?;
            Ok::<_, anyhow::Error>(cfg)
        })
        .transpose()?;

    let mut endpoint_builder = Endpoint::from_shared(endpoint.clone())
        .with_context(|| format!("output '{}': invalid gRPC endpoint '{}'", name, endpoint))?;

    let needs_tls = endpoint.starts_with("https://") || tls_cfg.is_some();
    if needs_tls {
        crate::tls::install_default_crypto_provider();
        let mut tls = ClientTlsConfig::new().with_native_roots();
        if let Some(cfg) = &tls_cfg {
            if let Some(ca_path) = &cfg.ca_path {
                let pem = std::fs::read(ca_path).with_context(|| {
                    format!("output '{}': cannot read CA cert {}", name, ca_path)
                })?;
                tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem));
            }
            if let (Some(cert_path), Some(key_path)) = (&cfg.cert_path, &cfg.key_path) {
                let cert_pem = std::fs::read(cert_path).with_context(|| {
                    format!("output '{}': cannot read client cert {}", name, cert_path)
                })?;
                let key_pem = std::fs::read(key_path).with_context(|| {
                    format!("output '{}': cannot read client key {}", name, key_path)
                })?;
                tls = tls.identity(tonic::transport::Identity::from_pem(cert_pem, key_pem));
            }
        }
        endpoint_builder = endpoint_builder
            .tls_config(tls)
            .with_context(|| format!("output '{}': failed to configure gRPC TLS", name))?;
    }
    let channel = endpoint_builder.connect_lazy();
    Ok(GrpcPeer { endpoint, channel })
}

impl Module for OtlpGrpcOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(OTLP_GRPC_OUTPUT_SCHEMA)
    }

    fn from_properties(
        name: &str,
        properties: &crate::dsl::module_props::ModuleProperties,
        ctx: &crate::modules::BuildContext,
    ) -> Result<Self> {
        let error_log = ctx.error_log.as_ref().map(Arc::clone);
        let properties = properties.user_properties();

        let batch_size = props::get_positive_int(properties, "batch_size")?.unwrap_or(1) as usize;
        let batch_timeout = match props::get_string(properties, "batch_timeout") {
            Some(s) => props::parse_duration(&s)?,
            None => Duration::from_secs(5),
        };
        let batch_level_str = props::get_string(properties, "batch_level")
            .or_else(|| props::get_ident(properties, "batch_level"))
            .unwrap_or_else(|| "none".to_string());
        let batch_level = BatchLevel::parse(&batch_level_str, name)?;

        let headers = props::get_string_map(properties, "headers");

        // Single-peer shorthand (`peer { endpoint ... }`) or multi-peer
        // (`peers { peer { ... } ... }`). The schema's exclusive_group
        // already forbids both at once, so we just probe in priority
        // order.
        let peers = if let Some(peer_block) = props::get_block(properties, "peer") {
            vec![parse_peer(name, peer_block)?]
        } else if let Some(peers_block) = props::get_block(properties, "peers") {
            iter_peers_block(
                peers_block,
                &format!("output '{}': peers", name),
                |peer_props| parse_peer(name, peer_props),
            )?
        } else {
            anyhow::bail!(
                "output '{}': otlp_grpc requires a 'peer {{ ... }}' or 'peers {{ peer {{ ... }} ... }}' block",
                name
            );
        };

        let rotation = RotatingPeers::new(peers.len());

        let retry_config = RetryConfig::from_output_properties(properties)?;

        let metrics = OutputMetrics::register(&ctx.metrics, name)?;
        let policy = OtlpGrpcSinkPolicy {
            peers,
            rotation,
            batch_level,
            headers,
        };
        // The shared skeleton spawns the flusher actor; see
        // `crate::modules::output::batched` for the actor / shutdown
        // lifecycle contract.
        let sink = BatchedSink::new(
            policy,
            name,
            batch_size,
            batch_timeout,
            retry_config,
            error_log,
            ctx.error_log_fallback,
            Arc::clone(&metrics),
            ctx.shutdown_signal.clone(),
        );

        Ok(Self { sink, metrics })
    }
}

impl HasMetrics for OtlpGrpcOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for OtlpGrpcOutput {
    /// Buffer-only consume; the sink's actor owns every send. See
    /// `BatchedSink::consume` for the hand-off rationale.
    async fn consume(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        self.sink.consume(event, ack).await
    }

    /// Drain-time per-event entry: buffer only; the post-loop
    /// `shutdown()` call drains it bounded. See
    /// `BatchedSink::consume_shutdown` for the shutdown contract.
    async fn consume_shutdown(&self, event: &Event, ack: QueueAckHandle) -> Result<()> {
        self.sink.consume_shutdown(event, ack).await
    }

    async fn shutdown(
        &self,
        _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<()> {
        self.sink.shutdown().await
    }

    async fn shutdown_wedged(
        &self,
        _error_log: Option<&Arc<crate::error_log::ErrorLogWriter>>,
    ) -> Result<()> {
        self.sink.shutdown_wedged().await
    }
}

#[async_trait::async_trait]
impl BatchSinkPolicy for OtlpGrpcSinkPolicy {
    type Payload = Bytes;
    type Prepared = ExportLogsServiceRequest;

    fn kind(&self) -> &'static str {
        "otlp_grpc output"
    }

    /// Render a single Event into its OTLP `ResourceLogs` proto bytes
    /// — just a refcount bump on `event.egress`. Infallible in
    /// practice; the `Result` keeps per-event DLQ routing available
    /// (the pipeline egress contract is validated in `prepare`).
    fn render(&self, event: &Event) -> Result<Bytes> {
        Ok(event.egress.clone())
    }

    /// Decode the drained per-event protos and merge per
    /// `batch_level` into one request. A decode failure (= pipeline
    /// egress is not valid ResourceLogs) is deterministic, so the
    /// skeleton routes the batch to DLQ without burning the retry
    /// budget.
    fn prepare(&self, drained: Vec<Bytes>) -> Result<ExportLogsServiceRequest> {
        decode_drained_to_request(drained, self.batch_level)
    }

    /// One send attempt against the next rotation candidate; see
    /// `RotatingPeers` for the selection + cooldown contract
    /// (cooldown is measured from failure time so a slow 30s
    /// GRPC_REQUEST_TIMEOUT failure cannot record an already-expired
    /// cooldown).
    async fn send(&self, req: &ExportLogsServiceRequest) -> Result<SendOutcome> {
        let idx = self.rotation.select().await;
        match send_once(&self.peers[idx], self, req).await {
            Ok(outcome) => {
                self.rotation.mark_success(idx).await;
                Ok(outcome)
            }
            Err(e) => {
                self.rotation.mark_failure(idx).await;
                Err(e)
            }
        }
    }
}

async fn send_once(
    peer: &GrpcPeer,
    policy: &OtlpGrpcSinkPolicy,
    req: &ExportLogsServiceRequest,
) -> Result<SendOutcome> {
    let mut client = LogsServiceClient::new(peer.channel.clone());
    let mut request = tonic::Request::new(req.clone());
    let metadata = request.metadata_mut();
    for (k, v) in &policy.headers {
        // Lower-case the metadata key per HTTP/2 / gRPC convention;
        // tonic enforces this and will refuse `Authorization` etc.
        let key_lc = k.to_ascii_lowercase();
        match (
            tonic::metadata::MetadataKey::<tonic::metadata::Ascii>::from_bytes(key_lc.as_bytes()),
            tonic::metadata::MetadataValue::try_from(v.as_str()),
        ) {
            (Ok(mk), Ok(mv)) => {
                metadata.insert(mk, mv);
            }
            _ => {
                // Never log the value: typical contents are bearer
                // tokens, API keys, or cookie material — the exact
                // secret an OTLP backend authenticates with. The key
                // alone is enough to diagnose a misconfiguration
                // without leaking the credential.
                tracing::warn!(
                    "otlp_grpc: skipping header with invalid name or value (key={:?}); value redacted",
                    k
                );
            }
        }
    }
    let response = tokio::time::timeout(GRPC_REQUEST_TIMEOUT, client.export(request))
        .await
        .map_err(|_| {
            anyhow!(
                "output otlp_grpc: export to {} timed out after {:?}",
                peer.endpoint,
                GRPC_REQUEST_TIMEOUT
            )
        })?
        .with_context(|| format!("output otlp_grpc: export to {} failed", peer.endpoint))?;
    // The receiver may report `partial_success.rejected_log_records`.
    // Logged here as a warning AND propagated to the caller via
    // `SendOutcome.rejected` so the flush path can split the batch's
    // events between `events_written` (accepted) and `events_failed`
    // (rejected by the server). Selective re-send of *only* the
    // rejected records is queued for a later release; the retry loop
    // now lives in the batched-sink flush path (`flush_events`), not
    // in this policy's `send`; it handles hard failures (connection
    // refused, 5xx, …) but not partial-success deltas, since the
    // rejected set is a strict subset of what already shipped.
    let inner_resp = response.into_inner();
    let rejected = inner_resp
        .partial_success
        .as_ref()
        .map(|p| p.rejected_log_records.max(0) as u64)
        .unwrap_or(0);
    if let Some(partial) = inner_resp.partial_success.as_ref()
        && partial.rejected_log_records > 0
    {
        tracing::warn!(
            "otlp_grpc: {} rejected {} log record(s){}",
            peer.endpoint,
            partial.rejected_log_records,
            if partial.error_message.is_empty() {
                String::new()
            } else {
                format!(" — {}", partial.error_message)
            }
        );
    }
    Ok(SendOutcome { rejected })
}

// Note: `verify false` is intentionally not a property on `otlp_grpc`.
// tonic does not expose an insecure-skip-verify knob the way reqwest
// does; users that need plaintext for development environments simply
// use an `http://` endpoint.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::event::Event;
    use opentelemetry_proto::tonic::collector::logs::v1::{
        ExportLogsServiceResponse,
        logs_service_server::{LogsService, LogsServiceServer},
    };
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ResourceLogs, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use prost::Message;
    use std::net::SocketAddr;
    use std::sync::atomic::Ordering;
    use tokio::sync::Mutex;

    fn mp(props: &[Property]) -> crate::dsl::module_props::ModuleProperties {
        crate::dsl::module_props::ModuleProperties::from_parts("otlp_grpc", props.to_vec())
    }

    fn prop_str(key: &str, val: &str) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            key_span: None,
            value: Expr::spanless(ExprKind::StringLit(val.to_string())),
            value_span: None,
        }
    }

    fn prop_int(key: &str, val: i64) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            key_span: None,
            value: Expr::spanless(ExprKind::IntLit(val)),
            value_span: None,
        }
    }

    fn peer_block(endpoint: &str) -> Property {
        Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![prop_str("endpoint", endpoint)],
        }
    }

    fn peers_block_with(peers: Vec<Property>) -> Property {
        Property::Block {
            key: "peers".into(),
            key_span: None,
            properties: peers,
        }
    }

    fn one_peer_props(endpoint: &str) -> Vec<Property> {
        vec![peers_block_with(vec![peer_block(endpoint)])]
    }

    #[test]
    fn requires_peer_or_peers_block() {
        let err = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&[]),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("'peer {") && err.to_string().contains("'peers {"),
            "unexpected: {err}"
        );
    }

    #[tokio::test]
    async fn accepts_single_peer_shorthand() {
        let props = vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![prop_str("endpoint", "http://x:4317")],
        }];
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.policy.peers.len(), 1);
        assert_eq!(output.sink.inner.policy.peers[0].endpoint, "http://x:4317");
    }

    #[test]
    fn rejects_peers_block_with_no_peer() {
        let props = vec![peers_block_with(vec![])];
        let err = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("at least one peer"));
    }

    #[tokio::test]
    async fn accepts_plain_http_endpoint() {
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&one_peer_props("http://localhost:4317")),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.policy.peers.len(), 1);
    }

    #[test]
    fn rejects_tls_block_on_plaintext_endpoint() {
        // `tls { ... }` on an `http://` endpoint is almost always an
        // operator error — tonic only engages TLS when the URI scheme
        // is https, so the configured CA / client identity would be
        // silently dropped and the daemon would ship gRPC in clear
        // text. Fail fast at parse time. Mirrors the matching guard
        // in `output otlp_http`.
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "http://collector.example.com:4317"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("ca", "/etc/ca.pem")],
                },
            ],
        }])];
        let err = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .unwrap();
        let msg = err.to_string();
        assert!(
            msg.contains("plaintext") && msg.contains("https://"),
            "unexpected: {msg}"
        );
    }

    #[tokio::test]
    async fn accepts_empty_tls_block_on_https_endpoint() {
        // Regression guard for the plaintext-rejection check: https://
        // endpoints must still accept a (here empty) tls block. Empty
        // block keeps the test off-disk; the scheme check runs first.
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "https://collector.example.com:4317"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![],
                },
            ],
        }])];
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.policy.peers.len(), 1);
    }

    #[tokio::test]
    async fn accepts_https_endpoint_with_native_tls() {
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&one_peer_props("https://collector.example.com:4317")),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.policy.peers.len(), 1);
    }

    #[tokio::test]
    async fn parses_multi_peer_block() {
        let props = vec![peers_block_with(vec![
            peer_block("http://a:4317"),
            peer_block("http://b:4317"),
        ])];
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.policy.peers.len(), 2);
        assert_eq!(output.sink.inner.policy.peers[0].endpoint, "http://a:4317");
    }

    #[test]
    fn rejects_tls_with_key_but_no_cert() {
        let props = vec![peers_block_with(vec![Property::Block {
            key: "peer".into(),
            key_span: None,
            properties: vec![
                prop_str("endpoint", "https://x:4317"),
                Property::Block {
                    key: "tls".into(),
                    key_span: None,
                    properties: vec![prop_str("key", "/k.pem")],
                },
            ],
        }])];
        let err = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("cert and key"));
    }

    #[tokio::test]
    async fn batch_level_default_is_none() {
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&one_peer_props("http://x")),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert!(matches!(
            output.sink.inner.policy.batch_level,
            BatchLevel::None
        ));
    }

    #[tokio::test]
    async fn batch_size_defaults_to_one() {
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&one_peer_props("http://x")),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.batch_size, 1);
    }

    #[tokio::test]
    async fn retry_block_overrides_defaults() {
        let mut props = one_peer_props("http://x");
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 2),
                prop_str("initial_wait", "100ms"),
            ],
        });
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        assert_eq!(output.sink.inner.retry.max_attempts, 2);
    }

    // ---- wire-level round-trip ----

    fn singleton_bytes(time_unix_nano: u64) -> Bytes {
        let rl = ResourceLogs {
            resource: Some(Resource {
                attributes: vec![],
                dropped_attributes_count: 0,
                ..Default::default()
            }),
            scope_logs: vec![ScopeLogs {
                scope: Some(InstrumentationScope {
                    name: "limpid-test".into(),
                    version: "0.5.0".into(),
                    attributes: vec![],
                    dropped_attributes_count: 0,
                }),
                log_records: vec![LogRecord {
                    time_unix_nano,
                    severity_number: 9,
                    severity_text: "INFO".into(),
                    ..Default::default()
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        };
        let mut buf = Vec::with_capacity(rl.encoded_len());
        rl.encode(&mut buf).unwrap();
        Bytes::from(buf)
    }

    fn event_with_egress(egress: Bytes) -> Event {
        let mut e = Event::new(egress.clone(), "127.0.0.1:0".parse::<SocketAddr>().unwrap());
        e.egress = egress;
        e
    }

    /// Test shim mirroring the queue consumer's `consume` call.
    /// Same shape as the http shim: synthesises a Result from the
    /// ack disposition (Delivered or no-disp → Ok, Recovered → Err).
    async fn consume(output: &OtlpGrpcOutput, ev: &Event) -> Result<()> {
        let (ack, mut rx) = QueueAckHandle::for_test();
        let _ = output.consume(ev, ack).await;
        match rx.try_recv() {
            Ok((_, crate::queue::AckDisposition::Delivered)) => Ok(()),
            Ok((_, crate::queue::AckDisposition::Recovered)) => {
                Err(anyhow::anyhow!("recovered to DLQ"))
            }
            Ok((_, crate::queue::AckDisposition::Dropped)) => Err(anyhow::anyhow!("dropped")),
            Err(_) => Ok(()),
        }
    }

    #[allow(dead_code)]
    async fn consume_and_wait_disposition(
        output: &OtlpGrpcOutput,
        ev: &Event,
        timeout: Duration,
    ) -> Result<crate::queue::AckDisposition> {
        let (ack, mut rx) = QueueAckHandle::for_test();
        output.consume(ev, ack).await?;
        match tokio::time::timeout(timeout, rx.recv()).await {
            Ok(Some((_, disp))) => Ok(disp),
            Ok(None) => Err(anyhow::anyhow!("ack channel closed unexpectedly")),
            Err(_) => Err(anyhow::anyhow!(
                "actor did not resolve ack within {:?}",
                timeout
            )),
        }
    }

    #[allow(dead_code)]
    async fn consume_with_handle(
        output: &OtlpGrpcOutput,
        ev: &Event,
    ) -> Result<
        tokio::sync::mpsc::UnboundedReceiver<(
            crate::queue::AckPosition,
            crate::queue::AckDisposition,
        )>,
    > {
        let (ack, rx) = QueueAckHandle::for_test();
        output.consume(ev, ack).await?;
        Ok(rx)
    }

    async fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        for _ in 0..50 {
            if let Some(v) = probe() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for receiver to record the request");
    }

    struct RecordingLogs {
        received: Arc<Mutex<Vec<ExportLogsServiceRequest>>>,
    }

    #[tonic::async_trait]
    impl LogsService for RecordingLogs {
        async fn export(
            &self,
            request: tonic::Request<ExportLogsServiceRequest>,
        ) -> std::result::Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status>
        {
            self.received.lock().await.push(request.into_inner());
            Ok(tonic::Response::new(ExportLogsServiceResponse {
                partial_success: None,
            }))
        }
    }

    #[tokio::test]
    async fn round_trip_grpc_to_recording_collector() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let received = Arc::new(Mutex::new(Vec::new()));
        let svc = RecordingLogs {
            received: Arc::clone(&received),
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 1));
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(
            &output,
            &event_with_egress(singleton_bytes(1_700_000_000_000_000_000)),
        )
        .await
        .unwrap();

        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() { None } else { Some(g.clone()) }
        };
        let got = wait_for(probe).await;
        server.abort();

        assert_eq!(got.len(), 1);
        let lr = &got[0].resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(lr.time_unix_nano, 1_700_000_000_000_000_000);
    }

    /// Test-only LogsService that ALWAYS reports `partial_success.
    /// rejected_log_records = rejected_count`. Used to pin the
    /// behaviour of `events_written` / `events_failed` when the
    /// receiver advertises a partial-success rejection.
    struct PartialSuccessLogs {
        rejected_per_call: i64,
    }

    #[tonic::async_trait]
    impl LogsService for PartialSuccessLogs {
        async fn export(
            &self,
            _request: tonic::Request<ExportLogsServiceRequest>,
        ) -> std::result::Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status>
        {
            Ok(tonic::Response::new(ExportLogsServiceResponse {
                partial_success: Some(
                    opentelemetry_proto::tonic::collector::logs::v1::ExportLogsPartialSuccess {
                        rejected_log_records: self.rejected_per_call,
                        error_message: "test rejection".into(),
                    },
                ),
            }))
        }
    }

    #[tokio::test]
    async fn partial_success_rejected_log_records_routes_to_events_failed() {
        // Regression guard: when the receiver returns 2xx-equivalent
        // (gRPC OK) with `partial_success.rejected_log_records = N`,
        // events_written must NOT cover the N rejected records.
        // Previously the entire batch was counted as written and
        // operators saw zero events_failed for server-side rejections,
        // making partial-success outages invisible on dashboards.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let svc = PartialSuccessLogs {
            rejected_per_call: 2,
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        // batch_size=3 so the single Rendered write triggers flush at
        // exactly 3 events; receiver rejects 2 of them.
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 3));
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();

        for i in 0..3 {
            let ev = event_with_egress(singleton_bytes(1_000_000_000 + i));
            // With batch_size=3 the per-event disposition isn't observable
            // via the test shim's freshly-allocated handle channel — the
            // first two events stay buffered (handle held by the output)
            // and the third reaches the batch threshold and wakes the
            // actor; the actor's per-event outcome can be Delivered OR
            // Recovered depending on the partial-success split order.
            // The metric counts (asserted below) are the contract under
            // test.
            let _ = consume(&output, &ev).await;
        }
        // Give the actor flush a moment.
        for _ in 0..50 {
            if output.metrics.events_written.load(Ordering::Relaxed)
                + output.metrics.events_failed.load(Ordering::Relaxed)
                >= 3
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        server.abort();

        // 3 events total: 2 rejected → events_failed, 1 accepted → events_written.
        let written = output.metrics.events_written.load(Ordering::Relaxed);
        let failed = output.metrics.events_failed.load(Ordering::Relaxed);
        assert_eq!(
            written, 1,
            "expected 1 written, got {written} (failed={failed})"
        );
        assert_eq!(
            failed, 2,
            "expected 2 failed, got {failed} (written={written})"
        );
    }

    #[tokio::test]
    async fn drop_aborts_idle_flusher_actor() {
        // `consume` buffers events below `batch_size`; the long-
        // lived flusher actor sleeps on `batch_timeout`. Drop must
        // signal cooperative shutdown then abort (last-resort,
        // sync Drop cannot `.await`) so test teardown doesn't leak
        // the spawned actor.
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 1024));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(&output, &event_with_egress(singleton_bytes(1)))
            .await
            .unwrap();
        let handle_before = output.sink.actor_handle.lock().await.is_some();
        assert!(handle_before, "flusher actor must be spawned");
        drop(output);
    }

    #[tokio::test(start_paused = true)]
    async fn export_timeout_fires_against_stalled_peer() {
        // A peer that accepts the TCP connection but never returns a
        // gRPC HEADERS frame must surface as a timeout failure within
        // GRPC_REQUEST_TIMEOUT (30 s). Without the
        // `tokio::time::timeout(GRPC_REQUEST_TIMEOUT, …)` wrapper a
        // single stalled collector would block the rotation forever.
        // The constant-value assertion in another test catches the
        // case where the constant gets renamed, but it would not catch
        // a regression where the wrapper itself was removed or
        // pointed at a different (e.g. much larger) duration. This
        // test exercises the firing path end-to-end.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        // Stalled "server": accept the TCP connection and hold the
        // socket open without ever sending HTTP/2 SETTINGS or a gRPC
        // response. tonic's client preface goes out, then it waits
        // forever for the server preface — exactly the case the
        // GRPC_REQUEST_TIMEOUT exists to bound.
        let stall = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                if let Ok((sock, _)) = listener.accept().await {
                    held.push(sock);
                }
            }
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 1));
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        let output = Arc::new(
            OtlpGrpcOutput::from_properties(
                "test",
                &mp(&props),
                &crate::modules::BuildContext::for_testing(),
            )
            .unwrap(),
        );

        let (ack, mut rx) = QueueAckHandle::for_test();
        output
            .consume(&event_with_egress(singleton_bytes(1)), ack)
            .await
            .unwrap();

        // Let the actor wake, take the batch, and reach the
        // GRPC_REQUEST_TIMEOUT-wrapped send await.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(GRPC_REQUEST_TIMEOUT + Duration::from_secs(1)).await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }

        let (_, disp) = rx
            .recv()
            .await
            .expect("ack must resolve, not drop unresolved");
        assert!(
            matches!(disp, crate::queue::AckDisposition::Recovered),
            "stalled peer must surface as Recovered, got {:?}",
            disp
        );
        let _ = output.shutdown(None).await;
        stall.abort();
    }

    #[tokio::test]
    async fn consume_event_buffers_below_batch_size() {
        // `consume` always buffers under `batch_size > 1`; the
        // long-lived flusher actor drains on `batch_timeout` or on
        // a threshold `flush_notify`. (An earlier version armed a
        // per-flush spawned timer task here — the old abort
        // surface. The actor is already spawned at construction.)
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 1024));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        consume(&output, &event_with_egress(singleton_bytes(1)))
            .await
            .expect("buffering a single event must succeed");
        let batch_len = output.sink.inner.batch.lock().await.len();
        assert_eq!(batch_len, 1, "event must sit in the buffer");
        let actor_spawned = output.sink.actor_handle.lock().await.is_some();
        assert!(
            actor_spawned,
            "the long-lived flusher actor must be available to drain on batch_timeout or notify"
        );
    }

    #[tokio::test]
    async fn shutdown_flushes_pending_batch_buffer() {
        // Regression mirror of `output http` / `otlp_http`: when
        // batch_size > 1 `consume()` parks the event + ack handle in
        // the buffer; the queue layer cannot advance its cursor until
        // the handle resolves at flush time. If shutdown happens
        // before the batch fills or before the actor's batch_timeout
        // wake, `shutdown()` must signal/join the actor and
        // final-drain any leftover buffer with one bounded send
        // attempt (or DLQ drain).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);

        let received = Arc::new(Mutex::new(Vec::new()));
        let svc = RecordingLogs {
            received: Arc::clone(&received),
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        // Large batch + long timer: only shutdown drains the buffer.
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();

        for ts in [1u64, 2u64] {
            consume(&output, &event_with_egress(singleton_bytes(ts)))
                .await
                .unwrap();
        }
        assert_eq!(
            output.sink.inner.batch.lock().await.len(),
            2,
            "writes must land in the buffer"
        );

        output.shutdown(None).await.unwrap();

        assert_eq!(
            output.sink.inner.batch.lock().await.len(),
            0,
            "shutdown() must drain the buffer"
        );

        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() { None } else { Some(g.clone()) }
        };
        let got = wait_for(probe).await;
        server.abort();

        assert_eq!(got.len(), 1, "shutdown flush must send exactly once");
        let record_count: usize = got[0]
            .resource_logs
            .iter()
            .flat_map(|rl| rl.scope_logs.iter())
            .map(|sl| sl.log_records.len())
            .sum();
        assert_eq!(
            record_count, 2,
            "shutdown send must carry both buffered records"
        );
    }

    #[tokio::test]
    async fn flush_failure_routes_batch_to_dlq_and_resolves_recovered() {
        // Mirror of the otlp_http test: when the per-flush retry
        // budget exhausts, every entry resolves Recovered and the
        // buffer is empty afterwards.
        let mut props = one_peer_props("http://127.0.0.1:1");
        props.push(prop_int("batch_size", 2));
        props.push(prop_str("batch_timeout", "30s"));
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();

        let (ack1, mut rx1) = QueueAckHandle::for_test();
        output
            .consume(&event_with_egress(singleton_bytes(1)), ack1)
            .await
            .unwrap();
        assert!(rx1.try_recv().is_err());
        let (ack2, mut rx2) = QueueAckHandle::for_test();
        output
            .consume(&event_with_egress(singleton_bytes(2)), ack2)
            .await
            .unwrap();
        assert!(matches!(
            rx1.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        assert!(matches!(
            rx2.recv().await,
            Some((_, crate::queue::AckDisposition::Recovered))
        ));
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Shutdown-flush recovery: shutdown-time buffer-loss recovery via `error_log`.
    // -----------------------------------------------------------------------

    fn shutdown_recovery_props(endpoint: &str) -> Vec<Property> {
        let mut props = one_peer_props(endpoint);
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        // Single attempt + minimal wait so the shutdown flush against
        // an unreachable peer completes quickly.
        props.push(Property::Block {
            key: "retry".into(),
            key_span: None,
            properties: vec![
                prop_int("max_attempts", 1),
                prop_str("initial_wait", "1ms"),
                prop_str("max_wait", "1ms"),
            ],
        });
        props
    }

    async fn buffer_two(output: &OtlpGrpcOutput) {
        for ts in [1u64, 2u64] {
            consume(output, &event_with_egress(singleton_bytes(ts)))
                .await
                .unwrap();
        }
        assert_eq!(output.sink.inner.batch.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn shutdown_failure_with_error_log_persists_buffer() {
        // The shutdown path drains via flush_events, which
        // DLQ-routes every entry as Recovered. The operator-
        // configured writer must hold both records.
        let props = shutdown_recovery_props("http://127.0.0.1:1");
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));
        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        let output = OtlpGrpcOutput::from_properties("myout", &mp(&props), &ctx).unwrap();
        buffer_two(&output).await;

        output.shutdown(Some(&writer)).await.unwrap();
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);

        let body = tokio::fs::read_to_string(&path).await.unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["schema_version"], 2);
            assert_eq!(v["kind"], "output");
            assert_eq!(v["output"]["name"], "myout");
            assert!(v["event"]["ingress"].is_string() || v["event"]["ingress"].is_object());
            assert!(
                v["event"]["egress"].is_string() || v["event"]["egress"].is_object(),
                "Output records must carry egress for inject-output replay"
            );
        }
    }

    #[tokio::test]
    async fn shutdown_failure_without_error_log_returns_ok() {
        // Shutdown is infallible.
        let props = shutdown_recovery_props("http://127.0.0.1:1");
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        buffer_two(&output).await;

        output.shutdown(None).await.expect("shutdown is infallible");
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn shutdown_success_does_not_touch_error_log() {
        // Healthy server: shutdown flush succeeds, the operator's
        // audit trail must stay empty even with `error_log` set.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        let received = Arc::new(Mutex::new(Vec::new()));
        let svc = RecordingLogs {
            received: Arc::clone(&received),
        };
        let server = tokio::spawn(async move {
            let _ = tonic::transport::Server::builder()
                .add_service(LogsServiceServer::new(svc))
                .serve_with_incoming(incoming)
                .await;
        });

        let endpoint = format!("http://{}", addr);
        let mut props = one_peer_props(&endpoint);
        props.push(prop_int("batch_size", 100));
        props.push(prop_str("batch_timeout", "30s"));
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        buffer_two(&output).await;

        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path.clone()));

        output.shutdown(Some(&writer)).await.unwrap();
        server.abort();
        assert!(!path.exists(), "DLQ must stay untouched on clean shutdown");
    }

    #[tokio::test]
    async fn shutdown_recovery_writer_failure_does_not_recurse() {
        // error_log writer itself fails on every record (parent dir
        // missing). Helper must warn + continue, never loop or panic.
        let props = shutdown_recovery_props("http://127.0.0.1:1");
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&props),
            &crate::modules::BuildContext::for_testing(),
        )
        .unwrap();
        buffer_two(&output).await;

        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(
            std::path::PathBuf::from("/nonexistent/limpid-grpc-test/errored.jsonl"),
        ));
        output.shutdown(Some(&writer)).await.unwrap();
        assert_eq!(output.sink.inner.batch.lock().await.len(), 0);
    }

    /// Constructor-time error_log injection — see the matching test
    /// in `output::http` for the rationale.
    #[tokio::test]
    async fn constructor_injects_error_log_into_inner() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("errored.jsonl");
        let writer = Arc::new(crate::error_log::ErrorLogWriter::new(path));

        let ctx = crate::modules::BuildContext {
            error_log: Some(Arc::clone(&writer)),
            ..crate::modules::BuildContext::for_testing()
        };
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&one_peer_props("http://127.0.0.1:1")),
            &ctx,
        )
        .unwrap();
        let stored = output
            .sink
            .inner
            .error_log
            .as_ref()
            .expect("error_log must be set");
        assert!(
            Arc::ptr_eq(stored, &writer),
            "constructor must store the exact Arc passed in"
        );
    }
}
