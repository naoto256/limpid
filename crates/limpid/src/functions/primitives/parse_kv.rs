//! `parse_kv(text[, separator][, defaults])` — key=value pair parser
//! primitive.
//!
//! Handles `key=value` payloads used by FortiGate, Palo Alto, Cisco
//! ASA, Microsoft Defender, and similar vendors. Supports bare
//! `key=value`, quoted `key="value with spaces or separator"`, and
//! unquoted values containing punctuation other than the active
//! separator.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};
use crate::modules::schema::FieldType;
use anyhow::{Result, bail};

use super::parse_json::{apply_defaults, type_name};
use super::val_to_str;
use crate::functions::{FunctionRegistry, FunctionSig, ParserInfo};

const DEFAULT_SEP: u8 = b' ';

pub fn register(reg: &mut FunctionRegistry) {
    let sep_or_defaults = FieldType::Union(vec![FieldType::String, FieldType::Object]);
    let sig = FunctionSig::optional(
        &[FieldType::String, sep_or_defaults, FieldType::Object],
        1,
        FieldType::Object,
    );
    reg.register_with_sig("parse_kv", sig, |arena, args, _event| {
        parse_kv_impl(arena, args)
    });
    reg.register_parser(ParserInfo {
        namespace: None,
        name: "parse_kv",
        produces: Vec::new(),
        wildcards: true,
    });
}

fn parse_kv_impl<'bump>(
    arena: &'bump EventArena<'bump>,
    args: &[Value<'bump>],
) -> Result<Value<'bump>> {
    let text = val_to_str(&args[0])?;

    // Args[1] disambiguation: String → separator; Object/Null → defaults.
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

    let pairs = parse_kv_pairs(&text, sep);
    let mut builder = ObjectBuilder::with_capacity(arena, pairs.len());
    for (k, v) in pairs {
        builder.push_str(&k, Value::String(arena.alloc_str(&v)));
    }
    let parsed = builder.finish();
    let with_defaults = match defaults_arg {
        Some(d @ (Value::Object(_) | Value::Null)) => {
            apply_defaults(arena, "parse_kv", Some(d), parsed)?
        }
        Some(other) => bail!(
            "parse_kv(): defaults argument must be a hash literal, got {}",
            type_name(other)
        ),
        None => parsed,
    };
    Ok(with_defaults)
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
