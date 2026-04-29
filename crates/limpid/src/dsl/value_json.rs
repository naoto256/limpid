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

// ===========================================================================
// Direct `serde::Serialize` impl for `Value<'bump>` (hot path)
// ===========================================================================
//
// `to_json(value)` is the primary hot-path consumer of JSON encoding —
// the OCSF compose pipeline calls it once per event. Going through an
// intermediate `serde_json::Value` (the pre-Step-3 `value_view_to_json`)
// double-walked the tree (Value -> JsonValue -> bytes) and re-allocated
// every leaf string. The direct `Serialize` impl below streams into the
// caller's `Serializer` (typically `serde_json::ser::Serializer`)
// without the intermediate, dropping the `JsonValue` allocation column
// from the v0.6.0 D-pipeline flamegraph.
//
// Wire form is identical to the boundary `value_to_json` /
// `json_to_value` pair: bytes go through the `$bytes_b64` marker,
// `$`-prefixed user keys are doubled. Tests in this module pin the
// shape against `OwnedValue::view_in() -> Value<'bump>` round-trip via
// `json_to_value`, so any behavioural drift surfaces here rather than
// silently in user output.

impl<'bump> serde::Serialize for Value<'bump> {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Value::Null => serializer.serialize_unit(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Int(n) => serializer.serialize_i64(*n),
            Value::Float(n) => {
                if n.is_finite() {
                    serializer.serialize_f64(*n)
                } else {
                    Err(serde::ser::Error::custom(format!(
                        "cannot serialize non-finite float to JSON: {n}"
                    )))
                }
            }
            Value::String(s) => serializer.serialize_str(s),
            Value::Bytes(b) => {
                // Bytes marker: `{"$bytes_b64": "<base64>"}`. Same shape
                // the boundary `value_to_json` emits, so tap / replay /
                // queue persistence survive round-tripping through
                // either path.
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry(BYTES_MARKER_KEY, &B64.encode(b))?;
                map.end()
            }
            Value::Timestamp(dt) => {
                let nanos = dt.timestamp_nanos_opt().ok_or_else(|| {
                    serde::ser::Error::custom("timestamp out of i64 nanosecond range")
                })?;
                serializer.serialize_i64(nanos)
            }
            Value::Array(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items.iter() {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Object(entries) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (k, v) in entries.iter() {
                    if k.starts_with('$') {
                        // Escape: `$x` -> `$$x` so the bytes marker
                        // remains unambiguous on decode.
                        let mut escaped = String::with_capacity(k.len() + 1);
                        escaped.push('$');
                        escaped.push_str(k);
                        map.serialize_entry(escaped.as_str(), v)?;
                    } else {
                        map.serialize_entry(*k, v)?;
                    }
                }
                map.end()
            }
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
    use crate::dsl::arena::EventArena;
    use bytes::Bytes;

    fn roundtrip_owned(v: &OwnedValue) -> OwnedValue {
        json_to_value(&value_to_json(v).unwrap()).unwrap()
    }

    /// Drive the direct `Serialize` impl through `serde_json::to_string`
    /// against an arena-backed view of `owned`. Returns the JSON wire
    /// string the hot path emits.
    fn direct_to_json_string(owned: &OwnedValue) -> Result<String> {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let view = owned.view_in(&arena);
        Ok(serde_json::to_string(&view)?)
    }

    /// Pin the contract: `to_json(view) == to_json(owned)`. The
    /// boundary `value_to_json` (OwnedValue -> serde_json::Value ->
    /// string) and the hot-path direct serialize must produce
    /// byte-identical wire output, so the marker / escape / numeric
    /// conventions stay aligned.
    fn assert_direct_matches_boundary(owned: &OwnedValue) {
        let direct = direct_to_json_string(owned).expect("direct path");
        let boundary = serde_json::to_string(&value_to_json(owned).expect("boundary path"))
            .expect("boundary stringify");
        assert_eq!(
            direct, boundary,
            "direct vs boundary diverged for {owned:?}"
        );
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

    // ===== Direct `Serialize` impl for `Value<'bump>` (Step 3) =====
    //
    // The hot-path `to_json(value)` primitive bypasses the intermediate
    // `serde_json::Value` and streams `Value<'bump>` straight into the
    // serializer. These tests pin the wire shape (numeric typing,
    // bytes marker, key escape) and assert byte-identical output
    // against the boundary path so the two never silently drift.

    #[test]
    fn direct_serialize_scalar_shapes() {
        // Integers stay i64 (not "1234"-as-string); floats stay
        // numbers; booleans / null are JSON natives.
        assert_eq!(
            direct_to_json_string(&OwnedValue::Int(42)).unwrap(),
            "42"
        );
        assert_eq!(
            direct_to_json_string(&OwnedValue::Int(-1)).unwrap(),
            "-1"
        );
        assert_eq!(
            direct_to_json_string(&OwnedValue::Float(2.5)).unwrap(),
            "2.5"
        );
        assert_eq!(
            direct_to_json_string(&OwnedValue::Bool(true)).unwrap(),
            "true"
        );
        assert_eq!(
            direct_to_json_string(&OwnedValue::Null).unwrap(),
            "null"
        );
        assert_eq!(
            direct_to_json_string(&OwnedValue::String("hi".into())).unwrap(),
            "\"hi\""
        );
    }

    #[test]
    fn direct_serialize_non_finite_float_errors() {
        // JSON cannot represent NaN / ±Inf; the direct path errors at
        // serialize time rather than emitting a literal `NaN` token
        // that downstream parsers would reject.
        assert!(direct_to_json_string(&OwnedValue::Float(f64::NAN)).is_err());
        assert!(direct_to_json_string(&OwnedValue::Float(f64::INFINITY)).is_err());
        assert!(direct_to_json_string(&OwnedValue::Float(f64::NEG_INFINITY)).is_err());
    }

    #[test]
    fn direct_serialize_bytes_marker_shape() {
        // `Value::Bytes(b)` -> `{"$bytes_b64": "<base64>"}` — same wire
        // shape the boundary `value_to_json` produces, so tap / queue
        // / replay survive flowing through either path.
        let v = OwnedValue::Bytes(Bytes::from_static(b"\x00\x01\xff\xfe"));
        let s = direct_to_json_string(&v).unwrap();
        assert_eq!(s, r#"{"$bytes_b64":"AAH//g=="}"#);
        assert_direct_matches_boundary(&v);
    }

    #[test]
    fn direct_serialize_dollar_key_escape() {
        // User-side `$weird` -> wire `$$weird`. Doubled keys (`$$y`)
        // become `$$$y`. Keeps the bytes marker unambiguous on decode.
        let mut m = Map::new();
        m.insert("$weird".into(), OwnedValue::Int(1));
        m.insert("$$y".into(), OwnedValue::Int(2));
        m.insert("normal".into(), OwnedValue::Int(3));
        let v = OwnedValue::Object(m);
        let s = direct_to_json_string(&v).unwrap();
        // Insertion order preserved (IndexMap on the Owned side, frozen
        // slice on the borrowed side both keep order); $-prefix gets one
        // extra $.
        assert_eq!(s, r#"{"$$weird":1,"$$$y":2,"normal":3}"#);
        assert_direct_matches_boundary(&v);
    }

    #[test]
    fn direct_serialize_nested_object_and_array() {
        // Deeper structure: nested object inside array inside object.
        // Ensures the recursive `Serialize` impl doesn't lose order /
        // type information at any level.
        let mut inner = Map::new();
        inner.insert("a".into(), OwnedValue::Int(1));
        inner.insert("b".into(), OwnedValue::String("x".into()));
        let arr = OwnedValue::Array(vec![
            OwnedValue::Int(1),
            OwnedValue::Object(inner),
            OwnedValue::Null,
        ]);
        let mut outer = Map::new();
        outer.insert("items".into(), arr);
        let v = OwnedValue::Object(outer);
        let s = direct_to_json_string(&v).unwrap();
        assert_eq!(s, r#"{"items":[1,{"a":1,"b":"x"},null]}"#);
        assert_direct_matches_boundary(&v);
    }

    #[test]
    fn direct_serialize_bytes_nested_inside_object_uses_marker() {
        // Bytes deep inside the tree must still surface via marker so
        // the wire form stays self-describing for the round-trip path.
        let mut inner = Map::new();
        inner.insert("blob".into(), OwnedValue::Bytes(Bytes::from_static(b"\x00\xff")));
        let mut outer = Map::new();
        outer.insert("payload".into(), OwnedValue::Object(inner));
        let v = OwnedValue::Object(outer);
        let s = direct_to_json_string(&v).unwrap();
        assert_eq!(s, r#"{"payload":{"blob":{"$bytes_b64":"AP8="}}}"#);
        assert_direct_matches_boundary(&v);
    }

    #[test]
    fn direct_serialize_timestamp_emits_unix_nanos() {
        // Wire form is unix nanoseconds (i64), matching OTLP
        // `time_unix_nano` and the boundary `value_to_json`. Pin the
        // numeric encoding so a future drift to RFC3339-as-string
        // (which tap UI sometimes asks for) shows up here.
        let dt = chrono::DateTime::parse_from_rfc3339("2026-04-15T10:30:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let v = OwnedValue::Timestamp(dt);
        let s = direct_to_json_string(&v).unwrap();
        assert_eq!(s, "1776249000000000000");
        assert_direct_matches_boundary(&v);
    }

    #[test]
    fn direct_serialize_round_trip_through_json_to_value() {
        // End-to-end: serialize a Value<'bump> with the direct path,
        // parse the JSON back through `json_to_value`, and require the
        // OwnedValue equivalent. This is the contract `to_json(x)` /
        // `parse_json(s)` users rely on.
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);

        let mut inner = Map::new();
        inner.insert("k".into(), OwnedValue::Int(7));
        let owned_in = OwnedValue::Object(
            [(
                "wrap".to_string(),
                OwnedValue::Array(vec![OwnedValue::Object(inner), OwnedValue::Bool(false)]),
            )]
            .into_iter()
            .collect(),
        );
        let view = owned_in.view_in(&arena);
        let s = serde_json::to_string(&view).unwrap();
        let parsed: JsonValue = serde_json::from_str(&s).unwrap();
        let owned_out = json_to_value(&parsed).unwrap();
        assert_eq!(owned_out, owned_in);
    }
}
