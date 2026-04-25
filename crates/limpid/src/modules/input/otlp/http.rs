//! OTLP/HTTP input: receive logs via the OTLP/HTTP transport.
//!
//! Listens for `POST /v1/logs` requests, accepts either
//! `application/x-protobuf` (canonical) or `application/json`
//! payloads, and delegates request splitting to
//! [`super::split_request`]. The shared helper is what every OTLP
//! input transport runs through, so the per-Event shape stays
//! identical across HTTP and gRPC.
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
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use prost::Message;
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::split_request;
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

/// Handler for `POST /v1/logs`. Decodes the wire body per
/// `Content-Type`, then hands the parsed request off to
/// [`split_request`] which is shared with [`super::grpc`].
async fn handle_logs(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    state.metrics.events_received.fetch_add(1, Ordering::Relaxed);

    // Default to protobuf when Content-Type is missing or unrecognised
    // — canonical OTLP form, matches what `opentelemetry-collector`
    // does when clients omit the header.
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

    let outcome = split_request(req, peer, &state.metrics, &state.tx, "otlp_http").await;
    if outcome.aborted {
        return (StatusCode::SERVICE_UNAVAILABLE, "pipeline closed").into_response();
    }
    // OTLP/HTTP does not return `partial_success` on the wire — the
    // gRPC variant does, but `application/x-protobuf` over HTTP would
    // need an `ExportLogsServiceResponse` body which we omit for
    // simplicity. `events_invalid` already counts the rejects.
    (StatusCode::OK, "").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_bind_address() {
        let i = OtlpHttpInput::from_properties("o", &[]).unwrap();
        assert_eq!(i.bind_addr, "0.0.0.0:4318");
    }
}
