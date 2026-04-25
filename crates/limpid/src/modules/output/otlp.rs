//! OTLP output: forwards Events to an OpenTelemetry collector / SaaS
//! backend via OTLP's three transports (HTTP/JSON, HTTP/protobuf,
//! gRPC).
//!
//! Each Event's `egress` is expected to be the singleton ResourceLogs
//! protobuf bytes produced by `otlp.encode_resourcelog_protobuf` —
//! this is the v0.5.0 hop contract for OTLP (see
//! `_DESIGN_V050_OTLP.md` §4.2). Output buffers the per-Event
//! ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the
//! batch in an `ExportLogsServiceRequest`, and ships it.
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
//! v0.5.0 only implements `none` (pure concat). The proto3 `repeated`
//! field guarantees that concatenating wire bytes is equivalent to a
//! merged message at the receiver, so a `none` batch is semantically
//! valid OTLP — what changes with `resource` / `scope` levels is wire
//! efficiency (fewer ResourceLogs / ScopeLogs entries), not the data
//! itself. The merge logic is queued for v0.5.x; see the design memo
//! §4.5.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::logs::v1::{
        ExportLogsServiceRequest, logs_service_client::LogsServiceClient,
    },
    logs::v1::ResourceLogs,
};
use prost::Message;
use tokio::sync::Mutex;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output};

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
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    transport: Transport,
    /// Buffered per-Event singleton ResourceLogs proto bytes. Each
    /// entry is exactly what `otlp.encode_resourcelog_protobuf`
    /// produced; flush concatenates them into one
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
        let batch_level = props::get_string(properties, "batch_level")
            .or_else(|| props::get_ident(properties, "batch_level"))
            .unwrap_or_else(|| "none".to_string());
        if batch_level != "none" {
            bail!(
                "output '{}': batch_level '{}' is not implemented in v0.5.0 (only 'none' supported)",
                name,
                batch_level
            );
        }

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

        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                protocol,
                headers,
                batch_timeout,
                transport,
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
/// `ExportLogsServiceRequest`, and ship it via the configured
/// transport. Pulled out of `OtlpOutput` so the size-flush and
/// timeout-flush paths can share one implementation.
async fn send_batch(inner: &Inner, drained: Vec<Bytes>) -> Result<()> {
    let mut req = ExportLogsServiceRequest::default();
    req.resource_logs.reserve(drained.len());
    for proto in &drained {
        let rl = ResourceLogs::decode(&**proto).with_context(|| {
            "output otlp: pipeline egress is not a valid ResourceLogs proto (wire it through `otlp.encode_resourcelog_protobuf`)"
        })?;
        req.resource_logs.push(rl);
    }
    match &inner.transport {
        Transport::Http(client) => send_http(inner, client, &req).await,
        Transport::Grpc(channel) => send_grpc(inner, channel, req).await,
    }
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
    req: ExportLogsServiceRequest,
) -> Result<()> {
    let mut client = LogsServiceClient::new(channel.clone());
    let mut request = tonic::Request::new(req);
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
    // for v0.5.x (see design memo §8 open issue #1 / #2).
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
    fn rejects_unsupported_batch_level() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            prop_str("batch_level", "scope"),
        ];
        let err = OtlpOutput::from_properties("o", &props).err().unwrap();
        assert!(err.to_string().contains("not implemented"));
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
}
