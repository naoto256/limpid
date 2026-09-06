//! Declarative property schema for limpid configuration surfaces.
//!
//! Each Module (and the top-level `control` / `table` blocks) advertises
//! its acceptable property shape as a `&'static [PropertySpec]`. The
//! same static slice is consumed by:
//!
//! * `--check` (the analyzer) — to surface unknown keys, missing
//!   required ones, type-mismatched values, and out-of-set enum values
//!   as loud diagnostics *before* the daemon is started; and
//! * runtime (the module registry's build path, before each
//!   `Module::from_properties` factory runs) — to fail fast with the
//!   same wording when a config slips past `--check` (e.g. someone
//!   runs the daemon directly).
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
    /// `100ms`). Unlike the plain `String` kind, Duration accepts only
    /// `StringLit` — `Template` and bare `Ident` are rejected, since
    /// the parser needs a fixed literal it can hand to the duration
    /// parser at analyzer time. Parse failures are flagged the same way.
    Duration,
    /// String literal parseable by `props::parse_size` (`1GB`, `512MB`).
    /// Same `StringLit`-only restriction as [`Duration`].
    Size,
    /// Bare ident whose value must be one of the listed strings.
    /// `framing { octet_counting | non_transparent }`, `queue.type
    /// { memory | disk }`, etc.
    Enum(&'static [&'static str]),
    /// Nested block. The slice describes the inner schema; the
    /// validator recurses.
    Block(&'static [PropertySpec]),
    /// Open block whose keys are user-defined identifiers and whose
    /// values must each be a nested block conforming to the given schema.
    /// `tls { profile_a { ca "..." } profile_b { ca "..." cert "..." key "..." } }`
    BlockMap(&'static [PropertySpec]),
    /// Open block whose keys are identifiers or static quoted strings (HTTP
    /// header names, k8s-style labels, etc.) and whose values must
    /// each be string-shaped. The schema validator never flags an
    /// "unknown key" inside this block — it only checks that every
    /// entry is a key-value (not a nested sub-block) with a
    /// string-shaped value.
    StringMap,
    /// Value can be one of multiple shapes. Used for keys that accept
    /// either an inline block or a reference identifier.
    OneOf(&'static [PropertyValueKind]),
}

impl PropertyValueKind {
    /// Whether this variant's outer property shape is `Property::Block`
    /// (block-shaped) versus `Property::KeyValue` (scalar-shaped).
    /// Used by `check_one_of` to decide which `OneOf` variants are
    /// *structural* matches for the actual property — independent of
    /// whatever inner content errors the variant's full validator
    /// produced.
    fn expects_block_outer_shape(self) -> bool {
        matches!(
            self,
            PropertyValueKind::Block(_)
                | PropertyValueKind::BlockMap(_)
                | PropertyValueKind::StringMap
        )
    }

    fn label(self) -> &'static str {
        match self {
            PropertyValueKind::String => "String",
            PropertyValueKind::Int => "Int",
            PropertyValueKind::Bool => "Bool",
            PropertyValueKind::Duration => "Duration",
            PropertyValueKind::Size => "Size",
            PropertyValueKind::Enum(_) => "Ident",
            PropertyValueKind::Block(_) => "Block",
            PropertyValueKind::BlockMap(_) => "BlockMap",
            PropertyValueKind::StringMap => "StringMap",
            PropertyValueKind::OneOf(_) => "OneOf",
        }
    }
}

impl Property {
    fn label(&self) -> &'static str {
        match self {
            Property::KeyValue { value, .. } => value.label(),
            Property::Block { .. } => "Block",
        }
    }
}

impl Expr {
    fn label(&self) -> &'static str {
        match self.kind {
            ExprKind::StringLit(_) => "String",
            ExprKind::Template(_) => "Template",
            ExprKind::IntLit(_) => "Int",
            ExprKind::FloatLit(_) => "Float",
            ExprKind::BoolLit(_) => "Bool",
            ExprKind::Null => "Null",
            ExprKind::Ident(_) => "Ident",
            ExprKind::FuncCall { .. } => "FuncCall",
            ExprKind::BinOp(_, _, _) => "BinOp",
            ExprKind::UnaryOp(_, _) => "UnaryOp",
            ExprKind::HashLit(_) => "Hash",
            ExprKind::ArrayLit(_) => "Array",
            ExprKind::PropertyAccess(_, _) => "PropertyAccess",
            ExprKind::SwitchExpr { .. } => "Switch",
        }
    }
}

/// One declared key in a property surface.
#[derive(Debug, Clone, Copy)]
pub struct PropertySpec {
    pub name: &'static str,
    pub required: bool,
    pub repeatable: bool,
    pub exclusive_group: Option<&'static str>,
    pub kind: PropertyValueKind,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// What went wrong with a single property — what the schema expected
/// versus what the parsed config carries.
#[derive(Debug, Clone)]
pub enum SchemaErrorKind {
    /// Quoted keys belong only to arbitrary-key StringMap entries.
    QuotedKeyOutsideStringMap,
    /// Property key isn't declared in the schema.
    UnknownKey,
    /// Schema marks `required: true` and the key is absent.
    MissingRequired,
    /// Same key appears twice in the same surface. Later occurrence
    /// silently won in legacy code — surface it explicitly.
    DuplicateKey,
    /// Multiple mutually exclusive properties from the same group
    /// appeared in the same surface.
    ExclusiveGroupViolation {
        group: &'static str,
        conflicting: Vec<String>,
    },
    /// Schema declares this key as a `Block(_)` but the config wrote a
    /// scalar (`tls "..."`).
    ExpectedBlock,
    /// Schema declares this key as a scalar/enum but the config wrote
    /// a block (`address { ... }`).
    ExpectedValue,
    /// Scalar value doesn't match the declared kind (e.g. an `Int`
    /// expected but a string given; a `Duration` that failed to parse).
    TypeMismatch { expected: &'static str },
    /// Value did not match any variant declared by `OneOf`.
    OneOfMismatch {
        expected: Vec<&'static str>,
        actual: &'static str,
    },
    /// Bare ident value doesn't match any of the allowed enum variants.
    UnknownEnumValue { allowed: &'static [&'static str] },
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
            QuotedKeyOutsideStringMap => self.key_span.or(self.value_span),
            UnknownKey
            | MissingRequired
            | DuplicateKey
            | ExclusiveGroupViolation { .. }
            | ExpectedBlock
            | ExpectedValue => self.key_span.or(self.value_span),
            TypeMismatch { .. } | OneOfMismatch { .. } | UnknownEnumValue { .. } => {
                self.value_span.or(self.key_span)
            }
        }
    }
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use SchemaErrorKind::*;
        match &self.kind {
            QuotedKeyOutsideStringMap => write!(
                f,
                "quoted property keys are only allowed inside a StringMap"
            ),
            UnknownKey => {
                write!(f, "unknown property '{}'", self.key)?;
                if let Some(s) = &self.did_you_mean {
                    write!(f, " (did you mean '{}'?)", s)?;
                }
                Ok(())
            }
            MissingRequired => write!(f, "missing required property '{}'", self.key),
            DuplicateKey => write!(f, "duplicate property '{}'", self.key),
            ExclusiveGroupViolation { group, conflicting } => write!(
                f,
                "properties in exclusive group '{}' conflict: {}",
                group,
                conflicting.join(", ")
            ),
            ExpectedBlock => write!(f, "'{}' expects a block ({{ ... }})", self.key),
            ExpectedValue => write!(f, "'{}' expects a value, not a block", self.key),
            TypeMismatch { expected } => {
                write!(f, "'{}' expects {}", self.key, expected)
            }
            OneOfMismatch { expected, actual } => write!(
                f,
                "'{}' expects one of: {}, got {}",
                self.key,
                expected.join(" | "),
                actual
            ),
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
    let mut exclusive_groups: std::collections::HashMap<&'static str, Vec<String>> =
        std::collections::HashMap::new();

    for prop in props {
        if let Some(error) = quoted_key_error(prop) {
            errs.push(error);
            continue;
        }
        let (name, key_span) = match prop {
            Property::KeyValue { key, key_span, .. } => (key.as_str(), *key_span),
            Property::Block { key, key_span, .. } => (key.as_str(), *key_span),
        };

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

        if !seen.insert(name) && !s.repeatable {
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

        if let Some(group) = s.exclusive_group {
            exclusive_groups
                .entry(group)
                .or_default()
                .push(name.to_string());
        }

        errs.extend(check_property(name, s.kind, prop, key_span));
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

    for (group, mut conflicting) in exclusive_groups {
        conflicting.sort();
        conflicting.dedup();
        if conflicting.len() > 1 {
            errs.push(SchemaError {
                kind: SchemaErrorKind::ExclusiveGroupViolation { group, conflicting },
                key: group.to_string(),
                key_span: None,
                value_span: None,
                did_you_mean: None,
            });
        }
    }

    errs
}

fn check_property(
    key: &str,
    kind: PropertyValueKind,
    prop: &Property,
    key_span: Option<Span>,
) -> Vec<SchemaError> {
    match kind {
        PropertyValueKind::Block(inner_spec) => match prop {
            Property::Block { properties, .. } => validate(properties, inner_spec),
            Property::KeyValue { value_span, .. } => vec![SchemaError {
                kind: SchemaErrorKind::ExpectedBlock,
                key: key.to_string(),
                key_span,
                value_span: *value_span,
                did_you_mean: None,
            }],
        },
        PropertyValueKind::BlockMap(inner_spec) => match prop {
            Property::Block { properties, .. } => validate_block_map(properties, inner_spec),
            Property::KeyValue { value_span, .. } => vec![SchemaError {
                kind: SchemaErrorKind::ExpectedBlock,
                key: key.to_string(),
                key_span,
                value_span: *value_span,
                did_you_mean: None,
            }],
        },
        PropertyValueKind::StringMap => match prop {
            Property::Block { properties, .. } => validate_string_map(properties),
            Property::KeyValue { value_span, .. } => vec![SchemaError {
                kind: SchemaErrorKind::ExpectedBlock,
                key: key.to_string(),
                key_span,
                value_span: *value_span,
                did_you_mean: None,
            }],
        },
        PropertyValueKind::OneOf(variants) => check_one_of(key, variants, prop, key_span),
        scalar_kind => match prop {
            Property::Block { .. } => vec![SchemaError {
                kind: SchemaErrorKind::ExpectedValue,
                key: key.to_string(),
                key_span,
                value_span: None,
                did_you_mean: None,
            }],
            Property::KeyValue {
                value, value_span, ..
            } => check_value(key, scalar_kind, value, *value_span, key_span)
                .into_iter()
                .collect(),
        },
    }
}

fn check_one_of(
    key: &str,
    variants: &'static [PropertyValueKind],
    prop: &Property,
    key_span: Option<Span>,
) -> Vec<SchemaError> {
    debug_assert!(
        variants
            .iter()
            .all(|kind| !matches!(kind, PropertyValueKind::OneOf(_))),
        "nested PropertyValueKind::OneOf is not supported"
    );

    // Try every variant; cache the per-variant error set so we can
    // make a smarter pick than "first error wins".
    let per_variant: Vec<Vec<SchemaError>> = variants
        .iter()
        .map(|kind| check_property(key, *kind, prop, key_span))
        .collect();

    // Happy path — at least one variant accepted the property.
    if per_variant.iter().any(Vec::is_empty) {
        return Vec::new();
    }

    // None accepted, but a variant may have *structurally* matched
    // (the property's outer shape — Block vs. KeyValue — fit) and only
    // failed on inner content (e.g. an inline `tls { ... }` block
    // missing a required key inside). When exactly one variant matches
    // structurally, surface its inner errors instead of collapsing to
    // a generic `OneOfMismatch` — the latter says "expected Block |
    // Ident, got Block" which is actively misleading when the user
    // wrote a Block and the real problem is one missing inner key.
    //
    // Structural match is decided by comparing the variant's expected
    // outer shape (`Block` / `BlockMap` / `StringMap` → block-shaped;
    // everything else → scalar-shaped) against the actual `Property`
    // variant. Earlier this filter walked the per-variant error list
    // looking for `ExpectedBlock` / `ExpectedValue` kinds, which
    // misclassified variants that fit the outer shape but produced an
    // inner `ExpectedValue` (e.g. a nested scalar key whose value
    // shape was wrong) — those nested errors disqualified the variant
    // even though its outer shape matched.
    //
    // When 0 or 2+ variants structurally match we deliberately
    // collapse to `OneOfMismatch`:
    //   - 0: every variant rejected the outer shape, so listing all
    //     allowed shapes ("expected Block | Ident, got KeyValue") is
    //     more useful than picking one arbitrary structural-rejection
    //     error.
    //   - 2+: the outer shape fit multiple variants but each disagrees
    //     on the inner type. Picking one would hide the others (e.g.
    //     for `OneOf[String, Int]` given a Bool, surfacing only
    //     "expected String, got Bool" hides that Int was also OK).
    //     `OneOfMismatch { expected: ["String", "Int"], actual:
    //     "Bool" }` reads as "expected String | Int, got Bool" which
    //     names all valid forms in one line.
    let actual_is_block = matches!(prop, Property::Block { .. });
    let structural_matches: Vec<&Vec<SchemaError>> = variants
        .iter()
        .zip(per_variant.iter())
        .filter(|(kind, _)| kind.expects_block_outer_shape() == actual_is_block)
        .map(|(_, errs)| errs)
        .collect();

    if let [only] = structural_matches.as_slice() {
        return (*only).clone();
    }

    vec![SchemaError {
        kind: SchemaErrorKind::OneOfMismatch {
            expected: variants.iter().map(|kind| kind.label()).collect(),
            actual: prop.label(),
        },
        key: key.to_string(),
        key_span,
        value_span: prop_value_span(prop),
        did_you_mean: None,
    }]
}

/// Validate a `BlockMap`-kind block: every entry must be a nested block
/// whose contents satisfy the declared inner schema. The entry names are
/// user-defined, so there is no unknown-key check at this level.
fn validate_block_map(properties: &[Property], inner_spec: &[PropertySpec]) -> Vec<SchemaError> {
    let mut errs = Vec::new();
    for prop in properties {
        if let Some(error) = quoted_key_error(prop) {
            errs.push(error);
            continue;
        }
        match prop {
            Property::Block {
                properties: inner_properties,
                ..
            } => errs.extend(validate(inner_properties, inner_spec)),
            Property::KeyValue {
                key,
                key_span,
                value_span,
                ..
            } => errs.push(SchemaError {
                kind: SchemaErrorKind::ExpectedBlock,
                key: key.clone(),
                key_span: *key_span,
                value_span: *value_span,
                did_you_mean: None,
            }),
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
                ..
            } => match &value.kind {
                ExprKind::StringLit(_)
                | ExprKind::Template(_)
                | ExprKind::Ident(_)
                | ExprKind::IntLit(_) => {}
                _ => errs.push(SchemaError {
                    kind: SchemaErrorKind::TypeMismatch {
                        expected: "a string",
                    },
                    key: key.clone(),
                    key_span: *key_span,
                    value_span: *value_span,
                    did_you_mean: None,
                }),
            },
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

fn quoted_key_error(prop: &Property) -> Option<SchemaError> {
    let (key, key_span, quoted) = match prop {
        Property::KeyValue {
            key,
            key_span,
            key_quoted,
            ..
        }
        | Property::Block {
            key,
            key_span,
            key_quoted,
            ..
        } => (key, key_span, key_quoted),
    };
    quoted.then(|| SchemaError {
        kind: SchemaErrorKind::QuotedKeyOutsideStringMap,
        key: key.clone(),
        key_span: *key_span,
        value_span: prop_value_span(prop),
        did_you_mean: None,
    })
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
        PropertyValueKind::Block(_)
        | PropertyValueKind::BlockMap(_)
        | PropertyValueKind::StringMap
        | PropertyValueKind::OneOf(_) => {
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
///
/// Reused by `check::suggestions` (for unbound workspace / function
/// idents) and by `check::module_props` (for unknown `type` ident on
/// `def input/output`). Keeping a single implementation here means
/// the threshold and tie-break rules can't drift across surfaces.
pub fn nearest<'a, I>(needle: &str, candidates: I) -> Option<String>
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
            key_quoted: false,
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    fn block(key: &str, properties: Vec<Property>) -> Property {
        Property::Block {
            key: key.into(),
            key_quoted: false,
            key_span: None,
            properties,
        }
    }

    const SIMPLE: &[PropertySpec] = &[
        PropertySpec {
            name: "address",
            required: true,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::String,
        },
        PropertySpec {
            name: "port",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::Int,
        },
        PropertySpec {
            name: "framing",
            required: false,
            repeatable: false,
            exclusive_group: None,
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
            SchemaErrorKind::TypeMismatch {
                expected: "an integer"
            }
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
        assert!(
            errs.iter()
                .any(|e| matches!(e.kind, SchemaErrorKind::DuplicateKey))
        );
    }

    #[test]
    fn repeatable_block_accepts_duplicate_keys() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "peer",
            required: false,
            repeatable: true,
            exclusive_group: None,
            kind: PropertyValueKind::Block(&[PropertySpec {
                name: "host",
                required: true,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::String,
            }]),
        }];
        let props = vec![
            block("peer", vec![kv("host", ExprKind::StringLit("a".into()))]),
            block("peer", vec![kv("host", ExprKind::StringLit("b".into()))]),
        ];
        let errs = validate(&props, S);
        assert!(errs.is_empty(), "unexpected errors: {:?}", errs);
    }

    #[test]
    fn non_repeatable_block_rejects_duplicate_keys() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "peer",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::Block(&[PropertySpec {
                name: "host",
                required: true,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::String,
            }]),
        }];
        let props = vec![
            block("peer", vec![kv("host", ExprKind::StringLit("a".into()))]),
            block("peer", vec![kv("host", ExprKind::StringLit("b".into()))]),
        ];
        let errs = validate(&props, S);
        assert!(
            errs.iter()
                .any(|e| matches!(e.kind, SchemaErrorKind::DuplicateKey))
        );
    }

    #[test]
    fn block_expected_but_scalar_given() {
        const NESTED: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::Block(&[PropertySpec {
                name: "cert",
                required: true,
                repeatable: false,
                exclusive_group: None,
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
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::Block(&[
                PropertySpec {
                    name: "cert",
                    required: true,
                    repeatable: false,
                    exclusive_group: None,
                    kind: PropertyValueKind::String,
                },
                PropertySpec {
                    name: "key",
                    required: false,
                    repeatable: false,
                    exclusive_group: None,
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
    fn block_map_accepts_arbitrary_keys_and_validates_inner_schema() {
        const TLS_PROFILE: &[PropertySpec] = &[
            PropertySpec {
                name: "ca",
                required: true,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::String,
            },
            PropertySpec {
                name: "cert",
                required: false,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::String,
            },
        ];
        const S: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::BlockMap(TLS_PROFILE),
        }];

        let valid = vec![block(
            "tls",
            vec![block(
                "profile_a",
                vec![kv("ca", ExprKind::StringLit("ca.pem".into()))],
            )],
        )];
        assert!(validate(&valid, S).is_empty());

        let invalid = vec![block(
            "tls",
            vec![
                block(
                    "missing_ca",
                    vec![kv("cert", ExprKind::StringLit("c.pem".into()))],
                ),
                block(
                    "unknown_key",
                    vec![
                        kv("ca", ExprKind::StringLit("ca.pem".into())),
                        kv("bogus", ExprKind::StringLit("x".into())),
                    ],
                ),
            ],
        )];
        let errs = validate(&invalid, S);
        assert!(
            errs.iter()
                .any(|e| matches!(e.kind, SchemaErrorKind::MissingRequired))
        );
        assert!(
            errs.iter()
                .any(|e| matches!(e.kind, SchemaErrorKind::UnknownKey))
        );
    }

    #[test]
    fn block_map_rejects_scalar_entries() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::BlockMap(&[PropertySpec {
                name: "ca",
                required: true,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::String,
            }]),
        }];
        let props = vec![block(
            "tls",
            vec![kv("profile_a", ExprKind::StringLit("ca.pem".into()))],
        )];
        let errs = validate(&props, S);
        assert_eq!(errs.len(), 1);
        assert!(matches!(errs[0].kind, SchemaErrorKind::ExpectedBlock));
    }

    #[test]
    fn one_of_accepts_either_variant() {
        const TLS_INLINE: &[PropertySpec] = &[PropertySpec {
            name: "ca",
            required: true,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::String,
        }];
        const S: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::OneOf(&[
                PropertyValueKind::Block(TLS_INLINE),
                PropertyValueKind::Enum(&["profile_a"]),
            ]),
        }];

        assert!(
            validate(
                &[block(
                    "tls",
                    vec![kv("ca", ExprKind::StringLit("ca.pem".into()))]
                )],
                S
            )
            .is_empty()
        );
        assert!(validate(&[kv("tls", ExprKind::Ident(vec!["profile_a".into()]))], S).is_empty());
    }

    #[test]
    fn one_of_rejects_neither_variant() {
        // OneOf[Block, Ident] with a string value: the user's input
        // didn't match either outer shape (Block needs `{ ... }`,
        // Ident needs a bare word). Both branches failed *structurally*,
        // so the error collapses to `OneOfMismatch` which lists every
        // variant so the operator can pick a direction.
        const S: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::OneOf(&[
                PropertyValueKind::Block(&[]),
                PropertyValueKind::Enum(&["profile_a"]),
            ]),
        }];
        // Wait — the Ident branch *can* take a KeyValue (just with the
        // wrong value kind), so it actually matches structurally and
        // emits TypeMismatch. The collapse to OneOfMismatch only
        // happens when **multiple or zero** variants structurally
        // match. Here exactly one (Enum) does, so we surface its
        // specific error: "expects an identifier".
        let errs = validate(&[kv("tls", ExprKind::StringLit("profile_a".into()))], S);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(matches!(errs[0].kind, SchemaErrorKind::TypeMismatch { .. }));
        let rendered = errs[0].to_string();
        assert!(
            rendered.contains("identifier"),
            "expected the Enum branch's specific error, got: {rendered}"
        );
    }

    #[test]
    fn one_of_falls_back_to_mismatch_when_multiple_scalar_variants_structurally_match() {
        // OneOf[String, Int] given a Bool literal: both variants accept
        // the KeyValue outer shape and emit TypeMismatch on the inner
        // type. With 2 structural matches the fallback is intentional —
        // OneOfMismatch reads as "expected String | Int, got Bool"
        // which is more useful than surfacing only one variant's
        // TypeMismatch and hiding that the other was also allowed.
        // This guards the doc-comment claim in `check_one_of`.
        const S: &[PropertySpec] = &[PropertySpec {
            name: "x",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::OneOf(&[PropertyValueKind::String, PropertyValueKind::Int]),
        }];
        let errs = validate(&[kv("x", ExprKind::BoolLit(true))], S);
        assert_eq!(errs.len(), 1, "{errs:?}");
        match &errs[0].kind {
            SchemaErrorKind::OneOfMismatch { expected, actual } => {
                assert!(expected.contains(&"String"), "{expected:?}");
                assert!(expected.contains(&"Int"), "{expected:?}");
                assert!(actual.contains("Bool"), "{actual}");
            }
            other => panic!("expected OneOfMismatch, got {other:?}"),
        }
    }

    #[test]
    fn one_of_falls_back_to_mismatch_when_zero_variants_structurally_match() {
        // OneOf[Block, Block-with-other-shape] — two block variants,
        // user wrote a KeyValue. Both fail with ExpectedBlock; neither
        // structurally matches; the error collapses to OneOfMismatch.
        const S: &[PropertySpec] = &[PropertySpec {
            name: "x",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::OneOf(&[
                PropertyValueKind::Block(&[]),
                PropertyValueKind::BlockMap(&[]),
            ]),
        }];
        let errs = validate(&[kv("x", ExprKind::IntLit(5))], S);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0].kind,
            SchemaErrorKind::OneOfMismatch { .. }
        ));
    }

    #[test]
    fn one_of_surfaces_inner_block_error_when_block_variant_matches() {
        // OneOf[Block(inner_schema), Ident] where the user wrote a
        // Block but forgot a required inner key. The Block variant
        // matches structurally and reports MissingRequired; the Ident
        // variant fails with ExpectedValue (structural). Exactly one
        // structural match → return the Block variant's inner error
        // rather than a vague "expected Block | Ident, got Block".
        const INNER: &[PropertySpec] = &[PropertySpec {
            name: "cert",
            required: true,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::String,
        }];
        const S: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::OneOf(&[
                PropertyValueKind::Block(INNER),
                PropertyValueKind::Enum(&["profile_a"]),
            ]),
        }];
        // Inline block with `cert` missing.
        let errs = validate(
            &[Property::Block {
                key: "tls".into(),
                key_quoted: false,
                key_span: None,
                properties: vec![],
            }],
            S,
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(matches!(errs[0].kind, SchemaErrorKind::MissingRequired));
        assert_eq!(errs[0].key, "cert");
    }

    #[test]
    fn one_of_surfaces_inner_nested_value_error_when_block_variant_matches() {
        // Regression: the structural-match filter used to look at the
        // per-variant error list for `ExpectedBlock` / `ExpectedValue`
        // kinds. That misclassified variants whose outer shape DID
        // match but whose inner content produced an `ExpectedValue` —
        // e.g. an inline block where a scalar inner key was instead
        // written as a sub-block. The fix decides structural match
        // from the variant's expected outer shape vs. the actual
        // `Property` variant, so nested errors inside an otherwise-
        // matching block variant no longer disqualify it.
        const INNER: &[PropertySpec] = &[PropertySpec {
            name: "cert",
            required: true,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::String,
        }];
        const S: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::OneOf(&[
                PropertyValueKind::Block(INNER),
                PropertyValueKind::Enum(&["profile_a"]),
            ]),
        }];
        // Inline block where `cert` is written as a sub-block instead
        // of a String value. The Block variant should structurally
        // match and surface its inner `ExpectedValue` error rather
        // than collapse to a generic `OneOfMismatch`.
        let errs = validate(
            &[Property::Block {
                key: "tls".into(),
                key_quoted: false,
                key_span: None,
                properties: vec![Property::Block {
                    key: "cert".into(),
                    key_quoted: false,
                    key_span: None,
                    properties: vec![],
                }],
            }],
            S,
        );
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            matches!(errs[0].kind, SchemaErrorKind::ExpectedValue),
            "expected ExpectedValue, got {:?}",
            errs[0].kind
        );
        assert_eq!(errs[0].key, "cert");
    }

    #[test]
    fn exclusive_group_allows_one() {
        const S: &[PropertySpec] = &[
            PropertySpec {
                name: "peer",
                required: false,
                repeatable: false,
                exclusive_group: Some("destinations"),
                kind: PropertyValueKind::Block(&[]),
            },
            PropertySpec {
                name: "peers",
                required: false,
                repeatable: false,
                exclusive_group: Some("destinations"),
                kind: PropertyValueKind::Block(&[]),
            },
        ];
        assert!(validate(&[block("peer", vec![])], S).is_empty());
    }

    #[test]
    fn exclusive_group_rejects_multiple() {
        const S: &[PropertySpec] = &[
            PropertySpec {
                name: "peer",
                required: false,
                repeatable: false,
                exclusive_group: Some("destinations"),
                kind: PropertyValueKind::Block(&[]),
            },
            PropertySpec {
                name: "peers",
                required: false,
                repeatable: false,
                exclusive_group: Some("destinations"),
                kind: PropertyValueKind::Block(&[]),
            },
        ];
        let errs = validate(&[block("peer", vec![]), block("peers", vec![])], S);
        let err = errs
            .iter()
            .find(|e| matches!(e.kind, SchemaErrorKind::ExclusiveGroupViolation { .. }))
            .expect("expected exclusive group violation");
        let rendered = err.to_string();
        assert!(rendered.contains("peer"), "{rendered}");
        assert!(rendered.contains("peers"), "{rendered}");
    }

    #[test]
    fn duration_and_size_kinds_accept_valid_strings() {
        const S: &[PropertySpec] = &[
            PropertySpec {
                name: "interval",
                required: false,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::Duration,
            },
            PropertySpec {
                name: "max_size",
                required: false,
                repeatable: false,
                exclusive_group: None,
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
            repeatable: false,
            exclusive_group: None,
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
            repeatable: false,
            exclusive_group: None,
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
    fn quoted_keys_are_only_accepted_inside_string_maps() {
        const S: &[PropertySpec] = &[
            PropertySpec {
                name: "headers",
                required: false,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::StringMap,
            },
            PropertySpec {
                name: "peer",
                required: false,
                repeatable: false,
                exclusive_group: None,
                kind: PropertyValueKind::Block(&[]),
            },
        ];
        for source in [
            r#"def output logs { type http "headers" { Authorization "v" } }"#,
            r#"def output logs { type http peer { "url" "v" } }"#,
        ] {
            let config = crate::dsl::parser::parse_config(source).unwrap();
            let crate::dsl::ast::Definition::Output(output) = &config.definitions[0] else {
                panic!("output")
            };
            let errors = validate(output.properties.user_properties(), S);
            assert!(
                errors.iter().any(|e| e.to_string().contains("quoted")),
                "{errors:?}"
            );
        }
        assert!(crate::dsl::parser::parse_config(r#"def output logs { "type" http }"#).is_err());
    }

    #[test]
    fn quoted_string_map_empty_and_protocol_independent_keys() {
        const MAP: &[PropertySpec] = &[PropertySpec {
            name: "headers",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::StringMap,
        }];
        for source in [
            r#"def output logs { type http headers {} }"#,
            r#"def output logs { type http headers { "" "v" "not an HTTP key" "v" Authorization "v" } }"#,
        ] {
            let config = crate::dsl::parser::parse_config(source).unwrap();
            let crate::dsl::ast::Definition::Output(output) = &config.definitions[0] else {
                panic!("output")
            };
            assert!(validate(output.properties.user_properties(), MAP).is_empty());
        }
        const NAMED: &[PropertySpec] = &[PropertySpec {
            name: "tls",
            required: false,
            repeatable: false,
            exclusive_group: None,
            kind: PropertyValueKind::BlockMap(&[]),
        }];
        let config = crate::dsl::parser::parse_config(
            r#"def output logs { type http tls { "profile" {} } }"#,
        )
        .unwrap();
        let crate::dsl::ast::Definition::Output(output) = &config.definitions[0] else {
            panic!("output")
        };
        assert!(
            validate(output.properties.user_properties(), NAMED)
                .iter()
                .any(|e| matches!(e.kind, SchemaErrorKind::QuotedKeyOutsideStringMap))
        );
    }

    #[test]
    fn string_map_accepts_any_key_with_string_value() {
        const S: &[PropertySpec] = &[PropertySpec {
            name: "headers",
            required: false,
            repeatable: false,
            exclusive_group: None,
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
            repeatable: false,
            exclusive_group: None,
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
