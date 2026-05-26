//! Parser-header schema (parse + lint).
//!
//! The schema is documented in
//! `packaging/snippets/README.md` § *Authoring conventions > Header
//! schema*. The required keys, in the order they must appear, are:
//!
//!   1. `Vendor:` — prose product name
//!   2. `Wire:` — prose wire format
//!   3. `Upstream:` — `ingress (raw wire)` or pipe-separated
//!      `parse_<name>` references
//!   4. `Intake:` — required iff `Upstream` is not `ingress (raw
//!      wire)`; multi-line `workspace.<vendor>.<key>` schema
//!   5. `Output:` — prose description of what the parser writes
//!   6. `Test corpus:` — `real|public|synthetic|spec-only` + parens
//!
//! Unknown `// <Key>:` lines are tolerated (warning, not error) so
//! parsers can carry vendor-specific context like `// FortiGate CEF
//! dialect quirks:`.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

/// Canonical key names, in the order they must appear.
const REQUIRED_KEYS: &[&str] = &["Vendor", "Wire", "Upstream", "Intake", "Output", "Test corpus"];

/// `Intake:` is required only when `Upstream:` is not `ingress (raw
/// wire)`. The lint applies this conditional check after parsing.
const INTAKE_KEY: &str = "Intake";
const UPSTREAM_KEY: &str = "Upstream";
const INGRESS_RAW: &str = "ingress (raw wire)";

const TEST_CORPUS_CATEGORIES: &[&str] = &["real", "public", "synthetic", "spec-only"];

/// One parsed parser header. The raw key/value bag plus the file
/// the header came from. Fields for inventory rendering
/// (`vendor`, `wire`) are surfaced as direct accessors; less
/// inventory-relevant ones stay in `keys`.
#[derive(Debug, Clone)]
pub struct ParserHeader {
    pub file: PathBuf,
    /// Insertion-ordered key → value. Multi-line values are joined
    /// with `\n`. Stored as `BTreeMap` only for stable iteration in
    /// tests; key order in source is tracked separately for the
    /// order-lint via `observed_order`.
    pub keys: BTreeMap<String, String>,
    /// Order keys appeared in the source file. Used for order-lint.
    pub observed_order: Vec<String>,
}

impl ParserHeader {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.keys.get(key).map(String::as_str)
    }

    pub fn upstream(&self) -> Option<&str> {
        self.get(UPSTREAM_KEY)
    }

    pub fn is_ingress_raw_wire(&self) -> bool {
        self.upstream()
            .map(|v| v.lines().next().unwrap_or("").trim() == INGRESS_RAW)
            .unwrap_or(false)
    }
}

/// One lint result. `Error` means lint failed; `Warning` is
/// informational (unknown keys, etc).
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

/// Parse the leading comment block of a `.limpid` file into a
/// [`ParserHeader`].
///
/// Stops at the first non-`//` line. Blank `//` lines do not
/// terminate the header (parsers often separate semantic blocks
/// with a blank `//` for readability before the body).
///
/// A header line has the form `// <Key>: <value>`. Continuation
/// lines (`//` followed by ≥2 spaces of indent and no `<Key>:`
/// pattern) extend the previous value. The continuation indent
/// width is not normalised here — values are stored verbatim
/// (joined with `\n`) and downstream consumers can re-indent.
pub fn parse(file: &Path) -> Result<ParserHeader, Box<dyn Error>> {
    let content = fs::read_to_string(file)?;
    parse_str(file, &content)
}

fn parse_str(file: &Path, content: &str) -> Result<ParserHeader, Box<dyn Error>> {
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
            // blank // — section divider. End of current key's
            // continuation run but the header continues.
            current_key = None;
            continue;
        }

        // Continuation: only counts if there's a current key.
        // Otherwise it's freeform comment text (sample lines, prose
        // intro, etc) which we ignore.
        if let Some(ref key) = current_key {
            let value = keys.get_mut(key).expect("current_key must exist");
            value.push('\n');
            value.push_str(after_slashes.trim_start_matches(' '));
        }
    }

    Ok(ParserHeader { file: file.to_path_buf(), keys, observed_order })
}

/// Match a single header line against the `<Key>: <value>` pattern.
/// `after_slashes` is the substring after `//`.
///
/// Recognised keys are the canonical six plus any other PascalCase
/// or "Words With Spaces" name (e.g. `Test corpus`, `Coverage
/// scope`, `FortiGate CEF dialect quirks`). Practically: anything
/// before the first `:` that starts at column 1 (after `//` + some
/// space) and is human-prose-looking.
///
/// To avoid mis-matching sample log lines (e.g. ``// `alice :
/// TTY=...` ``), we require that the candidate key:
///   - starts with an uppercase ASCII letter
///   - contains only ASCII letters, digits, spaces, and hyphens
///   - is followed by `:` then whitespace (or end-of-line)
///
/// In particular: no backticks, dots, slashes, or other code-shaped
/// characters in the key. Keys are human-prose headings.
fn parse_key_line(after_slashes: &str) -> Option<(String, &str)> {
    // Require at least one space after `//`. Multiple spaces are
    // tolerated so contributors using a heavier indent (`//   Key:`)
    // aren't silently routed to "continuation". Lines with no
    // leading space (`//Key:`) are not recognised — that pattern
    // indicates a stylistic outlier we don't want to encourage.
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
    // Must be followed by whitespace or EOL after the colon.
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    Some((head.to_string(), rest.trim_start()))
}

/// Lint a parsed header against the schema. Returns a list of
/// findings; an empty list means the header is clean.
pub fn lint(header: &ParserHeader) -> Vec<Finding> {
    let mut findings = Vec::new();

    // 1. Required keys present.
    for key in REQUIRED_KEYS {
        if *key == INTAKE_KEY {
            continue; // conditional; checked separately
        }
        if !header.keys.contains_key(*key) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!("missing required key `{key}:`"),
            });
        }
    }

    // 2. Intake conditional.
    let has_intake = header.keys.contains_key(INTAKE_KEY);
    let is_raw = header.is_ingress_raw_wire();
    match (has_intake, is_raw) {
        (false, false) => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "missing required key `{INTAKE_KEY}:` (required when `Upstream:` is not `{INGRESS_RAW}`)"
            ),
        }),
        (true, true) => findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "unexpected key `{INTAKE_KEY}:` for `Upstream: {INGRESS_RAW}` parser (intake is `ingress` directly)"
            ),
        }),
        _ => {}
    }

    // 3. Order: required keys (those that are present) must appear
    //    in the canonical order, with unknown keys interleaved
    //    anywhere.
    let canonical: Vec<&str> = REQUIRED_KEYS
        .iter()
        .filter(|k| header.keys.contains_key(**k))
        .copied()
        .collect();
    let observed_required: Vec<&str> = header
        .observed_order
        .iter()
        .filter(|k| REQUIRED_KEYS.contains(&k.as_str()))
        .map(String::as_str)
        .collect();
    if observed_required != canonical {
        findings.push(Finding {
            file: header.file.clone(),
            severity: Severity::Error,
            message: format!(
                "required keys appear out of order: observed {:?}, expected {:?}",
                observed_required, canonical
            ),
        });
    }

    // 4. Test corpus category-prefix check.
    if let Some(tc) = header.get("Test corpus") {
        let first_word = tc.split_whitespace().next().unwrap_or("");
        let trimmed = first_word.trim_end_matches([':', ',', ';']);
        if !TEST_CORPUS_CATEGORIES.contains(&trimmed) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Error,
                message: format!(
                    "`Test corpus:` value must start with one of {:?}, found {:?}",
                    TEST_CORPUS_CATEGORIES, trimmed
                ),
            });
        }
    }

    // 5. Unknown keys → warning (not error). Allow vendor-specific
    //    prose blocks like `FortiGate CEF dialect quirks:`.
    for key in &header.observed_order {
        if !REQUIRED_KEYS.contains(&key.as_str()) {
            findings.push(Finding {
                file: header.file.clone(),
                severity: Severity::Warning,
                message: format!("unknown key `{key}:` (tolerated; will not appear in inventory)"),
            });
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ParserHeader {
        parse_str(Path::new("test.limpid"), s).unwrap()
    }

    #[test]
    fn parses_canonical_six_keys() {
        let h = p("\
// OpenSSH parser
//
// Vendor:      OpenSSH
// Wire:        sshd body
// Upstream:    parse_syslog | parse_journald
// Intake:      workspace.openssh.body (required)
//              workspace.openssh.pid (optional)
// Output:      OCSF 3002 fields
// Test corpus: real (playground sshd)

def process foo {}
");
        assert_eq!(h.get("Vendor"), Some("OpenSSH"));
        assert_eq!(h.get("Wire"), Some("sshd body"));
        assert_eq!(h.upstream(), Some("parse_syslog | parse_journald"));
        assert!(h.keys.contains_key("Intake"));
        assert_eq!(
            h.keys.get("Intake").unwrap(),
            "workspace.openssh.body (required)\nworkspace.openssh.pid (optional)"
        );
        assert_eq!(h.get("Output"), Some("OCSF 3002 fields"));
        assert_eq!(h.get("Test corpus"), Some("real (playground sshd)"));
        assert!(lint(&h).is_empty(), "expected clean lint, got {:?}", lint(&h));
    }

    #[test]
    fn ingress_raw_wire_must_not_have_intake() {
        let h = p("\
// Vendor:      X
// Wire:        Y
// Upstream:    ingress (raw wire)
// Intake:      workspace.x.body
// Output:      Z
// Test corpus: synthetic (a)
");
        let findings = lint(&h);
        assert!(
            findings.iter().any(|f| f.message.contains("unexpected key `Intake:`")),
            "findings: {findings:?}"
        );
    }

    #[test]
    fn bridged_must_have_intake() {
        let h = p("\
// Vendor:      X
// Wire:        Y
// Upstream:    parse_syslog
// Output:      Z
// Test corpus: real (b)
");
        let findings = lint(&h);
        assert!(
            findings.iter().any(|f| f.message.contains("missing required key `Intake:`")),
            "findings: {findings:?}"
        );
    }

    #[test]
    fn order_violation_caught() {
        let h = p("\
// Wire:        Y
// Vendor:      X
// Upstream:    ingress (raw wire)
// Output:      Z
// Test corpus: real (c)
");
        let findings = lint(&h);
        assert!(
            findings.iter().any(|f| f.message.contains("out of order")),
            "findings: {findings:?}"
        );
    }

    #[test]
    fn test_corpus_category_required() {
        let h = p("\
// Vendor:      X
// Wire:        Y
// Upstream:    ingress (raw wire)
// Output:      Z
// Test corpus: live (some-corpus)
");
        let findings = lint(&h);
        assert!(
            findings
                .iter()
                .any(|f| f.message.contains("Test corpus") && f.message.contains("live")),
            "findings: {findings:?}"
        );
    }

    #[test]
    fn unknown_key_is_warning_not_error() {
        let h = p("\
// Vendor:      X
// Wire:        Y
// Upstream:    ingress (raw wire)
// FortiGate CEF dialect quirks: vendor-specific note
// Output:      Z
// Test corpus: real (d)
");
        let findings = lint(&h);
        let errors: Vec<_> = findings.iter().filter(|f| f.severity == Severity::Error).collect();
        let warnings: Vec<_> = findings.iter().filter(|f| f.severity == Severity::Warning).collect();
        assert!(errors.is_empty(), "should have no errors, got {errors:?}");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].message.contains("FortiGate CEF dialect quirks"));
    }

    #[test]
    fn header_stops_at_first_non_slash_line() {
        let h = p("\
// Vendor:      X
// Wire:        Y
def process foo {}
// Upstream:    ingress (raw wire)
");
        // Upstream is in code, not header
        assert!(h.upstream().is_none());
    }

    #[test]
    fn blank_slash_does_not_end_header() {
        let h = p("\
// Vendor:      X
//
// Wire:        Y
// Upstream:    ingress (raw wire)
// Output:      Z
// Test corpus: real (e)
");
        assert_eq!(h.get("Wire"), Some("Y"));
    }
}
