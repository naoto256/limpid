//! Property extraction helpers.
//!
//! Used by modules to parse their own configuration from DSL property lists.

use super::ast::{Expr, Property};

/// Get a string value for a key (from StringLit, Ident, or IntLit).
pub fn get_string(props: &[Property], key: &str) -> Option<String> {
    for prop in props {
        if let Property::KeyValue(k, expr) = prop
            && k == key {
                return match expr {
                    Expr::StringLit(s) => Some(s.clone()),
                    Expr::Ident(parts) => Some(parts.join(".")),
                    Expr::IntLit(n) => Some(n.to_string()),
                    _ => None,
                };
            }
    }
    None
}

/// Get an identifier value for a key (first segment of ident path).
pub fn get_ident(props: &[Property], key: &str) -> Option<String> {
    for prop in props {
        if let Property::KeyValue(k, Expr::Ident(parts)) = prop
            && k == key {
                return parts.first().cloned();
            }
    }
    None
}

/// Get an integer value for a key.
pub fn get_int(props: &[Property], key: &str) -> Option<i64> {
    for prop in props {
        if let Property::KeyValue(k, Expr::IntLit(n)) = prop
            && k == key {
                return Some(*n);
            }
    }
    None
}

/// Get a non-negative integer value for a key. Returns None if key is absent.
/// Returns Err if value is negative.
pub fn get_positive_int(props: &[Property], key: &str) -> anyhow::Result<Option<u64>> {
    match get_int(props, key) {
        Some(n) if n >= 0 => Ok(Some(n as u64)),
        Some(n) => anyhow::bail!("'{}' must be non-negative, got {}", key, n),
        None => Ok(None),
    }
}

/// Get a strictly positive integer (>= 1). Returns None if key is absent.
/// Returns Err if value is zero or negative.
pub fn get_strictly_positive_int(props: &[Property], key: &str) -> anyhow::Result<Option<u64>> {
    match get_int(props, key) {
        Some(n) if n >= 1 => Ok(Some(n as u64)),
        Some(n) => anyhow::bail!("'{}' must be >= 1, got {}", key, n),
        None => Ok(None),
    }
}

/// Get a nested block's properties by key name.
pub fn get_block<'a>(props: &'a [Property], key: &str) -> Option<&'a Vec<Property>> {
    for prop in props {
        if let Property::Block(k, inner) = prop
            && k == key {
                return Some(inner);
            }
    }
    None
}

/// Parse size strings like "1GB", "512MB", "1024" (bytes).
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim().to_uppercase();
    let parse = |num_str: &str, unit: &str, multiplier: u64| -> anyhow::Result<u64> {
        num_str.trim().parse::<u64>()
            .map(|n| n * multiplier)
            .map_err(|_| anyhow::anyhow!("invalid size '{}': expected a number before '{}'", s, unit))
    };
    if s.ends_with("GB") {
        parse(&s[..s.len() - 2], "GB", 1024 * 1024 * 1024)
    } else if s.ends_with("MB") {
        parse(&s[..s.len() - 2], "MB", 1024 * 1024)
    } else if s.ends_with("KB") {
        parse(&s[..s.len() - 2], "KB", 1024)
    } else {
        s.parse::<u64>()
            .map_err(|_| anyhow::anyhow!("invalid size '{}': expected a number with optional KB/MB/GB suffix", s))
    }
}

/// Parse duration strings like "1s", "5m", "100ms".
pub fn parse_duration(s: &str) -> anyhow::Result<std::time::Duration> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("ms") {
        let n: u64 = num.trim().parse()
            .map_err(|_| anyhow::anyhow!("invalid duration '{}': expected a number before 'ms'", s))?;
        Ok(std::time::Duration::from_millis(n))
    } else if let Some(num) = s.strip_suffix('s') {
        let n: u64 = num.trim().parse()
            .map_err(|_| anyhow::anyhow!("invalid duration '{}': expected a number before 's'", s))?;
        Ok(std::time::Duration::from_secs(n))
    } else if let Some(num) = s.strip_suffix('m') {
        let n: u64 = num.trim().parse()
            .map_err(|_| anyhow::anyhow!("invalid duration '{}': expected a number before 'm'", s))?;
        Ok(std::time::Duration::from_secs(n * 60))
    } else {
        let n: u64 = s.parse()
            .map_err(|_| anyhow::anyhow!("invalid duration '{}': expected a number with optional ms/s/m suffix", s))?;
        Ok(std::time::Duration::from_millis(n))
    }
}
