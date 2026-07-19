//! `map(array) { |value| body }` /
//! `map(object) { |key, value| body }` — block-arg primitive.
//!
//! Arrays bind one block parameter per element. Objects bind the entry
//! key and value as two parameters. Both forms collect block results
//! into an Array in insertion order. `Null` follows the empty-Array
//! form, requires one block parameter, and returns an empty Array.
//!
//! Real evaluation happens in [`crate::dsl::eval::eval_block_primitive`];
//! call sites with a trailing block are intercepted at the
//! `ExprKind::FuncCall` arm before ordinary primitive dispatch. The
//! registration here exists so that:
//!
//! - the analyzer can pull the signature for arity / return type;
//! - a call site missing its block (e.g. `map(arr)`) routes through this
//!   stub, which loud-fails with a clear error.
//!
//! Signature: `map(Array | Object | Null) -> Array`. The body's
//! per-element return type is `Any` (block bodies are not pinned by
//! FieldType today).
use crate::dsl::field_schema::FieldType;
use crate::dsl::value::Value;
use crate::functions::{FunctionRegistry, FunctionSig};

use super::block_collection_type;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "map",
        FunctionSig::fixed(&[block_collection_type()], FieldType::Array),
        |_arena, _args, _event| -> anyhow::Result<Value<'_>> {
            anyhow::bail!("map() requires a block argument: `map(arr) {{ |x| <body> }}`");
        },
    );
}
