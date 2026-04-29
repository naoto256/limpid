//! `to_int(x)` — coerce a value to a 64-bit signed integer.
//!
//! Behaviour:
//! * `Int` → itself.
//! * `Float` → finite values truncated toward zero; `NaN` / `±∞` → `Null`.
//! * `String` → `str::parse::<i64>` (trimmed); unparseable → `Null`.
//! * `Bool` → `1` or `0`.
//! * `Timestamp` → unix nanoseconds (`i64`).
//! * `Null` → `Null` pass-through.
//! * `Bytes` → error.
//! * Arrays / Objects → `Null`.

use anyhow::{Result, bail};

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "to_int",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Int),
        |_arena, args, _event| coerce(&args[0]),
    );
}

fn coerce<'bump>(v: &Value<'bump>) -> Result<Value<'bump>> {
    Ok(match v {
        Value::Null => Value::Null,
        Value::Bool(b) => Value::Int(if *b { 1 } else { 0 }),
        Value::Int(n) => Value::Int(*n),
        Value::Float(f) => {
            if f.is_finite() {
                Value::Int(*f as i64)
            } else {
                Value::Null
            }
        }
        Value::String(s) => {
            let t = s.trim();
            match t.parse::<i64>() {
                Ok(i) => Value::Int(i),
                Err(_) => Value::Null,
            }
        }
        Value::Bytes(_) => bail!("to_int() does not accept bytes (use to_string() first)"),
        Value::Timestamp(dt) => Value::Int(dt.timestamp_nanos_opt().unwrap_or(0)),
        Value::Array(_) | Value::Object(_) => Value::Null,
    })
}
