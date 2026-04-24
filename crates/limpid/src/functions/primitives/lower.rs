//! `lower(str)` — ASCII/Unicode lowercasing.

use anyhow::bail;
use serde_json::Value;

use super::val_to_str;
use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("lower", |args, _event| {
        if args.len() != 1 {
            bail!("lower() expects 1 argument");
        }
        Ok(Value::String(val_to_str(&args[0]).to_lowercase()))
    });
}
