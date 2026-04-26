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

use crate::dsl::value::Value;

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
    use crate::value;

    #[test]
    fn finds_first_matching_object() {
        let arr = value!([
            {"entityType": "Process", "fileName": "ps.exe"},
            {"entityType": "User", "name": "alice"},
        ]);
        let got = find(&arr, &value!("entityType"), &value!("User"));
        assert_eq!(got, value!({"entityType": "User", "name": "alice"}));
    }

    #[test]
    fn returns_first_match_not_subsequent() {
        let arr = value!([
            {"t": "a", "n": 1},
            {"t": "a", "n": 2},
        ]);
        let got = find(&arr, &value!("t"), &value!("a"));
        assert_eq!(got, value!({"t": "a", "n": 1}));
    }

    #[test]
    fn no_match_returns_null() {
        let arr = value!([{"t": "a"}, {"t": "b"}]);
        assert_eq!(find(&arr, &value!("t"), &value!("c")), Value::Null);
    }

    #[test]
    fn non_array_input_returns_null() {
        assert_eq!(
            find(&value!({"t": "a"}), &value!("t"), &value!("a")),
            Value::Null
        );
        assert_eq!(find(&Value::Null, &value!("t"), &value!("a")), Value::Null);
        assert_eq!(
            find(&value!("not an array"), &value!("t"), &value!("a")),
            Value::Null
        );
    }

    #[test]
    fn non_string_key_returns_null() {
        let arr = value!([{"t": "a"}]);
        assert_eq!(find(&arr, &value!(42), &value!("a")), Value::Null);
    }

    #[test]
    fn skips_non_object_elements() {
        let arr = value!(["scalar", 42, {"t": "a"}, null]);
        assert_eq!(find(&arr, &value!("t"), &value!("a")), value!({"t": "a"}));
    }

    #[test]
    fn matches_by_integer_value() {
        let arr = value!([{"n": 1}, {"n": 2}, {"n": 3}]);
        assert_eq!(find(&arr, &value!("n"), &value!(2)), value!({"n": 2}));
    }

    #[test]
    fn no_coercion_between_types() {
        // "2" (string) should NOT match 2 (int). Callers cast explicitly.
        let arr = value!([{"n": 2}]);
        assert_eq!(find(&arr, &value!("n"), &value!("2")), Value::Null);
    }

    #[test]
    fn empty_array_returns_null() {
        let arr = value!([]);
        assert_eq!(find(&arr, &value!("t"), &value!("a")), Value::Null);
    }
}
