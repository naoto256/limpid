//! Path resolution for xtask subcommands.
//!
//! Paths to snippet artifacts are anchored at the workspace root,
//! not at the current working directory. The workspace root is
//! derived at compile time from `CARGO_MANIFEST_DIR` (= the xtask
//! crate dir, `crates/xtask`) by walking up two parents. This lets
//! `cargo xtask <subcommand>` work from any subdirectory, not just
//! the workspace root.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

/// Path to the workspace root.
///
/// Derived as `<xtask crate dir>/../..` at compile time. Two
/// `parent()` calls because `CARGO_MANIFEST_DIR` for the xtask
/// binary is `crates/xtask`, and the workspace root is the
/// grandparent. If the layout ever changes (xtask moves out of
/// `crates/`), this constant must be revisited.
pub fn workspace_root() -> PathBuf {
    let package_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    package_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root: CARGO_MANIFEST_DIR has no grandparent (xtask moved?)")
        .to_path_buf()
}

pub fn parsers_dir() -> PathBuf {
    workspace_root().join("packaging/snippets/parsers")
}

pub fn readme_path() -> PathBuf {
    workspace_root().join("packaging/snippets/README.md")
}

/// Recursively collect every `.limpid` file under [`parsers_dir`].
/// Recursive (not just one level) so future organisational
/// subdirs under `parsers/` (e.g. by vendor family) Just Work
/// without code changes.
pub fn list_parser_files() -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut out = Vec::new();
    walk_limpid(&parsers_dir(), &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_limpid(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Box<dyn Error>> {
    let entries = fs::read_dir(dir).map_err(|e| {
        format!(
            "read_dir({}): {e} — expected the workspace to contain `packaging/snippets/parsers/`; \
             is the xtask binary running against the right workspace?",
            dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_limpid(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("limpid") {
            out.push(path);
        }
    }
    Ok(())
}
