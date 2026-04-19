//! Expression evaluator: evaluate DSL expressions against an Event.

use anyhow::{Result, bail};
use serde_json::Value;

use super::ast::{BinOp, Expr, TemplateFragment, UnaryOp};
use crate::event::Event;
use crate::functions::FunctionRegistry;

/// Evaluate an expression in the context of an Event, producing a serde_json::Value.
pub fn eval_expr(expr: &Expr, event: &Event, funcs: &FunctionRegistry) -> Result<Value> {
    match expr {
        Expr::StringLit(s) => Ok(Value::String(s.clone())),
        Expr::Template(fragments) => {
            // Render template fragments against the current event.
            // Interpolated values are coerced to string via value_to_string
            // so that `${facility}` (Number) and `${fields.foo}` (arbitrary)
            // both interpolate cleanly.
            let mut out = String::new();
            for frag in fragments {
                match frag {
                    TemplateFragment::Literal(s) => out.push_str(s),
                    TemplateFragment::Interp(expr) => {
                        let v = eval_expr(expr, event, funcs)?;
                        out.push_str(&value_to_string(&v));
                    }
                }
            }
            Ok(Value::String(out))
        }
        Expr::IntLit(n) => Ok(Value::Number((*n).into())),
        Expr::FloatLit(f) => Ok(Value::Number(
            serde_json::Number::from_f64(*f).unwrap_or(0.into()),
        )),
        Expr::BoolLit(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),

        Expr::Ident(parts) => resolve_ident(parts, event),

        Expr::FuncCall(name, args) => {
            let evaluated_args: Vec<Value> = args
                .iter()
                .map(|a| eval_expr(a, event, funcs))
                .collect::<Result<Vec<_>>>()?;
            funcs.call(name, &evaluated_args, event)
        }

        Expr::BinOp(left, op, right) => {
            let lv = eval_expr(left, event, funcs)?;
            let rv = eval_expr(right, event, funcs)?;
            eval_bin_op(&lv, *op, &rv)
        }

        Expr::UnaryOp(op, operand) => {
            let v = eval_expr(operand, event, funcs)?;
            eval_unary_op(*op, &v)
        }

        Expr::HashLit(entries) => {
            let mut map = serde_json::Map::new();
            for (key, val_expr) in entries {
                let val = eval_expr(val_expr, event, funcs)?;
                map.insert(key.clone(), val);
            }
            Ok(Value::Object(map))
        }

        Expr::PropertyAccess(base, fields) => {
            let mut current = eval_expr(base, event, funcs)?;
            for field in fields {
                current = match current {
                    Value::Object(ref map) => map.get(field).cloned().unwrap_or(Value::Null),
                    _ => Value::Null,
                };
            }
            Ok(current)
        }
    }
}

/// Resolve a dotted identifier path against an Event.
/// Convert Bytes to Value::String, avoiding allocation if already valid UTF-8.
fn bytes_to_value(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
    }
}

fn resolve_ident(parts: &[String], event: &Event) -> Result<Value> {
    match parts.first().map(|s| s.as_str()) {
        Some("raw") => Ok(bytes_to_value(&event.raw)),
        Some("message") => Ok(bytes_to_value(&event.message)),
        Some("timestamp") => Ok(Value::String(event.timestamp.to_rfc3339())),
        Some("source") => Ok(Value::String(event.source.ip().to_string())),
        Some("severity") => match event.severity {
            Some(s) => Ok(Value::Number(s.into())),
            None => Ok(Value::Null),
        },
        Some("facility") => match event.facility {
            Some(f) => Ok(Value::Number(f.into())),
            None => Ok(Value::Null),
        },
        Some("error") => {
            // `error` is available inside catch blocks, stored as fields._error
            Ok(event.fields.get("_error").cloned().unwrap_or(Value::Null))
        }
        Some("fields") if parts.len() == 1 => {
            // `fields` alone — return the whole fields map
            Ok(Value::Object(event.fields.clone().into_iter().collect()))
        }
        Some("fields") => {
            // `fields.xxx.yyy` — direct lookup, no clone of entire map
            let rest = &parts[1..];
            resolve_fields_direct(rest, &event.fields)
        }
        _ => {
            bail!("unknown identifier: {}", parts.join("."))
        }
    }
}

/// Direct lookup into event.fields HashMap — no clone.
fn resolve_fields_direct(
    parts: &[String],
    fields: &std::collections::HashMap<String, Value>,
) -> Result<Value> {
    let first = fields.get(&parts[0]).unwrap_or(&Value::Null);
    if parts.len() == 1 {
        return Ok(first.clone());
    }
    resolve_fields_path(&parts[1..], first)
}

fn resolve_fields_path(parts: &[String], value: &Value) -> Result<Value> {
    if parts.is_empty() {
        return Ok(value.clone());
    }
    match value {
        Value::Object(map) => {
            let next = map.get(&parts[0]).unwrap_or(&Value::Null);
            resolve_fields_path(&parts[1..], next)
        }
        _ => Ok(Value::Null),
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

fn eval_bin_op(left: &Value, op: BinOp, right: &Value) -> Result<Value> {
    match op {
        BinOp::Eq => Ok(Value::Bool(values_equal(left, right))),
        BinOp::Ne => Ok(Value::Bool(!values_equal(left, right))),
        BinOp::Lt => Ok(Value::Bool(
            compare_values(left, right) == Some(std::cmp::Ordering::Less),
        )),
        BinOp::Le => Ok(Value::Bool(matches!(
            compare_values(left, right),
            Some(std::cmp::Ordering::Less | std::cmp::Ordering::Equal)
        ))),
        BinOp::Gt => Ok(Value::Bool(
            compare_values(left, right) == Some(std::cmp::Ordering::Greater),
        )),
        BinOp::Ge => Ok(Value::Bool(matches!(
            compare_values(left, right),
            Some(std::cmp::Ordering::Greater | std::cmp::Ordering::Equal)
        ))),
        BinOp::And => Ok(Value::Bool(is_truthy(left) && is_truthy(right))),
        BinOp::Or => Ok(Value::Bool(is_truthy(left) || is_truthy(right))),
        BinOp::Add => {
            // If either side is a String, concatenate as strings. This gives
            // the usual dynamic-language intuition for building messages like
            // `"[" + severity + "] " + message`.
            if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
                Ok(Value::String(format!(
                    "{}{}",
                    value_to_string(left),
                    value_to_string(right)
                )))
            } else {
                numeric_op(left, right, |a, b| a + b)
            }
        }
        BinOp::Sub => numeric_op(left, right, |a, b| a - b),
        BinOp::Mul => numeric_op(left, right, |a, b| a * b),
        BinOp::Div => numeric_op(left, right, |a, b| if b != 0.0 { a / b } else { 0.0 }),
        BinOp::Mod => numeric_op(left, right, |a, b| if b != 0.0 { a % b } else { 0.0 }),
    }
}

fn eval_unary_op(op: UnaryOp, val: &Value) -> Result<Value> {
    match op {
        UnaryOp::Not => Ok(Value::Bool(!is_truthy(val))),
        UnaryOp::Neg => {
            let n = value_to_f64(val);
            Ok(serde_json::Number::from_f64(-n)
                .map(Value::Number)
                .unwrap_or(Value::Null))
        }
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

pub fn values_match(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        _ => left == right,
    }
}

pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn value_to_f64(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    }
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::String(a), Value::String(b)) => a == b,
        _ => left == right,
    }
}

fn compare_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => {
            let a = a.as_f64().unwrap_or(0.0);
            let b = b.as_f64().unwrap_or(0.0);
            a.partial_cmp(&b)
        }
        _ => None,
    }
}

fn numeric_op(left: &Value, right: &Value, f: impl Fn(f64, f64) -> f64) -> Result<Value> {
    let a = value_to_f64(left);
    let b = value_to_f64(right);
    let result = f(a, b);
    Ok(serde_json::Number::from_f64(result)
        .map(Value::Number)
        .unwrap_or(Value::Null))
}
