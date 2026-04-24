//! `to_json()` / `to_json(value)` — serialize the whole event (no arg)
//! or a single value (one arg) to a JSON string.

use anyhow::bail;
use serde_json::Value;

use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_json",
        FunctionSig::optional(&[FieldType::Any], 0, FieldType::String),
        |args, event| {
            if args.is_empty() {
                Ok(Value::String(event.to_json_string()))
            } else if args.len() == 1 {
                Ok(Value::String(serde_json::to_string(&args[0])?))
            } else {
                bail!("to_json() expects 0 or 1 argument");
            }
        },
    );
}
