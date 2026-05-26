//! `lint-snippet-headers` subcommand.
//!
//! Walks `packaging/snippets/parsers/**/*.limpid` (recursively),
//! parses each header, and runs the schema lint. Prints all
//! findings; exits non-zero if any are errors (warnings are
//! surfaced but tolerated).

use std::error::Error;

use crate::header::{Finding, Severity, lint, parse};
use crate::paths::{list_parser_files, parsers_dir};

pub fn run() -> Result<(), Box<dyn Error>> {
    let files = list_parser_files()?;
    if files.is_empty() {
        return Err(format!("no `.limpid` files found under {}", parsers_dir().display()).into());
    }

    let mut all: Vec<Finding> = Vec::new();
    for file in &files {
        let header = match parse(file) {
            Ok(h) => h,
            Err(e) => {
                all.push(Finding {
                    file: file.clone(),
                    severity: Severity::Error,
                    message: format!("failed to read/parse: {e}"),
                });
                continue;
            }
        };
        all.extend(lint(&header));
    }

    let n_files = files.len();
    let mut n_err = 0usize;
    let mut n_warn = 0usize;
    for f in &all {
        println!("{f}");
        match f.severity {
            Severity::Error => n_err += 1,
            Severity::Warning => n_warn += 1,
        }
    }
    println!(
        "\nlint-snippet-headers: {n_files} file(s), {n_err} error(s), {n_warn} warning(s)"
    );
    if n_err > 0 {
        return Err(format!("{n_err} lint error(s)").into());
    }
    Ok(())
}
