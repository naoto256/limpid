//! String predicates — `contains`, `starts_with`, `ends_with`.

use crate::dsl::value::Value;

use super::val_to_str;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

fn register_predicate(
    reg: &mut FunctionRegistry,
    name: &'static str,
    pred: fn(&str, &str) -> bool,
) {
    reg.register_with_sig(
        name,
        FunctionSig::fixed(&[FieldType::String, FieldType::String], FieldType::Bool),
        move |args, _event| {
            let haystack = val_to_str(&args[0])?;
            let needle = val_to_str(&args[1])?;
            Ok(Value::Bool(pred(&haystack, &needle)))
        },
    );
}

pub fn register(reg: &mut FunctionRegistry) {
    register_predicate(reg, "contains", |h, n| h.contains(n));
    register_predicate(reg, "starts_with", |h, n| h.starts_with(n));
    register_predicate(reg, "ends_with", |h, n| h.ends_with(n));
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

    fn call_pred(reg: &FunctionRegistry, name: &str, h: &str, n: &str) -> bool {
        let e = dummy_event();
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let v = reg
            .call(
                None,
                name,
                &[Value::String(h.into()), Value::String(n.into())],
                &e,
                &arena,
            )
            .unwrap();
        let Value::Bool(b) = v else { panic!() };
        b
    }

    fn make_reg() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        reg
    }

    #[test]
    fn contains_matches_anywhere() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        assert!(call_pred(&reg, "contains", "hello world", "lo wo"));
        assert!(call_pred(&reg, "contains", "abc", ""));
        assert!(!call_pred(&reg, "contains", "abc", "xyz"));
    }

    #[test]
    fn starts_with_matches_prefix_only() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        assert!(call_pred(&reg, "starts_with", "CEF:0|Vendor", "CEF:"));
        assert!(call_pred(&reg, "starts_with", "abc", ""));
        assert!(!call_pred(&reg, "starts_with", "<134>CEF:0|Vendor", "CEF:"));
        assert!(!call_pred(&reg, "starts_with", "C", "CEF:"));
    }

    #[test]
    fn ends_with_matches_suffix_only() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        assert!(call_pred(&reg, "ends_with", "/var/log/foo.log", ".log"));
        assert!(call_pred(&reg, "ends_with", "abc", ""));
        assert!(!call_pred(&reg, "ends_with", "/var/log/foo.txt", ".log"));
        assert!(!call_pred(&reg, "ends_with", "g", ".log"));
    }
}
