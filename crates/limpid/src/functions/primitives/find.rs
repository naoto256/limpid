//! `find(array) { |value| predicate }` /
//! `find(object) { |key, value| predicate }` — find the first match.
//!
//! Arrays bind one block parameter and return the first matching
//! element. Objects bind the entry key and value as two parameters and
//! return the first match as a `[key, value]` Array. A miss returns
//! `Null`; `Null` input follows the empty-Array form and requires one
//! block parameter.
//!
//! Replaces the v0.7.3-and-earlier `find_by(arr, key, value)`. Real
//! evaluation lives in `eval::eval_block_primitive`; this stub installs
//! the signature so the analyzer can type-check call sites and so a
//! call with the block missing produces a clear error rather than the
//! generic "expected a block" surprise.
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

use super::block_collection_type;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "find",
        FunctionSig::fixed(&[block_collection_type()], FieldType::Any),
        |_arena, _args, _event| -> anyhow::Result<Value<'_>> {
            anyhow::bail!("find() requires a block argument: `find(arr) {{ |x| <predicate> }}`");
        },
    );
}
