//! `upper(str)` — uppercasing (counterpart of `lower`).

use crate::dsl::value::Value;

use super::val_to_str;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "upper",
        FunctionSig::fixed(&[FieldType::String], FieldType::String),
        |arena, args, _event| {
            let s = val_to_str(&args[0])?.to_uppercase();
            Ok(Value::String(arena.alloc_str(&s)))
        },
    );
}
