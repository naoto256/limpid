//! `contains(haystack, needle)` — substring membership test.

use crate::dsl::value::Value;

use super::val_to_str;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "contains",
        FunctionSig::fixed(&[FieldType::String, FieldType::String], FieldType::Bool),
        |args, _event| {
            let haystack = val_to_str(&args[0])?;
            let needle = val_to_str(&args[1])?;
            Ok(Value::Bool(haystack.contains(&needle)))
        },
    );
}
