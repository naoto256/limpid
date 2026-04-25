//! `otlp_*` input modules: receive OpenTelemetry logs over OTLP/HTTP
//! and OTLP/gRPC. Both transports emit the same per-Event shape, so
//! the request-splitting logic and the singleton ResourceLogs builder
//! live here once and the transport modules ([`http`], [`grpc`]) only
//! own the framing / response shape that is genuinely transport-bound.

pub mod grpc;
pub mod http;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    common::v1::InstrumentationScope,
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use prost::Message;
use tokio::sync::mpsc;
use tracing::warn;

use crate::event::Event;
use crate::metrics::InputMetrics;

/// Result of splitting an `ExportLogsServiceRequest` along the
/// LogRecord axis. Transport-specific code translates this into the
/// transport's own response form (HTTP status code, gRPC `Status` /
/// `partial_success`, …). Counters that callers actually inspect:
///
/// - `rejected` — LogRecords that failed to re-encode (rare). Already
///   counted in `metrics.events_invalid`; the gRPC transport surfaces
///   it via `partial_success.rejected_log_records`.
/// - `aborted` — `true` when the pipeline channel closed mid-iteration
///   (shutdown). Remaining records were not attempted; transports
///   reply with their "service unavailable" form.
pub struct SplitOutcome {
    pub rejected: usize,
    pub aborted: bool,
}

/// Split an OTLP `ExportLogsServiceRequest` along the LogRecord axis,
/// emitting one Event per record. Each Event's `ingress` is the
/// singleton ResourceLogs proto-encoded bytes (1 Resource + 1 Scope +
/// 1 LogRecord) — the v0.5.0 OTLP hop contract.
///
/// Re-encode failures (rare; would only happen on a corrupt prost
/// state) are logged and counted in `metrics.events_invalid`. The
/// caller is expected to bump `events_received` once per RPC before
/// calling, regardless of body content.
///
/// `transport_name` is the literal `"otlp_http"` / `"otlp_grpc"` used
/// in tracing output so log lines stay attributable.
pub async fn split_request(
    req: ExportLogsServiceRequest,
    peer: SocketAddr,
    metrics: &Arc<InputMetrics>,
    tx: &mpsc::Sender<Event>,
    transport_name: &'static str,
) -> SplitOutcome {
    let mut rejected = 0usize;

    for rl in &req.resource_logs {
        for sl in &rl.scope_logs {
            for lr in &sl.log_records {
                let singleton = build_singleton(
                    rl.resource.clone(),
                    sl.scope.clone(),
                    lr.clone(),
                    rl.schema_url.clone(),
                    sl.schema_url.clone(),
                );
                let mut buf = Vec::with_capacity(singleton.encoded_len());
                if let Err(e) = singleton.encode(&mut buf) {
                    warn!("{transport_name} [{peer}]: re-encode failed: {e}");
                    metrics.events_invalid.fetch_add(1, Ordering::Relaxed);
                    rejected += 1;
                    continue;
                }
                let event = Event::new(Bytes::from(buf), peer);
                if tx.send(event).await.is_err() {
                    return SplitOutcome {
                        rejected,
                        aborted: true,
                    };
                }
            }
        }
    }

    SplitOutcome {
        rejected,
        aborted: false,
    }
}

/// Construct a singleton ResourceLogs message (1 Resource + 1 Scope +
/// exactly 1 LogRecord) — the per-Event shape that the rest of the
/// pipeline receives.
pub(super) fn build_singleton(
    resource: Option<Resource>,
    scope: Option<InstrumentationScope>,
    log_record: LogRecord,
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
        let lr = LogRecord {
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
