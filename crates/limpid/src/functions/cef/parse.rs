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
//!
//! The seven positionally-determined header fields land as direct keys
//! of the result object; the data-driven Extension key=value pairs are
//! isolated under the nested `extension` sub-object (raw blob under
//! `extension_raw`). The two planes carry different trust: header
//! values are fixed by position, extension keys are whatever the
//! producer (or an injected payload) put on the wire, so they must not
//! share a namespace. Before this split, an extension such as
//! `severity=9` or `name=x` was pushed as a duplicate sibling of the
//! header key — arena field reads resolved first-wins (header) but the
//! owned workspace snapshot resolved last-wins, so downstream processes
//! saw the header value silently replaced by the extension value.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};
use anyhow::{Result, bail};

use crate::dsl::field_schema::{FieldSpec, FieldType};
use crate::functions::primitives::parse_json::{apply_defaults, type_name};
use crate::functions::primitives::val_to_str;
use crate::functions::{FunctionRegistry, ParserInfo};

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
            // the `key=value` split). Present only when the Extension
            // section was non-empty; omitted otherwise to mirror
            // `syslog.parse`'s treatment of `msg`. Useful for
            // passthrough / re-emission, debugging the splitter, or
            // surfacing dialect-specific extension content that the
            // splitter doesn't decode (escape sequences, custom
            // separators).
            FieldSpec::new(&["workspace", "extension_raw"], FieldType::String),
            // Split extension key=value pairs, isolated in their own
            // sub-object so data-driven keys can never collide with the
            // positional header fields above. Individual keys are
            // data-driven (wildcards).
            FieldSpec::new(&["workspace", "extension"], FieldType::Object),
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

    // Of the escapes the CEF specification defines, two are generic
    // structural escapes relevant to header field splitting: `\|`
    // (literal pipe) and `\\` (literal backslash). Only an unescaped
    // `|` is a field separator; a naive `find('|')` would split on the
    // escaped form and shift every subsequent header field (name
    // absorbs the fragment, severity receives the next field,
    // extensions receive the severity). The spec's field-specific
    // escaping (`=` / `%` / `#` when carrying vulnerability spellings
    // in deviceEventClassId / name) is field-internal grammar this
    // generic primitive does not interpret — those spellings pass
    // through raw.
    let mut parts: [&str; 7] = [""; 7];
    let mut remaining = body;
    for slot in parts.iter_mut() {
        if let Some((field, rest)) = split_unescaped_pipe(remaining) {
            *slot = field;
            remaining = rest;
        } else {
            bail!("cef.parse(): incomplete CEF header");
        }
    }

    let mut builder = ObjectBuilder::new(arena);
    builder.push("version", unescape_header_field(arena, parts[0]));
    builder.push("device_vendor", unescape_header_field(arena, parts[1]));
    builder.push("device_product", unescape_header_field(arena, parts[2]));
    builder.push("device_version", unescape_header_field(arena, parts[3]));
    builder.push("signature_id", unescape_header_field(arena, parts[4]));
    builder.push("name", unescape_header_field(arena, parts[5]));
    let severity_text = match unescape_header_field(arena, parts[6]) {
        Value::String(s) => s,
        _ => unreachable!("unescape_header_field always returns a String"),
    };
    let severity_value = severity_text
        .parse::<i64>()
        .map(Value::Int)
        .unwrap_or(Value::String(severity_text));
    builder.push("severity", severity_value);

    // Emit the raw extension blob (omitted when empty, to mirror
    // `syslog.parse`'s treatment of `msg`), then the split pairs in
    // their own `extension` sub-object. The nesting is the collision
    // barrier: extension keys named after header fields (`severity=`,
    // `name=`, dialect quirks or log injection alike) stay in the
    // extension plane and can never shadow a positional header value.
    if !remaining.is_empty() {
        builder.push("extension_raw", Value::String(arena.alloc_str(remaining)));
        let mut ext_builder = ObjectBuilder::new(arena);
        parse_cef_extensions(arena, remaining, &mut ext_builder);
        builder.push("extension", ext_builder.finish());
    }

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

/// Split the input at the first **unescaped** `|`, returning the raw
/// (still-escaped) field and the rest after the separator.
///
/// A `\` consumes the following byte, so `\|` never separates and the
/// pipe after `\\` (an escaped backslash) does — `a\\|b` splits into
/// the field `a\\` and rest `b`. Scanning bytes is UTF-8-safe because
/// `\` and `|` are ASCII and cannot occur inside a multi-byte sequence.
fn split_unescaped_pipe(input: &str) -> Option<(&str, &str)> {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'|' => return Some((&input[..i], &input[i + 1..])),
            _ => i += 1,
        }
    }
    None
}

/// Decode the generic structural header escapes in a raw field:
/// `\|` → `|` and `\\` → `\` — the two escapes that participate in
/// field splitting. The spec's field-specific escaping (`=` / `%` /
/// `#` for vulnerability spellings in deviceEventClassId / name) is
/// field-internal grammar this generic primitive does not interpret;
/// those spellings are preserved raw. Sequences outside the two
/// structural escapes (`\x`, …) and a trailing lone `\` are kept
/// literally — lenient by design, so a producer's stray backslash
/// degrades to visible bytes instead of a parse error or silent data
/// loss. Extension-section escapes are a separate scope and are
/// deliberately not decoded here (see the `extension_raw` FieldSpec
/// note).
fn unescape_header_field<'bump>(arena: &'bump EventArena<'bump>, field: &str) -> Value<'bump> {
    if !field.contains('\\') {
        return Value::String(arena.alloc_str(field));
    }
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('|') => out.push('|'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    Value::String(arena.alloc_str(&out))
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

    /// Look a key up inside the nested `extension` sub-object.
    fn ext_lookup<'bump>(
        entries: &'bump [(&'bump str, Value<'bump>)],
        key: &str,
    ) -> Option<Value<'bump>> {
        let Some(Value::Object(ext)) = lookup(entries, "extension") else {
            return None;
        };
        lookup(ext, key)
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
    fn extension_split_into_nested_keys_and_raw_blob() {
        // Both forms must be present: the split per-key form under the
        // nested `extension` sub-object (the documented authoring
        // surface, isolated from the positional header plane) AND the
        // raw `extension_raw` blob.
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
        // Split per-key form, isolated under `extension`
        assert_eq!(ext_lookup(entries, "src"), Some(Value::String("10.0.0.1")));
        assert_eq!(ext_lookup(entries, "dst"), Some(Value::String("10.0.0.2")));
        assert_eq!(ext_lookup(entries, "act"), Some(Value::String("accept")));
        // Raw blob
        assert_eq!(
            lookup(entries, "extension_raw"),
            Some(Value::String("src=10.0.0.1 dst=10.0.0.2 act=accept"))
        );
    }

    #[test]
    fn empty_extensions_omits_extension_fields() {
        // CEF allows an empty Extension section. When empty, both
        // `extension_raw` and the `extension` sub-object must be
        // omitted entirely (mirrors syslog.parse's treatment of empty
        // `msg`), so callers can write presence tests rather than
        // empty-string tests.
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
        // Both extension planes must be absent (not "" or null).
        assert!(
            lookup(entries, "extension_raw").is_none(),
            "extension_raw must be omitted when Extensions are empty"
        );
        assert!(
            lookup(entries, "extension").is_none(),
            "extension sub-object must be omitted when Extensions are empty"
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
            ext_lookup(entries, "msg"),
            Some(Value::String("Failed login attempt"))
        );
        assert_eq!(ext_lookup(entries, "user"), Some(Value::String("alice")));
        assert_eq!(ext_lookup(entries, "src"), Some(Value::String("10.0.0.1")));
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
            ext_lookup(entries, "request"),
            Some(Value::String("https://x.example.com/a?b=1&c=2"))
        );
        assert_eq!(ext_lookup(entries, "src"), Some(Value::String("10.0.0.1")));
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
        assert_eq!(
            ext_lookup(entries, "vendor_field"),
            Some(Value::String("v1"))
        );
        assert_eq!(
            ext_lookup(entries, "cs1Label"),
            Some(Value::String("Source IP"))
        );
        assert_eq!(ext_lookup(entries, "cs1"), Some(Value::String("10.0.0.1")));
    }

    #[test]
    fn header_escaped_pipe_is_literal_not_separator() {
        // CEF spec: `\|` inside a header field is a literal pipe, NOT a
        // field separator. Splitting on it shifts every subsequent
        // header field — name absorbed `deny\`, severity received
        // `drop`, and the extension blob received `3|act=block`, so the
        // positionally-determined severity was silently misclassified
        // downstream. The splitter must honor the escape and the stored
        // field must be unescaped.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        // 7 fields; the `name` field contains an escaped pipe.
        let line = arena.alloc_str("CEF:0|V|P|1.0|sig|deny\\|drop|3|act=block");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object on escaped-pipe input");
        };
        assert_eq!(lookup(entries, "name"), Some(Value::String("deny|drop")));
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(3)));
        assert_eq!(
            lookup(entries, "extension_raw"),
            Some(Value::String("act=block"))
        );
        assert_eq!(ext_lookup(entries, "act"), Some(Value::String("block")));
    }

    #[test]
    fn header_escaped_backslash_unescapes_to_single_backslash() {
        // CEF spec: `\\` inside a header field is a literal backslash.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|V|A\\\\B|1.0|sig|name|3|act=block");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(
            lookup(entries, "device_product"),
            Some(Value::String("A\\B"))
        );
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(3)));
    }

    #[test]
    fn header_pipe_after_escaped_backslash_separates() {
        // `\\` consumes both backslashes, so the pipe that follows an
        // escaped backslash IS a separator: the field `a\\` ends there
        // and unescapes to `a\`.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|a\\\\|P|1.0|sig|name|3|act=block");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "device_vendor"), Some(Value::String("a\\")));
        assert_eq!(lookup(entries, "device_product"), Some(Value::String("P")));
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(3)));
    }

    #[test]
    fn header_undefined_escape_is_preserved() {
        // Only `\|` and `\\` are the generic structural escapes that
        // participate in field splitting. A sequence outside those two
        // (`\x`) is kept literally (lenient): a producer's stray
        // backslash degrades to visible bytes rather than a parse
        // error or silent loss.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|V|a\\xb|1.0|sig|tail\\|3|7|act=block");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        // `\x` is not a structural escape — both bytes survive.
        assert_eq!(
            lookup(entries, "device_product"),
            Some(Value::String("a\\xb"))
        );
        // The `\|` inside `tail\|3` is consumed as an escape, so the
        // name field runs through the literal pipe to the next
        // unescaped separator — name `tail|3`, severity 7 from the
        // following field.
        assert_eq!(lookup(entries, "name"), Some(Value::String("tail|3")));
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(7)));
    }

    #[test]
    fn header_unescape_preserves_trailing_backslash() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);

        assert_eq!(
            unescape_header_field(&arena, "tail\\"),
            Value::String("tail\\")
        );
    }

    #[test]
    fn extension_raw_trailing_backslash_is_preserved() {
        // A trailing lone `\` at the end of the *extension* section
        // reaches `extension_raw` unmodified — the extension plane is
        // not header-unescaped. (A complete header field ending in a
        // lone `\` is structurally unobservable through the splitter:
        // an odd backslash immediately before `|` always reads as the
        // `\|` escape, so the header-side lone-backslash policy is
        // exercised only via the unescape helper's terminal branch.)
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|V|P|1.0|sig|name|3|msg=x\\");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(3)));
        assert_eq!(
            lookup(entries, "extension_raw"),
            Some(Value::String("msg=x\\"))
        );
    }

    #[test]
    fn header_escape_handles_utf8_and_odd_backslash_parity() {
        // Pin two shapes the reviewer confirmed the splitter handles:
        // multi-byte UTF-8 around an escaped pipe (the byte scan keys
        // on ASCII `\` / `|`, which cannot occur inside a multi-byte
        // sequence), and odd backslash parity before a pipe —
        // `a\\\|b` is an escaped backslash followed by an escaped
        // pipe, so nothing separates and the field unescapes to
        // `a\|b`.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("CEF:0|V|日本\\|語|1.0|sig|a\\\\\\|b|3|act=block");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(
            lookup(entries, "device_product"),
            Some(Value::String("日本|語"))
        );
        assert_eq!(lookup(entries, "name"), Some(Value::String("a\\|b")));
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(3)));
        assert_eq!(
            lookup(entries, "extension_raw"),
            Some(Value::String("act=block"))
        );
    }

    #[test]
    fn duplicate_extension_keys_first_one_wins() {
        // Some vendors emit the same key twice (intentionally for
        // multi-value sources, accidentally for buggy templates).
        // `ObjectBuilder` does not deduplicate, so both `src` entries
        // are emitted in source order inside the `extension`
        // sub-object; `ext_lookup` returns the first match, making the
        // observable semantics first-one-wins. Pin
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
        assert_eq!(ext_lookup(entries, "src"), Some(Value::String("10.0.0.1")));
        assert_eq!(ext_lookup(entries, "dst"), Some(Value::String("8.8.8.8")));
    }

    #[test]
    fn header_named_extension_keys_cannot_shadow_header_fields() {
        // Extensions named after header fields (`severity=`, `name=`,
        // `ext=`) — dialect quirks, buggy templates, or log injection —
        // must stay isolated in the `extension` sub-object. Before the
        // split, `ObjectBuilder` pushed them as duplicate siblings of
        // the header keys: arena field reads resolved first-wins (the
        // header), but the owned workspace snapshot resolved last-wins,
        // so a downstream process read `severity = "9"` in place of the
        // positional header value 7. The nesting removes the duplicate
        // keys entirely.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena
            .alloc_str("CEF:0|Vendor|Product|1.0|100|realname|7|severity=9 name=fakename ext=x");
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        // Header plane untouched.
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(7)));
        assert_eq!(lookup(entries, "name"), Some(Value::String("realname")));
        // No duplicate top-level keys: each header key appears once.
        for key in ["severity", "name"] {
            assert_eq!(
                entries.iter().filter(|(k, _)| *k == key).count(),
                1,
                "header key {key} must appear exactly once"
            );
        }
        // Colliding extensions isolated in the extension plane.
        assert_eq!(ext_lookup(entries, "severity"), Some(Value::String("9")));
        assert_eq!(ext_lookup(entries, "name"), Some(Value::String("fakename")));
        assert_eq!(ext_lookup(entries, "ext"), Some(Value::String("x")));
        assert_eq!(
            lookup(entries, "extension_raw"),
            Some(Value::String("severity=9 name=fakename ext=x"))
        );
    }
}
