//! `null_omit(value)` — recursively strip `Null` from objects and arrays.
//!
//! Designed for the OCSF / replay-shape composers that build a HashLit
//! from a mix of populated and unpopulated workspace fields, then
//! `to_json`. Without `null_omit`, every absent field renders as
//! `"key": null` in the output — not strictly invalid OCSF, but noisy
//! enough that downstream consumers (Sentinel, Splunk DM) sometimes
//! fail schema validation. `null_omit` is the post-hoc strip: pass the
//! HashLit value through it before `to_json`, and the `null` keys
//! disappear without changing any populated key's shape.
//!
//! Semantics (recursive, single pass):
//!
//! - `Null` at the top level returns `Null`.
//! - `Object { k1: Null, k2: V }` drops `k1` and recurses into `V` →
//!   `Object { k2: V_recursed }`.
//! - `Array [Null, V1, Null, V2]` keeps the `Null` slots and recurses
//!   into the non-null elements →
//!   `Array [Null, V1_recursed, Null, V2_recursed]`. The function name
//!   advertises "omit null *keys*", not "compact arrays"; an explicit
//!   `Null` in an array is often a parser's placeholder ("this slot
//!   was unknown") and silently dropping it would hide the signal
//!   from anyone reading `tap --json`. Use a dedicated `array_filter`
//!   primitive when array compaction is what you want.
//! - Empty containers (`{}` / `[]`) are kept as-is. The function only
//!   strips `Null` leaves from objects; it does not recursively
//!   collapse a structure that just became empty.
//! - Any other scalar (`String`, `Int`, `Float`, `Bool`, `Bytes`,
//!   `Timestamp`) passes through unchanged.
//!
//! The "empty container survives" rule keeps the function simple and
//! its behaviour predictable: callers know what they're producing,
//! and "build a HashLit, strip nulls" doesn't accidentally collapse
//! intentional empty structures (e.g. an empty `evidences: []` array
//! a parser explicitly initialised).

use crate::dsl::value::{Map, Value};
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "null_omit",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Any),
        |args, _event| Ok(strip(&args[0]).unwrap_or(Value::Null)),
    );
}

/// Walk `value` and produce its null-stripped form.
///
/// Returns `None` when the *whole* value is `Null` and its caller is
/// an Object — the caller drops the key. Otherwise returns
/// `Some(stripped)`. Arrays preserve `Null` elements: a `Null` slot
/// in `[a, null, b]` survives so the array length and the parser's
/// intent stay visible.
fn strip(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::Object(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                if let Some(stripped) = strip(v) {
                    out.insert(k.clone(), stripped);
                }
            }
            Some(Value::Object(out))
        }
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                // Recurse into each element. A `Null` element comes
                // back as `None`; we restore it as `Null` so the
                // array shape is preserved. Non-null elements come
                // back as `Some(stripped)`.
                match strip(item) {
                    Some(s) => out.push(s),
                    None => out.push(Value::Null),
                }
            }
            Some(Value::Array(out))
        }
        other => Some(other.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::FunctionRegistry;
    use bytes::Bytes;

    fn make_reg() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        reg
    }

    fn dummy_event() -> crate::event::Event {
        crate::event::Event::new(Bytes::from_static(b""), "127.0.0.1:0".parse().unwrap())
    }

    fn obj(pairs: &[(&str, Value)]) -> Value {
        let mut m = Map::new();
        for (k, v) in pairs {
            m.insert((*k).to_string(), v.clone());
        }
        Value::Object(m)
    }

    #[test]
    fn flat_object_strips_null_keys() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        let input = obj(&[
            ("a", Value::Int(1)),
            ("b", Value::Null),
            ("c", Value::Int(3)),
        ]);
        let out = reg
            .call(None, "null_omit", &[input], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(out, obj(&[("a", Value::Int(1)), ("c", Value::Int(3))]));
    }

    #[test]
    fn nested_object_strips_null_at_every_depth() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        let input = obj(&[
            ("a", Value::Int(1)),
            ("b", obj(&[("c", Value::Null), ("d", Value::Int(2))])),
        ]);
        let out = reg
            .call(None, "null_omit", &[input], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(
            out,
            obj(&[("a", Value::Int(1)), ("b", obj(&[("d", Value::Int(2))]))])
        );
    }

    #[test]
    fn array_keeps_null_elements_intact() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // Arrays are not compacted: a `Null` element survives so the
        // array length and the parser's intent stay visible. The
        // function name advertises "omit null *keys*", not "compact
        // arrays" — use a separate `array_filter` primitive when array
        // compaction is what you want.
        let reg = make_reg();
        let input = obj(&[(
            "list",
            Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)]),
        )]);
        let out = reg
            .call(None, "null_omit", &[input], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(
            out,
            obj(&[(
                "list",
                Value::Array(vec![Value::Int(1), Value::Null, Value::Int(3)])
            )])
        );
    }

    #[test]
    fn empty_object_after_strip_is_kept() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `{a: 1, b: {c: null}}` → `{a: 1, b: {}}`: the inner Object
        // becomes empty after stripping `c`, but it stays as `{}` rather
        // than being recursively dropped. Operators may have intentional
        // empty structures (e.g. an `evidences: []` placeholder); the
        // function's contract is "strip nulls", not "minimise output".
        let reg = make_reg();
        let input = obj(&[("a", Value::Int(1)), ("b", obj(&[("c", Value::Null)]))]);
        let out = reg
            .call(None, "null_omit", &[input], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(out, obj(&[("a", Value::Int(1)), ("b", obj(&[]))]));
    }

    #[test]
    fn array_of_only_nulls_is_unchanged() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `[null, null]` does not become `[]` — `null_omit` strips
        // null *keys* from objects, not null *elements* from arrays.
        let reg = make_reg();
        let input = obj(&[("list", Value::Array(vec![Value::Null, Value::Null]))]);
        let out = reg
            .call(None, "null_omit", &[input], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(
            out,
            obj(&[("list", Value::Array(vec![Value::Null, Value::Null]))])
        );
    }

    #[test]
    fn top_level_null_returns_null() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        let out = reg
            .call(None, "null_omit", &[Value::Null], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn scalar_passes_through_unchanged() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        for v in [
            Value::Int(42),
            Value::Float(3.14),
            Value::String("hello".into()),
            Value::Bool(true),
        ] {
            let out = reg
                .call(None, "null_omit", &[v.clone()], &dummy_event(), &arena)
                .unwrap();
            assert_eq!(out, v);
        }
    }

    #[test]
    fn array_of_objects_recurses_into_each() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let reg = make_reg();
        let input = obj(&[(
            "evidences",
            Value::Array(vec![
                obj(&[
                    ("file", Value::String("a.exe".into())),
                    ("hash", Value::Null),
                ]),
                obj(&[
                    ("file", Value::Null),
                    ("process", Value::String("p".into())),
                ]),
            ]),
        )]);
        let out = reg
            .call(None, "null_omit", &[input], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(
            out,
            obj(&[(
                "evidences",
                Value::Array(vec![
                    obj(&[("file", Value::String("a.exe".into()))]),
                    obj(&[("process", Value::String("p".into()))]),
                ])
            )])
        );
    }

    #[test]
    fn idempotent_on_already_stripped() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // Calling `null_omit` on a value that has no nulls leaves it
        // unchanged. Important for chained pipelines where the same
        // composer might run twice or for downstream re-processing.
        let reg = make_reg();
        let input = obj(&[
            ("a", Value::Int(1)),
            ("nested", obj(&[("k", Value::String("v".into()))])),
        ]);
        let once = reg
            .call(None, "null_omit", &[input.clone()], &dummy_event(), &arena)
            .unwrap();
        let twice = reg
            .call(None, "null_omit", &[once.clone()], &dummy_event(), &arena)
            .unwrap();
        assert_eq!(once, input);
        assert_eq!(twice, once);
    }
}
