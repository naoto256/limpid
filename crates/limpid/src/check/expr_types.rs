//! Static type inference and operator / function-arg type checks.
//!
//! `infer` walks an expression tree and returns the best-known
//! [`FieldType`] given the current [`Bindings`] and the function
//! registry. `check_types` walks the same tree and emits warnings for
//! operator and function-arg mismatches it can pin down precisely. The
//! two are used in tandem: `check_types` consumes the inferred types
//! and folds them into diagnostics, while `infer` is also called
//! independently when `Assign` needs the RHS type.
//!
//! `Any` is the loose-fit escape hatch on both sides — it suppresses
//! warnings rather than producing false positives. The analyzer favors
//! silence over noise; precision improves with each parser / function
//! whose signature is registered.

use crate::dsl::ast::{BinOp, Expr, ExprKind, TemplateFragment, UnaryOp};
use crate::dsl::span::Span;
use crate::functions::{Arity, FunctionRegistry, FunctionSig};
use crate::modules::schema::{FieldType, type_compatible};

use super::bindings::Bindings;
use super::suggestions;
use super::{Diagnostic, Level};

// ---------------------------------------------------------------------------
// Type inference
// ---------------------------------------------------------------------------

/// Infer the static type of an expression in the context of `bindings`
/// and the function `registry`. Anything the analyzer can't pin down
/// falls back to `FieldType::Any`.
pub fn infer(expr: &Expr, bindings: &Bindings, registry: &FunctionRegistry) -> FieldType {
    match &expr.kind {
        ExprKind::StringLit(_) | ExprKind::Template(_) => FieldType::String,
        ExprKind::IntLit(_) => FieldType::Int,
        ExprKind::FloatLit(_) => FieldType::Float,
        ExprKind::BoolLit(_) => FieldType::Bool,
        ExprKind::Null => FieldType::Null,
        ExprKind::HashLit(_) => FieldType::Object,
        ExprKind::Ident(parts) => ident_type(parts, bindings),
        ExprKind::PropertyAccess(base, suffix) => {
            // `geoip(x).country.name` — collapse to a workspace lookup
            // when the base is a bare ident chain we can resolve.
            if let ExprKind::Ident(base_parts) = &base.kind {
                let mut combined = base_parts.clone();
                combined.extend(suffix.iter().cloned());
                ident_type(&combined, bindings)
            } else {
                FieldType::Any
            }
        }
        ExprKind::FuncCall {
            namespace,
            name,
            args: _,
        } => registry
            .signature(namespace.as_deref(), name)
            .map(|s| s.ret.clone())
            .unwrap_or(FieldType::Any),
        ExprKind::BinOp(_l, op, _r) => match op {
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                FieldType::Bool
            }
            BinOp::And | BinOp::Or => FieldType::Bool,
            // Arithmetic: stays Any to dodge cascading false positives.
            // The dedicated operator check still flags illegal combos.
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => FieldType::Any,
        },
        ExprKind::UnaryOp(op, inner) => match op {
            UnaryOp::Not => FieldType::Bool,
            UnaryOp::Neg => infer(inner, bindings, registry),
        },
    }
}

/// Resolve a dotted identifier path against `bindings` plus the fixed
/// reserved-event-ident table.
///
/// Reserved idents (always present, fixed type):
/// - `ingress` / `egress` — String (raw bytes UTF-8-decoded)
/// - `source` — String (peer IP)
/// - `timestamp` — Timestamp
/// - `error` — String (`workspace._error` shortcut inside catch)
///
/// Single-segment idents that aren't reserved fall through to `let`
/// scope. Anything else lands in `workspace.*` (or `Any` if unknown).
fn ident_type(parts: &[String], bindings: &Bindings) -> FieldType {
    if parts.is_empty() {
        return FieldType::Any;
    }
    // Reserved event idents: always exist, fixed type.
    match parts.first().map(String::as_str) {
        Some("ingress") if parts.len() == 1 => return FieldType::String,
        Some("egress") if parts.len() == 1 => return FieldType::String,
        Some("source") if parts.len() == 1 => return FieldType::String,
        Some("timestamp") if parts.len() == 1 => return FieldType::Timestamp,
        Some("error") if parts.len() == 1 => return FieldType::String,
        Some("workspace") => {
            // Direct workspace lookup. Try exact path; fall back to
            // ancestor (Object); then wildcard.
            if parts.len() == 1 {
                return FieldType::Object; // bare `workspace` is the whole map
            }
            if let Some(t) = bindings.get_workspace(parts) {
                return t.clone();
            }
            // Walk up looking for an ancestor binding (Object).
            for i in (2..parts.len()).rev() {
                if bindings.get_workspace(&parts[..i]).is_some() {
                    return FieldType::Any;
                }
            }
            // Unknown leaf — Any prevents false positives in type
            // checks. The presence check (visible-from-output) is
            // separate and runs on `workspace_visible`.
            return FieldType::Any;
        }
        _ => {}
    }
    // Single-segment: try `let` scope.
    if parts.len() == 1
        && let Some(t) = bindings.get_let(&parts[0])
    {
        return t.clone();
    }
    // Unknown — `Any` to avoid cascading false positives from the type
    // checks. The dedicated "unknown identifier" diagnostic surfaces in
    // a future commit (Phase 3 UX).
    FieldType::Any
}

// ---------------------------------------------------------------------------
// Type checks
// ---------------------------------------------------------------------------

/// Walk `expr` and emit warnings for operator and function-arg
/// mismatches. Recurses into sub-expressions so nested calls are
/// checked too.
///
/// The `fallback_span` parameter is used when the analyzer wants to
/// anchor the whole tree to a coarser location (e.g. the `value_span`
/// of an output property). Individual warnings still prefer the tight
/// sub-expression span from the AST — carried on each [`Expr`] since
/// Block 11 — and only fall back to this coarser span when the parser
/// couldn't attribute a precise source range (e.g. synthesized AST
/// rebuilds in the analyzer that use [`Expr::spanless`]).
pub fn check_types(
    expr: &Expr,
    pipeline_name: &str,
    bindings: &Bindings,
    registry: &FunctionRegistry,
    fallback_span: Option<Span>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match &expr.kind {
        ExprKind::BinOp(l, op, r) => {
            check_types(
                l,
                pipeline_name,
                bindings,
                registry,
                fallback_span,
                diagnostics,
            );
            check_types(
                r,
                pipeline_name,
                bindings,
                registry,
                fallback_span,
                diagnostics,
            );
            // Precise span for operator-type mismatches: the whole
            // BinOp sub-tree covers `[l.span.start, r.span.end)` — the
            // parser sets that on `expr` itself in `fold_by_precedence`.
            let span = prefer_span(expr, fallback_span);
            check_binop(
                l,
                *op,
                r,
                pipeline_name,
                bindings,
                registry,
                span,
                diagnostics,
            );
        }
        ExprKind::UnaryOp(_op, inner) => {
            check_types(
                inner,
                pipeline_name,
                bindings,
                registry,
                fallback_span,
                diagnostics,
            );
        }
        ExprKind::FuncCall {
            namespace,
            name,
            args,
        } => {
            for a in args {
                check_types(
                    a,
                    pipeline_name,
                    bindings,
                    registry,
                    fallback_span,
                    diagnostics,
                );
            }
            // Function-call-level diagnostic (unknown function, arg
            // type mismatch) anchors to the call expression itself; the
            // per-argument diagnostic below prefers the individual arg
            // span for tight carets.
            let call_span = prefer_span(expr, fallback_span);
            check_fn_call(
                namespace.as_deref(),
                name,
                args,
                pipeline_name,
                bindings,
                registry,
                call_span,
                diagnostics,
            );
        }
        ExprKind::Template(fragments) => {
            for f in fragments {
                if let TemplateFragment::Interp(e) = f {
                    check_types(
                        e,
                        pipeline_name,
                        bindings,
                        registry,
                        fallback_span,
                        diagnostics,
                    );
                }
            }
        }
        ExprKind::HashLit(entries) => {
            for (_k, v) in entries {
                check_types(
                    v,
                    pipeline_name,
                    bindings,
                    registry,
                    fallback_span,
                    diagnostics,
                );
            }
        }
        ExprKind::PropertyAccess(base, _) => {
            check_types(
                base,
                pipeline_name,
                bindings,
                registry,
                fallback_span,
                diagnostics,
            );
        }
        ExprKind::StringLit(_)
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::BoolLit(_)
        | ExprKind::Null
        | ExprKind::Ident(_) => {}
    }
}

/// Prefer the AST-carried span on `expr` when it isn't the
/// [`Span::dummy`] placeholder; otherwise fall back to the caller-
/// supplied coarser span. Keeps synthesized expressions (test fixtures,
/// analyzer rebuilds) from emitting diagnostics anchored to garbage
/// offsets.
fn prefer_span(expr: &Expr, fallback: Option<Span>) -> Option<Span> {
    if expr.span.file_id == u32::MAX {
        fallback
    } else {
        Some(expr.span)
    }
}

#[allow(clippy::too_many_arguments)]
fn check_binop(
    l: &Expr,
    op: BinOp,
    r: &Expr,
    pipeline_name: &str,
    bindings: &Bindings,
    registry: &FunctionRegistry,
    span: Option<Span>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let lt = infer(l, bindings, registry);
    let rt = infer(r, bindings, registry);

    if lt.is_any() || rt.is_any() {
        return;
    }

    match op {
        BinOp::Eq | BinOp::Ne => {
            if !type_compatible(&lt, &rt) && !type_compatible(&rt, &lt) {
                diagnostics.push(warning(
                    pipeline_name,
                    format!(
                        "comparison `{}` between {} and {} always evaluates the same; types don't match",
                        binop_display(op),
                        lt.display(),
                        rt.display()
                    ),
                    span,
                ));
            }
        }
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            let ok_l = lt.is_numeric() || matches!(lt, FieldType::Timestamp);
            let ok_r = rt.is_numeric() || matches!(rt, FieldType::Timestamp);
            if !(ok_l && ok_r) {
                diagnostics.push(warning(
                    pipeline_name,
                    format!(
                        "ordering comparison `{}` between {} and {} is not numeric or temporal",
                        binop_display(op),
                        lt.display(),
                        rt.display()
                    ),
                    span,
                ));
            }
        }
        BinOp::Add => {
            // `+` is numeric addition or string concat. Warn only if
            // neither shape applies.
            let both_numeric = lt.is_numeric() && rt.is_numeric();
            let any_string = lt.is_string() || rt.is_string();
            if !both_numeric && !any_string {
                diagnostics.push(warning(
                    pipeline_name,
                    format!(
                        "`+` between {} and {} is neither numeric addition nor string concatenation",
                        lt.display(),
                        rt.display()
                    ),
                    span,
                ));
            }
        }
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            if !(lt.is_numeric() && rt.is_numeric()) {
                diagnostics.push(warning(
                    pipeline_name,
                    format!(
                        "arithmetic `{}` requires numeric operands, got {} and {}",
                        binop_display(op),
                        lt.display(),
                        rt.display()
                    ),
                    span,
                ));
            }
        }
        BinOp::And | BinOp::Or => {
            // Runtime truthiness is permissive — anything goes.
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_fn_call(
    namespace: Option<&str>,
    name: &str,
    args: &[Expr],
    pipeline_name: &str,
    bindings: &Bindings,
    registry: &FunctionRegistry,
    span: Option<Span>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(sig) = registry.signature(namespace, name) else {
        // Unknown function — emit a hint with a near-match if any
        // registered name is close. Skipped for namespaced calls because
        // user-defined namespaces aren't enumerable here yet.
        if namespace.is_none()
            && let Some(near) = suggestions::near_function_name(name, registry)
        {
            let mut diag = Diagnostic::warning(format!(
                "[pipeline {}] call to unknown function `{}`",
                pipeline_name, name
            ))
            .with_span(span);
            diag = diag.with_help(format!("did you mean `{}`?", near));
            diagnostics.push(diag);
        }
        return;
    };
    if !arity_in_range(sig, args.len()) {
        // Wrong-arity calls are caught loudly at runtime; double-flagging
        // is just noise. Skip.
        return;
    }
    for (i, actual) in args.iter().enumerate() {
        let expected = expected_arg_type(sig, i);
        let actual_ty = infer(actual, bindings, registry);
        if !type_compatible(expected, &actual_ty) {
            let display = qualified_name(namespace, name);
            // Prefer the tight per-argument span from the AST; fall
            // back to the call-level span only when the arg came from
            // a synthesized rebuild (`Expr::spanless`).
            let arg_span = prefer_span(actual, span);
            diagnostics.push(warning(
                pipeline_name,
                format!(
                    "function `{}` argument {} expects {}, got {}",
                    display,
                    i + 1,
                    expected.display(),
                    actual_ty.display()
                ),
                arg_span,
            ));
        }
    }
}

fn arity_in_range(sig: &FunctionSig, n: usize) -> bool {
    match sig.arity {
        Arity::Fixed => n == sig.args.len(),
        Arity::Optional { required } => n >= required && n <= sig.args.len(),
        Arity::Variadic => n + 1 >= sig.args.len(), // declared - 1 are required positional, then variadic tail
    }
}

fn expected_arg_type(sig: &FunctionSig, i: usize) -> &FieldType {
    match sig.arity {
        Arity::Fixed | Arity::Optional { .. } => &sig.args[i.min(sig.args.len().saturating_sub(1))],
        Arity::Variadic => {
            if i < sig.args.len() - 1 {
                &sig.args[i]
            } else {
                sig.args.last().unwrap()
            }
        }
    }
}

fn qualified_name(namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{}.{}", ns, name),
        None => name.to_string(),
    }
}

fn binop_display(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
    }
}

fn warning(pipeline: &str, message: String, span: Option<Span>) -> Diagnostic {
    Diagnostic {
        level: Level::Warning,
        message: format!("[pipeline {}] {}", pipeline, message),
        span,
        help: None,
    }
}
