//! `regex_replace(target, pattern, replacement)` — replace all matches.
//!
//! Replacement strings support `$1`, `$2`, … capture-group backrefs
//! (via `regex_lite`'s `replace_all` behaviour).

use anyhow::bail;
use serde_json::Value;

use super::{get_cached_regex, val_to_str};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "regex_replace",
        FunctionSig::fixed(
            &[FieldType::String, FieldType::String, FieldType::String],
            FieldType::String,
        ),
        |args, _event| {
            let target = val_to_str(&args[0]);
            let pattern = val_to_str(&args[1]);
            let replacement = val_to_str(&args[2]);
            match get_cached_regex(&pattern) {
                Ok(re) => Ok(Value::String(
                    re.replace_all(&target, replacement.as_str()).into_owned(),
                )),
                Err(e) => bail!("invalid regex: {}", e),
            }
        },
    );
}
