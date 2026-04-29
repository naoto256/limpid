//! `len(x)` — cardinality primitive.
//!
//! Behaviour:
//! * `Array` → number of elements (i64).
//! * `String` → number of Unicode *characters* (not bytes).
//! * `Bytes` → byte length.
//! * `Object` → number of top-level keys.
//! * `Null` → `Null` pass-through.
//! * Any scalar → `Null`.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "len",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Int),
        |_arena, args, _event| Ok(measure(&args[0])),
    );
}

fn measure<'bump>(v: &Value<'bump>) -> Value<'bump> {
    match v {
        Value::Null => Value::Null,
        Value::Array(items) => Value::Int(items.len() as i64),
        Value::String(s) => Value::Int(s.chars().count() as i64),
        Value::Bytes(b) => Value::Int(b.len() as i64),
        Value::Object(entries) => Value::Int(entries.len() as i64),
        Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Timestamp(_) => Value::Null,
    }
}
