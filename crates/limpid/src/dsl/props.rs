//! Property extraction helpers.
//!
//! Used by modules to parse their own configuration from DSL property lists.

use super::ast::{Expr, ExprKind, Property, TemplateFragment};

/// Get a string value for a key (from StringLit, Ident, or IntLit).
///
/// For `Template` expressions (strings containing `${...}`), this
/// reconstructs the source-level form (`"literal${ident.path}literal"`)
/// so existing modules with their own template evaluators (e.g.
/// `output file` with its dynamic path) keep working without change.
/// Consumers that want structured evaluation should use
/// `get_expr` and evaluate via `dsl::eval::eval_expr`.
pub fn get_string(props: &[Property], key: &str) -> Option<String> {
    for prop in props {
        if let Property::KeyValue {
            key: k,
            value: expr,
            ..
        } = prop
            && k == key
        {
            return match &expr.kind {
                ExprKind::StringLit(s) => Some(s.clone()),
                ExprKind::Template(frags) => Some(template_to_source(frags)),
                ExprKind::Ident(parts) => Some(parts.join(".")),
                ExprKind::IntLit(n) => Some(n.to_string()),
                _ => None,
            };
        }
    }
    None
}

/// Return the raw `Expr` bound to `key`, if any. Modules wanting to
/// evaluate templates per-event (with a `FunctionRegistry`) should use
/// this rather than `get_string`.
pub fn get_expr<'a>(props: &'a [Property], key: &str) -> Option<&'a Expr> {
    for prop in props {
        if let Property::KeyValue {
            key: k,
            value: expr,
            ..
        } = prop
            && k == key
        {
            return Some(expr);
        }
    }
    None
}

/// Best-effort reconstruction of a Template's source text. Used only
/// for backwards compatibility with modules that still run their own
/// string-level template parser. Handles identifiers and string/int
/// literals; other expression shapes fall back to their `Debug` form.
fn template_to_source(frags: &[TemplateFragment]) -> String {
    let mut out = String::new();
    for f in frags {
        match f {
            TemplateFragment::Literal(s) => out.push_str(s),
            TemplateFragment::Interp(expr) => {
                out.push_str("${");
                push_expr_source(&mut out, expr);
                out.push('}');
            }
        }
    }
    out
}

fn push_expr_source(out: &mut String, expr: &Expr) {
    match &expr.kind {
        ExprKind::Ident(parts) => out.push_str(&parts.join(".")),
        ExprKind::StringLit(s) => {
            out.push('"');
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    c => out.push(c),
                }
            }
            out.push('"');
        }
        ExprKind::IntLit(n) => out.push_str(&n.to_string()),
        ExprKind::FloatLit(n) => out.push_str(&n.to_string()),
        ExprKind::BoolLit(b) => out.push_str(if *b { "true" } else { "false" }),
        ExprKind::Null => out.push_str("null"),
        other => out.push_str(&format!("{:?}", other)),
    }
}

/// Get an identifier value for a key (first segment of ident path).
pub fn get_ident(props: &[Property], key: &str) -> Option<String> {
    for prop in props {
        if let Property::KeyValue {
            key: k,
            value:
                Expr {
                    kind: ExprKind::Ident(parts),
                    ..
                },
            ..
        } = prop
            && k == key
        {
            return parts.first().cloned();
        }
    }
    None
}

/// Get a boolean value for a key.
///
/// Accepts both spellings the schema validator
/// ([`crate::dsl::schema`], `PropertyValueKind::Bool`) admits:
///
/// - `ExprKind::BoolLit` — what the pest grammar actually produces for
///   `verify false` (`bool_lit` wins over `ident_path` in the `atom`
///   rule, so a bare `true` / `false` never parses as an ident);
/// - `ExprKind::Ident(["true"])` / `Ident(["false"])` — hand-built
///   ASTs (test helpers, potential future programmatic config).
///
/// Reading Bool-kind properties through [`get_ident`] is a bug: the
/// parser emits `BoolLit`, `get_ident` returns `None`, and the caller
/// silently falls back to its default (this is exactly how
/// `verify false` was ignored until v0.7.9 — see `output http` /
/// `output otlp_http`).
pub fn get_bool(props: &[Property], key: &str) -> Option<bool> {
    for prop in props {
        if let Property::KeyValue {
            key: k,
            value: expr,
            ..
        } = prop
            && k == key
        {
            return match &expr.kind {
                ExprKind::BoolLit(b) => Some(*b),
                ExprKind::Ident(parts) if parts.len() == 1 => match parts[0].as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                },
                _ => None,
            };
        }
    }
    None
}

/// Get an integer value for a key.
pub fn get_int(props: &[Property], key: &str) -> Option<i64> {
    for prop in props {
        if let Property::KeyValue {
            key: k,
            value:
                Expr {
                    kind: ExprKind::IntLit(n),
                    ..
                },
            ..
        } = prop
            && k == key
        {
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
        if let Property::Block {
            key: k,
            properties: inner,
            ..
        } = prop
            && k == key
        {
            return Some(inner);
        }
    }
    None
}

/// Extract a `StringMap`-shaped sub-block as a list of `(key, value)`
/// pairs. The block's key set is open (HTTP header names, k8s-style
/// labels, etc.) and every value is rendered to `String` through the
/// same rules `get_string` uses (`StringLit` / `Template` source
/// reconstruction / `Ident` joined by `.` / `IntLit` decimal).
///
/// Returns `Vec::new()` when the block is absent. The analyzer flags
/// non-string entries via the schema (`PropertyValueKind::StringMap`),
/// so callers here can safely drop them without re-reporting.
pub fn get_string_map(props: &[Property], key: &str) -> Vec<(String, String)> {
    let Some(block) = get_block(props, key) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for prop in block {
        if let Property::KeyValue {
            key: k,
            value: expr,
            ..
        } = prop
            && let Some(val) = match &expr.kind {
                ExprKind::StringLit(s) => Some(s.clone()),
                ExprKind::Template(frags) => Some(template_to_source(frags)),
                ExprKind::Ident(parts) => Some(parts.join(".")),
                ExprKind::IntLit(n) => Some(n.to_string()),
                _ => None,
            }
        {
            out.push((k.clone(), val));
        }
    }
    out
}

/// Parse size strings like "1GB", "512MB", "1024" (bytes).
pub fn parse_size(s: &str) -> anyhow::Result<u64> {
    let s = s.trim().to_uppercase();
    let parse = |num_str: &str, unit: &str, multiplier: u64| -> anyhow::Result<u64> {
        num_str
            .trim()
            .parse::<u64>()
            .map(|n| n * multiplier)
            .map_err(|_| {
                anyhow::anyhow!("invalid size '{}': expected a number before '{}'", s, unit)
            })
    };
    if s.ends_with("GB") {
        parse(&s[..s.len() - 2], "GB", 1024 * 1024 * 1024)
    } else if s.ends_with("MB") {
        parse(&s[..s.len() - 2], "MB", 1024 * 1024)
    } else if s.ends_with("KB") {
        parse(&s[..s.len() - 2], "KB", 1024)
    } else {
        s.parse::<u64>().map_err(|_| {
            anyhow::anyhow!(
                "invalid size '{}': expected a number with optional KB/MB/GB suffix",
                s
            )
        })
    }
}

/// Parse duration strings like "1s", "5m", "100ms".
pub fn parse_duration(s: &str) -> anyhow::Result<std::time::Duration> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix("ms") {
        let n: u64 = num.trim().parse().map_err(|_| {
            anyhow::anyhow!("invalid duration '{}': expected a number before 'ms'", s)
        })?;
        Ok(std::time::Duration::from_millis(n))
    } else if let Some(num) = s.strip_suffix('s') {
        let n: u64 = num.trim().parse().map_err(|_| {
            anyhow::anyhow!("invalid duration '{}': expected a number before 's'", s)
        })?;
        Ok(std::time::Duration::from_secs(n))
    } else if let Some(num) = s.strip_suffix('m') {
        let n: u64 = num.trim().parse().map_err(|_| {
            anyhow::anyhow!("invalid duration '{}': expected a number before 'm'", s)
        })?;
        Ok(std::time::Duration::from_secs(n * 60))
    } else {
        let n: u64 = s.parse().map_err(|_| {
            anyhow::anyhow!(
                "invalid duration '{}': expected a number with optional ms/s/m suffix",
                s
            )
        })?;
        Ok(std::time::Duration::from_millis(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(key: &str, kind: ExprKind) -> Property {
        Property::KeyValue {
            key: key.to_string(),
            key_span: None,
            value: Expr::spanless(kind),
            value_span: None,
        }
    }

    #[test]
    fn get_bool_reads_bool_lit() {
        // The form the pest grammar actually produces for `verify false`.
        let props = vec![kv("verify", ExprKind::BoolLit(false))];
        assert_eq!(get_bool(&props, "verify"), Some(false));
        let props = vec![kv("verify", ExprKind::BoolLit(true))];
        assert_eq!(get_bool(&props, "verify"), Some(true));
    }

    #[test]
    fn get_bool_reads_legacy_ident_spelling() {
        // Hand-built ASTs (test helpers) encode booleans as bare idents;
        // the schema validator admits this form, so get_bool must too.
        let props = vec![kv("verify", ExprKind::Ident(vec!["false".into()]))];
        assert_eq!(get_bool(&props, "verify"), Some(false));
        let props = vec![kv("verify", ExprKind::Ident(vec!["true".into()]))];
        assert_eq!(get_bool(&props, "verify"), Some(true));
    }

    #[test]
    fn get_bool_rejects_non_bool_shapes() {
        assert_eq!(get_bool(&[], "verify"), None);
        let props = vec![kv("verify", ExprKind::StringLit("false".into()))];
        assert_eq!(get_bool(&props, "verify"), None);
        let props = vec![kv("verify", ExprKind::Ident(vec!["yes".into()]))];
        assert_eq!(get_bool(&props, "verify"), None);
        // Multi-segment ident path is not a boolean.
        let props = vec![kv(
            "verify",
            ExprKind::Ident(vec!["a".into(), "false".into()]),
        )];
        assert_eq!(get_bool(&props, "verify"), None);
    }
}
