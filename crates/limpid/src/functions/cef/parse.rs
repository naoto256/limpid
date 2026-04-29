//! `cef.parse(text[, defaults])` — Common Event Format parser.
//!
//! CEF messages look like:
//!
//! ```text
//! CEF:Version|Device Vendor|Device Product|Device Version|Signature ID|Name|Severity|Extensions
//! ```
//!
//! The input must start with `CEF:` — syslog wrapper handling is the
//! caller's responsibility.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};
use anyhow::{Result, bail};

use crate::functions::primitives::parse_json::{apply_defaults, type_name};
use crate::functions::primitives::val_to_str;
use crate::functions::{FunctionRegistry, ParserInfo};
use crate::modules::schema::{FieldSpec, FieldType};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in("cef", "parse", |arena, args, _event| {
        parse_impl(arena, args)
    });
    reg.register_parser(ParserInfo {
        namespace: Some("cef"),
        name: "parse",
        produces: vec![
            FieldSpec::new(&["workspace", "version"], FieldType::String),
            FieldSpec::new(&["workspace", "device_vendor"], FieldType::String),
            FieldSpec::new(&["workspace", "device_product"], FieldType::String),
            FieldSpec::new(&["workspace", "device_version"], FieldType::String),
            FieldSpec::new(&["workspace", "signature_id"], FieldType::String),
            FieldSpec::new(&["workspace", "name"], FieldType::String),
            FieldSpec::new(
                &["workspace", "severity"],
                FieldType::Union(vec![FieldType::Int, FieldType::String]),
            ),
        ],
        wildcards: true,
    });
}

fn parse_impl<'bump>(
    arena: &'bump EventArena<'bump>,
    args: &[Value<'bump>],
) -> Result<Value<'bump>> {
    let text = val_to_str(&args[0])?;

    let body = text
        .strip_prefix("CEF:")
        .ok_or_else(|| anyhow::anyhow!("cef.parse(): input does not start with `CEF:`"))?;

    let mut parts: [&str; 7] = [""; 7];
    let mut remaining = body;
    for slot in parts.iter_mut() {
        if let Some(pos) = remaining.find('|') {
            *slot = &remaining[..pos];
            remaining = &remaining[pos + 1..];
        } else {
            bail!("cef.parse(): incomplete CEF header");
        }
    }

    let mut builder = ObjectBuilder::new(arena);
    builder.push("version", Value::String(arena.alloc_str(parts[0])));
    builder.push("device_vendor", Value::String(arena.alloc_str(parts[1])));
    builder.push("device_product", Value::String(arena.alloc_str(parts[2])));
    builder.push("device_version", Value::String(arena.alloc_str(parts[3])));
    builder.push("signature_id", Value::String(arena.alloc_str(parts[4])));
    builder.push("name", Value::String(arena.alloc_str(parts[5])));
    let severity_value = parts[6]
        .parse::<i64>()
        .map(Value::Int)
        .unwrap_or_else(|_| Value::String(arena.alloc_str(parts[6])));
    builder.push("severity", severity_value);

    parse_cef_extensions(arena, remaining, &mut builder);

    let parsed = builder.finish();

    if let Some(v) = args.get(1) {
        match v {
            Value::Object(_) | Value::Null => apply_defaults(arena, "cef.parse", Some(v), parsed),
            other => bail!(
                "cef.parse(): second argument must be a hash literal, got {}",
                type_name(other)
            ),
        }
    } else {
        Ok(parsed)
    }
}

fn parse_cef_extensions<'bump>(
    arena: &EventArena<'bump>,
    extensions: &str,
    builder: &mut ObjectBuilder<'bump>,
) {
    if extensions.is_empty() {
        return;
    }
    let bytes = extensions.as_bytes();
    let mut key_positions: Vec<(String, usize)> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let key_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' && i > key_start {
            let key = &extensions[key_start..i];
            i += 1;
            key_positions.push((key.to_string(), i));
            while i < bytes.len() {
                if bytes[i] == b' ' {
                    let lookahead = i + 1;
                    let mut j = lookahead;
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                    {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'=' && j > lookahead {
                        break;
                    }
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }

    for idx in 0..key_positions.len() {
        let (ref key, val_start) = key_positions[idx];
        let val_end = if idx + 1 < key_positions.len() {
            let next_val_start = key_positions[idx + 1].1;
            let next_key_len = key_positions[idx + 1].0.len();
            next_val_start
                .saturating_sub(next_key_len + 2)
                .max(val_start)
        } else {
            extensions.len()
        };
        let value = extensions[val_start..val_end].trim();
        builder.push_str(key, Value::String(arena.alloc_str(value)));
    }
}
