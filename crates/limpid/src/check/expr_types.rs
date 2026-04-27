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
use super::{DiagKind, Diagnostic, Level};

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
        // Array literal infers to `Array`. Element-type refinement
        // (`Array<String>` etc.) is deferred to v0.5.x — the element
        // type surfaced at every read site is `Any` for now.
        ExprKind::ArrayLit(_) => FieldType::Array,
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
        ExprKind::SwitchExpr { arms, .. } => {
            // Switch expression's type is the union of all arm bodies'
            // types. With no default arm, `Null` joins the union to
            // cover the no-match case.
            let mut has_default = false;
            let mut joined: Option<FieldType> = None;
            for arm in arms {
                if arm.pattern.is_none() {
                    has_default = true;
                }
                let arm_ty = infer(&arm.body, bindings, registry);
                joined = Some(match joined {
                    None => arm_ty,
                    Some(prev) => FieldType::union(prev, arm_ty),
                });
            }
            let result = joined.unwrap_or(FieldType::Null);
            if has_default {
                result
            } else {
                FieldType::union(result, FieldType::Null)
            }
        }
    }
}

/// Resolve a dotted identifier path against `bindings` plus the fixed
/// reserved-event-ident table.
///
/// Reserved idents (always present, fixed type):
/// - `ingress` / `egress` — String (raw bytes UTF-8-decoded)
/// - `source` — String (peer IP)
/// - `received_at` — Timestamp (wall-clock at which this hop received
///   the event; `timestamp` was the pre-0.5 name and is no longer
///   reserved)
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
        Some("received_at") if parts.len() == 1 => return FieldType::Timestamp,
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
    // Unknown — return `Any` to avoid cascading type-mismatch false
    // positives. The dedicated "unknown identifier" diagnostic is
    // emitted by `check_types` (see `check_unknown_ident` below) so
    // each unresolved reference surfaces as a distinct warning instead
    // of a chain of typed-operator complaints.
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
        ExprKind::ArrayLit(items) => {
            for item in items {
                check_types(
                    item,
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
        ExprKind::Ident(parts) => {
            check_unknown_ident(
                parts,
                pipeline_name,
                bindings,
                prefer_span(expr, fallback_span),
                diagnostics,
            );
        }
        ExprKind::SwitchExpr { scrutinee, arms } => {
            check_types(
                scrutinee,
                pipeline_name,
                bindings,
                registry,
                fallback_span,
                diagnostics,
            );
            for arm in arms {
                if let Some(pat) = &arm.pattern {
                    check_types(
                        pat,
                        pipeline_name,
                        bindings,
                        registry,
                        fallback_span,
                        diagnostics,
                    );
                }
                check_types(
                    &arm.body,
                    pipeline_name,
                    bindings,
                    registry,
                    fallback_span,
                    diagnostics,
                );
            }
        }
        ExprKind::StringLit(_)
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::BoolLit(_)
        | ExprKind::Null => {}
    }
}

/// Emit a warning for an identifier reference that doesn't resolve to
/// any reserved event ident, `let` binding, or `workspace.*` path.
///
/// Multi-segment idents starting with `workspace` are skipped here —
/// the dataflow check (`outputs::check_workspace_visibility`) owns the
/// "workspace key not produced upstream" diagnostic so we don't
/// double-flag.
///
/// Special-case: bare `timestamp` was the reserved Event-time ident
/// pre-0.5; surface a targeted hint pointing at the rename rather
/// than a generic "did you mean" miss (the levenshtein distance from
/// `timestamp` to `received_at` is too far for the suggestion engine
/// to reach on its own).
fn check_unknown_ident(
    parts: &[String],
    pipeline_name: &str,
    bindings: &Bindings,
    span: Option<Span>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if parts.is_empty() {
        return;
    }
    let first = parts[0].as_str();
    // Reserved single-segment idents.
    if parts.len() == 1
        && matches!(
            first,
            "ingress" | "egress" | "source" | "received_at" | "error"
        )
    {
        return;
    }
    // `workspace.*` paths are handled by the dataflow visibility pass.
    if first == "workspace" {
        return;
    }
    // `let` bindings.
    if parts.len() == 1 && bindings.get_let(first).is_some() {
        return;
    }
    // Unresolved — emit a warning. Tailor the message for the most
    // common 0.4→0.5 migration miss.
    let dotted = parts.join(".");
    let (message, help) = if parts.len() == 1 && first == "timestamp" {
        (
            format!(
                "[pipeline {}] bare `timestamp` is no longer a reserved identifier (renamed in v0.5.0)",
                pipeline_name
            ),
            Some(
                "use `received_at` for the wall-clock event time, or `timestamp()` for the current instant"
                    .to_string(),
            ),
        )
    } else {
        let near = if parts.len() == 1 {
            suggestions::near_workspace_path(first, bindings)
        } else {
            None
        };
        (
            format!(
                "[pipeline {}] unknown identifier `{}`",
                pipeline_name, dotted
            ),
            near.map(|n| format!("did you mean `{}`?", n)),
        )
    };
    let mut diag = Diagnostic::warning_kind(DiagKind::UnknownIdent, message).with_span(span);
    if let Some(h) = help {
        diag = diag.with_help(h);
    }
    diagnostics.push(diag);
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
            let mut diag = Diagnostic::warning_kind(
                DiagKind::UnknownIdent,
                format!(
                    "[pipeline {}] call to unknown function `{}`",
                    pipeline_name, name
                ),
            )
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
    }
}

fn expected_arg_type(sig: &FunctionSig, i: usize) -> &FieldType {
    &sig.args[i.min(sig.args.len().saturating_sub(1))]
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
    // All diagnostics produced by this helper come from
    // `check_binop` / `check_fn_call` arg-type branches, both of which
    // are type-mismatch findings. Tag accordingly so `--ultra-strict`
    // leaves them as warnings (only UnknownIdent gets promoted).
    Diagnostic {
        level: Level::Warning,
        kind: DiagKind::TypeMismatch,
        message: format!("[pipeline {}] {}", pipeline, message),
        span,
        help: None,
    }
}
