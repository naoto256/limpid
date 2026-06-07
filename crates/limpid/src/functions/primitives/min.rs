//! `min(arr)` — minimum of a numeric Array, or `null` if empty.
//!
//! Sibling of `max` — see `max::pick` for the shared comparison loop.

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "min",
        FunctionSig::fixed(&[FieldType::Array], FieldType::Any),
        |_arena, args, _event| super::max::pick(&args[0], /*want_max=*/ false),
    );
}
