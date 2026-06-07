//! `distinct(arr)` — equality dedupe, preserves first occurrence.
//!
//! Equality follows `Value::PartialEq` (the same rule the DSL `==`
//! operator uses for structural comparison; numbers cross-compare Int /
//! Float, Bytes never equals String). O(n²) worst case — acceptable for
//! the typical array sizes flowing through limpid (handful to low
//! thousands); a hashing path can land if profiles ever flag it.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ArrayBuilder, Value};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "distinct",
        FunctionSig::fixed(&[FieldType::Array], FieldType::Array),
        |arena, args, _event| Ok(dedup(arena, &args[0])),
    );
}

fn dedup<'bump>(arena: &'bump EventArena<'bump>, v: &Value<'bump>) -> Value<'bump> {
    let items = match v {
        Value::Array(items) => *items,
        _ => return Value::empty_array(),
    };
    // Track membership in a scratch arena Vec, then freeze through the
    // builder. ArrayBuilder doesn't expose iteration, so doing the
    // membership check off the side is the simplest path that keeps the
    // hot loop bounds-check-free.
    let mut seen: bumpalo::collections::Vec<Value<'bump>> =
        bumpalo::collections::Vec::with_capacity_in(items.len(), arena.bump());
    for item in items {
        if !seen.iter().any(|s| s == item) {
            seen.push(*item);
        }
    }
    let mut out = ArrayBuilder::with_capacity(arena, seen.len());
    for s in seen.iter() {
        out.push(*s);
    }
    out.finish()
}
