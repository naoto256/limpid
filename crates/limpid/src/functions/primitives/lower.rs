//! `lower(str)` — ASCII/Unicode lowercasing.

use serde_json::Value;

use super::val_to_str;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "lower",
        FunctionSig::fixed(&[FieldType::String], FieldType::String),
        |args, _event| Ok(Value::String(val_to_str(&args[0]).to_lowercase())),
    );
}
