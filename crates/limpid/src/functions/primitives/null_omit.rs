//! `null_omit(value)` — recursively strip `Null` from objects and arrays.
//!
//! Designed for OCSF / replay-shape composers that build a HashLit
//! from a mix of populated and unpopulated workspace fields, then
//! `to_json`. Without `null_omit`, every absent field renders as
//! `"key": null` in the output. `null_omit` is the post-hoc strip:
//! pass the HashLit through it before `to_json` and the `null` keys
//! disappear without changing populated keys.
//!
//! Semantics (recursive, single pass):
//!
//! - `Null` at the top level returns `Null`.
//! - `Object { k1: Null, k2: V }` drops `k1` and recurses into `V`.
//! - `Array [Null, V1, ...]` keeps the `Null` slots and recurses into
//!   the non-null elements (the function strips null *keys*, not null
//!   *elements*).
//! - Empty containers survive — the function only strips `Null` leaves
//!   from objects.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ArrayBuilder, ObjectBuilder, Value};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "null_omit",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Any),
        |arena, args, _event| Ok(strip(arena, &args[0]).unwrap_or(Value::Null)),
    );
}

/// Walk `value` and produce its null-stripped form. Returns `None` when
/// the *whole* value is `Null` (the caller — an Object — drops the key).
/// Otherwise returns `Some(stripped)`. Arrays preserve `Null` elements.
fn strip<'bump>(
    arena: &EventArena<'bump>,
    value: &Value<'bump>,
) -> Option<Value<'bump>> {
    match value {
        Value::Null => None,
        Value::Object(entries) => {
            let mut builder = ObjectBuilder::with_capacity(arena, entries.len());
            for (k, v) in entries.iter() {
                if let Some(stripped) = strip(arena, v) {
                    builder.push(k, stripped);
                }
            }
            Some(builder.finish())
        }
        Value::Array(items) => {
            let mut builder = ArrayBuilder::with_capacity(arena, items.len());
            for item in items.iter() {
                match strip(arena, item) {
                    Some(s) => builder.push(s),
                    None => builder.push(Value::Null),
                }
            }
            Some(builder.finish())
        }
        other => Some(*other),
    }
}
