//! Cross-cutting `--check` warning: outputs that disable TLS
//! certificate verification (`verify false`).
//!
//! `output http` and `output otlp_http` both accept a top-level
//! `verify` toggle that, when set to `false`, calls
//! `reqwest::ClientBuilder::danger_accept_invalid_certs(true)` — the
//! peer's certificate (hostname, chain, expiry) is no longer checked,
//! so any on-path attacker can present an arbitrary certificate and
//! the daemon will happily ship data to it. The runtime already emits
//! a `tracing::warn!` at startup for this (see `modules/output/http.rs`
//! and `modules/output/otlp/http.rs`), but that only fires when the
//! daemon actually loads the config with a live HTTPS peer — CI /
//! `--check` runs on the raw config file get no signal at all today.
//!
//! This module raises one [`Level::Warning`] per affected output so
//! `limpid --check` surfaces the same footgun statically, before the
//! config is ever loaded by a running daemon.
//!
//! The warning is intentionally [`Level::Warning`] rather than
//! [`Level::Error`]: `verify false` is a legitimate, deliberate choice
//! for local/test environments (self-signed certs, mitmproxy-style
//! debugging) — hard-rejecting it at `--check` time would break valid
//! non-production configs. `--check --ultra-strict` promotion is
//! handled at the CLI layer per existing convention.
//!
//! Detection is config-shape-only — no runtime, no I/O. The set of
//! "TLS-client output types with a `verify` toggle" is intentionally
//! hardcoded to the two modules that currently expose it; adding a
//! new one should extend [`OUTPUT_TYPES_WITH_VERIFY_TOGGLE`] in
//! lockstep.

use crate::dsl::props;
use crate::pipeline::CompiledConfig;

use super::{DiagKind, Diagnostic, Level};

/// Output `type` names whose schema exposes a top-level `verify` bool
/// controlling `danger_accept_invalid_certs`. Kept in lockstep with
/// the modules that read `verify` off their properties (see
/// `modules/output/http.rs` and `modules/output/otlp/http.rs`).
const OUTPUT_TYPES_WITH_VERIFY_TOGGLE: &[&str] = &["http", "otlp_http"];

/// Returns `true` when `output`'s `verify` property is explicitly set
/// to `false`. Uses [`props::get_bool`], which accepts both the
/// `BoolLit` form the parser produces and the legacy `Ident("false")`
/// form, so the warning matches every spelling the schema validator
/// admits. Defaults to verification *enabled* when unset — same as
/// the runtime.
fn verify_disabled(output: &crate::dsl::ast::OutputDef) -> bool {
    let props_view = output.properties.user_properties();
    props::get_bool(props_view, "verify") == Some(false)
}

pub(super) fn analyze_all(config: &CompiledConfig, diags: &mut Vec<Diagnostic>) {
    let mut affected: Vec<&str> = Vec::new();
    for (name, output) in &config.outputs {
        if OUTPUT_TYPES_WITH_VERIFY_TOGGLE.contains(&output.properties.type_name())
            && verify_disabled(output)
        {
            affected.push(name.as_str());
        }
    }

    if affected.is_empty() {
        return;
    }

    // Stable ordering for deterministic test assertions and stable
    // diff-able output between runs (HashMap iteration is otherwise
    // arbitrary).
    affected.sort();

    for name in affected {
        let message = format!(
            "output '{}': TLS certificate verification disabled (`verify false`) — \
             MITM possible; intended for test environments only. \
             Connections to this peer will accept any certificate, valid or not.",
            name,
        );
        diags.push(Diagnostic {
            level: Level::Warning,
            kind: DiagKind::Other,
            message,
            span: None,
            help: None,
        });
    }
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

    fn verify_warnings(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| {
                d.level == Level::Warning
                    && d.message.contains("TLS certificate verification disabled")
            })
            .collect()
    }

    #[test]
    fn warns_when_http_output_disables_verify() {
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type http
    peer { url "https://example.com/ingest" }
    verify false
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let ws = verify_warnings(&diags);
        assert_eq!(ws.len(), 1, "got: {:?}", diags);
        assert!(ws[0].message.contains("'o'"), "got: {}", ws[0].message);
    }

    #[test]
    fn warns_when_otlp_http_output_disables_verify() {
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type otlp_http
    peer { endpoint "https://example.com:4318" }
    verify false
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        let ws = verify_warnings(&diags);
        assert_eq!(ws.len(), 1, "got: {:?}", diags);
        assert!(ws[0].message.contains("'o'"), "got: {}", ws[0].message);
    }

    #[test]
    fn no_warning_when_verify_unset() {
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type http
    peer { url "https://example.com/ingest" }
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            verify_warnings(&diags).is_empty(),
            "verify defaults to true, expected no warning, got: {:?}",
            diags,
        );
    }

    #[test]
    fn no_warning_when_verify_true() {
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o {
    type http
    peer { url "https://example.com/ingest" }
    verify true
}
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            verify_warnings(&diags).is_empty(),
            "explicit verify true, expected no warning, got: {:?}",
            diags,
        );
    }

    #[test]
    fn no_warning_for_non_tls_output_type() {
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
            verify_warnings(&diags).is_empty(),
            "file output has no verify toggle, got: {:?}",
            diags,
        );
    }

    #[test]
    fn collapses_multiple_affected_outputs_into_separate_warnings() {
        // Unlike recovery_readiness (single coalesced warning), each
        // insecure output gets its own warning line so an operator
        // scanning `--check` output can see exactly which peers are
        // affected without parsing a combined message.
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output a {
    type http
    peer { url "https://example.com/ingest" }
    verify false
}
def output b {
    type otlp_http
    peer { endpoint "https://example.com:4318" }
    verify false
}
def pipeline p { input i; output a }
"#;
        let diags = analyze_str(src);
        let ws = verify_warnings(&diags);
        assert_eq!(
            ws.len(),
            2,
            "expected one warning per output, got: {:?}",
            diags
        );
    }
}
