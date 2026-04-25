//! Expression evaluator: evaluate DSL expressions against an Event.

use std::collections::HashMap;

use anyhow::{Result, bail};
use serde_json::Value;

use super::ast::{BinOp, Expr, ExprKind, TemplateFragment, UnaryOp};
use crate::event::Event;
use crate::functions::FunctionRegistry;

/// Per-process scratch bindings introduced by `let <name> = expr`.
///
/// `let` has process scope, not hop scope — it exists precisely because
/// `workspace` is contract-ish (pipeline-local scratch that survives
/// across process boundaries within the pipeline), whereas a `let`
/// binding is "material for building a single workspace write" and is
/// dropped when the process body returns.
///
/// The AST's [`super::ast::ProcessStatement::LetBinding`] calls
/// [`LocalScope::bind`] as statements execute; expression evaluation
/// ([`eval_expr_with_scope`]) consults the same scope when resolving
/// bare identifiers.
///
/// Call semantics: when a user-defined process calls another process,
/// callers pass a *fresh* scope (or [`LocalScope::new`]). Locals do not
/// leak across process calls. This matches the mental model of
/// `workspace`-as-material and `let`-as-scratch — callee scratches are
/// callee-only.
#[derive(Debug, Clone, Default)]
pub struct LocalScope {
    bindings: HashMap<String, Value>,
}

impl LocalScope {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind (or shadow-rebind) `name` to `value`. The previous value, if
    /// any, is discarded — by design this is the only way to "reassign"
    /// a let (`let x = 1; let x = 2`), matching Rust's shadowing rules.
    pub fn bind(&mut self, name: &str, value: Value) {
        self.bindings.insert(name.to_string(), value);
    }

    /// Return the current binding for `name`, or `None` if not bound.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.bindings.get(name)
    }
}

/// Evaluate an expression without any let bindings.
///
/// Convenience wrapper around [`eval_expr_with_scope`] for call sites
/// that don't have a `LocalScope` (e.g. pipeline-level branches,
/// file-output templates, tests). The evaluator treats an unbound bare
/// identifier as an error regardless of scope; this wrapper merely
/// saves callers from constructing an empty scope.
pub fn eval_expr(expr: &Expr, event: &Event, funcs: &FunctionRegistry) -> Result<Value> {
    let scope = LocalScope::new();
    eval_expr_with_scope(expr, event, funcs, &scope)
}

/// Evaluate an expression against an Event, consulting `scope` for bare
/// identifier resolution. Bare `x` (not `workspace.x`) resolves to the
/// `let x = ...` binding currently in scope; if there is no such
/// binding, resolution falls through to Event metadata (`ingress`,
/// `egress`, `source`, `timestamp`, `error`, `workspace`). Anything
/// else produces an "unknown identifier" error.
pub fn eval_expr_with_scope(
    expr: &Expr,
    event: &Event,
    funcs: &FunctionRegistry,
    scope: &LocalScope,
) -> Result<Value> {
    match &expr.kind {
        ExprKind::StringLit(s) => Ok(Value::String(s.clone())),
        ExprKind::Template(fragments) => {
            // Render template fragments against the current event.
            // Interpolated values are coerced to string via value_to_string
            // so that `${source}` (String) and `${workspace.foo}` (arbitrary)
            // both interpolate cleanly.
            let mut out = String::new();
            for frag in fragments {
                match frag {
                    TemplateFragment::Literal(s) => out.push_str(s),
                    TemplateFragment::Interp(expr) => {
                        let v = eval_expr_with_scope(expr, event, funcs, scope)?;
                        out.push_str(&value_to_string(&v));
                    }
                }
            }
            Ok(Value::String(out))
        }
        ExprKind::IntLit(n) => Ok(Value::Number((*n).into())),
        ExprKind::FloatLit(f) => Ok(Value::Number(
            serde_json::Number::from_f64(*f).unwrap_or(0.into()),
        )),
        ExprKind::BoolLit(b) => Ok(Value::Bool(*b)),
        ExprKind::Null => Ok(Value::Null),

        ExprKind::Ident(parts) => resolve_ident(parts, event, scope),

        ExprKind::FuncCall {
            namespace,
            name,
            args,
        } => {
            let evaluated_args: Vec<Value> = args
                .iter()
                .map(|a| eval_expr_with_scope(a, event, funcs, scope))
                .collect::<Result<Vec<_>>>()?;
            funcs.call(namespace.as_deref(), name, &evaluated_args, event)
        }

        ExprKind::BinOp(left, op, right) => {
            let lv = eval_expr_with_scope(left, event, funcs, scope)?;
            let rv = eval_expr_with_scope(right, event, funcs, scope)?;
            eval_bin_op(&lv, *op, &rv)
        }

        ExprKind::UnaryOp(op, operand) => {
            let v = eval_expr_with_scope(operand, event, funcs, scope)?;
            eval_unary_op(*op, &v)
        }

        ExprKind::HashLit(entries) => {
            let mut map = serde_json::Map::new();
            for (key, val_expr) in entries {
                let val = eval_expr_with_scope(val_expr, event, funcs, scope)?;
                map.insert(key.clone(), val);
            }
            Ok(Value::Object(map))
        }

        ExprKind::ArrayLit(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval_expr_with_scope(item, event, funcs, scope)?);
            }
            Ok(Value::Array(out))
        }

        ExprKind::PropertyAccess(base, path) => {
            let mut current = eval_expr_with_scope(base, event, funcs, scope)?;
            for field in path {
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

fn resolve_ident(parts: &[String], event: &Event, scope: &LocalScope) -> Result<Value> {
    // Single-segment idents: check let scope first, then Event metadata.
    // `workspace.*` must always be written explicitly — there is no
    // "bare field lookup" fallback into workspace.
    if parts.len() == 1
        && let Some(v) = scope.get(&parts[0])
    {
        return Ok(v.clone());
    }

    match parts.first().map(|s| s.as_str()) {
        Some("ingress") => Ok(bytes_to_value(&event.ingress)),
        Some("egress") => Ok(bytes_to_value(&event.egress)),
        Some("timestamp") => Ok(Value::String(event.timestamp.to_rfc3339())),
        Some("source") => Ok(Value::String(event.source.ip().to_string())),
        Some("error") => {
            // `error` is available inside catch blocks, stored as workspace._error
            Ok(event
                .workspace
                .get("_error")
                .cloned()
                .unwrap_or(Value::Null))
        }
        Some("workspace") if parts.len() == 1 => {
            // `workspace` alone — return the whole workspace map
            Ok(Value::Object(event.workspace.clone().into_iter().collect()))
        }
        Some("workspace") => {
            // `workspace.xxx.yyy` — direct lookup, no clone of entire map
            let rest = &parts[1..];
            resolve_workspace_direct(rest, &event.workspace)
        }
        _ => {
            bail!("unknown identifier: {}", parts.join("."))
        }
    }
}

/// Direct lookup into event.workspace HashMap — no clone.
fn resolve_workspace_direct(
    parts: &[String],
    workspace: &std::collections::HashMap<String, Value>,
) -> Result<Value> {
    let first = workspace.get(&parts[0]).unwrap_or(&Value::Null);
    if parts.len() == 1 {
        return Ok(first.clone());
    }
    resolve_workspace_path(&parts[1..], first)
}

fn resolve_workspace_path(parts: &[String], value: &Value) -> Result<Value> {
    if parts.is_empty() {
        return Ok(value.clone());
    }
    match value {
        Value::Object(map) => {
            let next = map.get(&parts[0]).unwrap_or(&Value::Null);
            resolve_workspace_path(&parts[1..], next)
        }
        _ => Ok(Value::Null),
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

fn eval_bin_op(left: &Value, op: BinOp, right: &Value) -> Result<Value> {
    match op {
        BinOp::Eq => Ok(Value::Bool(values_match(left, right))),
        BinOp::Ne => Ok(Value::Bool(!values_match(left, right))),
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
            // `"[" + tag + "] " + message`.
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

/// Value equality used for both the `==`/`!=` binary operators and
/// `switch` pattern matching. Numbers compare by their `f64` value so
/// `1` and `1.0` agree; strings and other shapes fall through to the
/// structural `PartialEq` impl.
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
