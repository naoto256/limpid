//! Static analyzer for limpid configurations.
//!
//! Entry point for `limpid --check`. The pipeline:
//!
//! - **Bindings** (`bindings::Bindings`) — per-scope tracking of
//!   workspace paths, `let` locals, and reserved event idents.
//! - **Type inference** (`expr_types::infer`) — `Expr` → `FieldType`,
//!   consulting the bindings and the function registry's signature
//!   table.
//! - **Type checks** (`expr_types::check_types`) — operator and
//!   function-argument warnings (Int compared to String, `lower(Int)`,
//!   etc.). Spans are threaded through so output-side warnings get
//!   caret rendering; process-body warnings still fall back to the
//!   one-line format until the AST grows per-statement spans.
//! - **Parser effects** — bare `parse_*(text)` / `syslog.parse(...)` /
//!   `cef.parse(...)` statements merge their `produces` schema into
//!   the `workspace.*` bindings (or wildcard, when the parser's keys
//!   are data-driven).
//! - **Control flow** — `if` / `else if` / `else`, `switch`, and
//!   `try`/`catch` use branch intersection to compute the bindings
//!   guaranteed at the join. Catch bodies pre-bind `workspace._error`
//!   as `String` (matching the runtime).
//! - **Output reference checks** — output-side `${workspace.x}` /
//!   `workspace.x` references must correspond to a workspace key
//!   produced upstream; unresolved refs emit an Error with the
//!   property's value span and a Levenshtein "did you mean" hint.
//! - **Rendering** (`render::render_diagnostic`) — rustc-style multi-
//!   line snippet + caret + optional `help:` line when a span is
//!   attached, ANSI-coloured on TTY, plain otherwise.
//!
//! Things deliberately deferred to commit 5:
//! - Submodule split (control_flow / outputs / parser_effects / types).
//! - Include expansion, summary counts, `Configuration OK` dataflow
//!   hint.

pub mod bindings;
pub mod expr_types;
pub mod render;
pub mod suggestions;

use crate::dsl::ast::{
    AssignTarget, BranchBody, Expr, IfChain, OutputDef, PipelineDef, PipelineStatement,
    ProcessChainElement, ProcessDef, ProcessStatement, Property, SwitchArm, TemplateFragment,
};
use crate::dsl::span::{SourceMap, Span};
use crate::functions::FunctionRegistry;
use crate::modules::schema::FieldType;
use crate::pipeline::CompiledConfig;

use bindings::{Bindings, intersect_branches};

/// Severity of an analyzer diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

/// A single issue produced by the analyzer.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    pub span: Option<Span>,
    /// Optional `help: ...` line emitted under the caret. Used by the
    /// suggestion engine to surface a near-match workspace key or
    /// function name.
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            message: message.into(),
            span: None,
            help: None,
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            message: message.into(),
            span: None,
            help: None,
        }
    }

    #[allow(dead_code)]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: Level::Info,
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
    // Pipelines have a single ambient `let` scope — locals introduced
    // at pipeline-statement level (rare but legal) live here. Process
    // bodies push their own scopes on top.
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

fn analyze_pipeline_stmt(
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
                analyze_output(out, pipeline_name, registry, bindings, diagnostics);
            }
        }
        PipelineStatement::Drop | PipelineStatement::Finish => {}
        PipelineStatement::If(chain) => {
            analyze_if_chain(
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
            analyze_switch(arms, pipeline_name, config, registry, bindings, diagnostics);
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
                // (Validate would have already errored if this name is
                // truly unknown; we just guard the analyzer state.)
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

fn analyze_process_stmt(
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
                        diagnostics.push(Diagnostic::warning(format!(
                            "[pipeline {}] assignment to `{}` overwrites an Object with {}; \
                             nested references (e.g. `{}.*`) will become dead",
                            pipeline_name,
                            full.join("."),
                            new_ty.display(),
                            full.join("."),
                        )));
                    }

                    bindings.bind_workspace(&full, new_ty);
                }
                AssignTarget::Egress => {
                    // egress is a bytes sink — no workspace effect, but
                    // the RHS was already type-checked above.
                }
            }
        }
        ProcessStatement::LetBinding(name, expr) => {
            expr_types::check_types(expr, pipeline_name, bindings, registry, None, diagnostics);
            let ty = expr_types::infer(expr, bindings, registry);
            bindings.bind_let(name, ty);
        }
        ProcessStatement::ExprStmt(Expr::FuncCall {
            namespace,
            name,
            args,
        }) => {
            for a in args {
                expr_types::check_types(a, pipeline_name, bindings, registry, None, diagnostics);
            }
            // Bare function-call statement: type-check the call as a
            // value too (catches arg-type mismatches), then apply the
            // parser merge effect into workspace if it's a parser.
            let call_expr = Expr::FuncCall {
                namespace: namespace.clone(),
                name: name.clone(),
                args: args.clone(),
            };
            expr_types::check_types(
                &call_expr,
                pipeline_name,
                bindings,
                registry,
                None,
                diagnostics,
            );
            apply_parser_effects(namespace.as_deref(), name, args, registry, bindings);
        }
        ProcessStatement::ExprStmt(e) => {
            expr_types::check_types(e, pipeline_name, bindings, registry, None, diagnostics);
        }
        ProcessStatement::ProcessCall(_name, args) => {
            for a in args {
                expr_types::check_types(a, pipeline_name, bindings, registry, None, diagnostics);
            }
            // Process bodies were validated separately when the named
            // process was defined; we don't recurse from here (the
            // analyzer's pipeline-level walk hits user processes via
            // ProcessChainElement::Named instead).
        }
        ProcessStatement::Drop => {}
        ProcessStatement::If(chain) => {
            analyze_inline_if(chain, pipeline_name, registry, bindings, diagnostics);
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
            analyze_inline_switch(arms, pipeline_name, registry, bindings, diagnostics);
        }
        ProcessStatement::TryCatch(try_body, catch_body) => {
            // Try and catch are alternate branches; bindings at the join
            // are the intersection of both. The catch body starts with
            // `workspace._error` pre-bound as String to match runtime.
            let starting = bindings.clone();

            let mut try_b = starting.clone();
            try_b.push_let_scope();
            for s in try_body {
                analyze_process_stmt(s, pipeline_name, registry, &mut try_b, diagnostics);
            }
            try_b.pop_let_scope();

            let mut catch_b = starting.clone();
            catch_b.push_let_scope();
            catch_b.bind_workspace(&["workspace".into(), "_error".into()], FieldType::String);
            for s in catch_body {
                analyze_process_stmt(s, pipeline_name, registry, &mut catch_b, diagnostics);
            }
            catch_b.pop_let_scope();

            *bindings = intersect_branches(&[try_b, catch_b]);
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
            // Body may or may not run; treat it like an `if` without
            // else for binding purposes — additions don't survive.
            let starting = bindings.clone();
            let mut iter_b = starting.clone();
            iter_b.push_let_scope();
            for s in body {
                analyze_process_stmt(s, pipeline_name, registry, &mut iter_b, diagnostics);
            }
            iter_b.pop_let_scope();
            *bindings = intersect_branches(&[iter_b, starting]);
        }
    }
}

fn analyze_inline_if(
    chain: &IfChain,
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(chain.branches.len() + 1);
    for (cond, body) in &chain.branches {
        expr_types::check_types(cond, pipeline_name, &starting, registry, None, diagnostics);
        let mut b = starting.clone();
        b.push_let_scope();
        for item in body {
            if let BranchBody::Process(s) = item {
                analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
            }
        }
        b.pop_let_scope();
        results.push(b);
    }
    if let Some(else_body) = &chain.else_body {
        let mut b = starting.clone();
        b.push_let_scope();
        for item in else_body {
            if let BranchBody::Process(s) = item {
                analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
            }
        }
        b.pop_let_scope();
        results.push(b);
    } else {
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

fn analyze_inline_switch(
    arms: &[SwitchArm],
    pipeline_name: &str,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(arms.len() + 1);
    let mut has_default = false;
    for arm in arms {
        if let Some(p) = &arm.pattern {
            expr_types::check_types(p, pipeline_name, &starting, registry, None, diagnostics);
        } else {
            has_default = true;
        }
        let mut b = starting.clone();
        b.push_let_scope();
        for item in &arm.body {
            if let BranchBody::Process(s) = item {
                analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
            }
        }
        b.pop_let_scope();
        results.push(b);
    }
    if !has_default {
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

fn analyze_if_chain(
    chain: &IfChain,
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(chain.branches.len() + 1);
    for (cond, body) in &chain.branches {
        expr_types::check_types(cond, pipeline_name, &starting, registry, None, diagnostics);
        let mut b = starting.clone();
        for item in body {
            match item {
                BranchBody::Pipeline(p) => {
                    analyze_pipeline_stmt(p, pipeline_name, config, registry, &mut b, diagnostics);
                }
                BranchBody::Process(s) => {
                    analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
                }
            }
        }
        results.push(b);
    }
    if let Some(else_body) = &chain.else_body {
        let mut b = starting.clone();
        for item in else_body {
            match item {
                BranchBody::Pipeline(p) => {
                    analyze_pipeline_stmt(p, pipeline_name, config, registry, &mut b, diagnostics);
                }
                BranchBody::Process(s) => {
                    analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
                }
            }
        }
        results.push(b);
    } else {
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

fn analyze_switch(
    arms: &[SwitchArm],
    pipeline_name: &str,
    config: &CompiledConfig,
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let starting = bindings.clone();
    let mut results: Vec<Bindings> = Vec::with_capacity(arms.len() + 1);
    let mut has_default = false;
    for arm in arms {
        if let Some(p) = &arm.pattern {
            expr_types::check_types(p, pipeline_name, &starting, registry, None, diagnostics);
        } else {
            has_default = true;
        }
        let mut b = starting.clone();
        for item in &arm.body {
            match item {
                BranchBody::Pipeline(p) => {
                    analyze_pipeline_stmt(p, pipeline_name, config, registry, &mut b, diagnostics);
                }
                BranchBody::Process(s) => {
                    analyze_process_stmt(s, pipeline_name, registry, &mut b, diagnostics);
                }
            }
        }
        results.push(b);
    }
    if !has_default {
        results.push(starting);
    }
    *bindings = intersect_branches(&results);
}

// ---------------------------------------------------------------------------
// Parser effects (workspace merge for bare parse_*(text) statements)
// ---------------------------------------------------------------------------

fn apply_parser_effects(
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
    if let Some(Expr::HashLit(entries)) = args.get(1) {
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
    match e {
        Expr::StringLit(_) | Expr::Template(_) => FieldType::String,
        Expr::IntLit(_) => FieldType::Int,
        Expr::FloatLit(_) => FieldType::Float,
        Expr::BoolLit(_) => FieldType::Bool,
        Expr::Null => FieldType::Null,
        Expr::HashLit(_) => FieldType::Object,
        _ => FieldType::Any,
    }
}

// ---------------------------------------------------------------------------
// Output reference checks
// ---------------------------------------------------------------------------

fn analyze_output(
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
    match expr {
        Expr::Ident(parts) => cb(parts),
        Expr::PropertyAccess(base, suffix) => {
            if let Expr::Ident(base_parts) = base.as_ref() {
                let mut combined = base_parts.clone();
                combined.extend(suffix.iter().cloned());
                cb(&combined);
            } else {
                collect_workspace_refs(base, cb);
            }
        }
        Expr::Template(fragments) => {
            for f in fragments {
                if let TemplateFragment::Interp(e) = f {
                    collect_workspace_refs(e, cb);
                }
            }
        }
        Expr::FuncCall { args, .. } => {
            for a in args {
                collect_workspace_refs(a, cb);
            }
        }
        Expr::BinOp(l, _, r) => {
            collect_workspace_refs(l, cb);
            collect_workspace_refs(r, cb);
        }
        Expr::UnaryOp(_, inner) => collect_workspace_refs(inner, cb),
        Expr::HashLit(entries) => {
            for (_k, v) in entries {
                collect_workspace_refs(v, cb);
            }
        }
        Expr::StringLit(_)
        | Expr::IntLit(_)
        | Expr::FloatLit(_)
        | Expr::BoolLit(_)
        | Expr::Null => {}
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
        // After `syslog.parse(ingress)`, downstream `${workspace.syslog_msg}`
        // resolves silently.
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
        // Defaults narrow the wildcard — typos on undeclared keys are
        // caught.
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
        // No defaults → wildcard, so any workspace.* read is admitted.
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
        // workspace.port is bound as Int via HashLit defaults.
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
        // try sets workspace.a only; catch sets workspace.b only — neither
        // survives the intersection; output of workspace.a errors.
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

    // ----- let bindings --------------------------------------------------

    // ----- Phase 3 UX: spans + suggestions ------------------------------

    #[test]
    fn output_unresolved_workspace_ref_carries_value_span() {
        // The error should carry the span of the property's value
        // expression (the template containing `${workspace.nope}`).
        let src = r#"
def input i { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "${workspace.nope}" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let errs = errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        let span = errs[0].span.expect("error should carry a span");
        // The span should cover the template literal — sanity-check it
        // by resolving against a fresh source map and confirming it
        // points at the offending output line.
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
        // `workspace.synlog_msg` (one transposition off `syslog_msg`) → suggest.
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
        // No bindings exist that are within edit distance — no help.
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
}
