//! JSON boundary for the DSL value types.
//!
//! JSON (RFC 8259 §8.1) requires strings to be valid Unicode, so raw
//! `OwnedValue::Bytes` cannot be embedded directly. We use a tagged
//! marker and round-trip-safe escape, applied symmetrically by
//! [`value_to_json`] / [`json_to_value`]:
//!
//! | Value form                    | JSON form                           |
//! |-------------------------------|-------------------------------------|
//! | `OwnedValue::Bytes(b)`        | `{"$bytes_b64": "<base64>"}`        |
//! | `Object` with key `$x`        | `{"$$x": ...}`  (`$` doubled)       |
//! | `Object` with key `$$y`       | `{"$$$y": ...}` (`$` count + 1)     |
//! | every other shape             | structural mirror                   |
//!
//! Decoding inverts the above: `{"$bytes_b64": "..."}` (exactly one
//! key, exactly that name) becomes `OwnedValue::Bytes`; any other
//! `$`-prefixed key has its leading `$` stripped one level. A
//! `$`-prefixed key that is *not* `$bytes_b64` and is *not* doubled
//! enough to be an escape is rejected — those forms are reserved for
//! future markers and must not slip through silently.
//!
//! The marker form is **internal** (tap `--json`, persistence).
//! User-facing primitives `to_json` / `parse_json` error on Bytes and
//! on the marker respectively, so the marker never appears in
//! pipeline-visible JSON.
//!
//! ## Owned vs arena-backed
//!
//! The boundary helpers operate on [`OwnedValue`] only. Pipeline-internal
//! parsers (e.g. `primitives::parse_json`) build a [`Value<'bump>`]
//! directly into the per-event arena via [`json_to_value_in`] — going
//! through `OwnedValue` first would defeat the per-event allocator.

use anyhow::{Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::arena::EventArena;
use super::value::{Map, ObjectBuilder, OwnedValue, Value};

/// Marker key that flags a `Bytes` payload. See module docs.
pub const BYTES_MARKER_KEY: &str = "$bytes_b64";

// ===========================================================================
// OwnedValue ⇄ serde_json::Value (boundary)
// ===========================================================================

/// Serialise an [`OwnedValue`] to `serde_json::Value` using the marker /
/// escape convention. Used by tap output, queue persistence, and any
/// other internal channel that needs a JSON wire form.
///
/// Returns `Err` only for non-finite floats (NaN / ±Inf), which JSON
/// cannot represent — callers either coerce upstream or surface the
/// error to the user.
pub fn value_to_json(v: &OwnedValue) -> Result<JsonValue> {
    match v {
        OwnedValue::Null => Ok(JsonValue::Null),
        OwnedValue::Bool(b) => Ok(JsonValue::Bool(*b)),
        OwnedValue::Int(n) => Ok(JsonValue::Number((*n).into())),
        OwnedValue::Float(n) => JsonNumber::from_f64(*n)
            .map(JsonValue::Number)
            .ok_or_else(|| anyhow!("cannot serialize non-finite float to JSON: {n}")),
        OwnedValue::String(s) => Ok(JsonValue::String(s.clone())),
        // Wire form for timestamps is unix nanoseconds (i64) — matches
        // OTLP `time_unix_nano`, lossless round-trip vs RFC3339, no
        // timezone ambiguity. RFC3339 remains the DSL Display form.
        OwnedValue::Timestamp(dt) => {
            let nanos = dt
                .timestamp_nanos_opt()
                .ok_or_else(|| anyhow!("timestamp out of i64 nanosecond range"))?;
            Ok(JsonValue::Number(nanos.into()))
        }
        OwnedValue::Bytes(b) => {
            let mut m = JsonMap::new();
            m.insert(
                BYTES_MARKER_KEY.to_string(),
                JsonValue::String(B64.encode(b)),
            );
            Ok(JsonValue::Object(m))
        }
        OwnedValue::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for item in a {
                out.push(value_to_json(item)?);
            }
            Ok(JsonValue::Array(out))
        }
        OwnedValue::Object(m) => {
            let mut out = JsonMap::new();
            for (k, val) in m {
                let key = escape_user_key(k);
                out.insert(key, value_to_json(val)?);
            }
            Ok(JsonValue::Object(out))
        }
    }
}

/// Deserialise a `serde_json::Value` into an [`OwnedValue`], applying
/// the marker / escape convention defined in the module docs.
pub fn json_to_value(v: &JsonValue) -> Result<OwnedValue> {
    match v {
        JsonValue::Null => Ok(OwnedValue::Null),
        JsonValue::Bool(b) => Ok(OwnedValue::Bool(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(OwnedValue::Int(i))
            } else if let Some(u) = n.as_u64() {
                if u <= i64::MAX as u64 {
                    Ok(OwnedValue::Int(u as i64))
                } else {
                    Ok(OwnedValue::Float(u as f64))
                }
            } else {
                Ok(OwnedValue::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        JsonValue::String(s) => Ok(OwnedValue::String(s.clone())),
        JsonValue::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for item in a {
                out.push(json_to_value(item)?);
            }
            Ok(OwnedValue::Array(out))
        }
        JsonValue::Object(m) => {
            // Bytes marker first: exactly one key, exactly the reserved
            // name, value is a base64 string.
            if m.len() == 1
                && let Some(payload) = m.get(BYTES_MARKER_KEY)
            {
                let s = payload.as_str().ok_or_else(|| {
                    anyhow!(
                        "{BYTES_MARKER_KEY} marker must hold a string, got {}",
                        json_shape(payload)
                    )
                })?;
                let bytes = B64
                    .decode(s)
                    .map_err(|e| anyhow!("invalid base64 in {BYTES_MARKER_KEY}: {e}"))?;
                return Ok(OwnedValue::Bytes(bytes::Bytes::from(bytes)));
            }
            let mut out = Map::new();
            for (k, val) in m {
                let key = unescape_user_key(k)?;
                out.insert(key, json_to_value(val)?);
            }
            Ok(OwnedValue::Object(out))
        }
    }
}

// ===========================================================================
// serde_json::Value → Value<'bump> (arena-backed, hot path)
// ===========================================================================

/// Build a [`Value<'bump>`] directly from `serde_json::Value`, allocating
/// strings, arrays, and object entries into `arena` rather than going
/// through [`OwnedValue`] first. Used by `parse_json` and the inject
/// replay path so the per-event arena holds the parsed tree from the
/// start.
///
/// Applies the same `$bytes_b64` marker recognition as [`json_to_value`].
pub fn json_to_value_in<'bump>(
    v: &JsonValue,
    arena: &EventArena<'bump>,
) -> Result<Value<'bump>> {
    match v {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(b) => Ok(Value::Bool(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Int(i))
            } else if let Some(u) = n.as_u64() {
                if u <= i64::MAX as u64 {
                    Ok(Value::Int(u as i64))
                } else {
                    Ok(Value::Float(u as f64))
                }
            } else {
                Ok(Value::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        JsonValue::String(s) => Ok(Value::String(arena.alloc_str(s))),
        JsonValue::Array(a) => {
            let mut out =
                bumpalo::collections::Vec::with_capacity_in(a.len(), arena.bump());
            for item in a {
                out.push(json_to_value_in(item, arena)?);
            }
            Ok(Value::Array(out.into_bump_slice()))
        }
        JsonValue::Object(m) => {
            // Bytes marker check — same shape rule as `json_to_value`.
            if m.len() == 1
                && let Some(payload) = m.get(BYTES_MARKER_KEY)
            {
                let s = payload.as_str().ok_or_else(|| {
                    anyhow!(
                        "{BYTES_MARKER_KEY} marker must hold a string, got {}",
                        json_shape(payload)
                    )
                })?;
                let decoded = B64
                    .decode(s)
                    .map_err(|e| anyhow!("invalid base64 in {BYTES_MARKER_KEY}: {e}"))?;
                return Ok(Value::Bytes(arena.alloc_bytes(&decoded)));
            }
            let mut builder = ObjectBuilder::with_capacity(arena, m.len());
            for (k, val) in m {
                let key = unescape_user_key(k)?;
                let key_in = arena.alloc_str(&key);
                builder.push(key_in, json_to_value_in(val, arena)?);
            }
            Ok(builder.finish())
        }
    }
}

/// Serialise an arena-backed [`Value<'bump>`] to `serde_json::Value`.
/// Use sparingly on hot paths — the result is heap-allocated. Step 3
/// of the v0.6.0 perf milestone replaces this with a direct
/// `serde::Serialize` impl that streams into a `serde_json::Serializer`
/// without the intermediate `JsonValue`.
pub fn value_view_to_json(v: &Value<'_>) -> Result<JsonValue> {
    match v {
        Value::Null => Ok(JsonValue::Null),
        Value::Bool(b) => Ok(JsonValue::Bool(*b)),
        Value::Int(n) => Ok(JsonValue::Number((*n).into())),
        Value::Float(n) => JsonNumber::from_f64(*n)
            .map(JsonValue::Number)
            .ok_or_else(|| anyhow!("cannot serialize non-finite float to JSON: {n}")),
        Value::String(s) => Ok(JsonValue::String((*s).to_string())),
        Value::Timestamp(dt) => {
            let nanos = dt
                .timestamp_nanos_opt()
                .ok_or_else(|| anyhow!("timestamp out of i64 nanosecond range"))?;
            Ok(JsonValue::Number(nanos.into()))
        }
        Value::Bytes(b) => {
            let mut m = JsonMap::new();
            m.insert(
                BYTES_MARKER_KEY.to_string(),
                JsonValue::String(B64.encode(b)),
            );
            Ok(JsonValue::Object(m))
        }
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for item in a.iter() {
                out.push(value_view_to_json(item)?);
            }
            Ok(JsonValue::Array(out))
        }
        Value::Object(entries) => {
            let mut out = JsonMap::new();
            for (k, val) in entries.iter() {
                let key = escape_user_key(k);
                out.insert(key, value_view_to_json(val)?);
            }
            Ok(JsonValue::Object(out))
        }
    }
}

// ===========================================================================
// Key escape helpers
// ===========================================================================

/// Encode-side: any user key starting with `$` is prefixed with one
/// extra `$`. `foo` stays `foo`; `$x` becomes `$$x`; `$$y` becomes
/// `$$$y`. The reserved marker key (`$bytes_b64`) never appears here
/// — `value_to_json` constructs it directly for `OwnedValue::Bytes`.
fn escape_user_key(k: &str) -> String {
    if k.starts_with('$') {
        let mut out = String::with_capacity(k.len() + 1);
        out.push('$');
        out.push_str(k);
        out
    } else {
        k.to_string()
    }
}

/// Decode-side inverse of [`escape_user_key`]. A double-or-more `$`
/// prefix has one `$` stripped to recover the user's original key.
/// A single `$` prefix that is *not* a recognised marker is rejected:
/// such keys are reserved for future internal use and must not be
/// interpreted as user data.
fn unescape_user_key(k: &str) -> Result<String> {
    if let Some(rest) = k.strip_prefix("$$") {
        let mut out = String::with_capacity(k.len() - 1);
        out.push('$');
        out.push_str(rest);
        Ok(out)
    } else if k.starts_with('$') {
        bail!(
            "unknown reserved key in JSON object: {k:?} \
             (single-`$`-prefixed keys are reserved for limpid internal markers; \
             user keys starting with `$` must be doubled in JSON form)"
        )
    } else {
        Ok(k.to_string())
    }
}

fn json_shape(v: &JsonValue) -> &'static str {
    match v {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "bool",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn roundtrip_owned(v: &OwnedValue) -> OwnedValue {
        json_to_value(&value_to_json(v).unwrap()).unwrap()
    }

    #[test]
    fn scalar_round_trip() {
        for v in [
            OwnedValue::Null,
            OwnedValue::Bool(true),
            OwnedValue::Bool(false),
            OwnedValue::Int(42),
            OwnedValue::Int(-1),
            OwnedValue::Float(2.5),
            OwnedValue::String("hello".into()),
        ] {
            assert_eq!(roundtrip_owned(&v), v, "round-trip mismatch for {v:?}");
        }
    }

    #[test]
    fn bytes_round_trip_via_marker() {
        let v = OwnedValue::Bytes(Bytes::from_static(b"\x00\x01\xff\xfe"));
        let json = value_to_json(&v).unwrap();
        assert_eq!(
            json.get(BYTES_MARKER_KEY).and_then(|x| x.as_str()),
            Some("AAH//g==")
        );
        assert_eq!(roundtrip_owned(&v), v);
    }

    #[test]
    fn float_nan_errors_on_serialize() {
        let v = OwnedValue::Float(f64::NAN);
        assert!(value_to_json(&v).is_err());
    }

    #[test]
    fn user_dollar_key_is_escaped_and_unescaped() {
        let mut m = Map::new();
        m.insert("$weird".into(), OwnedValue::Int(1));
        m.insert("normal".into(), OwnedValue::Int(2));
        let v = OwnedValue::Object(m);
        let json = value_to_json(&v).unwrap();
        assert!(json.get("$$weird").is_some());
        assert!(json.get("$weird").is_none());
        assert_eq!(roundtrip_owned(&v), v);
    }

    #[test]
    fn nested_dollar_key_escape_levels() {
        let mut m = Map::new();
        m.insert("$$y".into(), OwnedValue::Int(1));
        let v = OwnedValue::Object(m);
        let json = value_to_json(&v).unwrap();
        assert!(json.get("$$$y").is_some());
        assert_eq!(roundtrip_owned(&v), v);
    }

    #[test]
    fn unknown_dollar_marker_rejected() {
        let json: JsonValue = serde_json::from_str(r#"{"$unknown_marker":1}"#).unwrap();
        let err = json_to_value(&json).unwrap_err();
        assert!(
            err.to_string().contains("reserved"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bytes_marker_with_wrong_shape_errors() {
        let json: JsonValue = serde_json::from_str(r#"{"$bytes_b64":42}"#).unwrap();
        assert!(json_to_value(&json).is_err());
        let json: JsonValue = serde_json::from_str(r#"{"$bytes_b64":"!!!not-base64!!!"}"#).unwrap();
        assert!(json_to_value(&json).is_err());
    }

    #[test]
    fn nested_object_with_bytes_inside() {
        let mut inner = Map::new();
        inner.insert("blob".into(), OwnedValue::Bytes(Bytes::from_static(b"\x00\xff")));
        let mut outer = Map::new();
        outer.insert("payload".into(), OwnedValue::Object(inner));
        let v = OwnedValue::Object(outer);
        assert_eq!(roundtrip_owned(&v), v);
    }

    #[test]
    fn array_round_trip() {
        let v = OwnedValue::Array(vec![
            OwnedValue::Int(1),
            OwnedValue::String("two".into()),
            OwnedValue::Bytes(Bytes::from_static(b"\x00")),
        ]);
        assert_eq!(roundtrip_owned(&v), v);
    }

    #[test]
    fn integer_preserved_not_promoted_to_float() {
        let v = OwnedValue::Int(1234567890);
        assert_eq!(roundtrip_owned(&v), v);
        assert!(matches!(roundtrip_owned(&v), OwnedValue::Int(_)));
    }

    #[test]
    fn arena_view_round_trip_via_owned() {
        // `json_to_value_in` builds straight into the arena; the result
        // must agree structurally with the OwnedValue path (after
        // converting back to OwnedValue for comparison).
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let json: JsonValue =
            serde_json::from_str(r#"{"a":1,"b":"hi","c":[1,2,3]}"#).unwrap();
        let view = json_to_value_in(&json, &arena).unwrap();
        let owned = view.to_owned_value();
        let from_owned = json_to_value(&json).unwrap();
        assert_eq!(owned, from_owned);
    }
}
