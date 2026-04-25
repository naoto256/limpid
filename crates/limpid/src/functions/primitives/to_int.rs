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
//! * `Float` → truncated to `i64` (no rounding, no overflow check other
//!   than what serde_json's `as_i64` performs).
//! * `String` → `str::parse::<i64>` result, trimmed; also accepts a
//!   numeric prefix for robustness (`"54321"` → `54321`, `" 42 "` → `42`).
//! * `Bool` → `1` or `0`.
//! * `Null` → `Null` pass-through (`to_int(null) = null`).
//! * Anything non-parseable (`"abc"`, arrays, objects) → `Null`.
//!
//! The "return `Null` on failure rather than error" policy matches
//! `regex_extract` / `table_lookup` / other primitives that model
//! partial data. Callers who want a hard failure can compare the result
//! against `null` explicitly.

use serde_json::Value;

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_int",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Int),
        |args, _event| Ok(coerce(&args[0])),
    );
}

fn coerce(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Bool(b) => Value::Number(serde_json::Number::from(if *b { 1i64 } else { 0 })),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(serde_json::Number::from(i))
            } else if let Some(f) = n.as_f64() {
                // truncate toward zero, matching C / Rust `as i64` semantics
                Value::Number(serde_json::Number::from(f as i64))
            } else {
                Value::Null
            }
        }
        Value::String(s) => {
            let t = s.trim();
            match t.parse::<i64>() {
                Ok(i) => Value::Number(serde_json::Number::from(i)),
                Err(_) => Value::Null,
            }
        }
        Value::Array(_) | Value::Object(_) => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(i: i64) -> Value {
        Value::Number(serde_json::Number::from(i))
    }

    #[test]
    fn string_numeric() {
        assert_eq!(coerce(&Value::String("54321".into())), n(54321));
        assert_eq!(coerce(&Value::String(" 42 ".into())), n(42));
        assert_eq!(coerce(&Value::String("-7".into())), n(-7));
    }

    #[test]
    fn int_passthrough() {
        assert_eq!(coerce(&n(123)), n(123));
    }

    #[test]
    fn float_truncates() {
        let f = Value::Number(serde_json::Number::from_f64(3.7).unwrap());
        assert_eq!(coerce(&f), n(3));
        let neg = Value::Number(serde_json::Number::from_f64(-3.7).unwrap());
        assert_eq!(coerce(&neg), n(-3));
    }

    #[test]
    fn bool_maps_to_one_zero() {
        assert_eq!(coerce(&Value::Bool(true)), n(1));
        assert_eq!(coerce(&Value::Bool(false)), n(0));
    }

    #[test]
    fn null_passthrough() {
        assert_eq!(coerce(&Value::Null), Value::Null);
    }

    #[test]
    fn unparseable_returns_null() {
        assert_eq!(coerce(&Value::String("abc".into())), Value::Null);
        assert_eq!(coerce(&Value::String("".into())), Value::Null);
        assert_eq!(coerce(&Value::Array(vec![])), Value::Null);
        assert_eq!(coerce(&Value::Object(serde_json::Map::new())), Value::Null);
    }
}
