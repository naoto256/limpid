//! `syslog.set_pri(s, facility, severity)` — write or rewrite the
//! leading `<PRI>` header.
//!
//! Behaviour:
//! - If `s` already starts with a valid `<PRI>`, rewrite it.
//! - Otherwise prepend `<PRI>`.
//! - `facility` must be in 0..=23.
//! - `severity` must be in 0..=7.

use crate::dsl::arena::EventArena;
use crate::dsl::value::Value;
use anyhow::{Result, bail};

use crate::functions::primitives::val_to_str;
use crate::functions::syslog::pri::parse_leading_pri;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in_with_sig(
        "syslog",
        "set_pri",
        FunctionSig::fixed(
            &[FieldType::String, FieldType::Int, FieldType::Int],
            FieldType::String,
        ),
        |arena, args, _event| set_pri_impl(arena, args),
    );
}

fn set_pri_impl<'bump>(
    arena: &EventArena<'bump>,
    args: &[Value<'bump>],
) -> Result<Value<'bump>> {
    let text = val_to_str(&args[0])?;
    let facility = arg_as_u8(&args[1], "facility", 23)?;
    let severity = arg_as_u8(&args[2], "severity", 7)?;
    let pri = (facility as u16) * 8 + (severity as u16);

    let body = match parse_leading_pri(&text) {
        Some((_, offset)) => &text[offset..],
        None => &text,
    };

    let out = format!("<{}>{}", pri, body);
    Ok(Value::String(arena.alloc_str(&out)))
}

fn arg_as_u8(v: &Value<'_>, name: &str, max: u8) -> Result<u8> {
    let n = match v {
        Value::Int(n) if *n >= 0 => *n as u64,
        Value::Float(f) if f.is_finite() && *f >= 0.0 && f.fract() == 0.0 => *f as u64,
        _ => bail!("syslog.set_pri(): {} must be a non-negative integer", name),
    };
    if n > max as u64 {
        bail!("syslog.set_pri(): {} must be 0-{}, got {}", name, max, n);
    }
    Ok(n as u8)
}
