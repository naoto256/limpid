//! `coalesce(a, b, c, ...)` — first non-null argument wins.
//!
//! Takes 1+ arguments and returns the leftmost one that is not
//! `Value::Null`. If every argument is null, returns `Null`.
//!
//! Designed for the OCSF / replay-shape composers, where a populated
//! field on `workspace.limpid` should win over an environmental
//! fallback (`received_at`, `hostname()`, an explicit literal). The
//! pre-coalesce idiom was a per-leaf `switch true { x != null { x }
//! default { y } }`, which is correct but verbose at 27 OCSF leaves.
//! `coalesce(workspace.limpid.time, received_at)` is the same
//! semantically, ten characters wide, and reads top-to-bottom.
//!
//! Semantics:
//! - **Eager**: every argument is evaluated before `coalesce` runs (DSL
//!   call sites have no short-circuit). The function then returns the
//!   first non-null. Since DSL identifiers and built-ins are pure
//!   (no side-effects), eager evaluation has no observable difference
//!   from short-circuit at the user level.
//! - `Null` is the only value that is "passed over". Empty strings,
//!   zero, empty objects, and empty arrays are returned as-is — they
//!   are real values, not absences. Callers who want "blank string is
//!   also absent" should write that condition explicitly.
//! - Variadic arity (≥ 1). Calling with no arguments is rejected by
//!   the analyzer / runtime arity check.
//! - Return type is `Any`: at static check time we cannot prove which
//!   slot wins, and the slots may carry different types. Downstream
//!   uses pin the type at the use site.

use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "coalesce",
        FunctionSig::variadic(FieldType::Any, 1, FieldType::Any),
        |_arena, args, _event| {
            for v in args {
                if !matches!(v, Value::Null) {
                    return Ok(*v);
                }
            }
            Ok(Value::Null)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::arena::EventArena;
    use crate::event::OwnedEvent;
    use crate::functions::FunctionRegistry;
    use crate::functions::table::TableStore;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn dummy_event() -> OwnedEvent {
        OwnedEvent::new(
            Bytes::from_static(b""),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn make_registry() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = TableStore::from_configs(vec![]).unwrap();
        crate::functions::register_builtins(&mut reg, table_store);
        reg
    }

    fn call_coalesce<'bump>(
        reg: &FunctionRegistry,
        bevent: &crate::event::BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
        args: &[Value<'bump>],
    ) -> anyhow::Result<Value<'bump>> {
        reg.call(None, "coalesce", args, bevent, arena)
    }

    #[test]
    fn returns_first_non_null_argument() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let v = call_coalesce(
            &reg,
            &bevent,
            &arena,
            &[Value::Null, Value::Int(42), Value::Int(99)],
        )
        .unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn first_argument_wins_when_non_null() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let v = call_coalesce(
            &reg,
            &bevent,
            &arena,
            &[Value::Int(1), Value::Int(2), Value::Int(3)],
        )
        .unwrap();
        assert_eq!(v, Value::Int(1));
    }

    #[test]
    fn all_null_returns_null() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let v = call_coalesce(
            &reg,
            &bevent,
            &arena,
            &[Value::Null, Value::Null, Value::Null],
        )
        .unwrap();
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn single_arg_is_returned_as_is() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        assert_eq!(
            call_coalesce(&reg, &bevent, &arena, &[Value::Int(7)]).unwrap(),
            Value::Int(7)
        );
        assert_eq!(
            call_coalesce(&reg, &bevent, &arena, &[Value::Null]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn empty_string_is_a_real_value_not_a_skip() {
        // "" is not Null — it is a present-but-empty string. coalesce
        // returns it. Callers who want "blank string is also absent"
        // express that condition themselves.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let v = call_coalesce(
            &reg,
            &bevent,
            &arena,
            &[
                Value::Null,
                Value::String(arena.alloc_str("")),
                Value::String(arena.alloc_str("fallback")),
            ],
        )
        .unwrap();
        assert_eq!(v, Value::String(""));
    }

    #[test]
    fn zero_is_a_real_value() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let v = call_coalesce(
            &reg,
            &bevent,
            &arena,
            &[Value::Null, Value::Int(0), Value::Int(99)],
        )
        .unwrap();
        assert_eq!(v, Value::Int(0));
    }

    #[test]
    fn zero_args_is_rejected_by_arity() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = call_coalesce(&reg, &bevent, &arena, &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("at least 1 argument") && msg.contains("got 0"),
            "unexpected error message: {}",
            msg
        );
    }

    #[test]
    fn mixed_types_pass_through_first_non_null() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let v = call_coalesce(
            &reg,
            &bevent,
            &arena,
            &[
                Value::Null,
                Value::String(arena.alloc_str("alice")),
                Value::Int(42),
            ],
        )
        .unwrap();
        assert_eq!(v, Value::String("alice"));
    }
}
