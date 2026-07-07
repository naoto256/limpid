//! Cross-cutting `--check` validator: every `match` filter on a
//! `type journal` input must have `FIELD=value` shape.
//!
//! The runtime path already validates this at daemon startup
//! (`JournalInput::from_properties`), so a malformed filter would
//! fail-fast anyway — but that failure only surfaces the moment the
//! daemon is asked to start, not at `--check` time. Operators
//! running `limpid --check` on a config with `match "no_equals"` are
//! entitled to see the error before deploy, so the same rule is
//! applied on the analyzer side.
//!
//! Format validation stops at the `=` separator. libsystemd's own
//! per-field rules (uppercase field names, no NUL, etc.) are still
//! enforced at runtime by `sd_journal_add_match`; when it rejects a
//! filter the reader terminates with a loud diagnostic — that
//! semantics is not something `--check` can replicate without
//! linking libsystemd on the analyzer path.

use crate::dsl::ast::{Expr, ExprKind, Property};
use crate::dsl::span::Span;
use crate::pipeline::CompiledConfig;

use super::{DiagKind, Diagnostic};

pub(super) fn analyze_all(config: &CompiledConfig, diags: &mut Vec<Diagnostic>) {
    for input_def in config.inputs.values() {
        if input_def.properties.type_name() != "journal" {
            continue;
        }
        for prop in input_def.properties.user_properties() {
            if let Property::KeyValue {
                key,
                value:
                    Expr {
                        kind: ExprKind::StringLit(s),
                        ..
                    },
                value_span,
                ..
            } = prop
                && key == "match"
                && !s.contains('=')
            {
                diags.push(diag_for_bad_match(&input_def.name, s, *value_span));
            }
        }
    }
}

fn diag_for_bad_match(input_name: &str, offending: &str, span: Option<Span>) -> Diagnostic {
    let msg = format!(
        "input '{}': journal `match` requires `FIELD=value` shape (got \"{}\") — each match \
         string must contain '='; see `docs/src/inputs/journal.md` for the filter combining rules",
        input_name, offending
    );
    let mut d = Diagnostic::error_kind(DiagKind::PropertySchema, msg);
    d.span = span;
    d
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

    fn match_format_errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| {
                d.level == crate::check::Level::Error
                    && d.message.contains("journal `match` requires `FIELD=value`")
            })
            .collect()
    }

    #[cfg(feature = "journal")]
    #[test]
    fn rejects_match_string_without_equals() {
        // Format validation is the whole point of this analyzer.
        // `logger -t foo` is fine; the operator config is not.
        let src = r#"
def input j { type journal match "no_equals_here" }
def output o { type file path "/tmp/o.log" }
def pipeline p { input j; output o }
"#;
        let diags = analyze_str(src);
        let errs = match_format_errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        assert!(errs[0].message.contains("\"no_equals_here\""));
    }

    #[cfg(feature = "journal")]
    #[test]
    fn accepts_valid_match_string() {
        let src = r#"
def input j { type journal match "SYSLOG_IDENTIFIER=app" }
def output o { type file path "/tmp/o.log" }
def pipeline p { input j; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            match_format_errors(&diags).is_empty(),
            "valid `FIELD=value` must not error; got: {:?}",
            diags
        );
    }

    #[cfg(feature = "journal")]
    #[test]
    fn accepts_multiple_valid_match_strings() {
        // Repeatable `match` — every one must be validated.
        let src = r#"
def input j {
    type journal
    match "SYSLOG_IDENTIFIER=app1"
    match "SYSLOG_IDENTIFIER=app2"
    match "_UID=1000"
}
def output o { type file path "/tmp/o.log" }
def pipeline p { input j; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            match_format_errors(&diags).is_empty(),
            "all three valid match strings must pass; got: {:?}",
            diags
        );
    }

    #[cfg(feature = "journal")]
    #[test]
    fn rejects_mixed_valid_and_invalid_match_strings() {
        // A valid earlier match must not mask a later malformed one —
        // operators should see every bad filter in one pass.
        let src = r#"
def input j {
    type journal
    match "SYSLOG_IDENTIFIER=app"
    match "typo_here"
    match "_UID=1000"
}
def output o { type file path "/tmp/o.log" }
def pipeline p { input j; output o }
"#;
        let diags = analyze_str(src);
        let errs = match_format_errors(&diags);
        assert_eq!(errs.len(), 1, "got: {:?}", diags);
        assert!(errs[0].message.contains("\"typo_here\""));
    }

    #[test]
    fn ignores_match_property_on_non_journal_inputs() {
        // `match` is a journal-input-only property. The syslog / http
        // / etc. modules do not have it; if they ever gain a
        // similarly-named property with different semantics the
        // analyzer must not misfire.
        let src = r#"
def input i { type syslog_tcp bind "0.0.0.0:514" }
def output o { type file path "/tmp/o.log" }
def pipeline p { input i; output o }
"#;
        let diags = analyze_str(src);
        assert!(
            match_format_errors(&diags).is_empty(),
            "non-journal inputs must not trip the match format check; got: {:?}",
            diags
        );
    }
}
