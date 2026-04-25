//! `syslog.parse(text[, defaults])` — RFC 3164 / 5424 header parser.
//!
//! Auto-detects the RFC version: a single digit followed by SP after
//! `<PRI>` is treated as RFC 5424 (versioned), anything else as RFC
//! 3164 (BSD traditional).
//!
//! Returns a `Value::Object` with the following keys (all string-valued
//! when present, omitted otherwise):
//!
//! | key                         | meaning                               |
//! |-----------------------------|---------------------------------------|
//! | `syslog_hostname`           | originating host                      |
//! | `syslog_appname`            | app-name (5424) / tag (3164)          |
//! | `syslog_procid`             | process id (when present)             |
//! | `syslog_msgid`              | message id (5424 only)                |
//! | `syslog_msg`                | body after header                     |
//!
//! Unlike the old native `parse_syslog` process, this function does
//! **not** rewrite `event.egress` — it's pure. Users who want the old
//! behaviour write `egress = workspace.syslog_msg` on the next line.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use crate::functions::primitives::parse_json::{apply_defaults, type_name};
use crate::functions::primitives::val_to_str;
use crate::functions::{FunctionRegistry, ParserInfo};
use crate::modules::schema::{FieldSpec, FieldType};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_in("syslog", "parse", |args, _event| parse_impl(args));
    // syslog header fields are statically known (not data-driven), so
    // declare them precisely. The analyzer uses these to type-check
    // downstream `workspace.syslog_*` references after a bare
    // `syslog.parse(ingress)` statement.
    reg.register_parser(ParserInfo {
        namespace: Some("syslog"),
        name: "parse",
        produces: vec![
            FieldSpec::new(&["workspace", "syslog_hostname"], FieldType::String),
            FieldSpec::new(&["workspace", "syslog_appname"], FieldType::String),
            FieldSpec::new(&["workspace", "syslog_procid"], FieldType::String),
            FieldSpec::new(&["workspace", "syslog_msgid"], FieldType::String),
            FieldSpec::new(&["workspace", "syslog_msg"], FieldType::String),
        ],
        wildcards: false,
    });
}

fn parse_impl(args: &[Value]) -> Result<Value> {
    // Arity is validated by the registry via the sig installed from
    // `register_parser` (1 to 2 arguments).
    let text = val_to_str(&args[0]);

    let after_pri =
        skip_pri(&text).ok_or_else(|| anyhow::anyhow!("syslog.parse(): no PRI header"))?;

    let mut map = Map::new();
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

fn skip_pri(ingress: &str) -> Option<&str> {
    if !ingress.starts_with('<') {
        return None;
    }
    let end = ingress.find('>')?;
    Some(&ingress[end + 1..])
}

fn parse_rfc5424(input: &str, map: &mut Map<String, Value>) {
    let mut parts = input.splitn(7, ' ');
    let _version = parts.next();
    let _timestamp = parts.next();
    let hostname = parts.next().unwrap_or("-");
    let appname = parts.next().unwrap_or("-");
    let procid = parts.next().unwrap_or("-");
    let msgid = parts.next().unwrap_or("-");
    let remainder = parts.next().unwrap_or("");

    let msg = skip_structured_data(remainder);

    set_field(map, "syslog_hostname", hostname);
    set_field(map, "syslog_appname", appname);
    if procid != "-" {
        set_field(map, "syslog_procid", procid);
    }
    if msgid != "-" {
        set_field(map, "syslog_msgid", msgid);
    }
    if !msg.is_empty() {
        map.insert("syslog_msg".into(), Value::String(msg.to_string()));
    }
}

fn parse_rfc3164(input: &str, map: &mut Map<String, Value>) {
    let mut rest = input;
    if let Some(idx) = nth_space(rest, 3) {
        rest = &rest[idx..];
    }
    let (hostname, after_host) = next_token(rest);
    let (appname, procid, msg) = parse_tag_and_msg(after_host);

    if !hostname.is_empty() {
        set_field(map, "syslog_hostname", hostname);
    }
    if !appname.is_empty() {
        set_field(map, "syslog_appname", appname);
    }
    if let Some(pid) = procid {
        set_field(map, "syslog_procid", pid);
    }
    if !msg.is_empty() {
        map.insert("syslog_msg".into(), Value::String(msg.to_string()));
    }
}

fn parse_tag_and_msg(input: &str) -> (&str, Option<&str>, &str) {
    let input = input.trim_start();
    if let Some(colon_pos) = input.find(": ") {
        let tag_part = &input[..colon_pos];
        let msg = &input[colon_pos + 2..];
        if let Some(bracket_start) = tag_part.find('[')
            && let Some(bracket_end) = tag_part.find(']')
        {
            let appname = &tag_part[..bracket_start];
            let procid = &tag_part[bracket_start + 1..bracket_end];
            return (appname, Some(procid), msg);
        }
        return (tag_part, None, msg);
    }
    ("", None, input)
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

fn set_field(map: &mut Map<String, Value>, key: &str, value: &str) {
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

    fn call_syslog_parse(reg: &FunctionRegistry, s: &str) -> Result<Map<String, Value>> {
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
        assert_eq!(m["syslog_hostname"], Value::String("firewall01".into()));
        assert_eq!(m["syslog_appname"], Value::String("sshd".into()));
        assert_eq!(m["syslog_procid"], Value::String("1234".into()));
        assert_eq!(m["syslog_msg"], Value::String("Failed password".into()));
    }

    #[test]
    fn rfc5424_with_structured_data() {
        let reg = make_reg();
        let m = call_syslog_parse(
            &reg,
            "<134>1 2026-04-15T10:30:00Z host app 999 ID1 [meta src=\"10.0.0.1\"] Hello world",
        )
        .unwrap();
        assert_eq!(m["syslog_hostname"], Value::String("host".into()));
        assert_eq!(m["syslog_appname"], Value::String("app".into()));
        assert_eq!(m["syslog_msgid"], Value::String("ID1".into()));
        assert_eq!(m["syslog_msg"], Value::String("Hello world".into()));
    }

    #[test]
    fn rfc3164_with_pid() {
        let reg = make_reg();
        let m = call_syslog_parse(
            &reg,
            "<134>Apr 15 10:30:00 myhost sshd[1234]: Failed password",
        )
        .unwrap();
        assert_eq!(m["syslog_hostname"], Value::String("myhost".into()));
        assert_eq!(m["syslog_appname"], Value::String("sshd".into()));
        assert_eq!(m["syslog_procid"], Value::String("1234".into()));
        assert_eq!(m["syslog_msg"], Value::String("Failed password".into()));
    }

    #[test]
    fn rfc3164_without_pid() {
        let reg = make_reg();
        let m =
            call_syslog_parse(&reg, "<134>Apr 15 10:30:00 myhost kernel: Out of memory").unwrap();
        assert_eq!(m["syslog_hostname"], Value::String("myhost".into()));
        assert_eq!(m["syslog_appname"], Value::String("kernel".into()));
        assert_eq!(m["syslog_msg"], Value::String("Out of memory".into()));
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
            [(
                "syslog_appname".to_string(),
                Value::String("unknown".into()),
            )]
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
        assert_eq!(m["syslog_appname"], Value::String("unknown".into()));
    }
}
