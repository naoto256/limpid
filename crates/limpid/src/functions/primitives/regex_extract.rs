//! `regex_extract(target, pattern)` — first capture group (or full
//! match), `null` on no match.
//!
//! The "first capture group if any, else group 0" behaviour is
//! intentional: users writing `regex_extract(x, "src=(\S+)")` want the
//! value, not `src=...`. Patterns with no explicit group still work.

use anyhow::bail;
use serde_json::Value;

use super::{get_cached_regex, val_to_str};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "regex_extract",
        FunctionSig::fixed(&[FieldType::String, FieldType::String], FieldType::String),
        |args, _event| {
            let target = val_to_str(&args[0]);
            let pattern = val_to_str(&args[1]);
            match get_cached_regex(&pattern) {
                Ok(re) => {
                    if let Some(caps) = re.captures(&target) {
                        if let Some(m) = caps.get(1) {
                            Ok(Value::String(m.as_str().to_string()))
                        } else if let Some(m) = caps.get(0) {
                            Ok(Value::String(m.as_str().to_string()))
                        } else {
                            Ok(Value::Null)
                        }
                    } else {
                        Ok(Value::Null)
                    }
                }
                Err(e) => bail!("invalid regex: {}", e),
            }
        },
    );
}
