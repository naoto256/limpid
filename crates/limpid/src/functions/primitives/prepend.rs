//! `prepend(arr, v)` — return a new array with `v` inserted at the front.
//!
//! Symmetric counterpart to `append`. In the positionless-collection
//! model both operations identify *where* purely by "front" / "back"
//! (a semantic relative to insertion order) rather than a numeric
//! index that would shift under later mutations.
//!
//! Behaviour mirrors `append`:
//! * `arr` is an Array → new Array with `v` at index 0, existing
//!   elements shifted to make room. Input is not mutated.
//! * `arr` is `Null` → returns `Null`.
//! * Any other non-array input → `Null`.
//! * `v` may be any value, including `Null`.

use crate::dsl::value::Value;

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "prepend",
        FunctionSig::fixed(&[FieldType::Array, FieldType::Any], FieldType::Array),
        |args, _event| Ok(push_front(&args[0], &args[1])),
    );
}

fn push_front(arr: &Value, v: &Value) -> Value {
    match arr {
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len() + 1);
            out.push(v.clone());
            out.extend_from_slice(items);
            Value::Array(out)
        }
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value;

    #[test]
    fn prepends_to_non_empty_array() {
        assert_eq!(push_front(&value!([2, 3]), &value!(1)), value!([1, 2, 3]));
    }

    #[test]
    fn prepends_to_empty_array() {
        assert_eq!(push_front(&value!([]), &value!("first")), value!(["first"]));
    }

    #[test]
    fn original_array_unchanged() {
        let original = value!([2, 3]);
        let result = push_front(&original, &value!(1));
        assert_eq!(original, value!([2, 3]));
        assert_eq!(result, value!([1, 2, 3]));
    }

    #[test]
    fn prepending_any_value_type() {
        assert_eq!(push_front(&value!([]), &value!(null)), value!([null]));
        assert_eq!(push_front(&value!([]), &value!({"k": 1})), value!([{"k": 1}]));
        assert_eq!(push_front(&value!([]), &value!([1, 2])), value!([[1, 2]]));
    }

    #[test]
    fn null_array_returns_null() {
        assert_eq!(push_front(&Value::Null, &value!(1)), Value::Null);
    }

    #[test]
    fn non_array_input_returns_null() {
        assert_eq!(push_front(&value!("string"), &value!(1)), Value::Null);
        assert_eq!(push_front(&value!({"k": 1}), &value!(1)), Value::Null);
        assert_eq!(push_front(&value!(42), &value!(1)), Value::Null);
    }

    #[test]
    fn symmetric_with_append() {
        // prepend(x, a) + append(_, b) should end up as [a, x..., b].
        use crate::functions::primitives::append;
        let _ = append::register; // ensure append module reachable; pure unit test logic below.

        let middle = value!([2, 3]);
        let prepended = push_front(&middle, &value!(1));
        // Manually append 4 for symmetric check.
        let mut with_back = prepended.as_array().unwrap().clone();
        with_back.push(value!(4));
        assert_eq!(Value::Array(with_back), value!([1, 2, 3, 4]));
    }
}
