//! `to_json()` / `to_json(value)` — serialize the whole event (no arg)
//! or a single value (one arg) to a JSON string.
//!
//! This primitive errors on `Value::Bytes` anywhere in its input. The user-facing JSON form is "what JSON spec
//! says JSON is" (UTF-8 strings, numbers, etc); raw bytes need an
//! explicit conversion via `to_string(b)` (UTF-8) or a transport-level
//! encoding the user names. The internal `event::to_json_string` path
//! is separate and preserves bytes via the `$bytes_b64` marker for tap
//! / persistence; that path is *not* user-facing.

use anyhow::{Result, bail};

use crate::dsl::value::Value;
use crate::dsl::value_json::value_to_json;
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
                ensure_no_bytes(&args[0])?;
                let json = value_to_json(&args[0])?;
                Ok(Value::String(serde_json::to_string(&json)?))
            } else {
                bail!("to_json() expects 0 or 1 argument");
            }
        },
    );
}

fn ensure_no_bytes(v: &Value) -> Result<()> {
    match v {
        Value::Bytes(_) => bail!(
            "to_json() does not accept bytes; convert explicitly via to_string()"
        ),
        Value::Array(a) => {
            for item in a {
                ensure_no_bytes(item)?;
            }
            Ok(())
        }
        Value::Object(m) => {
            for val in m.values() {
                ensure_no_bytes(val)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
