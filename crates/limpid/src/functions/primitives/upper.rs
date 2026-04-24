//! `upper(str)` — uppercasing (counterpart of `lower`).

use anyhow::bail;
use serde_json::Value;

use super::val_to_str;
use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("upper", |args, _event| {
        if args.len() != 1 {
            bail!("upper() expects 1 argument");
        }
        Ok(Value::String(val_to_str(&args[0]).to_uppercase()))
    });
}
