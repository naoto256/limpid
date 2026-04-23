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
