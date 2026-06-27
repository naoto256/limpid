//! `parse_json(text[, defaults])` — JSON format parser primitive.
//!
//! Parses the input text as JSON and returns the top-level object as a
//! `Value::Object`. Non-object JSON (arrays, scalars) is wrapped under
//! the `_json` key so the return is always an object.

use anyhow::{Result, bail};

use crate::dsl::arena::EventArena;
use crate::dsl::value::{ObjectBuilder, Value};
use crate::dsl::value_json::json_to_value_in;

use super::val_to_str;
use crate::functions::{FunctionRegistry, ParserInfo};

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("parse_json", |arena, args, _event| {
        parse_json_impl(arena, args)
    });
    reg.register_parser(ParserInfo {
        namespace: None,
        name: "parse_json",
        produces: Vec::new(),
        wildcards: true,
        defaults_arg_indices: &[1],
        defaults_arg_extractor: None,
    });
}

fn parse_json_impl<'bump>(
    arena: &'bump EventArena<'bump>,
    args: &[Value<'bump>],
) -> Result<Value<'bump>> {
    let text = val_to_str(&args[0])?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse_json(): JSON parse error: {}", e))?;
    let parsed =
        json_to_value_in(&json, arena).map_err(|e| anyhow::anyhow!("parse_json(): {}", e))?;

    let result = match parsed {
        Value::Object(_) => parsed,
        // Non-object JSON: wrap under `_json` so the bare-statement
        // workspace-merge rule doesn't silently drop the value.
        other => {
            let mut wrap = ObjectBuilder::with_capacity(arena, 1);
            wrap.push("_json", other);
            wrap.finish()
        }
    };

    apply_defaults(arena, "parse_json", args.get(1), result)
}

/// Fill in keys from `defaults` that aren't already present on `value`
/// (which must be a `Value::Object`). Mirrors the pre-arena
/// `apply_defaults` semantics: input wins on key collisions, missing
/// keys come from defaults. The result is freshly built into `arena`.
pub(crate) fn apply_defaults<'bump>(
    arena: &'bump EventArena<'bump>,
    name: &'static str,
    defaults: Option<&Value<'bump>>,
    value: Value<'bump>,
) -> Result<Value<'bump>> {
    let entries = match value {
        Value::Object(e) => e,
        // `value` was constructed by the parser primitive above and is
        // always an Object — surface a hard error if a future caller
        // hands in something else.
        other => bail!(
            "{}(): internal error — apply_defaults expected Object, got {}",
            name,
            other.type_name()
        ),
    };

    let Some(d) = defaults else {
        return Ok(value);
    };
    let defaults_entries = match d {
        Value::Object(de) => *de,
        Value::Null => return Ok(value),
        other => bail!(
            "{}(): second argument must be a hash literal, got {}",
            name,
            other.type_name()
        ),
    };

    let mut builder = ObjectBuilder::with_capacity(arena, entries.len() + defaults_entries.len());
    for (k, v) in entries.iter() {
        builder.push(k, *v);
    }
    for (k, v) in defaults_entries.iter() {
        let already = entries.iter().any(|(ek, _)| *ek == *k);
        if !already {
            builder.push(k, *v);
        }
    }
    Ok(builder.finish())
}

pub(crate) fn type_name(v: &Value<'_>) -> &'static str {
    v.type_name()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::arena::EventArena;

    fn call(input: &str, defaults: Option<Value<'_>>) -> Result<String> {
        // Run the parser with a one-shot arena and convert the result
        // back to JSON for easy assertion. This lets us assert on the
        // wrapping contract without threading lifetimes through the
        // test API.
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let text_val = Value::String(arena.alloc_str(input));
        let args: Vec<Value<'_>> = match defaults {
            Some(d) => vec![text_val, d],
            None => vec![text_val],
        };
        let result = parse_json_impl(&arena, &args)?;
        Ok(crate::dsl::value_json::value_to_json(&result.to_owned_value())?.to_string())
    }

    #[test]
    fn object_root_returns_object_as_is() {
        let s = call(r#"{"a":1,"b":2}"#, None).unwrap();
        // Field order preserved; no _json wrap.
        assert_eq!(s, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn array_root_wraps_under_underscore_json() {
        // Documented contract: non-object roots wrap under `_json`
        // so the bare-statement workspace-merge rule doesn't silently
        // drop the value. A regression that returned the raw array
        // would break every downstream process pipeline that bare-
        // calls `parse_json(ingress)`.
        let s = call(r#"[1,2,3]"#, None).unwrap();
        assert_eq!(s, r#"{"_json":[1,2,3]}"#);
    }

    #[test]
    fn scalar_string_root_wraps_under_underscore_json() {
        let s = call(r#""hello""#, None).unwrap();
        assert_eq!(s, r#"{"_json":"hello"}"#);
    }

    #[test]
    fn scalar_number_root_wraps_under_underscore_json() {
        let s = call("42", None).unwrap();
        assert_eq!(s, r#"{"_json":42}"#);
    }

    #[test]
    fn scalar_bool_root_wraps_under_underscore_json() {
        let s = call("true", None).unwrap();
        assert_eq!(s, r#"{"_json":true}"#);
    }

    #[test]
    fn scalar_null_root_wraps_under_underscore_json() {
        let s = call("null", None).unwrap();
        assert_eq!(s, r#"{"_json":null}"#);
    }

    #[test]
    fn defaults_fill_only_missing_keys() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        // Build defaults = {a: "default_a", c: "default_c"}.
        let mut db = ObjectBuilder::with_capacity(&arena, 2);
        db.push("a", Value::String(arena.alloc_str("default_a")));
        db.push("c", Value::String(arena.alloc_str("default_c")));
        let defaults = db.finish();

        let text_val = Value::String(arena.alloc_str(r#"{"a":"input_a","b":"input_b"}"#));
        let result = parse_json_impl(&arena, &[text_val, defaults]).unwrap();
        let json = crate::dsl::value_json::value_to_json(&result.to_owned_value()).unwrap();
        // Input wins for `a` and `b`; default fills `c`.
        assert_eq!(json["a"], "input_a");
        assert_eq!(json["b"], "input_b");
        assert_eq!(json["c"], "default_c");
    }

    #[test]
    fn defaults_null_arg_is_noop() {
        // Documented behaviour: passing Null for defaults is the
        // same as omitting them; must not error.
        let s = call(r#"{"a":1}"#, Some(Value::Null)).unwrap();
        assert_eq!(s, r#"{"a":1}"#);
    }

    #[test]
    fn malformed_json_errors_with_parse_json_prefix() {
        let bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let text_val = Value::String(arena.alloc_str("{not valid json"));
        let err = parse_json_impl(&arena, &[text_val]).unwrap_err();
        assert!(err.to_string().contains("parse_json"), "got: {err}");
    }
}
