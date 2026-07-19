//! `filter(array) { |value| predicate }` /
//! `filter(object) { |key, value| predicate }` — block-arg primitive.
//!
//! Arrays bind one block parameter and return an Array containing the
//! truthy elements. Objects bind the entry key and value as two
//! parameters and return an Object containing the truthy entries in
//! insertion order. `Null` follows the empty-Array form, requires one
//! block parameter, and returns an empty Array.
//!
//! See `map.rs` for the dispatch story (real evaluation in
//! `eval::eval_block_primitive`; this stub installs the signature and
//! loud-fails when the block is omitted).
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

use super::block_collection_type;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "filter",
        FunctionSig::fixed(
            &[block_collection_type()],
            FieldType::Union(vec![FieldType::Array, FieldType::Object]),
        ),
        |_arena, _args, _event| -> anyhow::Result<Value<'_>> {
            anyhow::bail!(
                "filter() requires a block argument: `filter(arr) {{ |x| <predicate> }}`"
            );
        },
    );
}
