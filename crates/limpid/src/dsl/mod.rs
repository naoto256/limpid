//! DSL: parser, AST, evaluator, and executor for the limpid configuration language.

pub mod arena;
pub mod ast;
pub mod eval;
#[cfg(test)]
mod eval_test;
pub mod exec;
#[cfg(test)]
mod exec_test;
pub mod parser;
pub mod props;
pub mod span;
pub mod value;
pub mod value_json;

// `OwnedValue` is re-exported here so the rest of the crate (and any
// future external consumers of the DSL surface) can spell
// `crate::dsl::OwnedValue` rather than the more verbose
// `crate::dsl::value::OwnedValue`. The unused-import warning fires
// only on builds where no module spells the short form; current
// callers (`queue::disk` etc.) do, so the lint is silenced here in
// case future cleanups inline those references.
#[allow(unused_imports)]
pub use value::OwnedValue;

/// Construct an [`OwnedValue`] from a JSON-literal-shaped expression.
///
/// Wraps [`serde_json::json!`] and converts the result through the JSON
/// boundary (so the marker / escape rules in [`value_json`] apply).
/// Intended for tests and config-literal construction; pipeline hot
/// paths build arena-backed [`Value`] directly via the
/// `value::ObjectBuilder` / `ArrayBuilder` helpers.
///
/// Returns `OwnedValue` (not `Value<'bump>`) because the macro has no
/// arena in scope. Tests that need a borrowed view can chain
/// `.view_in(&arena)`.
#[macro_export]
macro_rules! value {
    ($($t:tt)*) => {
        $crate::dsl::value_json::json_to_value(&::serde_json::json!($($t)*))
            .expect("value! macro: invalid literal")
    };
}
