//! Declarative property schema for limpid configuration surfaces.
//!
//! Each Module (and the top-level `control` / `table` blocks) advertises
//! its acceptable property shape as a `&'static [PropertySpec]`. The
//! same static slice is consumed by:
//!
//! * `--check` (the analyzer) — to surface unknown keys, missing
//!   required ones, type-mismatched values, and out-of-set enum values
//!   as loud diagnostics *before* the daemon is started; and
//! * runtime (`Module::from_properties`) — to fail fast with the same
//!   wording when a config slips past `--check` (e.g. someone runs the
//!   daemon directly).
//!
//! Single source of truth: the validator never invents semantics the
//! analyzer or the runtime don't share. If `framing` accepts
//! `octet_counting | non_transparent`, both surfaces enforce exactly
//! that set, and a typo (`non_trasnaprent`) gets a did-you-mean from
//! the same Levenshtein call.

use super::ast::{Expr, ExprKind, Property};
use super::span::Span;

// ---------------------------------------------------------------------------
// Schema declaration types
// ---------------------------------------------------------------------------

/// What kind of value (or sub-block) a property is allowed to carry.
///
/// `Copy` so a `PropertySpec` literal in `static` context is trivial.
/// `Block` recurses into another static slice — Rust allows this
/// because `&'static [PropertySpec]` is a reference, not the struct
/// itself, so there's no infinite size.

#[derive(Debug, Clone, Copy)]
pub enum PropertyValueKind {
    /// Any string-shaped scalar: `StringLit`, `Template`, or a bare
    /// `Ident` (the legacy "unquoted-string" form many existing
    /// configs use — `path /var/log/foo.log`).
    String,
    /// Signed integer literal.
    Int,
    /// `true` / `false`, either as `BoolLit` (the canonical form) or as
    /// a bare ident — the latter preserves existing `verify false`
    /// idioms without forcing a config rewrite.
    Bool,
    /// String literal parseable by `props::parse_duration` (`1s`, `5m`,
    /// `100ms`). The validator accepts any string shape and only
    /// rejects parse failures — that keeps the rule "schema does
    /// shape, runtime does semantics" intact.
    Duration,
    /// String literal parseable by `props::parse_size` (`1GB`, `512MB`).
    Size,
    /// Bare ident whose value must be one of the listed strings.
    /// `framing { octet_counting | non_transparent }`, `queue.type
    /// { memory | disk }`, etc.
    Enum(&'static [&'static str]),
    /// Nested block. The slice describes the inner schema; the
    /// validator recurses.
    Block(&'static [PropertySpec]),
    /// Open block whose keys are user-defined identifiers (HTTP
    /// header names, k8s-style labels, etc.) and whose values must
    /// each be string-shaped. The schema validator never flags an
    /// "unknown key" inside this block — it only checks that every
    /// entry is a key-value (not a nested sub-block) with a
    /// string-shaped value.
    StringMap,
}

/// One declared key in a property surface.
#[derive(Debug, Clone, Copy)]
pub struct PropertySpec {
    pub name: &'static str,
    pub required: bool,
    pub kind: PropertyValueKind,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What went wrong with a single property — what the schema expected
/// versus what the parsed config carries.
#[derive(Debug, Clone)]
pub enum SchemaErrorKind {
    /// Property key isn't declared in the schema.
    UnknownKey,
    /// Schema marks `required: true` and the key is absent.
    MissingRequired,
    /// Same key appears twice in the same surface. Later occurrence
    /// silently won in legacy code — surface it explicitly.
    DuplicateKey,
    /// Schema declares this key as a `Block(_)` but the config wrote a
    /// scalar (`tls "..."`).
    ExpectedBlock,
    /// Schema declares this key as a scalar/enum but the config wrote
    /// a block (`address { ... }`).
    ExpectedValue,
    /// Scalar value doesn't match the declared kind (e.g. an `Int`
    /// expected but a string given; a `Duration` that failed to parse).
    TypeMismatch { expected: &'static str },
    /// Bare ident value doesn't match any of the allowed enum variants.
    UnknownEnumValue {
        allowed: &'static [&'static str],
    },
}

/// One validation finding. Carries enough span context for either the
/// runtime (which formats it via [`std::fmt::Display`]) or the analyzer
/// (which threads `key_span` / `value_span` into its `Diagnostic`).
#[derive(Debug, Clone)]
pub struct SchemaError {
    pub kind: SchemaErrorKind,
    pub key: String,
    pub key_span: Option<Span>,
    pub value_span: Option<Span>,
    pub did_you_mean: Option<String>,
}

impl SchemaError {
    /// Best span for a caret — `key_span` for key-shaped problems,
    /// `value_span` for value-shaped ones, falling back to whichever
    /// is present.

    pub fn primary_span(&self) -> Option<Span> {
        use SchemaErrorKind::*;
        match self.kind {
            UnknownKey | MissingRequired | DuplicateKey | ExpectedBlock | ExpectedValue => {
                self.key_span.or(self.value_span)
            }
            TypeMismatch { .. } | UnknownEnumValue { .. } => self.value_span.or(self.key_span),
        }
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use SchemaErrorKind::*;
        match &self.kind {
            UnknownKey => {
                write!(f, "unknown property '{}'", self.key)?;
                if let Some(s) = &self.did_you_mean {
                    write!(f, " (did you mean '{}'?)", s)?;
                }
                Ok(())
            }
            MissingRequired => write!(f, "missing required property '{}'", self.key),
            DuplicateKey => write!(f, "duplicate property '{}'", self.key),
            ExpectedBlock => write!(f, "'{}' expects a block ({{ ... }})", self.key),
            ExpectedValue => write!(f, "'{}' expects a value, not a block", self.key),
            TypeMismatch { expected } => {
                write!(f, "'{}' expects {}", self.key, expected)
            }
            UnknownEnumValue { allowed } => {
                write!(f, "'{}' has unknown value", self.key)?;
                if let Some(s) = &self.did_you_mean {
                    write!(f, " (did you mean '{}'?)", s)?;
                } else {
                    write!(f, " (allowed: {})", allowed.join(", "))?;
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Validator
// ---------------------------------------------------------------------------

/// Validate one property surface against its schema. Returns every
/// finding (not just the first) so `--check` can show the full list
/// in one pass — the whole point of the loud diagnostic mode.
///
/// Sub-blocks are validated recursively against their inner spec.
pub fn validate(props: &[Property], spec: &[PropertySpec]) -> Vec<SchemaError> {
    let mut errs = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for prop in props {
        let (name, key_span) = match prop {
            Property::KeyValue { key, key_span, .. } => (key.as_str(), *key_span),
            Property::Block { key, key_span, .. } => (key.as_str(), *key_span),
        };

        if !seen.insert(name) {
            errs.push(SchemaError {
                kind: SchemaErrorKind::DuplicateKey,
                key: name.to_string(),
                key_span,
                value_span: None,
                did_you_mean: None,
            });
            // Still fall through and validate the value — typo and
            // type-mismatch findings on the duplicate are still useful.
        }

        let Some(s) = spec.iter().find(|s| s.name == name) else {
            let dym = nearest(name, spec.iter().map(|s| s.name));
            errs.push(SchemaError {
                kind: SchemaErrorKind::UnknownKey,
                key: name.to_string(),
                key_span,
                value_span: prop_value_span(prop),
                did_you_mean: dym,
            });
            continue;
        };

        match (&s.kind, prop) {
            (PropertyValueKind::Block(inner_spec), Property::Block { properties, .. }) => {
                errs.extend(validate(properties, inner_spec));
            }
            (PropertyValueKind::StringMap, Property::Block { properties, .. }) => {
                errs.extend(validate_string_map(properties));
            }
            (PropertyValueKind::Block(_) | PropertyValueKind::StringMap,
                Property::KeyValue { value_span, .. }) => {
                errs.push(SchemaError {
                    kind: SchemaErrorKind::ExpectedBlock,
                    key: name.to_string(),
                    key_span,
                    value_span: *value_span,
                    did_you_mean: None,
                });
            }
            (_, Property::Block { .. }) => {
                errs.push(SchemaError {
                    kind: SchemaErrorKind::ExpectedValue,
                    key: name.to_string(),
                    key_span,
                    value_span: None,
                    did_you_mean: None,
                });
            }
            (kind, Property::KeyValue {
                value, value_span, ..
            }) => {
                if let Some(err) = check_value(name, *kind, value, *value_span, key_span) {
                    errs.push(err);
                }
            }
        }
    }

    for s in spec {
        if s.required && !seen.contains(s.name) {
            errs.push(SchemaError {
                kind: SchemaErrorKind::MissingRequired,
                key: s.name.to_string(),
                key_span: None,
                value_span: None,
                did_you_mean: None,
            });
        }
    }

    errs
}

/// Validate a `StringMap`-kind block: every entry must be a key-value
/// with a string-shaped value. The key set is open, so there is no
/// unknown-key check.
fn validate_string_map(properties: &[Property]) -> Vec<SchemaError> {
    let mut errs = Vec::new();
    for prop in properties {
        match prop {
            Property::KeyValue {
                key,
                key_span,
                value,
                value_span,
            } => {
                match &value.kind {
                    ExprKind::StringLit(_)
                    | ExprKind::Template(_)
                    | ExprKind::Ident(_)
                    | ExprKind::IntLit(_) => {}
                    _ => errs.push(SchemaError {
                        kind: SchemaErrorKind::TypeMismatch { expected: "a string" },
                        key: key.clone(),
                        key_span: *key_span,
                        value_span: *value_span,
                        did_you_mean: None,
                    }),
                }
            }
            Property::Block { key, key_span, .. } => {
                errs.push(SchemaError {
                    kind: SchemaErrorKind::ExpectedValue,
                    key: key.clone(),
                    key_span: *key_span,
                    value_span: None,
                    did_you_mean: None,
                });
            }
        }
    }
    errs
}

fn prop_value_span(p: &Property) -> Option<Span> {
    match p {
        Property::KeyValue { value_span, .. } => *value_span,
        Property::Block { .. } => None,
    }
}

fn check_value(
    key: &str,
    kind: PropertyValueKind,
    value: &Expr,
    value_span: Option<Span>,
    key_span: Option<Span>,
) -> Option<SchemaError> {
    let mismatch = |expected: &'static str| {
        Some(SchemaError {
            kind: SchemaErrorKind::TypeMismatch { expected },
            key: key.to_string(),
            key_span,
            value_span,
            did_you_mean: None,
        })
    };

    match kind {
        PropertyValueKind::String => match &value.kind {
            ExprKind::StringLit(_) | ExprKind::Template(_) | ExprKind::Ident(_) => None,
            ExprKind::IntLit(_) => None, // int → string coercion is accepted by props::get_string
            _ => mismatch("a string"),
        },
        PropertyValueKind::Int => match &value.kind {
            ExprKind::IntLit(_) => None,
            _ => mismatch("an integer"),
        },
        PropertyValueKind::Bool => match &value.kind {
            ExprKind::BoolLit(_) => None,
            ExprKind::Ident(parts)
                if parts.len() == 1 && (parts[0] == "true" || parts[0] == "false") =>
            {
                None
            }
            _ => mismatch("a boolean (true | false)"),
        },
        PropertyValueKind::Duration => match &value.kind {
            ExprKind::StringLit(s) => {
                if super::props::parse_duration(s).is_ok() {
                    None
                } else {
                    mismatch("a duration string like \"5s\" or \"100ms\"")
                }
            }
            _ => mismatch("a duration string"),
        },
        PropertyValueKind::Size => match &value.kind {
            ExprKind::StringLit(s) => {
                if super::props::parse_size(s).is_ok() {
                    None
                } else {
                    mismatch("a size string like \"100MB\" or \"1GB\"")
                }
            }
            _ => mismatch("a size string"),
        },
        PropertyValueKind::Enum(allowed) => match &value.kind {
            ExprKind::Ident(parts) if parts.len() == 1 => {
                let candidate = parts[0].as_str();
                if allowed.contains(&candidate) {
                    None
                } else {
                    let dym = nearest(candidate, allowed.iter().copied());
                    Some(SchemaError {
                        kind: SchemaErrorKind::UnknownEnumValue { allowed },
                        key: key.to_string(),
                        key_span,
                        value_span,
                        did_you_mean: dym,
                    })
                }
            }
            _ => mismatch("an identifier"),
        },
        PropertyValueKind::Block(_) | PropertyValueKind::StringMap => {
            // unreachable: handled at the caller's match arm before we get here.
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Levenshtein-based "did you mean ...?"
// ---------------------------------------------------------------------------

/// Pick the closest candidate to `needle` from `candidates`, or `None`
/// when nothing falls within the typo threshold `max(2, len/3)`.
/// Tie-breaks alphabetically for deterministic output.
fn nearest<'a, I>(needle: &str, candidates: I) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let threshold = (needle.len() / 3).max(2);
    let mut best: Option<(&'a str, usize)> = None;
    for cand in candidates {
        if cand == needle {
            continue;
        }
        let d = levenshtein(needle, cand);
        if d > threshold {
            continue;
        }
        match best {
            None => best = Some((cand, d)),
            Some((bname, bd)) => {
                if d < bd || (d == bd && cand < bname) {
                    best = Some((cand, d));
                }
            }
        }
    }
    best.map(|(n, _)| n.to_string())
}

/// Standard Levenshtein edit distance. Case-sensitive on purpose —
/// DSL identifiers are case-sensitive.
pub fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.is_empty() {
        return b_chars.len();
    }
    if b_chars.is_empty() {
        return a_chars.len();
    }
    let (short, long) = if a_chars.len() <= b_chars.len() {
        (&a_chars, &b_chars)
    } else {
        (&b_chars, &a_chars)
    };
    let mut prev: Vec<usize> = (0..=short.len()).collect();
    let mut curr = vec![0usize; short.len() + 1];
    for (i, lc) in long.iter().enumerate() {
        curr[0] = i + 1;
        for (j, sc) in short.iter().enumerate() {
            let cost = if lc == sc { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[short.len()]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::{Expr, ExprKind};

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.into(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str, properties: Vec<Property>) -> Property {
        Property::Block {
            key: key.into(),
            key_span: None,
            properties,
        }
    }

    const SIMPLE: &[PropertySpec] = &[
        PropertySpec {
            name: "address",
            required: true,
            kind: PropertyValueKind::String,
        },
        PropertySpec {
            name: "port",
            required: false,
            kind: PropertyValueKind::Int,
        },
        PropertySpec {
            name: "framing",
            required: false,
            kind: PropertyValueKind::Enum(&["octet_counting", "non_transparent"]),
        },
    ];

    #[test]
    fn accepts_minimal_valid_config() {
        let props = vec![kv("address", ExprKind::StringLit("0.0.0.0:514".into()))];
        let errs = validate(&props, SIMPLE);
        assert!(errs.is_empty(), "unexpected errors: {:?}", errs);
    }

    #[test]
    fn flags_unknown_key_with_did_you_mean() {
        let props = vec![
            kv("address", ExprKind::StringLit("x".into())),
            kv("portt", ExprKind::IntLit(514)),
        ];
        let errs = validate(&props, SIMPLE);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::UnknownKey));
        assert_eq!(errs[0].did_you_mean.as_deref(), Some("port"));
    }

    #[test]
    fn flags_missing_required() {
        let errs = validate(&[], SIMPLE);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::MissingRequired));
        assert_eq!(errs[0].key, "address");
    }

    #[test]
    fn flags_type_mismatch() {
        let props = vec![
            kv("address", ExprKind::StringLit("x".into())),
            kv("port", ExprKind::StringLit("eighty".into())),
        ];
        let errs = validate(&props, SIMPLE);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0].kind,
            SchemaErrorKind::TypeMismatch { expected: "an integer" }
        ));
    }

    #[test]
    fn flags_unknown_enum_with_suggestion() {
        let props = vec![
            kv("address", ExprKind::StringLit("x".into())),
            kv("framing", ExprKind::Ident(vec!["non_trasnaprent".into()])),
        ];
        let errs = validate(&props, SIMPLE);
        assert_eq!(errs.len(), 1);
        match &errs[0].kind {
            SchemaErrorKind::UnknownEnumValue { allowed } => {
                assert_eq!(*allowed, &["octet_counting", "non_transparent"]);
            }
            other => panic!("expected UnknownEnumValue, got {:?}", other),
        }
        assert_eq!(errs[0].did_you_mean.as_deref(), Some("non_transparent"));
    }

    #[test]
    fn accepts_correct_enum_value() {
        let props = vec![
            kv("address", ExprKind::StringLit("x".into())),
            kv("framing", ExprKind::Ident(vec!["non_transparent".into()])),
        ];
        let errs = validate(&props, SIMPLE);
        assert!(errs.is_empty(), "unexpected: {:?}", errs);
    }

    #[test]
    fn flags_duplicate_key() {
        let props = vec![
            kv("address", ExprKind::StringLit("a".into())),
            kv("address", ExprKind::StringLit("b".into())),
        ];
        let errs = validate(&props, SIMPLE);
        assert!(errs.iter().any(|e| matches!(e.kind, SchemaErrorKind::DuplicateKey)));
    }

    #[test]
    fn block_expected_but_scalar_given() {
        const NESTED: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            kind: PropertyValueKind::Block(&[PropertySpec {
                name: "cert",
                required: true,
                kind: PropertyValueKind::String,
            }]),
        }];
        let props = vec![kv("tls", ExprKind::StringLit("not-a-block".into()))];
        let errs = validate(&props, NESTED);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::ExpectedBlock));
    }

    #[test]
    fn block_recurses_into_inner_schema() {
        const NESTED: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            kind: PropertyValueKind::Block(&[
                PropertySpec {
                    name: "cert",
                    required: true,
                    kind: PropertyValueKind::String,
                },
                PropertySpec {
                    name: "key",
                    required: false,
                    kind: PropertyValueKind::String,
                },
            ]),
        }];
        // Missing required `cert` inside the block.
        let props = vec![block(
            "tls",
            vec![kv("key", ExprKind::StringLit("/k.pem".into()))],
        )];
        let errs = validate(&props, NESTED);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::MissingRequired));
        assert_eq!(errs[0].key, "cert");
    }

    #[test]
    fn duration_and_size_kinds_accept_valid_strings() {
        const S: &[PropertySpec] = &[
            PropertySpec {
                name: "interval",
                required: false,
                kind: PropertyValueKind::Duration,
            },
            PropertySpec {
                name: "max_size",
                required: false,
                kind: PropertyValueKind::Size,
            },
        ];
        let props = vec![
            kv("interval", ExprKind::StringLit("5s".into())),
            kv("max_size", ExprKind::StringLit("100MB".into())),
        ];
        assert!(validate(&props, S).is_empty());
    }

    #[test]
    fn duration_rejects_unparseable_string() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "interval",
            required: false,
            kind: PropertyValueKind::Duration,
        }];
        let props = vec![kv("interval", ExprKind::StringLit("yesterday".into()))];
        let errs = validate(&props, S);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::TypeMismatch { .. }));
    }

    #[test]
    fn bool_kind_accepts_both_boollit_and_ident_form() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "verify",
            required: false,
            kind: PropertyValueKind::Bool,
        }];
        assert!(validate(&[kv("verify", ExprKind::BoolLit(false))], S).is_empty());
        assert!(validate(&[kv("verify", ExprKind::Ident(vec!["false".into()]))], S).is_empty());
        let errs = validate(&[kv("verify", ExprKind::IntLit(0))], S);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::TypeMismatch { .. }));
    }

    #[test]
    fn collect_all_errors_in_one_pass() {
        let props = vec![
            // Missing required `address`,
            // unknown key `bindd`,
            // wrong type for `port`,
            // unknown enum for `framing`.
            kv("bindd", ExprKind::StringLit("x".into())),
            kv("port", ExprKind::StringLit("x".into())),
            kv("framing", ExprKind::Ident(vec!["xx".into()])),
        ];
        let errs = validate(&props, SIMPLE);
        assert_eq!(errs.len(), 4);
    }

    #[test]
    fn string_map_accepts_any_key_with_string_value() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "headers",
            required: false,
            kind: PropertyValueKind::StringMap,
        }];
        let props = vec![block(
            "headers",
            vec![
                kv("Authorization", ExprKind::StringLit("Bearer xxx".into())),
                kv("X-Tenant", ExprKind::Ident(vec!["acme".into()])),
            ],
        )];
        assert!(validate(&props, S).is_empty());
    }

    #[test]
    fn string_map_rejects_non_string_value() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "headers",
            required: false,
            kind: PropertyValueKind::StringMap,
        }];
        let props = vec![block(
            "headers",
            vec![kv("Retry-After", ExprKind::BoolLit(true))],
        )];
        let errs = validate(&props, S);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::TypeMismatch { .. }));
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("foo", "foo"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
    }
}
