//! Property-schema validation pass over every `def input` and
//! `def output` in the configuration.
//!
//! Looks up each definition's `type` ident against the module
//! registry, fetches the declared `&[PropertySpec]`, and runs
//! [`dsl::schema::validate`] across the property surface. Every
//! finding becomes one [`Diagnostic`] tagged
//! [`DiagKind::PropertySchema`] with the offending key/value span
//! attached.
//!
//! The `type` property is stripped before validation — it isn't part
//! of the Module's own surface (it selects *which* module). Modules
//! without a registered schema are skipped (the gradual migration
//! path: `property_schema() = None` defaults to "do not enforce yet").

use crate::dsl::ast::{Expr, ExprKind, InputDef, OutputDef, Property};
use crate::dsl::schema::{self as ds, nearest};
use crate::dsl::span::Span;
use crate::modules::ModuleRegistry;
use crate::pipeline::CompiledConfig;

use super::{DiagKind, Diagnostic};

/// Validate every `def input` / `def output` in the compiled config
/// against the schema declared by its Module type. Pushed findings
/// share the same wording the runtime emits when the same config is
/// fed to the daemon directly, so `--check` and "start the daemon"
/// give the operator the same message.
pub(super) fn analyze_all(
    config: &CompiledConfig,
    registry: &ModuleRegistry,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for def in config.inputs.values() {
        analyze_input_def(def, registry, diagnostics);
    }
    for def in config.outputs.values() {
        analyze_output_def(def, registry, diagnostics);
    }
}

fn analyze_input_def(def: &InputDef, registry: &ModuleRegistry, diags: &mut Vec<Diagnostic>) {
    let Some((type_name, type_span)) = read_type_ident(&def.properties) else {
        return;
    };
    let Some(spec) = registry.input_schema(type_name) else {
        diags.push(unknown_module_type_diag(
            "input",
            &def.name,
            type_name,
            type_span,
            registry.input_type_names(),
        ));
        return;
    };
    let stripped = strip_type_property(&def.properties);
    let errs = ds::validate(&stripped, spec);
    let surface = format!("input '{}'", def.name);
    for err in errs {
        diags.push(diagnostic_from(&err, &surface));
    }
}

fn analyze_output_def(def: &OutputDef, registry: &ModuleRegistry, diags: &mut Vec<Diagnostic>) {
    let Some((type_name, type_span)) = read_type_ident(&def.properties) else {
        return;
    };
    let Some(spec) = registry.output_schema(type_name) else {
        diags.push(unknown_module_type_diag(
            "output",
            &def.name,
            type_name,
            type_span,
            registry.output_type_names(),
        ));
        return;
    };
    let stripped = strip_type_property(&def.properties);
    let errs = ds::validate(&stripped, spec);
    let surface = format!("output '{}'", def.name);
    for err in errs {
        diags.push(diagnostic_from(&err, &surface));
    }
}

fn unknown_module_type_diag<'a>(
    surface: &str,
    name: &str,
    bad_type: &str,
    span: Option<Span>,
    candidates: impl Iterator<Item = &'a str>,
) -> Diagnostic {
    let mut diag = Diagnostic::error_kind(
        DiagKind::PropertySchema,
        format!("{} '{}': unknown type '{}'", surface, name, bad_type),
    )
    .with_span(span);
    if let Some(near) = nearest(bad_type, candidates) {
        diag = diag.with_help(format!("did you mean `{}`?", near));
    }
    diag
}

/// Names a Module's `type tcp` style identifier and the value span
/// for the diagnostic caret. Bare ident only — `type "tcp"` (string
/// literal) is not idiomatic and is ignored here; the runtime would
/// already reject it.
fn read_type_ident(properties: &[Property]) -> Option<(&str, Option<Span>)> {
    for prop in properties {
        if let Property::KeyValue {
            key,
            value: Expr {
                kind: ExprKind::Ident(parts),
                ..
            },
            value_span,
            ..
        } = prop
            && key == "type"
        {
            return parts.first().map(|s| (s.as_str(), *value_span));
        }
    }
    None
}

/// Module schemas describe only the Module's own properties — `type`
/// is the indirection that picks the Module. Strip it before
/// validation so the schema doesn't have to (every Module would
/// otherwise duplicate the same `type: String` entry).
fn strip_type_property(properties: &[Property]) -> Vec<Property> {
    properties
        .iter()
        .filter(|p| match p {
            Property::KeyValue { key, .. } => key != "type",
            Property::Block { key, .. } => key != "type",
        })
        .cloned()
        .collect()
}

fn diagnostic_from(err: &ds::SchemaError, surface: &str) -> Diagnostic {
    let mut d = Diagnostic::error_kind(
        DiagKind::PropertySchema,
        format!("{}: {}", surface, err),
    )
    .with_span(err.primary_span());
    if let Some(s) = &err.did_you_mean {
        d = d.with_help(format!("did you mean `{}`?", s));
    }
    d
}

/// Returns true if `key` is declared in `spec`. Lets the existing
/// `outputs::analyze_output` walk skip its generic
/// `check_unknown_ident` pass on values whose meaning the schema
/// already owns — that fixes the false-positive where
/// `framing non_transparent` (a perfectly valid enum value) was
/// flagged as an unknown identifier by the expression-level walker.
pub(super) fn schema_declares_key(spec: &[ds::PropertySpec], key: &str) -> bool {
    spec.iter().any(|p| p.name == key)
}
