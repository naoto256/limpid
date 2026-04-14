//! Configuration loader: reads a main config file and resolves `include` directives.
//!
//! Include directives support glob patterns (e.g. `include "inputs/*.limpid"`)
//! and are resolved relative to the main config file's directory.
//! Nested includes (include within an included file) are not allowed.

use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::dsl::ast::Config;
use crate::dsl::parser::parse_config;

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

    for include_pattern in std::mem::take(&mut config.includes) {
        let full_pattern = base_dir.join(&include_pattern);
        let pattern_str = full_pattern.to_str()
            .ok_or_else(|| anyhow::anyhow!("include path is not valid UTF-8: {}", include_pattern))?;

        let mut paths: Vec<_> = glob::glob(pattern_str)
            .with_context(|| format!("invalid include pattern: {}", include_pattern))?
            .collect::<Result<Vec<_>, _>>()
            .with_context(|| format!("error reading include pattern: {}", include_pattern))?;
        paths.sort();

        for path in paths {
            let canonical = std::fs::canonicalize(&path)
                .with_context(|| format!("failed to canonicalize {}", path.display()))?;
            if canonical == canonical_main {
                bail!("self-inclusion detected: {} includes itself", config_file.display());
            }

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

        fs::write(&sub_file, r#"def input fw { type syslog_udp bind "0.0.0.0:514" }"#).unwrap();
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

        fs::write(sub_dir.join("a.limpid"), r#"def input a { type syslog_udp bind "0.0.0.0:514" }"#).unwrap();
        fs::write(sub_dir.join("b.limpid"), r#"def input b { type syslog_tcp bind "0.0.0.0:514" }"#).unwrap();

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
    fn test_no_includes() {
        let dir = TempDir::new().unwrap();
        let main_conf = dir.path().join("main.conf");
        fs::write(&main_conf, r#"
            control {
                socket "/var/run/limpid/control.sock"
            }
        "#).unwrap();

        let config = load_config(&main_conf).unwrap();
        assert!(config.definitions.is_empty());
        assert_eq!(config.global_blocks.len(), 1);
    }
}
