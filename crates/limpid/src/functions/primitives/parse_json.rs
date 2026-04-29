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
    });
}

fn parse_json_impl<'bump>(
    arena: &'bump EventArena<'bump>,
    args: &[Value<'bump>],
) -> Result<Value<'bump>> {
    let text = val_to_str(&args[0])?;
    let json: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse_json(): JSON parse error: {}", e))?;
    let parsed = json_to_value_in(&json, arena)
        .map_err(|e| anyhow::anyhow!("parse_json(): {}", e))?;

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
