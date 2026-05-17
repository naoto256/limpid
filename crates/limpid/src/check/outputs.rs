//! Output-side reference checks: every `workspace.*` reference in an
//! output property must correspond to a key produced upstream.
//!
//! Walks every property's value expression, collects the workspace
//! references (idents, property accesses, template `${…}` interps,
//! function-call args, binary/unary subexpressions, hash literals),
//! and emits an Error per unresolved key. Levenshtein-based "did you
//! mean" hints are attached when a near match exists.

use crate::dsl::ast::{Expr, ExprKind, OutputDef, Property, walk_children};
use crate::dsl::span::Span;
use crate::functions::FunctionRegistry;
use crate::modules::ModuleRegistry;

use super::bindings::Bindings;
use super::module_props::schema_declares_key;
use super::{DiagKind, Diagnostic, expr_types, suggestions};

pub(super) fn analyze_output(
    output: &OutputDef,
    pipeline_name: &str,
    registry: &FunctionRegistry,
    module_registry: &ModuleRegistry,
    bindings: &Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let schema = module_registry.output_schema(output.properties.type_name());

    // `type` no longer appears in `user_properties()` — it lives in a typed
    // slot on `ModuleProperties` — so the explicit `if key == "type" { continue; }`
    // guard that earlier versions carried is gone by construction.
    for prop in output.properties.user_properties() {
        if let Property::KeyValue {
            key,
            value: expr,
            value_span,
            ..
        } = prop
        {
            // If this key is declared in the Module's property schema,
            // the schema validator (see `module_props::analyze_all`)
            // owns the value's *shape* — bare-ident enum values like
            // `framing non_transparent` are no longer flagged as
            // unresolved idents. Workspace references inside template
            // values (`address "${workspace.x}:1"`) remain a dataflow
            // concern, so the `collect_workspace_refs` walk still runs
            // regardless of whether the schema covers the key.
            let schema_owned = schema.is_some_and(|s| schema_declares_key(s, key));
            if !schema_owned {
                expr_types::check_types(
                    expr,
                    pipeline_name,
                    bindings,
                    registry,
                    *value_span,
                    diagnostics,
                );
            }
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
        let mut diag = Diagnostic::error_kind(
            DiagKind::UnknownIdent,
            format!(
                "[pipeline {}] output `{}` references `{}` which is not produced by any upstream module",
                pipeline_name, output_name, joined,
            ),
        )
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
            // Combine `Ident(["workspace", "x"]) . y . z` into a single
            // path so the caller can match it against produced
            // workspace keys; for non-Ident bases (e.g. `geoip(...)`),
            // recurse normally.
            if let ExprKind::Ident(base_parts) = &base.kind {
                let mut combined = base_parts.clone();
                combined.extend(suffix.iter().cloned());
                cb(&combined);
            } else {
                collect_workspace_refs(base, cb);
            }
        }
        // Generic recursion for the rest — sub-expressions carry no
        // structural meaning beyond "look here for refs too".
        _ => walk_children(expr, |child| collect_workspace_refs(child, cb)),
    }
}
