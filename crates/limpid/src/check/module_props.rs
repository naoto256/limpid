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
//! the [`crate::dsl::module_props::ModuleProperties`] wrapper for the def block;
//! the analyzer reads `def.properties.type_name()` and validates only
//! `def.properties.user_properties()`. The previous "strip `type` from
//! a raw `Vec<Property>` before validating" pattern was the source of
//! the v0.7.2 asymmetry bug where the runtime forgot the strip — see
//! the [`crate::dsl::module_props::ModuleProperties`] type docs for the
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
    #[cfg(windows)]
    if type_name == "unix_socket" {
        diags.push(
            Diagnostic::error_kind(
                DiagKind::PropertySchema,
                format!(
                    "input '{}': unix_socket requires Unix and is not available on Windows",
                    def.name
                ),
            )
            .with_span(def.properties.type_span()),
        );
        return;
    }
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
    #[cfg(windows)]
    if type_name == "unix_socket" {
        diags.push(
            Diagnostic::error_kind(
                DiagKind::PropertySchema,
                format!(
                    "output '{}': unix_socket requires Unix and is not available on Windows",
                    def.name
                ),
            )
            .with_span(def.properties.type_span()),
        );
        return;
    }
    #[cfg(windows)]
    if type_name == "file" {
        for prop in def.properties.user_properties() {
            let (key, key_span) = match prop {
                crate::dsl::ast::Property::KeyValue { key, key_span, .. }
                | crate::dsl::ast::Property::Block { key, key_span, .. } => (key, key_span),
            };
            if matches!(key.as_str(), "mode" | "owner" | "group") {
                diags.push(Diagnostic::error_kind(DiagKind::PropertySchema,
                    format!("output '{}': file mode/owner/group require Unix; configure Windows filesystem ACLs outside the DSL", def.name))
                    .with_span(*key_span));
            }
        }
    }
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

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind, Property};
    use crate::dsl::module_props::ModuleProperties;

    #[test]
    fn unix_socket_diagnostics_explain_platform_instead_of_unknown_type() {
        let registry = ModuleRegistry::new();
        let mut diags = Vec::new();
        analyze_input_def(
            &InputDef {
                name: "local".into(),
                properties: ModuleProperties::from_parts("unix_socket", vec![]),
            },
            &registry,
            &mut diags,
        );
        analyze_output_def(
            &OutputDef {
                name: "local".into(),
                properties: ModuleProperties::from_parts("unix_socket", vec![]),
            },
            &registry,
            &mut diags,
        );
        assert_eq!(diags.len(), 2);
        assert!(diags.iter().all(|d| d.message.contains("requires Unix")));
    }

    #[test]
    fn file_metadata_options_are_rejected_by_check_and_factory() {
        use crate::modules::output::file::FileOutput;
        use crate::modules::{BuildContext, Module, register_builtins};
        let mut registry = ModuleRegistry::new();
        register_builtins(&mut registry);
        for (key, value) in [
            ("mode", "0640"),
            ("owner", "operator"),
            ("group", "operators"),
        ] {
            let property = |key: &str, value: &str| Property::KeyValue {
                key: key.into(),
                key_quoted: false,
                key_span: None,
                value: Expr::spanless(ExprKind::StringLit(value.into())),
                value_span: None,
            };
            let def = OutputDef {
                name: "file".into(),
                properties: ModuleProperties::from_parts(
                    "file",
                    vec![property("path", "test.log"), property(key, value)],
                ),
            };
            let mut diags = Vec::new();
            analyze_output_def(&def, &registry, &mut diags);
            assert!(diags.iter().any(|d| d.message.contains("require Unix")));
            let error =
                FileOutput::from_properties("file", &def.properties, &BuildContext::for_testing())
                    .err()
                    .expect("Windows must not silently ignore Unix file metadata");
            assert!(error.to_string().contains("require Unix"));
        }
    }
}
