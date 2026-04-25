//! OTLP output: forwards Events to an OpenTelemetry collector / SaaS
//! backend via OTLP's three transports (HTTP/JSON, HTTP/protobuf,
//! gRPC).
//!
//! Each Event's `egress` is expected to be the singleton ResourceLogs
//! protobuf bytes produced by `otlp.encode_resourcelog_protobuf` —
//! this is the v0.5.0 OTLP hop contract (1 Resource + 1 Scope + 1
//! LogRecord per Event). Output buffers the per-Event ResourceLogs,
//! flushes on `batch_size` or `batch_timeout`, wraps the batch in an
//! `ExportLogsServiceRequest`, and ships it.
//!
//! ## Configuration
//!
//! ```text
//! def output otlp_out {
//!     type otlp
//!     endpoint "https://collector.example.com:4318"
//!     protocol "http_protobuf"   // http_json | http_protobuf | grpc
//!     batch_size 512
//!     batch_timeout "5s"
//!     headers {
//!         Authorization "Bearer ${env.OTLP_TOKEN}"
//!     }
//!     tls {
//!         ca "/etc/limpid/ca.crt"
//!     }
//! }
//! ```
//!
//! ### Endpoint conventions
//!
//! - **HTTP transports** point at the full OTLP/HTTP path (typically
//!   `:4318/v1/logs`). limpid does not append `/v1/logs`
//!   automatically; collectors that mount the receiver elsewhere
//!   (e.g. behind a path prefix) just work.
//! - **gRPC** points at the gRPC server URL (typically `:4317`); the
//!   service name (`opentelemetry.proto.collector.logs.v1.LogsService`)
//!   is implicit in the generated client. `https://` and `http://`
//!   schemes select TLS / plaintext respectively.
//!
//! ### `batch_level`
//!
//! Three levels, each producing semantically identical OTLP at the
//! receiver — they differ only in wire framing:
//!
//! - **`none`** (default): one ResourceLogs entry per buffered Event.
//!   Cheapest CPU, largest wire, suitable when batch_size = 1 or the
//!   collector tolerates redundancy.
//! - **`resource`**: Events sharing a Resource collapse into a single
//!   ResourceLogs entry; their ScopeLogs sit side-by-side under it.
//! - **`scope`**: as `resource` plus Events sharing a Scope inside the
//!   same Resource collapse into a single ScopeLogs whose
//!   `log_records[]` accumulates everything. Smallest wire, slightly
//!   higher CPU (Resource and Scope equality scans).
//!
//! All three modes are valid OTLP — the proto3 `repeated` field
//! guarantees concat-equals-merge at the receiver, so picking a level
//! is a compression / latency trade, not a correctness one.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::logs::v1::{
        ExportLogsServiceRequest, logs_service_client::LogsServiceClient,
    },
    common::v1::{InstrumentationScope, KeyValue},
    logs::v1::{ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use prost::Message;
use tokio::sync::Mutex;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};
use crate::queue::{BackoffStrategy, RetryConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BatchLevel {
    /// One ResourceLogs entry per buffered Event — pure concat, no
    /// equality scans. Cheapest CPU.
    None,
    /// Group buffered Events by their Resource and merge each group
    /// into a single ResourceLogs whose `scope_logs[]` accumulates the
    /// inputs.
    Resource,
    /// `Resource` plus an inner pass that groups by Scope inside each
    /// Resource, merging `log_records[]`. Smallest wire.
    Scope,
}

impl BatchLevel {
    fn parse(s: &str, output_name: &str) -> Result<Self> {
        match s {
            "none" => Ok(BatchLevel::None),
            "resource" => Ok(BatchLevel::Resource),
            "scope" => Ok(BatchLevel::Scope),
            other => bail!(
                "output '{}': unknown batch_level '{}' (expected none, resource, or scope)",
                output_name,
                other
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    HttpJson,
    HttpProtobuf,
    Grpc,
}

impl Protocol {
    fn parse(s: &str, output_name: &str) -> Result<Self> {
        match s {
            "http_json" => Ok(Protocol::HttpJson),
            "http_protobuf" => Ok(Protocol::HttpProtobuf),
            "grpc" => Ok(Protocol::Grpc),
            other => bail!(
                "output '{}': unknown protocol '{}' (expected http_json, http_protobuf, or grpc)",
                output_name,
                other
            ),
        }
    }

    fn content_type(self) -> Option<&'static str> {
        match self {
            Protocol::HttpJson => Some("application/json"),
            Protocol::HttpProtobuf => Some("application/x-protobuf"),
            // gRPC sets its own framing and trailer-encoded
            // content-type via tonic; the Inner.client field is unused
            // on this path.
            Protocol::Grpc => None,
        }
    }
}

/// Transport-specific client handles. HTTP transports share the
/// reqwest client; gRPC owns a lazy tonic Channel that connects on
/// first request and is reused thereafter.
enum Transport {
    Http(reqwest::Client),
    Grpc(Channel),
}

struct Inner {
    endpoint: String,
    protocol: Protocol,
    batch_level: BatchLevel,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    transport: Transport,
    /// Per-batch retry policy. The shared `RetryConfig` parser used by
    /// the file/tcp/http outputs reads `retry { max_attempts initial_wait
    /// max_wait backoff }` from the output's properties; we re-use it
    /// so every limpid output speaks the same retry vocabulary.
    ///
    /// Internal retry matters for the OTLP output specifically because
    /// it batches Events from multiple `write()` calls — without an
    /// internal retry, a single transient ship failure would lose the
    /// whole drained batch (the queue layer's per-event retry only
    /// re-pushes the most recent Event).
    retry_config: RetryConfig,
    /// Buffered per-Event singleton ResourceLogs proto bytes. Each
    /// entry is exactly what `otlp.encode_resourcelog_protobuf`
    /// produced; flush wraps them per `batch_level` into one
    /// ExportLogsServiceRequest.
    batch: Mutex<Vec<Bytes>>,
}

pub struct OtlpOutput {
    inner: Arc<Inner>,
    batch_size: usize,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    metrics: Arc<OutputMetrics>,
}

impl Module for OtlpOutput {
    fn from_properties(name: &str, properties: &[Property]) -> Result<Self> {
        let endpoint = props::get_string(properties, "endpoint")
            .ok_or_else(|| anyhow!("output '{}': otlp requires 'endpoint'", name))?;
        let protocol_str = props::get_string(properties, "protocol")
            .or_else(|| props::get_ident(properties, "protocol"))
            .unwrap_or_else(|| "http_protobuf".to_string());
        let protocol = Protocol::parse(&protocol_str, name)?;
        let batch_size = props::get_positive_int(properties, "batch_size")?.unwrap_or(1) as usize;
        let batch_timeout = match props::get_string(properties, "batch_timeout") {
            Some(s) => props::parse_duration(&s)?,
            None => Duration::from_secs(5),
        };
        let batch_level_str = props::get_string(properties, "batch_level")
            .or_else(|| props::get_ident(properties, "batch_level"))
            .unwrap_or_else(|| "none".to_string());
        let batch_level = BatchLevel::parse(&batch_level_str, name)?;

        let mut headers = Vec::new();
        if let Some(block) = props::get_block(properties, "headers") {
            for prop in block {
                if let Property::KeyValue {
                    key, value: expr, ..
                } = prop
                    && let Some(val) = match &expr.kind {
                        crate::dsl::ast::ExprKind::StringLit(s) => Some(s.clone()),
                        crate::dsl::ast::ExprKind::Ident(parts) => Some(parts.join(".")),
                        _ => None,
                    }
                {
                    headers.push((key.clone(), val));
                }
            }
        }

        // TLS / `verify` block parsing is shared across transports —
        // both reqwest and tonic accept the same on-disk PEM, so we
        // resolve once and apply per-transport below.
        let verify = props::get_ident(properties, "verify")
            .map(|s| s != "false")
            .unwrap_or(true);
        let ca_path = props::get_block(properties, "tls")
            .and_then(|block| props::get_string(block, "ca"));
        let ca_pem = ca_path
            .as_ref()
            .map(|p| {
                std::fs::read(p).with_context(|| {
                    format!("output '{}': cannot read CA cert {}", name, p)
                })
            })
            .transpose()?;

        let transport = match protocol {
            Protocol::HttpJson | Protocol::HttpProtobuf => {
                let mut builder = reqwest::Client::builder();
                if !verify {
                    builder = builder.danger_accept_invalid_certs(true);
                }
                if let Some(pem) = &ca_pem {
                    let cert = reqwest::Certificate::from_pem(pem).with_context(|| {
                        format!(
                            "output '{}': invalid CA cert PEM at {}",
                            name,
                            ca_path.as_deref().unwrap_or("<inline>")
                        )
                    })?;
                    builder = builder.add_root_certificate(cert);
                }
                let client = builder.build().with_context(|| {
                    format!("output '{}': failed to build HTTP client", name)
                })?;
                Transport::Http(client)
            }
            Protocol::Grpc => {
                let mut endpoint_builder = Endpoint::from_shared(endpoint.clone())
                    .with_context(|| format!("output '{}': invalid gRPC endpoint", name))?;
                if endpoint.starts_with("https://") || ca_pem.is_some() {
                    install_default_crypto_provider();
                    let mut tls = ClientTlsConfig::new().with_native_roots();
                    if let Some(pem) = &ca_pem {
                        tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem));
                    }
                    endpoint_builder = endpoint_builder.tls_config(tls).with_context(|| {
                        format!("output '{}': failed to configure gRPC TLS", name)
                    })?;
                }
                if !verify {
                    // tonic does not expose a "skip verify" knob the
                    // way reqwest does; users that need it for dev
                    // environments must run on plaintext (`http://`)
                    // until rustls-on-tonic gains the equivalent.
                    bail!(
                        "output '{}': `verify false` is not supported on gRPC transport (use `http://` for plaintext or set up a real CA for TLS)",
                        name
                    );
                }
                let channel = endpoint_builder.connect_lazy();
                Transport::Grpc(channel)
            }
        };

        let retry_config = RetryConfig::from_output_properties(properties)?;

        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                protocol,
                batch_level,
                headers,
                batch_timeout,
                transport,
                retry_config,
                batch: Mutex::new(Vec::new()),
            }),
            batch_size,
            flush_handle: Mutex::new(None),
            metrics: Arc::new(OutputMetrics::default()),
        })
    }
}

impl HasMetrics for OtlpOutput {
    type Stats = OutputMetrics;
    fn metrics(&self) -> Arc<OutputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[async_trait::async_trait]
impl Output for OtlpOutput {
    async fn write(&self, event: &Event) -> Result<()> {
        // Egress must be the singleton ResourceLogs proto bytes
        // produced by `otlp.encode_resourcelog_protobuf`. We do not
        // re-encode here — that's the process layer's job. If the user
        // wired the pipeline differently (e.g. forgot the encode), the
        // collector will see malformed wire and reject the batch.
        let proto = event.egress.clone();
        let mut batch = self.inner.batch.lock().await;
        batch.push(proto);
        let should_flush = batch.len() >= self.batch_size;
        drop(batch);

        if should_flush {
            self.flush().await?;
        } else {
            self.ensure_flush_timer().await;
        }
        Ok(())
    }
}

impl OtlpOutput {
    /// Drain the current batch, build an ExportLogsServiceRequest, and
    /// ship it. No-op if the buffer is empty.
    async fn flush(&self) -> Result<()> {
        let drained: Vec<Bytes> = {
            let mut batch = self.inner.batch.lock().await;
            std::mem::take(&mut *batch)
        };
        if drained.is_empty() {
            return Ok(());
        }
        let count = drained.len();
        let result = send_batch(&self.inner, drained).await;
        match result {
            Ok(()) => {
                self.metrics
                    .events_written
                    .fetch_add(count as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.metrics
                    .events_failed
                    .fetch_add(count as u64, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// Schedule (or refresh) a deferred flush so events do not sit in
    /// the buffer indefinitely when traffic is below `batch_size`.
    async fn ensure_flush_timer(&self) {
        let mut handle = self.flush_handle.lock().await;
        if let Some(h) = handle.as_ref()
            && !h.is_finished()
        {
            return;
        }

        let inner = Arc::clone(&self.inner);
        let metrics = Arc::clone(&self.metrics);
        let new_handle = tokio::spawn(async move {
            tokio::time::sleep(inner.batch_timeout).await;
            let drained: Vec<Bytes> = {
                let mut batch = inner.batch.lock().await;
                std::mem::take(&mut *batch)
            };
            if drained.is_empty() {
                return;
            }
            let count = drained.len();
            match send_batch(&inner, drained).await {
                Ok(()) => {
                    metrics
                        .events_written
                        .fetch_add(count as u64, Ordering::Relaxed);
                }
                Err(e) => {
                    tracing::warn!("otlp flush timer: send failed ({})", e);
                    metrics
                        .events_failed
                        .fetch_add(count as u64, Ordering::Relaxed);
                }
            }
        });
        *handle = Some(new_handle);
    }
}

/// rustls 0.23 requires explicit `CryptoProvider` selection — call
/// once before the first gRPC TLS endpoint is built. Uses aws-lc-rs
/// (the OpenSSL-style provider, present transitively via tonic's
/// tls-roots feature). Idempotent: subsequent calls observe the
/// already-installed provider and silently no-op.
fn install_default_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // `install_default` returns `Err` if a provider is already
        // installed (which happens when reqwest set one up first).
        // Either way we leave with a provider available, so swallow
        // the error.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Decode the per-Event ResourceLogs proto bytes, gather them into one
/// `ExportLogsServiceRequest` (merged per `batch_level`), and ship it
/// via the configured transport. Pulled out of `OtlpOutput` so the
/// size-flush and timeout-flush paths can share one implementation.
async fn send_batch(inner: &Inner, drained: Vec<Bytes>) -> Result<()> {
    let mut decoded: Vec<ResourceLogs> = Vec::with_capacity(drained.len());
    for proto in &drained {
        let rl = ResourceLogs::decode(&**proto).with_context(|| {
            "output otlp: pipeline egress is not a valid ResourceLogs proto (wire it through `otlp.encode_resourcelog_protobuf`)"
        })?;
        decoded.push(rl);
    }
    let req = match inner.batch_level {
        BatchLevel::None => ExportLogsServiceRequest {
            resource_logs: decoded,
        },
        BatchLevel::Resource => merge_by_resource(decoded),
        BatchLevel::Scope => merge_by_scope(decoded),
    };

    // Internal retry loop. The OTLP output batches Events from
    // multiple `write()` calls into one request; if a transient
    // failure happened *outside* this loop the whole drained batch
    // would be lost (the queue layer's per-event retry only re-pushes
    // the most recent Event). Retry inside `send_batch` keeps every
    // buffered Event in play until either ship succeeds or
    // `max_attempts` is exhausted.
    let cfg = &inner.retry_config;
    let max_attempts = cfg.max_attempts.max(1);
    let mut attempt = 0u32;
    let mut wait = cfg.initial_wait;
    loop {
        let result = match &inner.transport {
            Transport::Http(client) => send_http(inner, client, &req).await,
            Transport::Grpc(channel) => send_grpc(inner, channel, &req).await,
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 >= max_attempts => return Err(e),
            Err(e) => {
                attempt += 1;
                tracing::warn!(
                    "otlp output: ship attempt {}/{} failed: {} — retrying in {:?}",
                    attempt,
                    max_attempts,
                    e,
                    wait,
                );
                tokio::time::sleep(wait).await;
                if matches!(cfg.backoff, BackoffStrategy::Exponential) {
                    wait = (wait * 2).min(cfg.max_wait);
                }
            }
        }
    }
}

/// Group ResourceLogs by their Resource (attribute set +
/// dropped_attributes_count). Same-Resource entries collapse into one
/// ResourceLogs whose `scope_logs[]` is the concat of the inputs'
/// scope_logs. Order within each merged group preserves arrival order.
fn merge_by_resource(decoded: Vec<ResourceLogs>) -> ExportLogsServiceRequest {
    let mut out: Vec<ResourceLogs> = Vec::new();
    for rl in decoded {
        if let Some(idx) = out
            .iter()
            .position(|existing| resources_eq(&existing.resource, &rl.resource))
        {
            // Promote schema_url if the accumulator was empty and the
            // incoming entry has one (rare but spec-allowed).
            if out[idx].schema_url.is_empty() && !rl.schema_url.is_empty() {
                out[idx].schema_url = rl.schema_url;
            }
            out[idx].scope_logs.extend(rl.scope_logs);
        } else {
            out.push(rl);
        }
    }
    ExportLogsServiceRequest { resource_logs: out }
}

/// `merge_by_resource` plus an inner pass: within each Resource bucket,
/// ScopeLogs sharing an InstrumentationScope (name + version +
/// attributes + dropped_attributes_count) collapse into a single
/// ScopeLogs whose `log_records[]` is the concat of the inputs.
fn merge_by_scope(decoded: Vec<ResourceLogs>) -> ExportLogsServiceRequest {
    let mut req = merge_by_resource(decoded);
    for rl in &mut req.resource_logs {
        let scope_logs = std::mem::take(&mut rl.scope_logs);
        let mut grouped: Vec<ScopeLogs> = Vec::new();
        for sl in scope_logs {
            if let Some(idx) = grouped
                .iter()
                .position(|existing| scopes_eq(&existing.scope, &sl.scope))
            {
                if grouped[idx].schema_url.is_empty() && !sl.schema_url.is_empty() {
                    grouped[idx].schema_url = sl.schema_url;
                }
                grouped[idx].log_records.extend(sl.log_records);
            } else {
                grouped.push(sl);
            }
        }
        rl.scope_logs = grouped;
    }
    req
}

fn resources_eq(a: &Option<Resource>, b: &Option<Resource>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.dropped_attributes_count == y.dropped_attributes_count
                && attrs_eq(&x.attributes, &y.attributes)
        }
        _ => false,
    }
}

fn scopes_eq(a: &Option<InstrumentationScope>, b: &Option<InstrumentationScope>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => {
            x.name == y.name
                && x.version == y.version
                && x.dropped_attributes_count == y.dropped_attributes_count
                && attrs_eq(&x.attributes, &y.attributes)
        }
        _ => false,
    }
}

/// Attribute-set equality up to ordering. proto3 does not guarantee a
/// canonical attribute order on the wire, so we sort by `key` before
/// comparing — otherwise two semantically identical Resources with
/// attributes in different order would refuse to merge.
fn attrs_eq(a: &[KeyValue], b: &[KeyValue]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a_sorted: Vec<&KeyValue> = a.iter().collect();
    let mut b_sorted: Vec<&KeyValue> = b.iter().collect();
    a_sorted.sort_by(|x, y| x.key.cmp(&y.key));
    b_sorted.sort_by(|x, y| x.key.cmp(&y.key));
    a_sorted
        .iter()
        .zip(b_sorted.iter())
        .all(|(x, y)| x.key == y.key && x.value == y.value)
}

async fn send_http(
    inner: &Inner,
    client: &reqwest::Client,
    req: &ExportLogsServiceRequest,
) -> Result<()> {
    let body = match inner.protocol {
        Protocol::HttpProtobuf => {
            let mut buf = Vec::with_capacity(req.encoded_len());
            req.encode(&mut buf)
                .map_err(|e| anyhow!("output otlp: protobuf encode failed: {e}"))?;
            buf
        }
        Protocol::HttpJson => serde_json::to_vec(req)
            .map_err(|e| anyhow!("output otlp: JSON encode failed: {e}"))?,
        Protocol::Grpc => unreachable!("send_http called for gRPC transport"),
    };
    let content_type = inner
        .protocol
        .content_type()
        .expect("HTTP transport has a content type");
    let mut http_req = client
        .post(&inner.endpoint)
        .header("Content-Type", content_type)
        .body(body);
    for (k, v) in &inner.headers {
        http_req = http_req.header(k, v);
    }
    let resp = http_req
        .send()
        .await
        .with_context(|| format!("output otlp: POST {} failed", inner.endpoint))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "output otlp: {} returned HTTP {} — {}",
            inner.endpoint,
            status.as_u16(),
            text.chars().take(500).collect::<String>()
        );
    }
    Ok(())
}

async fn send_grpc(
    inner: &Inner,
    channel: &Channel,
    req: &ExportLogsServiceRequest,
) -> Result<()> {
    let mut client = LogsServiceClient::new(channel.clone());
    let mut request = tonic::Request::new(req.clone());
    let metadata = request.metadata_mut();
    for (k, v) in &inner.headers {
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
                tracing::warn!(
                    "otlp gRPC: skipping malformed header {:?}={:?}",
                    k,
                    v
                );
            }
        }
    }
    let response = client
        .export(request)
        .await
        .with_context(|| format!("output otlp: gRPC export to {} failed", inner.endpoint))?;
    // The receiver may report a `partial_success` with rejected
    // records. Surface as a warning — retry / drop policy is queued
    // for v0.5.x.
    let inner_resp = response.into_inner();
    if let Some(partial) = inner_resp.partial_success
        && partial.rejected_log_records > 0
    {
        tracing::warn!(
            "otlp gRPC: {} rejected {} log record(s){}",
            inner.endpoint,
            partial.rejected_log_records,
            if partial.error_message.is_empty() {
                String::new()
            } else {
                format!(" — {}", partial.error_message)
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind};

    fn prop_str(key: &str, val: &str) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            value: Expr::spanless(ExprKind::StringLit(val.to_string())),
            value_span: None,
        }
    }

    fn prop_int(key: &str, val: i64) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            value: Expr::spanless(ExprKind::IntLit(val)),
            value_span: None,
        }
    }

    #[test]
    fn requires_endpoint() {
        let err = OtlpOutput::from_properties("o", &[]).err().unwrap();
        assert!(err.to_string().contains("endpoint"));
    }

    #[tokio::test]
    async fn accepts_grpc_protocol() {
        let props = vec![
            prop_str("endpoint", "http://localhost:4317"),
            prop_str("protocol", "grpc"),
        ];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert!(matches!(output.inner.protocol, Protocol::Grpc));
        assert!(matches!(output.inner.transport, Transport::Grpc(_)));
    }

    #[tokio::test]
    async fn grpc_with_https_uses_tls_channel() {
        // The endpoint scheme drives TLS choice — `https://` triggers
        // ClientTlsConfig.with_native_roots(). We can't observe the
        // config from outside the channel, but construction not
        // erroring is the contract here.
        let props = vec![
            prop_str("endpoint", "https://collector.example.com:4317"),
            prop_str("protocol", "grpc"),
        ];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert!(matches!(output.inner.transport, Transport::Grpc(_)));
    }

    #[tokio::test]
    async fn grpc_rejects_verify_false() {
        // tonic does not expose insecure-skip-verify; surface a clear
        // error instead of silently accepting.
        let props = vec![
            prop_str("endpoint", "https://x:4317"),
            prop_str("protocol", "grpc"),
            Property::KeyValue {
                key: "verify".into(),
                value: Expr::spanless(ExprKind::Ident(vec!["false".into()])),
                value_span: None,
            },
        ];
        let err = OtlpOutput::from_properties("o", &props).err().unwrap();
        assert!(
            err.to_string().contains("verify false"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn rejects_unknown_protocol() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            prop_str("protocol", "carrier_pigeon"),
        ];
        let err = OtlpOutput::from_properties("o", &props).err().unwrap();
        assert!(err.to_string().contains("unknown"));
    }

    #[test]
    fn batch_level_default_is_none() {
        let output =
            OtlpOutput::from_properties("o", &[prop_str("endpoint", "http://x")]).unwrap();
        assert!(matches!(output.inner.batch_level, BatchLevel::None));
    }

    #[test]
    fn batch_level_accepts_resource_and_scope() {
        let r = OtlpOutput::from_properties(
            "o",
            &[
                prop_str("endpoint", "http://x"),
                prop_str("batch_level", "resource"),
            ],
        )
        .unwrap();
        assert!(matches!(r.inner.batch_level, BatchLevel::Resource));
        let s = OtlpOutput::from_properties(
            "o",
            &[
                prop_str("endpoint", "http://x"),
                prop_str("batch_level", "scope"),
            ],
        )
        .unwrap();
        assert!(matches!(s.inner.batch_level, BatchLevel::Scope));
    }

    #[test]
    fn rejects_unknown_batch_level() {
        let err = OtlpOutput::from_properties(
            "o",
            &[
                prop_str("endpoint", "http://x"),
                prop_str("batch_level", "logrecord"),
            ],
        )
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("unknown batch_level"),
            "unexpected: {err}"
        );
    }

    // ---- merge logic unit tests --------------------------------------

    fn make_resource(svc: &str) -> Option<Resource> {
        Some(Resource {
            attributes: vec![KeyValue {
                key: "service.name".into(),
                value: Some(opentelemetry_proto::tonic::common::v1::AnyValue {
                    value: Some(
                        opentelemetry_proto::tonic::common::v1::any_value::Value::StringValue(
                            svc.into(),
                        ),
                    ),
                }),
            }],
            dropped_attributes_count: 0,
        })
    }

    fn make_scope(name: &str) -> Option<InstrumentationScope> {
        Some(InstrumentationScope {
            name: name.into(),
            version: "0".into(),
            attributes: vec![],
            dropped_attributes_count: 0,
        })
    }

    fn make_record(t: u64) -> opentelemetry_proto::tonic::logs::v1::LogRecord {
        opentelemetry_proto::tonic::logs::v1::LogRecord {
            time_unix_nano: t,
            ..Default::default()
        }
    }

    /// One singleton per Event — the shape every `otlp.encode_*`
    /// caller produces.
    fn singleton(svc: &str, scope: &str, t: u64) -> ResourceLogs {
        ResourceLogs {
            resource: make_resource(svc),
            scope_logs: vec![ScopeLogs {
                scope: make_scope(scope),
                log_records: vec![make_record(t)],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }
    }

    #[test]
    fn merge_by_resource_collapses_same_resource() {
        // Two events on the same Resource but different Scopes:
        // resource-level merge keeps one ResourceLogs entry whose
        // scope_logs[] holds both scopes.
        let input = vec![singleton("svc-a", "scope-1", 1), singleton("svc-a", "scope-2", 2)];
        let req = merge_by_resource(input);
        assert_eq!(req.resource_logs.len(), 1);
        assert_eq!(req.resource_logs[0].scope_logs.len(), 2);
    }

    #[test]
    fn merge_by_resource_keeps_distinct_resources_separate() {
        let input = vec![singleton("svc-a", "scope-1", 1), singleton("svc-b", "scope-1", 2)];
        let req = merge_by_resource(input);
        assert_eq!(req.resource_logs.len(), 2);
    }

    #[test]
    fn merge_by_scope_collapses_same_resource_and_scope() {
        // Two events on the same Resource AND same Scope:
        // scope-level merge keeps one ScopeLogs whose log_records[]
        // holds both records.
        let input = vec![singleton("svc-a", "scope-1", 1), singleton("svc-a", "scope-1", 2)];
        let req = merge_by_scope(input);
        assert_eq!(req.resource_logs.len(), 1);
        assert_eq!(req.resource_logs[0].scope_logs.len(), 1);
        assert_eq!(req.resource_logs[0].scope_logs[0].log_records.len(), 2);
        let times: Vec<u64> = req.resource_logs[0].scope_logs[0]
            .log_records
            .iter()
            .map(|lr| lr.time_unix_nano)
            .collect();
        assert_eq!(times, vec![1, 2]);
    }

    #[test]
    fn merge_by_scope_handles_three_levels() {
        // Two Resources; under each, two Scopes; under each scope,
        // two records. After scope-level merge: 2 Resources × 2 Scopes
        // × 2 records, identical event count, minimum framing.
        let input = vec![
            singleton("svc-a", "scope-1", 1),
            singleton("svc-a", "scope-1", 2),
            singleton("svc-a", "scope-2", 3),
            singleton("svc-a", "scope-2", 4),
            singleton("svc-b", "scope-1", 5),
            singleton("svc-b", "scope-1", 6),
            singleton("svc-b", "scope-2", 7),
            singleton("svc-b", "scope-2", 8),
        ];
        let req = merge_by_scope(input);
        assert_eq!(req.resource_logs.len(), 2);
        for rl in &req.resource_logs {
            assert_eq!(rl.scope_logs.len(), 2);
            for sl in &rl.scope_logs {
                assert_eq!(sl.log_records.len(), 2);
            }
        }
        // No record was lost.
        let total_records: usize = req
            .resource_logs
            .iter()
            .flat_map(|rl| rl.scope_logs.iter())
            .map(|sl| sl.log_records.len())
            .sum();
        assert_eq!(total_records, 8);
    }

    #[test]
    fn attrs_eq_is_order_insensitive() {
        let a = vec![
            KeyValue {
                key: "k1".into(),
                value: None,
            },
            KeyValue {
                key: "k2".into(),
                value: None,
            },
        ];
        let b = vec![
            KeyValue {
                key: "k2".into(),
                value: None,
            },
            KeyValue {
                key: "k1".into(),
                value: None,
            },
        ];
        assert!(attrs_eq(&a, &b));
    }

    #[test]
    fn defaults_protocol_to_http_protobuf() {
        let props = vec![prop_str("endpoint", "http://x")];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert!(matches!(output.inner.protocol, Protocol::HttpProtobuf));
    }

    #[test]
    fn batch_size_defaults_to_one() {
        let props = vec![prop_str("endpoint", "http://x")];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert_eq!(output.batch_size, 1);
    }

    #[test]
    fn batch_size_explicit() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            prop_int("batch_size", 64),
        ];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert_eq!(output.batch_size, 64);
    }

    #[test]
    fn retry_config_defaults_match_shared_default() {
        let output = OtlpOutput::from_properties(
            "o",
            &[prop_str("endpoint", "http://x")],
        )
        .unwrap();
        let default = RetryConfig::default();
        assert_eq!(output.inner.retry_config.max_attempts, default.max_attempts);
        assert_eq!(
            output.inner.retry_config.initial_wait,
            default.initial_wait
        );
    }

    #[test]
    fn retry_block_overrides_defaults() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            Property::Block {
                key: "retry".into(),
                properties: vec![
                    prop_int("max_attempts", 2),
                    prop_str("initial_wait", "100ms"),
                    prop_str("max_wait", "500ms"),
                ],
            },
        ];
        let output = OtlpOutput::from_properties("o", &props).unwrap();
        assert_eq!(output.inner.retry_config.max_attempts, 2);
        assert_eq!(
            output.inner.retry_config.initial_wait,
            Duration::from_millis(100)
        );
        assert_eq!(
            output.inner.retry_config.max_wait,
            Duration::from_millis(500)
        );
    }

    // ------------------------------------------------------------------------
    // Wire-level round-trip tests: spin up a real receiver, point a real
    // OtlpOutput at it, write one Event, read what hit the wire. These
    // catch content-type / framing / metadata bugs that the per-piece
    // unit tests skip over because they short-circuit before the socket.
    // ------------------------------------------------------------------------

    use opentelemetry_proto::tonic::collector::logs::v1::{
        ExportLogsServiceResponse,
        logs_service_server::{LogsService, LogsServiceServer},
    };
    use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
    use opentelemetry_proto::tonic::logs::v1::{LogRecord, ScopeLogs};
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use std::net::SocketAddr;

    /// Build the singleton ResourceLogs proto bytes that
    /// `otlp.encode_resourcelog_protobuf` would have produced for the
    /// test event.
    fn singleton_bytes(time_unix_nano: u64) -> Bytes {
        let rl = ResourceLogs {
            resource: Some(Resource {
                attributes: vec![],
                dropped_attributes_count: 0,
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
        let mut e = Event::new(
            egress.clone(),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        );
        e.egress = egress;
        e
    }

    /// Wait up to `tries × 20ms` for `predicate` to become true.
    /// Returns the matched value or panics on timeout. Lets us watch
    /// the receiver without sleeping a fixed worst-case duration.
    async fn wait_for<T>(mut probe: impl FnMut() -> Option<T>) -> T {
        for _ in 0..50 {
            if let Some(v) = probe() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timeout waiting for receiver to record the request");
    }

    // --- gRPC ----

    struct RecordingLogs {
        received: Arc<Mutex<Vec<ExportLogsServiceRequest>>>,
    }

    #[tonic::async_trait]
    impl LogsService for RecordingLogs {
        async fn export(
            &self,
            request: tonic::Request<ExportLogsServiceRequest>,
        ) -> std::result::Result<tonic::Response<ExportLogsServiceResponse>, tonic::Status> {
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
        let output = OtlpOutput::from_properties(
            "test",
            &[
                prop_str("endpoint", &endpoint),
                prop_str("protocol", "grpc"),
                prop_int("batch_size", 1),
            ],
        )
        .unwrap();
        output
            .write(&event_with_egress(singleton_bytes(1_700_000_000_000_000_000)))
            .await
            .unwrap();

        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() {
                None
            } else {
                Some(g.clone())
            }
        };
        let got = wait_for(probe).await;
        server.abort();

        assert_eq!(got.len(), 1);
        let lr = &got[0].resource_logs[0].scope_logs[0].log_records[0];
        assert_eq!(lr.time_unix_nano, 1_700_000_000_000_000_000);
        assert_eq!(
            got[0].resource_logs[0].scope_logs[0]
                .scope
                .as_ref()
                .unwrap()
                .name,
            "limpid-test"
        );
    }

    // --- HTTP/protobuf ----

    /// Spin up a tiny axum POST handler that decodes the request body
    /// per content-type and stashes the result. Shared by the
    /// http_protobuf and http_json round-trip tests because the wire
    /// is the only thing that differs.
    async fn run_http_collector(
        protocol: &'static str,
    ) -> (SocketAddr, Arc<Mutex<Vec<ExportLogsServiceRequest>>>, tokio::task::JoinHandle<()>)
    {
        use axum::{
            Router,
            extract::State,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
            routing::post,
        };

        #[derive(Clone)]
        struct AppState {
            received: Arc<Mutex<Vec<ExportLogsServiceRequest>>>,
            protocol: &'static str,
        }

        async fn handle(
            State(state): State<AppState>,
            headers: HeaderMap,
            body: axum::body::Bytes,
        ) -> impl IntoResponse {
            let ct = headers
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let req: ExportLogsServiceRequest = match state.protocol {
                "http_protobuf" => {
                    if !ct.starts_with("application/x-protobuf") {
                        return (
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            format!("expected protobuf, got {ct:?}"),
                        )
                            .into_response();
                    }
                    match ExportLogsServiceRequest::decode(&*body) {
                        Ok(r) => r,
                        Err(e) => {
                            return (StatusCode::BAD_REQUEST, format!("decode: {e}"))
                                .into_response();
                        }
                    }
                }
                "http_json" => {
                    if !ct.starts_with("application/json") {
                        return (
                            StatusCode::UNSUPPORTED_MEDIA_TYPE,
                            format!("expected json, got {ct:?}"),
                        )
                            .into_response();
                    }
                    match serde_json::from_slice(&body) {
                        Ok(r) => r,
                        Err(e) => {
                            return (StatusCode::BAD_REQUEST, format!("json: {e}"))
                                .into_response();
                        }
                    }
                }
                _ => unreachable!("test-only enumeration"),
            };
            state.received.lock().await.push(req);
            (StatusCode::OK, "").into_response()
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new().route("/v1/logs", post(handle)).with_state(AppState {
            received: Arc::clone(&received),
            protocol,
        });
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, received, handle)
    }

    /// Functional retry — spin up a server that returns 503 for the
    /// first N attempts, then 200. The output's internal retry loop
    /// must exhaust the failures and ultimately deliver the request.
    /// This proves the batched-Event preservation property: a single
    /// transient failure does not silently lose the buffered batch.
    #[tokio::test]
    async fn http_output_retries_until_success() {
        use axum::{
            Router,
            extract::State,
            http::StatusCode,
            response::IntoResponse,
            routing::post,
        };
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        #[derive(Clone)]
        struct AppState {
            attempts: Arc<AtomicUsize>,
            fail_until: usize,
        }

        async fn handle(State(state): State<AppState>, _body: axum::body::Bytes) -> impl IntoResponse {
            let n = state.attempts.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            if n <= state.fail_until {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            }
        }

        let attempts = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/v1/logs", post(handle)).with_state(AppState {
            attempts: Arc::clone(&attempts),
            fail_until: 2, // first two requests get 503, third succeeds
        });
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = format!("http://{}/v1/logs", addr);
        let output = OtlpOutput::from_properties(
            "test",
            &[
                prop_str("endpoint", &endpoint),
                prop_str("protocol", "http_protobuf"),
                prop_int("batch_size", 1),
                Property::Block {
                    key: "retry".into(),
                    properties: vec![
                        prop_int("max_attempts", 5),
                        prop_str("initial_wait", "10ms"),
                        prop_str("max_wait", "50ms"),
                    ],
                },
            ],
        )
        .unwrap();

        // The first ship 503s twice then succeeds; the call should
        // return Ok overall thanks to the retry loop.
        output
            .write(&event_with_egress(singleton_bytes(123)))
            .await
            .unwrap();
        // 2 failures + 1 success = 3 attempts.
        assert_eq!(attempts.load(AtomicOrdering::SeqCst), 3);
        server.abort();
    }

    /// `max_attempts=N` with N consecutive failures bubbles the error
    /// up — the queue layer can then route the event to its
    /// `secondary` output or drop per its own policy.
    #[tokio::test]
    async fn http_output_gives_up_after_max_attempts() {
        use axum::{
            Router,
            http::StatusCode,
            response::IntoResponse,
            routing::post,
        };

        async fn always_fail(_body: axum::body::Bytes) -> impl IntoResponse {
            StatusCode::SERVICE_UNAVAILABLE
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/v1/logs", post(always_fail));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = format!("http://{}/v1/logs", addr);
        let output = OtlpOutput::from_properties(
            "test",
            &[
                prop_str("endpoint", &endpoint),
                prop_str("protocol", "http_protobuf"),
                prop_int("batch_size", 1),
                Property::Block {
                    key: "retry".into(),
                    properties: vec![
                        prop_int("max_attempts", 3),
                        prop_str("initial_wait", "10ms"),
                        prop_str("max_wait", "20ms"),
                    ],
                },
            ],
        )
        .unwrap();
        let err = output
            .write(&event_with_egress(singleton_bytes(456)))
            .await
            .err()
            .expect("send must fail after retries exhausted");
        assert!(
            err.to_string().contains("503") || err.to_string().contains("HTTP"),
            "unexpected error after retry exhaustion: {err}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn round_trip_http_protobuf_to_axum_handler() {
        let (addr, received, server) = run_http_collector("http_protobuf").await;
        let endpoint = format!("http://{}/v1/logs", addr);
        let output = OtlpOutput::from_properties(
            "test",
            &[
                prop_str("endpoint", &endpoint),
                prop_str("protocol", "http_protobuf"),
                prop_int("batch_size", 1),
            ],
        )
        .unwrap();
        output
            .write(&event_with_egress(singleton_bytes(123)))
            .await
            .unwrap();
        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() {
                None
            } else {
                Some(g.clone())
            }
        };
        let got = wait_for(probe).await;
        server.abort();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].resource_logs[0].scope_logs[0].log_records[0].time_unix_nano,
            123
        );
    }

    #[tokio::test]
    async fn round_trip_http_json_to_axum_handler() {
        let (addr, received, server) = run_http_collector("http_json").await;
        let endpoint = format!("http://{}/v1/logs", addr);
        let output = OtlpOutput::from_properties(
            "test",
            &[
                prop_str("endpoint", &endpoint),
                prop_str("protocol", "http_json"),
                prop_int("batch_size", 1),
            ],
        )
        .unwrap();
        output
            .write(&event_with_egress(singleton_bytes(456)))
            .await
            .unwrap();
        let probe = || {
            let g = received.try_lock().ok()?;
            if g.is_empty() {
                None
            } else {
                Some(g.clone())
            }
        };
        let got = wait_for(probe).await;
        server.abort();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].resource_logs[0].scope_logs[0].log_records[0].time_unix_nano,
            456
        );
    }
}
