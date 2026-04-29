//! `to_json(value)` — serialize a value to a JSON string.
//!
//! Errors on `Value::Bytes` anywhere in the input. The user-facing JSON
//! form is "what JSON spec says JSON is" (UTF-8 strings, numbers, etc);
//! raw bytes need an explicit conversion via `to_string(b)` (UTF-8) or a
//! transport-level encoding the user names. The internal
//! `event::to_json_string` path is separate and preserves bytes via the
//! `$bytes_b64` marker for tap / persistence; that path is *not*
//! user-facing.
//!
//! Streams `Value<'bump>` directly into `serde_json::ser::Serializer`
//! via the [`Serialize`] impl in [`crate::dsl::value_json`] — no
//! intermediate `serde_json::Value` allocation. This is the v0.6.0
//! Step 3 hot-path replacement for the prior `value_view_to_json`
//! double walk.

use anyhow::{Result, bail};

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_json",
        FunctionSig::fixed(&[FieldType::Any], FieldType::String),
        |arena, args, _event| {
            ensure_no_bytes(&args[0])?;
            let s = serde_json::to_string(&args[0])
                .map_err(|e| anyhow::anyhow!("to_json(): {e}"))?;
            Ok(Value::String(arena.alloc_str(&s)))
        },
    );
}

/// Guard against `Value::Bytes` anywhere in the tree before serialising.
/// The boundary path (`event::to_json_value` / queue persistence) keeps
/// bytes via the `$bytes_b64` marker, but `to_json` is user-facing —
/// users must convert explicitly via `to_string()` so the bytes marker
/// never leaks into pipeline-visible output.
fn ensure_no_bytes(v: &Value<'_>) -> Result<()> {
    match v {
        Value::Bytes(_) => {
            bail!("to_json() does not accept bytes; convert explicitly via to_string()")
        }
        Value::Array(a) => {
            for item in a.iter() {
                ensure_no_bytes(item)?;
            }
            Ok(())
        }
        Value::Object(entries) => {
            for (_, val) in entries.iter() {
                ensure_no_bytes(val)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}
