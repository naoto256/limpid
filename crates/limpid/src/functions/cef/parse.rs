//! `cef.parse(text[, defaults])` — Common Event Format parser.
//!
//! CEF messages look like:
//!
//! ```text
//! CEF:Version|Device Vendor|Device Product|Device Version|Signature ID|Name|Severity|Extensions
//! ```
//!
//! An optional syslog header (`<PRI>…`) is tolerated — the parser
//! locates `CEF:` anywhere in the input and parses from there.
//!
//! Emitted keys (always prefixed with `cef_` for the header fields so
//! workspace dumps stay self-describing; extension keys are copied
//! as-is since CEF-defined keys like `src`, `dst`, `act` are themselves
//! part of the CEF spec):
//!
//! | key                        | meaning                      |
//! |----------------------------|------------------------------|
//! | `cef_version`              | CEF version (usually `0`)    |
//! | `cef_device_vendor`        | device vendor                |
//! | `cef_device_product`       | device product               |
//! | `cef_device_version`       | device version               |
//! | `cef_signature_id`         | vendor-specific event id     |
//! | `cef_name`                 | human-readable event name    |
//! | `cef_severity`             | vendor severity (0-10)       |
//! | `<ext>` (e.g. `src`, `dst`)| CEF extension key=value pairs|

use anyhow::{Result, bail};
use crate::dsl::value::Map;
use crate::dsl::value::Value;

use crate::functions::primitives::parse_json::{apply_defaults, type_name};
use crate::functions::primitives::val_to_str;
use crate::functions::{FunctionRegistry, ParserInfo};
use crate::modules::schema::{FieldSpec, FieldType};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in("cef", "parse", |args, _event| parse_impl(args));
    // CEF header keys are statically known; the extension tail (`src=`
    // / `dst=` / vendor-specific) is data-driven, so `wildcards = true`.
    // `cef_severity` is emitted as Int when the field is well-formed
    // and falls back to String on garbage input — modelled as a Union
    // so type checks of `workspace.cef_severity == 5` stay silent while
    // `== "5"` warns.
    reg.register_parser(ParserInfo {
        namespace: Some("cef"),
        name: "parse",
        produces: vec![
            FieldSpec::new(&["workspace", "cef_version"], FieldType::String),
            FieldSpec::new(&["workspace", "cef_device_vendor"], FieldType::String),
            FieldSpec::new(&["workspace", "cef_device_product"], FieldType::String),
            FieldSpec::new(&["workspace", "cef_device_version"], FieldType::String),
            FieldSpec::new(&["workspace", "cef_signature_id"], FieldType::String),
            FieldSpec::new(&["workspace", "cef_name"], FieldType::String),
            FieldSpec::new(
                &["workspace", "cef_severity"],
                FieldType::Union(vec![FieldType::Int, FieldType::String]),
            ),
        ],
        wildcards: true,
    });
}

fn parse_impl(args: &[Value]) -> Result<Value> {
    // Arity is validated by the registry via the sig installed from
    // `register_parser` (1 to 2 arguments).
    let text = val_to_str(&args[0])?;

    let cef_start = text
        .find("CEF:")
        .ok_or_else(|| anyhow::anyhow!("cef.parse(): no CEF header found"))?;
    let body = &text[cef_start + 4..];

    let mut parts = Vec::new();
    let mut remaining = body;
    for _ in 0..7 {
        if let Some(pos) = remaining.find('|') {
            parts.push(&remaining[..pos]);
            remaining = &remaining[pos + 1..];
        } else {
            bail!("cef.parse(): incomplete CEF header");
        }
    }

    let mut map = Map::new();
    map.insert("cef_version".into(), Value::String(parts[0].to_string()));
    map.insert(
        "cef_device_vendor".into(),
        Value::String(parts[1].to_string()),
    );
    map.insert(
        "cef_device_product".into(),
        Value::String(parts[2].to_string()),
    );
    map.insert(
        "cef_device_version".into(),
        Value::String(parts[3].to_string()),
    );
    map.insert(
        "cef_signature_id".into(),
        Value::String(parts[4].to_string()),
    );
    map.insert("cef_name".into(), Value::String(parts[5].to_string()));
    // CEF Severity is a number (0-10 per the spec). Emit as Int when
    // the field parses cleanly; fall back to the raw string when the
    // producer sent garbage so existing pipelines don't break, and the
    // analyzer-side schema is `Union(Int, String)` to match.
    let severity_value = parts[6]
        .parse::<i64>()
        .map(Value::Int)
        .unwrap_or_else(|_| Value::String(parts[6].to_string()));
    map.insert("cef_severity".into(), severity_value);

    parse_cef_extensions(remaining, &mut map);

    if let Some(v) = args.get(1) {
        match v {
            Value::Object(_) | Value::Null => apply_defaults("cef.parse", Some(v), &mut map)?,
            other => bail!(
                "cef.parse(): second argument must be a hash literal, got {}",
                type_name(other)
            ),
        }
    }

    Ok(Value::Object(map))
}

fn parse_cef_extensions(extensions: &str, map: &mut Map) {
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
        map.insert(key.clone(), Value::String(value.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
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
    fn parses_basic_cef() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String(
                    "CEF:0|Fortinet|FortiGate|7.0|1234|Firewall event|5|src=10.0.0.1 dst=10.0.0.2 act=deny".into(),
                )],
                &e,
            )
            .unwrap();
        let Value::Object(m) = r else { panic!() };
        assert_eq!(m["cef_version"], Value::String("0".into()));
        assert_eq!(m["cef_device_vendor"], Value::String("Fortinet".into()));
        assert_eq!(m["cef_device_product"], Value::String("FortiGate".into()));
        assert_eq!(m["cef_signature_id"], Value::String("1234".into()));
        assert_eq!(m["cef_severity"], Value::Int(5));
        assert_eq!(m["src"], Value::String("10.0.0.1".into()));
        assert_eq!(m["dst"], Value::String("10.0.0.2".into()));
        assert_eq!(m["act"], Value::String("deny".into()));
    }

    #[test]
    fn severity_falls_back_to_string_on_garbage() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String("CEF:0|V|P|1|1|N|High|".into())],
                &e,
            )
            .unwrap();
        let Value::Object(m) = r else { panic!() };
        assert_eq!(m["cef_severity"], Value::String("High".into()));
    }

    #[test]
    fn tolerates_syslog_prefix() {
        let reg = make_reg();
        let e = dummy_event();
        let r = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String(
                    "<134>CEF:0|Security|IDS|1.0|100|Attack|8|src=192.168.1.1".into(),
                )],
                &e,
            )
            .unwrap();
        let Value::Object(m) = r else { panic!() };
        assert_eq!(m["cef_device_vendor"], Value::String("Security".into()));
        assert_eq!(m["src"], Value::String("192.168.1.1".into()));
    }

    #[test]
    fn errors_on_missing_header() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String("not a CEF message".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("no CEF header"));
    }

    #[test]
    fn errors_on_incomplete_header() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String("CEF:0|only|two".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("incomplete CEF header"));
    }

    #[test]
    fn defaults_fill_missing_keys() {
        let reg = make_reg();
        let e = dummy_event();
        let defaults = Value::Object(
            [("act".to_string(), Value::String("unknown".into()))]
                .into_iter()
                .collect(),
        );
        let r = reg
            .call(
                Some("cef"),
                "parse",
                &[
                    Value::String("CEF:0|V|P|1|id|name|3|src=1.1.1.1".into()),
                    defaults,
                ],
                &e,
            )
            .unwrap();
        let Value::Object(m) = r else { panic!() };
        assert_eq!(m["act"], Value::String("unknown".into()));
    }
}
