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
//! The `type` indirection is consumed by the parser when it constructs
//! the [`crate::modules::ModuleProperties`] wrapper for the def block;
//! the analyzer reads `def.properties.type_name()` and validates only
//! `def.properties.user_properties()`. The previous "strip `type` from
//! a raw `Vec<Property>` before validating" pattern was the source of
//! the v0.7.2 asymmetry bug where the runtime forgot the strip — see
//! the [`crate::modules::ModuleProperties`] type docs for the
//! structural fix that landed in 0.7.3.

use crate::dsl::ast::{InputDef, OutputDef};
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
    let type_name = def.properties.type_name();
    let Some(spec) = registry.input_schema(type_name) else {
        diags.push(unknown_module_type_diag(
            "input",
            &def.name,
            type_name,
            def.properties.type_span(),
            registry.input_type_names(),
        ));
        return;
    };
    let errs = ds::validate(def.properties.user_properties(), spec);
    let surface = format!("input '{}'", def.name);
    for err in errs {
        diags.push(Diagnostic::from_schema_error(&err, &surface));
    }
}

fn analyze_output_def(def: &OutputDef, registry: &ModuleRegistry, diags: &mut Vec<Diagnostic>) {
    let type_name = def.properties.type_name();
    let Some(spec) = registry.output_schema(type_name) else {
        diags.push(unknown_module_type_diag(
            "output",
            &def.name,
            type_name,
            def.properties.type_span(),
            registry.output_type_names(),
        ));
        return;
    };
    let errs = ds::validate(def.properties.user_properties(), spec);
    let surface = format!("output '{}'", def.name);
    for err in errs {
        diags.push(Diagnostic::from_schema_error(&err, &surface));
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

/// Returns true if `key` is declared in `spec`. Lets the existing
/// `outputs::analyze_output` walk skip its generic
/// `check_unknown_ident` pass on values whose meaning the schema
/// already owns — that fixes the false-positive where
/// `framing non_transparent` (a perfectly valid enum value) was
/// flagged as an unknown identifier by the expression-level walker.
pub(super) fn schema_declares_key(spec: &[ds::PropertySpec], key: &str) -> bool {
    spec.iter().any(|p| p.name == key)
}
