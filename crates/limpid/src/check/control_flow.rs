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

use crate::dsl::ast::{BranchBody, IfChain, ProcessStatement, SwitchArm, SwitchStmtArm};
use crate::functions::FunctionRegistry;
use crate::modules::schema::FieldType;
use crate::pipeline::CompiledConfig;

use super::bindings::{Bindings, intersect_branches};
use super::expr_types;
use super::{DiagKind, Diagnostic, analyze_pipeline_stmt, analyze_process_stmt};

/// Reject `switch` constructs whose `default` arm isn't last or
/// appears more than once.
///
/// The runtime walks arms in source order and dispatches at the first
/// match — `default` matches everything, so any arm after a `default`
/// is unreachable. Two failure modes:
///
/// - **default not last** (e.g. `default { … } case 1 { … }`): every
///   event hits the `default` arm and the trailing arms never run.
///   Operators reading the source see "case 1 is configured" and
///   reasonably expect it to fire; silent unreachability misleads.
/// - **multiple defaults**: ambiguous, only the first runs. Almost
///   certainly an operator typo (intended one `default` plus a pattern
///   arm).
///
/// Both surface as `DiagKind::Dataflow` errors so `--check` blocks the
/// deploy. The runtime behaviour is unchanged — this is a pre-load
/// gate, not a behavioural fix.
///
/// Generic over the arm body type so the same helper covers both
/// statement-form (`SwitchStmtArm`, called from this file) and
/// expression-form (`SwitchExprArm`, called from `expr_types` and
/// `function`) switches; the check looks at `pattern.is_some()` only
/// and never touches the body.
pub(super) fn validate_switch_default_position<B>(
    arms: &[SwitchArm<B>],
    pipeline_name: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut first_default: Option<usize> = None;
    for (i, arm) in arms.iter().enumerate() {
        if arm.pattern.is_some() {
            continue;
        }
        match first_default {
            None => {
                first_default = Some(i);
                if i + 1 < arms.len() {
                    diagnostics.push(Diagnostic::error_kind(
                        DiagKind::Dataflow,
                        format!(
                            "`switch` in pipeline '{pipeline_name}': `default` arm \
                             is at position {i} of {n} but must be the last arm; \
                             the {trailing} arm(s) after `default` are unreachable \
                             (the runtime dispatches at the first match)",
                            n = arms.len(),
                            trailing = arms.len() - i - 1,
                        ),
                    ));
                }
            }
            Some(prev) => {
                diagnostics.push(Diagnostic::error_kind(
                    DiagKind::Dataflow,
                    format!(
                        "`switch` in pipeline '{pipeline_name}': multiple `default` \
                         arms (positions {prev} and {i}); only one is allowed and \
                         only the first runs"
                    ),
                ));
            }
        }
    }
}

/// `if`/`else if`/`else` chain at *pipeline* statement level — branches
/// can contain pipeline statements (`output o`, `process p`, etc.) as
/// well as inline process statements.
pub(super) fn analyze_if_chain(
    chain: &IfChain,
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    module_registry: &crate::modules::ModuleRegistry,
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
                    analyze_pipeline_stmt(
                        p,
                        pipeline_name,
                        config,
                        registry,
                        module_registry,
                        &mut b,
                        diagnostics,
                    );
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
                    analyze_pipeline_stmt(
                        p,
                        pipeline_name,
                        config,
                        registry,
                        module_registry,
                        &mut b,
                        diagnostics,
                    );
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
    arms: &[SwitchStmtArm],
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    module_registry: &crate::modules::ModuleRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    validate_switch_default_position(arms, pipeline_name, diagnostics);
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
                    analyze_pipeline_stmt(
                        p,
                        pipeline_name,
                        config,
                        registry,
                        module_registry,
                        &mut b,
                        diagnostics,
                    );
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
    arms: &[SwitchStmtArm],
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    validate_switch_default_position(arms, pipeline_name, diagnostics);
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
