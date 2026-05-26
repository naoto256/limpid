//! Orchestrator for the `gen-snippet-inventory` subcommand.
//!
//! Walks `packaging/snippets/parsers/*.limpid`, parses each header
//! ([`crate::header`]), renders the parsers inventory markdown
//! block ([`crate::inventory`]), and substitutes it into the
//! `<!-- BEGIN: inventory:parsers --> … <!-- END: -->` region of
//! `packaging/snippets/README.md`.
//!
//! In `--check` mode the on-disk README is left untouched and the
//! command exits non-zero if regeneration would produce different
//! content. CI uses this mode to fail on drift; developers run
//! without `--check` to refresh the README.

use std::error::Error;
use std::fs;

use crate::header::{Severity, lint, parse};
use crate::inventory::{parsers_marker, render_parsers_table, replace_marked_region};
use crate::paths::{list_parser_files, parsers_dir, readme_path};

pub fn run(check: bool) -> Result<(), Box<dyn Error>> {
    let files = list_parser_files()?;
    if files.is_empty() {
        return Err(format!(
            "no `.limpid` files under {}",
            parsers_dir().display()
        )
        .into());
    }

    // Parse + lint every parser; abort with a clear error if any
    // header fails the schema, since the generator's output is
    // only meaningful when the headers are valid.
    let mut headers = Vec::with_capacity(files.len());
    let mut lint_errors: Vec<String> = Vec::new();
    for file in &files {
        let header = parse(file).map_err(|e| format!("{}: parse: {e}", file.display()))?;
        for finding in lint(&header) {
            if matches!(finding.severity, Severity::Error) {
                lint_errors.push(finding.to_string());
            }
        }
        headers.push(header);
    }
    if !lint_errors.is_empty() {
        return Err(format!(
            "{} header lint error(s); fix with `cargo xtask lint-snippet-headers`:\n  {}",
            lint_errors.len(),
            lint_errors.join("\n  ")
        )
        .into());
    }

    let (table, warnings) = render_parsers_table(&headers);
    for w in &warnings {
        eprintln!("gen-snippet-inventory: warning: {w}");
    }

    let readme = readme_path();
    let current = fs::read_to_string(&readme)
        .map_err(|e| format!("read {}: {e}", readme.display()))?;
    let updated = replace_marked_region(&current, parsers_marker(), &table)?;

    if check {
        if current == updated {
            println!("gen-snippet-inventory: README in sync ({} parser(s))", headers.len());
            Ok(())
        } else {
            Err(
                "README is out of sync with parser headers. \
                 Run `cargo xtask gen-snippet-inventory` (without `--check`) \
                 to refresh, then commit."
                    .into(),
            )
        }
    } else {
        if current == updated {
            println!(
                "gen-snippet-inventory: already in sync ({} parser(s))",
                headers.len()
            );
            return Ok(());
        }
        fs::write(&readme, updated).map_err(|e| format!("write {}: {e}", readme.display()))?;
        println!(
            "gen-snippet-inventory: updated {} ({} parser(s))",
            readme.display(),
            headers.len()
        );
        Ok(())
    }
}

