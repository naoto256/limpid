//! `regex_parse(target, pattern)` — named-capture based extraction.
//!
//! Returns an Object with one key per named capture group in `pattern`.
//! Capture names containing `.` build nested objects, so
//! `(?P<date.month>...)` produces `{ date: { month: "..." } }` and
//! sibling dotted names merge under the same parent. Used as a bare
//! statement, the result merges into `workspace`, matching the
//! `parse_json` / `parse_kv` / `syslog.parse` family. This is the
//! "parse a header into many fields" companion to `regex_extract`,
//! which remains the single-scalar variant.
//!
//! ## Implementation note: `__DOT__` marker
//!
//! The `regex` family disallows `.` in capture names, so we preprocess
//! the pattern to mangle `(?P<a.b>…)` to `(?P<a__DOT__b>…)`, compile,
//! then demangle when reading capture names back out. `__DOT__` is a
//! reserved internal marker — capture names that contain the literal
//! string `__DOT__` will be misinterpreted. This is acceptable: the
//! collision is unlikely, the alternative (escaping) buys nothing for
//! real use, and a future move to a regex engine that accepts dotted
//! names removes the need entirely.

use serde_json::{Map, Value};

use super::{get_cached_regex, val_to_str};
use crate::functions::{FunctionRegistry, FunctionSig, ParserInfo};
use crate::modules::schema::FieldType;

const DOT_MARKER: &str = "__DOT__";

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "regex_parse",
        FunctionSig::fixed(&[FieldType::String, FieldType::String], FieldType::Any),
        |args, _event| {
        let target = val_to_str(&args[0]);
        let pattern = val_to_str(&args[1]);

        let mangled = mangle_pattern(&pattern);
        let re = get_cached_regex(&mangled).map_err(|e| anyhow::anyhow!("invalid regex: {}", e))?;

        // No named captures at all → empty object so a bare statement is a no-op.
        if re.capture_names().flatten().next().is_none() {
            return Ok(Value::Object(Map::new()));
        }

        let caps = match re.captures(&target) {
            Some(c) => c,
            None => return Ok(Value::Null),
        };

        let mut out = Map::new();
        for name in re.capture_names().flatten() {
            let original = demangle(name);
            if let Some(m) = caps.name(name) {
                set_nested(&mut out, &original, Value::String(m.as_str().to_string()));
            }
        }
        Ok(Value::Object(out))
        },
    );
    // Register as parser so the analyzer knows that a bare
    // `regex_parse(ingress, "(?P<src>...)")` statement merges its
    // named-capture keys into workspace, matching the
    // `parse_json` / `parse_kv` / `syslog.parse` family. Output keys are
    // fully data-driven (each pattern names its own captures), so
    // `wildcards = true` — the analyzer falls back to wildcard unless a
    // downstream defaults-style schema pin emerges.
    reg.register_parser(ParserInfo {
        namespace: None,
        name: "regex_parse",
        produces: Vec::new(),
        wildcards: true,
    });
}

/// Replace `.` inside `(?P<…>)` / `(?<…>)` capture names with `__DOT__`
/// so the regex engine accepts the pattern. Other parts of the pattern
/// are passed through verbatim, so `.` as a metacharacter elsewhere
/// (e.g. `\w+\.\w+`) is unaffected.
fn mangle_pattern(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let bytes = pattern.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Detect a named-group opener. Both `(?P<name>` and `(?<name>`
        // are accepted by regex / regex-lite.
        let opener = if bytes[i..].starts_with(b"(?P<") {
            Some(4)
        } else if bytes[i..].starts_with(b"(?<") {
            Some(3)
        } else {
            None
        };

        if let Some(prefix_len) = opener {
            out.push_str(&pattern[i..i + prefix_len]);
            i += prefix_len;
            let start = i;
            while i < bytes.len() && bytes[i] != b'>' {
                i += 1;
            }
            let name = &pattern[start..i];
            out.push_str(&name.replace('.', DOT_MARKER));
        // The closing '>' (if any) is emitted on the next iteration.
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn demangle(s: &str) -> String {
    s.replace(DOT_MARKER, ".")
}

/// Insert `value` at the dotted `path`, creating intermediate objects
/// as needed. If an intermediate slot is currently a non-object, it is
/// overwritten — this only happens when the caller wrote conflicting
/// names like `(?P<a>…)(?P<a.b>…)`, which is malformed.
fn set_nested(map: &mut Map<String, Value>, path: &str, value: Value) {
    let mut parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return;
    }
    let leaf = parts.pop().unwrap();
    let mut current = map;
    for part in parts {
        // Reborrow loop using a single match expression so the
        // intermediate `entry` borrow ends before we reassign `current`.
        let slot = current
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if !matches!(slot, Value::Object(_)) {
            *slot = Value::Object(Map::new());
        }
        current = match slot {
            Value::Object(m) => m,
            _ => unreachable!("just ensured Object"),
        };
    }
    current.insert(leaf.to_string(), value);
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

    fn call(target: &str, pattern: &str) -> Value {
        let reg = make_reg();
        let e = dummy_event();
        reg.call(
            None,
            "regex_parse",
            &[Value::String(target.into()), Value::String(pattern.into())],
            &e,
        )
        .unwrap()
    }

    #[test]
    fn regex_parse_single_named_capture() {
        let v = call("hello", r"(?P<h>\w+)");
        let Value::Object(m) = v else { panic!() };
        assert_eq!(m["h"], Value::String("hello".into()));
    }

    #[test]
    fn regex_parse_multiple_flat_names() {
        let v = call("foo bar", r"(?P<a>\w+)\s+(?P<b>\w+)");
        let Value::Object(m) = v else { panic!() };
        assert_eq!(m["a"], Value::String("foo".into()));
        assert_eq!(m["b"], Value::String("bar".into()));
    }

    #[test]
    fn regex_parse_dotted_creates_nested() {
        let v = call("Apr", r"(?P<date.month>\w{3})");
        let Value::Object(m) = v else { panic!() };
        let Value::Object(date) = &m["date"] else {
            panic!("expected nested object")
        };
        assert_eq!(date["month"], Value::String("Apr".into()));
    }

    #[test]
    fn regex_parse_deep_nested() {
        let v = call("xyz", r"(?P<a.b.c>\w+)");
        let Value::Object(m) = v else { panic!() };
        let Value::Object(a) = &m["a"] else { panic!() };
        let Value::Object(b) = &a["b"] else { panic!() };
        assert_eq!(b["c"], Value::String("xyz".into()));
    }

    #[test]
    fn regex_parse_sibling_nested() {
        let v = call("Apr 24", r"(?P<date.month>\w{3})\s+(?P<date.day>\d+)");
        let Value::Object(m) = v else { panic!() };
        let Value::Object(date) = &m["date"] else {
            panic!()
        };
        assert_eq!(date["month"], Value::String("Apr".into()));
        assert_eq!(date["day"], Value::String("24".into()));
    }

    #[test]
    fn regex_parse_no_match_returns_null() {
        let v = call("nothing here", r"(?P<n>\d+)");
        assert_eq!(v, Value::Null);
    }

    #[test]
    fn regex_parse_no_named_captures_returns_empty_object() {
        // Only positional groups → empty object (bare statement no-op).
        let v = call("hello world", r"(\w+)\s+(\w+)");
        let Value::Object(m) = v else {
            panic!("expected empty object")
        };
        assert!(m.is_empty());
    }

    #[test]
    fn regex_parse_ignores_positional_groups() {
        let v = call("foo-42", r"(?P<named>\w+)-(\d+)");
        let Value::Object(m) = v else { panic!() };
        assert_eq!(m["named"], Value::String("foo".into()));
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn regex_parse_rejects_invalid_regex() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(
                None,
                "regex_parse",
                &[Value::String("x".into()), Value::String("(?P<bad>".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid regex"));
    }

    #[test]
    fn regex_parse_accepts_angle_bracket_form() {
        // `(?<name>…)` (no `P`) is accepted by regex / regex-lite and
        // must be mangled the same way.
        let v = call("hi", r"(?<g.h>\w+)");
        let Value::Object(m) = v else { panic!() };
        let Value::Object(g) = &m["g"] else { panic!() };
        assert_eq!(g["h"], Value::String("hi".into()));
    }
}
