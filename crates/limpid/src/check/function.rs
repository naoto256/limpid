//! Static analysis for `def function` declarations.
//!
//! User-defined functions are pure expression-returning units: the
//! body operates on its arguments, calls other (pure) functions, and
//! produces a value. To uphold that contract, the analyzer rejects
//! function bodies that:
//!
//! - reference any Event-bound identifier (`ingress`, `egress`,
//!   `source`, `received_at`, `error`, `workspace.*`),
//! - call into the user's `def process` declarations, and
//! - participate in a function-to-function call cycle (direct
//!   self-recursion or mutual recursion through a chain of calls).
//!
//! The runtime trusts these checks — `eval_expr_with_scope` does not
//! re-validate Event access or recursion at call time, so all
//! enforcement happens here.
//!
//! Built-in primitive calls are always allowed; the side-effect-free
//! ones (`lower`, `regex_match`, `to_int`, …) compose naturally with
//! mapping functions, and the few stateful ones (`hostname()`,
//! `timestamp()`) are pragmatically permitted because they don't
//! depend on Event and are useful even in pure contexts.

use std::collections::{HashMap, HashSet};

use crate::dsl::ast::{Expr, ExprKind, FunctionDef, walk_children};
use crate::pipeline::CompiledConfig;

use super::{DiagKind, Diagnostic, Level};

/// Reserved Event-bound identifier names. A function body that reads
/// any of these is rejected — the function is supposed to be a pure
/// transformation of its arguments, and pulling Event state in via
/// these names would silently couple the function to its caller's
/// pipeline context.
const EVENT_IDENTS: &[&str] = &[
    "ingress",
    "egress",
    "source",
    "received_at",
    "error",
    "workspace",
];

/// Walk every `def function` in `config`, emit diagnostics for purity
/// violations and call-graph cycles. Caller (top-level `analyze`) is
/// responsible for splicing the resulting diagnostics into the
/// existing pipeline-level diagnostic stream.
pub(super) fn check_all_functions(config: &CompiledConfig, diagnostics: &mut Vec<Diagnostic>) {
    // First pass: per-function purity (Event refs + process calls).
    for fn_def in config.functions.values() {
        check_function_body(fn_def, config, diagnostics);
    }

    // Second pass: call-graph cycle detection across all user
    // functions. Done globally so mutual recursion (A → B → A) is
    // caught even though no single function looks recursive in
    // isolation.
    check_function_cycles(config, diagnostics);
}

fn check_function_body(
    fn_def: &FunctionDef,
    config: &CompiledConfig,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // `known` accumulates names that are valid single-segment
    // references at this point in the body: parameters first, then each
    // `let` name as it appears (let RHS sees only what's bound *before*
    // it, so we extend after walking the RHS).
    let mut known: HashSet<String> = fn_def.params.iter().cloned().collect();
    for fl in &fn_def.body.lets {
        walk_for_purity(&fl.value, &fn_def.name, &known, config, diagnostics);
        known.insert(fl.name.clone());
    }
    walk_for_purity(&fn_def.body.ret, &fn_def.name, &known, config, diagnostics);
}

fn walk_for_purity(
    expr: &Expr,
    fn_name: &str,
    params: &HashSet<String>,
    config: &CompiledConfig,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::Ident(parts) => {
            // Bare ident may be a parameter — those are the only
            // single-segment names a function body can read.
            if parts.len() == 1 && params.contains(&parts[0]) {
                return;
            }
            // Otherwise, any reference whose head matches an Event
            // ident (single-segment `ingress` / `egress` / … or any
            // `workspace.*` path) is a purity violation.
            let head = parts.first().map(String::as_str);
            if let Some(h) = head
                && EVENT_IDENTS.contains(&h)
            {
                diagnostics.push(
                    Diagnostic::error_kind(
                        DiagKind::UnknownIdent,
                        format!(
                            "[function {}] body references Event-bound identifier `{}` — \
                             functions are pure and may only read their parameters",
                            fn_name,
                            parts.join(".")
                        ),
                    )
                    .with_span(Some(expr.span))
                    .with_help(
                        "rewrite the call site to pass the value as an argument; \
                         functions cannot read from the surrounding event"
                            .to_string(),
                    ),
                );
                return;
            }
            // Single-segment non-parameter reference is unknown
            // (function scope has no `let` bindings yet, so anything
            // unrecognised is a free variable).
            if parts.len() == 1 {
                diagnostics.push(
                    Diagnostic::error_kind(
                        DiagKind::UnknownIdent,
                        format!(
                            "[function {}] body references unknown identifier `{}` — \
                             not a parameter",
                            fn_name, parts[0]
                        ),
                    )
                    .with_span(Some(expr.span)),
                );
                return;
            }
            // Multi-segment path whose head is neither a parameter nor
            // an Event-bound name is a free variable too — the head
            // would fail to resolve at runtime (`config.foo`, `ctx.x`,
            // …). Surface it now instead of letting the event end up
            // in the error_log.
            if !params.contains(&parts[0]) {
                diagnostics.push(
                    Diagnostic::error_kind(
                        DiagKind::UnknownIdent,
                        format!(
                            "[function {}] body references unknown path `{}` — \
                             head `{}` is neither a parameter nor an Event-bound \
                             identifier",
                            fn_name,
                            parts.join("."),
                            parts[0]
                        ),
                    )
                    .with_span(Some(expr.span)),
                );
            }
        }
        ExprKind::FuncCall {
            namespace,
            name,
            args,
        } => {
            // Disallow calls to user-defined `def process` declarations
            // — process bodies have side effects that pure functions
            // cannot tolerate.
            if namespace.is_none() && config.processes.contains_key(name) {
                diagnostics.push(
                    Diagnostic::error_kind(
                        DiagKind::UnknownIdent,
                        format!(
                            "[function {}] body calls process `{}` — \
                             functions can only call other functions and built-in primitives",
                            fn_name, name
                        ),
                    )
                    .with_span(Some(expr.span)),
                );
            }
            for a in args {
                walk_for_purity(a, fn_name, params, config, diagnostics);
            }
        }
        // All other variants delegate recursion to `walk_children`;
        // their own structure carries no purity-relevant signal.
        _ => walk_children(expr, |child| {
            walk_for_purity(child, fn_name, params, config, diagnostics)
        }),
    }
}

/// Detect call cycles among user-defined functions. A cycle is
/// reported as a single diagnostic per cycle, naming all participants
/// so the operator can fix any one link.
fn check_function_cycles(config: &CompiledConfig, diagnostics: &mut Vec<Diagnostic>) {
    // Build adjacency: function name → list of user-function names it
    // calls (built-in primitive calls don't enter the graph).
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, fn_def) in &config.functions {
        let mut callees = Vec::new();
        for fl in &fn_def.body.lets {
            collect_user_callees(&fl.value, config, &mut callees);
        }
        collect_user_callees(&fn_def.body.ret, config, &mut callees);
        adj.insert(name.as_str(), callees);
    }

    // Standard DFS cycle detection. Mark colors: 0=unvisited,
    // 1=on-stack, 2=done. A back-edge to an on-stack node is a cycle.
    let mut color: HashMap<&str, u8> = adj.keys().map(|k| (*k, 0u8)).collect();
    let mut stack: Vec<&str> = Vec::new();
    let mut reported: HashSet<Vec<String>> = HashSet::new();
    let names: Vec<&str> = adj.keys().copied().collect();
    for name in names {
        if color[&name] == 0 {
            dfs_cycle(
                name,
                &adj,
                &mut color,
                &mut stack,
                &mut reported,
                diagnostics,
            );
        }
    }
}

fn collect_user_callees<'a>(expr: &'a Expr, config: &'a CompiledConfig, out: &mut Vec<&'a str>) {
    if let ExprKind::FuncCall {
        namespace,
        name,
        args,
    } = &expr.kind
    {
        if namespace.is_none()
            && let Some((stored_name, _)) = config.functions.get_key_value(name)
        {
            out.push(stored_name.as_str());
        }
        for a in args {
            collect_user_callees(a, config, out);
        }
        return;
    }
    walk_children(expr, |child| collect_user_callees(child, config, out));
}

fn dfs_cycle<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    color: &mut HashMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
    reported: &mut HashSet<Vec<String>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    color.insert(node, 1);
    stack.push(node);
    if let Some(callees) = adj.get(node) {
        for &callee in callees {
            match color.get(callee).copied().unwrap_or(0) {
                0 => dfs_cycle(callee, adj, color, stack, reported, diagnostics),
                1 => {
                    // Back-edge: extract the cycle slice from the stack.
                    let pos = stack.iter().position(|n| n == &callee).unwrap_or(0);
                    let cycle: Vec<String> = stack[pos..].iter().map(|s| s.to_string()).collect();
                    // Canonicalise so A→B→A and B→A→B report once.
                    let mut canon = cycle.clone();
                    let min_idx = canon
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, n)| n.as_str())
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    canon.rotate_left(min_idx);
                    if reported.insert(canon.clone()) {
                        let path = if cycle.len() == 1 {
                            format!("`{}` calls itself", cycle[0])
                        } else {
                            format!("`{}` → `{}`", cycle.join("` → `"), cycle[0],)
                        };
                        diagnostics.push(Diagnostic {
                            level: Level::Error,
                            kind: DiagKind::Other,
                            message: format!(
                                "function call cycle detected: {}; recursion in `def function` is not supported",
                                path
                            ),
                            span: None,
                            help: Some(
                                "use `def process` if you genuinely need recursion; otherwise rewrite the chain to be acyclic"
                                    .to_string(),
                            ),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    color.insert(node, 2);
    stack.pop();
}
