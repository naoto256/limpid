//! `find_by(array, key, value)` — locate the first object in an array
//! whose `key` field equals `value`.
//!
//! Motivation: event schemas that carry arrays-of-objects (MDE evidence,
//! CEF extension lists, OCSF observables) frequently need "pick the
//! first item matching this type". Without a primitive, DSL callers
//! would need array indexing plus manual iteration; `find_by` is the
//! obvious scalar-result shortcut.
//!
//! Behaviour:
//! * `array` is a JSON array. Non-array input returns `null`.
//! * `key` is a string; if any element is not an object, it is skipped.
//! * Equality is value-level (`serde_json::Value::eq`), so
//!   `find_by(arr, "count", 3)` matches integers, `find_by(arr, "tag",
//!   "process")` matches strings, etc. No coercion.
//! * Returns the first matching element as-is. `null` if nothing
//!   matches, same policy as other partial-data primitives.
//! * Does not recurse into nested objects — key lookup is top-level per
//!   element. Callers who need deeper matching can post-filter or call
//!   `find_by` twice.

use serde_json::Value;

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "find_by",
        FunctionSig::fixed(
            &[FieldType::Any, FieldType::String, FieldType::Any],
            FieldType::Any,
        ),
        |args, _event| Ok(find(&args[0], &args[1], &args[2])),
    );
}

fn find(array: &Value, key: &Value, value: &Value) -> Value {
    let key_str = match key {
        Value::String(s) => s.as_str(),
        _ => return Value::Null,
    };
    let Value::Array(items) = array else {
        return Value::Null;
    };
    for item in items {
        if let Value::Object(map) = item
            && let Some(got) = map.get(key_str)
            && got == value
        {
            return item.clone();
        }
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_first_matching_object() {
        let arr = json!([
            {"entityType": "Process", "fileName": "ps.exe"},
            {"entityType": "User", "name": "alice"},
        ]);
        let got = find(&arr, &json!("entityType"), &json!("User"));
        assert_eq!(got, json!({"entityType": "User", "name": "alice"}));
    }

    #[test]
    fn returns_first_match_not_subsequent() {
        let arr = json!([
            {"t": "a", "n": 1},
            {"t": "a", "n": 2},
        ]);
        let got = find(&arr, &json!("t"), &json!("a"));
        assert_eq!(got, json!({"t": "a", "n": 1}));
    }

    #[test]
    fn no_match_returns_null() {
        let arr = json!([{"t": "a"}, {"t": "b"}]);
        assert_eq!(find(&arr, &json!("t"), &json!("c")), Value::Null);
    }

    #[test]
    fn non_array_input_returns_null() {
        assert_eq!(find(&json!({"t": "a"}), &json!("t"), &json!("a")), Value::Null);
        assert_eq!(find(&Value::Null, &json!("t"), &json!("a")), Value::Null);
        assert_eq!(find(&json!("not an array"), &json!("t"), &json!("a")), Value::Null);
    }

    #[test]
    fn non_string_key_returns_null() {
        let arr = json!([{"t": "a"}]);
        assert_eq!(find(&arr, &json!(42), &json!("a")), Value::Null);
    }

    #[test]
    fn skips_non_object_elements() {
        let arr = json!(["scalar", 42, {"t": "a"}, null]);
        assert_eq!(find(&arr, &json!("t"), &json!("a")), json!({"t": "a"}));
    }

    #[test]
    fn matches_by_integer_value() {
        let arr = json!([{"n": 1}, {"n": 2}, {"n": 3}]);
        assert_eq!(find(&arr, &json!("n"), &json!(2)), json!({"n": 2}));
    }

    #[test]
    fn no_coercion_between_types() {
        // "2" (string) should NOT match 2 (int). Callers cast explicitly.
        let arr = json!([{"n": 2}]);
        assert_eq!(find(&arr, &json!("n"), &json!("2")), Value::Null);
    }

    #[test]
    fn empty_array_returns_null() {
        let arr = json!([]);
        assert_eq!(find(&arr, &json!("t"), &json!("a")), Value::Null);
    }
}
