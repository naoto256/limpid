//! OTLP/HTTP input: receive logs via the OTLP/HTTP transport.
//!
//! Listens for `POST /v1/logs` requests, accepts either
//! `application/x-protobuf` (canonical) or `application/json`
//! payloads, splits the incoming `ExportLogsServiceRequest` into one
//! Event per LogRecord, and emits each as a singleton ResourceLogs
//! (1 Resource + 1 Scope + 1 LogRecord — the v0.5.0 OTLP hop contract).
//!
//! ## Configuration
//!
//! ```text
//! def input otlp_in {
//!     type otlp_http
//!     bind "0.0.0.0:4318"
//! }
//! ```
//!
//! Per-Event shape (input writes only this much; payload semantics
//! belong to the process layer per Principle 2):
//!
//! - `ingress` = singleton ResourceLogs (1 Resource, 1 Scope, 1
//!   LogRecord) encoded as protobuf wire bytes
//! - `egress`  = `ingress.clone()` (default — process layer rewrites
//!   if needed)
//! - `source`  = TCP peer address
//! - `received_at` = `Utc::now()` at request handling time
//! - `workspace` = empty (decode is the process layer's job via
//!   `otlp.decode_resourcelog_protobuf(ingress)`)

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use anyhow::Result;
use axum::{
    Router,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest, common::v1::InstrumentationScope,
    logs::v1::{ResourceLogs, ScopeLogs}, resource::v1::Resource,
};
use prost::Message;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::dsl::ast::Property;
use crate::dsl::props;
use crate::event::Event;
use crate::metrics::InputMetrics;
use crate::modules::{HasMetrics, Input, Module};

pub struct OtlpHttpInput {
    bind_addr: String,
    metrics: Arc<InputMetrics>,
}

impl Module for OtlpHttpInput {
    fn from_properties(_name: &str, properties: &[Property]) -> Result<Self> {
        let bind = props::get_string(properties, "bind")
            .unwrap_or_else(|| "0.0.0.0:4318".to_string());
        Ok(Self {
            bind_addr: bind,
            metrics: Arc::new(InputMetrics::default()),
        })
    }
}

impl HasMetrics for OtlpHttpInput {
    type Stats = InputMetrics;
    fn metrics(&self) -> Arc<InputMetrics> {
        Arc::clone(&self.metrics)
    }
}

#[derive(Clone)]
struct AppState {
    tx: mpsc::Sender<Event>,
    metrics: Arc<InputMetrics>,
}

#[async_trait::async_trait]
impl Input for OtlpHttpInput {
    async fn run(
        self,
        tx: mpsc::Sender<Event>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let state = AppState {
            tx,
            metrics: Arc::clone(&self.metrics),
        };
        let app = Router::new()
            .route("/v1/logs", post(handle_logs))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind(&self.bind_addr).await?;
        info!("otlp_http listening on {}", self.bind_addr);

        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        });

        if let Err(e) = server.await {
            warn!("otlp_http server error: {}", e);
        }
        info!("otlp_http: shutting down");
        Ok(())
    }
}

/// Handler for `POST /v1/logs`.
async fn handle_logs(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    state.metrics.events_received.fetch_add(1, Ordering::Relaxed);

    // Decode the wire form. Default to protobuf when content-type is
    // missing or unrecognised — that's the canonical OTLP form and
    // matches what the `opentelemetry-collector` server does.
    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .unwrap_or_default();
    let req = match content_type.as_str() {
        "application/json" => match serde_json::from_slice::<ExportLogsServiceRequest>(&body) {
            Ok(r) => r,
            Err(e) => {
                warn!("otlp_http [{peer}]: JSON decode failed: {e}");
                state
                    .metrics
                    .events_invalid
                    .fetch_add(1, Ordering::Relaxed);
                return (StatusCode::BAD_REQUEST, "invalid OTLP/JSON payload").into_response();
            }
        },
        // application/x-protobuf, application/protobuf, or empty:
        // decode as proto. Empty content-type covers some clients that
        // omit the header but ship protobuf.
        _ => match ExportLogsServiceRequest::decode(&*body) {
            Ok(r) => r,
            Err(e) => {
                warn!("otlp_http [{peer}]: protobuf decode failed: {e}");
                state
                    .metrics
                    .events_invalid
                    .fetch_add(1, Ordering::Relaxed);
                return (StatusCode::BAD_REQUEST, "invalid OTLP/protobuf payload")
                    .into_response();
            }
        },
    };

    // Split into singleton ResourceLogs Events, one per LogRecord.
    let count = req
        .resource_logs
        .iter()
        .map(|rl| rl.scope_logs.iter().map(|sl| sl.log_records.len()).sum::<usize>())
        .sum::<usize>();
    if count == 0 {
        return (StatusCode::OK, "").into_response();
    }

    for rl in &req.resource_logs {
        for sl in &rl.scope_logs {
            for lr in &sl.log_records {
                let singleton = build_singleton(rl.resource.clone(), sl.scope.clone(), lr.clone(),
                    rl.schema_url.clone(), sl.schema_url.clone());
                let mut buf = Vec::with_capacity(singleton.encoded_len());
                if let Err(e) = singleton.encode(&mut buf) {
                    warn!("otlp_http [{peer}]: re-encode failed: {e}");
                    continue;
                }
                let bytes = Bytes::from(buf);
                let event = Event::new(bytes, peer);
                if state.tx.send(event).await.is_err() {
                    // Receiver dropped — pipeline shutting down.
                    return (StatusCode::SERVICE_UNAVAILABLE, "pipeline closed").into_response();
                }
            }
        }
    }

    (StatusCode::OK, "").into_response()
}

/// Build a singleton ResourceLogs message (1 Resource, 1 Scope,
/// exactly 1 LogRecord). This is the per-Event shape the rest of the
/// pipeline receives.
fn build_singleton(
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    log_record: opentelemetry_proto::tonic::logs::v1::LogRecord,
    resource_schema_url: String,
    scope_schema_url: String,
) -> ResourceLogs {
    ResourceLogs {
        resource,
        scope_logs: vec![ScopeLogs {
            scope,
            log_records: vec![log_record],
            schema_url: scope_schema_url,
        }],
        schema_url: resource_schema_url,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_address() {
        let i = OtlpHttpInput::from_properties("o", &[]).unwrap();
        assert_eq!(i.bind_addr, "0.0.0.0:4318");
    }

    #[test]
    fn build_singleton_yields_one_record_with_resource_and_scope() {
        let resource = Some(Resource {
            attributes: vec![],
            dropped_attributes_count: 0,
        });
        let scope = Some(InstrumentationScope {
            name: "test".into(),
            version: "0".into(),
            attributes: vec![],
            dropped_attributes_count: 0,
        });
        let lr = opentelemetry_proto::tonic::logs::v1::LogRecord {
            time_unix_nano: 42,
            severity_number: 9,
            severity_text: "INFO".into(),
            ..Default::default()
        };
        let s = build_singleton(resource, scope, lr, String::new(), String::new());
        assert_eq!(s.scope_logs.len(), 1);
        assert_eq!(s.scope_logs[0].log_records.len(), 1);
        assert_eq!(s.scope_logs[0].log_records[0].time_unix_nano, 42);
    }
}
