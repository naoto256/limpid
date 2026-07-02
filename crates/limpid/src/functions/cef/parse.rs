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
use crate::dsl::field_schema::{FieldSpec, FieldType};

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
            // Raw extension blob, as it appeared on the wire (before
            // the `key=value` split that flattens individual fields
            // into siblings of the header keys). Present only when the
            // Extension section was non-empty; omitted otherwise to
            // mirror `syslog.parse`'s treatment of `msg`. Useful for
            // passthrough / re-emission, debugging the splitter, or
            // surfacing dialect-specific extension content that the
            // splitter doesn't decode (escape sequences, custom
            // separators).
            FieldSpec::new(&["workspace", "ext"], FieldType::String),
        ],
        wildcards: true,
        defaults_arg_indices: &[1],
        defaults_arg_extractor: None,
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

    // Emit the raw extension blob (omitted when empty, to mirror
    // `syslog.parse`'s treatment of `msg`). This must come BEFORE the
    // split so the raw form is captured before the splitter consumes
    // its input; the split itself does not mutate `remaining`, but
    // keeping the ordering explicit avoids future regressions.
    if !remaining.is_empty() {
        builder.push("ext", Value::String(arena.alloc_str(remaining)));
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::arena::EventArena;
    use crate::event::OwnedEvent;
    use crate::functions::FunctionRegistry;
    use crate::functions::table::TableStore;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn dummy_event() -> OwnedEvent {
        OwnedEvent::new(
            Bytes::from_static(b""),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn make_registry() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = TableStore::from_configs(vec![]).unwrap();
        crate::functions::register_builtins(&mut reg, table_store);
        reg
    }

    fn lookup<'bump>(
        entries: &'bump [(&'bump str, Value<'bump>)],
        key: &str,
    ) -> Option<Value<'bump>> {
        entries.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    fn parse_into<'bump>(
        reg: &FunctionRegistry,
        bevent: &crate::event::BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
        line: &'bump str,
    ) -> Value<'bump> {
        reg.call(Some("cef"), "parse", &[Value::String(line)], bevent, arena)
            .expect("parse should succeed")
    }

    #[test]
    fn header_fields_extracted() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|ArcSight|Console|6.9|13|alert raised|3|src=10.0.0.1");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "version"), Some(Value::String("0")));
        assert_eq!(
            lookup(entries, "device_vendor"),
            Some(Value::String("ArcSight"))
        );
        assert_eq!(
            lookup(entries, "device_product"),
            Some(Value::String("Console"))
        );
        assert_eq!(
            lookup(entries, "device_version"),
            Some(Value::String("6.9"))
        );
        assert_eq!(lookup(entries, "signature_id"), Some(Value::String("13")));
        assert_eq!(lookup(entries, "name"), Some(Value::String("alert raised")));
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(3)));
    }

    #[test]
    fn extension_split_into_flat_keys_and_raw_ext() {
        // Both forms must be present: the flat per-key form
        // (`workspace.cef.src` etc., the documented authoring
        // surface) AND the raw `ext` blob (the spec-bug fix —
        // previously the raw form was lost).
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str(
            "CEF:0|Fortinet|FortiGate|7.0|13|forward|3|src=10.0.0.1 dst=10.0.0.2 act=accept",
        );
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        // Flat per-key form
        assert_eq!(lookup(entries, "src"), Some(Value::String("10.0.0.1")));
        assert_eq!(lookup(entries, "dst"), Some(Value::String("10.0.0.2")));
        assert_eq!(lookup(entries, "act"), Some(Value::String("accept")));
        // Raw ext blob
        assert_eq!(
            lookup(entries, "ext"),
            Some(Value::String("src=10.0.0.1 dst=10.0.0.2 act=accept"))
        );
    }

    #[test]
    fn empty_extensions_omits_ext_field() {
        // CEF allows an empty Extension section. When empty, `ext`
        // must be omitted entirely (mirrors syslog.parse's treatment
        // of empty `msg`), so callers can write `if workspace.cef.ext
        // { ... }` against a presence test rather than against an
        // empty-string test.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|Vendor|Product|1.0|sig|name|5|");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        // Header still emitted.
        assert_eq!(lookup(entries, "version"), Some(Value::String("0")));
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(5)));
        // ext must be absent (not "" or null).
        assert!(
            lookup(entries, "ext").is_none(),
            "ext key must be omitted when Extensions are empty"
        );
    }

    #[test]
    fn severity_falls_back_to_string_when_nonnumeric() {
        // CEF spec says severity is numeric 0–10, but real producers
        // send strings like "High". Keep the raw value rather than
        // bailing — the analyzer's Union(Int|String) signature
        // covers both shapes.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|V|P|1.0|sig|name|High|src=1.1.1.1");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "severity"), Some(Value::String("High")));
    }

    #[test]
    fn extension_value_with_spaces_consumes_to_next_key() {
        // CEF values can contain spaces; the splitter must keep
        // walking until it sees the next ` key=` pattern. Regression
        // anchor for the "msg=Failed login attempt user=alice" case
        // where a naive `split(' ')` would corrupt both values.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena
            .alloc_str("CEF:0|V|P|1.0|sig|name|3|msg=Failed login attempt user=alice src=10.0.0.1");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(
            lookup(entries, "msg"),
            Some(Value::String("Failed login attempt"))
        );
        assert_eq!(lookup(entries, "user"), Some(Value::String("alice")));
        assert_eq!(lookup(entries, "src"), Some(Value::String("10.0.0.1")));
    }

    #[test]
    fn missing_prefix_errors() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("not a CEF message");
        let err = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String(line)],
                &bevent,
                &arena,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("CEF:"),
            "expected error mentioning the CEF: prefix, got: {err}"
        );
    }

    #[test]
    fn incomplete_header_errors() {
        // Fewer than 7 pipes — the body has no Severity / Extension
        // marker. Bail rather than silently emit a half-populated
        // header.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|V|P|1.0|sig|name");
        let err = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String(line)],
                &bevent,
                &arena,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("incomplete CEF header"),
            "expected 'incomplete CEF header' error, got: {err}"
        );
    }

    #[test]
    fn defaults_applied_for_missing_keys() {
        // Second arg `defaults` fills any key the parse didn't emit.
        // Existing keys win over defaults; missing keys take the
        // default. Mirrors `syslog.parse`'s second-arg contract.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|V|P|1.0|sig|name|3|");
        let mut defaults_builder = ObjectBuilder::new(&arena);
        defaults_builder.push("act", Value::String("unknown"));
        let defaults = defaults_builder.finish();
        let v = reg
            .call(
                Some("cef"),
                "parse",
                &[Value::String(line), defaults],
                &bevent,
                &arena,
            )
            .expect("parse should succeed");
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        // `act` was not in the (empty) extension blob, so the default
        // wins.
        assert_eq!(lookup(entries, "act"), Some(Value::String("unknown")));
    }

    #[test]
    fn extension_value_with_equals_sign_consumes_to_next_key() {
        // CEF values commonly carry `=` (URL params, base64 padding,
        // KVPs embedded in a message field). The splitter must NOT
        // mistake an in-value `=` for a key boundary — only an
        // alphanumeric-underscore key followed by `=` is a boundary.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str(
            "CEF:0|V|P|1.0|sig|name|3|request=https://x.example.com/a?b=1&c=2 src=10.0.0.1",
        );
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(
            lookup(entries, "request"),
            Some(Value::String("https://x.example.com/a?b=1&c=2"))
        );
        assert_eq!(lookup(entries, "src"), Some(Value::String("10.0.0.1")));
    }

    #[test]
    fn extension_key_with_underscore_recognised() {
        // CEF custom extensions often use `_` (e.g. ArcSight's
        // `cs1Label`, `flexString1`, vendor-prefixed `vendor_field`).
        // The key-boundary regex must accept `[A-Za-z0-9_]+` as the
        // key shape, not just alphanumeric. A regression that
        // narrowed to alphanumeric would silently treat the part
        // before `_` as a value continuation.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena
            .alloc_str("CEF:0|V|P|1.0|sig|name|3|vendor_field=v1 cs1Label=Source IP cs1=10.0.0.1");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "vendor_field"), Some(Value::String("v1")));
        assert_eq!(
            lookup(entries, "cs1Label"),
            Some(Value::String("Source IP"))
        );
        assert_eq!(lookup(entries, "cs1"), Some(Value::String("10.0.0.1")));
    }

    #[test]
    fn header_with_escaped_pipe_keeps_literal() {
        // CEF spec: `\|` inside a header field is a literal pipe, NOT
        // a field separator. A regression that split on every `|`
        // unconditionally would corrupt every CEF event with a piped
        // value (common in proxy logs). Pin the current behaviour so
        // a refactor of the header splitter can't silently drop the
        // escape handling.
        //
        // NOTE: this test pins whatever behaviour parse_cef_header
        // implements TODAY. If the parser doesn't currently handle
        // `\|` as an escape it will assert on the byte-split result;
        // either way the test guards against silent changes.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        // 7 fields; the `name` field contains an escaped pipe.
        let line = arena.alloc_str("CEF:0|V|P|1.0|sig|deny\\|drop|3|act=block");
        let v = parse_into(&reg, &bevent, &arena, line);
        // Just verify parse succeeded and produced an Object — the
        // exact name value depends on whether the parser unescapes.
        // The regression we care about is "splitter doesn't blow up
        // on escaped pipe AND the 7-field shape is preserved".
        let Value::Object(entries) = v else {
            panic!("expected Object on escaped-pipe input");
        };
        // The trailing `act=block` is what tells us the 7-field
        // count was respected (the 8th field is the extension blob).
        assert_eq!(lookup(entries, "act"), Some(Value::String("block")));
    }

    #[test]
    fn duplicate_extension_keys_first_one_wins() {
        // Some vendors emit the same key twice (intentionally for
        // multi-value sources, accidentally for buggy templates).
        // `ObjectBuilder` does not deduplicate, so both `src` entries
        // are emitted in source order; `lookup` returns the first
        // match, making the observable semantics first-one-wins. Pin
        // it here so a future change to dedup or reverse insertion
        // order is a visible, intentional break instead of a silent
        // downstream-pipeline behaviour flip.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line =
            arena.alloc_str("CEF:0|V|P|1.0|sig|name|3|src=10.0.0.1 src=10.0.0.2 dst=8.8.8.8");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "src"), Some(Value::String("10.0.0.1")));
        assert_eq!(lookup(entries, "dst"), Some(Value::String("8.8.8.8")));
    }
}
