//! `first(arr)` — head element of an array, or `null` if empty.
//!
//! Non-array input (including `Null`) returns `null` so call sites can
//! pipeline through optional fields without an explicit existence guard.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "first",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Any),
        |_arena, args, _event| Ok(head(&args[0])),
    );
}

fn head<'bump>(v: &Value<'bump>) -> Value<'bump> {
    match v {
        Value::Array(items) => items.first().copied().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}
