//! `contains(haystack, needle)` — substring membership test.

use anyhow::bail;
use serde_json::Value;

use super::val_to_str;
use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("contains", |args, _event| {
        if args.len() != 2 {
            bail!("contains() expects 2 arguments");
        }
        let haystack = val_to_str(&args[0]);
        let needle = val_to_str(&args[1]);
        Ok(Value::Bool(haystack.contains(&needle)))
    });
}
