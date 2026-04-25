//! `syslog.extract_pri(s)` — return the leading `<PRI>` value as a
//! number, or null if no valid PRI is present.
//!
//! The PRI is `facility*8 + severity`. Valid range is 0..=191. Useful
//! for composing routing rules that depend on facility/severity without
//! re-parsing the full header.

use crate::dsl::value::Value;

use crate::functions::primitives::val_to_str;
use crate::functions::syslog::pri::parse_leading_pri;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in_with_sig(
        "syslog",
        "extract_pri",
        FunctionSig::fixed(&[FieldType::String], FieldType::Int),
        |args, _event| {
            let input = val_to_str(&args[0])?;
            Ok(parse_leading_pri(&input)
                .map(|(n, _)| Value::Int(n as i64))
                .unwrap_or(Value::Null))
        },
    );
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
    fn extracts_valid_pri() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "extract_pri",
                &[Value::String("<134>body".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::Int(134));
    }

    #[test]
    fn extracts_single_digit_pri() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "extract_pri",
                &[Value::String("<7>debug".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::Int(7));
    }

    #[test]
    fn returns_null_when_no_pri() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "extract_pri",
                &[Value::String("hello".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::Null);
    }

    #[test]
    fn returns_null_when_out_of_range() {
        // <999> exceeds max valid PRI (191)
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "extract_pri",
                &[Value::String("<999>body".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::Null);
    }

    #[test]
    fn returns_null_on_non_digit_pri() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "extract_pri",
                &[Value::String("<abc>body".into())],
                &e,
            )
            .unwrap();
        assert_eq!(r, Value::Null);
    }
}
