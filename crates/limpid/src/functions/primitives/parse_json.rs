//! `parse_json(text[, defaults])` — JSON format parser primitive.
//!
//! Parses the input text as JSON and returns the top-level object as a
//! `Value::Object`. Non-object JSON (arrays, scalars) is wrapped under
//! the `_json` key so the return is always an object — this keeps the
//! "bare statement merges an object into workspace" rule (see
//! [`crate::dsl::exec`]) from silently swallowing non-object payloads.
//!
//! The optional second argument is a hash of defaults: keys missing in
//! the parsed object are filled from here. This lets users declare the
//! expected shape inline and gives future analyzers a concrete schema.
//!
//! JSON is a *format*, not a schema — so this function lives in the
//! flat primitive namespace, not under `syslog.*` / `cef.*` / `ocsf.*`.
//! See Principle 5 in `design-principles.md`.

use anyhow::{Result, bail};

use crate::dsl::value::{Map, Value};
use crate::dsl::value_json::json_to_value;

use super::val_to_str;
use crate::functions::{FunctionRegistry, ParserInfo};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("parse_json", |args, _event| parse_json_impl(args));
    // Output keys are data-driven; the analyzer falls back to wildcard
    // unless the caller pins keys via the optional defaults HashLit.
    reg.register_parser(ParserInfo {
        namespace: None,
        name: "parse_json",
        produces: Vec::new(),
        wildcards: true,
    });
}

fn parse_json_impl(args: &[Value]) -> Result<Value> {
    // Arity is validated centrally by the registry (register_parser installs
    // the `(String, Object?) -> Object` signature). No manual check here.
    let text = val_to_str(&args[0])?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse_json(): JSON parse error: {}", e))?;
    let parsed = json_to_value(&json).map_err(|e| anyhow::anyhow!("parse_json(): {}", e))?;

    let mut map = match parsed {
        Value::Object(m) => m,
        // Non-object JSON: wrap under "_json" so the return is always an Object.
        other => {
            let mut m = Map::new();
            m.insert("_json".into(), other);
            m
        }
    };

    apply_defaults("parse_json", args.get(1), &mut map)?;
    Ok(Value::Object(map))
}

/// Fill in keys from `defaults` that aren't present in `map`.
pub(crate) fn apply_defaults(
    name: &'static str,
    defaults: Option<&Value>,
    map: &mut Map,
) -> Result<()> {
    let Some(v) = defaults else { return Ok(()) };
    match v {
        Value::Object(d) => {
            for (k, val) in d {
                map.entry(k.clone()).or_insert_with(|| val.clone());
            }
        }
        // Explicit null means "no defaults".
        Value::Null => {}
        other => bail!(
            "{}(): second argument must be a hash literal, got {}",
            name,
            type_name(other)
        ),
    }
    Ok(())
}

pub(crate) fn type_name(v: &Value) -> &'static str {
    v.type_name()
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
    fn parses_object() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "parse_json",
                &[Value::String(r#"{"a":1,"b":"x"}"#.into())],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else {
            panic!("expected Object")
        };
        assert_eq!(m["a"], Value::Int(1));
        assert_eq!(m["b"], Value::String("x".into()));
    }

    #[test]
    fn wraps_non_object_under_underscore_json() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(None, "parse_json", &[Value::String("[1,2,3]".into())], &e)
            .unwrap();
        let Value::Object(m) = result else {
            panic!("expected Object")
        };
        assert!(m["_json"].is_array());
    }

    #[test]
    fn rejects_invalid_json() {
        let reg = make_reg();
        let e = dummy_event();
        let err = reg
            .call(None, "parse_json", &[Value::String("not json".into())], &e)
            .unwrap_err();
        assert!(err.to_string().contains("JSON parse error"));
    }

    #[test]
    fn defaults_fill_missing_keys() {
        let reg = make_reg();
        let e = dummy_event();
        let defaults = Value::Object(
            [
                ("user".to_string(), Value::String("anon".into())),
                ("ip".to_string(), Value::String("0.0.0.0".into())),
            ]
            .into_iter()
            .collect(),
        );
        let result = reg
            .call(
                None,
                "parse_json",
                &[Value::String(r#"{"user":"alice"}"#.into()), defaults],
                &e,
            )
            .unwrap();
        let Value::Object(m) = result else {
            panic!("expected Object")
        };
        // Input wins for `user`
        assert_eq!(m["user"], Value::String("alice".into()));
        // Default fills `ip`
        assert_eq!(m["ip"], Value::String("0.0.0.0".into()));
    }

    #[test]
    fn defaults_null_is_ok() {
        let reg = make_reg();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "parse_json",
                &[Value::String(r#"{"x":1}"#.into()), Value::Null],
                &e,
            )
            .unwrap();
        assert!(result.is_object());
    }

    #[test]
    fn rejects_wrong_arity() {
        let reg = make_reg();
        let e = dummy_event();
        assert!(reg.call(None, "parse_json", &[], &e).is_err());
    }
}
