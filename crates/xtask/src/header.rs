//! Snippet-header schema (parse + lint) — 4 kinds, LSIS-era.
//!
//! The schema is documented in
//! `packaging/snippets/README.md` § *Authoring conventions > Header
//! schema*. Each snippet kind has its own canonical key set:
//!
//!   parser   (5 keys): Summary / Reads / Writes / Category / Test corpus
//!   composer (4 keys): Summary / Reads / Writes / Test corpus
//!   filter   (4 keys): Summary / Reads / Effect / Test corpus
//!   function (3 keys): Summary / Signature / Test corpus
//!
//! The kind is determined by the file's parent directory
//! (`packaging/snippets/{parsers,composers,filters,functions}/`), not
//! by any header field — the file layout is the dispatch mechanism.
//!
//! Governing principle: a header may contain only AUTHORED knowledge
//! — what the snippet author alone knows. Anything derivable from
//! other keys, other files, or the body is banned from the header and
//! surfaces via the xtask-generated inventory instead. This is why
//! `Used by:` for functions is derived by the inventory generator
//! rather than authored, and why `Category:` on composers/filters
//! (where it would duplicate Reads/Writes/Effect) is rejected.
//!
//! Unknown `// <Key>:` lines are tolerated with a warning — parsers
//! historically carry vendor-specific prose blocks like `// FortiGate
//! CEF dialect quirks:`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use crate::inventory::CATEGORIES;

// ---------------------------------------------------------------------------
// SnippetKind — dispatched from the file's parent directory.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnippetKind {
    Parser,
    Composer,
    Filter,
    Function,
}

impl SnippetKind {
    /// The four kinds in a stable order — used by inventory rendering
    /// and by `list_all_files`.
    pub fn all() -> &'static [Self] {
        &[Self::Parser, Self::Composer, Self::Filter, Self::Function]
    }

    /// Match the leaf-directory-name segment
    /// (`parsers` / `composers` / `filters` / `functions`).
    pub fn from_dir_name(name: &str) -> Option<Self> {
        match name {
            "parsers" => Some(Self::Parser),
            "composers" => Some(Self::Composer),
            "filters" => Some(Self::Filter),
            "functions" => Some(Self::Function),
            _ => None,
        }
    }

    /// Infer the kind from a snippet file path by walking upward and
    /// looking for a recognised leaf directory name. Returns `None` if
    /// the path is not under one of the four snippet kind directories.
    pub fn from_path(path: &Path) -> Option<Self> {
        for component in path.parent()?.components().rev() {
            if let Some(s) = component.as_os_str().to_str()
                && let Some(k) = Self::from_dir_name(s)
            {
                return Some(k);
            }
        }
        None
    }

    pub fn dir_name(&self) -> &'static str {
        match self {
            Self::Parser => "parsers",
            Self::Composer => "composers",
            Self::Filter => "filters",
            Self::Function => "functions",
        }
    }

    /// A short label for diagnostic messages ("parser" not "parsers").
    pub fn label(&self) -> &'static str {
        match self {
            Self::Parser => "parser",
            Self::Composer => "composer",
            Self::Filter => "filter",
            Self::Function => "function",
        }
    }

    /// Canonical required-key list in the order they must appear.
    ///
    /// Governing principle applied per kind:
    /// - `Summary` and `Test corpus` are universal (all kinds).
    /// - `Reads` is universal for stream kinds (parser/composer/filter).
    /// - `Writes` (translator) / `Effect` (filter) split the stream
    ///   kind by what happens to the payload.
    /// - `Category` is parser-only — the composer and filter would-be
    ///   category duplicates `Writes` / `Effect` / `Reads` and fails
    ///   the authored-knowledge test.
    /// - `Signature` is function-only.
    pub fn required_keys(&self) -> &'static [&'static str] {
        match self {
            Self::Parser => &["Summary", "Reads", "Writes", "Category", "Test corpus"],
            Self::Composer => &["Summary", "Reads", "Writes", "Test corpus"],
            Self::Filter => &["Summary", "Reads", "Effect", "Test corpus"],
            Self::Function => &["Summary", "Signature", "Test corpus"],
        }
    }

    /// True for kinds that flow events (have a `Reads:` key). The
    /// universal Reads-dot-line grammar applies to all of these.
    pub fn is_stream(&self) -> bool {
        matches!(self, Self::Parser | Self::Composer | Self::Filter)
    }

    /// Allowed first-word prefixes for the `Test corpus:` value.
    /// Stream kinds share the event-corpus vocabulary; functions are
    /// pure value transforms and get a separate `unit` vocabulary
    /// (verification source in parens).
    pub fn test_corpus_prefixes(&self) -> &'static [&'static str] {
        match self {
            Self::Function => &["unit"],
            _ => &["real", "public", "synthetic", "spec-only"],
        }
    }
}

// ---------------------------------------------------------------------------
// SnippetHeader — parsed key/value bag + kind + original content.
// ---------------------------------------------------------------------------

/// A parsed snippet header. Fields:
/// - `file`: source file path (for diagnostics + inventory rendering).
/// - `kind`: derived from the file's parent directory.
/// - `keys`: insertion-collapsed key → value bag. Multi-line values
///   are joined with `\n`. Stored as `BTreeMap` only for stable
///   iteration in tests; the observed-in-source order lives
///   separately in `observed_order` so the order-lint stays honest.
/// - `observed_order`: keys in the order they appeared in the source.
/// - `content`: full file content, kept for cross-checks that need to
///   see the body (specifically: `Signature:` vs `def function`
///   declaration for the Function kind).
#[derive(Debug, Clone)]
pub struct SnippetHeader {
    pub file: PathBuf,
    pub kind: SnippetKind,
    pub keys: BTreeMap<String, String>,
    pub observed_order: Vec<String>,
    pub content: String,
}

impl SnippetHeader {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(String::as_str)
    }
}

// ---------------------------------------------------------------------------
// Findings.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub file: PathBuf,
    pub severity: Severity,
    pub message: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{}: {}: {}", self.file.display(), level, self.message)
    }
}

// ---------------------------------------------------------------------------
// Public parse entry point.
// ---------------------------------------------------------------------------

/// Parse a snippet file into a [`SnippetHeader`]. The kind is
/// inferred from the file path; a file not under a recognised kind
/// directory is rejected.
pub fn parse(file: &Path) -> Result<SnippetHeader, Box<dyn Error>> {
    let content = fs::read_to_string(file)?;
    let kind = SnippetKind::from_path(file).ok_or_else(|| {
        format!(
            "{}: file is not under a recognised snippet kind directory \
             (packaging/snippets/{{parsers,composers,filters,functions}}/)",
            file.display()
        )
    })?;
    parse_str(file, kind, &content)
}

/// Inner parse — parameterised on kind so tests can synthesise
/// headers of any kind without touching the filesystem.
pub fn parse_str(
    file: &Path,
    kind: SnippetKind,
    content: &str,
) -> Result<SnippetHeader, Box<dyn Error>> {
    let mut keys: BTreeMap<String, String> = BTreeMap::new();
    let mut observed_order: Vec<String> = Vec::new();
    let mut current_key: Option<String> = None;

    for line in content.lines() {
        let Some(after_slashes) = line.strip_prefix("//") else {
            // first non-`//` line ends the header
            break;
        };

        // A `// <Key>: <value>` line: detect key pattern.
        if let Some((key, value)) = parse_key_line(after_slashes) {
            keys.insert(key.clone(), value.to_string());
            observed_order.push(key.clone());
            current_key = Some(key);
            continue;
        }

        // Otherwise it's either a blank `//` (= section divider,
        // skip — does not end the header) or a continuation line.
        let trimmed = after_slashes.trim();
        if trimmed.is_empty() {
            current_key = None;
            continue;
        }

        // Continuation: only counts if there's a current key.
        // Otherwise it's freeform prose (sample lines, intro).
        if let Some(ref key) = current_key {
            let value = keys.get_mut(key).expect("current_key must exist");
            value.push('\n');
            value.push_str(after_slashes.trim_start_matches(' '));
        }
    }

    Ok(SnippetHeader {
        file: file.to_path_buf(),
        kind,
        keys,
        observed_order,
        content: content.to_string(),
    })
}

/// Match a single header line against the `<Key>: <value>` pattern.
/// See the same-named function in the previous parser-only iteration
/// for the rationale on the accepted key shape (ASCII, prose-looking,
/// no code-shaped characters).
fn parse_key_line(after_slashes: &str) -> Option<(String, &str)> {
    let s = after_slashes.trim_start_matches(' ');
    if s.len() == after_slashes.len() {
        return None;
    }
    let (head, rest) = s.split_once(':')?;
    let head = head.trim();
    if head.is_empty() {
        return None;
    }
    let first = head.chars().next()?;
    if !first.is_ascii_uppercase() {
        return None;
    }
    if !head
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-')
    {
        return None;
    }
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some((head.to_string(), rest.trim_start()))
}

// ---------------------------------------------------------------------------
// Lint.
// ---------------------------------------------------------------------------

/// Lint a parsed header. Returns findings; empty means clean.
///
/// Rules (all enforced regardless of kind, dispatched per kind):
///   1. all required keys present, in canonical order
///   2. `Category:` is parser-only and its value is in the whitelist
///   3. `Test corpus:` prefix is in the kind's allowed set
///   4. `Reads:` dot-line grammar (universal for stream kinds)
///   5. `Signature:` cross-check against `def function <name>(params)`
///      in the same file (function only)
///   6. Summary present (falls out of rule 1)
///   7. Unknown keys → warning (not error), so vendor-specific prose
///      blocks are tolerated but visible.
pub fn lint(header: &SnippetHeader) -> Vec<Finding> {
    let mut findings = Vec::new();
    let kind = header.kind;

    // 1a. All required keys present.
    for key in kind.required_keys() {
        if !header.keys.contains_key(*key) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!("missing required key `{key}:` for {} header", kind.label()),
            });
        }
    }

    // 1b. Required keys appear in canonical order.
    let canonical: Vec<&str> = kind
        .required_keys()
        .iter()
        .filter(|k| header.keys.contains_key(**k))
        .copied()
        .collect();
    let observed_required: Vec<&str> = header
        .observed_order
        .iter()
        .filter(|k| kind.required_keys().contains(&k.as_str()))
        .map(String::as_str)
        .collect();
    if observed_required != canonical {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "required keys appear out of order: observed {observed_required:?}, expected {canonical:?}"
            ),
        });
    }

    // 2. Category: parser-only; value in whitelist.
    if let Some(cat) = header.get("Category") {
        if kind != SnippetKind::Parser {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "`Category:` is not permitted on {} headers — parser is the only kind \
                     with a classification axis not derivable from Reads/Writes/Effect",
                    kind.label()
                ),
            });
        } else {
            let trimmed = cat.trim();
            if !CATEGORIES.contains(&trimmed) {
                findings.push(Finding {
                    file: header.file.clone(),
                    severity: Severity::Error,
                    message: format!(
                        "`Category:` value {trimmed:?} is not in the allowed set. \
                         Allowed values (see `crates/xtask/src/inventory.rs::CATEGORIES`): {CATEGORIES:?}"
                    ),
                });
            }
        }
    }

    // 3. Test corpus prefix.
    if let Some(tc) = header.get("Test corpus") {
        let first_word = tc.split_whitespace().next().unwrap_or("");
        let trimmed = first_word.trim_end_matches([':', ',', ';']);
        let allowed = kind.test_corpus_prefixes();
        if !allowed.contains(&trimmed) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "`Test corpus:` value must start with one of {allowed:?} for {} kind, found {trimmed:?}",
                    kind.label()
                ),
            });
        }
    }

    // 4. Reads dot-line grammar (universal for stream kinds).
    if kind.is_stream()
        && let Some(reads) = header.get("Reads")
    {
        findings.extend(lint_reads_grammar(header, reads));
    }

    // 5. Signature cross-check (function only).
    if kind == SnippetKind::Function
        && let Some(sig) = header.get("Signature")
    {
        findings.extend(lint_signature(header, sig));
    }

    // 7. Unknown keys → warning.
    for key in &header.observed_order {
        if !kind.required_keys().contains(&key.as_str()) {
            // Category on a non-parser is already an error above; don't
            // double-report as an unknown-key warning too.
            if key == "Category" && kind != SnippetKind::Parser {
                continue;
            }
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Warning,
                message: format!("unknown key `{key}:` (tolerated; will not appear in inventory)"),
            });
        }
    }

    findings
}

// ---------------------------------------------------------------------------
// Rule 4: Reads dot-line grammar.
// ---------------------------------------------------------------------------

/// A parsed `.<name> (required|optional, <Type>)` intake row.
///
/// The lint only needs to know whether each dot-line parses; the
/// individual fields are exposed for tests and future consumers
/// (e.g. a stub generator that emits `use .name; require .name;`
/// prefaces for parser bodies from these declarations).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DotLineDecl {
    pub name: String,
    pub required: bool,
    pub ty: String,
}

/// The 7 acceptable intake types on a dot-line. Kept in sync with the
/// grammar documented in the packaging README's Authoring Conventions
/// section. String-form so we can report unknowns verbatim.
const INTAKE_TYPES: &[&str] = &[
    "String",
    "Int",
    "Float",
    "Bool",
    "Object",
    "Array",
    "Timestamp",
];

/// Enforce the universal `Reads:` grammar across all stream kinds.
///
/// - First line's first token is `ingress` → dot-lines FORBIDDEN.
/// - First line's first token matches `workspace.<ns>.*` → ≥1
///   dot-line REQUIRED; each dot-line matches
///   `^\.<ASCII_IDENT>\s+\((required|optional), <TYPE>\)`.
/// - Non-dot continuation lines are prose (permissive) and ignored.
fn lint_reads_grammar(header: &SnippetHeader, reads_value: &str) -> Vec<Finding> {
    let mut findings = Vec::new();

    let (first_line, rest) = match reads_value.split_once('\n') {
        Some((f, r)) => (f.trim(), Some(r)),
        None => (reads_value.trim(), None),
    };

    let shape = classify_reads_first_line(first_line);

    match shape {
        ReadsShape::Ingress => {
            // Any dot-shaped continuation is an error.
            if let Some(rest) = rest {
                for (idx, line) in rest.split('\n').enumerate() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with('.') {
                        findings.push(Finding {
                            file: header.file.clone(),
                            severity: Severity::Error,
                            message: format!(
                                "`Reads:` first token is `ingress`, so dot-line intake rows \
                                 are forbidden; found `{}` on continuation line {}",
                                trimmed,
                                idx + 2
                            ),
                        });
                    }
                }
            }
        }
        ReadsShape::WorkspaceNamespace => {
            // Require ≥1 dot line; each dot line must parse.
            let mut dot_lines_seen = 0usize;
            if let Some(rest) = rest {
                for (idx, line) in rest.split('\n').enumerate() {
                    let trimmed = line.trim_start();
                    if !trimmed.starts_with('.') {
                        continue; // prose line; permitted
                    }
                    match parse_dot_line(trimmed) {
                        Some(_) => dot_lines_seen += 1,
                        None => findings.push(Finding {
                            file: header.file.clone(),
                            severity: Severity::Error,
                            message: format!(
                                "`Reads:` dot-line at continuation line {} does not match the \
                                 required grammar `^\\.<IDENT>\\s+\\((required|optional), \
                                 <String|Int|Float|Bool|Object|Array|Timestamp>\\)`: {}",
                                idx + 2,
                                trimmed
                            ),
                        }),
                    }
                }
            }
            if dot_lines_seen == 0 {
                findings.push(Finding {
                    file: header.file.clone(),
                    severity: Severity::Error,
                    message: format!(
                        "`Reads:` first token is a workspace namespace (`{first_line}`), \
                         so at least one dot-line intake row is required (e.g. \
                         `.body (required, String) — sshd application body`)"
                    ),
                });
            }
        }
        ReadsShape::Unknown => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`Reads:` first token must be `ingress` (raw wire) or `workspace.<ns>.*` \
                 (bridge/reader); found: {first_line}"
            ),
        }),
    }

    findings
}

enum ReadsShape {
    Ingress,
    WorkspaceNamespace,
    Unknown,
}

fn classify_reads_first_line(first_line: &str) -> ReadsShape {
    let first_token = first_line.split_whitespace().next().unwrap_or("");
    if first_token == "ingress" {
        return ReadsShape::Ingress;
    }
    // workspace.<ident>.*
    if let Some(after_workspace) = first_token.strip_prefix("workspace.")
        && let Some(dot_star) = after_workspace.strip_suffix(".*")
        && is_ascii_ident(dot_star)
    {
        return ReadsShape::WorkspaceNamespace;
    }
    ReadsShape::Unknown
}

/// Parse a `.<name> (required|optional, <Type>)` line into a
/// [`DotLineDecl`]. Returns None if the line doesn't match. Trailing
/// prose (e.g. `— sshd application body`) is permitted after the
/// closing paren.
fn parse_dot_line(line: &str) -> Option<DotLineDecl> {
    let s = line.strip_prefix('.')?;
    // Consume identifier.
    let ident_end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_')
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if ident_end == 0 {
        return None;
    }
    let name = &s[..ident_end];
    if !is_ascii_ident(name) {
        return None;
    }
    // Consume required whitespace.
    let after_name = &s[ident_end..];
    let ws_end = after_name
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(after_name.len());
    if ws_end == 0 {
        return None;
    }
    let after_ws = &after_name[ws_end..];
    // Expect `(<req>, <ty>)`.
    let inside = after_ws.strip_prefix('(')?;
    let close = inside.find(')')?;
    let content = &inside[..close];
    let (req, ty) = content.split_once(',')?;
    let req = req.trim();
    let ty = ty.trim();
    if req != "required" && req != "optional" {
        return None;
    }
    if !INTAKE_TYPES.contains(&ty) {
        return None;
    }
    Some(DotLineDecl {
        name: name.to_string(),
        required: req == "required",
        ty: ty.to_string(),
    })
}

fn is_ascii_ident(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Rule 5: Signature cross-check.
// ---------------------------------------------------------------------------

/// Cross-check that the authored `Signature:` name + parameter list
/// matches a `def function <name>(<params>) { ... }` declaration in
/// the same file. The return contract stays authored (the DSL has no
/// return-type declaration to derive from), so we do not check it.
fn lint_signature(header: &SnippetHeader, sig_value: &str) -> Vec<Finding> {
    let mut findings = Vec::new();

    let Some((sig_name, sig_params)) = parse_signature(sig_value) else {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`Signature:` value must have shape `name(param1, param2, ...) → ReturnType`; \
                 could not parse: {sig_value:?}"
            ),
        });
        return findings;
    };

    match find_def_function_params(&header.content, &sig_name) {
        None => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`Signature:` names `{sig_name}` but no `def function {sig_name}(...)` \
                 declaration was found in the same file"
            ),
        }),
        Some(decl_params) => {
            if decl_params != sig_params {
                findings.push(Finding {
                    file: header.file.clone(),
                    severity: Severity::Error,
                    message: format!(
                        "`Signature:` parameter list {sig_params:?} does not match the \
                         `def function {sig_name}` declaration's parameters {decl_params:?}"
                    ),
                });
            }
        }
    }

    findings
}

/// Parse the header value of a `Signature:` key into (name, params).
///
/// Accepts shapes like `proto_num(name) → Int | null`,
/// `foo(a, b, c) → Object`. Whitespace within the paren list is
/// tolerated. The return part (everything from `→` on) is discarded.
pub fn parse_signature(value: &str) -> Option<(String, Vec<String>)> {
    let value = value.trim();
    // Split off the return part first (if any) — everything up to
    // (but not including) the `→` arrow. We only need the head.
    let head = value.split('→').next().unwrap_or(value).trim();
    let paren_open = head.find('(')?;
    let name = head[..paren_open].trim().to_string();
    if !is_ascii_ident(&name) {
        return None;
    }
    let after = &head[paren_open + 1..];
    let paren_close = after.rfind(')')?;
    let params_str = &after[..paren_close];
    let params: Vec<String> = params_str
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    Some((name, params))
}

/// Find `def function <name>(<params>)` in the file body and return
/// the parameter list. Skips lines that are pure header/body comments
/// (leading `//`) — the declaration lives in code, not comments.
fn find_def_function_params(content: &str, name: &str) -> Option<Vec<String>> {
    let needle = format!("def function {name}(");
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let Some(start) = trimmed.find(&needle) else {
            continue;
        };
        let after = &trimmed[start + needle.len()..];
        let close = after.find(')')?;
        let params_str = &after[..close];
        let params: Vec<String> = params_str
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        return Some(params);
    }
    None
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parser(s: &str) -> SnippetHeader {
        parse_str(
            Path::new("packaging/snippets/parsers/x.limpid"),
            SnippetKind::Parser,
            s,
        )
        .unwrap()
    }
    fn composer(s: &str) -> SnippetHeader {
        parse_str(
            Path::new("packaging/snippets/composers/x.limpid"),
            SnippetKind::Composer,
            s,
        )
        .unwrap()
    }
    fn filter(s: &str) -> SnippetHeader {
        parse_str(
            Path::new("packaging/snippets/filters/x.limpid"),
            SnippetKind::Filter,
            s,
        )
        .unwrap()
    }
    fn function(s: &str) -> SnippetHeader {
        parse_str(
            Path::new("packaging/snippets/functions/x.limpid"),
            SnippetKind::Function,
            s,
        )
        .unwrap()
    }

    // --- Kind dispatch from path ---
    #[test]
    fn kind_from_path_walks_upward() {
        assert_eq!(
            SnippetKind::from_path(Path::new(
                "/home/user/repo/packaging/snippets/parsers/parse_x.limpid"
            )),
            Some(SnippetKind::Parser)
        );
        assert_eq!(
            SnippetKind::from_path(Path::new(
                "/home/user/repo/packaging/snippets/functions/f.limpid"
            )),
            Some(SnippetKind::Function)
        );
        assert_eq!(
            SnippetKind::from_path(Path::new("/tmp/unknown/loc.limpid")),
            None
        );
    }

    // --- Rule 1: required keys + order ---
    #[test]
    fn parser_clean_canonical_header() {
        let h = parser(
            "\
// Summary:     OpenSSH events → LSIS Authentication
// Reads:       workspace.openssh.* (bridge from parse_syslog | parse_journald)
//                .body     (required, String)  — sshd body
//                .pid      (optional, String)  — pid
// Writes:      workspace.lsis.* — class_uid 3002 (Authentication)
// Category:    Endpoint / host audit (Unix)
// Test corpus: real (OpenSSH sample)

def process foo {}
",
        );
        assert!(lint(&h).is_empty(), "unexpected findings: {:?}", lint(&h));
    }

    #[test]
    fn parser_missing_summary_is_error() {
        let h = parser(
            "\
// Reads:       ingress (raw wire) — X CEF wire
// Writes:      workspace.lsis.* — 4001
// Category:    Network firewall / IPS
// Test corpus: real (x)
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("missing required key `Summary:`"))
        );
    }

    #[test]
    fn parser_order_violation_caught() {
        let h = parser(
            "\
// Reads:       ingress (raw wire) — X
// Summary:     X
// Writes:      workspace.lsis.* — 4001
// Category:    Network firewall / IPS
// Test corpus: real (x)
",
        );
        assert!(lint(&h).iter().any(|f| f.message.contains("out of order")));
    }

    // --- Rule 2: Category ---
    #[test]
    fn composer_with_category_is_error() {
        let h = composer(
            "\
// Summary:     OCSF renderer
// Reads:       workspace.lsis.* (LSIS)
//                .class_uid (required, Int) — dispatch discriminator
// Writes:      workspace.lsis.ocsf — OCSF 1.3.0 JSON string
// Category:    OCSF 1.3.0
// Test corpus: real (x)
",
        );
        let findings = lint(&h);
        assert!(
            findings.iter().any(|f| f
                .message
                .contains("`Category:` is not permitted on composer")),
            "findings: {findings:?}"
        );
    }

    #[test]
    fn parser_category_value_must_be_in_whitelist() {
        let h = parser(
            "\
// Summary:     X
// Reads:       ingress (raw wire) — X
// Writes:      workspace.lsis.* — 4001
// Category:    Made up category
// Test corpus: real (x)
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("not in the allowed set"))
        );
    }

    // --- Rule 3: Test corpus prefix ---
    #[test]
    fn function_needs_unit_prefix() {
        let h = function(
            "\
// Summary:     IANA proto lookup
// Signature:   proto_num(name) → Int | null
// Test corpus: real (some corpus)

def function proto_num(name) {}
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("must start with"))
        );
    }

    #[test]
    fn parser_cannot_use_unit_prefix() {
        let h = parser(
            "\
// Summary:     X
// Reads:       ingress (raw wire) — X
// Writes:      workspace.lsis.* — 4001
// Category:    Network firewall / IPS
// Test corpus: unit (some source)
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("must start with"))
        );
    }

    // --- Rule 4: Reads dot-line grammar (universal) ---
    #[test]
    fn parser_bridge_needs_dot_lines() {
        let h = parser(
            "\
// Summary:     X
// Reads:       workspace.openssh.* (bridge from parse_syslog)
// Writes:      workspace.lsis.* — 3002
// Category:    Endpoint / host audit (Unix)
// Test corpus: real (x)
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("at least one dot-line"))
        );
    }

    #[test]
    fn parser_ingress_forbids_dot_lines() {
        let h = parser(
            "\
// Summary:     X
// Reads:       ingress (raw wire) — X CEF
//                .body (required, String)
// Writes:      workspace.lsis.* — 4001
// Category:    Network firewall / IPS
// Test corpus: real (x)
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("dot-line intake rows are forbidden"))
        );
    }

    #[test]
    fn composer_reads_grammar_applies() {
        // Composer with the LSIS shape but zero dot lines → error.
        let h = composer(
            "\
// Summary:     OCSF renderer
// Reads:       workspace.lsis.* (LSIS)
// Writes:      workspace.lsis.ocsf — OCSF 1.3.0 JSON string
// Test corpus: real (x)
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("at least one dot-line"))
        );
    }

    #[test]
    fn filter_reads_grammar_applies() {
        let h = filter(
            "\
// Summary:     sshd journal noise drop
// Reads:       workspace.journald.* (from parse_journald)
//                .MESSAGE (required, String) — sshd body candidate
// Effect:      drop pam_unix(sshd:session): ... / else pass-through
// Test corpus: real (x)
",
        );
        assert!(lint(&h).is_empty(), "unexpected findings: {:?}", lint(&h));
    }

    #[test]
    fn dot_line_regex_accepts_leading_underscore() {
        // journald's `_PID` / `__REALTIME_TIMESTAMP` are real intake fields.
        let d = parse_dot_line("._PID (optional, String) — kernel-verified pid").unwrap();
        assert_eq!(d.name, "_PID");
        assert!(!d.required);
        assert_eq!(d.ty, "String");
        let d = parse_dot_line(".__REALTIME_TIMESTAMP (optional, Int) — µs").unwrap();
        assert_eq!(d.name, "__REALTIME_TIMESTAMP");
    }

    #[test]
    fn dot_line_regex_rejects_wrong_shape() {
        assert!(parse_dot_line(".body (mandatory, String)").is_none());
        assert!(parse_dot_line(".body (required, Blob)").is_none());
        assert!(parse_dot_line(".body [required, String]").is_none());
        assert!(parse_dot_line(".1body (required, String)").is_none());
    }

    #[test]
    fn dot_line_regex_tolerates_trailing_prose() {
        let d = parse_dot_line(".body (required, String)  — sshd body").unwrap();
        assert_eq!(d.name, "body");
        assert!(d.required);
    }

    #[test]
    fn reads_unknown_first_token_is_error() {
        let h = parser(
            "\
// Summary:     X
// Reads:       nothing sensible
// Writes:      workspace.lsis.* — 4001
// Category:    Network firewall / IPS
// Test corpus: real (x)
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("first token must be"))
        );
    }

    // --- Rule 5: Signature cross-check ---
    #[test]
    fn function_signature_matches_declaration() {
        let h = function(
            "\
// Summary:     IANA proto lookup
// Signature:   proto_num(name) → Int | null
// Test corpus: unit (IANA proto registry, RFC 5237)

def function proto_num(name) {
    switch lower(name) {
        \"tcp\" { 6 }
        default { null }
    }
}
",
        );
        assert!(lint(&h).is_empty(), "unexpected findings: {:?}", lint(&h));
    }

    #[test]
    fn function_signature_name_mismatch_is_error() {
        let h = function(
            "\
// Summary:     IANA proto lookup
// Signature:   proto_num(name) → Int | null
// Test corpus: unit (IANA proto registry)

def function protonum(name) {}
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("no `def function proto_num"))
        );
    }

    #[test]
    fn function_signature_param_mismatch_is_error() {
        let h = function(
            "\
// Summary:     Two-arg thing
// Signature:   foo(a, b) → Int
// Test corpus: unit (docs)

def function foo(a, b, c) {}
",
        );
        assert!(
            lint(&h)
                .iter()
                .any(|f| f.message.contains("parameter list"))
        );
    }

    // --- Rule 7: unknown key → warning ---
    #[test]
    fn unknown_key_is_warning_not_error() {
        let h = parser(
            "\
// Summary:     FortiGate CEF
// Reads:       ingress (raw wire) — FortiGate CEF
// Writes:      workspace.lsis.* — 4001
// Category:    Network firewall / IPS
// FortiGate CEF dialect quirks: some prose
// Test corpus: real (fortigate)
",
        );
        let f = lint(&h);
        let errors: Vec<_> = f.iter().filter(|x| x.severity == Severity::Error).collect();
        let warnings: Vec<_> = f
            .iter()
            .filter(|x| x.severity == Severity::Warning)
            .collect();
        assert!(errors.is_empty(), "errors: {errors:?}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("FortiGate CEF dialect quirks"));
    }

    // --- Signature parser corner cases ---
    #[test]
    fn parse_signature_variants() {
        assert_eq!(
            parse_signature("proto_num(name) → Int | null"),
            Some(("proto_num".to_string(), vec!["name".to_string()]))
        );
        assert_eq!(
            parse_signature("foo(a, b, c) → Object"),
            Some((
                "foo".to_string(),
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            ))
        );
        assert_eq!(
            parse_signature("nullary() → Bool"),
            Some(("nullary".to_string(), vec![]))
        );
        assert_eq!(parse_signature("no parens"), None);
    }
}
