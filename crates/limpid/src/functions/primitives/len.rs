//! `len(x)` — cardinality primitive.
//!
//! Arrays are positionless collections in the DSL (see
//! `docs/src/processing/user-defined.md`), so "how many" is the only
//! length question the DSL user ever asks. `len` extends the same
//! cardinality idea to `String` (number of characters) and `Object`
//! (number of top-level keys) so callers have one primitive for every
//! "count this" case instead of juggling three.
//!
//! Behaviour:
//! * `Array` → number of elements (i64).
//! * `String` → number of Unicode *characters* (not bytes). `chars()`
//!   counts scalar values, which is what users expect for log lines
//!   mixing ASCII and multi-byte UTF-8.
//! * `Object` → number of top-level keys.
//! * `Null` → `Null` (pass-through, same partial-data convention as
//!   `to_int`, `regex_extract`, `table_lookup`).
//! * Any scalar (`Int` / `Float` / `Bool`) → `Null`. These have no
//!   meaningful length and returning `0` or erroring would both
//!   surprise; `Null` is the consistent "not applicable" signal.

use anyhow::bail;
use serde_json::Value;

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "len",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Int),
        |args, _event| {
            if args.len() != 1 {
                bail!("len() expects 1 argument");
            }
            Ok(measure(&args[0]))
        },
    );
}

fn measure(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Array(items) => Value::Number(serde_json::Number::from(items.len() as i64)),
        Value::String(s) => Value::Number(serde_json::Number::from(s.chars().count() as i64)),
        Value::Object(m) => Value::Number(serde_json::Number::from(m.len() as i64)),
        Value::Bool(_) | Value::Number(_) => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn n(i: i64) -> Value {
        Value::Number(serde_json::Number::from(i))
    }

    #[test]
    fn counts_array_elements() {
        assert_eq!(measure(&json!([])), n(0));
        assert_eq!(measure(&json!([1, 2, 3])), n(3));
        assert_eq!(measure(&json!([null, "x", [1, 2]])), n(3));
    }

    #[test]
    fn counts_string_characters_not_bytes() {
        assert_eq!(measure(&json!("")), n(0));
        assert_eq!(measure(&json!("abc")), n(3));
        // Multi-byte UTF-8: each scalar counts once.
        assert_eq!(measure(&json!("日本語")), n(3));
        assert_eq!(measure(&json!("ascii + 日本")), n(10));
    }

    #[test]
    fn counts_object_keys() {
        assert_eq!(measure(&json!({})), n(0));
        assert_eq!(measure(&json!({"a": 1, "b": 2})), n(2));
        // Nested values are not recursed into.
        assert_eq!(measure(&json!({"outer": {"a": 1, "b": 2, "c": 3}})), n(1));
    }

    #[test]
    fn null_passes_through() {
        assert_eq!(measure(&Value::Null), Value::Null);
    }

    #[test]
    fn scalars_return_null() {
        assert_eq!(measure(&json!(42)), Value::Null);
        assert_eq!(measure(&json!(3.14)), Value::Null);
        assert_eq!(measure(&json!(true)), Value::Null);
        assert_eq!(measure(&json!(false)), Value::Null);
    }
}
