//! `filter(arr) { |x| body }` — block-arg primitive.
//!
//! See `map.rs` for the dispatch story (real evaluation in
//! `eval::eval_block_primitive`; this stub installs the signature and
//! loud-fails when the block is omitted).
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "filter",
        FunctionSig::fixed(&[FieldType::Array], FieldType::Array),
        |_arena, _args, _event| -> anyhow::Result<Value<'_>> {
            anyhow::bail!(
                "filter() requires a block argument: `filter(arr) {{ |x| <predicate> }}`"
            );
        },
    );
}
