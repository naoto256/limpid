//! `len(x)` — cardinality primitive.
//!
//! Arrays are positionless collections in the DSL (see
//! `docs/src/processing/user-defined.md`), so "how many" is the only
//! length question the DSL user ever asks. `len` extends the same
//! cardinality idea to `String` (number of characters), `Bytes` (number
//! of bytes), and `Object` (number of top-level keys) so callers have
//! one primitive for every "count this" case.
//!
//! Behaviour:
//! * `Array` → number of elements (i64).
//! * `String` → number of Unicode *characters* (not bytes). `chars()`
//!   counts scalar values, which is what users expect for log lines
//!   mixing ASCII and multi-byte UTF-8.
//! * `Bytes` → byte length.
//! * `Object` → number of top-level keys.
//! * `Null` → `Null` (pass-through, same partial-data convention as
//!   `to_int`, `regex_extract`, `table_lookup`).
//! * Any scalar (`Int` / `Float` / `Bool`) → `Null`. These have no
//!   meaningful length and returning `0` or erroring would both
//!   surprise; `Null` is the consistent "not applicable" signal.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "len",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Int),
        |args, _event| Ok(measure(&args[0])),
    );
}

fn measure(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Array(items) => Value::Int(items.len() as i64),
        Value::String(s) => Value::Int(s.chars().count() as i64),
        Value::Bytes(b) => Value::Int(b.len() as i64),
        Value::Object(m) => Value::Int(m.len() as i64),
        Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Timestamp(_) => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::value::Map;
    use bytes::Bytes;

    fn n(i: i64) -> Value {
        Value::Int(i)
    }

    fn arr(items: Vec<Value>) -> Value {
        Value::Array(items)
    }

    #[test]
    fn counts_array_elements() {
        assert_eq!(measure(&Value::Array(vec![])), n(0));
        assert_eq!(measure(&arr(vec![n(1), n(2), n(3)])), n(3));
        assert_eq!(
            measure(&arr(vec![
                Value::Null,
                Value::String("x".into()),
                arr(vec![n(1), n(2)]),
            ])),
            n(3)
        );
    }

    #[test]
    fn counts_string_characters_not_bytes() {
        assert_eq!(measure(&Value::String("".into())), n(0));
        assert_eq!(measure(&Value::String("abc".into())), n(3));
        // Multi-byte UTF-8: each scalar counts once.
        assert_eq!(measure(&Value::String("日本語".into())), n(3));
        assert_eq!(measure(&Value::String("ascii + 日本".into())), n(10));
    }

    #[test]
    fn counts_bytes_length() {
        assert_eq!(measure(&Value::Bytes(Bytes::new())), n(0));
        assert_eq!(measure(&Value::Bytes(Bytes::from_static(b"\x00\xff\x10"))), n(3));
    }

    #[test]
    fn counts_object_keys() {
        assert_eq!(measure(&Value::Object(Map::new())), n(0));
        let mut m = Map::new();
        m.insert("a".into(), n(1));
        m.insert("b".into(), n(2));
        assert_eq!(measure(&Value::Object(m)), n(2));
        // Nested values are not recursed into.
        let mut inner = Map::new();
        inner.insert("a".into(), n(1));
        inner.insert("b".into(), n(2));
        inner.insert("c".into(), n(3));
        let mut outer = Map::new();
        outer.insert("outer".into(), Value::Object(inner));
        assert_eq!(measure(&Value::Object(outer)), n(1));
    }

    #[test]
    fn null_passes_through() {
        assert_eq!(measure(&Value::Null), Value::Null);
    }

    #[test]
    fn scalars_return_null() {
        assert_eq!(measure(&Value::Int(42)), Value::Null);
        assert_eq!(measure(&Value::Float(3.14)), Value::Null);
        assert_eq!(measure(&Value::Bool(true)), Value::Null);
        assert_eq!(measure(&Value::Bool(false)), Value::Null);
    }
}
