//! `find_by(array, key, value)` — locate the first object in an array
//! whose `key` field equals `value`. Non-array input returns `null`.

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
        |_arena, args, _event| Ok(find(&args[0], &args[1], &args[2])),
    );
}

fn find<'bump>(
    array: &Value<'bump>,
    key: &Value<'bump>,
    value: &Value<'bump>,
) -> Value<'bump> {
    let key_str = match key {
        Value::String(s) => *s,
        _ => return Value::Null,
    };
    let Value::Array(items) = array else {
        return Value::Null;
    };
    for item in items.iter() {
        if let Value::Object(entries) = item {
            for (k, got) in entries.iter() {
                if *k == key_str && got == value {
                    return *item;
                }
            }
        }
    }
    Value::Null
}
