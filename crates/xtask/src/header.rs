//! Snippet-header schema (parse + lint) — file facade plus member contracts.
//!
//! The schema is documented in
//! `packaging/snippets/README.md` § *Authoring conventions > Header
//! schema*. File-level metadata declares `Facade`, `Category` (parser
//! only), and `Test corpus`. Every facade process/function has a
//! directly-adjacent member block carrying its authored contract.
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
//! Top-level definitions omitted from `Facade` are private implementation
//! details and do not need member blocks.

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

    /// Canonical file-level keys in source order.
    pub fn required_keys(&self) -> &'static [&'static str] {
        match self {
            Self::Parser => &["Facade", "Category", "Test corpus"],
            _ => &["Facade", "Test corpus"],
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemberKind {
    Process,
    Function,
}

impl MemberKind {
    fn keyword(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Function => "function",
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Process => "Process",
            Self::Function => "Function",
        }
    }

    fn required_keys(self) -> &'static [&'static str] {
        match self {
            Self::Process => &["Process", "Summary", "Reads", "Writes"],
            Self::Function => &["Function", "Summary", "Signature"],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FacadeMember {
    pub kind: MemberKind,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct MemberHeader {
    pub kind: MemberKind,
    pub name: String,
    pub keys: BTreeMap<String, String>,
    pub observed_order: Vec<String>,
    pub declaration: Option<FacadeMember>,
    pub line: usize,
}

impl MemberHeader {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(String::as_str)
    }
}

/// A parsed snippet header. Fields:
/// - `file`: source file path (for diagnostics + inventory rendering).
/// - `kind`: derived from the file's parent directory.
/// - `keys`: file-level key → value bag. Multi-line values
///   are joined with `\n`. Stored as `BTreeMap` only for stable
///   iteration in tests; the observed-in-source order lives
///   separately in `observed_order` so the order-lint stays honest.
/// - `observed_order`: keys in the order they appeared in the source.
/// - `facade`: parsed public process/function declarations.
/// - `members`: per-facade contract blocks found throughout the file.
/// - `content`: full file content, retained for inventory caller scans.
#[derive(Debug, Clone)]
pub struct SnippetHeader {
    pub file: PathBuf,
    pub kind: SnippetKind,
    pub keys: BTreeMap<String, String>,
    pub observed_order: Vec<String>,
    pub facade: Vec<FacadeMember>,
    pub members: Vec<MemberHeader>,
    pub content: String,
}

impl SnippetHeader {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(String::as_str)
    }

    pub fn member(&self, kind: MemberKind, name: &str) -> Option<&MemberHeader> {
        self.members
            .iter()
            .find(|member| member.kind == kind && member.name == name)
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
    let lines: Vec<&str> = content.lines().collect();
    let file_block_end = lines
        .iter()
        .position(|line| !line.starts_with("//"))
        .unwrap_or(lines.len());
    let (keys, observed_order) = parse_comment_keys(&lines[..file_block_end]);
    let facade = keys
        .get("Facade")
        .map(|value| parse_facade(value).0)
        .unwrap_or_default();

    let mut members = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        if !lines[index].starts_with("//") {
            index += 1;
            continue;
        }
        let start = index;
        while index < lines.len() && lines[index].starts_with("//") {
            index += 1;
        }
        let (member_keys, member_order) = parse_comment_keys(&lines[start..index]);
        let process_name = member_keys.get("Process").map(|name| name.trim());
        let function_name = member_keys.get("Function").map(|name| name.trim());
        let heading = match (process_name, function_name) {
            (Some(name), None) => Some((MemberKind::Process, name)),
            (None, Some(name)) => Some((MemberKind::Function, name)),
            (Some(name), Some(_)) => Some((MemberKind::Process, name)),
            (None, None) => None,
        };
        if let Some((member_kind, name)) = heading {
            members.push(MemberHeader {
                kind: member_kind,
                name: name.to_string(),
                keys: member_keys,
                observed_order: member_order,
                declaration: lines
                    .get(index)
                    .and_then(|line| parse_def_declaration(line)),
                line: start + 1,
            });
        }
    }

    Ok(SnippetHeader {
        file: file.to_path_buf(),
        kind,
        keys,
        observed_order,
        facade,
        members,
        content: content.to_string(),
    })
}

fn parse_comment_keys(lines: &[&str]) -> (BTreeMap<String, String>, Vec<String>) {
    let mut keys = BTreeMap::new();
    let mut observed_order = Vec::new();
    let mut current_key: Option<String> = None;

    for line in lines {
        let Some(after_slashes) = line.strip_prefix("//") else {
            break;
        };
        if let Some((key, value)) = parse_key_line(after_slashes) {
            keys.insert(key.clone(), value.to_string());
            observed_order.push(key.clone());
            current_key = Some(key);
            continue;
        }
        let trimmed = after_slashes.trim();
        if trimmed.is_empty() {
            current_key = None;
            continue;
        }
        if let Some(ref key) = current_key {
            let value = keys.get_mut(key).expect("current_key must exist");
            value.push('\n');
            value.push_str(after_slashes.trim_start_matches(' '));
        }
    }
    (keys, observed_order)
}

fn parse_facade(value: &str) -> (Vec<FacadeMember>, Vec<String>) {
    let mut members = Vec::new();
    let mut errors = Vec::new();
    for raw_entry in value.split(',') {
        let entry = raw_entry.trim();
        if entry.is_empty() {
            continue;
        }
        let parts: Vec<&str> = entry.split_whitespace().collect();
        let kind = match parts.first().copied() {
            Some("process") => Some(MemberKind::Process),
            Some("function") => Some(MemberKind::Function),
            _ => None,
        };
        if parts.len() != 2
            || kind.is_none()
            || !is_ascii_ident(parts.get(1).copied().unwrap_or(""))
        {
            errors.push(format!(
                "invalid `Facade:` entry {entry:?}; expected `process name` or `function name`"
            ));
            continue;
        }
        members.push(FacadeMember {
            kind: kind.expect("checked above"),
            name: parts[1].to_string(),
        });
    }
    (members, errors)
}

fn parse_def_declaration(line: &str) -> Option<FacadeMember> {
    let trimmed = line.trim_start();
    let (kind, rest) = if let Some(rest) = trimmed.strip_prefix("def process ") {
        (MemberKind::Process, rest)
    } else {
        (MemberKind::Function, trimmed.strip_prefix("def function ")?)
    };
    let name = rest
        .split(|character: char| character == '(' || character == '{' || character.is_whitespace())
        .next()
        .unwrap_or("");
    is_ascii_ident(name).then(|| FacadeMember {
        kind,
        name: name.to_string(),
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

/// Lint file-level facade metadata and every public member contract.
pub fn lint(header: &SnippetHeader) -> Vec<Finding> {
    let mut findings = Vec::new();
    let kind = header.kind;

    lint_required_keys(
        header,
        "file",
        &header.keys,
        &header.observed_order,
        kind.required_keys(),
        &mut findings,
    );

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

    // Test corpus stays file-level because all facade members share evidence.
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

    let (parsed_facade, facade_errors) = header.get("Facade").map(parse_facade).unwrap_or_default();
    for message in facade_errors {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message,
        });
    }
    let mut facade_counts: BTreeMap<FacadeMember, usize> = BTreeMap::new();
    for member in &parsed_facade {
        *facade_counts.entry(member.clone()).or_default() += 1;
    }
    for (member, count) in &facade_counts {
        if *count > 1 {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "duplicate `Facade:` entry `{} {}`",
                    member.kind.keyword(),
                    member.name
                ),
            });
        }
    }

    for facade_member in facade_counts.keys() {
        let matching: Vec<&MemberHeader> = header
            .members
            .iter()
            .filter(|member| member.kind == facade_member.kind && member.name == facade_member.name)
            .collect();
        match matching.as_slice() {
            [] => findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "`Facade:` lists `{} {}` but no matching per-member block was found",
                    facade_member.kind.keyword(),
                    facade_member.name
                ),
            }),
            [member] => findings.extend(lint_member(header, member)),
            _ => findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "multiple per-member blocks found for `{} {}`",
                    facade_member.kind.keyword(),
                    facade_member.name
                ),
            }),
        }
    }

    for member in &header.members {
        if !facade_counts.contains_key(&FacadeMember {
            kind: member.kind,
            name: member.name.clone(),
        }) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "orphan `{}: {}` block at line {} is not listed in `Facade:`",
                    member.kind.heading(),
                    member.name,
                    member.line
                ),
            });
        }
    }

    // Unknown file-level keys remain warnings so authored design prose that
    // looks like a key is visible without making migration brittle.
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

fn lint_required_keys(
    header: &SnippetHeader,
    context: &str,
    keys: &BTreeMap<String, String>,
    observed_order: &[String],
    required: &[&str],
    findings: &mut Vec<Finding>,
) {
    for key in required {
        if !keys.contains_key(*key) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!("missing required key `{key}:` for {context} header"),
            });
        }
    }
    let canonical: Vec<&str> = required
        .iter()
        .filter(|key| keys.contains_key(**key))
        .copied()
        .collect();
    let observed: Vec<&str> = observed_order
        .iter()
        .filter(|key| required.contains(&key.as_str()))
        .map(String::as_str)
        .collect();
    if observed != canonical {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "{context} required keys appear out of order: observed {observed:?}, expected {canonical:?}"
            ),
        });
    }
}

fn lint_member(header: &SnippetHeader, member: &MemberHeader) -> Vec<Finding> {
    let mut findings = Vec::new();
    let context = format!("{} `{}`", member.kind.keyword(), member.name);
    lint_required_keys(
        header,
        &context,
        &member.keys,
        &member.observed_order,
        member.kind.required_keys(),
        &mut findings,
    );

    match &member.declaration {
        Some(declaration) if declaration.kind == member.kind && declaration.name == member.name => {
        }
        Some(declaration) => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`{}: {}` block at line {} is followed by `def {} {}`",
                member.kind.heading(),
                member.name,
                member.line,
                declaration.kind.keyword(),
                declaration.name
            ),
        }),
        None => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`{}: {}` block at line {} must be immediately followed by its definition",
                member.kind.heading(),
                member.name,
                member.line
            ),
        }),
    }

    match member.kind {
        MemberKind::Process => {
            if let Some(reads) = member.get("Reads") {
                findings.extend(lint_io_grammar(header, "Reads", reads, &["ingress"]));
            }
            if let Some(writes) = member.get("Writes") {
                findings.extend(lint_io_grammar(
                    header,
                    "Writes",
                    writes,
                    &["ingress", "egress"],
                ));
            }
        }
        MemberKind::Function => {
            if let Some(signature) = member.get("Signature") {
                findings.extend(lint_signature(header, member, signature));
            }
        }
    }

    for key in &member.observed_order {
        if !member.kind.required_keys().contains(&key.as_str()) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Warning,
                message: format!(
                    "unknown key `{key}:` in {} `{}` block",
                    member.kind.keyword(),
                    member.name
                ),
            });
        }
    }
    findings
}

// ---------------------------------------------------------------------------
// Process Reads/Writes root grammar.
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

/// The 8 acceptable intake types on a dot-line. Kept in sync with the
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
    "Bytes",
];

/// Enforce reserved-value or one-or-more workspace-root contracts.
/// Each workspace root owns the following dot declarations until the next
/// root and must declare at least one field.
fn lint_io_grammar(
    header: &SnippetHeader,
    key: &str,
    value: &str,
    reserved: &[&str],
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let lines: Vec<&str> = value.lines().map(str::trim_start).collect();
    let first = lines.first().copied().unwrap_or("");
    let first_token = first.split_whitespace().next().unwrap_or("");

    if reserved.contains(&first_token) {
        for (index, line) in lines.iter().enumerate().skip(1) {
            if line.starts_with('.') || classify_workspace_root(line) {
                findings.push(Finding {
                    file: header.file.clone(),
                    severity: Severity::Error,
                    message: format!(
                        "`{key}:` starts with reserved `{first_token}`, so workspace roots and dot declarations are forbidden; found `{line}` on line {}",
                        index + 1
                    ),
                });
            }
        }
        return findings;
    }

    if !classify_workspace_root(first) {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`{key}:` first token must be one of {reserved:?} or `workspace.<ns>.*`; found: {first}"
            ),
        });
        return findings;
    }

    let mut current_root = first_token.to_string();
    let mut declarations = 0usize;
    for (index, line) in lines.iter().enumerate().skip(1) {
        if classify_workspace_root(line) {
            if declarations == 0 {
                findings.push(missing_root_declaration(header, key, &current_root));
            }
            current_root = line.split_whitespace().next().unwrap_or("").to_string();
            declarations = 0;
            continue;
        }
        if !line.starts_with('.') {
            continue;
        }
        match parse_dot_line(line) {
            Some(_) => declarations += 1,
            None => findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "`{key}:` dot-line {} does not match `.<IDENT> (required|optional, <TYPE>)`: {line}",
                    index + 1
                ),
            }),
        }
    }
    if declarations == 0 {
        findings.push(missing_root_declaration(header, key, &current_root));
    }
    findings
}

fn missing_root_declaration(header: &SnippetHeader, key: &str, root: &str) -> Finding {
    Finding {
        file: header.file.clone(),
        severity: Severity::Error,
        message: format!("`{key}:` workspace root `{root}` requires at least one dot declaration"),
    }
}

fn classify_workspace_root(line: &str) -> bool {
    let first_token = line.split_whitespace().next().unwrap_or("");
    if let Some(after_workspace) = first_token.strip_prefix("workspace.")
        && let Some(path) = after_workspace.strip_suffix(".*")
        && !path.is_empty()
        && path.split('.').all(is_ascii_ident)
    {
        return true;
    }
    false
}

/// Parse a `.<name> (required|optional, <Type>)` line into a
/// [`DotLineDecl`]. Returns None if the line doesn't match. Trailing
/// prose (e.g. `— sshd application body`) is permitted after the
/// closing paren.
///
/// `<name>` may be a single ident (`.body`) or a dot-separated path
/// (`.log_record.attributes`) that mirrors the structure of the
/// intake slot. Every segment must be an ascii ident.
fn parse_dot_line(line: &str) -> Option<DotLineDecl> {
    let s = line.strip_prefix('.')?;
    // Consume dot-separated identifier path: `<ident>(.<ident>)*`.
    let ident_end = s
        .char_indices()
        .find(|(_, c)| !c.is_ascii_alphanumeric() && *c != '_' && *c != '.')
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if ident_end == 0 {
        return None;
    }
    let name = &s[..ident_end];
    if !name.split('.').all(is_ascii_ident) {
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

/// Cross-check the authored typed `Signature:` against the adjacent
/// definition's name and arity. Parameter and return types remain authored
/// knowledge because the DSL declaration itself carries no type annotation.
fn lint_signature(header: &SnippetHeader, member: &MemberHeader, sig_value: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let entries: Vec<&str> = signature_entries(sig_value).collect();
    if entries.len() != 1 {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`Function: {}` must have exactly one `Signature:` entry",
                member.name
            ),
        });
        return findings;
    }

    let entry = entries[0];
    let Some((name, params)) = parse_signature(entry) else {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`Signature:` must have shape `name(Type1, Type2, ...) → ReturnType`; could not parse: {entry:?}"
            ),
        });
        return findings;
    };
    if name != member.name {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!("`Function: {}` has `Signature:` for `{name}`", member.name),
        });
        return findings;
    }
    let declarations: Vec<FunctionDecl> = find_def_functions(&header.content)
        .into_iter()
        .filter(|declaration| declaration.name == member.name)
        .collect();
    match declarations.as_slice() {
        [] => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!("no `def function {}(...)` declaration found", member.name),
        }),
        [declaration] if declaration.params.len() != params.len() => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "`Signature:` declares {} parameter type(s), but `def function {}` has {} parameter(s)",
                params.len(),
                member.name,
                declaration.params.len()
            ),
        }),
        [_] => {}
        _ => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!("duplicate `def function {}(...)` declarations", member.name),
        }),
    }

    findings
}

/// Split a `Signature:` value into one authored entry per line.
pub(crate) fn signature_entries(value: &str) -> impl Iterator<Item = &str> {
    value.lines().map(str::trim).filter(|line| !line.is_empty())
}

/// Parse the header value of a `Signature:` key into (name, parameter types).
///
/// Accepts shapes like `proto_num(String) → Int | Null` and
/// `foo(Object, String | Null) → Object`. Both parameter and return type
/// expressions are validated against the DSL value-type vocabulary.
pub fn parse_signature(value: &str) -> Option<(String, Vec<String>)> {
    let value = value.trim();
    let (head, return_type) = value.split_once('→')?;
    if return_type.contains('→') || !is_type_expr(return_type) {
        return None;
    }
    let head = head.trim();
    let paren_open = head.find('(')?;
    let name = head[..paren_open].trim().to_string();
    if !is_ascii_ident(&name) {
        return None;
    }
    let after = &head[paren_open + 1..];
    let paren_close = after.rfind(')')?;
    if !after[paren_close + 1..].trim().is_empty() {
        return None;
    }
    let params = parse_signature_params(&after[..paren_close])?;
    Some((name, params))
}

const SIGNATURE_TYPES: &[&str] = &[
    "Any",
    "String",
    "Int",
    "Float",
    "Bool",
    "Object",
    "Array",
    "Timestamp",
    "Bytes",
    "Null",
];

fn is_type_expr(value: &str) -> bool {
    let mut parts = value.split('|').map(str::trim).peekable();
    parts.peek().is_some() && parts.all(|part| SIGNATURE_TYPES.contains(&part))
}

fn parse_signature_params(value: &str) -> Option<Vec<String>> {
    if value.trim().is_empty() {
        return Some(Vec::new());
    }
    value
        .split(',')
        .map(str::trim)
        .map(|param| is_type_expr(param).then(|| param.to_string()))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct FunctionDecl {
    name: String,
    params: Vec<String>,
}

/// Scan all `def function <name>(<params>)` declarations once.
/// Pure comment lines are excluded; this intentionally remains a
/// lexical check rather than adding the DSL parser as an xtask dependency.
fn find_def_functions(content: &str) -> Vec<FunctionDecl> {
    let mut declarations = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        let Some(start) = trimmed.find("def function ") else {
            continue;
        };
        let declaration = &trimmed[start + "def function ".len()..];
        let Some(paren_open) = declaration.find('(') else {
            continue;
        };
        let name = declaration[..paren_open].trim();
        if !is_ascii_ident(name) {
            continue;
        }
        let after = &declaration[paren_open + 1..];
        let Some(paren_close) = after.find(')') else {
            continue;
        };
        let Some(params) = parse_params(&after[..paren_close]) else {
            continue;
        };
        declarations.push(FunctionDecl {
            name: name.to_string(),
            params,
        });
    }
    declarations
}

fn parse_params(value: &str) -> Option<Vec<String>> {
    if value.trim().is_empty() {
        return Some(Vec::new());
    }
    value
        .split(',')
        .map(str::trim)
        .map(|param| is_ascii_ident(param).then(|| param.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod facade_tests {
    use super::*;

    fn parser(source: &str) -> SnippetHeader {
        parse_str(
            Path::new("packaging/snippets/parsers/example.limpid"),
            SnippetKind::Parser,
            source,
        )
        .unwrap()
    }

    fn function(source: &str) -> SnippetHeader {
        parse_str(
            Path::new("packaging/snippets/functions/example.limpid"),
            SnippetKind::Function,
            source,
        )
        .unwrap()
    }

    fn clean_parser() -> &'static str {
        r#"// Facade: process parse_example, process example_to_otlp
// Category: Transport
// Test corpus: synthetic (unit fixtures)

// Process: parse_example
// Summary: parses an example event
// Reads: ingress (raw wire)
// Writes: workspace.lsis.parsed.*
//   .class_uid (required, Int)
def process parse_example { }

def function private_helper(value) { value }

// Process: example_to_otlp
// Summary: places example facts in OTLP fields
// Reads: workspace.lsis.parsed.*
//   .class_uid (required, Int)
//   workspace.example.*
//   .source (optional, String)
// Writes: workspace.lsis.shed.otlp.*
//   .body (required, Bytes)
def process example_to_otlp { }
"#
    }

    #[test]
    fn canonical_facade_and_private_definition_are_clean() {
        let header = parser(clean_parser());
        assert_eq!(header.facade.len(), 2);
        assert_eq!(header.members.len(), 2);
        assert!(lint(&header).is_empty(), "{:?}", lint(&header));
    }

    #[test]
    fn facade_continuation_is_comma_separated() {
        let source = clean_parser().replace(
            "process parse_example, process example_to_otlp",
            "process parse_example,\n//         process example_to_otlp",
        );
        let header = parser(&source);
        assert_eq!(header.facade.len(), 2);
        assert!(lint(&header).is_empty(), "{:?}", lint(&header));
    }

    #[test]
    fn malformed_and_duplicate_facade_entries_are_errors() {
        let source = clean_parser().replace(
            "process parse_example, process example_to_otlp",
            "process parse_example, process parse_example, parser nope",
        );
        let messages: Vec<String> = lint(&parser(&source))
            .into_iter()
            .map(|finding| finding.message)
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("duplicate `Facade:`"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("invalid `Facade:`"))
        );
    }

    #[test]
    fn missing_and_orphan_member_blocks_are_errors() {
        let missing = clean_parser().replace("// Process: example_to_otlp", "// Adapter contract");
        assert!(
            lint(&parser(&missing))
                .iter()
                .any(|finding| finding.message.contains("no matching per-member block"))
        );

        let orphan = clean_parser().replace(
            "process parse_example, process example_to_otlp",
            "process parse_example",
        );
        assert!(lint(&parser(&orphan)).iter().any(|finding| {
            finding
                .message
                .contains("orphan `Process: example_to_otlp`")
        }));
    }

    #[test]
    fn member_block_must_be_immediately_followed_by_matching_def() {
        let source = clean_parser().replace(
            "def process parse_example { }",
            "def process parse_other { }",
        );
        assert!(lint(&parser(&source)).iter().any(|finding| {
            finding
                .message
                .contains("followed by `def process parse_other`")
        }));
    }

    #[test]
    fn process_member_requires_canonical_keys() {
        let source = clean_parser().replace("// Summary: parses an example event\n", "");
        assert!(
            lint(&parser(&source))
                .iter()
                .any(|finding| finding.message.contains("missing required key `Summary:`"))
        );
    }

    #[test]
    fn multi_root_contract_validates_each_root() {
        let clean = parser(clean_parser());
        assert!(lint(&clean).is_empty(), "{:?}", lint(&clean));

        let source = clean_parser().replace(
            "//   workspace.example.*\n//   .source (optional, String)",
            "//   workspace.example.*",
        );
        assert!(lint(&parser(&source)).iter().any(|finding| {
            finding.message.contains("workspace.example.*")
                && finding
                    .message
                    .contains("requires at least one dot declaration")
        }));
    }

    #[test]
    fn reserved_io_shapes_reject_field_declarations() {
        let source = clean_parser().replace(
            "// Reads: ingress (raw wire)",
            "// Reads: ingress (raw wire)\n//   .body (required, String)",
        );
        assert!(lint(&parser(&source)).iter().any(|finding| {
            finding
                .message
                .contains("workspace roots and dot declarations are forbidden")
        }));
    }

    #[test]
    fn egress_write_is_valid_without_dot_declarations() {
        let source = clean_parser().replace(
            "// Writes: workspace.lsis.shed.otlp.*\n//   .body (required, Bytes)",
            "// Writes: egress (OTLP protobuf bytes)",
        );
        assert_ne!(
            source,
            clean_parser(),
            "egress fixture replacement must change the source"
        );
        assert!(
            lint(&parser(&source)).is_empty(),
            "{:?}",
            lint(&parser(&source))
        );
    }

    #[test]
    fn function_member_signature_matches_adjacent_definition() {
        let source = r#"// Facade: function convert
// Test corpus: unit (conversion table)

// Function: convert
// Summary: converts a value
// Signature: convert(String) → Int | Null
def function convert(value) { value }

def function private_helper(value) { value }
"#;
        let header = function(source);
        assert!(lint(&header).is_empty(), "{:?}", lint(&header));
    }

    #[test]
    fn function_signature_name_and_parameters_are_checked() {
        let source = r#"// Facade: function convert
// Test corpus: unit (conversion table)

// Function: convert
// Summary: converts a value
// Signature: other(Int, Int) → Int
def function convert(value) { value }
"#;
        assert!(
            lint(&function(source))
                .iter()
                .any(|finding| finding.message.contains("has `Signature:` for `other`"))
        );
    }

    #[test]
    fn file_metadata_order_category_and_corpus_are_checked() {
        let source = clean_parser().replace(
            "// Category: Transport\n// Test corpus: synthetic",
            "// Test corpus: unit\n// Category: Unknown\n// Note: synthetic",
        );
        let findings = lint(&parser(&source));
        assert!(
            findings
                .iter()
                .any(|finding| finding.message.contains("out of order"))
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.message.contains("allowed set"))
        );
        assert!(
            findings
                .iter()
                .any(|finding| finding.message.contains("must start"))
        );
    }

    #[test]
    fn parse_signature_variants() {
        assert_eq!(
            parse_signature("convert(String, Object | Null) → Object | Null"),
            Some((
                "convert".to_string(),
                vec!["String".to_string(), "Object | Null".to_string()]
            ))
        );
        assert_eq!(parse_signature("missing return"), None);
        assert_eq!(parse_signature("convert(value) → Object"), None);
    }
}
