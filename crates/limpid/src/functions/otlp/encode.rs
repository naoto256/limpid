//! Implementation of the four `otlp.*` primitives.
//!
//! Each primitive is a thin wrapper around two manual mappers:
//! [`hashlit_to_resourcelog`] and [`resourcelog_to_hashlit`]. Going
//! through prost-derived structs (rather than serde-on-Value) keeps the
//! DSL HashLit shape decoupled from the proto crate's `with-serde`
//! camelCase + numeric-as-string conventions; the JSON form applies
//! those conventions only at the wire boundary.
//!
//! HashLit shape (mirrors the OTLP logs proto3 tree, snake_case keys):
//!
//! ```text
//! {
//!   resource: { attributes: [{ key, value: <AnyValue> }, ...], dropped_attributes_count, schema_url? },
//!   scope_logs: [{
//!     scope: { name, version, attributes, dropped_attributes_count },
//!     log_records: [{
//!       time_unix_nano, observed_time_unix_nano,
//!       severity_number, severity_text,
//!       body: <AnyValue>,
//!       attributes: [...],
//!       flags?, trace_id?, span_id?
//!     }],
//!     schema_url?
//!   }],
//!   schema_url?
//! }
//! ```
//!
//! `AnyValue` accepts the proto3 oneof in tagged form:
//! `{ string_value: "x" }`, `{ int_value: 5 }`, `{ bool_value: true }`,
//! `{ double_value: 3.14 }`, `{ array_value: { values: [<AnyValue>, ...] } }`,
//! `{ kvlist_value: { values: [{ key, value }, ...] } }`,
//! `{ bytes_value: <Bytes> }`. Each AnyValue must hold exactly one
//! variant.

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use prost::Message;

use opentelemetry_proto::tonic::{
    common::v1::{AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};

use crate::dsl::value::{Map, Value};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in_with_sig(
        "otlp",
        "encode_resourcelog_protobuf",
        FunctionSig::fixed(&[FieldType::Object], FieldType::Bytes),
        |args, _event| {
            let rl = hashlit_to_resourcelog(&args[0])?;
            let mut buf = Vec::with_capacity(rl.encoded_len());
            rl.encode(&mut buf)
                .map_err(|e| anyhow!("otlp.encode_resourcelog_protobuf: {e}"))?;
            Ok(Value::Bytes(Bytes::from(buf)))
        },
    );
    reg.register_in_with_sig(
        "otlp",
        "decode_resourcelog_protobuf",
        FunctionSig::fixed(&[FieldType::Bytes], FieldType::Object),
        |args, _event| {
            let bytes = match &args[0] {
                Value::Bytes(b) => b.clone(),
                Value::String(s) => Bytes::from(s.clone().into_bytes()),
                other => bail!(
                    "otlp.decode_resourcelog_protobuf: expected bytes, got {}",
                    other.type_name()
                ),
            };
            let rl = ResourceLogs::decode(&*bytes)
                .map_err(|e| anyhow!("otlp.decode_resourcelog_protobuf: {e}"))?;
            Ok(resourcelog_to_hashlit(&rl))
        },
    );
    reg.register_in_with_sig(
        "otlp",
        "encode_resourcelog_json",
        FunctionSig::fixed(&[FieldType::Object], FieldType::String),
        |args, _event| {
            let rl = hashlit_to_resourcelog(&args[0])?;
            let s = serde_json::to_string(&rl)
                .map_err(|e| anyhow!("otlp.encode_resourcelog_json: {e}"))?;
            Ok(Value::String(s))
        },
    );
    reg.register_in_with_sig(
        "otlp",
        "decode_resourcelog_json",
        FunctionSig::fixed(&[FieldType::String], FieldType::Object),
        |args, _event| {
            let s: String = match &args[0] {
                Value::String(s) => s.clone(),
                Value::Bytes(b) => std::str::from_utf8(b)
                    .map_err(|_| {
                        anyhow!("otlp.decode_resourcelog_json: bytes are not valid UTF-8")
                    })?
                    .to_string(),
                other => bail!(
                    "otlp.decode_resourcelog_json: expected string, got {}",
                    other.type_name()
                ),
            };
            let rl: ResourceLogs = serde_json::from_str(&s)
                .map_err(|e| anyhow!("otlp.decode_resourcelog_json: {e}"))?;
            Ok(resourcelog_to_hashlit(&rl))
        },
    );
}

// ---------------------------------------------------------------------------
// HashLit → prost
// ---------------------------------------------------------------------------

/// Top-level entry: a HashLit describing one ResourceLogs message.
fn hashlit_to_resourcelog(v: &Value) -> Result<ResourceLogs> {
    let map = expect_object(v, "ResourceLogs")?;
    Ok(ResourceLogs {
        resource: opt_field(map, "resource", hashlit_to_resource)?,
        scope_logs: array_field(map, "scope_logs", hashlit_to_scope_logs)?,
        schema_url: string_field(map, "schema_url").unwrap_or_default(),
    })
}

fn hashlit_to_resource(v: &Value) -> Result<Resource> {
    let map = expect_object(v, "Resource")?;
    Ok(Resource {
        attributes: array_field(map, "attributes", hashlit_to_keyvalue)?,
        dropped_attributes_count: u32_field(map, "dropped_attributes_count").unwrap_or(0),
    })
}

fn hashlit_to_scope_logs(v: &Value) -> Result<ScopeLogs> {
    let map = expect_object(v, "ScopeLogs")?;
    Ok(ScopeLogs {
        scope: opt_field(map, "scope", hashlit_to_scope)?,
        log_records: array_field(map, "log_records", hashlit_to_log_record)?,
        schema_url: string_field(map, "schema_url").unwrap_or_default(),
    })
}

fn hashlit_to_scope(v: &Value) -> Result<InstrumentationScope> {
    let map = expect_object(v, "InstrumentationScope")?;
    Ok(InstrumentationScope {
        name: string_field(map, "name").unwrap_or_default(),
        version: string_field(map, "version").unwrap_or_default(),
        attributes: array_field(map, "attributes", hashlit_to_keyvalue)?,
        dropped_attributes_count: u32_field(map, "dropped_attributes_count").unwrap_or(0),
    })
}

fn hashlit_to_log_record(v: &Value) -> Result<LogRecord> {
    let map = expect_object(v, "LogRecord")?;
    Ok(LogRecord {
        time_unix_nano: u64_field(map, "time_unix_nano").unwrap_or(0),
        observed_time_unix_nano: u64_field(map, "observed_time_unix_nano").unwrap_or(0),
        severity_number: i32_field(map, "severity_number").unwrap_or(0),
        severity_text: string_field(map, "severity_text").unwrap_or_default(),
        body: opt_field(map, "body", hashlit_to_anyvalue)?,
        attributes: array_field(map, "attributes", hashlit_to_keyvalue)?,
        dropped_attributes_count: u32_field(map, "dropped_attributes_count").unwrap_or(0),
        flags: u32_field(map, "flags").unwrap_or(0),
        trace_id: bytes_field(map, "trace_id").unwrap_or_default(),
        span_id: bytes_field(map, "span_id").unwrap_or_default(),
    })
}

fn hashlit_to_keyvalue(v: &Value) -> Result<KeyValue> {
    let map = expect_object(v, "KeyValue")?;
    let key = string_field(map, "key").ok_or_else(|| anyhow!("KeyValue: missing string `key`"))?;
    let value = opt_field(map, "value", hashlit_to_anyvalue)?;
    Ok(KeyValue { key, value })
}

/// Convert the tagged HashLit form into the proto3 `oneof` AnyValue.
/// Exactly one variant key must be present; the empty `{}` case yields
/// a "value-unset" AnyValue which round-trips faithfully through the
/// proto layer.
fn hashlit_to_anyvalue(v: &Value) -> Result<AnyValue> {
    let map = expect_object(v, "AnyValue")?;
    let mut found: Option<any_value::Value> = None;
    let mut set_variant = |key: &str, val: any_value::Value| -> Result<()> {
        if found.is_some() {
            bail!(
                "AnyValue: multiple variant keys present (only one of string_value/int_value/.../bytes_value allowed; offending key: {key})"
            );
        }
        found = Some(val);
        Ok(())
    };

    if let Some(s) = map.get("string_value").and_then(Value::as_str) {
        set_variant("string_value", any_value::Value::StringValue(s.to_string()))?;
    }
    if let Some(b) = map.get("bool_value").and_then(Value::as_bool) {
        set_variant("bool_value", any_value::Value::BoolValue(b))?;
    }
    if let Some(n) = map.get("int_value").and_then(Value::as_i64) {
        set_variant("int_value", any_value::Value::IntValue(n))?;
    }
    if let Some(f) = map.get("double_value").and_then(Value::as_f64) {
        // `as_f64` accepts both Int and Float forms in our Value, so
        // user-written `double_value: 1` is accepted alongside `1.0`.
        set_variant("double_value", any_value::Value::DoubleValue(f))?;
    }
    if let Some(arr_v) = map.get("array_value") {
        let arr_map = expect_object(arr_v, "AnyValue.array_value")?;
        let values = array_field(arr_map, "values", hashlit_to_anyvalue)?;
        set_variant(
            "array_value",
            any_value::Value::ArrayValue(ArrayValue { values }),
        )?;
    }
    if let Some(kv_v) = map.get("kvlist_value") {
        let kv_map = expect_object(kv_v, "AnyValue.kvlist_value")?;
        let values = array_field(kv_map, "values", hashlit_to_keyvalue)?;
        set_variant(
            "kvlist_value",
            any_value::Value::KvlistValue(KeyValueList { values }),
        )?;
    }
    if let Some(b) = map.get("bytes_value") {
        let bytes = match b {
            Value::Bytes(b) => b.to_vec(),
            // Convenience: accept a UTF-8 string as bytes too. This is
            // not formal per the proto spec but matches user intent
            // when the payload is text being smuggled through.
            Value::String(s) => s.clone().into_bytes(),
            other => bail!(
                "AnyValue.bytes_value: expected bytes or string, got {}",
                other.type_name()
            ),
        };
        set_variant("bytes_value", any_value::Value::BytesValue(bytes))?;
    }

    Ok(AnyValue { value: found })
}

// --- HashLit field accessors ---------------------------------------------
//
// Each helper extracts a typed field from a HashLit map, propagating a
// uniform error when the shape mismatches. They intentionally return
// `Option` for fields that the proto spec marks optional (or have a
// reasonable zero default), and `Result` for fields where a wrong shape
// must be surfaced.

fn expect_object<'a>(v: &'a Value, ctx: &str) -> Result<&'a Map> {
    v.as_object()
        .ok_or_else(|| anyhow!("{ctx}: expected object, got {}", v.type_name()))
}

fn string_field(map: &Map, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_string)
}

fn u32_field(map: &Map, key: &str) -> Option<u32> {
    map.get(key)
        .and_then(Value::as_i64)
        .filter(|n| (0..=u32::MAX as i64).contains(n))
        .map(|n| n as u32)
}

fn i32_field(map: &Map, key: &str) -> Option<i32> {
    map.get(key)
        .and_then(Value::as_i64)
        .filter(|n| (i32::MIN as i64..=i32::MAX as i64).contains(n))
        .map(|n| n as i32)
}

fn u64_field(map: &Map, key: &str) -> Option<u64> {
    map.get(key).and_then(|v| match v {
        Value::Int(n) if *n >= 0 => Some(*n as u64),
        Value::Float(f) if f.is_finite() && *f >= 0.0 && f.fract() == 0.0 => Some(*f as u64),
        _ => None,
    })
}

/// Read a `bytes` proto field. Accepts `Value::Bytes` (canonical) or a
/// `Value::String` containing a hex / base64-ish blob — *not* parsed,
/// passed verbatim. Most users will arrive here via `to_bytes()`.
fn bytes_field(map: &Map, key: &str) -> Option<Vec<u8>> {
    map.get(key).and_then(|v| match v {
        Value::Bytes(b) => Some(b.to_vec()),
        Value::String(s) => Some(s.clone().into_bytes()),
        _ => None,
    })
}

fn opt_field<T, F>(map: &Map, key: &str, f: F) -> Result<Option<T>>
where
    F: FnOnce(&Value) -> Result<T>,
{
    match map.get(key) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => f(v).map(Some),
    }
}

fn array_field<T, F>(map: &Map, key: &str, mut f: F) -> Result<Vec<T>>
where
    F: FnMut(&Value) -> Result<T>,
{
    match map.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(&mut f).collect(),
        Some(other) => bail!("field `{key}`: expected array, got {}", other.type_name()),
    }
}

// ---------------------------------------------------------------------------
// prost → HashLit
// ---------------------------------------------------------------------------

fn resourcelog_to_hashlit(rl: &ResourceLogs) -> Value {
    let mut map = Map::new();
    if let Some(r) = &rl.resource {
        map.insert("resource".into(), resource_to_hashlit(r));
    }
    map.insert(
        "scope_logs".into(),
        Value::Array(rl.scope_logs.iter().map(scope_logs_to_hashlit).collect()),
    );
    if !rl.schema_url.is_empty() {
        map.insert("schema_url".into(), Value::String(rl.schema_url.clone()));
    }
    Value::Object(map)
}

fn resource_to_hashlit(r: &Resource) -> Value {
    let mut map = Map::new();
    map.insert(
        "attributes".into(),
        Value::Array(r.attributes.iter().map(keyvalue_to_hashlit).collect()),
    );
    if r.dropped_attributes_count != 0 {
        map.insert(
            "dropped_attributes_count".into(),
            Value::Int(r.dropped_attributes_count as i64),
        );
    }
    Value::Object(map)
}

fn scope_logs_to_hashlit(sl: &ScopeLogs) -> Value {
    let mut map = Map::new();
    if let Some(s) = &sl.scope {
        map.insert("scope".into(), scope_to_hashlit(s));
    }
    map.insert(
        "log_records".into(),
        Value::Array(sl.log_records.iter().map(log_record_to_hashlit).collect()),
    );
    if !sl.schema_url.is_empty() {
        map.insert("schema_url".into(), Value::String(sl.schema_url.clone()));
    }
    Value::Object(map)
}

fn scope_to_hashlit(s: &InstrumentationScope) -> Value {
    let mut map = Map::new();
    if !s.name.is_empty() {
        map.insert("name".into(), Value::String(s.name.clone()));
    }
    if !s.version.is_empty() {
        map.insert("version".into(), Value::String(s.version.clone()));
    }
    if !s.attributes.is_empty() {
        map.insert(
            "attributes".into(),
            Value::Array(s.attributes.iter().map(keyvalue_to_hashlit).collect()),
        );
    }
    if s.dropped_attributes_count != 0 {
        map.insert(
            "dropped_attributes_count".into(),
            Value::Int(s.dropped_attributes_count as i64),
        );
    }
    Value::Object(map)
}

fn log_record_to_hashlit(lr: &LogRecord) -> Value {
    let mut map = Map::new();
    if lr.time_unix_nano != 0 {
        map.insert(
            "time_unix_nano".into(),
            Value::Int(lr.time_unix_nano as i64),
        );
    }
    if lr.observed_time_unix_nano != 0 {
        map.insert(
            "observed_time_unix_nano".into(),
            Value::Int(lr.observed_time_unix_nano as i64),
        );
    }
    if lr.severity_number != 0 {
        map.insert(
            "severity_number".into(),
            Value::Int(lr.severity_number as i64),
        );
    }
    if !lr.severity_text.is_empty() {
        map.insert(
            "severity_text".into(),
            Value::String(lr.severity_text.clone()),
        );
    }
    if let Some(b) = &lr.body {
        map.insert("body".into(), anyvalue_to_hashlit(b));
    }
    if !lr.attributes.is_empty() {
        map.insert(
            "attributes".into(),
            Value::Array(lr.attributes.iter().map(keyvalue_to_hashlit).collect()),
        );
    }
    if lr.dropped_attributes_count != 0 {
        map.insert(
            "dropped_attributes_count".into(),
            Value::Int(lr.dropped_attributes_count as i64),
        );
    }
    if lr.flags != 0 {
        map.insert("flags".into(), Value::Int(lr.flags as i64));
    }
    if !lr.trace_id.is_empty() {
        map.insert(
            "trace_id".into(),
            Value::Bytes(Bytes::from(lr.trace_id.clone())),
        );
    }
    if !lr.span_id.is_empty() {
        map.insert(
            "span_id".into(),
            Value::Bytes(Bytes::from(lr.span_id.clone())),
        );
    }
    Value::Object(map)
}

fn keyvalue_to_hashlit(kv: &KeyValue) -> Value {
    let mut map = Map::new();
    map.insert("key".into(), Value::String(kv.key.clone()));
    if let Some(v) = &kv.value {
        map.insert("value".into(), anyvalue_to_hashlit(v));
    }
    Value::Object(map)
}

fn anyvalue_to_hashlit(av: &AnyValue) -> Value {
    let mut map = Map::new();
    match &av.value {
        None => {}
        Some(any_value::Value::StringValue(s)) => {
            map.insert("string_value".into(), Value::String(s.clone()));
        }
        Some(any_value::Value::BoolValue(b)) => {
            map.insert("bool_value".into(), Value::Bool(*b));
        }
        Some(any_value::Value::IntValue(n)) => {
            map.insert("int_value".into(), Value::Int(*n));
        }
        Some(any_value::Value::DoubleValue(f)) => {
            map.insert("double_value".into(), Value::Float(*f));
        }
        Some(any_value::Value::ArrayValue(arr)) => {
            let mut inner = Map::new();
            inner.insert(
                "values".into(),
                Value::Array(arr.values.iter().map(anyvalue_to_hashlit).collect()),
            );
            map.insert("array_value".into(), Value::Object(inner));
        }
        Some(any_value::Value::KvlistValue(kvl)) => {
            let mut inner = Map::new();
            inner.insert(
                "values".into(),
                Value::Array(kvl.values.iter().map(keyvalue_to_hashlit).collect()),
            );
            map.insert("kvlist_value".into(), Value::Object(inner));
        }
        Some(any_value::Value::BytesValue(b)) => {
            map.insert("bytes_value".into(), Value::Bytes(Bytes::from(b.clone())));
        }
    }
    Value::Object(map)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::functions::FunctionRegistry;
    use std::net::SocketAddr;

    fn make_reg() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        super::super::register(&mut reg);
        reg
    }

    fn dummy_event() -> Event {
        Event::new(
            Bytes::from_static(b"test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn sample_hashlit() -> Value {
        let mut log_record = Map::new();
        log_record.insert(
            "time_unix_nano".into(),
            Value::Int(1_700_000_000_000_000_000),
        );
        log_record.insert("severity_number".into(), Value::Int(9)); // INFO
        log_record.insert("severity_text".into(), Value::String("INFO".into()));
        let mut body = Map::new();
        body.insert("string_value".into(), Value::String("hello".into()));
        log_record.insert("body".into(), Value::Object(body));

        let mut attr = Map::new();
        attr.insert("key".into(), Value::String("user".into()));
        let mut attr_val = Map::new();
        attr_val.insert("string_value".into(), Value::String("alice".into()));
        attr.insert("value".into(), Value::Object(attr_val));
        log_record.insert("attributes".into(), Value::Array(vec![Value::Object(attr)]));

        let mut scope = Map::new();
        scope.insert("name".into(), Value::String("limpid".into()));
        scope.insert("version".into(), Value::String("0.5.0".into()));

        let mut scope_logs = Map::new();
        scope_logs.insert("scope".into(), Value::Object(scope));
        scope_logs.insert(
            "log_records".into(),
            Value::Array(vec![Value::Object(log_record)]),
        );

        let mut svc_attr = Map::new();
        svc_attr.insert("key".into(), Value::String("service.name".into()));
        let mut svc_val = Map::new();
        svc_val.insert("string_value".into(), Value::String("test-svc".into()));
        svc_attr.insert("value".into(), Value::Object(svc_val));

        let mut resource = Map::new();
        resource.insert(
            "attributes".into(),
            Value::Array(vec![Value::Object(svc_attr)]),
        );

        let mut top = Map::new();
        top.insert("resource".into(), Value::Object(resource));
        top.insert(
            "scope_logs".into(),
            Value::Array(vec![Value::Object(scope_logs)]),
        );
        Value::Object(top)
    }

    #[test]
    fn protobuf_round_trip() {
        let reg = make_reg();
        let e = dummy_event();
        let hashlit = sample_hashlit();
        let bytes = reg
            .call(
                Some("otlp"),
                "encode_resourcelog_protobuf",
                &[hashlit.clone()],
                &e,
            )
            .unwrap();
        assert!(matches!(bytes, Value::Bytes(_)));
        let decoded = reg
            .call(Some("otlp"), "decode_resourcelog_protobuf", &[bytes], &e)
            .unwrap();
        // Round-trip: reconstructing from the same hashlit must yield
        // the same hashlit (modulo unset-default fields, which the
        // builder zero-suppresses on the way out).
        assert_eq!(decoded, hashlit);
    }

    #[test]
    fn json_round_trip() {
        let reg = make_reg();
        let e = dummy_event();
        let hashlit = sample_hashlit();
        let s = reg
            .call(
                Some("otlp"),
                "encode_resourcelog_json",
                &[hashlit.clone()],
                &e,
            )
            .unwrap();
        let s_str = match &s {
            Value::String(s) => s.clone(),
            _ => panic!("expected string"),
        };
        // Spec: OTLP/JSON uses camelCase. Pin one obvious witness so a
        // proto crate upgrade that flipped serde rename_all to snake
        // would surface here.
        assert!(
            s_str.contains("\"scopeLogs\"") || s_str.contains("scopeLogs"),
            "expected camelCase scopeLogs in: {s_str}"
        );
        let decoded = reg
            .call(Some("otlp"), "decode_resourcelog_json", &[s], &e)
            .unwrap();
        assert_eq!(decoded, hashlit);
    }

    #[test]
    fn protobuf_and_json_describe_same_message() {
        // Same HashLit through both wire formats must decode back to
        // the same HashLit shape — sanity that the manual mappers do
        // not diverge from serde's view.
        let reg = make_reg();
        let e = dummy_event();
        let hashlit = sample_hashlit();
        let pb = reg
            .call(
                Some("otlp"),
                "encode_resourcelog_protobuf",
                &[hashlit.clone()],
                &e,
            )
            .unwrap();
        let js = reg
            .call(
                Some("otlp"),
                "encode_resourcelog_json",
                &[hashlit.clone()],
                &e,
            )
            .unwrap();
        let from_pb = reg
            .call(Some("otlp"), "decode_resourcelog_protobuf", &[pb], &e)
            .unwrap();
        let from_js = reg
            .call(Some("otlp"), "decode_resourcelog_json", &[js], &e)
            .unwrap();
        assert_eq!(from_pb, from_js);
    }

    #[test]
    fn anyvalue_rejects_multiple_variants() {
        let reg = make_reg();
        let e = dummy_event();
        let mut body = Map::new();
        body.insert("string_value".into(), Value::String("x".into()));
        body.insert("int_value".into(), Value::Int(1));
        let mut log_record = Map::new();
        log_record.insert("body".into(), Value::Object(body));
        let mut scope_logs = Map::new();
        scope_logs.insert(
            "log_records".into(),
            Value::Array(vec![Value::Object(log_record)]),
        );
        let mut top = Map::new();
        top.insert(
            "scope_logs".into(),
            Value::Array(vec![Value::Object(scope_logs)]),
        );
        let err = reg
            .call(
                Some("otlp"),
                "encode_resourcelog_protobuf",
                &[Value::Object(top)],
                &e,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("multiple variant keys"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn missing_required_object_shape_errors() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                Some("otlp"),
                "encode_resourcelog_protobuf",
                &[Value::String("not an object".into())],
                &e,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("expected object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bytes_input_to_decode_protobuf() {
        // Pure decode path: empty bytes is a legal "no-content"
        // ResourceLogs. The decoded HashLit should have an empty
        // `scope_logs` array (the builder zero-suppresses) — this
        // tests the entry contract, not the data model.
        let reg = make_reg();
        let e = dummy_event();
        let v = reg
            .call(
                Some("otlp"),
                "decode_resourcelog_protobuf",
                &[Value::Bytes(Bytes::new())],
                &e,
            )
            .unwrap();
        assert!(matches!(v, Value::Object(_)));
    }
}
