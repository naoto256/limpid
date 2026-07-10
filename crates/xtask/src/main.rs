//! xtask — dev workflow tasks for the limpid workspace.
//!
//! Run via the `cargo xtask` alias defined in `.cargo/config.toml`,
//! which maps to `cargo run --package xtask --`.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

mod gen_snippet_inventory;
mod header;
mod inventory;
mod lint_snippet_headers;
mod paths;

#[derive(Parser)]
#[command(name = "xtask", about = "Dev tasks for the limpid workspace")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Regenerate the snippet inventory blocks in
    /// `packaging/snippets/README.md` from parser headers.
    ///
    /// In `--check` mode, exits non-zero if the on-disk README does
    /// not match what regeneration would produce. CI uses this mode
    /// to fail on drift; developers run without `--check` to update.
    GenSnippetInventory {
        /// Verify only; do not write. Exits 1 on drift.
        #[arg(long)]
        check: bool,
    },

    /// Lint parser-header schema across
    /// `packaging/snippets/parsers/*.limpid`. Exits 1 if any errors
    /// are reported (warnings are printed but do not fail).
    LintSnippetHeaders,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Cmd::GenSnippetInventory { check } => gen_snippet_inventory::run(check),
        Cmd::LintSnippetHeaders => lint_snippet_headers::run(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::from(1)
        }
    }
}
