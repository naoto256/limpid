//! `sum(arr)` — sum a numeric Array.
//!
//! All-Int input stays Int; any Float participant promotes the result
//! to Float. Empty Array → `Int(0)`. Non-numeric elements bail rather
//! than silently coerce; mixed numeric / null arrays bail too because
//! "ignore the nulls" is a per-pipeline policy decision and limpid does
//! not pick one for the operator.

use anyhow::bail;

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "sum",
        FunctionSig::fixed(&[FieldType::Array], FieldType::Any),
        |_arena, args, _event| add(&args[0]),
    );
}

fn add<'bump>(v: &Value<'bump>) -> anyhow::Result<Value<'bump>> {
    let items = match v {
        Value::Array(items) => *items,
        other => bail!("sum() expects an Array, got {}", other.type_name()),
    };
    let mut int_acc: i64 = 0;
    let mut float_acc: f64 = 0.0;
    let mut saw_float = false;
    for item in items {
        match item {
            Value::Int(n) => int_acc += *n,
            Value::Float(n) => {
                saw_float = true;
                float_acc += *n;
            }
            other => bail!("sum() expects numeric elements, got {}", other.type_name()),
        }
    }
    if saw_float {
        Ok(Value::Float(int_acc as f64 + float_acc))
    } else {
        Ok(Value::Int(int_acc))
    }
}
