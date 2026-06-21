//! OTLP/gRPC output: forwards Events to an OpenTelemetry collector /
//! SaaS backend via OTLP over gRPC.
//!
//! ```text
//! def output otlp_out {
//!     type otlp_grpc
//!     endpoint "https://collector.example.com:4317"
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
//! Point at the gRPC server URL (typically `:4317`). The service name
//! (`opentelemetry.proto.collector.logs.v1.LogsService`) is implicit
//! in the generated client. `https://` and `http://` schemes select
//! TLS / plaintext respectively. Headers translate to gRPC metadata.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, logs_service_client::LogsServiceClient,
};
use tokio::sync::Mutex;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use crate::dsl::arena::EventArena;
use crate::dsl::props;
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::event::BorrowedEvent;
use crate::metrics::OutputMetrics;
use crate::modules::{HasMetrics, Module, Output, RenderedPayload};
use crate::queue::{BackoffStrategy, RetryConfig};

use super::{BatchLevel, OTLP_RETRY_BLOCK_PROPERTIES, OtlpPayload, decode_drained_to_request};

struct Inner {
    endpoint: String,
    batch_level: BatchLevel,
    headers: Vec<(String, String)>,
    batch_timeout: Duration,
    channel: Channel,
    /// Per-batch retry policy — see [`super::http`] for the
    /// rationale.
    retry_config: RetryConfig,
    /// Buffered per-Event singleton ResourceLogs proto bytes.
    batch: Mutex<Vec<Bytes>>,
}

pub struct OtlpGrpcOutput {
    inner: Arc<Inner>,
    batch_size: usize,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    metrics: Arc<OutputMetrics>,
}

const OTLP_GRPC_OUTPUT_SCHEMA: &[PropertySpec] = &[
    PropertySpec {
        name: "endpoint",
        required: true,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::String,
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
    PropertySpec {
        name: "tls",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(crate::tls::TLS_CLIENT_BLOCK_PROPERTIES),
    },
    PropertySpec {
        name: "retry",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Block(OTLP_RETRY_BLOCK_PROPERTIES),
    },
    crate::queue::QUEUE_PROPERTY_SPEC,
];

impl Module for OtlpGrpcOutput {
    fn property_schema() -> Option<&'static [PropertySpec]> {
        Some(OTLP_GRPC_OUTPUT_SCHEMA)
    }

    fn from_properties(name: &str, properties: &crate::modules::ModuleProperties) -> Result<Self> {
        let properties = properties.user_properties();
        let endpoint = props::get_string(properties, "endpoint")
            .ok_or_else(|| anyhow!("output '{}': otlp_grpc requires 'endpoint'", name))?;
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

        let ca_path =
            props::get_block(properties, "tls").and_then(|block| props::get_string(block, "ca"));
        let ca_pem = ca_path
            .as_ref()
            .map(|p| {
                std::fs::read(p)
                    .with_context(|| format!("output '{}': cannot read CA cert {}", name, p))
            })
            .transpose()?;

        let mut endpoint_builder = Endpoint::from_shared(endpoint.clone())
            .with_context(|| format!("output '{}': invalid gRPC endpoint", name))?;
        if endpoint.starts_with("https://") || ca_pem.is_some() {
            crate::tls::install_default_crypto_provider();
            let mut tls = ClientTlsConfig::new().with_native_roots();
            if let Some(pem) = &ca_pem {
                tls = tls.ca_certificate(tonic::transport::Certificate::from_pem(pem));
            }
            endpoint_builder = endpoint_builder
                .tls_config(tls)
                .with_context(|| format!("output '{}': failed to configure gRPC TLS", name))?;
        }
        let channel = endpoint_builder.connect_lazy();

        let retry_config = RetryConfig::from_output_properties(properties)?;

        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                batch_level,
                headers,
                batch_timeout,
                channel,
                retry_config,
                batch: Mutex::new(Vec::new()),
            }),
            batch_size,
            flush_handle: Mutex::new(None),
            metrics: Arc::new(OutputMetrics::default()),
        })
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
    fn render(
        &self,
        event: &BorrowedEvent<'_>,
        _arena: &EventArena<'_>,
    ) -> Result<RenderedPayload> {
        Ok(RenderedPayload::new(OtlpPayload {
            egress: event.egress.clone(),
        }))
    }

    async fn write(&self, payload: RenderedPayload) -> Result<()> {
        let payload: OtlpPayload = payload.downcast()?;
        let proto = payload.egress;
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

impl OtlpGrpcOutput {
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
                    tracing::warn!("otlp_grpc flush timer: send failed ({})", e);
                    metrics
                        .events_failed
                        .fetch_add(count as u64, Ordering::Relaxed);
                }
            }
        });
        *handle = Some(new_handle);
    }
}

impl Drop for OtlpGrpcOutput {
    fn drop(&mut self) {
        if let Some(h) = self.flush_handle.get_mut().take() {
            h.abort();
        }
        if let Ok(buf) = self.inner.batch.try_lock()
            && !buf.is_empty()
        {
            tracing::warn!(
                "otlp_grpc output: {} events in buffer at shutdown (will be re-delivered from queue)",
                buf.len()
            );
        }
    }
}

async fn send_batch(inner: &Inner, drained: Vec<Bytes>) -> Result<()> {
    let req = decode_drained_to_request(drained, inner.batch_level)?;

    let cfg = &inner.retry_config;
    let max_attempts = cfg.max_attempts.max(1);
    let mut attempt = 0u32;
    let mut wait = cfg.initial_wait;
    loop {
        let result = send_once(inner, &req).await;
        match result {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 >= max_attempts => return Err(e),
            Err(e) => {
                attempt += 1;
                tracing::warn!(
                    "otlp_grpc output: ship attempt {}/{} failed: {} — retrying in {:?}",
                    attempt,
                    max_attempts,
                    e,
                    wait,
                );
                tokio::time::sleep(wait).await;
                if matches!(cfg.backoff, BackoffStrategy::Exponential) {
                    wait = wait.saturating_mul(2).min(cfg.max_wait);
                }
            }
        }
    }
}

async fn send_once(inner: &Inner, req: &ExportLogsServiceRequest) -> Result<()> {
    let mut client = LogsServiceClient::new(inner.channel.clone());
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
    let response = client
        .export(request)
        .await
        .with_context(|| format!("output otlp_grpc: export to {} failed", inner.endpoint))?;
    // The receiver may report `partial_success.rejected_log_records`.
    // Currently logged as a warning; selective re-send of *only* the
    // rejected records is queued for a later release. The retry loop
    // in `send_batch` handles hard failures (connection refused, 5xx,
    // …) but not partial-success deltas, since the rejected set is a
    // strict subset of what already shipped.
    let inner_resp = response.into_inner();
    if let Some(partial) = inner_resp.partial_success
        && partial.rejected_log_records > 0
    {
        tracing::warn!(
            "otlp_grpc: {} rejected {} log record(s){}",
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

    fn mp(props: &[Property]) -> crate::modules::ModuleProperties {
        crate::modules::ModuleProperties::from_parts("otlp_grpc", props.to_vec())
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

    #[test]
    fn requires_endpoint() {
        let err = OtlpGrpcOutput::from_properties("o", &mp(&[]))
            .err()
            .unwrap();
        assert!(err.to_string().contains("endpoint"));
    }

    #[tokio::test]
    async fn accepts_plain_http_endpoint() {
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&[prop_str("endpoint", "http://localhost:4317")]),
        )
        .unwrap();
        // No external observation of the Channel beyond construction;
        // building without error is the contract.
        let _ = output.inner.endpoint.as_str();
    }

    #[tokio::test]
    async fn accepts_https_endpoint_with_native_tls() {
        let output = OtlpGrpcOutput::from_properties(
            "o",
            &mp(&[prop_str("endpoint", "https://collector.example.com:4317")]),
        )
        .unwrap();
        let _ = output.inner.endpoint.as_str();
    }

    #[tokio::test]
    async fn batch_level_default_is_none() {
        let output =
            OtlpGrpcOutput::from_properties("o", &mp(&[prop_str("endpoint", "http://x")])).unwrap();
        assert!(matches!(output.inner.batch_level, BatchLevel::None));
    }

    #[tokio::test]
    async fn batch_size_defaults_to_one() {
        let output =
            OtlpGrpcOutput::from_properties("o", &mp(&[prop_str("endpoint", "http://x")])).unwrap();
        assert_eq!(output.batch_size, 1);
    }

    #[tokio::test]
    async fn retry_block_overrides_defaults() {
        let props = vec![
            prop_str("endpoint", "http://x"),
            Property::Block {
                key: "retry".into(),
                key_span: None,
                properties: vec![
                    prop_int("max_attempts", 2),
                    prop_str("initial_wait", "100ms"),
                ],
            },
        ];
        let output = OtlpGrpcOutput::from_properties("o", &mp(&props)).unwrap();
        assert_eq!(output.inner.retry_config.max_attempts, 2);
        assert_eq!(
            output.inner.retry_config.initial_wait,
            Duration::from_millis(100)
        );
    }

    // ---- wire-level round-trip ----

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
        let mut e = Event::new(egress.clone(), "127.0.0.1:0".parse::<SocketAddr>().unwrap());
        e.egress = egress;
        e
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
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&[prop_str("endpoint", &endpoint), prop_int("batch_size", 1)]),
        )
        .unwrap();
        output
            .write_owned(&event_with_egress(singleton_bytes(
                1_700_000_000_000_000_000,
            )))
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
        assert_eq!(
            got[0].resource_logs[0].scope_logs[0]
                .scope
                .as_ref()
                .unwrap()
                .name,
            "limpid-test"
        );
    }

    #[tokio::test]
    async fn drop_aborts_pending_flush_timer() {
        let output = OtlpGrpcOutput::from_properties(
            "test",
            &mp(&[
                prop_str("endpoint", "http://127.0.0.1:1"),
                prop_int("batch_size", 1024),
                prop_str("batch_timeout", "30s"),
            ]),
        )
        .unwrap();
        output
            .write_owned(&event_with_egress(singleton_bytes(1)))
            .await
            .unwrap();
        let handle_before = output.flush_handle.lock().await.is_some();
        assert!(handle_before, "write must arm the flush timer");
        drop(output);
    }
}
