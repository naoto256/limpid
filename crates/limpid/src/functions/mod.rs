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
        Ok(Value::String(expand_format_template(&template, event)))
    });
}

/// Expand `%{name}` placeholders in a format template against an event.
///
/// Supported placeholders:
/// - `%{source}`, `%{facility}`, `%{severity}`, `%{timestamp}`
/// - `%{message}`, `%{raw}`
/// - `%{fields.xxx}`, `%{fields.xxx.yyy}` (nested field access)
fn expand_format_template(template: &str, event: &crate::event::Event) -> String {
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
            result.push_str(&resolve_format_var(&var, event));
        } else {
            result.push(ch);
        }
    }

    result
}

fn resolve_format_var(var: &str, event: &crate::event::Event) -> String {
    match var {
        "source" => event.source.ip().to_string(),
        "facility" => event.facility.map(|f| f.to_string()).unwrap_or_default(),
        "severity" => event.severity.map(|s| s.to_string()).unwrap_or_default(),
        "timestamp" => event.timestamp.to_rfc3339(),
        "message" => String::from_utf8_lossy(&event.message).into_owned(),
        "raw" => String::from_utf8_lossy(&event.raw).into_owned(),
        v if v.starts_with("fields.") => {
            let path: Vec<&str> = v["fields.".len()..].split('.').collect();
            resolve_format_fields(&path, &event.fields)
        }
        // Also try direct field name as shorthand for fields.xxx
        v => {
            if let Some(val) = event.fields.get(v) {
                match val {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Null => String::new(),
                    other => other.to_string(),
                }
            } else {
                String::new()
            }
        }
    }
}

fn resolve_format_fields(
    path: &[&str],
    fields: &std::collections::HashMap<String, serde_json::Value>,
) -> String {
    let first = match fields.get(path[0]) {
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
