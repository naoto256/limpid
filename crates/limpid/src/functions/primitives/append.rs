//! `append(arr, v)` — return a new array with `v` added at the end.
//!
//! In limpid's positionless-collection model, "add to the back" is one
//! of only two mutation operations exposed to the DSL (the other is
//! `prepend`). Neither refers to a numeric index, so both survive
//! insert/delete elsewhere without users needing to track "where" an
//! element is.
//!
//! Behaviour:
//! * `arr` is an Array → returns a new Array with `v` appended. The
//!   input is not mutated (callers re-bind via
//!   `workspace.x = append(workspace.x, v)`).
//! * `arr` is `Null` → returns `Null`. Partial-data convention; matches
//!   what other primitives do on missing inputs, and lets callers
//!   pipeline through optional fields without special-casing.
//! * Any other non-array input (`String`, `Object`, scalars) → `Null`.
//!   Appending to a non-array is a programmer error, but the runtime
//!   treats it as "nothing to append to" so a mis-routed bare-statement
//!   call doesn't crash the pipeline; the analyzer is responsible for
//!   flagging the shape mismatch.
//! * `v` is any value, including `Null` — if the caller wants to record
//!   "a slot with no value", that's a legitimate array element.

use serde_json::Value;

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "append",
        FunctionSig::fixed(&[FieldType::Array, FieldType::Any], FieldType::Array),
        |args, _event| Ok(push_back(&args[0], &args[1])),
    );
}

fn push_back(arr: &Value, v: &Value) -> Value {
    match arr {
        Value::Array(items) => {
            let mut out = items.clone();
            out.push(v.clone());
            Value::Array(out)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_to_non_empty_array() {
        assert_eq!(push_back(&json!([1, 2]), &json!(3)), json!([1, 2, 3]));
    }

    #[test]
    fn appends_to_empty_array() {
        assert_eq!(push_back(&json!([]), &json!("first")), json!(["first"]));
    }

    #[test]
    fn original_array_unchanged() {
        // The function returns a fresh array; callers must re-bind to
        // see the change. This test proves no mutation through the
        // cloned input.
        let original = json!([1, 2]);
        let result = push_back(&original, &json!(3));
        assert_eq!(original, json!([1, 2]));
        assert_eq!(result, json!([1, 2, 3]));
    }

    #[test]
    fn appending_any_value_type() {
        assert_eq!(push_back(&json!([]), &json!(null)), json!([null]));
        assert_eq!(push_back(&json!([]), &json!({"k": 1})), json!([{"k": 1}]));
        assert_eq!(push_back(&json!([]), &json!([1, 2])), json!([[1, 2]]));
    }

    #[test]
    fn null_array_returns_null() {
        assert_eq!(push_back(&Value::Null, &json!(1)), Value::Null);
    }

    #[test]
    fn non_array_input_returns_null() {
        assert_eq!(push_back(&json!("string"), &json!(1)), Value::Null);
        assert_eq!(push_back(&json!({"k": 1}), &json!(1)), Value::Null);
        assert_eq!(push_back(&json!(42), &json!(1)), Value::Null);
    }
}
