//! Cross-cutting `--check` warning: configurations that rely on the
//! `error_log` for failure recovery while leaving it unconfigured.
//!
//! Several recovery paths added in 0.7.8 (PR-O retry-exhausted recovery,
//! PR-P shutdown-flush drain) only persist the original payload when
//! `control { error_log "..." }` is set. With `error_log` missing the
//! runtime falls back to the 0.7.7 behaviour (log + metric only, no
//! replay-able record on disk) — so an operator writing a config with
//! retry / batched outputs and *no* `error_log` silently voids the
//! safety net those features were added to provide.
//!
//! This module raises one [`Level::Warning`] when:
//!
//! 1. `control { error_log "..." }` is not configured, AND
//! 2. at least one `def output` either:
//!    - declares a `retry { ... }` block (= invokes the
//!      [`write_with_retry`] recovery routing path), OR
//!    - is a batched output type (`http`, `otlp_http`, `otlp_grpc`)
//!      whose [`Output::shutdown`] implementation drains the buffer
//!      to `error_log` on flush failure.
//!
//! The warning is intentionally [`Level::Warning`] rather than
//! [`Level::Error`]: an operator may have made an informed choice to
//! accept silent drops on failure (e.g. low-criticality telemetry on
//! a host with no spare disk). `--check --ultra-strict` promotion is
//! handled at the CLI layer per existing convention.
//!
//! Detection is config-shape-only — no runtime, no I/O. The set of
//! "batched output types" is intentionally hardcoded to the three
//! modules that currently override [`Output::shutdown`] with an
//! error-log drain path; adding a new batched output should extend
//! [`OUTPUT_TYPES_WITH_SHUTDOWN_DRAIN`] in lockstep.

use crate::dsl::props;
use crate::pipeline::CompiledConfig;

use super::{DiagKind, Diagnostic, Level};

/// Output `type` names whose `Output::shutdown` impl drains pending
/// batch buffers to the configured `error_log` on flush failure.
/// Kept in lockstep with the modules that override the default
/// no-op `shutdown` in `OutputWriter`.
const OUTPUT_TYPES_WITH_SHUTDOWN_DRAIN: &[&str] = &["http", "otlp_http", "otlp_grpc"];

/// Returns `true` when the operator has set `control { error_log "..." }`.
/// A bare `control { }` block (or no `control` block at all) reads as
/// "not configured".
fn error_log_configured(config: &CompiledConfig) -> bool {
    config
        .global_blocks
        .get("control")
        .and_then(|p| props::get_string(p, "error_log"))
        .is_some()
}

/// Categorise *why* a given output needs `error_log` for full
/// recovery. Returns `None` when the output has no recovery-worthy
/// shape (e.g. a plain `file` output with no retry).
fn recovery_reason(output: &crate::dsl::ast::OutputDef) -> Option<&'static str> {
    let props_view = output.properties.user_properties();

    // Retry block present → enters write_with_retry, which routes
    // exhausted attempts through error_log (BC-3 / PR-O).
    if props::get_block(props_view, "retry").is_some() {
        return Some("retry");
    }

    // Batched output: shutdown flush failures drain to error_log
    // (BC-4 / PR-P). Detected by the output's `type` rather than by
    // a property name because "batched-ness" is an implementation
    // property of the module, not a config shape.
    if OUTPUT_TYPES_WITH_SHUTDOWN_DRAIN.contains(&output.properties.type_name()) {
        return Some("batched");
    }

    None
}

pub(super) fn analyze_all(config: &CompiledConfig, diags: &mut Vec<Diagnostic>) {
    if error_log_configured(config) {
        return;
    }

    // Collect every output that has a recovery-worthy shape. We emit
    // a single warning for the whole config (rather than one per
    // output) so the operator sees one actionable line, not a flood.
    let mut affected: Vec<(&str, &'static str)> = Vec::new();
    for (name, output) in &config.outputs {
        if let Some(reason) = recovery_reason(output) {
            affected.push((name.as_str(), reason));
        }
    }

    if affected.is_empty() {
        return;
    }

    // Stable ordering for deterministic test assertions and stable
    // diff-able output between runs (HashMap iteration is otherwise
    // arbitrary).
    affected.sort_by(|a, b| a.0.cmp(b.0));

    let summary: String = affected
        .iter()
        .map(|(name, reason)| format!("`{}` ({})", name, reason))
        .collect::<Vec<_>>()
        .join(", ");

    let message = format!(
        "config has outputs that depend on `error_log` for failure recovery, \
         but `control {{ error_log \"...\" }}` is not configured. \
         Affected outputs: {}. \
         On retry exhaustion or shutdown flush failure, the original payload \
         will be dropped silently (log + metric only, no replay-able record). \
         To enable recovery, add:\n    \
         control {{\n        error_log \"/var/log/limpid/error_log.jsonl\"\n    }}",
        summary,
    );

    diags.push(Diagnostic {
        level: Level::Warning,
        kind: DiagKind::Other,
        message,
        span: None,
        help: None,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::analyze;
    use crate::dsl::parser::parse_config;
    use crate::dsl::span::SourceMap;

    fn analyze_str(src: &str) -> Vec<Diagnostic> {
        let cfg = parse_config(src).expect("config should parse");
        let compiled = CompiledConfig::from_config(cfg).expect("compile");
        let mut sm = SourceMap::new();
        sm.add_anonymous(src);
        analyze(&compiled, &sm)
    }

    fn recovery_warnings(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| {
                d.level == Level::Warning
                    && d.message.contains("depend on `error_log` for failure recovery")
            })
            .collect()
    }

    #[test]
    fn warns_when_retry_block_present_without_error_log() {
        // syslog_tcp with an explicit retry block but no
        // `control { error_log }` — exactly the silent-drop scenario.
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    peer { host "h" port 1 }
    retry { max_attempts 3 }
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let ws = recovery_warnings(&diags);
        assert_eq!(ws.len(), 1, "expected exactly one recovery warning, got: {:?}", diags);
        assert!(ws[0].message.contains("`o` (retry)"), "got: {}", ws[0].message);
    }

    #[test]
    fn warns_for_batched_output_type_without_error_log() {
        // http output is batched (overrides Output::shutdown to drain
        // buffer to error_log on failure). Plain config, no retry,
        // still recovery-worthy.
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type http
    url "https://example.com/ingest"
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let ws = recovery_warnings(&diags);
        assert_eq!(ws.len(), 1, "got: {:?}", diags);
        assert!(ws[0].message.contains("`o` (batched)"), "got: {}", ws[0].message);
    }

    #[test]
    fn no_warning_when_error_log_configured() {
        let src = r#"
control { error_log "/var/log/limpid/error_log.jsonl" }
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type syslog_tcp
    peer { host "h" port 1 }
    retry { max_attempts 3 }
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            recovery_warnings(&diags).is_empty(),
            "operator opted in via error_log, expected no warning, got: {:?}",
            diags,
        );
    }

    #[test]
    fn no_warning_for_plain_file_output_without_recovery_shape() {
        // file output, no retry, no secondary, not batched → no
        // recovery-worthy path exists, so error_log is irrelevant.
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type file
    path "/tmp/o.log"
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            recovery_warnings(&diags).is_empty(),
            "plain file output should not raise the warning, got: {:?}",
            diags,
        );
    }

    #[test]
    fn collapses_multiple_affected_outputs_into_single_warning() {
        // Two outputs each carry a different recovery-worthy shape.
        // We still emit exactly one warning, listing both.
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output a {
    type syslog_tcp
    peer { host "h" port 1 }
    retry { max_attempts 3 }
}
def output b {
    type http
    url "https://example.com/ingest"
}
def pipeline p { input i; output a }
"#;
        let diags = analyze_str(src);
        let ws = recovery_warnings(&diags);
        assert_eq!(ws.len(), 1, "expected single coalesced warning, got: {:?}", diags);
        assert!(ws[0].message.contains("`a` (retry)"), "got: {}", ws[0].message);
        assert!(ws[0].message.contains("`b` (batched)"), "got: {}", ws[0].message);
    }
}
