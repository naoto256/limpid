//! `last(arr)` — tail element of an array, or `null` if empty.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::dsl::field_schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "last",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Any),
        |_arena, args, _event| Ok(tail(&args[0])),
    );
}

fn tail<'bump>(v: &Value<'bump>) -> Value<'bump> {
    match v {
        Value::Array(items) => items.last().copied().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}
