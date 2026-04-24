//! `regex_match(target, pattern)` — boolean match test.

use anyhow::bail;
use serde_json::Value;

use super::{get_cached_regex, val_to_str};
use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("regex_match", |args, _event| {
        if args.len() != 2 {
            bail!("regex_match() expects 2 arguments (target, pattern)");
        }
        let target = val_to_str(&args[0]);
        let pattern = val_to_str(&args[1]);
        match get_cached_regex(&pattern) {
            Ok(re) => Ok(Value::Bool(re.is_match(&target))),
            Err(e) => bail!("invalid regex: {}", e),
        }
    });
}
