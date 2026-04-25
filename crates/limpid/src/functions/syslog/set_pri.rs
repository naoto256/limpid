//! `syslog.set_pri(s, facility, severity)` — write or rewrite the
//! leading `<PRI>` header.
//!
//! Replaces the old implicit `facility = N` / `severity = N` assignment
//! side effect: once Event metadata no longer carries facility/severity
//! (v0.3.0 Block 4 Step 3), users set the PRI explicitly through this
//! function.
//!
//! Behaviour:
//! - If `s` already starts with a valid `<PRI>`, rewrite it.
//! - Otherwise prepend `<PRI>`.
//! - `facility` must be in 0..=23 (local0..local7 live at 16..23).
//! - `severity` must be in 0..=7 (emerg=0, alert=1, ..., debug=7).

use anyhow::{Result, bail};
use crate::dsl::value::Value;

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
        |args, _event| set_pri_impl(args),
    );
}

fn set_pri_impl(args: &[Value]) -> Result<Value> {
    // Arity check is centralised in the registry via FunctionSig.
    let text = val_to_str(&args[0])?;
    let facility = arg_as_u8(&args[1], "facility", 23)?;
    let severity = arg_as_u8(&args[2], "severity", 7)?;
    let pri = (facility as u16) * 8 + (severity as u16);

    let body = match parse_leading_pri(&text) {
        Some((_, offset)) => &text[offset..],
        None => &text,
    };

    Ok(Value::String(format!("<{}>{}", pri, body)))
}

fn arg_as_u8(v: &Value, name: &str, max: u8) -> Result<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn dummy_event() -> Event {
        Event::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn make_reg() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        reg
    }

    #[test]
    fn prepends_pri_when_missing() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "set_pri",
                &[
                    Value::String("hello".into()),
                    Value::Int(16),
                    Value::Int(6),
                ],
                &e,
            )
            .unwrap();
        // 16*8+6 = 134
        assert_eq!(r, Value::String("<134>hello".into()));
    }

    #[test]
    fn rewrites_existing_pri() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "set_pri",
                &[
                    Value::String("<185>body".into()),
                    Value::Int(16),
                    Value::Int(6),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::String("<134>body".into()));
    }

    #[test]
    fn leaves_invalid_pri_as_body() {
        // `<abc>` is not a real PRI — treat the whole string as body.
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "set_pri",
                &[
                    Value::String("<abc>body".into()),
                    Value::Int(16),
                    Value::Int(6),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::String("<134><abc>body".into()));
    }

    #[test]
    fn rejects_out_of_range_facility() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                Some("syslog"),
                "set_pri",
                &[
                    Value::String("msg".into()),
                    Value::Int(99),
                    Value::Int(0),
                ],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("facility must be 0-23"));
    }

    #[test]
    fn rejects_out_of_range_severity() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                Some("syslog"),
                "set_pri",
                &[
                    Value::String("msg".into()),
                    Value::Int(1),
                    Value::Int(8),
                ],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("severity must be 0-7"));
    }
}
