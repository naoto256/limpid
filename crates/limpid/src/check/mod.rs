//! Static analyzer for limpid configurations.
//!
//! Entry point for `limpid --check`. The pipeline:
//!
//! - **Bindings** ([`bindings::Bindings`]) — per-scope tracking of
//!   workspace paths, `let` locals, and reserved event idents.
//! - **Type inference** ([`expr_types::infer`]) — `Expr` → `FieldType`,
//!   consulting the bindings and the function registry's signature
//!   table.
//! - **Type checks** ([`expr_types::check_types`]) — operator and
//!   function-argument warnings.
//! - **Parser effects** ([`parser_effects`]) — bare `parse_*(text)` /
//!   `syslog.parse(...)` / `cef.parse(...)` statements merge their
//!   `produces` schema into the `workspace.*` bindings.
//! - **Control flow** ([`control_flow`]) — `if` / `else if` / `else`,
//!   `switch`, `try/catch`, `for_each` use branch intersection to
//!   compute the bindings guaranteed at the join.
//! - **Output reference checks** ([`outputs`]) — output-side
//!   `${workspace.x}` / `workspace.x` references must correspond to a
//!   workspace key produced upstream.
//! - **Rendering** ([`render::render_diagnostic`]) — rustc-style multi-
//!   line snippet + caret + optional `help:` line.
//!
//! Submodule layout (commit 5 split):
//! - `bindings`        — workspace + let scope tracking, branch intersect
//! - `control_flow`    — if/switch/try-catch/for-each branch handling
//! - `expr_types`      — `Expr` → `FieldType` inference + arg-/op-checks
//! - `outputs`         — output-side workspace reference checks
//! - `parser_effects`  — parser `produces` → workspace merge
//! - `render`          — rustc-style snippet+caret diagnostic emit
//! - `suggestions`     — Levenshtein "did you mean" hint
//! - `mod`             — pipeline walk + entry + Diagnostic / Level

pub mod bindings;
mod control_flow;
pub mod expr_types;
pub mod graph;
mod outputs;
mod parser_effects;
pub mod render;
pub mod suggestions;

use crate::dsl::ast::{
    AssignTarget, Config, Definition, Expr, ExprKind, PipelineDef, PipelineStatement,
    ProcessChainElement, ProcessDef, ProcessStatement,
};
use crate::dsl::span::{SourceMap, Span};
use crate::functions::FunctionRegistry;
use crate::modules::schema::FieldType;
use crate::pipeline::CompiledConfig;

use bindings::Bindings;

/// Severity of an analyzer diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

/// Category tag used by `--ultra-strict` to decide which warnings get
/// promoted to errors. Keeping this orthogonal to [`Level`] lets the CLI
/// filter on *what kind of problem* the diagnostic describes without
/// reparsing the `message` string. Future `--strict=idents,types` or
/// similar fine-grained flags can reuse this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagKind {
    /// Unknown / unresolved identifier: workspace key not produced
    /// upstream, function name typo, reserved-name misspelling. These
    /// are the high-confidence "CI should catch this" signals that
    /// `--ultra-strict` promotes from warning to error.
    UnknownIdent,
    /// Type mismatch between operator / function-arg and the inferred
    /// operand/argument type. Intentionally left out of `--ultra-strict`
    /// because borderline cases (Int vs String comparison, numeric +
    /// string concat) are noisier.
    TypeMismatch,
    /// Dataflow shape problem (e.g. assignment that overwrites an
    /// Object with a scalar and invalidates nested reads).
    Dataflow,
    /// Catch-all for diagnostics that don't fit the above buckets.
    #[allow(dead_code)]
    Other,
}

/// A single issue produced by the analyzer.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub level: Level,
    pub kind: DiagKind,
    pub message: String,
    pub span: Option<Span>,
    /// Optional `help: ...` line emitted under the caret. Used by the
    /// suggestion engine to surface a near-match workspace key or
    /// function name.
    pub help: Option<String>,
}

impl Diagnostic {
    /// Error with `DiagKind::Other`. Prefer [`Diagnostic::error_kind`]
    /// when a specific category applies. Kept for render-layer test
    /// ergonomics where the DiagKind isn't under test.
    #[allow(dead_code)]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            kind: DiagKind::Other,
            message: message.into(),
            span: None,
            help: None,
        }
    }

    pub fn error_kind(kind: DiagKind, message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            kind,
            message: message.into(),
            span: None,
            help: None,
        }
    }

    /// Warning with `DiagKind::Other`. Prefer [`Diagnostic::warning_kind`]
    /// so `--ultra-strict` can classify the diagnostic correctly. Kept
    /// for render-layer test ergonomics.
    #[allow(dead_code)]
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            kind: DiagKind::Other,
            message: message.into(),
            span: None,
            help: None,
        }
    }

    pub fn warning_kind(kind: DiagKind, message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            kind,
            message: message.into(),
            span: None,
            help: None,
        }
    }

    #[allow(dead_code)]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: Level::Info,
            kind: DiagKind::Other,
            message: message.into(),
            span: None,
            help: None,
        }
    }

    pub fn with_span(mut self, span: Option<Span>) -> Self {
        self.span = span;
        self
    }

    pub fn with_help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }
}

/// Post-process diagnostics for `--ultra-strict`: promote every
/// [`DiagKind::UnknownIdent`] warning to an error. Leaves other kinds
/// untouched. Returns the transformed vector.
///
/// Kept separate from the analyzer so the transform is testable in
/// isolation and so the analyzer itself stays policy-free — every
/// diagnostic is emitted at the level the type-check logic believes,
/// and the CLI layer decides whether to promote.
pub fn promote_unknown_idents(mut diags: Vec<Diagnostic>) -> Vec<Diagnostic> {
    for d in &mut diags {
        if d.level == Level::Warning && d.kind == DiagKind::UnknownIdent {
            d.level = Level::Error;
        }
    }
    diags
}

/// Definition counts derived from a parsed [`Config`]. Emitted in the
/// `--check` summary header so operators see the scope of what was
/// validated (a config that walks 0 pipelines because of a stale
/// include glob is "OK" by default; the count makes that visible).
#[derive(Debug, Clone, Copy, Default)]
pub struct DefCounts {
    pub inputs: usize,
    pub outputs: usize,
    pub processes: usize,
    pub pipelines: usize,
}

impl DefCounts {
    /// Walk a parsed [`Config`] and tally each definition kind.
    pub fn from_config(config: &Config) -> Self {
        let mut c = Self::default();
        for def in &config.definitions {
            match def {
                Definition::Input(_) => c.inputs += 1,
                Definition::Output(_) => c.outputs += 1,
                Definition::Process(_) => c.processes += 1,
                Definition::Pipeline(_) => c.pipelines += 1,
            }
        }
        c
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Run the static analyzer.
///
/// Walks every pipeline in `config`, threading a [`Bindings`] through
/// each `input → process chain → output` sequence and emitting
/// diagnostics for type errors, missing workspace produces, and
/// operator / function-arg mismatches.
///
/// The [`SourceMap`] argument lets callers resolve diagnostic spans for
/// snippet rendering. The analyzer itself never reads file text — it
/// just propagates spans recorded in the AST. A no-op `SourceMap::new()`
/// is fine when caret rendering isn't needed.
pub fn analyze(config: &CompiledConfig, _source_map: &SourceMap) -> Vec<Diagnostic> {
    // Build a registry that mirrors what the runtime uses, so the
    // analyzer sees exactly the same function signatures and parser
    // effects as the executor. This keeps the type-check table from
    // drifting against the actual registered functions.
    let table_store = match crate::functions::table::TableStore::from_configs(vec![]) {
        Ok(t) => t,
        Err(_) => {
            // An empty config can't fail to build a table store; if it
            // somehow does, fall back to no functions registered. The
            // analyzer just skips type checks rather than crashing.
            return Vec::new();
        }
    };
    let mut registry = FunctionRegistry::new();
    crate::functions::register_builtins(&mut registry, table_store);

    let mut diagnostics = Vec::new();
    for (name, pipeline) in &config.pipelines {
        analyze_pipeline(name, pipeline, config, &registry, &mut diagnostics);
    }
    diagnostics
}

// ---------------------------------------------------------------------------
// Pipeline walk
// ---------------------------------------------------------------------------

fn analyze_pipeline(
    pipeline_name: &str,
    pipeline: &PipelineDef,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut bindings = Bindings::new();
    bindings.push_let_scope();

    for stmt in &pipeline.body {
        analyze_pipeline_stmt(
            stmt,
            pipeline_name,
            config,
            registry,
            &mut bindings,
            diagnostics,
        );
    }

    bindings.pop_let_scope();
}

pub(super) fn analyze_pipeline_stmt(
    stmt: &PipelineStatement,
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match stmt {
        PipelineStatement::Input(_names) => {
            // Inputs are I/O-pure: ingress bytes flow in. No workspace
            // bindings produced. Reserved idents (ingress / source /
            // timestamp) are always present and resolved by ident_type.
        }
        PipelineStatement::ProcessChain(elements) => {
            for element in elements {
                analyze_chain_element(
                    element,
                    pipeline_name,
                    config,
                    registry,
                    bindings,
                    diagnostics,
                );
            }
        }
        PipelineStatement::Output(name) => {
            if let Some(out) = config.outputs.get(name) {
                outputs::analyze_output(out, pipeline_name, registry, bindings, diagnostics);
            }
        }
        PipelineStatement::Drop | PipelineStatement::Finish => {}
        PipelineStatement::If(chain) => {
            control_flow::analyze_if_chain(
                chain,
                pipeline_name,
                config,
                registry,
                bindings,
                diagnostics,
            );
        }
        PipelineStatement::Switch(scrutinee, arms) => {
            expr_types::check_types(
                scrutinee,
                pipeline_name,
                bindings,
                registry,
                None,
                diagnostics,
            );
            control_flow::analyze_switch(
                arms,
                pipeline_name,
                config,
                registry,
                bindings,
                diagnostics,
            );
        }
    }
}

fn analyze_chain_element(
    elem: &ProcessChainElement,
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match elem {
        ProcessChainElement::Named(name, args) => {
            for a in args {
                expr_types::check_types(a, pipeline_name, bindings, registry, None, diagnostics);
            }
            if let Some(pdef) = config.processes.get(name) {
                analyze_process_body(pdef, pipeline_name, registry, bindings, diagnostics);
            } else {
                // Unknown user-defined process — pessimistic wildcard so
                // we don't false-positive on workspace reads downstream.
                bindings.set_workspace_wildcard();
            }
        }
        ProcessChainElement::Inline(stmts) => {
            // Inline blocks run in their own `let` scope.
            bindings.push_let_scope();
            for s in stmts {
                analyze_process_stmt(s, pipeline_name, registry, bindings, diagnostics);
            }
            bindings.pop_let_scope();
        }
    }
}

fn analyze_process_body(
    pdef: &ProcessDef,
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    bindings.push_let_scope();
    for s in &pdef.body {
        analyze_process_stmt(s, pipeline_name, registry, bindings, diagnostics);
    }
    bindings.pop_let_scope();
}

// ---------------------------------------------------------------------------
// Process statement walk
// ---------------------------------------------------------------------------

pub(super) fn analyze_process_stmt(
    stmt: &ProcessStatement,
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    match stmt {
        ProcessStatement::Assign(target, expr) => {
            expr_types::check_types(expr, pipeline_name, bindings, registry, None, diagnostics);
            match target {
                AssignTarget::Workspace(path) => {
                    let mut full = vec!["workspace".to_string()];
                    full.extend(path.iter().cloned());
                    let new_ty = expr_types::infer(expr, bindings, registry);

                    // Object → Scalar overwrite: nested references die.
                    if let Some(old_ty) = bindings.get_workspace(&full).cloned()
                        && matches!(old_ty, FieldType::Object)
                        && !matches!(new_ty, FieldType::Object | FieldType::Any)
                    {
                        // Anchor to the RHS expression if it came from
                        // the parser; otherwise leave spanless.
                        let span = if expr.span.file_id == u32::MAX {
                            None
                        } else {
                            Some(expr.span)
                        };
                        diagnostics.push(
                            Diagnostic::warning_kind(
                                DiagKind::Dataflow,
                                format!(
                                    "[pipeline {}] assignment to `{}` overwrites an Object with {}; \
                                     nested references (e.g. `{}.*`) will become dead",
                                    pipeline_name,
                                    full.join("."),
                                    new_ty.display(),
                                    full.join("."),
                                ),
                            )
                            .with_span(span),
                        );
                    }

                    bindings.bind_workspace(&full, new_ty);
                }
                AssignTarget::Egress => {
                    // egress is a bytes sink — RHS already type-checked.
                }
            }
        }
        ProcessStatement::LetBinding(name, expr) => {
            expr_types::check_types(expr, pipeline_name, bindings, registry, None, diagnostics);
            let ty = expr_types::infer(expr, bindings, registry);
            bindings.bind_let(name, ty);
        }
        ProcessStatement::ExprStmt(
            call @ Expr {
                kind:
                    ExprKind::FuncCall {
                        namespace,
                        name,
                        args,
                    },
                ..
            },
        ) => {
            // Bare function-call statement: type-check the call
            // expression (which recurses into args and checks arg-type
            // compatibility against the signature), then apply the
            // parser merge effect into workspace if it's a parser. No
            // separate arg-only pass — `check_types(call, …)` already
            // walks `args`.
            expr_types::check_types(call, pipeline_name, bindings, registry, None, diagnostics);
            parser_effects::apply_parser_effects(
                namespace.as_deref(),
                name,
                args,
                registry,
                bindings,
            );
        }
        ProcessStatement::ExprStmt(e) => {
            expr_types::check_types(e, pipeline_name, bindings, registry, None, diagnostics);
        }
        ProcessStatement::ProcessCall(_name, args) => {
            for a in args {
                expr_types::check_types(a, pipeline_name, bindings, registry, None, diagnostics);
            }
            // Process bodies were validated separately when the named
            // process was defined; we don't recurse from here.
        }
        ProcessStatement::Drop => {}
        ProcessStatement::If(chain) => {
            control_flow::analyze_inline_if(chain, pipeline_name, registry, bindings, diagnostics);
        }
        ProcessStatement::Switch(scrutinee, arms) => {
            expr_types::check_types(
                scrutinee,
                pipeline_name,
                bindings,
                registry,
                None,
                diagnostics,
            );
            control_flow::analyze_inline_switch(
                arms,
                pipeline_name,
                registry,
                bindings,
                diagnostics,
            );
        }
        ProcessStatement::TryCatch(try_body, catch_body) => {
            control_flow::analyze_try_catch(
                try_body,
                catch_body,
                pipeline_name,
                registry,
                bindings,
                diagnostics,
            );
        }
        ProcessStatement::ForEach(iterable, body) => {
            expr_types::check_types(
                iterable,
                pipeline_name,
                bindings,
                registry,
                None,
                diagnostics,
            );
            control_flow::analyze_for_each(body, pipeline_name, registry, bindings, diagnostics);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;
    use crate::pipeline::CompiledConfig;

    fn analyze_str(src: &str) -> Vec<Diagnostic> {
        let cfg = parse_config(src).expect("config should parse");
        let compiled = CompiledConfig::from_config(cfg).expect("compile");
        let mut sm = SourceMap::new();
        sm.add_anonymous(src);
        analyze(&compiled, &sm)
    }

    fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags.iter().filter(|d| d.level == Level::Error).collect()
    }

    fn warnings(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags.iter().filter(|d| d.level == Level::Warning).collect()
    }

    // ----- workspace produce / consume -----------------------------------

    #[test]
    fn output_referencing_unproduced_workspace_key_errors() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.nope}" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        assert!(errs[0].message.contains("workspace.nope"));
    }

    #[test]
    fn syslog_parse_binds_known_workspace_keys() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.syslog_msg}" }
def pipeline p {
    input i
    process { syslog.parse(ingress) }
    output o
}
"#;
        let diags = analyze_str(src);
        assert!(errors(&diags).is_empty(), "got: {:?}", diags);
    }

    #[test]
    fn parse_json_with_defaults_narrows_to_declared_keys() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.usr}" }
def pipeline p {
    input i
    process { parse_json(ingress, {user: "anon"}) }
    output o
}
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        assert!(errs[0].message.contains("workspace.usr"));
    }

    #[test]
    fn parse_json_without_defaults_wildcards() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.anything}" }
def pipeline p {
    input i
    process { parse_json(ingress) }
    output o
}
"#;
        let diags = analyze_str(src);
        assert!(errors(&diags).is_empty(), "got: {:?}", diags);
    }

    // ----- branch intersection -------------------------------------------

    #[test]
    fn if_without_else_does_not_propagate_branch_bindings() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.tag}" }
def pipeline p {
    input i
    process {
        if contains(ingress, "x") {
            workspace.tag = "y"
        }
    }
    output o
}
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
    }

    #[test]
    fn if_else_with_both_branches_binding_is_guaranteed() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.tag}" }
def pipeline p {
    input i
    process {
        if contains(ingress, "x") {
            workspace.tag = "y"
        } else {
            workspace.tag = "z"
        }
    }
    output o
}
"#;
        let diags = analyze_str(src);
        assert!(errors(&diags).is_empty(), "got: {:?}", diags);
    }

    // ----- operator type checks ------------------------------------------

    #[test]
    fn eq_int_workspace_vs_string_warns() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {port: 0}) }
    process {
        if workspace.port == "80" {
            workspace.tag = "hot"
        }
    }
    output o
}
"#;
        let diags = analyze_str(src);
        let warns = warnings(&diags);
        assert!(
            warns.iter().any(|w| w.message.contains("=="))
                && warns.iter().any(|w| w.message.contains("Int"))
                && warns.iter().any(|w| w.message.contains("String")),
            "got: {:?}",
            diags
        );
    }

    #[test]
    fn eq_int_and_int_silent() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {port: 0}) }
    process {
        if workspace.port == 80 {
            workspace.tag = "hot"
        }
    }
    output o
}
"#;
        let diags = analyze_str(src);
        assert!(warnings(&diags).is_empty(), "got: {:?}", diags);
    }

    #[test]
    fn arith_minus_on_string_warns() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {name: "x"}) }
    process { workspace.y = workspace.name - 1 }
    output o
}
"#;
        let diags = analyze_str(src);
        let warns = warnings(&diags);
        assert!(
            warns.iter().any(|w| w.message.contains("arithmetic")),
            "got: {:?}",
            diags
        );
    }

    #[test]
    fn lower_on_int_workspace_warns() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process { workspace.tag = lower(workspace.count) }
    output o
}
"#;
        let diags = analyze_str(src);
        let warns = warnings(&diags);
        assert!(
            warns.iter().any(|w| w.message.contains("lower")
                && w.message.contains("String")
                && w.message.contains("Int")),
            "got: {:?}",
            diags
        );
    }

    // ----- assignment overwrite ------------------------------------------

    #[test]
    fn object_overwritten_with_string_warns() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process {
        workspace.geo = geoip(source)
        workspace.geo = "unknown"
    }
    output o
}
"#;
        let diags = analyze_str(src);
        let warns = warnings(&diags);
        assert!(
            warns
                .iter()
                .any(|w| w.message.contains("overwrite") && w.message.contains("Object")),
            "got: {:?}",
            diags
        );
    }

    // ----- try/catch -----------------------------------------------------

    #[test]
    fn try_catch_intersects_bindings() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.a}" }
def pipeline p {
    input i
    process {
        try {
            workspace.a = "x"
        } catch {
            workspace.b = "y"
        }
    }
    output o
}
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
    }

    #[test]
    fn catch_body_binds_workspace_error_as_string() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process {
        try {
            workspace.a = "x"
        } catch {
            workspace.msg = lower(workspace._error)
        }
    }
    output o
}
"#;
        let diags = analyze_str(src);
        assert!(warnings(&diags).is_empty(), "got: {:?}", diags);
    }

    // ----- Phase 3 UX: spans + suggestions ------------------------------

    #[test]
    fn output_unresolved_workspace_ref_carries_value_span() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.nope}" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        let span = errs[0].span.expect("error should carry a span");
        let mut sm = SourceMap::new();
        sm.add_anonymous(src);
        let resolved = sm.resolve(&span).expect("span should resolve");
        assert!(
            resolved.line_text.contains("template"),
            "span line: {}",
            resolved.line_text
        );
    }

    #[test]
    fn unresolved_workspace_ref_suggests_near_match() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.synlog_msg}" }
def pipeline p {
    input i
    process { syslog.parse(ingress) }
    output o
}
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        let help = errs[0].help.as_deref().expect("should have help line");
        assert!(help.contains("workspace.syslog_msg"), "help was: {}", help);
    }

    #[test]
    fn unresolved_workspace_ref_silent_when_nothing_close() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.completely_unrelated_zzz}" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        assert!(
            errs[0].help.is_none(),
            "should not suggest when nothing close: help={:?}",
            errs[0].help
        );
    }

    #[test]
    fn let_binding_visible_inside_process() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process {
        let x = "hello"
        workspace.echo = upper(x)
    }
    output o
}
"#;
        let diags = analyze_str(src);
        assert!(warnings(&diags).is_empty(), "got: {:?}", diags);
    }

    // ----- expr-level spans (Block 11-A) ---------------------------------

    #[test]
    fn lower_on_int_workspace_carries_arg_span() {
        // Regression: before 11-A, type warnings inside process bodies
        // were spanless and fell back to the one-line
        // `warning: ...` format. After 11-A the arg's `ExprKind::Ident`
        // span resolves to the `workspace.count` substring, so the
        // diagnostic renders with a snippet + caret.
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process { workspace.tag = lower(workspace.count) }
    output o
}
"#;
        let diags = analyze_str(src);
        let warn = warnings(&diags)
            .into_iter()
            .find(|w| w.message.contains("lower"))
            .expect("expected a `lower` warning");
        let span = warn.span.expect("expected expr-level span");
        let mut sm = SourceMap::new();
        sm.add_anonymous(src);
        let resolved = sm.resolve(&span).expect("span should resolve");
        assert!(
            resolved
                .line_text
                .contains("workspace.tag = lower(workspace.count)"),
            "unexpected line: {}",
            resolved.line_text
        );
        assert!(
            resolved.line_text[resolved.col as usize - 1..].starts_with("workspace.count"),
            "caret should sit on `workspace.count` (col {}), line: {}",
            resolved.col,
            resolved.line_text
        );
    }

    #[test]
    fn operator_type_mismatch_carries_binop_span() {
        // `workspace.port == "80"` where workspace.port is Int: the
        // BinOp span should cover the comparison expression, which
        // lives on the `if` line. Pre-11-A the warning was spanless.
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {port: 0}) }
    process {
        if workspace.port == "80" {
            workspace.tag = "hot"
        }
    }
    output o
}
"#;
        let diags = analyze_str(src);
        let warn = warnings(&diags)
            .into_iter()
            .find(|w| w.message.contains("=="))
            .expect("expected == warning");
        let span = warn.span.expect("expected span for `==` warning");
        let mut sm = SourceMap::new();
        sm.add_anonymous(src);
        let resolved = sm.resolve(&span).expect("span should resolve");
        assert!(
            resolved.line_text.contains("workspace.port == \"80\""),
            "unexpected line: {}",
            resolved.line_text
        );
    }

    // ----- DiagKind tagging / promotion (Block 11-C) --------------------

    #[test]
    fn unresolved_workspace_output_ref_tagged_unknown_ident() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.nope}" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1);
        assert_eq!(errs[0].kind, DiagKind::UnknownIdent);
    }

    #[test]
    fn unknown_function_tagged_unknown_ident() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { workspace.x = upperr(ingress) }
    output o
}
"#;
        let diags = analyze_str(src);
        let w = warnings(&diags)
            .into_iter()
            .find(|w| w.message.contains("unknown function"))
            .expect("expected unknown function warning");
        assert_eq!(w.kind, DiagKind::UnknownIdent);
    }

    #[test]
    fn type_mismatch_tagged_type_mismatch() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process { workspace.tag = lower(workspace.count) }
    output o
}
"#;
        let diags = analyze_str(src);
        let w = warnings(&diags)
            .into_iter()
            .find(|w| w.message.contains("lower"))
            .expect("expected lower warning");
        assert_eq!(w.kind, DiagKind::TypeMismatch);
    }

    #[test]
    fn object_overwrite_tagged_dataflow() {
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process {
        workspace.geo = geoip(source)
        workspace.geo = "unknown"
    }
    output o
}
"#;
        let diags = analyze_str(src);
        let w = warnings(&diags)
            .into_iter()
            .find(|w| w.message.contains("overwrite"))
            .expect("expected overwrite warning");
        assert_eq!(w.kind, DiagKind::Dataflow);
    }

    #[test]
    fn promote_unknown_idents_escalates_only_matching_warnings() {
        // Mixed batch: an unknown function warning (UnknownIdent) and a
        // type mismatch (TypeMismatch). `promote_unknown_idents` should
        // escalate the first and leave the second alone.
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input i
    process { parse_json(ingress, {count: 0}) }
    process {
        workspace.a = upperr(ingress)
        workspace.b = lower(workspace.count)
    }
    output o
}
"#;
        let diags = analyze_str(src);
        // Sanity: at least one UnknownIdent warning and one TypeMismatch
        // warning before promotion.
        assert!(
            diags
                .iter()
                .any(|d| d.level == Level::Warning && d.kind == DiagKind::UnknownIdent),
        );
        assert!(
            diags
                .iter()
                .any(|d| d.level == Level::Warning && d.kind == DiagKind::TypeMismatch),
        );

        let promoted = promote_unknown_idents(diags);
        // UnknownIdent warnings all escalated.
        assert!(
            !promoted
                .iter()
                .any(|d| d.level == Level::Warning && d.kind == DiagKind::UnknownIdent),
            "UnknownIdent warnings should be promoted"
        );
        assert!(
            promoted
                .iter()
                .any(|d| d.level == Level::Error && d.kind == DiagKind::UnknownIdent),
            "expected a promoted UnknownIdent error"
        );
        // TypeMismatch warnings still warnings.
        assert!(
            promoted
                .iter()
                .any(|d| d.level == Level::Warning && d.kind == DiagKind::TypeMismatch),
            "TypeMismatch should not be promoted"
        );
    }

    // ----- DefCounts -----------------------------------------------------

    #[test]
    fn def_counts_aggregate_each_kind() {
        let src = r#"
def input i1 { type tcp bind "0.0.0.0:514" }
def input i2 { type udp bind "0.0.0.0:514" }
def output o1 { type stdout template "x" }
def process p { workspace.a = "z" }
def pipeline pl { input i1; output o1 }
"#;
        let cfg = parse_config(src).expect("parse");
        let counts = DefCounts::from_config(&cfg);
        assert_eq!(counts.inputs, 2);
        assert_eq!(counts.outputs, 1);
        assert_eq!(counts.processes, 1);
        assert_eq!(counts.pipelines, 1);
    }
}
