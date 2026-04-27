//! Expression evaluator: evaluate DSL expressions against an Event.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::ast::{BinOp, Expr, ExprKind, TemplateFragment, UnaryOp};
use super::value::{Map, Value};
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
/// `egress`, `source`, `received_at`, `error`, `workspace`). Anything
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
            // Interpolated values are coerced to string via value_to_string;
            // Bytes interpolation is rejected per Bytes design §3 — users
            // must convert explicitly via `to_string()`.
            let mut out = String::new();
            for frag in fragments {
                match frag {
                    TemplateFragment::Literal(s) => out.push_str(s),
                    TemplateFragment::Interp(expr) => {
                        let v = eval_expr_with_scope(expr, event, funcs, scope)?;
                        if matches!(v, Value::Bytes(_)) {
                            bail!(
                                "cannot interpolate bytes into a string template (use to_string() first)"
                            );
                        }
                        out.push_str(&value_to_string(&v));
                    }
                }
            }
            Ok(Value::String(out))
        }
        ExprKind::IntLit(n) => Ok(Value::Int(*n)),
        ExprKind::FloatLit(f) => Ok(Value::Float(*f)),
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
            let mut map = Map::new();
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
                    // Per Bytes design §13, property traversal through a
                    // scalar (Bytes / String / number / bool) is an error
                    // — the analyzer flags it statically; at runtime we
                    // surface the same condition so dynamic data shapes
                    // don't silently return Null.
                    Value::Bytes(_) => bail!("cannot access field `{}` on a bytes value", field),
                    _ => Value::Null,
                };
            }
            Ok(current)
        }
        ExprKind::SwitchExpr { scrutinee, arms } => {
            // Expression-form switch: evaluate scrutinee, walk arms in
            // order, return the matching arm's body value. Default arm
            // (pattern = None) acts as the fallthrough; if no match and
            // no default, the expression's value is `Null` — mirrors
            // the partial-data convention used by `regex_extract`,
            // `table_lookup`, etc.
            let target = eval_expr_with_scope(scrutinee, event, funcs, scope)?;
            for arm in arms {
                match &arm.pattern {
                    None => return eval_expr_with_scope(&arm.body, event, funcs, scope),
                    Some(pat) => {
                        let pat_val = eval_expr_with_scope(pat, event, funcs, scope)?;
                        if values_equal(&target, &pat_val) {
                            return eval_expr_with_scope(&arm.body, event, funcs, scope);
                        }
                    }
                }
            }
            Ok(Value::Null)
        }
    }
}

/// Equality check used by [`ExprKind::SwitchExpr`] arm matching. Mirrors
/// the statement-form switch's match semantics: integer / float
/// comparison normalised through `f64`, strings byte-equal, bools
/// direct, null only matches null.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => (*x as f64) == *y,
        (Value::String(x), Value::String(y)) => x == y,
        _ => false,
    }
}

/// Convert raw bytes into a runtime [`Value`].
///
/// UTF-8-clean payloads surface as `Value::String` — this preserves the
/// historical limpid behaviour for text-shaped data (syslog, CEF, JSON).
/// Non-UTF-8 payloads now surface as `Value::Bytes` rather than being
/// silently corrupted by `from_utf8_lossy` (the previous behaviour).
fn bytes_to_value(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => Value::Bytes(bytes::Bytes::copy_from_slice(bytes)),
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
        Some("received_at") => Ok(Value::Timestamp(event.received_at)),
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
            Ok(Value::Object(
                event
                    .workspace
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ))
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
    let first = workspace.get(&parts[0]).cloned().unwrap_or(Value::Null);
    if parts.len() == 1 {
        return Ok(first);
    }
    resolve_workspace_path(&parts[1..], &first)
}

fn resolve_workspace_path(parts: &[String], value: &Value) -> Result<Value> {
    if parts.is_empty() {
        return Ok(value.clone());
    }
    match value {
        Value::Object(map) => {
            let next = map.get(&parts[0]).cloned().unwrap_or(Value::Null);
            resolve_workspace_path(&parts[1..], &next)
        }
        Value::Bytes(_) => bail!("cannot access field `{}` on a bytes value", parts[0]),
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
        BinOp::And => Ok(Value::Bool(left.is_truthy() && right.is_truthy())),
        BinOp::Or => Ok(Value::Bool(left.is_truthy() || right.is_truthy())),
        BinOp::Add => add_values(left, right),
        BinOp::Sub => numeric_op("subtract", left, right, |a, b| a - b),
        BinOp::Mul => numeric_op("multiply", left, right, |a, b| a * b),
        BinOp::Div => numeric_op(
            "divide",
            left,
            right,
            |a, b| if b != 0.0 { a / b } else { 0.0 },
        ),
        BinOp::Mod => numeric_op(
            "modulo",
            left,
            right,
            |a, b| if b != 0.0 { a % b } else { 0.0 },
        ),
    }
}

fn eval_unary_op(op: UnaryOp, val: &Value) -> Result<Value> {
    match op {
        UnaryOp::Not => Ok(Value::Bool(!val.is_truthy())),
        UnaryOp::Neg => {
            if matches!(val, Value::Bytes(_)) {
                bail!("cannot negate a bytes value");
            }
            let n = value_to_f64(val);
            Ok(numeric_value_from_f64(-n))
        }
    }
}

// ---------------------------------------------------------------------------
// Value helpers
// ---------------------------------------------------------------------------

/// Value equality used for both the `==`/`!=` binary operators and
/// `switch` pattern matching. Numbers compare by their numeric value so
/// `1 == 1.0` agrees; Bytes compares byte-wise but never matches a
/// String of the same UTF-8 spelling (per Bytes design §1).
pub fn values_match(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Int(a), Value::Float(b)) | (Value::Float(b), Value::Int(a)) => (*a as f64) == *b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bytes(a), Value::Bytes(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Null, Value::Null) => true,
        (Value::Array(a), Value::Array(b)) => a == b,
        (Value::Object(a), Value::Object(b)) => a == b,
        _ => false,
    }
}

/// True if `v` is truthy under DSL rules. Re-exported for callers that
/// previously imported this from `eval` directly; the canonical
/// implementation lives on [`Value::is_truthy`].
pub fn is_truthy(v: &Value) -> bool {
    v.is_truthy()
}

/// String coercion used by templates, format() placeholders, and any
/// other user-facing primitive that needs a printable representation.
/// Bytes is not coerced — text helpers reject it upstream so we never
/// reach here with a Bytes value, but the fallback returns a placeholder
/// shape rather than a UTF-8-lossy string to make any bug surface
/// loudly.
pub fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => {
            if n.fract() == 0.0 && n.is_finite() {
                format!("{}", *n as i64)
            } else {
                n.to_string()
            }
        }
        Value::Bytes(_) => "<bytes>".to_string(),
        Value::Timestamp(dt) => dt.to_rfc3339(),
        Value::Array(a) => {
            let mut s = String::from("[");
            for (i, item) in a.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&value_to_string(item));
            }
            s.push(']');
            s
        }
        Value::Object(m) => {
            let mut s = String::from("{");
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(k);
                s.push_str(": ");
                s.push_str(&value_to_string(v));
            }
            s.push('}');
            s
        }
    }
}

fn value_to_f64(v: &Value) -> f64 {
    match v {
        Value::Int(n) => *n as f64,
        Value::Float(n) => *n,
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    }
}

fn compare_values(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Bytes(a), Value::Bytes(b)) => Some(a.as_ref().cmp(b.as_ref())),
        _ => None,
    }
}

/// `+` operator. String concat (existing behaviour), Bytes concat (new
/// for v0.5.0 Bytes design §2), numeric otherwise. Mixed-type Bytes
/// participation is an error.
fn add_values(left: &Value, right: &Value) -> Result<Value> {
    if matches!(left, Value::Bytes(_)) || matches!(right, Value::Bytes(_)) {
        return match (left, right) {
            (Value::Bytes(a), Value::Bytes(b)) => {
                let mut buf = Vec::with_capacity(a.len() + b.len());
                buf.extend_from_slice(a);
                buf.extend_from_slice(b);
                Ok(Value::Bytes(bytes::Bytes::from(buf)))
            }
            _ => bail!(
                "cannot concatenate {} and {} (only bytes + bytes is supported)",
                left.type_name(),
                right.type_name()
            ),
        };
    }
    if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
        return Ok(Value::String(format!(
            "{}{}",
            value_to_string(left),
            value_to_string(right)
        )));
    }
    numeric_op("add", left, right, |a, b| a + b)
}

fn numeric_op(op: &str, left: &Value, right: &Value, f: impl Fn(f64, f64) -> f64) -> Result<Value> {
    if matches!(left, Value::Bytes(_)) || matches!(right, Value::Bytes(_)) {
        bail!("cannot {} a bytes value", op);
    }
    let a = value_to_f64(left);
    let b = value_to_f64(right);
    Ok(numeric_value_from_f64(f(a, b)))
}

/// Convert an `f64` result back into a `Value`. Integer-valued finites
/// land as `Value::Int` so subsequent equality / comparison agrees with
/// the integer path — `numeric_op` collapses int and float arithmetic
/// onto f64 internally for math, but the type that surfaces should
/// match user expectations.
fn numeric_value_from_f64(n: f64) -> Value {
    if n.is_finite() && n.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&n) {
        Value::Int(n as i64)
    } else if n.is_finite() {
        Value::Float(n)
    } else {
        Value::Null
    }
}
