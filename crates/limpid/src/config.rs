//! Configuration loader: reads a main config file and resolves `include` directives.
//!
//! Includes support two source trees:
//!
//! 1. **Config root** — the directory of the top-level config file. All
//!    relative `include` paths resolve here, and canonicalised results
//!    must stay inside the root so `..` escapes are impossible.
//! 2. **System snippet tree** ([`SYSTEM_SNIPPET_DIR`], default
//!    `/usr/share/limpid/snippets`) — read-only packager-provided
//!    library files. Absolute paths pointing under this tree are
//!    allowed so users can `include
//!    "/usr/share/limpid/snippets/parsers/fortigate.limpid"` and
//!    similar without copying the files into their own config
//!    directory. Any other absolute path is rejected.
//!
//! Includes are **recursive with cycle detection and a depth cap**. A
//! shared `loaded` set makes the loader diamond-tolerant (a snippet
//! referenced from two parents is parsed once), while a `loading`
//! stack catches true cycles (`a → b → a`). The depth cap
//! ([`MAX_INCLUDE_DEPTH`]) is a DoS guard for pathologically deep
//! trees; legitimate library use rarely exceeds 2-3 levels.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::dsl::ast::Config;
use crate::dsl::parser::parse_config_with_file_id;
use crate::dsl::span::SourceMap;

/// Read-only system-provided snippet tree. Absolute include paths are
/// allowed when (and only when) they resolve under this prefix —
/// packaged snippets live here and users reference them via absolute
/// path so `apt upgrade` propagates fixes without per-config copies.
pub const SYSTEM_SNIPPET_DIR: &str = "/usr/share/limpid/snippets";

/// Maximum include depth. A normal library layout is 2-3 deep
/// (user config → vendor snippet → shared helper); this cap is a
/// DoS guard, not a design limit.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Validate a raw include pattern string from the DSL source. Relative
/// patterns are always acceptable; absolute patterns are acceptable
/// only when they point into the system snippet tree.
fn validate_include_pattern(pattern: &str) -> Result<()> {
    if Path::new(pattern).is_absolute() && !pattern.starts_with(SYSTEM_SNIPPET_DIR) {
        bail!(
            "include path '{}' must be relative to the including file's directory \
             (absolute paths are allowed only under {})",
            pattern,
            SYSTEM_SNIPPET_DIR
        );
    }
    Ok(())
}

/// Verify a canonical path resolves to a permitted location:
/// either inside the user's config root, or inside the system
/// snippet tree. Any other location is rejected.
fn ensure_permitted_location(
    canonical: &Path,
    canonical_root: &Path,
    original_pattern: &str,
) -> Result<()> {
    if canonical.starts_with(canonical_root) {
        return Ok(());
    }
    if canonical.starts_with(SYSTEM_SNIPPET_DIR) {
        return Ok(());
    }
    bail!(
        "include path '{}' ({}) is outside the config directory ({}) \
         and the system snippet tree ({})",
        original_pattern,
        canonical.display(),
        canonical_root.display(),
        SYSTEM_SNIPPET_DIR,
    );
}

/// Canonicalize the main config's parent directory. Canonical form is
/// required so `starts_with` comparisons below are meaningful
/// (resolves symlinks, collapses `..`).
fn canonical_root(config_file: &Path) -> Result<PathBuf> {
    let base_dir = config_file.parent().unwrap_or(Path::new("."));
    let base_for_canon = if base_dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        base_dir
    };
    std::fs::canonicalize(base_for_canon).with_context(|| {
        format!(
            "failed to canonicalize config directory {}",
            base_dir.display()
        )
    })
}

/// Recursion state shared across every include-graph traversal call.
///
/// `loaded` records files that have already contributed their
/// definitions: re-encountering one is a silent skip (diamond
/// tolerance, like `#pragma once`). `loading` records files currently
/// on the load stack: re-encountering one is a true cycle.
#[derive(Default)]
struct IncludeState {
    loaded: HashSet<PathBuf>,
    loading: HashSet<PathBuf>,
}

/// Recursive loader: parses `path`, resolves its includes (each
/// processed through this same function), and returns the merged
/// `Config`. `source_map` is populated with every file actually loaded.
fn load_recursive(
    path: &Path,
    canonical_root: &Path,
    state: &mut IncludeState,
    depth: usize,
    source_map: &mut SourceMap,
) -> Result<Config> {
    if depth > MAX_INCLUDE_DEPTH {
        bail!(
            "include depth exceeds limit ({}); likely a deep cycle or runaway include tree at {}",
            MAX_INCLUDE_DEPTH,
            path.display()
        );
    }

    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;

    if state.loading.contains(&canonical) {
        bail!(
            "include cycle detected: {} is already being loaded",
            canonical.display()
        );
    }
    if state.loaded.contains(&canonical) {
        // Diamond: this file's definitions were already merged via an
        // earlier parent. Returning an empty config keeps duplication
        // out of the merged AST.
        return Ok(Config {
            definitions: Vec::new(),
            global_blocks: Vec::new(),
            includes: Vec::new(),
        });
    }
    state.loading.insert(canonical.clone());

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let file_id = source_map.add_file(path.to_path_buf(), content.clone());
    let mut config = parse_config_with_file_id(&content, file_id)
        .with_context(|| format!("failed to parse {}", path.display()))?;

    let base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    for include_pattern in std::mem::take(&mut config.includes) {
        validate_include_pattern(&include_pattern)?;

        let full_pattern: PathBuf = if Path::new(&include_pattern).is_absolute() {
            PathBuf::from(&include_pattern)
        } else {
            base_dir.join(&include_pattern)
        };
        let pattern_str = full_pattern.to_str().ok_or_else(|| {
            anyhow::anyhow!("include path is not valid UTF-8: {}", include_pattern)
        })?;
        let mut paths: Vec<_> = glob::glob(pattern_str)
            .with_context(|| format!("invalid include pattern: {}", include_pattern))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("error reading include pattern: {}", include_pattern))?;
        paths.sort();

        for inc_path in paths {
            let canonical_inc = std::fs::canonicalize(&inc_path)
                .with_context(|| format!("failed to canonicalize {}", inc_path.display()))?;
            ensure_permitted_location(&canonical_inc, canonical_root, &include_pattern)?;

            let inc_config =
                load_recursive(&inc_path, canonical_root, state, depth + 1, source_map)?;
            config.definitions.extend(inc_config.definitions);
            config.global_blocks.extend(inc_config.global_blocks);
        }
    }

    state.loading.remove(&canonical);
    state.loaded.insert(canonical);
    Ok(config)
}

/// Load configuration from a main config file, resolving every
/// `include` directive recursively.
///
/// - Relative include paths resolve against the including file's
///   directory.
/// - Absolute include paths are allowed only under
///   [`SYSTEM_SNIPPET_DIR`].
/// - Self-inclusion (a file including itself) and longer cycles are
///   both reported as "include cycle detected".
/// - Files referenced more than once (diamonds) are parsed exactly
///   once; subsequent references silently skip.
/// - Depth is capped at [`MAX_INCLUDE_DEPTH`].
pub fn load_config(config_file: &Path) -> Result<Config> {
    let mut source_map = SourceMap::new();
    let mut state = IncludeState::default();
    let canonical_root = canonical_root(config_file)?;
    load_recursive(config_file, &canonical_root, &mut state, 0, &mut source_map)
}

/// Same loading semantics as [`load_config`], but additionally returns
/// the [`SourceMap`] populated with every file touched. Each file
/// receives a distinct `file_id` so analyzer diagnostics can render
/// `snippet + caret` against the right physical file.
pub fn load_config_with_source_map(config_file: &Path) -> Result<(Config, SourceMap)> {
    let mut source_map = SourceMap::new();
    let mut state = IncludeState::default();
    let canonical_root = canonical_root(config_file)?;
    let config = load_recursive(config_file, &canonical_root, &mut state, 0, &mut source_map)?;
    Ok((config, source_map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_basic_include() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        let sub_file = dir.path().join("sub.limpid");

        fs::write(
            &sub_file,
            r#"def input fw { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        fs::write(&main_conf, r#"include "sub.limpid""#).unwrap();

        let config = load_config(&main_conf).unwrap();
        assert_eq!(config.definitions.len(), 1);
        assert!(config.includes.is_empty());
    }

    #[test]
    fn test_glob_include() {
        let dir = TempDir::new().unwrap();
        let sub_dir = dir.path().join("inputs");
        fs::create_dir(&sub_dir).unwrap();

        fs::write(
            sub_dir.join("a.limpid"),
            r#"def input a { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        fs::write(
            sub_dir.join("b.limpid"),
            r#"def input b { type syslog_tcp bind "0.0.0.0:514" }"#,
        )
        .unwrap();

        let main_conf = dir.path().join("main.conf");
        fs::write(&main_conf, r#"include "inputs/*.limpid""#).unwrap();

        let config = load_config(&main_conf).unwrap();
        assert_eq!(config.definitions.len(), 2);
    }

    #[test]
    fn test_self_inclusion_detected_as_cycle() {
        // Self-inclusion is a cycle of length 1; the new recursive
        // loader reports it via the unified cycle diagnostic.
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        fs::write(&main_conf, r#"include "main.conf""#).unwrap();

        let err = load_config(&main_conf).unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn test_nested_include_now_allowed() {
        // Regression: v0.4 rejected nested includes entirely. They are
        // required for the snippet library (parser snippets include
        // shared helpers). Diamond / dedup semantics are verified
        // below; this test just confirms nesting no longer errors.
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        let mid_file = dir.path().join("mid.limpid");
        let leaf_file = dir.path().join("leaf.limpid");

        fs::write(
            &leaf_file,
            r#"def input fw { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        fs::write(&mid_file, r#"include "leaf.limpid""#).unwrap();
        fs::write(&main_conf, r#"include "mid.limpid""#).unwrap();

        let config = load_config(&main_conf).unwrap();
        assert_eq!(config.definitions.len(), 1);
    }

    #[test]
    fn test_include_cycle_detected() {
        // a → b → a is a real cycle. The loader must catch this
        // without recursing indefinitely.
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        let a = dir.path().join("a.limpid");
        let b = dir.path().join("b.limpid");

        fs::write(&a, r#"include "b.limpid""#).unwrap();
        fs::write(&b, r#"include "a.limpid""#).unwrap();
        fs::write(&main_conf, r#"include "a.limpid""#).unwrap();

        let err = load_config(&main_conf).unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn test_diamond_dedup_loads_shared_file_once() {
        // main → {parent_a, parent_b} → shared. `shared` must only
        // contribute its definitions once, not twice, or downstream
        // consumers see duplicate `def` entries.
        let dir = TempDir::new().unwrap();
        let shared = dir.path().join("shared.limpid");
        let parent_a = dir.path().join("parent_a.limpid");
        let parent_b = dir.path().join("parent_b.limpid");
        let main_conf = dir.path().join("main.conf");

        fs::write(
            &shared,
            r#"def input shared { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        fs::write(&parent_a, r#"include "shared.limpid""#).unwrap();
        fs::write(&parent_b, r#"include "shared.limpid""#).unwrap();
        fs::write(
            &main_conf,
            "include \"parent_a.limpid\"\ninclude \"parent_b.limpid\"",
        )
        .unwrap();

        let config = load_config(&main_conf).unwrap();
        assert_eq!(
            config.definitions.len(),
            1,
            "shared.limpid should contribute its single definition exactly once"
        );
    }

    #[test]
    fn test_no_matches_is_ok() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        fs::write(&main_conf, r#"include "nonexistent/*.limpid""#).unwrap();

        let config = load_config(&main_conf).unwrap();
        assert!(config.definitions.is_empty());
    }

    #[test]
    fn test_load_with_source_map_assigns_distinct_ids() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        let sub_file = dir.path().join("sub.limpid");
        fs::write(
            &sub_file,
            r#"def input fw { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        fs::write(&main_conf, r#"include "sub.limpid""#).unwrap();
        let (config, sm) = load_config_with_source_map(&main_conf).unwrap();
        assert_eq!(config.definitions.len(), 1);
        // Main + 1 included file → 2 distinct file ids registered.
        assert!(sm.file_count() >= 2);
    }

    #[test]
    fn test_reject_absolute_include_outside_system_dir() {
        // Absolute paths that aren't under the system snippet tree
        // are still rejected — the tree is the single broadening of
        // the rule, not a general "absolute paths welcome".
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        fs::write(&main_conf, r#"include "/etc/hosts""#).unwrap();

        let err = load_config(&main_conf).unwrap_err().to_string();
        assert!(
            err.contains("absolute paths are allowed only under"),
            "got: {err}"
        );
        assert!(err.contains(SYSTEM_SNIPPET_DIR), "got: {err}");
    }

    #[test]
    fn test_reject_parent_dir_escape() {
        let dir = TempDir::new().unwrap();
        let inner = dir.path().join("inner");
        fs::create_dir(&inner).unwrap();
        let outside = dir.path().join("outside.limpid");
        fs::write(
            &outside,
            r#"def input fw { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        let main_conf = inner.join("main.conf");
        fs::write(&main_conf, r#"include "../outside.limpid""#).unwrap();

        let err = load_config(&main_conf).unwrap_err().to_string();
        assert!(err.contains("outside the config directory"), "got: {err}");
    }

    #[test]
    fn test_subdirectory_include_allowed() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        fs::create_dir(&sub).unwrap();
        fs::write(
            sub.join("x.limpid"),
            r#"def input a { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        let main_conf = dir.path().join("main.conf");
        fs::write(&main_conf, r#"include "subdir/x.limpid""#).unwrap();

        let config = load_config(&main_conf).unwrap();
        assert_eq!(config.definitions.len(), 1);
    }

    #[test]
    fn test_source_map_loader_also_rejects_escape() {
        let dir = TempDir::new().unwrap();
        let inner = dir.path().join("inner");
        fs::create_dir(&inner).unwrap();
        let outside = dir.path().join("outside.limpid");
        fs::write(
            &outside,
            r#"def input fw { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        let main_conf = inner.join("main.conf");
        fs::write(&main_conf, r#"include "../outside.limpid""#).unwrap();

        let err = load_config_with_source_map(&main_conf)
            .unwrap_err()
            .to_string();
        assert!(err.contains("outside the config directory"), "got: {err}");
    }

    #[test]
    fn test_no_includes() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        fs::write(
            &main_conf,
            r#"
            control {
                socket "/var/run/limpid/control.sock"
            }
        "#,
        )
        .unwrap();

        let config = load_config(&main_conf).unwrap();
        assert!(config.definitions.is_empty());
        assert_eq!(config.global_blocks.len(), 1);
    }

    // ---------------------------------------------------------------
    // System snippet tree integration
    //
    // The tests below synthesise a `SYSTEM_SNIPPET_DIR`-shaped tree
    // inside a tempdir via a symlink so we exercise the allow-list
    // code path without touching `/usr/share/limpid/snippets` on the
    // host. If the host lacks symlink support the tests are skipped
    // with a warning rather than failing.
    // ---------------------------------------------------------------

    /// Best-effort helper: tries to create a symlink at
    /// `SYSTEM_SNIPPET_DIR` pointing to `target`. Returns `None` if
    /// the host refuses (non-root / read-only root filesystem), in
    /// which case the caller should skip the test.
    #[cfg(unix)]
    fn try_symlink_system_dir(target: &Path) -> Option<()> {
        let sys = Path::new(SYSTEM_SNIPPET_DIR);
        if sys.exists() {
            // Don't clobber a real install.
            return None;
        }
        if let Some(parent) = sys.parent() {
            if !parent.exists() {
                // /usr/share/limpid may not exist in CI; try to create
                // the whole chain. Fall through on EACCES.
                let _ = std::fs::create_dir_all(parent);
            }
        }
        std::os::unix::fs::symlink(target, sys).ok()
    }

    #[cfg(unix)]
    fn cleanup_symlink_system_dir() {
        let sys = Path::new(SYSTEM_SNIPPET_DIR);
        if sys.is_symlink() {
            let _ = std::fs::remove_file(sys);
        }
    }

    // The symlink-based tests share global filesystem state so we
    // serialise them behind a mutex. A real environment check would
    // spin up a containerised namespace, but the mutex is enough to
    // avoid interleaving within a single `cargo test` run.
    #[cfg(unix)]
    static SYSTEM_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    #[cfg(unix)]
    fn test_absolute_system_snippet_include() {
        let _guard = SYSTEM_DIR_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let snippets = dir.path().join("system_snippets");
        fs::create_dir(&snippets).unwrap();
        fs::write(
            snippets.join("helper.limpid"),
            r#"def input sys { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();

        if try_symlink_system_dir(&snippets).is_none() {
            eprintln!("skipping: cannot symlink {SYSTEM_SNIPPET_DIR}");
            return;
        }

        let main_conf = dir.path().join("main.conf");
        fs::write(
            &main_conf,
            format!(r#"include "{SYSTEM_SNIPPET_DIR}/helper.limpid""#),
        )
        .unwrap();

        let result = load_config(&main_conf);
        cleanup_symlink_system_dir();

        let config = result.unwrap();
        assert_eq!(config.definitions.len(), 1);
    }

    #[test]
    #[cfg(unix)]
    fn test_nested_include_across_trees() {
        // A user-tree config includes a system snippet, which in turn
        // includes its own sibling under the system tree. Exercises
        // both the nested-include loop and the permitted-location
        // branch for non-root paths.
        let _guard = SYSTEM_DIR_LOCK.lock().unwrap();
        let dir = TempDir::new().unwrap();
        let snippets = dir.path().join("system_snippets");
        let common = snippets.join("_common");
        let parsers = snippets.join("parsers");
        fs::create_dir_all(&common).unwrap();
        fs::create_dir_all(&parsers).unwrap();
        fs::write(
            common.join("shared.limpid"),
            r#"def input shared { type syslog_udp bind "0.0.0.0:514" }"#,
        )
        .unwrap();
        fs::write(
            parsers.join("vendor.limpid"),
            r#"include "../_common/shared.limpid""#,
        )
        .unwrap();

        if try_symlink_system_dir(&snippets).is_none() {
            eprintln!("skipping: cannot symlink {SYSTEM_SNIPPET_DIR}");
            return;
        }

        let main_conf = dir.path().join("main.conf");
        fs::write(
            &main_conf,
            format!(r#"include "{SYSTEM_SNIPPET_DIR}/parsers/vendor.limpid""#),
        )
        .unwrap();

        let result = load_config(&main_conf);
        cleanup_symlink_system_dir();

        let config = result.unwrap();
        assert_eq!(config.definitions.len(), 1);
    }
}
