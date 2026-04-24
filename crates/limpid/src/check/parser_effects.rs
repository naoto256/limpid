//! Effects of bare parser-call statements on the analyzer's binding
//! state.
//!
//! A statement like `syslog.parse(ingress)` or `parse_json(ingress, {…})`
//! produces workspace fields at runtime; the analyzer mirrors that by
//! merging the parser's declared `produces` schema (and any
//! user-supplied `defaults` HashLit) into the current `Bindings`.
//! Data-driven parsers without explicit defaults widen workspace to a
//! wildcard so downstream `workspace.*` reads remain admissible.

use crate::dsl::ast::{Expr, ExprKind};
use crate::functions::FunctionRegistry;
use crate::modules::schema::FieldType;

use super::bindings::Bindings;

pub(super) fn apply_parser_effects(
    namespace: Option<&str>,
    name: &str,
    args: &[Expr],
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
) {
    let Some(info) = registry.parser(namespace, name) else {
        // Not a parser — nothing to merge into workspace. Side-effect-
        // only functions (`table_upsert`, `table_delete`) return Null
        // and contribute nothing; that's intentional silence.
        return;
    };

    // Static produces: bind each declared `(workspace.key, type)` pair.
    for spec in &info.produces {
        bindings.bind_workspace(&spec.path, spec.ty.clone());
    }

    // Defaults arg (HashLit): every declared key becomes a workspace
    // binding too, with type inferred from the literal value. This is
    // the "user-declared schema" knob that lets parse_json / parse_kv
    // narrow the wildcard to a precise key set.
    if let Some(Expr {
        kind: ExprKind::HashLit(entries),
        ..
    }) = args.get(1)
    {
        for (k, v) in entries {
            let path = vec!["workspace".to_string(), k.clone()];
            bindings.bind_workspace(&path, literal_type(v));
        }
    } else if info.wildcards {
        // Data-driven parser called without explicit defaults — widen
        // workspace to wildcard so downstream `workspace.*` reads are
        // admitted (we no longer know which keys exist).
        bindings.set_workspace_wildcard();
    }
}

/// Best-effort type from a literal-shaped expression. Used for
/// HashLit defaults inference in parser calls; non-literal entries
/// fall through to `Any`.
fn literal_type(e: &Expr) -> FieldType {
    match &e.kind {
        ExprKind::StringLit(_) | ExprKind::Template(_) => FieldType::String,
        ExprKind::IntLit(_) => FieldType::Int,
        ExprKind::FloatLit(_) => FieldType::Float,
        ExprKind::BoolLit(_) => FieldType::Bool,
        ExprKind::Null => FieldType::Null,
        ExprKind::HashLit(_) => FieldType::Object,
        _ => FieldType::Any,
    }
}
