//! `syslog.strip_pri(s)` — remove a leading `<PRI>` header.
//!
//! Returns the input unchanged if it doesn't start with `<N>` where
//! `N` is 1-3 digits (the valid PRI range is 0..=191). Strictly
//! byte-oriented — no allocation when nothing to strip.

use anyhow::bail;
use serde_json::Value;

use crate::functions::FunctionRegistry;
use crate::functions::primitives::val_to_str;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in("syslog", "strip_pri", |args, _event| {
        if args.len() != 1 {
            bail!("syslog.strip_pri() expects 1 argument (input string)");
        }
        let input = val_to_str(&args[0]);
        let stripped = match strip_pri_prefix(&input) {
            Some(rest) => rest.to_string(),
            None => input,
        };
        Ok(Value::String(stripped))
    });
}

/// If `s` starts with a valid `<PRI>` header (1-3 digits between '<'
/// and '>'), return the remainder. Otherwise `None`.
pub(crate) fn strip_pri_prefix(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }
    let limit = bytes.len().min(6);
    let gt_pos = bytes[..limit].iter().position(|&b| b == b'>')?;
    if gt_pos < 2 {
        return None;
    }
    if !bytes[1..gt_pos].iter().all(|b| b.is_ascii_digit()) {
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
    fn removes_valid_header() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[Value::String("<185>hello".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::String("hello".into()));
    }

    #[test]
    fn passthrough_when_no_pri() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[Value::String("hello".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::String("hello".into()));
    }

    #[test]
    fn rejects_non_digit_pri() {
        // `<abc>` is not valid — leave unchanged
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[Value::String("<abc>hi".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::String("<abc>hi".into()));
    }

    #[test]
    fn rejects_wrong_arity() {
        let reg = make_reg();
        let e = dummy_event();
        assert!(reg.call(Some("syslog"), "strip_pri", &[], &e).is_err());
    }
}
