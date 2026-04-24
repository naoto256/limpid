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
use serde_json::Value;

use crate::functions::FunctionRegistry;
use crate::functions::primitives::val_to_str;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in("syslog", "set_pri", |args, _event| set_pri_impl(args));
}

fn set_pri_impl(args: &[Value]) -> Result<Value> {
    if args.len() != 3 {
        bail!("syslog.set_pri() expects 3 arguments (text, facility, severity)");
    }
    let text = val_to_str(&args[0]);
    let facility = arg_as_u8(&args[1], "facility", 23)?;
    let severity = arg_as_u8(&args[2], "severity", 7)?;
    let pri = (facility as u16) * 8 + (severity as u16);

    let body = match strip_leading_pri(&text) {
        Some(rest) => rest,
        None => &text,
    };

    Ok(Value::String(format!("<{}>{}", pri, body)))
}

fn arg_as_u8(v: &Value, name: &str, max: u8) -> Result<u8> {
    let n = match v {
        Value::Number(n) => n.as_u64().ok_or_else(|| {
            anyhow::anyhow!("syslog.set_pri(): {} must be a non-negative integer", name)
        })?,
        _ => bail!("syslog.set_pri(): {} must be a number", name),
    };
    if n > max as u64 {
        bail!("syslog.set_pri(): {} must be 0-{}, got {}", name, max, n);
    }
    Ok(n as u8)
}

/// If `s` begins with a syntactically valid `<N>` header (1-3 digits,
/// resulting value ≤ 191), return the remainder. Otherwise `None`.
fn strip_leading_pri(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let limit = bytes.len().min(6);
    let gt_pos = bytes[..limit].iter().position(|&b| b == b'>')?;
    if gt_pos < 2 {
        return None;
    }
    let digits = &bytes[1..gt_pos];
    if !digits.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Parse numeric value — reject out-of-range values rather than
    // treating them as PRI.
    let n: u16 = std::str::from_utf8(digits).ok()?.parse().ok()?;
    if n > 191 {
        return None;
    }
    Some(&s[gt_pos + 1..])
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
                    Value::Number(16.into()),
                    Value::Number(6.into()),
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
                    Value::Number(16.into()),
                    Value::Number(6.into()),
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
                    Value::Number(16.into()),
                    Value::Number(6.into()),
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
                    Value::Number(99.into()),
                    Value::Number(0.into()),
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
                    Value::Number(1.into()),
                    Value::Number(8.into()),
                ],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("severity must be 0-7"));
    }
}
