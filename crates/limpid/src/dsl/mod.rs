//! DSL: parser, AST, evaluator, and executor for the limpid configuration language.

pub mod ast;
pub mod eval;
pub mod exec;
pub mod parser;
pub mod props;
#[cfg(test)]
mod eval_test;
#[cfg(test)]
mod exec_test;

