//! `parse_kv(text[, separator][, defaults])` — key=value pair parser
//! primitive.
//!
//! Handles `key=value` payloads used by FortiGate, Palo Alto, Cisco
//! ASA, Microsoft Defender, and similar vendors. Supports bare
//! `key=value`, quoted `key="value with spaces or separator"`, and
//! unquoted values containing punctuation other than the active
//! separator.
//!
//! `separator` is a single ASCII byte (default `' '`). Comma-separated
//! payloads pass `parse_kv(text, ",")`. The optional defaults hash
//! literal fills missing keys, identical to `parse_json` defaults
//! semantics.
//!
//! KV is a *format*, not a schema — flat primitive namespace.

use anyhow::{Result, bail};
use crate::dsl::value::Map;
use crate::dsl::value::Value;
use crate::modules::schema::FieldType;

use super::parse_json::{apply_defaults, type_name};
use super::val_to_str;
use crate::functions::{FunctionRegistry, FunctionSig, ParserInfo};

const DEFAULT_SEP: u8 = b' ';

pub fn register(reg: &mut FunctionRegistry) {
    // Custom signature first so `register_parser` (below) does not
    // overwrite it with the default `(String, Object?) -> Object` shape.
    // We accept up to three args:
    //   parse_kv(text)
    //   parse_kv(text, ",")
    //   parse_kv(text, {defaults})
    //   parse_kv(text, ",", {defaults})
    // Args[1] is dispatched at runtime on type (String → separator,
    // Object/Null → defaults).
    let sep_or_defaults = FieldType::Union(vec![FieldType::String, FieldType::Object]);
    let sig = FunctionSig::optional(
        &[FieldType::String, sep_or_defaults, FieldType::Object],
        1,
        FieldType::Object,
    );
    reg.register_with_sig("parse_kv", sig, |args, _event| parse_kv_impl(args));
    reg.register_parser(ParserInfo {
        namespace: None,
        name: "parse_kv",
        produces: Vec::new(),
        wildcards: true,
    });
}

fn parse_kv_impl(args: &[Value]) -> Result<Value> {
    let text = val_to_str(&args[0])?;

    // Args[1] disambiguation: String → separator; Object/Null → defaults.
    // Args[2] (when present) is always defaults, and args[1] must be the
    // separator string in that case.
    let (sep, defaults_arg) = match (args.get(1), args.get(2)) {
        (None, _) => (DEFAULT_SEP, None),
        (Some(Value::String(s)), Some(d)) => (separator_byte(s)?, Some(d)),
        (Some(Value::String(s)), None) => (separator_byte(s)?, None),
        (Some(d @ (Value::Object(_) | Value::Null)), None) => (DEFAULT_SEP, Some(d)),
        (Some(other), _) => bail!(
            "parse_kv(): second argument must be a separator string or a hash literal, got {}",
            type_name(other)
        ),
    };

    let mut map = Map::new();
    for (k, v) in parse_kv_pairs(&text, sep) {
        map.insert(k, Value::String(v));
    }
    if let Some(d) = defaults_arg {
        match d {
            Value::Object(_) | Value::Null => apply_defaults("parse_kv", Some(d), &mut map)?,
            other => bail!(
                "parse_kv(): defaults argument must be a hash literal, got {}",
                type_name(other)
            ),
        }
    }
    Ok(Value::Object(map))
}

fn separator_byte(s: &str) -> Result<u8> {
    if s.len() != 1 || !s.is_ascii() {
        bail!(
            "parse_kv(): separator must be a single ASCII byte, got {:?}",
            s
        );
    }
    Ok(s.as_bytes()[0])
}

/// Safe substring from byte offsets, handling potential non-UTF-8 boundaries.
fn safe_slice(input: &str, start: usize, end: usize) -> &str {
    let s = start.min(input.len());
    let e = end.min(input.len());
    let s = (s..=e).find(|&i| input.is_char_boundary(i)).unwrap_or(e);
    let e = (s..=e).rfind(|&i| input.is_char_boundary(i)).unwrap_or(s);
    &input[s..e]
}

fn parse_kv_pairs(input: &str, sep: u8) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        while i < len && bytes[i] == sep {
            i += 1;
        }
        if i >= len {
            break;
        }

        let key_start = i;
        while i < len && bytes[i] != b'=' && bytes[i] != sep {
            i += 1;
        }

        if i >= len || bytes[i] != b'=' {
            while i < len && bytes[i] != sep {
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
            while i < len && bytes[i] != sep {
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

    #[test]
    fn comma_separator() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "parse_kv",
                &[
                    Value::String("src=10.0.0.1,dst=1.2.3.4,act=deny".into()),
                    Value::String(",".into()),
                ],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["src"], Value::String("10.0.0.1".into()));
        assert_eq!(m["dst"], Value::String("1.2.3.4".into()));
        assert_eq!(m["act"], Value::String("deny".into()));
    }

    #[test]
    fn comma_separator_with_quoted_value_containing_comma() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "parse_kv",
                &[
                    Value::String(r#"a=1,b="two,three",c=4"#.into()),
                    Value::String(",".into()),
                ],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["a"], Value::String("1".into()));
        assert_eq!(m["b"], Value::String("two,three".into()));
        assert_eq!(m["c"], Value::String("4".into()));
    }

    #[test]
    fn separator_with_defaults() {
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
                &[
                    Value::String("dst=1.2.3.4".into()),
                    Value::String(",".into()),
                    defaults,
                ],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["src"], Value::String("0.0.0.0".into()));
        assert_eq!(m["dst"], Value::String("1.2.3.4".into()));
    }

    #[test]
    fn separator_must_be_single_ascii_byte() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                None,
                "parse_kv",
                &[
                    Value::String("a=1".into()),
                    Value::String(",,".into()),
                ],
                &e,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("single ASCII byte"),
            "got: {}",
            err
        );
    }
}
