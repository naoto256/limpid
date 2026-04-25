//! DSL: parser, AST, evaluator, and executor for the limpid configuration language.

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

pub use value::{Map, Value};

/// Construct a [`Value`] from a JSON-literal-shaped expression.
///
/// Wraps [`serde_json::json!`] and converts the result through the
/// JSON boundary (so the marker / escape rules in [`value_json`]
/// apply). Intended for tests and config-literal construction; runtime
/// hot paths should build [`Value`] directly.
#[macro_export]
macro_rules! value {
    ($($t:tt)*) => {
        $crate::dsl::value_json::json_to_value(&::serde_json::json!($($t)*))
            .expect("value! macro: invalid literal")
    };
}
