//! Shared OTLP output internals. The two public OTLP output modules
//! ([`http`] = `output otlp_http`, [`grpc`] = `output otlp_grpc`) own
//! their own DSL schema, transport client, and ship path. The bits
//! that are genuinely transport-agnostic — batch-level merging,
//! Resource / Scope equality, the retry block schema, the per-Event
//! payload type — live here so both transports stay byte-equivalent at
//! the OTLP wire layer regardless of which one a pipeline picks.
//!
//! Each Event's `egress` is expected to be the singleton ResourceLogs
//! protobuf bytes produced by `otlp.encode_resourcelog_protobuf` —
//! this is the v0.5.0 OTLP hop contract (1 Resource + 1 Scope + 1
//! LogRecord per Event). Each transport buffers the per-Event
//! ResourceLogs, flushes on `batch_size` or `batch_timeout`, wraps the
//! batch in an `ExportLogsServiceRequest`, and ships it.
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

pub mod grpc;
pub mod http;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use opentelemetry_proto::tonic::{
    collector::logs::v1::ExportLogsServiceRequest,
    common::v1::{InstrumentationScope, KeyValue},
    logs::v1::{ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};
use prost::Message;

use crate::dsl::schema::{PropertySpec, PropertyValueKind};

/// Transport-success outcome from a single OTLP export call.
///
/// `rejected` is the number of LogRecords the receiver acknowledged
/// as not-stored via OTLP's `partial_success.rejected_log_records`.
/// The HTTP 2xx / gRPC OK is still a transport success — the receiver
/// processed the request, it just refused some records (typically
/// quota / schema / size violations). limpid does not retry rejected
/// records (selective re-send is queued for a later release); the
/// counter split lets `events_failed` reflect the data loss so
/// operator dashboards stay accurate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SendOutcome {
    pub rejected: u64,
}

pub(crate) struct OtlpPayload {
    pub(crate) egress: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BatchLevel {
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
    pub(crate) fn parse(s: &str, output_name: &str) -> Result<Self> {
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

pub(crate) const OTLP_RETRY_BLOCK_PROPERTIES: &[PropertySpec] = &[
    PropertySpec {
        name: "max_attempts",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Int,
    },
    PropertySpec {
        name: "initial_wait",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "max_wait",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Duration,
    },
    PropertySpec {
        name: "backoff",
        required: false,
        repeatable: false,
        exclusive_group: None,
        kind: PropertyValueKind::Enum(&["fixed", "exponential"]),
    },
];

/// Decode a drained batch of per-Event ResourceLogs proto bytes and
/// wrap them in one `ExportLogsServiceRequest`, merging per
/// `batch_level`. Returns the request ready to be shipped over any
/// transport.
pub(crate) fn decode_drained_to_request(
    drained: Vec<Bytes>,
    batch_level: BatchLevel,
) -> Result<ExportLogsServiceRequest> {
    let mut decoded: Vec<ResourceLogs> = Vec::with_capacity(drained.len());
    for proto in &drained {
        let rl = ResourceLogs::decode(&**proto).with_context(|| {
            "output otlp: pipeline egress is not a valid ResourceLogs proto (wire it through `otlp.encode_resourcelog_protobuf`)"
        })?;
        decoded.push(rl);
    }
    Ok(match batch_level {
        BatchLevel::None => ExportLogsServiceRequest {
            resource_logs: decoded,
        },
        BatchLevel::Resource => merge_by_resource(decoded),
        BatchLevel::Scope => merge_by_scope(decoded),
    })
}

/// Group ResourceLogs by their Resource (attribute set +
/// dropped_attributes_count). Same-Resource entries collapse into one
/// ResourceLogs whose `scope_logs[]` is the concat of the inputs'
/// scope_logs. Order within each merged group preserves arrival order.
pub(crate) fn merge_by_resource(decoded: Vec<ResourceLogs>) -> ExportLogsServiceRequest {
    let mut out: Vec<ResourceLogs> = Vec::new();
    for rl in decoded {
        // Match only on (Resource, schema_url-compat). Two ResourceLogs
        // entries with identical Resource but *different non-empty*
        // schema_urls describe the same resource under different
        // schemas, and OTLP semantics say these are distinct — merging
        // them would silently drop one schema_url and conflate two
        // semantically different declarations into one bucket. Keep
        // them separate.
        if let Some(idx) = out.iter().position(|existing| {
            resources_eq(&existing.resource, &rl.resource)
                && schema_urls_compatible(&existing.schema_url, &rl.schema_url)
        }) {
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

/// Two schema_urls are merge-compatible when they're equal OR at least
/// one side is empty (= unspecified, can take the other's). Different
/// non-empty schema_urls are NOT compatible — keeping them in separate
/// buckets prevents the silent drop of one of the two declarations.
fn schema_urls_compatible(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b
}

/// `merge_by_resource` plus an inner pass: within each Resource bucket,
/// ScopeLogs sharing an InstrumentationScope (name + version +
/// attributes + dropped_attributes_count) collapse into a single
/// ScopeLogs whose `log_records[]` is the concat of the inputs.
pub(crate) fn merge_by_scope(decoded: Vec<ResourceLogs>) -> ExportLogsServiceRequest {
    let mut req = merge_by_resource(decoded);
    for rl in &mut req.resource_logs {
        let scope_logs = std::mem::take(&mut rl.scope_logs);
        let mut grouped: Vec<ScopeLogs> = Vec::new();
        for sl in scope_logs {
            // Same logic as the Resource-level merge above: only merge
            // ScopeLogs that share an InstrumentationScope AND have
            // compatible schema_urls. Different non-empty schema_urls
            // describe the same scope under different schemas and must
            // stay separate to avoid silently dropping one.
            if let Some(idx) = grouped.iter().position(|existing| {
                scopes_eq(&existing.scope, &sl.scope)
                    && schema_urls_compatible(&existing.schema_url, &sl.schema_url)
            }) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let input = vec![
            singleton("svc-a", "scope-1", 1),
            singleton("svc-a", "scope-2", 2),
        ];
        let req = merge_by_resource(input);
        assert_eq!(req.resource_logs.len(), 1);
        assert_eq!(req.resource_logs[0].scope_logs.len(), 2);
    }

    #[test]
    fn merge_by_resource_keeps_distinct_resources_separate() {
        let input = vec![
            singleton("svc-a", "scope-1", 1),
            singleton("svc-b", "scope-1", 2),
        ];
        let req = merge_by_resource(input);
        assert_eq!(req.resource_logs.len(), 2);
    }

    /// Helper: same-resource singleton with an explicit Resource-level
    /// schema_url so the schema-url merge rule can be exercised.
    fn singleton_with_schema(
        svc: &str,
        scope: &str,
        t: u64,
        resource_schema: &str,
    ) -> ResourceLogs {
        let mut rl = singleton(svc, scope, t);
        rl.schema_url = resource_schema.to_string();
        rl
    }

    #[test]
    fn merge_by_resource_does_not_drop_distinct_schema_urls() {
        // Same Resource, two different non-empty schema_urls. The
        // pre-fix code silently dropped one schema_url and conflated
        // the two declarations into a single bucket. They MUST stay
        // in separate buckets so neither schema_url is lost.
        let input = vec![
            singleton_with_schema("svc-a", "scope-1", 1, "https://schemas.example.com/v1"),
            singleton_with_schema("svc-a", "scope-1", 2, "https://schemas.example.com/v2"),
        ];
        let req = merge_by_resource(input);
        assert_eq!(
            req.resource_logs.len(),
            2,
            "different schema_urls must stay distinct"
        );
        let urls: Vec<&str> = req
            .resource_logs
            .iter()
            .map(|rl| rl.schema_url.as_str())
            .collect();
        assert!(urls.contains(&"https://schemas.example.com/v1"));
        assert!(urls.contains(&"https://schemas.example.com/v2"));
    }

    #[test]
    fn merge_by_resource_merges_empty_into_existing_schema_url() {
        // Same Resource, one with schema_url, one without. The empty
        // side is unspecified — taking the populated side's schema_url
        // is the existing 'rare but spec-allowed' promotion behaviour
        // and must not regress.
        let input = vec![
            singleton_with_schema("svc-a", "scope-1", 1, "https://schemas.example.com/v1"),
            singleton_with_schema("svc-a", "scope-2", 2, ""),
        ];
        let req = merge_by_resource(input);
        assert_eq!(req.resource_logs.len(), 1);
        assert_eq!(
            req.resource_logs[0].schema_url,
            "https://schemas.example.com/v1"
        );
        assert_eq!(req.resource_logs[0].scope_logs.len(), 2);
    }

    #[test]
    fn merge_by_resource_promotes_empty_acc_with_incoming_schema_url() {
        // Reverse order: empty schema_url first, then populated.
        let input = vec![
            singleton_with_schema("svc-a", "scope-1", 1, ""),
            singleton_with_schema("svc-a", "scope-2", 2, "https://schemas.example.com/v1"),
        ];
        let req = merge_by_resource(input);
        assert_eq!(req.resource_logs.len(), 1);
        assert_eq!(
            req.resource_logs[0].schema_url,
            "https://schemas.example.com/v1"
        );
    }

    #[test]
    fn merge_by_scope_does_not_drop_distinct_scope_schema_urls() {
        // Same Resource, same Scope, two different non-empty Scope-
        // level schema_urls. Like the Resource-level test, the
        // pre-fix code silently dropped one of the two.
        let mut a = singleton("svc-a", "scope-1", 1);
        a.scope_logs[0].schema_url = "https://schemas.example.com/scope/v1".into();
        let mut b = singleton("svc-a", "scope-1", 2);
        b.scope_logs[0].schema_url = "https://schemas.example.com/scope/v2".into();
        let req = merge_by_scope(vec![a, b]);
        // Same Resource bucket (only the resource matches; schema_urls
        // at the Resource level are empty so they're compatible) ...
        assert_eq!(req.resource_logs.len(), 1);
        // ... but two distinct ScopeLogs entries inside, one per
        // schema_url, with their log_records intact.
        assert_eq!(req.resource_logs[0].scope_logs.len(), 2);
        let scope_urls: Vec<&str> = req.resource_logs[0]
            .scope_logs
            .iter()
            .map(|sl| sl.schema_url.as_str())
            .collect();
        assert!(scope_urls.contains(&"https://schemas.example.com/scope/v1"));
        assert!(scope_urls.contains(&"https://schemas.example.com/scope/v2"));
    }

    #[test]
    fn merge_by_scope_collapses_same_resource_and_scope() {
        let input = vec![
            singleton("svc-a", "scope-1", 1),
            singleton("svc-a", "scope-1", 2),
        ];
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
}
