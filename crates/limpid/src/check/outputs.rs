//! Output-side reference checks: every `workspace.*` reference in an
//! output property must correspond to a key produced upstream.
//!
//! Walks every property's value expression, collects the workspace
//! references (idents, property accesses, template `${…}` interps,
//! function-call args, binary/unary subexpressions, hash literals),
//! and emits an Error per unresolved key. Levenshtein-based "did you
//! mean" hints are attached when a near match exists.

use crate::dsl::ast::{Expr, ExprKind, OutputDef, Property, walk_children};
use crate::dsl::schema::{PropertySpec, PropertyValueKind};
use crate::dsl::span::Span;
use crate::functions::FunctionRegistry;
use crate::modules::ModuleRegistry;

use super::bindings::Bindings;
use super::module_props::schema_declares_key;
use super::{DiagKind, Diagnostic, expr_types, suggestions};

/// When descending into a `Property::Block`, look up the inner schema
/// for that block's key in the parent schema (if any). Returns the
/// inner spec so the recursion can re-evaluate `schema_owned` for each
/// nested key on its own merits rather than inheriting the parent's
/// flag. Supports `Block`, `BlockMap`, and `OneOf` (the last by
/// preferring the first block-shaped variant).
fn inner_block_schema(
    parent: &'static [PropertySpec],
    key: &str,
) -> Option<&'static [PropertySpec]> {
    let spec = parent.iter().find(|p| p.name == key)?;
    inner_block_schema_of(&spec.kind)
}

fn inner_block_schema_of(
    kind: &PropertyValueKind,
) -> Option<&'static [PropertySpec]> {
    match kind {
        PropertyValueKind::Block(s) => Some(s),
        PropertyValueKind::BlockMap(s) => Some(s),
        PropertyValueKind::OneOf(variants) => {
            // Return a schema only when *exactly one* block-shaped
            // variant exists. The production schema where this is
            // exercised today is `OneOf[Block(TLS_CLIENT_BLOCK_PROPERTIES),
            // String]` (where String is the inline-CA-pem-path
            // shorthand), so "the one block variant" is unambiguous.
            // The day a `OneOf[Block(A), Block(B)]` lands — e.g.
            // inline-TLS vs inline-mTLS configs — arbitrarily picking
            // the first would silently validate against the wrong
            // schema with no signal to the operator. Returning None
            // in that case falls back to expression-level checks,
            // which is the conservative choice until a per-OneOf
            // resolution rule is encoded explicitly.
            let mut found: Option<&'static [PropertySpec]> = None;
            for v in variants.iter() {
                if let Some(s) = inner_block_schema_of(v) {
                    if found.is_some() {
                        return None;
                    }
                    found = Some(s);
                }
            }
            found
        }
        _ => None,
    }
}

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
        analyze_property(
            prop,
            schema,
            &output.name,
            pipeline_name,
            registry,
            bindings,
            diagnostics,
        );
    }
}

fn property_key(prop: &Property) -> &str {
    match prop {
        Property::KeyValue { key, .. } | Property::Block { key, .. } => key,
    }
}

fn analyze_property(
    prop: &Property,
    parent_schema: Option<&'static [PropertySpec]>,
    output_name: &str,
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Whether *this* key is declared in the *parent* schema. The
    // previous shape carried a single `schema_owned` flag down through
    // every recursion level, which silently silenced expression-level
    // diagnostics inside any nested block whose outer key was
    // schema-declared (e.g. `peer { host "${upperr(...)}" }` skipped
    // `expr_types::check_types` on `host` because the parent `peer`
    // block was schema-declared). Recomputing per-key against the
    // current schema level — and descending into the inner spec when
    // we enter a block — fixes that.
    let schema_owned =
        parent_schema.is_some_and(|s| schema_declares_key(s, property_key(prop)));

    match prop {
        Property::KeyValue {
            value: expr,
            value_span,
            ..
        } => {
            // The schema validator (see `module_props::analyze_all`)
            // owns the *shape* of schema-declared values — that's why
            // `framing non_transparent` (a bare-ident enum value)
            // doesn't surface as "unresolved ident". The previous
            // implementation skipped `expr_types::check_types` for
            // every schema-declared key, which over-applied the
            // silencing: an unknown function inside a template
            // interpolation like `host "${upperr(...)}"` was silenced
            // too because `host` is schema-declared (as `String`).
            //
            // Restrict the skip to the only case it actually targets:
            // a bare top-level `ExprKind::Ident` value, i.e. the form
            // `framing non_transparent`. Every other value shape
            // (StringLit with interpolations, IntLit, BoolLit,
            // FuncCall, etc.) still gets walked by `check_types` so
            // unknown-function / type-mismatch diagnostics inside
            // template bodies continue to surface.
            let skip_expr_types =
                schema_owned && matches!(expr.kind, ExprKind::Ident(_));
            if !skip_expr_types {
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
                    output_name,
                    pipeline_name,
                    bindings,
                    *value_span,
                    diagnostics,
                );
            });
        }
        Property::Block { properties, .. } => {
            // Descend into the inner schema for this block's key if
            // the parent declares one. If the key isn't schema-owned
            // at this level, we pass `None` so inner keys evaluate
            // against no schema (and therefore aren't "owned") —
            // expression-level diagnostics will run on them.
            let inner_schema = parent_schema
                .and_then(|s| inner_block_schema(s, property_key(prop)));
            for inner in properties {
                analyze_property(
                    inner,
                    inner_schema,
                    output_name,
                    pipeline_name,
                    registry,
                    bindings,
                    diagnostics,
                );
            }
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
