//! Output-side reference checks: every `workspace.*` reference in an
//! output property must correspond to a key produced upstream.
//!
//! Walks every property's value expression, collects the workspace
//! references (idents, property accesses, template `${…}` interps,
//! function-call args, binary/unary subexpressions, hash literals),
//! and emits an Error per unresolved key. Levenshtein-based "did you
//! mean" hints are attached when a near match exists.

use crate::dsl::ast::{Expr, ExprKind, OutputDef, Property, TemplateFragment};
use crate::dsl::span::Span;
use crate::functions::FunctionRegistry;

use super::bindings::Bindings;
use super::{Diagnostic, expr_types, suggestions};

pub(super) fn analyze_output(
    output: &OutputDef,
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for prop in &output.properties {
        if let Property::KeyValue {
            value: expr,
            value_span,
            ..
        } = prop
        {
            expr_types::check_types(
                expr,
                pipeline_name,
                bindings,
                registry,
                *value_span,
                diagnostics,
            );
            collect_workspace_refs(expr, &mut |path| {
                check_workspace_reference(
                    path,
                    &output.name,
                    pipeline_name,
                    bindings,
                    *value_span,
                    diagnostics,
                );
            });
        }
    }
}

fn check_workspace_reference(
    path: &[String],
    output_name: &str,
    pipeline_name: &str,
    bindings: &Bindings,
    span: Option<Span>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Only `workspace.*` references with at least one segment under
    // workspace are interesting — reserved idents (ingress / source /
    // timestamp / error / egress) are always present.
    if path.first().map(String::as_str) != Some("workspace") || path.len() < 2 {
        return;
    }
    if !bindings.workspace_visible(path) {
        let joined = path.join(".");
        let mut diag = Diagnostic::error(format!(
            "[pipeline {}] output `{}` references `{}` which is not produced by any upstream module",
            pipeline_name, output_name, joined,
        ))
        .with_span(span);
        if let Some(near) = suggestions::near_workspace_path(&joined, bindings) {
            diag = diag.with_help(format!("did you mean `{}`?", near));
        }
        diagnostics.push(diag);
    }
}

fn collect_workspace_refs(expr: &Expr, cb: &mut dyn FnMut(&[String])) {
    match &expr.kind {
        ExprKind::Ident(parts) => cb(parts),
        ExprKind::PropertyAccess(base, suffix) => {
            if let ExprKind::Ident(base_parts) = &base.kind {
                let mut combined = base_parts.clone();
                combined.extend(suffix.iter().cloned());
                cb(&combined);
            } else {
                collect_workspace_refs(base, cb);
            }
        }
        ExprKind::Template(fragments) => {
            for f in fragments {
                if let TemplateFragment::Interp(e) = f {
                    collect_workspace_refs(e, cb);
                }
            }
        }
        ExprKind::FuncCall { args, .. } => {
            for a in args {
                collect_workspace_refs(a, cb);
            }
        }
        ExprKind::BinOp(l, _, r) => {
            collect_workspace_refs(l, cb);
            collect_workspace_refs(r, cb);
        }
        ExprKind::UnaryOp(_, inner) => collect_workspace_refs(inner, cb),
        ExprKind::HashLit(entries) => {
            for (_k, v) in entries {
                collect_workspace_refs(v, cb);
            }
        }
        ExprKind::StringLit(_)
        | ExprKind::IntLit(_)
        | ExprKind::FloatLit(_)
        | ExprKind::BoolLit(_)
        | ExprKind::Null => {}
    }
}
