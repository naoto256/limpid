//! `reduce(arr, init) { |acc, x| body }` — left fold.
//!
//! Block takes two parameters: the running accumulator and the current
//! element. Real evaluation lives in `eval::eval_block_primitive`; this
//! stub installs the signature and the missing-block error.
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "reduce",
        FunctionSig::fixed(&[FieldType::Array, FieldType::Any], FieldType::Any),
        |_arena, _args, _event| -> anyhow::Result<Value<'_>> {
            anyhow::bail!(
                "reduce() requires a block argument: `reduce(arr, init) {{ |acc, x| <step> }}`"
            );
        },
    );
}
