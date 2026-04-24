//! Static analyzer for limpid configurations.
//!
//! This module is the entry point for `limpid --check`. Commit 1 of Block 9
//! only lays down the API skeleton — the analyzer returns an empty diagnostic
//! list, preserving the historical "Configuration OK" behaviour. Subsequent
//! commits flesh out type checking, span-aware diagnostics, suggestions,
//! include expansion, and submodule splits.
//!
//! API shape (kept deliberately small for now):
//!   - [`analyze`] — run static checks against a compiled configuration.
//!   - [`Diagnostic`] — a single issue discovered by the analyzer.
//!   - [`Level`] — severity of a diagnostic.
//!   - [`Span`] — byte range into the original source, used in later commits.

use crate::pipeline::CompiledConfig;

/// Severity of an analyzer diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
    Info,
}

/// Byte range into the original source text. Populated by later commits
/// (Phase 3 UX); kept as a placeholder type today so call sites can already
/// thread `Option<Span>` through without churn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

/// A single issue produced by the analyzer.
///
/// `span` is intentionally unused at this commit — it is wired into
/// diagnostic construction in the Phase 3 UX commit (source snippet + caret
/// rendering). Keeping the field today avoids churn at the call sites that
/// later commits add.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub level: Level,
    pub message: String,
    #[allow(dead_code)]
    pub span: Option<Span>,
}

impl Diagnostic {
    #[allow(dead_code)]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            message: message.into(),
            span: None,
        }
    }

    #[allow(dead_code)]
    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            message: message.into(),
            span: None,
        }
    }

    #[allow(dead_code)]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: Level::Info,
            message: message.into(),
            span: None,
        }
    }
}

/// Run the static analyzer.
///
/// At this commit the analyzer is intentionally empty: all real checks are
/// handled by `CompiledConfig::from_config` and `CompiledConfig::validate` in
/// the caller. Later commits move type checking, ident resolution, and
/// parser-effect tracking into this module.
pub fn analyze(_config: &CompiledConfig, _source: &str) -> Vec<Diagnostic> {
    Vec::new()
}
