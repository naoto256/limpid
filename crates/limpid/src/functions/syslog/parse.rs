//! `syslog.parse(text[, defaults])` — RFC 3164 / 5424 header parser.
//!
//! Auto-detects the RFC version: a single digit followed by SP after
//! `<PRI>` is treated as RFC 5424 (versioned), anything else as RFC
//! 3164 (BSD traditional).
//!
//! Returns a `Value::Object` with the following keys:
//!
//! | key         | type   | meaning                                       |
//! |-------------|--------|-----------------------------------------------|
//! | `pri`       | Int    | raw `<PRI>` value (0..=191)                   |
//! | `facility`  | Int    | `pri / 8`                                     |
//! | `severity`  | Int    | `pri % 8`                                     |
//! | `timestamp` | String | source-claimed event time                     |
//! | `hostname`  | String | originating host                              |
//! | `appname`   | String | app-name (5424) / tag (3164)                  |
//! | `procid`    | String | process id (when present)                     |
//! | `msgid`     | String | message id (5424 only)                        |
//! | `msg`       | String | body after header                             |
//!
//! String fields are present only when the wire format provides a
//! non-empty, non-`-` value. `pri` / `facility` / `severity` are
//! always present — the parser errors when no valid `<PRI>` header
//! is found.

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};
use anyhow::{Result, bail};

use crate::functions::primitives::parse_json::{apply_defaults, type_name};
use crate::functions::primitives::val_to_str;
use crate::functions::syslog::pri::parse_leading_pri;
use crate::functions::{FunctionRegistry, ParserInfo};
use crate::modules::schema::{FieldSpec, FieldType};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in("syslog", "parse", |arena, args, _event| {
        parse_impl(arena, args)
    });
    reg.register_parser(ParserInfo {
        namespace: Some("syslog"),
        name: "parse",
        produces: vec![
            FieldSpec::new(&["workspace", "pri"], FieldType::Int),
            FieldSpec::new(&["workspace", "facility"], FieldType::Int),
            FieldSpec::new(&["workspace", "severity"], FieldType::Int),
            FieldSpec::new(&["workspace", "timestamp"], FieldType::String),
            FieldSpec::new(&["workspace", "hostname"], FieldType::String),
            FieldSpec::new(&["workspace", "appname"], FieldType::String),
            FieldSpec::new(&["workspace", "procid"], FieldType::String),
            FieldSpec::new(&["workspace", "msgid"], FieldType::String),
            FieldSpec::new(&["workspace", "msg"], FieldType::String),
        ],
        wildcards: false,
    });
}

fn parse_impl<'bump>(
    arena: &'bump EventArena<'bump>,
    args: &[Value<'bump>],
) -> Result<Value<'bump>> {
    let text = val_to_str(&args[0])?;

    let (pri, body_offset) =
        parse_leading_pri(&text).ok_or_else(|| anyhow::anyhow!("syslog.parse(): no PRI header"))?;
    let after_pri = &text[body_offset..];

    let mut builder = ObjectBuilder::with_capacity(arena, 9);
    builder.push("pri", Value::Int(pri as i64));
    builder.push("facility", Value::Int((pri / 8) as i64));
    builder.push("severity", Value::Int((pri % 8) as i64));

    if after_pri.len() >= 2
        && after_pri.as_bytes()[0].is_ascii_digit()
        && after_pri.as_bytes()[1] == b' '
    {
        parse_rfc5424(arena, after_pri, &mut builder);
    } else {
        parse_rfc3164(arena, after_pri, &mut builder);
    }

    let parsed = builder.finish();

    if let Some(v) = args.get(1) {
        match v {
            Value::Object(_) | Value::Null => apply_defaults(arena, "syslog.parse", Some(v), parsed),
            other => bail!(
                "syslog.parse(): second argument must be a hash literal, got {}",
                type_name(other)
            ),
        }
    } else {
        Ok(parsed)
    }
}

fn parse_rfc5424<'bump>(
    arena: &EventArena<'bump>,
    input: &str,
    builder: &mut ObjectBuilder<'bump>,
) {
    let mut parts = input.splitn(7, ' ');
    let _version = parts.next();
    let timestamp = parts.next().unwrap_or("-");
    let hostname = parts.next().unwrap_or("-");
    let appname = parts.next().unwrap_or("-");
    let procid = parts.next().unwrap_or("-");
    let msgid = parts.next().unwrap_or("-");
    let remainder = parts.next().unwrap_or("");

    let msg = skip_structured_data(remainder);

    set_field(arena, builder, "timestamp", timestamp);
    set_field(arena, builder, "hostname", hostname);
    set_field(arena, builder, "appname", appname);
    if procid != "-" {
        set_field(arena, builder, "procid", procid);
    }
    if msgid != "-" {
        set_field(arena, builder, "msgid", msgid);
    }
    if !msg.is_empty() {
        builder.push("msg", Value::String(arena.alloc_str(msg)));
    }
}

fn parse_rfc3164<'bump>(
    arena: &EventArena<'bump>,
    input: &str,
    builder: &mut ObjectBuilder<'bump>,
) {
    let mut rest = input;
    let timestamp_str = match nth_space(rest, 3) {
        Some(idx) => {
            let s = &rest[..idx];
            rest = &rest[idx..];
            Some(s.trim())
        }
        None => None,
    };
    let (hostname, after_host) = next_token(rest);
    let (appname, procid, msg) = parse_tag_and_msg(after_host);

    if let Some(ts) = timestamp_str
        && !ts.is_empty()
    {
        set_field(arena, builder, "timestamp", ts);
    }
    if !hostname.is_empty() {
        set_field(arena, builder, "hostname", hostname);
    }
    if !appname.is_empty() {
        set_field(arena, builder, "appname", appname);
    }
    if let Some(pid) = procid {
        set_field(arena, builder, "procid", pid);
    }
    if !msg.is_empty() {
        builder.push("msg", Value::String(arena.alloc_str(msg)));
    }
}

fn parse_tag_and_msg(input: &str) -> (&str, Option<&str>, &str) {
    let input = input.trim_start();
    let bytes = input.as_bytes();

    // RFC 3164 §4.1.3: the TAG is a leading run of alphanumerics (max
    // 32 chars), terminated by the first non-alphanumeric character.
    // We additionally require the terminator to look like `: ` (or
    // `[pid]: `, or a bare trailing `:`) — otherwise the body itself
    // contains the colon-space token (e.g. CEF extensions like
    // `msg=applications3: Shenzhen...`) and a permissive `find(": ")`
    // would split the TAG mid-payload.
    const TAG_MAX: usize = 32;
    let mut tag_end = 0;
    while tag_end < bytes.len() && tag_end < TAG_MAX && bytes[tag_end].is_ascii_alphanumeric() {
        tag_end += 1;
    }
    if tag_end == 0 {
        return ("", None, input);
    }
    let tag = &input[..tag_end];
    let after_tag = &input[tag_end..];

    let (procid, after_pid) = if let Some(rest) = after_tag.strip_prefix('[') {
        match rest.find(']') {
            Some(close) => (Some(&rest[..close]), &rest[close + 1..]),
            None => return ("", None, input),
        }
    } else {
        (None, after_tag)
    };

    if let Some(msg) = after_pid.strip_prefix(": ") {
        (tag, procid, msg)
    } else if after_pid == ":" {
        (tag, procid, "")
    } else {
        ("", None, input)
    }
}

fn skip_structured_data(input: &str) -> &str {
    let input = input.trim_start();
    if input.starts_with('[') {
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() && bytes[i] == b'[' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    i += 2;
                } else if bytes[i] == b']' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            while i < bytes.len() && bytes[i] == b' ' {
                i += 1;
            }
        }
        &input[i.min(input.len())..]
    } else if let Some(rest) = input.strip_prefix('-') {
        rest.trim_start()
    } else {
        input
    }
}

fn set_field<'bump>(
    arena: &EventArena<'bump>,
    builder: &mut ObjectBuilder<'bump>,
    key: &str,
    value: &str,
) {
    if value != "-" && !value.is_empty() {
        builder.push_str(key, Value::String(arena.alloc_str(value)));
    }
}

fn next_token(input: &str) -> (&str, &str) {
    let input = input.trim_start();
    match input.find(' ') {
        Some(pos) => (&input[..pos], &input[pos + 1..]),
        None => (input, ""),
    }
}

fn nth_space(input: &str, n: usize) -> Option<usize> {
    let mut count = 0;
    for (i, b) in input.bytes().enumerate() {
        if b == b' ' {
            count += 1;
            if count == n {
                return Some(i + 1);
            }
        }
    }
    None
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
        reg.call(
            Some("syslog"),
            "parse",
            &[Value::String(line)],
            bevent,
            arena,
        )
        .expect("parse should succeed")
    }

    #[test]
    fn rfc5424_basic_yields_expected_fields() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str(
            "<134>1 2026-04-15T10:30:00Z firewall01 sshd 1234 - - Failed password",
        );
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        // PRI 134 = facility 16 (local0), severity 6 (info)
        assert_eq!(lookup(entries, "pri"), Some(Value::Int(134)));
        assert_eq!(lookup(entries, "facility"), Some(Value::Int(16)));
        assert_eq!(lookup(entries, "severity"), Some(Value::Int(6)));
        assert_eq!(
            lookup(entries, "timestamp"),
            Some(Value::String("2026-04-15T10:30:00Z"))
        );
        assert_eq!(lookup(entries, "hostname"), Some(Value::String("firewall01")));
        assert_eq!(lookup(entries, "appname"), Some(Value::String("sshd")));
        assert_eq!(lookup(entries, "procid"), Some(Value::String("1234")));
        assert_eq!(lookup(entries, "msg"), Some(Value::String("Failed password")));
    }

    #[test]
    fn rfc3164_with_pid_extracts_tag_and_procid() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str(
            "<134>Apr 15 10:30:00 myhost sshd[1234]: Failed password",
        );
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(
            lookup(entries, "timestamp"),
            Some(Value::String("Apr 15 10:30:00"))
        );
        assert_eq!(lookup(entries, "hostname"), Some(Value::String("myhost")));
        assert_eq!(lookup(entries, "appname"), Some(Value::String("sshd")));
        assert_eq!(lookup(entries, "procid"), Some(Value::String("1234")));
        assert_eq!(
            lookup(entries, "msg"),
            Some(Value::String("Failed password"))
        );
    }

    #[test]
    fn rfc3164_without_pid_extracts_tag_and_msg() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str(
            "<134>Apr 15 10:30:00 myhost kernel: Out of memory",
        );
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "hostname"), Some(Value::String("myhost")));
        assert_eq!(lookup(entries, "appname"), Some(Value::String("kernel")));
        assert_eq!(
            lookup(entries, "msg"),
            Some(Value::String("Out of memory"))
        );
    }

    #[test]
    fn no_pri_returns_error() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str("no pri header");
        let err = reg
            .call(
                Some("syslog"),
                "parse",
                &[Value::String(line)],
                &bevent,
                &arena,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("no PRI header"),
            "expected 'no PRI header' message, got: {}",
            err
        );
    }

    #[test]
    fn rfc3164_cef_payload_not_split_on_inner_colon_space() {
        // CEF extensions can carry `key=value: ...` patterns
        // (e.g. `msg=applications3: Shenzhen...`). The TAG/MSG split
        // must not greedily consume the body up to that inner `": "`
        // — otherwise downstream `cef.parse(workspace.msg)` receives
        // a tail fragment instead of the CEF prefix. Regression test
        // for the v0.5.8 anchor fix (`fea8dfa fix(syslog.parse):
        // anchor RFC 3164 TAG to start, require ': ' separator`).
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_event();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let line = arena.alloc_str(
            "<134>Apr 15 10:30:00 fwhost CEF:0|Fortinet|FortiGate|7.0|13056|app-ctrl|3|msg=applications3: Shenzhen.TVT",
        );
        let v = parse_into(&reg, &bevent, &arena, line);
        let Value::Object(entries) = v else {
            panic!("expected Object");
        };
        assert_eq!(lookup(entries, "hostname"), Some(Value::String("fwhost")));
        // No syntactic TAG ahead of the CEF payload — appname must
        // not be set, and the entire CEF string must reach `msg`.
        assert!(lookup(entries, "appname").is_none());
        let Some(Value::String(msg)) = lookup(entries, "msg") else {
            panic!("expected msg to be a String");
        };
        assert!(
            msg.starts_with("CEF:0|Fortinet|FortiGate"),
            "msg should retain the CEF prefix, got {msg:?}",
        );
        assert!(msg.ends_with("Shenzhen.TVT"));
    }
}
