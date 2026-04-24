//! Branch / control-flow analysis for `if`/`else if`/`else`, `switch`,
//! `try/catch`, and `for_each` constructs.
//!
//! Each branch is analyzed in a clone of the entry bindings; at the
//! join point the bindings reduce to the intersection (a key survives
//! iff every branch produced it with a compatible type). Implicit
//! "no-match" branches (an `if` without `else`, a `switch` without a
//! default) reuse the starting bindings to model "no work happened".
//!
//! Catch bodies pre-bind `workspace._error` as `String` to mirror the
//! runtime convention.

use crate::dsl::ast::{BranchBody, IfChain, ProcessStatement, SwitchArm};
use crate::functions::FunctionRegistry;
use crate::modules::schema::FieldType;
use crate::pipeline::CompiledConfig;

use super::bindings::{Bindings, intersect_branches};
use super::expr_types;
use super::{Diagnostic, analyze_pipeline_stmt, analyze_process_stmt};

/// `if`/`else if`/`else` chain at *pipeline* statement level — branches
/// can contain pipeline statements (`output o`, `process p`, etc.) as
/// well as inline process statements.
pub(super) fn analyze_if_chain(
    chain: &IfChain,
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(chain.branches.len() + 1);
    for (cond, body) in &chain.branches {
        expr_types::check_types(cond, pipeline_name, &starting, registry, None, diagnostics);
        let mut b = starting.clone();
        for item in body {
            match item {
                BranchBody::Pipeline(p) => {
                    analyze_pipeline_stmt(p, pipeline_name, config, registry, &mut b, diagnostics);
                }
                BranchBody::Process(s) => {
                    analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
                }
            }
        }
        results.push(b);
    }
    if let Some(else_body) = &chain.else_body {
        let mut b = starting.clone();
        for item in else_body {
            match item {
                BranchBody::Pipeline(p) => {
                    analyze_pipeline_stmt(p, pipeline_name, config, registry, &mut b, diagnostics);
                }
                BranchBody::Process(s) => {
                    analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
                }
            }
        }
        results.push(b);
    } else {
        // No else → the "no match" path keeps the starting bindings,
        // which forces the intersection to drop any branch-only adds.
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

/// `switch` at *pipeline* level. A `switch` with a default arm covers
/// every input; without one we add the starting bindings as the implicit
/// "fell through" branch.
pub(super) fn analyze_switch(
    arms: &[SwitchArm],
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(arms.len() + 1);
    let mut has_default = false;
    for arm in arms {
        if let Some(p) = &arm.pattern {
            expr_types::check_types(p, pipeline_name, &starting, registry, None, diagnostics);
        } else {
            has_default = true;
        }
        let mut b = starting.clone();
        for item in &arm.body {
            match item {
                BranchBody::Pipeline(p) => {
                    analyze_pipeline_stmt(p, pipeline_name, config, registry, &mut b, diagnostics);
                }
                BranchBody::Process(s) => {
                    analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
                }
            }
        }
        results.push(b);
    }
    if !has_default {
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

/// `if` chain inside a process body — branches contain only process
/// statements (BranchBody::Pipeline arms are ignored, matching prior
/// inline behaviour).
pub(super) fn analyze_inline_if(
    chain: &IfChain,
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(chain.branches.len() + 1);
    for (cond, body) in &chain.branches {
        expr_types::check_types(cond, pipeline_name, &starting, registry, None, diagnostics);
        let mut b = starting.clone();
        b.push_let_scope();
        for item in body {
            if let BranchBody::Process(s) = item {
                analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
            }
        }
        b.pop_let_scope();
        results.push(b);
    }
    if let Some(else_body) = &chain.else_body {
        let mut b = starting.clone();
        b.push_let_scope();
        for item in else_body {
            if let BranchBody::Process(s) = item {
                analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
            }
        }
        b.pop_let_scope();
        results.push(b);
    } else {
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

/// `switch` inside a process body — process-statement bodies only.
pub(super) fn analyze_inline_switch(
    arms: &[SwitchArm],
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(arms.len() + 1);
    let mut has_default = false;
    for arm in arms {
        if let Some(p) = &arm.pattern {
            expr_types::check_types(p, pipeline_name, &starting, registry, None, diagnostics);
        } else {
            has_default = true;
        }
        let mut b = starting.clone();
        b.push_let_scope();
        for item in &arm.body {
            if let BranchBody::Process(s) = item {
                analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
            }
        }
        b.pop_let_scope();
        results.push(b);
    }
    if !has_default {
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

/// `try { … } catch { … }` — alternate branches; `catch` body starts
/// with `workspace._error: String` pre-bound.
pub(super) fn analyze_try_catch(
    try_body: &[ProcessStatement],
    catch_body: &[ProcessStatement],
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();

    let mut try_b = starting.clone();
    try_b.push_let_scope();
    for s in try_body {
        analyze_process_stmt(s, pipeline_name, registry, &mut try_b, diagnostics);
    }
    try_b.pop_let_scope();

    let mut catch_b = starting.clone();
    catch_b.push_let_scope();
    catch_b.bind_workspace(&["workspace".into(), "_error".into()], FieldType::String);
    for s in catch_body {
        analyze_process_stmt(s, pipeline_name, registry, &mut catch_b, diagnostics);
    }
    catch_b.pop_let_scope();

    *bindings = intersect_branches(&[try_b, catch_b]);
}

/// `for_each` body — may not run at all, so any new bindings must
/// intersect with "skipped" (the starting bindings).
pub(super) fn analyze_for_each(
    body: &[ProcessStatement],
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut iter_b = starting.clone();
    iter_b.push_let_scope();
    for s in body {
        analyze_process_stmt(s, pipeline_name, registry, &mut iter_b, diagnostics);
    }
    iter_b.pop_let_scope();
    *bindings = intersect_branches(&[iter_b, starting]);
}
