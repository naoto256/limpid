//! `find(arr) { |x| body }` — return the first element where the block
//! returns truthy, or `null`.
//!
//! Replaces the v0.7.3-and-earlier `find_by(arr, key, value)`. Real
//! evaluation lives in `eval::eval_block_primitive`; this stub installs
//! the signature so the analyzer can type-check call sites and so a
//! call with the block missing produces a clear error rather than the
//! generic "expected a block" surprise.
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "find",
        FunctionSig::fixed(&[FieldType::Array], FieldType::Any),
        |_arena, _args, _event| -> anyhow::Result<Value<'_>> {
            anyhow::bail!("find() requires a block argument: `find(arr) {{ |x| <predicate> }}`");
        },
    );
}
