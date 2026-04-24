//! `parse_kv(text[, defaults])` — key=value pair parser primitive.
//!
//! Handles common `key=value` syslog payloads used by FortiGate, Palo
//! Alto, etc. Supports bare `key=value`, quoted `key="value with spaces"`,
//! and unquoted values containing punctuation other than space. Returns
//! the parsed pairs as a `Value::Object` so a bare `parse_kv(egress)`
//! statement merges them into the event workspace.
//!
//! KV is a *format*, not a schema — flat primitive namespace.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use super::parse_json::{apply_defaults, type_name};
use super::val_to_str;
use crate::functions::{FunctionRegistry, ParserInfo};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("parse_kv", |args, _event| parse_kv_impl(args));
    reg.register_parser(ParserInfo {
        namespace: None,
        name: "parse_kv",
        produces: Vec::new(),
        wildcards: true,
    });
}

fn parse_kv_impl(args: &[Value]) -> Result<Value> {
    // Arity is validated centrally by the registry (register_parser installs
    // the `(String, Object?) -> Object` signature). No manual check here.
    let text = val_to_str(&args[0]);
    let mut map = Map::new();
    for (k, v) in parse_kv_pairs(&text) {
        map.insert(k, Value::String(v));
    }
    // `apply_defaults` handles Object / Null / error — shared with parse_json.
    // Use a local wrapper so the error message says `parse_kv`.
    if let Some(v) = args.get(1) {
        match v {
            Value::Object(_) | Value::Null => apply_defaults("parse_kv", Some(v), &mut map)?,
            other => bail!(
                "parse_kv(): second argument must be a hash literal, got {}",
                type_name(other)
            ),
        }
    }
    Ok(Value::Object(map))
}

/// Safe substring from byte offsets, handling potential non-UTF-8 boundaries.
fn safe_slice(input: &str, start: usize, end: usize) -> &str {
    let s = start.min(input.len());
    let e = end.min(input.len());
    let s = (s..=e).find(|&i| input.is_char_boundary(i)).unwrap_or(e);
    let e = (s..=e).rfind(|&i| input.is_char_boundary(i)).unwrap_or(s);
    &input[s..e]
}

fn parse_kv_pairs(input: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        while i < len && bytes[i] == b' ' {
            i += 1;
        }
        if i >= len {
            break;
        }

        let key_start = i;
        while i < len && bytes[i] != b'=' && bytes[i] != b' ' {
            i += 1;
        }

        if i >= len || bytes[i] != b'=' {
            while i < len && bytes[i] != b' ' {
                i += 1;
            }
            continue;
        }

        let key = safe_slice(input, key_start, i);
        i += 1; // skip '='

        if i >= len {
            pairs.push((key.to_string(), String::new()));
            break;
        }

        let value = if bytes[i] == b'"' {
            i += 1;
            let val_start = i;
            while i < len && bytes[i] != b'"' {
                if bytes[i] == b'\\' && i + 1 < len {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            let val = safe_slice(input, val_start, i);
            if i < len {
                i += 1;
            }
            val.to_string()
        } else {
            let val_start = i;
            while i < len && bytes[i] != b' ' {
                i += 1;
            }
            safe_slice(input, val_start, i).to_string()
        };

        if !key.is_empty() {
            pairs.push((key.to_string(), value));
        }
    }

    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::functions::FunctionRegistry;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn dummy_event() -> Event {
        Event::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn make_reg() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        reg
    }

    #[test]
    fn basic_pairs() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "parse_kv",
                &[Value::String("src=10.0.0.1 dst=1.2.3.4 act=deny".into())],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["src"], Value::String("10.0.0.1".into()));
        assert_eq!(m["dst"], Value::String("1.2.3.4".into()));
        assert_eq!(m["act"], Value::String("deny".into()));
    }

    #[test]
    fn quoted_values() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "parse_kv",
                &[Value::String(r#"msg="login failed" user=admin"#.into())],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["msg"], Value::String("login failed".into()));
        assert_eq!(m["user"], Value::String("admin".into()));
    }

    #[test]
    fn non_kv_tokens_skipped() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "parse_kv",
                &[Value::String(
                    "junk src=10.0.0.1 more_junk dst=5.6.7.8".into(),
                )],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["src"], Value::String("10.0.0.1".into()));
        assert_eq!(m["dst"], Value::String("5.6.7.8".into()));
        assert!(!m.contains_key("junk"));
    }

    #[test]
    fn defaults_fill_missing() {
        let reg = make_reg();
        let e = dummy_event();
        let defaults = Value::Object(
            [("src".to_string(), Value::String("0.0.0.0".into()))]
                .into_iter()
                .collect(),
        );
        let result = reg
            .call(
                None,
                "parse_kv",
                &[Value::String("dst=1.2.3.4".into()), defaults],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["src"], Value::String("0.0.0.0".into()));
        assert_eq!(m["dst"], Value::String("1.2.3.4".into()));
    }
}
