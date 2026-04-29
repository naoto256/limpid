//! `prepend(arr, v)` — return a new array with `v` inserted at the front.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ArrayBuilder, Value};

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "prepend",
        FunctionSig::fixed(&[FieldType::Array, FieldType::Any], FieldType::Array),
        |arena, args, _event| Ok(push_front(arena, &args[0], &args[1])),
    );
}

fn push_front<'bump>(
    arena: &EventArena<'bump>,
    arr: &Value<'bump>,
    v: &Value<'bump>,
) -> Value<'bump> {
    match arr {
        Value::Array(items) => {
            let mut builder = ArrayBuilder::with_capacity(arena, items.len() + 1);
            builder.push(*v);
            for item in items.iter() {
                builder.push(*item);
            }
            builder.finish()
        }
        _ => Value::Null,
    }
}
