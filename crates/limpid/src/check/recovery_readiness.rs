//! Cross-cutting `--check` warning: configurations that rely on the
//! `error_log` for failure recovery while leaving it unconfigured.
//!
//! Several recovery paths added in 0.7.8 (retry-exhausted recovery and
//! the batched-output shutdown-flush drain) only persist the original
//! payload to a durable, easily-replayable **file** when
//! `control { error_log "..." }` is set. With `error_log` missing the
//! runtime emits a one-line `tracing::error!` summary per failure —
//! the payload is not written anywhere by default. The `Meta` and
//! `Full` values of [`crate::error_log::ErrorLogFallback`] can attach
//! structured metadata / the full JSONL to the tracing line, but
//! that opt-in only takes effect when `error_log` is set (a
//! separate `--check` warning surfaces the inert
//! `error_log_fallback`-without-`error_log` combination). Without
//! either a DLQ file or an explicit fallback opt-in the failure
//! record is a summary line only — no `limpidctl inject` replay
//! shortcut, no journald payload extraction.
//!
//! This module raises one [`Level::Warning`] when:
//!
//! 1. `control { error_log "..." }` is not configured, AND
//! 2. at least one `def output` either:
//!    - declares a `retry { ... }` block (= enters the output's
//!      retry-exhausted recovery path that routes the payload to
//!      `error_log`), OR
//!    - is a batched output type (`http`, `otlp_http`, `otlp_grpc`)
//!      whose [`Output::shutdown`] implementation drains the buffer
//!      to `error_log` on flush failure.
//!
//! The warning is intentionally [`Level::Warning`] rather than
//! [`Level::Error`]: an operator may have made an informed choice to
//! accept the fragile tracing fallback on failure (e.g.
//! low-criticality telemetry on a host with no spare disk, where
//! journald-based recovery is acceptable). `--check --ultra-strict`
//! promotion is handled at the CLI layer per existing convention.
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
/// no-op `shutdown` on the `modules::Output` trait.
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

    // Retry block present → enters the output's retry-exhausted
    // recovery path, which routes the payload through error_log.
    if props::get_block(props_view, "retry").is_some() {
        return Some("retry");
    }

    // Batched output: shutdown flush failures drain to error_log.
    // Detected by the output's `type` rather than by
    // a property name because "batched-ness" is an implementation
    // property of the module, not a config shape.
    if OUTPUT_TYPES_WITH_SHUTDOWN_DRAIN.contains(&output.properties.type_name()) {
        return Some("batched");
    }

    None
}

pub(super) fn analyze_all(config: &CompiledConfig, diags: &mut Vec<Diagnostic>) {
    // Independent warning: `error_log_fallback` set without a
    // corresponding `error_log`. The fallback is a confidentiality
    // opt-in that only shapes what appears on the tracing side when
    // `error_log` writes fail; without `error_log` the runtime
    // ignores the value (row-A ordering guard in
    // `emit_dlq_tracing_fallback`), so a solo `error_log_fallback`
    // is inert and almost certainly an operator misconfiguration.
    // Warn — not error — because the shape harmlessly appears in
    // shared-template configs where individual environments
    // deactivate `error_log`.
    if let Some(fallback_str) = config
        .global_blocks
        .get("control")
        .and_then(|p| props::get_string(p, "error_log_fallback"))
        && !error_log_configured(config)
    {
        diags.push(Diagnostic {
            level: Level::Warning,
            kind: DiagKind::Other,
            message: format!(
                "control.error_log_fallback = \"{}\" is set but control.error_log \
                 is unset — the fallback is inert without a durable DLQ target. \
                 Either set control.error_log to opt into the fallback (payload / \
                 metadata surface on the tracing side when the DLQ write itself \
                 fails), or remove control.error_log_fallback to silence this \
                 warning.",
                fallback_str
            ),
            span: None,
            help: None,
        });
    }

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
         Without `error_log` the daemon emits a one-line `tracing::error!` \
         summary per failed event but does not persist the payload anywhere \
         — the metadata / full-JSONL tracing fallback is off by default and \
         requires an explicit `control {{ error_log_fallback \"meta\" | \"full\" }}` \
         opt-in, which itself only takes effect when `error_log` is also set. \
         To enable durable file-based recovery, add:\n    \
         control {{\n        error_log \"/var/log/limpid/errored.jsonl\"\n    }}",
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
                    && d.message
                        .contains("depend on `error_log` for failure recovery")
            })
            .collect()
    }

    #[test]
    fn warns_when_retry_block_present_without_error_log() {
        // syslog_tcp with an explicit retry block but no
        // `control { error_log }` — exactly the tracing-fallback
        // scenario the warning targets.
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
        assert_eq!(
            ws.len(),
            1,
            "expected exactly one recovery warning, got: {:?}",
            diags
        );
        assert!(
            ws[0].message.contains("`o` (retry)"),
            "got: {}",
            ws[0].message
        );
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
        assert!(
            ws[0].message.contains("`o` (batched)"),
            "got: {}",
            ws[0].message
        );
    }

    #[test]
    fn no_warning_when_error_log_configured() {
        let src = r#"
control { error_log "/var/log/limpid/errored.jsonl" }
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
        // file output, no retry, not batched → no recovery-worthy
        // path exists, so error_log is irrelevant.
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

    fn fallback_warnings(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| {
                d.level == Level::Warning && d.message.contains("control.error_log_fallback = \"")
            })
            .collect()
    }

    #[test]
    fn warns_when_error_log_fallback_set_without_error_log() {
        // Operator opted into the tracing-side confidentiality
        // ladder but never gave the runtime a DLQ target. The
        // fallback is inert here; surface the misconfiguration so
        // the operator either adds error_log or drops the setting.
        let src = r#"
control { error_log_fallback "meta" }
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type file path "/tmp/o.log" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let ws = fallback_warnings(&diags);
        assert_eq!(
            ws.len(),
            1,
            "expected fallback-without-error_log warning, got: {:?}",
            diags
        );
        assert!(
            ws[0].message.contains("\"meta\""),
            "warning must quote the offending value; got: {}",
            ws[0].message
        );
    }

    #[test]
    fn no_fallback_warning_when_error_log_also_set() {
        // Both configured together is the healthy shape.
        let src = r#"
control {
    error_log "/var/log/limpid/errored.jsonl"
    error_log_fallback "full"
}
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type file path "/tmp/o.log" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            fallback_warnings(&diags).is_empty(),
            "fallback + error_log together should not warn; got: {:?}",
            diags
        );
    }

    #[test]
    fn no_fallback_warning_when_neither_set() {
        // The recovery-readiness warning may still fire for
        // recovery-worthy outputs, but the fallback-specific
        // warning must not: nothing about the fallback is
        // misconfigured here.
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type file path "/tmp/o.log" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            fallback_warnings(&diags).is_empty(),
            "no `error_log_fallback` set should not fire the fallback warning; \
             got: {:?}",
            diags
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
        assert_eq!(
            ws.len(),
            1,
            "expected single coalesced warning, got: {:?}",
            diags
        );
        assert!(
            ws[0].message.contains("`a` (retry)"),
            "got: {}",
            ws[0].message
        );
        assert!(
            ws[0].message.contains("`b` (batched)"),
            "got: {}",
            ws[0].message
        );
    }
}
