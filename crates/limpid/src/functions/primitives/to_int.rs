//! `to_int(x)` — coerce a value to a 64-bit signed integer.
//!
//! Motivation: CEF extension values and CSV column values arrive as
//! strings even when they carry numeric content (ports, byte counts,
//! session IDs). OCSF schemas often require `Integer` for the same
//! fields. Rather than writing ad-hoc string→int gymnastics in every
//! parser snippet, `to_int` is the schema-agnostic cast.
//!
//! Behaviour:
//! * `Int` → itself (pass-through).
//! * `Float` → truncated toward zero via `as i64` (matches C / Rust
//!   cast semantics; saturating on overflow).
//! * `String` → `str::parse::<i64>` result, trimmed
//!   (`"54321"` → `54321`, `" 42 "` → `42`).
//! * `Bool` → `1` or `0`.
//! * `Timestamp` → unix nanoseconds (`i64`), matching OTLP
//!   `time_unix_nano`. So `to_int(received_at)` is the natural way to
//!   get a numeric epoch value.
//! * `Null` → `Null` pass-through (`to_int(null) = null`).
//! * `Bytes` → error (no standard numeric interpretation; convert via
//!   `to_string()` first when the bytes are decimal text).
//! * Anything non-parseable (`"abc"`, arrays, objects) → `Null`.
//!
//! The "return `Null` on failure rather than error" policy matches
//! `regex_extract` / `table_lookup` / other primitives that model
//! partial data. Callers who want a hard failure can compare the result
//! against `null` explicitly.

use anyhow::{Result, bail};

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_int",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Int),
        |args, _event| coerce(&args[0]),
    );
}

fn coerce(v: &Value) -> Result<Value> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Bool(b) => Value::Int(if *b { 1 } else { 0 }),
        Value::Int(n) => Value::Int(*n),
        Value::Float(f) => {
            // truncate toward zero, matching C / Rust `as i64` semantics
            Value::Int(*f as i64)
        }
        Value::String(s) => {
            let t = s.trim();
            match t.parse::<i64>() {
                Ok(i) => Value::Int(i),
                Err(_) => Value::Null,
            }
        }
        // Per Bytes design §17: bytes have no standard numeric
        // interpretation. User must convert explicitly via to_string().
        Value::Bytes(_) => bail!("to_int() does not accept bytes (use to_string() first)"),
        // Timestamps are nanos under the hood (matches OTLP); coerce to
        // i64 unix nanoseconds so `to_int(received_at)` is the natural
        // way to get a numeric epoch value.
        Value::Timestamp(dt) => Value::Int(dt.timestamp_nanos_opt().unwrap_or(0)),
        Value::Array(_) | Value::Object(_) => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::value::Map;
    use bytes::Bytes;

    fn n(i: i64) -> Value {
        Value::Int(i)
    }

    fn coerce_ok(v: &Value) -> Value {
        coerce(v).expect("coerce should not error in this test")
    }

    #[test]
    fn string_numeric() {
        assert_eq!(coerce_ok(&Value::String("54321".into())), n(54321));
        assert_eq!(coerce_ok(&Value::String(" 42 ".into())), n(42));
        assert_eq!(coerce_ok(&Value::String("-7".into())), n(-7));
    }

    #[test]
    fn int_passthrough() {
        assert_eq!(coerce_ok(&n(123)), n(123));
    }

    #[test]
    fn float_truncates() {
        let f = Value::Float(3.7);
        assert_eq!(coerce_ok(&f), n(3));
        let neg = Value::Float(-3.7);
        assert_eq!(coerce_ok(&neg), n(-3));
    }

    #[test]
    fn bool_maps_to_one_zero() {
        assert_eq!(coerce_ok(&Value::Bool(true)), n(1));
        assert_eq!(coerce_ok(&Value::Bool(false)), n(0));
    }

    #[test]
    fn null_passthrough() {
        assert_eq!(coerce_ok(&Value::Null), Value::Null);
    }

    #[test]
    fn unparseable_returns_null() {
        assert_eq!(coerce_ok(&Value::String("abc".into())), Value::Null);
        assert_eq!(coerce_ok(&Value::String("".into())), Value::Null);
        assert_eq!(coerce_ok(&Value::Array(vec![])), Value::Null);
        assert_eq!(coerce_ok(&Value::Object(Map::new())), Value::Null);
    }

    #[test]
    fn bytes_errors() {
        // Decision §17: bytes carry no numeric interpretation, must be
        // converted explicitly via `to_string()` first.
        let err = coerce(&Value::Bytes(Bytes::from_static(b"123"))).unwrap_err();
        assert!(err.to_string().contains("to_int"));
    }
}
