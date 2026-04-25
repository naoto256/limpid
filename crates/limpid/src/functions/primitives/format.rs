//! `format(template)` — expand `%{name}` placeholders against the
//! current event.
//!
//! `format` predates the general `${expr}` string interpolation and is
//! kept as a convenience for event-wide single-argument templates. New
//! code should prefer the DSL's string interpolation; this function
//! exists because tearing it out would break every existing user
//! config.
//!
//! Placeholder resolution is strict: anything that is not an
//! event-level name or `workspace.*` is an error. The old shorthand
//! that silently fell back to `workspace.<name>` is gone — it turned
//! typos into empty strings.

use anyhow::{Result, bail};

use crate::dsl::value::Value;
use crate::dsl::eval::value_to_string;

use super::val_to_str;
use crate::event::Event;
use crate::functions::{FunctionRegistry, FunctionSig};
use crate::modules::schema::FieldType;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register_with_sig(
        "format",
        FunctionSig::fixed(&[FieldType::String], FieldType::String),
        |args, event| {
            let template = val_to_str(&args[0])?;
            Ok(Value::String(expand_format_template(&template, event)?))
        },
    );
}

/// Expand `%{name}` placeholders in a format template against an event.
///
/// Supported placeholders (explicit only — no bare-name shorthand):
/// - `%{source}`, `%{received_at}`
/// - `%{egress}`, `%{ingress}` (UTF-8-clean only — bytes are rejected)
/// - `%{workspace.xxx}`, `%{workspace.xxx.yyy}` (nested workspace access)
fn expand_format_template(template: &str, event: &Event) -> Result<String> {
    let mut result = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '%' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            for c in chars.by_ref() {
                if c == '}' {
                    break;
                }
                var.push(c);
            }
            result.push_str(&resolve_format_var(&var, event)?);
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

fn resolve_format_var(var: &str, event: &Event) -> Result<String> {
    match var {
        "source" => Ok(event.source.ip().to_string()),
        "received_at" => Ok(event.received_at.to_rfc3339()),
        "egress" => bytes_field_to_string("egress", &event.egress),
        "ingress" => bytes_field_to_string("ingress", &event.ingress),
        v if v.starts_with("workspace.") => {
            let path: Vec<&str> = v["workspace.".len()..].split('.').collect();
            resolve_format_workspace(&path, &event.workspace)
        }
        // Anything else is a user error. The old shorthand silently did
        // a workspace lookup here; we now refuse and point at the
        // explicit form so typos don't turn into empty strings.
        other => bail!(
            "format(): unknown placeholder `%{{{}}}`; use `%{{workspace.{}}}` or one of the event-level names (source / received_at / ingress / egress)",
            other,
            other
        ),
    }
}

/// Stringify an `ingress` / `egress` byte buffer for placeholder
/// expansion. UTF-8-clean payloads (the historical case) interpolate
/// verbatim; non-UTF-8 payloads error rather than corrupt — the user
/// must convert explicitly via `to_string(ingress)` etc. per Bytes
/// design memo §3.
fn bytes_field_to_string(name: &str, buf: &bytes::Bytes) -> Result<String> {
    std::str::from_utf8(buf)
        .map(|s| s.to_string())
        .map_err(|_| anyhow::anyhow!(
            "format(): `%{{{}}}` is not valid UTF-8; convert explicitly via `to_string()`",
            name
        ))
}

fn resolve_format_workspace(
    path: &[&str],
    workspace: &std::collections::HashMap<String, Value>,
) -> Result<String> {
    let first = match workspace.get(path[0]) {
        Some(v) => v,
        None => return Ok(String::new()),
    };

    let mut current = first;
    for &segment in &path[1..] {
        match current {
            Value::Object(map) => {
                current = match map.get(segment) {
                    Some(v) => v,
                    None => return Ok(String::new()),
                };
            }
            // Per Bytes design §13: traversal through Bytes errors —
            // the analyzer rejects this statically, but at runtime we
            // surface it too rather than silently returning empty.
            Value::Bytes(_) => bail!(
                "format(): cannot traverse field `{}` on a bytes value",
                segment
            ),
            _ => return Ok(String::new()),
        }
    }

    match current {
        Value::Bytes(_) => bail!(
            "format(): bytes value cannot be interpolated; convert explicitly via `to_string()`"
        ),
        other => Ok(value_to_string(other)),
    }
}
