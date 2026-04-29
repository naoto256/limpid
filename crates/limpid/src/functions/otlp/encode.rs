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
use prost::Message;

use opentelemetry_proto::tonic::{
    common::v1::{AnyValue, ArrayValue, InstrumentationScope, KeyValue, KeyValueList, any_value},
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in_with_sig(
        "otlp",
        "encode_resourcelog_protobuf",
        FunctionSig::fixed(&[FieldType::Object], FieldType::Bytes),
        |arena, args, _event| {
            let rl = hashlit_to_resourcelog(&args[0])?;
            let mut buf = Vec::with_capacity(rl.encoded_len());
            rl.encode(&mut buf)
                .map_err(|e| anyhow!("otlp.encode_resourcelog_protobuf: {e}"))?;
            Ok(Value::Bytes(arena.alloc_bytes(&buf)))
        },
    );
    reg.register_in_with_sig(
        "otlp",
        "decode_resourcelog_protobuf",
        FunctionSig::fixed(&[FieldType::Bytes], FieldType::Object),
        |arena, args, _event| {
            let bytes: &[u8] = match &args[0] {
                Value::Bytes(b) => b,
                Value::String(s) => s.as_bytes(),
                other => bail!(
                    "otlp.decode_resourcelog_protobuf: expected bytes, got {}",
                    other.type_name()
                ),
            };
            let rl = ResourceLogs::decode(bytes)
                .map_err(|e| anyhow!("otlp.decode_resourcelog_protobuf: {e}"))?;
            Ok(resourcelog_to_hashlit(arena, &rl))
        },
    );
    reg.register_in_with_sig(
        "otlp",
        "encode_resourcelog_json",
        FunctionSig::fixed(&[FieldType::Object], FieldType::String),
        |arena, args, _event| {
            let rl = hashlit_to_resourcelog(&args[0])?;
            let s = serde_json::to_string(&rl)
                .map_err(|e| anyhow!("otlp.encode_resourcelog_json: {e}"))?;
            Ok(Value::String(arena.alloc_str(&s)))
        },
    );
    reg.register_in_with_sig(
        "otlp",
        "decode_resourcelog_json",
        FunctionSig::fixed(&[FieldType::String], FieldType::Object),
        |arena, args, _event| {
            let s: &str = match &args[0] {
                Value::String(s) => s,
                Value::Bytes(b) => std::str::from_utf8(b).map_err(|_| {
                    anyhow!("otlp.decode_resourcelog_json: bytes are not valid UTF-8")
                })?,
                other => bail!(
                    "otlp.decode_resourcelog_json: expected string, got {}",
                    other.type_name()
                ),
            };
            let rl: ResourceLogs = serde_json::from_str(s)
                .map_err(|e| anyhow!("otlp.decode_resourcelog_json: {e}"))?;
            Ok(resourcelog_to_hashlit(arena, &rl))
        },
    );
}

// ---------------------------------------------------------------------------
// HashLit → prost
// ---------------------------------------------------------------------------

/// Top-level entry: a HashLit describing one ResourceLogs message.
fn hashlit_to_resourcelog(v: &Value<'_>) -> Result<ResourceLogs> {
    let entries = expect_object(v, "ResourceLogs")?;
    Ok(ResourceLogs {
        resource: opt_field(entries, "resource", hashlit_to_resource)?,
        scope_logs: array_field(entries, "scope_logs", hashlit_to_scope_logs)?,
        schema_url: string_field(entries, "schema_url").unwrap_or_default(),
    })
}

fn hashlit_to_resource(v: &Value<'_>) -> Result<Resource> {
    let entries = expect_object(v, "Resource")?;
    Ok(Resource {
        attributes: array_field(entries, "attributes", hashlit_to_keyvalue)?,
        dropped_attributes_count: u32_field(entries, "dropped_attributes_count").unwrap_or(0),
    })
}

fn hashlit_to_scope_logs(v: &Value<'_>) -> Result<ScopeLogs> {
    let entries = expect_object(v, "ScopeLogs")?;
    Ok(ScopeLogs {
        scope: opt_field(entries, "scope", hashlit_to_scope)?,
        log_records: array_field(entries, "log_records", hashlit_to_log_record)?,
        schema_url: string_field(entries, "schema_url").unwrap_or_default(),
    })
}

fn hashlit_to_scope(v: &Value<'_>) -> Result<InstrumentationScope> {
    let entries = expect_object(v, "InstrumentationScope")?;
    Ok(InstrumentationScope {
        name: string_field(entries, "name").unwrap_or_default(),
        version: string_field(entries, "version").unwrap_or_default(),
        attributes: array_field(entries, "attributes", hashlit_to_keyvalue)?,
        dropped_attributes_count: u32_field(entries, "dropped_attributes_count").unwrap_or(0),
    })
}

fn hashlit_to_log_record(v: &Value<'_>) -> Result<LogRecord> {
    let entries = expect_object(v, "LogRecord")?;
    Ok(LogRecord {
        time_unix_nano: u64_field(entries, "time_unix_nano").unwrap_or(0),
        observed_time_unix_nano: u64_field(entries, "observed_time_unix_nano").unwrap_or(0),
        severity_number: i32_field(entries, "severity_number").unwrap_or(0),
        severity_text: string_field(entries, "severity_text").unwrap_or_default(),
        body: opt_field(entries, "body", hashlit_to_anyvalue)?,
        attributes: array_field(entries, "attributes", hashlit_to_keyvalue)?,
        dropped_attributes_count: u32_field(entries, "dropped_attributes_count").unwrap_or(0),
        flags: u32_field(entries, "flags").unwrap_or(0),
        trace_id: bytes_field(entries, "trace_id").unwrap_or_default(),
        span_id: bytes_field(entries, "span_id").unwrap_or_default(),
    })
}

fn hashlit_to_keyvalue(v: &Value<'_>) -> Result<KeyValue> {
    let entries = expect_object(v, "KeyValue")?;
    let key =
        string_field(entries, "key").ok_or_else(|| anyhow!("KeyValue: missing string `key`"))?;
    let value = opt_field(entries, "value", hashlit_to_anyvalue)?;
    Ok(KeyValue { key, value })
}

/// Convert the tagged HashLit form into the proto3 `oneof` AnyValue.
fn hashlit_to_anyvalue(v: &Value<'_>) -> Result<AnyValue> {
    let entries = expect_object(v, "AnyValue")?;
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

    if let Some(s) = lookup(entries, "string_value").and_then(|v| v.as_str()) {
        set_variant("string_value", any_value::Value::StringValue(s.to_string()))?;
    }
    if let Some(b) = lookup(entries, "bool_value").and_then(|v| v.as_bool()) {
        set_variant("bool_value", any_value::Value::BoolValue(b))?;
    }
    if let Some(n) = lookup(entries, "int_value").and_then(|v| v.as_i64()) {
        set_variant("int_value", any_value::Value::IntValue(n))?;
    }
    if let Some(f) = lookup(entries, "double_value").and_then(|v| v.as_f64()) {
        set_variant("double_value", any_value::Value::DoubleValue(f))?;
    }
    if let Some(arr_v) = lookup(entries, "array_value") {
        let arr_entries = expect_object(&arr_v, "AnyValue.array_value")?;
        let values = array_field(arr_entries, "values", hashlit_to_anyvalue)?;
        set_variant(
            "array_value",
            any_value::Value::ArrayValue(ArrayValue { values }),
        )?;
    }
    if let Some(kv_v) = lookup(entries, "kvlist_value") {
        let kv_entries = expect_object(&kv_v, "AnyValue.kvlist_value")?;
        let values = array_field(kv_entries, "values", hashlit_to_keyvalue)?;
        set_variant(
            "kvlist_value",
            any_value::Value::KvlistValue(KeyValueList { values }),
        )?;
    }
    if let Some(b) = lookup(entries, "bytes_value") {
        let bytes = match b {
            Value::Bytes(b) => b.to_vec(),
            // Convenience: accept a UTF-8 string as bytes too.
            Value::String(s) => s.as_bytes().to_vec(),
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

type Entries<'bump> = &'bump [(&'bump str, Value<'bump>)];

fn expect_object<'a, 'bump: 'a>(v: &'a Value<'bump>, ctx: &str) -> Result<Entries<'bump>> {
    v.as_object()
        .ok_or_else(|| anyhow!("{ctx}: expected object, got {}", v.type_name()))
}

fn lookup<'bump>(entries: Entries<'bump>, key: &str) -> Option<Value<'bump>> {
    entries
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)
}

fn string_field(entries: Entries<'_>, key: &str) -> Option<String> {
    lookup(entries, key)
        .and_then(|v| v.as_str().map(|s| s.to_string()))
}

fn u32_field(entries: Entries<'_>, key: &str) -> Option<u32> {
    lookup(entries, key)
        .and_then(|v| v.as_i64())
        .filter(|n| (0..=u32::MAX as i64).contains(n))
        .map(|n| n as u32)
}

fn i32_field(entries: Entries<'_>, key: &str) -> Option<i32> {
    lookup(entries, key)
        .and_then(|v| v.as_i64())
        .filter(|n| (i32::MIN as i64..=i32::MAX as i64).contains(n))
        .map(|n| n as i32)
}

fn u64_field(entries: Entries<'_>, key: &str) -> Option<u64> {
    lookup(entries, key).and_then(|v| match v {
        Value::Int(n) if n >= 0 => Some(n as u64),
        Value::Float(f) if f.is_finite() && f >= 0.0 && f.fract() == 0.0 => Some(f as u64),
        _ => None,
    })
}

fn bytes_field(entries: Entries<'_>, key: &str) -> Option<Vec<u8>> {
    lookup(entries, key).and_then(|v| match v {
        Value::Bytes(b) => Some(b.to_vec()),
        Value::String(s) => Some(s.as_bytes().to_vec()),
        _ => None,
    })
}

fn opt_field<'bump, T, F>(entries: Entries<'bump>, key: &str, f: F) -> Result<Option<T>>
where
    F: FnOnce(&Value<'bump>) -> Result<T>,
{
    match lookup(entries, key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => f(&v).map(Some),
    }
}

fn array_field<'bump, T, F>(entries: Entries<'bump>, key: &str, mut f: F) -> Result<Vec<T>>
where
    F: FnMut(&Value<'bump>) -> Result<T>,
{
    match lookup(entries, key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(&mut f).collect(),
        Some(other) => bail!("field `{key}`: expected array, got {}", other.type_name()),
    }
}

// ---------------------------------------------------------------------------
// prost → HashLit (arena-backed)
// ---------------------------------------------------------------------------

fn resourcelog_to_hashlit<'bump>(
    arena: &EventArena<'bump>,
    rl: &ResourceLogs,
) -> Value<'bump> {
    let mut b = ObjectBuilder::new(arena);
    if let Some(r) = &rl.resource {
        b.push("resource", resource_to_hashlit(arena, r));
    }
    let mut sl_arr = bumpalo::collections::Vec::with_capacity_in(rl.scope_logs.len(), arena.bump());
    for sl in &rl.scope_logs {
        sl_arr.push(scope_logs_to_hashlit(arena, sl));
    }
    b.push("scope_logs", Value::Array(sl_arr.into_bump_slice()));
    if !rl.schema_url.is_empty() {
        b.push("schema_url", Value::String(arena.alloc_str(&rl.schema_url)));
    }
    b.finish()
}

fn resource_to_hashlit<'bump>(arena: &EventArena<'bump>, r: &Resource) -> Value<'bump> {
    let mut b = ObjectBuilder::new(arena);
    let mut attrs = bumpalo::collections::Vec::with_capacity_in(r.attributes.len(), arena.bump());
    for kv in &r.attributes {
        attrs.push(keyvalue_to_hashlit(arena, kv));
    }
    b.push("attributes", Value::Array(attrs.into_bump_slice()));
    if r.dropped_attributes_count != 0 {
        b.push_str(
            "dropped_attributes_count",
            Value::Int(r.dropped_attributes_count as i64),
        );
    }
    b.finish()
}

fn scope_logs_to_hashlit<'bump>(arena: &EventArena<'bump>, sl: &ScopeLogs) -> Value<'bump> {
    let mut b = ObjectBuilder::new(arena);
    if let Some(s) = &sl.scope {
        b.push("scope", scope_to_hashlit(arena, s));
    }
    let mut lrs = bumpalo::collections::Vec::with_capacity_in(sl.log_records.len(), arena.bump());
    for lr in &sl.log_records {
        lrs.push(log_record_to_hashlit(arena, lr));
    }
    b.push("log_records", Value::Array(lrs.into_bump_slice()));
    if !sl.schema_url.is_empty() {
        b.push("schema_url", Value::String(arena.alloc_str(&sl.schema_url)));
    }
    b.finish()
}

fn scope_to_hashlit<'bump>(
    arena: &EventArena<'bump>,
    s: &InstrumentationScope,
) -> Value<'bump> {
    let mut b = ObjectBuilder::new(arena);
    if !s.name.is_empty() {
        b.push("name", Value::String(arena.alloc_str(&s.name)));
    }
    if !s.version.is_empty() {
        b.push("version", Value::String(arena.alloc_str(&s.version)));
    }
    if !s.attributes.is_empty() {
        let mut attrs =
            bumpalo::collections::Vec::with_capacity_in(s.attributes.len(), arena.bump());
        for kv in &s.attributes {
            attrs.push(keyvalue_to_hashlit(arena, kv));
        }
        b.push("attributes", Value::Array(attrs.into_bump_slice()));
    }
    if s.dropped_attributes_count != 0 {
        b.push_str(
            "dropped_attributes_count",
            Value::Int(s.dropped_attributes_count as i64),
        );
    }
    b.finish()
}

fn log_record_to_hashlit<'bump>(arena: &EventArena<'bump>, lr: &LogRecord) -> Value<'bump> {
    let mut b = ObjectBuilder::new(arena);
    if lr.time_unix_nano != 0 {
        b.push("time_unix_nano", Value::Int(lr.time_unix_nano as i64));
    }
    if lr.observed_time_unix_nano != 0 {
        b.push_str(
            "observed_time_unix_nano",
            Value::Int(lr.observed_time_unix_nano as i64),
        );
    }
    if lr.severity_number != 0 {
        b.push("severity_number", Value::Int(lr.severity_number as i64));
    }
    if !lr.severity_text.is_empty() {
        b.push_str(
            "severity_text",
            Value::String(arena.alloc_str(&lr.severity_text)),
        );
    }
    if let Some(body) = &lr.body {
        b.push("body", anyvalue_to_hashlit(arena, body));
    }
    if !lr.attributes.is_empty() {
        let mut attrs =
            bumpalo::collections::Vec::with_capacity_in(lr.attributes.len(), arena.bump());
        for kv in &lr.attributes {
            attrs.push(keyvalue_to_hashlit(arena, kv));
        }
        b.push("attributes", Value::Array(attrs.into_bump_slice()));
    }
    if lr.dropped_attributes_count != 0 {
        b.push_str(
            "dropped_attributes_count",
            Value::Int(lr.dropped_attributes_count as i64),
        );
    }
    if lr.flags != 0 {
        b.push("flags", Value::Int(lr.flags as i64));
    }
    if !lr.trace_id.is_empty() {
        b.push("trace_id", Value::Bytes(arena.alloc_bytes(&lr.trace_id)));
    }
    if !lr.span_id.is_empty() {
        b.push("span_id", Value::Bytes(arena.alloc_bytes(&lr.span_id)));
    }
    b.finish()
}

fn keyvalue_to_hashlit<'bump>(arena: &EventArena<'bump>, kv: &KeyValue) -> Value<'bump> {
    let mut b = ObjectBuilder::with_capacity(arena, 2);
    b.push("key", Value::String(arena.alloc_str(&kv.key)));
    if let Some(v) = &kv.value {
        b.push("value", anyvalue_to_hashlit(arena, v));
    }
    b.finish()
}

fn anyvalue_to_hashlit<'bump>(arena: &EventArena<'bump>, av: &AnyValue) -> Value<'bump> {
    let mut b = ObjectBuilder::new(arena);
    match &av.value {
        None => {}
        Some(any_value::Value::StringValue(s)) => {
            b.push("string_value", Value::String(arena.alloc_str(s)));
        }
        Some(any_value::Value::BoolValue(bv)) => {
            b.push("bool_value", Value::Bool(*bv));
        }
        Some(any_value::Value::IntValue(n)) => {
            b.push("int_value", Value::Int(*n));
        }
        Some(any_value::Value::DoubleValue(f)) => {
            b.push("double_value", Value::Float(*f));
        }
        Some(any_value::Value::ArrayValue(arr)) => {
            let mut inner = ObjectBuilder::with_capacity(arena, 1);
            let mut vals =
                bumpalo::collections::Vec::with_capacity_in(arr.values.len(), arena.bump());
            for vv in &arr.values {
                vals.push(anyvalue_to_hashlit(arena, vv));
            }
            inner.push("values", Value::Array(vals.into_bump_slice()));
            b.push("array_value", inner.finish());
        }
        Some(any_value::Value::KvlistValue(kvl)) => {
            let mut inner = ObjectBuilder::with_capacity(arena, 1);
            let mut vals =
                bumpalo::collections::Vec::with_capacity_in(kvl.values.len(), arena.bump());
            for kv in &kvl.values {
                vals.push(keyvalue_to_hashlit(arena, kv));
            }
            inner.push("values", Value::Array(vals.into_bump_slice()));
            b.push("kvlist_value", inner.finish());
        }
        Some(any_value::Value::BytesValue(bytes)) => {
            b.push("bytes_value", Value::Bytes(arena.alloc_bytes(bytes)));
        }
    }
    b.finish()
}
