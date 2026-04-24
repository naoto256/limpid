//! Function registry: maps function names to implementations.
//!
//! All expression functions (built-in and future dynamic plugins) register
//! themselves here. The DSL evaluator resolves function calls through the
//! registry instead of hardcoded match arms.
//!
//! This is the extension point for future dynamic (.so) function loading.

pub mod geoip;
pub mod table;

use std::collections::HashMap;

use anyhow::{Result, bail};
use serde_json::Value;

use crate::event::Event;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

type ExprFn = Box<dyn Fn(&[Value], &Event) -> Result<Value> + Send + Sync>;

pub struct FunctionRegistry {
    functions: HashMap<String, ExprFn>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
        }
    }

    pub fn register<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[Value], &Event) -> Result<Value> + Send + Sync + 'static,
    {
        self.functions.insert(name.to_string(), Box::new(f));
    }

    pub fn call(&self, name: &str, args: &[Value], event: &Event) -> Result<Value> {
        let f = self
            .functions
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown function: {}", name))?;
        f(args, event)
    }
}

// ---------------------------------------------------------------------------
// Built-in function registration
// ---------------------------------------------------------------------------

/// Register all built-in expression functions.
pub fn register_builtins(reg: &mut FunctionRegistry, table_store: table::TableStore) {
    use std::cell::RefCell;

    // Thread-local regex cache with size limit to prevent memory exhaustion
    const REGEX_CACHE_MAX: usize = 256;

    fn get_cached_regex(pattern: &str) -> Result<regex_lite::Regex, regex_lite::Error> {
        thread_local! {
            static CACHE: RefCell<HashMap<String, regex_lite::Regex>> = RefCell::new(HashMap::new());
        }
        CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(re) = cache.get(pattern) {
                return Ok(re.clone());
            }
            let re = regex_lite::Regex::new(pattern)?;
            if cache.len() >= REGEX_CACHE_MAX {
                cache.clear(); // evict all when full
            }
            cache.insert(pattern.to_string(), re.clone());
            Ok(re)
        })
    }

    fn val_to_str(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }

    reg.register("contains", |args, _event| {
        if args.len() != 2 {
            bail!("contains() expects 2 arguments");
        }
        let haystack = val_to_str(&args[0]);
        let needle = val_to_str(&args[1]);
        Ok(Value::Bool(haystack.contains(&needle)))
    });

    reg.register("lower", |args, _event| {
        if args.len() != 1 {
            bail!("lower() expects 1 argument");
        }
        Ok(Value::String(val_to_str(&args[0]).to_lowercase()))
    });

    reg.register("upper", |args, _event| {
        if args.len() != 1 {
            bail!("upper() expects 1 argument");
        }
        Ok(Value::String(val_to_str(&args[0]).to_uppercase()))
    });

    reg.register("regex_match", |args, _event| {
        if args.len() != 2 {
            bail!("regex_match() expects 2 arguments (target, pattern)");
        }
        let target = val_to_str(&args[0]);
        let pattern = val_to_str(&args[1]);
        match get_cached_regex(&pattern) {
            Ok(re) => Ok(Value::Bool(re.is_match(&target))),
            Err(e) => bail!("invalid regex: {}", e),
        }
    });

    reg.register("regex_extract", |args, _event| {
        if args.len() != 2 {
            bail!("regex_extract() expects 2 arguments");
        }
        let target = val_to_str(&args[0]);
        let pattern = val_to_str(&args[1]);
        match get_cached_regex(&pattern) {
            Ok(re) => {
                if let Some(caps) = re.captures(&target) {
                    if let Some(m) = caps.get(1) {
                        Ok(Value::String(m.as_str().to_string()))
                    } else if let Some(m) = caps.get(0) {
                        Ok(Value::String(m.as_str().to_string()))
                    } else {
                        Ok(Value::Null)
                    }
                } else {
                    Ok(Value::Null)
                }
            }
            Err(e) => bail!("invalid regex: {}", e),
        }
    });

    reg.register("to_json", |args, event| {
        if args.is_empty() {
            Ok(Value::String(event.to_json_string()))
        } else if args.len() == 1 {
            Ok(Value::String(serde_json::to_string(&args[0])?))
        } else {
            bail!("to_json() expects 0 or 1 argument");
        }
    });

    {
        let store = table_store.clone();
        reg.register("table_lookup", move |args, _event| {
            if args.len() != 2 {
                bail!("table_lookup() expects 2 arguments (table, key)");
            }
            let table_name = val_to_str(&args[0]);
            let key = val_to_str(&args[1]);
            Ok(store.lookup(&table_name, &key))
        });
    }

    {
        let store = table_store.clone();
        reg.register("table_upsert", move |args, _event| {
            if args.len() < 3 || args.len() > 4 {
                bail!("table_upsert() expects 3 or 4 arguments (table, key, value, expire?)");
            }
            let table_name = val_to_str(&args[0]);
            let key = val_to_str(&args[1]);
            let value = args[2].clone();
            if args.len() == 3 {
                store.upsert_with_default(&table_name, &key, value);
            } else {
                let secs = match &args[3] {
                    Value::Number(n) => n.as_u64(),
                    other => {
                        tracing::warn!("table_upsert: expire must be a number, got {} — using table default TTL", other);
                        None
                    }
                };
                match secs {
                    Some(0) => store.upsert(&table_name, &key, value, None), // 0 = no expiry
                    Some(s) => store.upsert(&table_name, &key, value, Some(std::time::Duration::from_secs(s))),
                    None => store.upsert_with_default(&table_name, &key, value), // fallback to default TTL
                };
            }
            Ok(Value::Null)
        });
    }

    {
        let store = table_store;
        reg.register("table_delete", move |args, _event| {
            if args.len() != 2 {
                bail!("table_delete() expects 2 arguments (table, key)");
            }
            let table_name = val_to_str(&args[0]);
            let key = val_to_str(&args[1]);
            store.delete(&table_name, &key);
            Ok(Value::Null)
        });
    }

    reg.register("geoip", |args, _event| {
        if args.len() != 1 {
            bail!("geoip() expects 1 argument (IP address string)");
        }
        let ip_str = val_to_str(&args[0]);
        geoip::lookup(&ip_str)
    });

    reg.register("md5", |args, _event| {
        if args.len() != 1 {
            bail!("md5() expects 1 argument");
        }
        use digest::Digest;
        let input = val_to_str(&args[0]);
        let hash = md5::Md5::digest(input.as_bytes());
        Ok(Value::String(format!("{:x}", hash)))
    });

    reg.register("sha1", |args, _event| {
        if args.len() != 1 {
            bail!("sha1() expects 1 argument");
        }
        use digest::Digest;
        let input = val_to_str(&args[0]);
        let hash = sha1::Sha1::digest(input.as_bytes());
        Ok(Value::String(format!("{:x}", hash)))
    });

    reg.register("sha256", |args, _event| {
        if args.len() != 1 {
            bail!("sha256() expects 1 argument");
        }
        use digest::Digest;
        let input = val_to_str(&args[0]);
        let hash = sha2::Sha256::digest(input.as_bytes());
        Ok(Value::String(format!("{:x}", hash)))
    });

    reg.register("regex_replace", |args, _event| {
        if args.len() != 3 {
            bail!("regex_replace() expects 3 arguments (target, pattern, replacement)");
        }
        let target = val_to_str(&args[0]);
        let pattern = val_to_str(&args[1]);
        let replacement = val_to_str(&args[2]);
        match get_cached_regex(&pattern) {
            Ok(re) => Ok(Value::String(
                re.replace_all(&target, replacement.as_str()).into_owned(),
            )),
            Err(e) => bail!("invalid regex: {}", e),
        }
    });

    reg.register("format", |args, event| {
        if args.len() != 1 {
            bail!("format() expects 1 argument (template string)");
        }
        let template = val_to_str(&args[0]);
        Ok(Value::String(expand_format_template(&template, event)?))
    });

    reg.register("strftime", |args, _event| {
        // strftime(value, fmt)           — format in value's own timezone
        // strftime(value, fmt, "local")  — convert to local time, then format
        // strftime(value, fmt, "UTC")    — convert to UTC, then format
        // strftime(value, fmt, "+09:00") — convert to fixed offset, then format
        if !(args.len() == 2 || args.len() == 3) {
            bail!("strftime() expects 2 or 3 arguments (value, format[, timezone])");
        }
        let value = val_to_str(&args[0]);
        let fmt = val_to_str(&args[1]);
        let tz = if args.len() == 3 {
            Some(val_to_str(&args[2]))
        } else {
            None
        };

        // Parse value as RFC3339 (Event::timestamp serialises this way, as
        // does `now()`). Treat any parse failure as a loud error — silently
        // producing an empty string on bad input would violate the
        // zero-hidden-behaviour principle.
        let dt = chrono::DateTime::parse_from_rfc3339(&value).map_err(|e| {
            anyhow::anyhow!("strftime(): invalid RFC3339 timestamp '{}': {}", value, e)
        })?;

        let formatted = match tz.as_deref() {
            None => dt.format(&fmt).to_string(),
            Some("local") => dt.with_timezone(&chrono::Local).format(&fmt).to_string(),
            Some("UTC") | Some("utc") => dt.with_timezone(&chrono::Utc).format(&fmt).to_string(),
            Some(offset) => {
                let fixed = parse_fixed_offset(offset).ok_or_else(|| {
                    anyhow::anyhow!(
                        "strftime(): invalid timezone '{}' (expected 'local', 'UTC', or ±HH:MM)",
                        offset
                    )
                })?;
                dt.with_timezone(&fixed).format(&fmt).to_string()
            }
        };

        Ok(Value::String(formatted))
    });
}

/// Parse `+HH:MM` / `-HH:MM` (or `+HHMM`) into a `FixedOffset`.
fn parse_fixed_offset(s: &str) -> Option<chrono::FixedOffset> {
    let bytes = s.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let rest = &s[1..];
    let (h_str, m_str) = if let Some((h, m)) = rest.split_once(':') {
        (h, m)
    } else if rest.len() == 4 {
        rest.split_at(2)
    } else {
        return None;
    };
    let h: i32 = h_str.parse().ok()?;
    let m: i32 = m_str.parse().ok()?;
    let secs = sign * (h * 3600 + m * 60);
    chrono::FixedOffset::east_opt(secs)
}

/// Expand `%{name}` placeholders in a format template against an event.
///
/// Supported placeholders (explicit only — no bare-name shorthand):
/// - `%{source}`, `%{facility}`, `%{severity}`, `%{timestamp}`
/// - `%{egress}`, `%{ingress}`
/// - `%{workspace.xxx}`, `%{workspace.xxx.yyy}` (nested workspace access)
///
/// A bare `%{pid}` used to be shorthand for `%{workspace.pid}`; that
/// quietly papered over typos (a misspelled key rendered as an empty
/// string) and conflicted with the `let` binding introduced alongside
/// the workspace rename. The shorthand is now an error and the user is
/// pointed at the explicit `%{workspace.xxx}` form.
fn expand_format_template(template: &str, event: &crate::event::Event) -> Result<String> {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var.push(c);
            }
            result.push_str(&resolve_format_var(&var, event)?);
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

fn resolve_format_var(var: &str, event: &crate::event::Event) -> Result<String> {
    match var {
        "source" => Ok(event.source.ip().to_string()),
        "facility" => Ok(event.facility.map(|f| f.to_string()).unwrap_or_default()),
        "severity" => Ok(event.severity.map(|s| s.to_string()).unwrap_or_default()),
        "timestamp" => Ok(event.timestamp.to_rfc3339()),
        "egress" => Ok(String::from_utf8_lossy(&event.egress).into_owned()),
        "ingress" => Ok(String::from_utf8_lossy(&event.ingress).into_owned()),
        v if v.starts_with("workspace.") => {
            let path: Vec<&str> = v["workspace.".len()..].split('.').collect();
            Ok(resolve_format_workspace(&path, &event.workspace))
        }
        // Anything else is a user error. The old shorthand silently did
        // a workspace lookup here; we now refuse and point at the
        // explicit form so typos don't turn into empty strings.
        other => bail!(
            "format(): unknown placeholder `%{{{}}}`; use `%{{workspace.{}}}` or one of the event-level names (source / facility / severity / timestamp / ingress / egress)",
            other,
            other
        ),
    }
}

fn resolve_format_workspace(
    path: &[&str],
    workspace: &std::collections::HashMap<String, serde_json::Value>,
) -> String {
    let first = match workspace.get(path[0]) {
        Some(v) => v,
        None => return String::new(),
    };

    let mut current = first;
    for &segment in &path[1..] {
        match current {
            serde_json::Value::Object(map) => {
                current = match map.get(segment) {
                    Some(v) => v,
                    None => return String::new(),
                };
            }
            _ => return String::new(),
        }
    }

    match current {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::table::TableStore;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn make_registry() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut reg, table_store);
        reg
    }

    fn dummy_event() -> Event {
        Event::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    #[test]
    fn strftime_formats_rfc3339_input() {
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+00:00".into()),
                    Value::String("%Y/%m/%d %H:%M:%S".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("2026/04/19 10:30:45".into()));
    }

    #[test]
    fn strftime_bsd_syslog_format() {
        // Reproduce the old `prepend_timestamp` default format.
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(
                "strftime",
                &[
                    Value::String("2026-04-19T05:07:09+00:00".into()),
                    Value::String("%b %e %H:%M:%S".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("Apr 19 05:07:09".into()));
    }

    #[test]
    fn strftime_utc_timezone() {
        let reg = make_registry();
        let e = dummy_event();
        // Input is +09:00; force to UTC.
        let result = reg
            .call(
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+09:00".into()),
                    Value::String("%Y-%m-%dT%H:%M:%S%z".into()),
                    Value::String("UTC".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("2026-04-19T01:30:45+0000".into()));
    }

    #[test]
    fn strftime_fixed_offset() {
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+00:00".into()),
                    Value::String("%H:%M".into()),
                    Value::String("+09:00".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("19:30".into()));
    }

    #[test]
    fn strftime_rejects_invalid_rfc3339() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                "strftime",
                &[
                    Value::String("not-a-timestamp".into()),
                    Value::String("%Y".into()),
                ],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid RFC3339"));
    }

    #[test]
    fn strftime_rejects_bad_timezone() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+00:00".into()),
                    Value::String("%Y".into()),
                    Value::String("bogus".into()),
                ],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid timezone"));
    }

    #[test]
    fn strftime_rejects_wrong_arity() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                "strftime",
                &[Value::String("2026-04-19T10:30:45+00:00".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("2 or 3 arguments"));
    }

    #[test]
    fn parse_fixed_offset_variants() {
        assert_eq!(
            parse_fixed_offset("+09:00").map(|o| o.local_minus_utc()),
            Some(9 * 3600)
        );
        assert_eq!(
            parse_fixed_offset("-05:30").map(|o| o.local_minus_utc()),
            Some(-(5 * 3600 + 30 * 60))
        );
        assert_eq!(
            parse_fixed_offset("+0900").map(|o| o.local_minus_utc()),
            Some(9 * 3600)
        );
        assert!(parse_fixed_offset("UTC").is_none());
        assert!(parse_fixed_offset("").is_none());
        assert!(parse_fixed_offset("09:00").is_none()); // missing sign
    }

    // ---- format() ---------------------------------------------------------

    #[test]
    fn format_expands_event_level_placeholders() {
        let reg = make_registry();
        let mut e = dummy_event();
        e.severity = Some(3);
        e.facility = Some(16);
        let result = reg
            .call(
                "format",
                &[Value::String("[%{severity}] %{egress}".into())],
                &e,
            )
            .unwrap();
        // egress defaults to the raw bytes ("test") in dummy_event
        assert_eq!(result, Value::String("[3] test".into()));
    }

    #[test]
    fn format_expands_explicit_workspace_placeholder() {
        let reg = make_registry();
        let mut e = dummy_event();
        e.workspace
            .insert("host".into(), serde_json::Value::String("web01".into()));
        let result = reg
            .call(
                "format",
                &[Value::String("host=%{workspace.host}".into())],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("host=web01".into()));
    }

    #[test]
    fn format_rejects_bare_shorthand() {
        // `%{pid}` used to silently fall back to workspace.pid. Now it
        // must be an error so typos don't become empty strings.
        let reg = make_registry();
        let mut e = dummy_event();
        e.workspace
            .insert("pid".into(), serde_json::Value::String("42".into()));
        let err = reg
            .call("format", &[Value::String("pid=%{pid}".into())], &e)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown placeholder"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains("workspace.pid"),
            "error should suggest `workspace.pid`, got: {}",
            msg
        );
    }

    #[test]
    fn format_error_suggests_explicit_form_for_typos() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                "format",
                &[Value::String("x=%{nope_not_a_thing}".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("workspace.nope_not_a_thing"));
    }
}
