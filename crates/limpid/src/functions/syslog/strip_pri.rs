//! `syslog.strip_pri(s)` — remove a leading `<PRI>` header.
//!
//! Returns the input unchanged if it doesn't start with `<N>` where
//! `N` is 1-3 digits (the valid PRI range is 0..=191). Strictly
//! byte-oriented — no allocation when nothing to strip.

use crate::dsl::value::Value;

use crate::functions::primitives::val_to_str;
use crate::functions::syslog::pri::parse_leading_pri;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in_with_sig(
        "syslog",
        "strip_pri",
        FunctionSig::fixed(&[FieldType::String], FieldType::String),
        |args, _event| {
            let input = val_to_str(&args[0])?;
            let stripped = match parse_leading_pri(&input) {
                Some((_, body)) => input[body..].to_string(),
                None => input,
            };
            Ok(Value::String(stripped))
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
    fn removes_valid_header() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[Value::String("<185>hello".into())],
                &e,
                &arena,
            )
            .unwrap();
        assert_eq!(r, Value::String("hello".into()));
    }

    #[test]
    fn passthrough_when_no_pri() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[Value::String("hello".into())],
                &e,
                &arena,
            )
            .unwrap();
        assert_eq!(r, Value::String("hello".into()));
    }

    #[test]
    fn rejects_non_digit_pri() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `<abc>` is not valid — leave unchanged
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[Value::String("<abc>hi".into())],
                &e,
                &arena,
            )
            .unwrap();
        assert_eq!(r, Value::String("<abc>hi".into()));
    }

    #[test]
    fn rejects_wrong_arity() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        let e = dummy_event();
        assert!(
            reg.call(Some("syslog"), "strip_pri", &[], &e, &arena)
                .is_err()
        );
    }
}
