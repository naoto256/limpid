//! `is_array(value) → Bool` — type predicate.
//!
//! Returns `true` when `value` is an Array, `false` for any other type
//! (including `null`). Pairs with `find` / `map` / `filter` as the
//! pre-check used by snippet parsers to convert "non-array where I
//! expected an array" into a parser-authored loud-fail message instead
//! of the runtime `find()/map()/filter() expects an array as its first
//! argument, got <type>` message.
//!
//! Limpid is dynamically typed; this is the minimum predicate surface
//! needed for parsers to validate intake shape. Sibling predicates
//! (`is_object`, `is_string`, …) can land as separate primitives when
//! Rule of Three triggers — `is_array` alone covers the array-input
//! pre-check pattern, which is the immediate need.

use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "is_array",
        FunctionSig::fixed(&[FieldType::Any], FieldType::Bool),
        |_arena, args, _event| Ok(Value::Bool(matches!(args[0], Value::Array(_)))),
    );
}

// Behavior tests are in `dsl::exec::tests` alongside the other array
// primitive execution tests.
