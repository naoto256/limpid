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

pub use value::{Map, OwnedValue, Value};

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
