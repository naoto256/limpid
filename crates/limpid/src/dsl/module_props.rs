//! `ModuleProperties` — type-enforced property surface for `def input/output`.
//!
//! Why a dedicated type instead of `&[Property]`:
//!
//! The DSL `def output foo { type syslog_tcp; peer { host "..."; ... } }` block produces a
//! property list that mixes one structural key (`type`, the module selector)
//! with the Module's own user properties (`address`, `bind`, `queue { ... }`).
//! Every consumer downstream — analyzer schema check, runtime schema check,
//! factory closure, Module impl — wants only the *user* properties; `type` is
//! the registry's indirection and is not part of any module's declared schema.
//!
//! 0.7.2 carried this invariant only by convention. Each call site was expected
//! to strip `type` before validating against the schema (the analyzer carried a
//! private `strip_type_property` helper in `check::module_props` for that
//! purpose; both since removed in 0.7.3). The runtime side (`create_input` /
//! `create_output`) skipped the strip, and the resulting asymmetry slipped
//! past CI: `--check` reported "OK", `cargo run` rejected the same config with
//! "unknown property 'type'". Diagnosed and root-caused in v0.7.3.
//!
//! `ModuleProperties` makes that asymmetry structurally impossible. The parser
//! constructs one of these for every `def input` / `def output`; `type` is
//! extracted into a typed field at construction time and never re-surfaces in
//! the `&[Property]` view that Module code reads. There is no way to forget
//! the strip because the strip happened once, at the type boundary.
//!
//! This type lives in `dsl::` (not `modules::`) because its own logic only
//! manipulates [`crate::dsl::ast::Property`] — it carries no module-runtime
//! knowledge (`Module`, `BuildContext`, registry). `dsl::ast::InputDef` /
//! `OutputDef` already embed it, so keeping it in `dsl::` makes that
//! dependency one-directional (`dsl` never reaches into `modules`) instead
//! of the earlier `dsl::ast` <-> `modules` cycle.

use super::ast::{Expr, ExprKind, Property};
use super::span::Span;

/// Property surface of a single `def input` / `def output` block.
///
/// Constructed by the parser. Carries the resolved module `type` (the
/// indirection that selects the Module), the span of the `type` value
/// expression for diagnostics, and the remaining user properties that the
/// Module impl actually consumes. There is intentionally no public accessor
/// that returns the raw property list with `type` still in it — the strip is
/// the entire reason this type exists.
#[derive(Debug, Clone)]
pub struct ModuleProperties {
    type_name: String,
    type_span: Option<Span>,
    user: Vec<Property>,
}

/// Error returned by [`ModuleProperties::parse`] when the property surface
/// does not satisfy the structural invariant. These surface at parse time —
/// before any analyzer pass or registry lookup runs — because a `def input` /
/// `def output` without a valid `type` is structurally incomplete in the
/// same sense as an unclosed brace.
#[derive(Debug, Clone)]
pub enum ModulePropertyError {
    /// No `type` key was present in the property list.
    Missing,
    /// The `type` key exists but its value is not a bare identifier (e.g.
    /// `type "syslog_tcp"` as a string literal, or `type { ... }` as a block).
    /// The grammar in principle should reject this earlier, but the
    /// property parser is permissive about value shapes, so we re-check.
    /// `span` is the value (or block-key) span; currently unused by `Display`,
    /// kept so a future `--check` integration can underline the offender.
    NonIdent {
        #[allow(dead_code)]
        span: Option<Span>,
    },
    /// More than one `type` key was supplied. Last-write-wins would mask
    /// the operator's intent, so we reject loudly. `span` points at the
    /// second occurrence — same forward-compatibility caveat as
    /// [`Self::NonIdent`].
    Duplicate {
        #[allow(dead_code)]
        span: Option<Span>,
    },
}

impl std::fmt::Display for ModulePropertyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "missing required property 'type'"),
            Self::NonIdent { .. } => {
                write!(
                    f,
                    "'type' must be a bare identifier (e.g. `type syslog_tcp`)"
                )
            }
            Self::Duplicate { .. } => write!(f, "'type' specified more than once"),
        }
    }
}

impl std::error::Error for ModulePropertyError {}

impl ModuleProperties {
    /// Build a `ModuleProperties` from the raw property list a parser produced
    /// for a single `def input` / `def output` block. Extracts `type` into a
    /// typed slot and returns the remaining user properties; rejects missing,
    /// duplicated, or non-ident `type`.
    pub fn parse(raw: Vec<Property>) -> Result<Self, ModulePropertyError> {
        let mut type_name: Option<(String, Option<Span>)> = None;
        let mut user = Vec::with_capacity(raw.len());
        for prop in raw {
            let key = match &prop {
                Property::KeyValue { key, .. } | Property::Block { key, .. } => key.as_str(),
            };
            if key != "type" {
                user.push(prop);
                continue;
            }
            match &prop {
                Property::KeyValue {
                    value:
                        Expr {
                            kind: ExprKind::Ident(parts),
                            ..
                        },
                    value_span,
                    ..
                } => {
                    // Single-segment idents only. `type kafka.output` is
                    // rejected as `NonIdent` — otherwise the multi-segment
                    // `ident_path` from the grammar would silently truncate
                    // to the first segment and resolve as `kafka`.
                    let [first] = parts.as_slice() else {
                        return Err(ModulePropertyError::NonIdent { span: *value_span });
                    };
                    if type_name.is_some() {
                        return Err(ModulePropertyError::Duplicate { span: *value_span });
                    }
                    type_name = Some((first.clone(), *value_span));
                }
                Property::KeyValue { value_span, .. } => {
                    return Err(ModulePropertyError::NonIdent { span: *value_span });
                }
                Property::Block { key_span, .. } => {
                    return Err(ModulePropertyError::NonIdent { span: *key_span });
                }
            }
        }
        let (type_name, type_span) = type_name.ok_or(ModulePropertyError::Missing)?;
        Ok(Self {
            type_name,
            type_span,
            user,
        })
    }

    /// The resolved module type identifier (e.g. `"syslog_tcp"`, `"syslog_udp"`).
    /// Always populated by construction.
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Span of the `type` value expression in the source file, when the
    /// parser supplied one. Used by `--check` to point its caret at the
    /// offending `type tcsp` ident on a typo'd type name.
    pub fn type_span(&self) -> Option<Span> {
        self.type_span
    }

    /// All properties the Module impl is allowed to see — i.e. everything
    /// except `type`. Schema validation, analyzer passes, and
    /// `from_properties` impls all consume this view.
    pub fn user_properties(&self) -> &[Property] {
        &self.user
    }

    /// Build directly from a `type` name + user properties without going
    /// through the parser path. Used by tests that hand-construct a
    /// `ModuleProperties` to drive a Module impl; production paths always
    /// go through [`Self::parse`].
    #[cfg(test)]
    pub fn from_parts(type_name: impl Into<String>, user: Vec<Property>) -> Self {
        Self {
            type_name: type_name.into(),
            type_span: None,
            user,
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str) -> Property {
        Property::Block {
            key: key.into(),
            key_span: None,
            properties: vec![],
        }
    }

    #[test]
    fn parse_extracts_type_and_strips_it_from_user_properties() {
        let raw = vec![
            kv("type", ExprKind::Ident(vec!["syslog_tcp".into()])),
            kv("address", ExprKind::StringLit("127.0.0.1:514".into())),
            block("queue"),
        ];
        let mp = ModuleProperties::parse(raw).expect("should parse");
        assert_eq!(mp.type_name(), "syslog_tcp");
        // type is gone from the user view; only address + queue remain
        assert_eq!(mp.user_properties().len(), 2);
        let keys: Vec<&str> = mp
            .user_properties()
            .iter()
            .map(|p| match p {
                Property::KeyValue { key, .. } | Property::Block { key, .. } => key.as_str(),
            })
            .collect();
        assert_eq!(keys, vec!["address", "queue"]);
    }

    #[test]
    fn parse_rejects_missing_type() {
        let raw = vec![kv("address", ExprKind::StringLit("h:1".into()))];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::Missing));
        assert!(err.to_string().contains("missing required property 'type'"));
    }

    #[test]
    fn parse_rejects_non_ident_type_string_literal() {
        // `type "syslog_tcp"` — string instead of bare ident
        let raw = vec![kv("type", ExprKind::StringLit("syslog_tcp".into()))];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::NonIdent { .. }));
    }

    #[test]
    fn parse_rejects_non_ident_type_block() {
        // `type { ... }` — block instead of value
        let raw = vec![block("type")];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::NonIdent { .. }));
    }

    #[test]
    fn parse_rejects_multi_segment_type_ident() {
        // `type kafka.output` — the parser produces
        // `ExprKind::Ident(vec!["kafka", "output"])` via the `ident_path`
        // rule. `parse` must reject this rather than silently truncating
        // to the first segment.
        let raw = vec![kv(
            "type",
            ExprKind::Ident(vec!["kafka".into(), "output".into()]),
        )];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::NonIdent { .. }));
    }

    #[test]
    fn parse_rejects_empty_ident_path() {
        // Defensive: `Ident(vec![])` should also fail as `NonIdent`.
        let raw = vec![kv("type", ExprKind::Ident(vec![]))];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::NonIdent { .. }));
    }

    #[test]
    fn parse_rejects_duplicate_type() {
        let raw = vec![
            kv("type", ExprKind::Ident(vec!["syslog_tcp".into()])),
            kv("type", ExprKind::Ident(vec!["syslog_udp".into()])),
        ];
        let err = ModuleProperties::parse(raw).expect_err("should fail");
        assert!(matches!(err, ModulePropertyError::Duplicate { .. }));
    }

    #[test]
    fn from_parts_preserves_user_properties_verbatim() {
        // Test-only helper short-circuits the parse step; verify the round-trip
        // shape matches what a parser-produced ModuleProperties would expose.
        let props = vec![kv("address", ExprKind::StringLit("h:1".into()))];
        let mp = ModuleProperties::from_parts("syslog_tcp", props.clone());
        assert_eq!(mp.type_name(), "syslog_tcp");
        assert_eq!(mp.user_properties().len(), 1);
    }
}
