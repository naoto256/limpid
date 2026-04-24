//! Configuration loader: reads a main config file and resolves `include` directives.
//!
//! Include directives support glob patterns (e.g. `include "inputs/*.limpid"`)
//! and are resolved relative to the main config file's directory.
//! Nested includes (include within an included file) are not allowed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::dsl::ast::Config;
use crate::dsl::parser::{parse_config, parse_config_with_file_id};
use crate::dsl::span::SourceMap;

/// Reject include patterns that are absolute paths. Relative patterns
/// are the only supported form — they are resolved against the main
/// config file's directory and later confirmed to stay inside that
/// directory (see [`ensure_within_root`]).
///
/// On Unix an absolute path starts with `/`. `Path::is_absolute`
/// handles the platform-correct check (Windows drive letters, UNC).
fn reject_absolute_include(pattern: &str) -> Result<()> {
    if Path::new(pattern).is_absolute() {
        bail!(
            "include path '{}' must be relative to the config directory (absolute paths are not allowed)",
            pattern
        );
    }
    Ok(())
}

/// Ensure `candidate` (already canonicalized) lives inside `root`
/// (already canonicalized). Rejects `..` escapes and absolute paths
/// that slipped through glob expansion.
fn ensure_within_root(candidate: &Path, root: &Path, original_pattern: &str) -> Result<()> {
    if !candidate.starts_with(root) {
        bail!(
            "include path '{}' escapes the config directory ({} is outside {})",
            original_pattern,
            candidate.display(),
            root.display()
        );
    }
    Ok(())
}

/// Canonicalize the main config's parent directory so include paths
/// can be checked with `starts_with` on canonical forms (resolves
/// symlinks and `..` components).
fn canonical_root(config_file: &Path) -> Result<PathBuf> {
    let base_dir = config_file.parent().unwrap_or(Path::new("."));
    // canonicalize requires the directory to exist; for a bare file
    // name with no parent we fall back to the current dir.
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

/// Load configuration from a main config file, resolving any `include` directives.
///
/// - The main config file can contain `include "pattern"` directives.
/// - Include paths are resolved relative to the main config file's directory.
/// - Glob patterns are supported (e.g. `include "inputs/*.limpid"`).
/// - Included files cannot themselves contain `include` directives.
/// - Self-inclusion is detected and rejected.
pub fn load_config(config_file: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(config_file)
        .with_context(|| format!("failed to read {}", config_file.display()))?;
    let mut config = parse_config(&content)
        .with_context(|| format!("failed to parse {}", config_file.display()))?;

    if config.includes.is_empty() {
        return Ok(config);
    }

    let base_dir = config_file.parent().unwrap_or(Path::new("."));
    let canonical_main = std::fs::canonicalize(config_file)
        .with_context(|| format!("failed to canonicalize {}", config_file.display()))?;
    let canonical_root = canonical_root(config_file)?;

    for include_pattern in std::mem::take(&mut config.includes) {
        reject_absolute_include(&include_pattern)?;

        let full_pattern = base_dir.join(&include_pattern);
        let pattern_str = full_pattern.to_str().ok_or_else(|| {
            anyhow::anyhow!("include path is not valid UTF-8: {}", include_pattern)
        })?;

        let mut paths: Vec<_> = glob::glob(pattern_str)
            .with_context(|| format!("invalid include pattern: {}", include_pattern))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("error reading include pattern: {}", include_pattern))?;
        paths.sort();

        for path in paths {
            let canonical = std::fs::canonicalize(&path)
                .with_context(|| format!("failed to canonicalize {}", path.display()))?;
            if canonical == canonical_main {
                bail!(
                    "self-inclusion detected: {} includes itself",
                    config_file.display()
                );
            }
            ensure_within_root(&canonical, &canonical_root, &include_pattern)?;

            let inc_content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read included file {}", path.display()))?;
            let inc_config = parse_config(&inc_content)
                .with_context(|| format!("failed to parse included file {}", path.display()))?;

            if !inc_config.includes.is_empty() {
                bail!(
                    "nested include not allowed: {} contains include directives (included from {})",
                    path.display(),
                    config_file.display(),
                );
            }

            config.definitions.extend(inc_config.definitions);
            config.global_blocks.extend(inc_config.global_blocks);
        }
    }

    Ok(config)
}

/// Like [`load_config`] but also builds a [`SourceMap`] populated with
/// every file that contributed to the AST. Each file gets a distinct
/// `file_id` so spans recorded by the parser resolve to the correct
/// physical file when `--check` renders snippet + caret.
///
/// `file_id` 0 is assigned to the main config; each included file gets
/// a subsequent id.
pub fn load_config_with_source_map(config_file: &Path) -> Result<(Config, SourceMap)> {
    let mut source_map = SourceMap::new();
    let main_content = std::fs::read_to_string(config_file)
        .with_context(|| format!("failed to read {}", config_file.display()))?;
    let main_id = source_map.add_file(config_file.to_path_buf(), main_content.clone());
    let mut config = parse_config_with_file_id(&main_content, main_id)
        .with_context(|| format!("failed to parse {}", config_file.display()))?;

    if config.includes.is_empty() {
        return Ok((config, source_map));
    }

    let base_dir = config_file.parent().unwrap_or(Path::new("."));
    let canonical_main = std::fs::canonicalize(config_file)
        .with_context(|| format!("failed to canonicalize {}", config_file.display()))?;
    let canonical_root = canonical_root(config_file)?;

    for include_pattern in std::mem::take(&mut config.includes) {
        reject_absolute_include(&include_pattern)?;

        let full_pattern = base_dir.join(&include_pattern);
        let pattern_str = full_pattern.to_str().ok_or_else(|| {
            anyhow::anyhow!("include path is not valid UTF-8: {}", include_pattern)
        })?;

        let mut paths: Vec<_> = glob::glob(pattern_str)
            .with_context(|| format!("invalid include pattern: {}", include_pattern))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("error reading include pattern: {}", include_pattern))?;
        paths.sort();

        for path in paths {
            let canonical = std::fs::canonicalize(&path)
                .with_context(|| format!("failed to canonicalize {}", path.display()))?;
            if canonical == canonical_main {
                bail!(
                    "self-inclusion detected: {} includes itself",
                    config_file.display()
                );
            }
            ensure_within_root(&canonical, &canonical_root, &include_pattern)?;

            let inc_content = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read included file {}", path.display()))?;
            let inc_id = source_map.add_file(path.clone(), inc_content.clone());
            let inc_config = parse_config_with_file_id(&inc_content, inc_id)
                .with_context(|| format!("failed to parse included file {}", path.display()))?;

            if !inc_config.includes.is_empty() {
                bail!(
                    "nested include not allowed: {} contains include directives (included from {})",
                    path.display(),
                    config_file.display(),
                );
            }

            config.definitions.extend(inc_config.definitions);
            config.global_blocks.extend(inc_config.global_blocks);
        }
    }

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
    fn test_self_inclusion_error() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        fs::write(&main_conf, r#"include "main.conf""#).unwrap();

        let result = load_config(&main_conf);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("self-inclusion"));
    }

    #[test]
    fn test_nested_include_error() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        let sub_file = dir.path().join("sub.limpid");

        fs::write(&sub_file, r#"include "other.limpid""#).unwrap();
        fs::write(&main_conf, r#"include "sub.limpid""#).unwrap();

        let result = load_config(&main_conf);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("nested include"));
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
    fn test_reject_absolute_include_path() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        // Use a path that definitely exists but is absolute — /etc on Unix.
        fs::write(&main_conf, r#"include "/etc/hosts""#).unwrap();

        let result = load_config(&main_conf);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("absolute"), "got: {msg}");
    }

    #[test]
    fn test_reject_parent_dir_escape() {
        // Place main.conf in a subdir, then try to include `../outside.limpid`.
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

        let result = load_config(&main_conf);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("escapes the config directory"), "got: {msg}");
    }

    #[test]
    fn test_subdirectory_include_allowed() {
        // Regression: the confinement check must not reject legitimate
        // relative paths into a subdirectory.
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
        // Parity with load_config: load_config_with_source_map must
        // apply the same confinement.
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

        let result = load_config_with_source_map(&main_conf);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("escapes the config directory")
        );
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
}
