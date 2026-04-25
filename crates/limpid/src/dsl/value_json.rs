//! JSON boundary for the DSL [`Value`] type.
//!
//! JSON (RFC 8259 §8.1) requires strings to be valid Unicode, so raw
//! `Value::Bytes` cannot be embedded directly. We use a tagged marker
//! and round-trip-safe escape, applied symmetrically by `to_json` /
//! `from_json`:
//!
//! | Value form               | JSON form                           |
//! |--------------------------|-------------------------------------|
//! | `Value::Bytes(b)`        | `{"$bytes_b64": "<base64>"}`        |
//! | `Object` with key `$x`   | `{"$$x": ...}`  (`$` doubled)       |
//! | `Object` with key `$$y`  | `{"$$$y": ...}` (`$` count + 1)     |
//! | every other shape        | structural mirror                   |
//!
//! Decoding inverts the above: `{"$bytes_b64": "..."}` (exactly one
//! key, exactly that name) becomes `Value::Bytes`; any other
//! `$`-prefixed key has its leading `$` stripped one level. A
//! `$`-prefixed key that is *not* `$bytes_b64` and is *not* doubled
//! enough to be an escape is rejected — those forms are reserved for
//! future markers and must not slip through silently.
//!
//! The marker form is **internal** (tap `--json`, persistence).
//! User-facing primitives `to_json` / `parse_json` error on Bytes and
//! on the marker respectively, so the marker never appears in
//! pipeline-visible JSON.

use anyhow::{Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use serde_json::{Map as JsonMap, Number as JsonNumber, Value as JsonValue};

use super::value::{Map, Value};

/// Marker key that flags a `Value::Bytes` payload. See module docs.
pub const BYTES_MARKER_KEY: &str = "$bytes_b64";

// --- Value → serde_json::Value -------------------------------------------

/// Serialize a [`Value`] to `serde_json::Value` using the marker /
/// escape convention. Used by tap output, queue persistence, and any
/// other internal channel that needs a JSON wire form.
///
/// Returns `Err` only for non-finite floats (NaN / ±Inf), which JSON
/// cannot represent — callers either coerce upstream or surface the
/// error to the user.
pub fn value_to_json(v: &Value) -> Result<JsonValue> {
    match v {
        Value::Null => Ok(JsonValue::Null),
        Value::Bool(b) => Ok(JsonValue::Bool(*b)),
        Value::Int(n) => Ok(JsonValue::Number((*n).into())),
        Value::Float(n) => JsonNumber::from_f64(*n)
            .map(JsonValue::Number)
            .ok_or_else(|| anyhow!("cannot serialize non-finite float to JSON: {n}")),
        Value::String(s) => Ok(JsonValue::String(s.clone())),
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
            for item in a {
                out.push(value_to_json(item)?);
            }
            Ok(JsonValue::Array(out))
        }
        Value::Object(m) => {
            let mut out = JsonMap::new();
            for (k, val) in m {
                let key = escape_user_key(k);
                out.insert(key, value_to_json(val)?);
            }
            Ok(JsonValue::Object(out))
        }
    }
}

// --- serde_json::Value → Value -------------------------------------------

/// Deserialize a `serde_json::Value` into a DSL [`Value`], applying the
/// marker / escape convention defined in the module docs.
pub fn json_to_value(v: &JsonValue) -> Result<Value> {
    match v {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(b) => Ok(Value::Bool(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Int(i))
            } else if let Some(u) = n.as_u64() {
                // u64 outside i64 range falls through to f64 to avoid
                // a silently truncated `as i64` cast.
                if u <= i64::MAX as u64 {
                    Ok(Value::Int(u as i64))
                } else {
                    Ok(Value::Float(u as f64))
                }
            } else {
                Ok(Value::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        JsonValue::String(s) => Ok(Value::String(s.clone())),
        JsonValue::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for item in a {
                out.push(json_to_value(item)?);
            }
            Ok(Value::Array(out))
        }
        JsonValue::Object(m) => {
            // Check for the bytes marker first: exactly one key, key
            // matches BYTES_MARKER_KEY, value is a base64 string.
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
                return Ok(Value::Bytes(bytes::Bytes::from(bytes)));
            }
            let mut out = Map::new();
            for (k, val) in m {
                let key = unescape_user_key(k)?;
                out.insert(key, json_to_value(val)?);
            }
            Ok(Value::Object(out))
        }
    }
}

// --- Key escape helpers --------------------------------------------------

/// Encode-side: any user key starting with `$` is prefixed with one
/// extra `$`. `foo` stays `foo`; `$x` becomes `$$x`; `$$y` becomes
/// `$$$y`. The reserved marker key (`$bytes_b64`) never appears here
/// — `value_to_json` constructs it directly for `Value::Bytes`.
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
        // Doubled-or-more prefix: strip one level. `$$$x` → `$$x`.
        let mut out = String::with_capacity(k.len() - 1);
        out.push('$');
        out.push_str(rest);
        Ok(out)
    } else if k.starts_with('$') {
        // Single `$` and not the bytes marker (handled in caller).
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

    fn roundtrip(v: &Value) -> Value {
        json_to_value(&value_to_json(v).unwrap()).unwrap()
    }

    #[test]
    fn scalar_round_trip() {
        for v in [
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(42),
            Value::Int(-1),
            Value::Float(3.14),
            Value::String("hello".into()),
        ] {
            assert_eq!(roundtrip(&v), v, "round-trip mismatch for {v:?}");
        }
    }

    #[test]
    fn bytes_round_trip_via_marker() {
        // Decision §5–§6: marker is internal but must round-trip
        // losslessly through value_to_json / json_to_value.
        let v = Value::Bytes(Bytes::from_static(b"\x00\x01\xff\xfe"));
        let json = value_to_json(&v).unwrap();
        // shape: { "$bytes_b64": "AAH//g==" }
        assert_eq!(
            json.get(BYTES_MARKER_KEY).and_then(|x| x.as_str()),
            Some("AAH//g==")
        );
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn float_nan_errors_on_serialize() {
        let v = Value::Float(f64::NAN);
        assert!(value_to_json(&v).is_err());
    }

    #[test]
    fn user_dollar_key_is_escaped_and_unescaped() {
        // User-side `$x` must round-trip. The on-wire form is `$$x` so
        // the bytes marker remains unambiguous.
        let mut m = Map::new();
        m.insert("$weird".into(), Value::Int(1));
        m.insert("normal".into(), Value::Int(2));
        let v = Value::Object(m);
        let json = value_to_json(&v).unwrap();
        assert!(json.get("$$weird").is_some());
        assert!(json.get("$weird").is_none());
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn nested_dollar_key_escape_levels() {
        // `$$y` (already a `$`-prefixed user key from the user's PoV)
        // must escape to `$$$y` to keep the round-trip unambiguous.
        let mut m = Map::new();
        m.insert("$$y".into(), Value::Int(1));
        let v = Value::Object(m);
        let json = value_to_json(&v).unwrap();
        assert!(json.get("$$$y").is_some());
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn unknown_dollar_marker_rejected() {
        // Single-`$`-prefixed keys other than the bytes marker are
        // reserved — silently treating them as user data would let a
        // future marker collide with old data.
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
        inner.insert(
            "blob".into(),
            Value::Bytes(Bytes::from_static(b"\x00\xff")),
        );
        let mut outer = Map::new();
        outer.insert("payload".into(), Value::Object(inner));
        let v = Value::Object(outer);
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn array_round_trip() {
        let v = Value::Array(vec![
            Value::Int(1),
            Value::String("two".into()),
            Value::Bytes(Bytes::from_static(b"\x00")),
        ]);
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn integer_preserved_not_promoted_to_float() {
        // serde_json normally exposes any integer that round-trips as
        // f64; we need Int to survive the trip so type-strict primitives
        // (like FieldType::Int) don't drift.
        let v = Value::Int(1234567890);
        assert_eq!(roundtrip(&v), v);
        assert!(matches!(roundtrip(&v), Value::Int(_)));
    }
}
