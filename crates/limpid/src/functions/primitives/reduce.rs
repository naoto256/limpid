//! `reduce(array, init) { |acc, value| step }` /
//! `reduce(object, init) { |acc, key, value| step }` — left fold.
//!
//! Arrays bind the accumulator and current element as two block
//! parameters. Objects bind the accumulator, entry key, and entry value
//! as three parameters. Both forms fold in insertion order and return
//! the final accumulator. `Null` follows the empty-Array form, requires
//! two block parameters, and returns `init` unchanged. Real evaluation
//! lives in `eval::eval_block_primitive`; this stub installs the
//! signature and the missing-block error.
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

use super::block_collection_type;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "reduce",
        FunctionSig::fixed(&[block_collection_type(), FieldType::Any], FieldType::Any),
        |_arena, _args, _event| -> anyhow::Result<Value<'_>> {
            anyhow::bail!(
                "reduce() requires a block argument: `reduce(arr, init) {{ |acc, x| <step> }}`"
            );
        },
    );
}
