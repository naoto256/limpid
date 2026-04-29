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
//! | `timestamp` | String | source-claimed event time (5424 / 3164 token) |
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
//!
//! This function does **not** rewrite `event.egress` — it's pure.

use crate::dsl::value::Map;
use crate::dsl::value::Value;
use anyhow::{Result, bail};

use crate::functions::primitives::parse_json::{apply_defaults, type_name};
use crate::functions::primitives::val_to_str;
use crate::functions::syslog::pri::parse_leading_pri;
use crate::functions::{FunctionRegistry, ParserInfo};
use crate::modules::schema::{FieldSpec, FieldType};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in("syslog", "parse", |args, _event| parse_impl(args));
    // syslog header fields are statically known (not data-driven), so
    // declare them precisely. The analyzer uses these to type-check
    // downstream `workspace.*` references after a bare
    // `syslog.parse(ingress)` statement.
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

fn parse_impl(args: &[Value]) -> Result<Value> {
    // Arity is validated by the registry via the sig installed from
    // `register_parser` (1 to 2 arguments).
    let text = val_to_str(&args[0])?;

    let (pri, body_offset) =
        parse_leading_pri(&text).ok_or_else(|| anyhow::anyhow!("syslog.parse(): no PRI header"))?;
    let after_pri = &text[body_offset..];

    let mut map = Map::new();
    map.insert("pri".into(), Value::Int(pri as i64));
    map.insert("facility".into(), Value::Int((pri / 8) as i64));
    map.insert("severity".into(), Value::Int((pri % 8) as i64));

    if after_pri.len() >= 2
        && after_pri.as_bytes()[0].is_ascii_digit()
        && after_pri.as_bytes()[1] == b' '
    {
        parse_rfc5424(after_pri, &mut map);
    } else {
        parse_rfc3164(after_pri, &mut map);
    }

    // Shared defaults helper — uses `syslog.parse` in error text.
    if let Some(v) = args.get(1) {
        match v {
            Value::Object(_) | Value::Null => apply_defaults("syslog.parse", Some(v), &mut map)?,
            other => bail!(
                "syslog.parse(): second argument must be a hash literal, got {}",
                type_name(other)
            ),
        }
    }

    Ok(Value::Object(map))
}

fn parse_rfc5424(input: &str, map: &mut Map) {
    let mut parts = input.splitn(7, ' ');
    let _version = parts.next();
    let timestamp = parts.next().unwrap_or("-");
    let hostname = parts.next().unwrap_or("-");
    let appname = parts.next().unwrap_or("-");
    let procid = parts.next().unwrap_or("-");
    let msgid = parts.next().unwrap_or("-");
    let remainder = parts.next().unwrap_or("");

    let msg = skip_structured_data(remainder);

    // RFC 5424 timestamp is the source's claimed event time — surface
    // it into workspace so snippets can compare against
    // Event.timestamp (limpid's receipt time, kept independent per
    // Principle 2: input is dumb transport).
    set_field(map, "timestamp", timestamp);
    set_field(map, "hostname", hostname);
    set_field(map, "appname", appname);
    if procid != "-" {
        set_field(map, "procid", procid);
    }
    if msgid != "-" {
        set_field(map, "msgid", msgid);
    }
    if !msg.is_empty() {
        map.insert("msg".into(), Value::String(msg.to_string()));
    }
}

fn parse_rfc3164(input: &str, map: &mut Map) {
    let mut rest = input;
    // RFC 3164 timestamp is 3 space-separated tokens: "Mon DD HH:MM:SS"
    // (no year, no timezone). Capture them as a single string before
    // advancing past the timestamp.
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
        set_field(map, "timestamp", ts);
    }
    if !hostname.is_empty() {
        set_field(map, "hostname", hostname);
    }
    if !appname.is_empty() {
        set_field(map, "appname", appname);
    }
    if let Some(pid) = procid {
        set_field(map, "procid", pid);
    }
    if !msg.is_empty() {
        map.insert("msg".into(), Value::String(msg.to_string()));
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

fn set_field(map: &mut Map, key: &str, value: &str) {
    if value != "-" && !value.is_empty() {
        map.insert(key.into(), Value::String(value.into()));
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
    use crate::event::Event;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn dummy_event() -> Event {
        Event::new(
            Bytes::from("test"),
            "127.0.0.1:514".parse::<SocketAddr>().unwrap(),
        )
    }

    fn make_reg() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        register(&mut reg);
        reg
    }

    fn call_syslog_parse(reg: &FunctionRegistry, s: &str) -> Result<Map> {
        let e = dummy_event();
        let v = reg.call(Some("syslog"), "parse", &[Value::String(s.into())], &e)?;
        let Value::Object(m) = v else {
            panic!("expected Object")
        };
        Ok(m)
    }

    #[test]
    fn rfc5424_basic() {
        let reg = make_reg();
        let m = call_syslog_parse(
            &reg,
            "<134>1 2026-04-15T10:30:00Z firewall01 sshd 1234 - - Failed password",
        )
        .unwrap();
        // PRI 134 = facility 16 (local0), severity 6 (info)
        assert_eq!(m["pri"], Value::Int(134));
        assert_eq!(m["facility"], Value::Int(16));
        assert_eq!(m["severity"], Value::Int(6));
        assert_eq!(m["timestamp"], Value::String("2026-04-15T10:30:00Z".into()));
        assert_eq!(m["hostname"], Value::String("firewall01".into()));
        assert_eq!(m["appname"], Value::String("sshd".into()));
        assert_eq!(m["procid"], Value::String("1234".into()));
        assert_eq!(m["msg"], Value::String("Failed password".into()));
    }

    #[test]
    fn rfc5424_with_structured_data() {
        let reg = make_reg();
        let m = call_syslog_parse(
            &reg,
            "<134>1 2026-04-15T10:30:00Z host app 999 ID1 [meta src=\"10.0.0.1\"] Hello world",
        )
        .unwrap();
        assert_eq!(m["hostname"], Value::String("host".into()));
        assert_eq!(m["appname"], Value::String("app".into()));
        assert_eq!(m["msgid"], Value::String("ID1".into()));
        assert_eq!(m["msg"], Value::String("Hello world".into()));
    }

    #[test]
    fn rfc3164_with_pid() {
        let reg = make_reg();
        let m = call_syslog_parse(
            &reg,
            "<134>Apr 15 10:30:00 myhost sshd[1234]: Failed password",
        )
        .unwrap();
        assert_eq!(m["timestamp"], Value::String("Apr 15 10:30:00".into()));
        assert_eq!(m["hostname"], Value::String("myhost".into()));
        assert_eq!(m["appname"], Value::String("sshd".into()));
        assert_eq!(m["procid"], Value::String("1234".into()));
        assert_eq!(m["msg"], Value::String("Failed password".into()));
    }

    #[test]
    fn rfc3164_without_pid() {
        let reg = make_reg();
        let m =
            call_syslog_parse(&reg, "<134>Apr 15 10:30:00 myhost kernel: Out of memory").unwrap();
        assert_eq!(m["hostname"], Value::String("myhost".into()));
        assert_eq!(m["appname"], Value::String("kernel".into()));
        assert_eq!(m["msg"], Value::String("Out of memory".into()));
    }

    #[test]
    fn rfc3164_cef_payload_not_split_on_inner_colon_space() {
        // CEF extensions can carry `key=value: ...` patterns (e.g.
        // `msg=applications3: Shenzhen...`). The TAG/MSG split must
        // not greedily consume the body up to that inner `": "` —
        // otherwise downstream `cef.parse(workspace.syslog.msg)`
        // receives a tail fragment instead of the CEF prefix.
        let reg = make_reg();
        let m = call_syslog_parse(
            &reg,
            "<134>Apr 15 10:30:00 fwhost CEF:0|Fortinet|FortiGate|7.0|13056|app-ctrl|3|msg=applications3: Shenzhen.TVT",
        )
        .unwrap();
        assert_eq!(m["hostname"], Value::String("fwhost".into()));
        // No syntactic TAG ahead of the CEF payload — appname must
        // not be set, and the entire CEF string must reach `msg`.
        assert!(!m.contains_key("appname"));
        let Value::String(msg) = &m["msg"] else { panic!() };
        assert!(
            msg.starts_with("CEF:0|Fortinet|FortiGate"),
            "msg should retain the CEF prefix, got {msg:?}",
        );
        assert!(msg.ends_with("Shenzhen.TVT"));
    }

    #[test]
    fn no_pri_errors() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                Some("syslog"),
                "parse",
                &[Value::String("no pri header".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("no PRI header"));
    }

    #[test]
    fn defaults_fill_missing_keys() {
        let reg = make_reg();
        let defaults = Value::Object(
            [("appname".to_string(), Value::String("unknown".into()))]
                .into_iter()
                .collect(),
        );
        let e = dummy_event();
        // RFC 5424 with appname `-` (NILVALUE) — missing after parse
        let result = reg
            .call(
                Some("syslog"),
                "parse",
                &[
                    Value::String("<134>1 2026-04-15T10:30:00Z host - - - - body".into()),
                    defaults,
                ],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else { panic!() };
        assert_eq!(m["appname"], Value::String("unknown".into()));
    }
}
