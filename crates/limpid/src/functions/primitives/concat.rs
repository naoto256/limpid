//! `concat(a, b, ...)` — variadic Array concatenation.
//!
//! At least one Array argument is required. Every argument must be an
//! Array; mixed-type input loud-fails so the user is forced to convert
//! at the call site (no `[scalar]` auto-wrap).

use anyhow::bail;

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ArrayBuilder, Value};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "concat",
        FunctionSig::variadic(FieldType::Array, 1, FieldType::Array),
        |arena, args, _event| join(arena, args),
    );
}

fn join<'bump>(
    arena: &'bump EventArena<'bump>,
    args: &[Value<'bump>],
) -> anyhow::Result<Value<'bump>> {
    // Pre-size — sums avoid a builder grow.
    let total: usize = args
        .iter()
        .map(|v| match v {
            Value::Array(items) => items.len(),
            _ => 0,
        })
        .sum();
    let mut out = ArrayBuilder::with_capacity(arena, total);
    for v in args {
        match v {
            Value::Array(items) => {
                for item in *items {
                    out.push(*item);
                }
            }
            other => bail!(
                "concat() expects Array arguments, got {}",
                other.type_name()
            ),
        }
    }
    Ok(out.finish())
}
