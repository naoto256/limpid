//! Orchestrator for the `gen-snippet-inventory` subcommand.
//!
//! Walks every `.limpid` file under all four snippet kind directories
//! (`packaging/snippets/{parsers,composers,filters,functions}/`),
//! parses each header ([`crate::header`]), renders the four inventory
//! markdown blocks ([`crate::inventory`]), and substitutes them into
//! the corresponding BEGIN/END regions of
//! `packaging/snippets/README.md`.
//!
//! In `--check` mode the on-disk README is left untouched and the
//! command exits non-zero if regeneration would produce different
//! content. CI uses this mode to fail on drift; developers run
//! without `--check` to refresh the README.

use std::error::Error;
use std::fs;

use crate::header::{Severity, SnippetKind, lint, parse};
use crate::inventory::{
    marker_for, render_composers_table, render_filters_table, render_functions_table,
    render_parsers_table, replace_marked_region,
};
use crate::paths::{list_all_files, readme_path};

pub fn run(check: bool) -> Result<(), Box<dyn Error>> {
    let files = list_all_files()?;
    if files.is_empty() {
        return Err(
            "no `.limpid` files under packaging/snippets/{parsers,composers,filters,functions}/"
                .into(),
        );
    }

    // Parse + lint every snippet across all kinds; abort on any
    // error, since inventory output is only meaningful when every
    // header is schema-valid.
    let mut headers = Vec::with_capacity(files.len());
    let mut lint_errors: Vec<String> = Vec::new();
    for (_kind, file) in &files {
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

    let readme = readme_path();
    let current =
        fs::read_to_string(&readme).map_err(|e| format!("read {}: {e}", readme.display()))?;

    // Render each of the four blocks and substitute in sequence.
    let mut updated = current.clone();
    for kind in SnippetKind::all() {
        let block = match kind {
            SnippetKind::Parser => render_parsers_table(&headers),
            SnippetKind::Composer => render_composers_table(&headers),
            SnippetKind::Filter => render_filters_table(&headers),
            SnippetKind::Function => render_functions_table(&headers),
        };
        updated = replace_marked_region(&updated, marker_for(*kind), &block)?;
    }

    let counts = counts_by_kind(&headers);
    if check {
        if current == updated {
            println!("gen-snippet-inventory: README in sync ({counts})");
            Ok(())
        } else {
            Err("README is out of sync with snippet headers. \
                 Run `cargo xtask gen-snippet-inventory` (without `--check`) \
                 to refresh, then commit."
                .into())
        }
    } else if current == updated {
        println!("gen-snippet-inventory: already in sync ({counts})");
        Ok(())
    } else {
        fs::write(&readme, updated).map_err(|e| format!("write {}: {e}", readme.display()))?;
        println!(
            "gen-snippet-inventory: updated {} ({counts})",
            readme.display()
        );
        Ok(())
    }
}

fn counts_by_kind(headers: &[crate::header::SnippetHeader]) -> String {
    let mut p = 0usize;
    let mut c = 0usize;
    let mut f = 0usize;
    let mut fn_ = 0usize;
    for h in headers {
        match h.kind {
            SnippetKind::Parser => p += 1,
            SnippetKind::Composer => c += 1,
            SnippetKind::Filter => f += 1,
            SnippetKind::Function => fn_ += 1,
        }
    }
    format!("{p} parser(s), {c} composer(s), {f} filter(s), {fn_} function(s)")
}
