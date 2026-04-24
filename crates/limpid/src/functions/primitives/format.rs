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
use serde_json::Value;

use super::val_to_str;
use crate::event::Event;
use crate::functions::FunctionRegistry;

pub fn register(reg: &mut FunctionRegistry) {
    reg.register("format", |args, event| {
        if args.len() != 1 {
            bail!("format() expects 1 argument (template string)");
        }
        let template = val_to_str(&args[0]);
        Ok(Value::String(expand_format_template(&template, event)?))
    });
}

/// Expand `%{name}` placeholders in a format template against an event.
///
/// Supported placeholders (explicit only — no bare-name shorthand):
/// - `%{source}`, `%{timestamp}`
/// - `%{egress}`, `%{ingress}`
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
        "timestamp" => Ok(event.timestamp.to_rfc3339()),
        "egress" => Ok(String::from_utf8_lossy(&event.egress).into_owned()),
        "ingress" => Ok(String::from_utf8_lossy(&event.ingress).into_owned()),
        v if v.starts_with("workspace.") => {
            let path: Vec<&str> = v["workspace.".len()..].split('.').collect();
            Ok(resolve_format_workspace(&path, &event.workspace))
        }
        // Anything else is a user error. The old shorthand silently did
        // a workspace lookup here; we now refuse and point at the
        // explicit form so typos don't turn into empty strings.
        other => bail!(
            "format(): unknown placeholder `%{{{}}}`; use `%{{workspace.{}}}` or one of the event-level names (source / timestamp / ingress / egress)",
            other,
            other
        ),
    }
}

fn resolve_format_workspace(
    path: &[&str],
    workspace: &std::collections::HashMap<String, serde_json::Value>,
) -> String {
    let first = match workspace.get(path[0]) {
        Some(v) => v,
        None => return String::new(),
    };

    let mut current = first;
    for &segment in &path[1..] {
        match current {
            serde_json::Value::Object(map) => {
                current = match map.get(segment) {
                    Some(v) => v,
                    None => return String::new(),
                };
            }
            _ => return String::new(),
        }
    }

    match current {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}
