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

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow, bail};
use prost::Message;

use opentelemetry_proto::tonic::{
    common::v1::{
        AnyValue, ArrayValue, EntityRef, InstrumentationScope, KeyValue, KeyValueList, any_value,
    },
    logs::v1::{LogRecord, ResourceLogs, ScopeLogs},
    resource::v1::Resource,
};

use crate::dsl::arena::EventArena;
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::{ObjectBuilder, Value};
use crate::functions::{FunctionRegistry, FunctionSig};

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
        // OTLP 0.32: Resource can carry a list of EntityRef pointers into the
        // Resource's own attributes. Non-Entity workflows leave this empty; we
        // still accept it so a round-trip through the DSL primitive preserves
        // an inbound EntityRef list a caller might one day emit.
        entity_refs: array_field(entries, "entity_refs", hashlit_to_entity_ref)?,
    })
}

fn hashlit_to_entity_ref(v: &Value<'_>) -> Result<EntityRef> {
    let entries = expect_object(v, "EntityRef")?;
    let string_list = |key: &str| -> Result<Vec<String>> {
        array_field(entries, key, |v| {
            v.as_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("EntityRef.{}: expected string element", key))
        })
    };
    Ok(EntityRef {
        schema_url: string_field(entries, "schema_url").unwrap_or_default(),
        r#type: string_field(entries, "type").unwrap_or_default(),
        id_keys: string_list("id_keys")?,
        description_keys: string_list("description_keys")?,
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
        time_unix_nano: timestamp_u64_field(entries, "time_unix_nano", &WARNED_TIME_UNIX_NANO),
        observed_time_unix_nano: timestamp_u64_field(
            entries,
            "observed_time_unix_nano",
            &WARNED_OBSERVED_TIME_UNIX_NANO,
        ),
        severity_number: i32_field(entries, "severity_number").unwrap_or(0),
        severity_text: string_field(entries, "severity_text").unwrap_or_default(),
        body: opt_field(entries, "body", hashlit_to_anyvalue)?,
        attributes: array_field(entries, "attributes", hashlit_to_keyvalue)?,
        dropped_attributes_count: u32_field(entries, "dropped_attributes_count").unwrap_or(0),
        flags: u32_field(entries, "flags").unwrap_or(0),
        trace_id: bytes_field(entries, "trace_id").unwrap_or_default(),
        span_id: bytes_field(entries, "span_id").unwrap_or_default(),
        // OTLP 0.32: promoted from an `event.name` attribute to a first-class
        // field on LogRecord.
        event_name: string_field(entries, "event_name").unwrap_or_default(),
    })
}

fn hashlit_to_keyvalue(v: &Value<'_>) -> Result<KeyValue> {
    let entries = expect_object(v, "KeyValue")?;
    let key =
        string_field(entries, "key").ok_or_else(|| anyhow!("KeyValue: missing string `key`"))?;
    let value = opt_field(entries, "value", hashlit_to_anyvalue)?;
    Ok(KeyValue {
        key,
        value,
        // OTLP 0.32: Profiles-only field (index into
        // ProfilesDictionary.string_table). Non-Profiles signals such as
        // Logs / Traces / Metrics leave it at 0; we accept it from the DSL so
        // a Profiles-aware caller can round-trip through the primitive.
        key_strindex: i32_field(entries, "key_strindex").unwrap_or(0),
    })
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
    // OTLP 0.32: Profiles-only variant (index into
    // ProfilesDictionary.string_table). Round-tripped through the DSL so a
    // Profiles-aware caller can construct it; not resolved to a string here
    // because the string table is a Profiles-level structure the primitive
    // does not carry. `i32_field` range-checks the DSL value so an
    // out-of-`i32`-range integer becomes a load-time error rather than a
    // silent wrap — same treatment as the sibling `key_strindex` on
    // `KeyValue`.
    if let Some(n) = i32_field(entries, "string_value_strindex") {
        set_variant(
            "string_value_strindex",
            any_value::Value::StringValueStrindex(n),
        )?;
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
    entries.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

fn string_field(entries: Entries<'_>, key: &str) -> Option<String> {
    lookup(entries, key).and_then(|v| v.as_str().map(|s| s.to_string()))
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

fn coerce_u64(v: Value<'_>) -> Option<u64> {
    match v {
        Value::Int(n) if n >= 0 => Some(n as u64),
        // `f < u64::MAX as f64` (strict less-than): `as u64` is a
        // saturating cast since Rust 1.45, so a value that satisfies
        // the existing finite / non-negative / integral guards but
        // sits above the u64 range would silently saturate to
        // `u64::MAX` and be encoded as a year-2554+ timestamp on the
        // wire. The strict bound also rejects `u64::MAX as f64`
        // itself: that f64 rounds up to 2^64, which is outside the
        // u64 range and would saturate to `u64::MAX` for the same
        // reason. Out-of-range floats flow to `None` here and pick
        // up the `timestamp_u64_field` warn-once + encode-0 path.
        Value::Float(f) if f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f < u64::MAX as f64 => {
            Some(f as u64)
        }
        Value::Timestamp(dt) => dt.timestamp_nanos_opt().and_then(|n| u64::try_from(n).ok()),
        _ => None,
    }
}

static WARNED_TIME_UNIX_NANO: AtomicBool = AtomicBool::new(false);
static WARNED_OBSERVED_TIME_UNIX_NANO: AtomicBool = AtomicBool::new(false);

/// Same coercion as `u64_field`, but distinguishes "key absent" (silent —
/// legitimate per OTLP, the receiver may fill the timestamp server-side)
/// from "key present but uncoercible" (warn once per process, then stay
/// silent so a broken upstream cannot flood logs at wire rate).
fn timestamp_u64_field(entries: Entries<'_>, key: &str, warned: &AtomicBool) -> u64 {
    let Some(v) = lookup(entries, key) else {
        return 0;
    };
    if let Some(n) = coerce_u64(v) {
        return n;
    }
    if !warned.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            field = key,
            value_type = v.type_name(),
            "otlp.encode_resourcelog_*: {key} present but uncoercible to u64 nanos; \
             encoding 0 on the wire. Further occurrences will not be logged for this field."
        );
    }
    0
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

fn resourcelog_to_hashlit<'bump>(arena: &EventArena<'bump>, rl: &ResourceLogs) -> Value<'bump> {
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
    if !r.entity_refs.is_empty() {
        let mut refs =
            bumpalo::collections::Vec::with_capacity_in(r.entity_refs.len(), arena.bump());
        for er in &r.entity_refs {
            refs.push(entity_ref_to_hashlit(arena, er));
        }
        b.push("entity_refs", Value::Array(refs.into_bump_slice()));
    }
    b.finish()
}

fn entity_ref_to_hashlit<'bump>(arena: &EventArena<'bump>, er: &EntityRef) -> Value<'bump> {
    let mut b = ObjectBuilder::new(arena);
    if !er.schema_url.is_empty() {
        b.push("schema_url", Value::String(arena.alloc_str(&er.schema_url)));
    }
    if !er.r#type.is_empty() {
        b.push("type", Value::String(arena.alloc_str(&er.r#type)));
    }
    let push_strings = |b: &mut ObjectBuilder<'bump>, key: &'static str, xs: &[String]| {
        if xs.is_empty() {
            return;
        }
        let mut arr = bumpalo::collections::Vec::with_capacity_in(xs.len(), arena.bump());
        for s in xs {
            arr.push(Value::String(arena.alloc_str(s)));
        }
        b.push(key, Value::Array(arr.into_bump_slice()));
    };
    push_strings(&mut b, "id_keys", &er.id_keys);
    push_strings(&mut b, "description_keys", &er.description_keys);
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

fn scope_to_hashlit<'bump>(arena: &EventArena<'bump>, s: &InstrumentationScope) -> Value<'bump> {
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
    if !lr.event_name.is_empty() {
        b.push("event_name", Value::String(arena.alloc_str(&lr.event_name)));
    }
    b.finish()
}

fn keyvalue_to_hashlit<'bump>(arena: &EventArena<'bump>, kv: &KeyValue) -> Value<'bump> {
    let mut b = ObjectBuilder::with_capacity(arena, 2);
    b.push("key", Value::String(arena.alloc_str(&kv.key)));
    if let Some(v) = &kv.value {
        b.push("value", anyvalue_to_hashlit(arena, v));
    }
    if kv.key_strindex != 0 {
        b.push("key_strindex", Value::Int(kv.key_strindex as i64));
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
        Some(any_value::Value::StringValueStrindex(idx)) => {
            b.push("string_value_strindex", Value::Int(*idx as i64));
        }
    }
    b.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    #[test]
    fn coerce_u64_accepts_timestamp_value() {
        let dt: DateTime<Utc> = Utc.timestamp_opt(1_700_000_000, 123_456_789).unwrap();
        let expected = dt.timestamp_nanos_opt().unwrap() as u64;
        assert_eq!(coerce_u64(Value::Timestamp(dt)), Some(expected));
    }

    #[test]
    fn coerce_u64_rejects_pre_epoch_timestamp() {
        let dt: DateTime<Utc> = Utc.timestamp_opt(-1, 0).unwrap();
        assert_eq!(coerce_u64(Value::Timestamp(dt)), None);
    }

    #[test]
    fn log_record_carries_timestamp_nanos_on_wire() {
        let dt: DateTime<Utc> = Utc.timestamp_opt(1_700_000_000, 123_456_789).unwrap();
        let expected = dt.timestamp_nanos_opt().unwrap() as u64;
        let entries: &[(&str, Value<'_>)] = &[
            ("time_unix_nano", Value::Timestamp(dt)),
            ("observed_time_unix_nano", Value::Timestamp(dt)),
        ];
        let lr = hashlit_to_log_record(&Value::Object(entries)).unwrap();
        assert_eq!(lr.time_unix_nano, expected);
        assert_eq!(lr.observed_time_unix_nano, expected);
    }

    #[test]
    fn coerce_u64_rejects_oversized_float_instead_of_saturating() {
        // `as u64` is a saturating cast since Rust 1.45: a value
        // that satisfies the finite / non-negative / integral guards
        // but sits above the u64 range would silently encode as
        // `u64::MAX` on the wire (= year 2554+ in OTLP nanos). The
        // upper-bound guard must take it through `None` instead so
        // the warn-once + encode-0 path catches it.
        assert_eq!(coerce_u64(Value::Float(1e30)), None);

        // `u64::MAX as f64` is the saturating target itself. The
        // f64 rounds *up* to 2^64 (= one ulp past the u64 range)
        // because u64::MAX is not exactly representable; the strict
        // `<` comparison must catch that boundary so the cast does
        // not silently land at `u64::MAX`.
        assert_eq!(coerce_u64(Value::Float(u64::MAX as f64)), None);

        // Round-trip safety: a u64-fitting integral float still
        // passes through. This literal is inside the u64 range and
        // is representable as an integral f64 at this magnitude's
        // spacing, so `fract() == 0.0` and the cast is safe.
        assert_eq!(
            coerce_u64(Value::Float(1_700_000_000_000_000_000.0)),
            Some(1_700_000_000_000_000_000)
        );
    }

    #[test]
    fn timestamp_field_missing_key_is_silent_zero() {
        let warned = AtomicBool::new(false);
        let entries: &[(&str, Value<'_>)] = &[];
        assert_eq!(timestamp_u64_field(entries, "time_unix_nano", &warned), 0);
        assert!(
            !warned.load(Ordering::Relaxed),
            "missing key is legitimate; warn flag must stay clear"
        );
    }

    #[test]
    fn timestamp_field_uncoercible_warns_once_then_silent() {
        let warned = AtomicBool::new(false);
        let entries: &[(&str, Value<'_>)] = &[("time_unix_nano", Value::String("not a number"))];
        assert_eq!(timestamp_u64_field(entries, "time_unix_nano", &warned), 0);
        assert!(
            warned.load(Ordering::Relaxed),
            "first uncoercible value must flip the warn flag"
        );
        let flag_before = warned.load(Ordering::Relaxed);
        assert_eq!(timestamp_u64_field(entries, "time_unix_nano", &warned), 0);
        assert_eq!(
            warned.load(Ordering::Relaxed),
            flag_before,
            "subsequent uncoercible values must not re-flip the flag"
        );
    }

    #[test]
    fn key_strindex_out_of_i32_range_rejected() {
        // A DSL-side integer that overflows `i32` must not silently wrap
        // to a garbage strindex; `i32_field` filters it to `None` and the
        // KeyValue reaches the wire with `key_strindex = 0`.
        let entries: &[(&str, Value<'_>)] = &[
            ("key", Value::String("k")),
            ("key_strindex", Value::Int(i64::MAX)),
        ];
        let kv = hashlit_to_keyvalue(&Value::Object(entries)).unwrap();
        assert_eq!(kv.key_strindex, 0);
    }

    #[test]
    fn string_value_strindex_out_of_i32_range_rejected() {
        // The `AnyValue::StringValueStrindex` oneof arm must apply the
        // same range check as `KeyValue::key_strindex`; otherwise a DSL
        // value above `i32::MAX` would silently wrap on the wire.
        let entries: &[(&str, Value<'_>)] = &[("string_value_strindex", Value::Int(i64::MAX))];
        let av = hashlit_to_anyvalue(&Value::Object(entries)).unwrap();
        // Out-of-range → variant not set, `AnyValue.value` stays `None`.
        assert!(av.value.is_none());
    }

    #[test]
    fn string_value_strindex_in_range_round_trips() {
        let entries: &[(&str, Value<'_>)] = &[("string_value_strindex", Value::Int(42))];
        let av = hashlit_to_anyvalue(&Value::Object(entries)).unwrap();
        match av.value {
            Some(any_value::Value::StringValueStrindex(n)) => assert_eq!(n, 42),
            other => panic!("expected StringValueStrindex(42), got {:?}", other),
        }
    }

    /// OTLP 0.32 promoted `Resource.entity_refs` (`EntityRef`) to a
    /// first-class wire surface. The DSL primitive must preserve
    /// every named field on that surface across a decode/encode
    /// cycle — schema_url, type, id_keys, description_keys — or a
    /// caller that constructs an EntityRef through the DSL will
    /// silently lose the field on the wire.
    #[test]
    fn entity_ref_round_trips_all_fields_through_hashlit_form() {
        // Construct a full-shape EntityRef DSL object.
        let id_keys: &[Value<'_>] = &[Value::String("host.id"), Value::String("host.name")];
        let description_keys: &[Value<'_>] = &[Value::String("host.type")];
        let entity_entries: &[(&str, Value<'_>)] = &[
            (
                "schema_url",
                Value::String("https://opentelemetry.io/schemas/1.32.0"),
            ),
            ("type", Value::String("host")),
            ("id_keys", Value::Array(id_keys)),
            ("description_keys", Value::Array(description_keys)),
        ];

        // DSL → proto: every named field must land on the wire.
        let er = hashlit_to_entity_ref(&Value::Object(entity_entries)).unwrap();
        assert_eq!(er.schema_url, "https://opentelemetry.io/schemas/1.32.0");
        assert_eq!(er.r#type, "host");
        assert_eq!(
            er.id_keys,
            vec!["host.id".to_string(), "host.name".to_string()]
        );
        assert_eq!(er.description_keys, vec!["host.type".to_string()]);

        // proto → DSL: the same fields must come back through the
        // arena-side encoder in the object shape the DSL side reads.
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let out = entity_ref_to_hashlit(&arena, &er);
        let out_entries = match out {
            Value::Object(entries) => entries,
            other => panic!("entity_ref_to_hashlit must return Object, got {:?}", other),
        };
        let get = |k: &str| -> Value<'_> {
            out_entries
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| *v)
                .unwrap_or_else(|| panic!("EntityRef roundtrip: missing field `{}`", k))
        };
        assert!(matches!(
            get("schema_url"),
            Value::String("https://opentelemetry.io/schemas/1.32.0")
        ));
        assert!(matches!(get("type"), Value::String("host")));
        match get("id_keys") {
            Value::Array(xs) => {
                let strs: Vec<&str> = xs
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => *s,
                        other => panic!("id_keys element must be String, got {:?}", other),
                    })
                    .collect();
                assert_eq!(strs, vec!["host.id", "host.name"]);
            }
            other => panic!("id_keys must be Array, got {:?}", other),
        }
        match get("description_keys") {
            Value::Array(xs) => {
                let strs: Vec<&str> = xs
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => *s,
                        other => panic!("description_keys element must be String, got {:?}", other),
                    })
                    .collect();
                assert_eq!(strs, vec!["host.type"]);
            }
            other => panic!("description_keys must be Array, got {:?}", other),
        }
    }
}
