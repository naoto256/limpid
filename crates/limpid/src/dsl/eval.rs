//! Expression evaluator: evaluate DSL expressions against a borrowed event.
//!
//! Every value handed back lives in the per-event arena; the evaluator
//! never escapes the `'bump` lifetime to the heap. String coercions
//! that need to leave the arena (e.g. [`value_to_string`] feeding a
//! template render whose final output is a fresh `String`) heap-alloc
//! at the boundary site, not here.

use std::collections::HashMap;

use anyhow::{Result, bail};

use super::arena::EventArena;
use super::ast::{BinOp, Expr, ExprKind, TemplateFragment, UnaryOp};
use super::value::{ArrayBuilder, ObjectBuilder, Value};
use crate::event::BorrowedEvent;
use crate::functions::FunctionRegistry;

/// Local-scope variable bindings introduced by `let <name> = expr`.
///
/// Used by both:
///
/// - **Process bodies**: each [`super::ast::ProcessStatement::LetBinding`]
///   calls [`LocalScope::bind`] as statements execute; the scope lives
///   for the duration of the process body and is dropped when the body
///   returns. `let` has process scope (not hop scope), distinguishing
///   it from `workspace` (pipeline-local scratch surviving across
///   process boundaries).
/// - **Function bodies**: [`FunctionRegistry::call`] constructs a
///   fresh `LocalScope`, binds the call arguments to the declared
///   parameters, then evaluates each `let` in [`super::ast::FuncBody`]
///   in declaration order before the trailing return expression. The
///   scope is discarded when the call returns.
///
/// Bound values borrow at `'bump` from the per-event arena, matching
/// the lifetime of every other transient value flowing through the
/// evaluator.
///
/// Call semantics: when a user-defined process or function calls
/// another, the callee receives a *fresh* scope (or
/// [`LocalScope::new`]). Locals do not leak across calls — callee
/// scratches are callee-only.
#[derive(Debug, Clone, Default)]
pub struct LocalScope<'bump> {
    bindings: HashMap<String, Value<'bump>>,
}

impl<'bump> LocalScope<'bump> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name` to `value`. The previous value, if any, is discarded.
    ///
    /// limpid models `let` as the **assignment form** for local-scope
    /// variables (not as a separate "declaration" step). `let x = 1;
    /// let x = 2` is two assignments to the same `x` — there is no
    /// `let mut` / re-assign distinction, and no separate scope for
    /// rebinding. Internally this is `HashMap::insert` overwriting the
    /// prior value, but the user-facing semantics is "assignment to a
    /// local-scope variable", not "shadowing".
    pub fn bind(&mut self, name: &str, value: Value<'bump>) {
        self.bindings.insert(name.to_string(), value);
    }

    /// Return the current binding for `name`, or `None` if not bound.
    pub fn get(&self, name: &str) -> Option<Value<'bump>> {
        self.bindings.get(name).copied()
    }
}

/// Evaluate an expression without any let bindings.
///
/// Convenience wrapper around [`eval_expr_with_scope`] for call sites
/// that don't have a `LocalScope` (e.g. pipeline-level branches,
/// file-output templates, tests). The evaluator treats an unbound bare
/// identifier as an error regardless of scope; this wrapper merely
/// saves callers from constructing an empty scope.
pub fn eval_expr<'bump>(
    expr: &Expr,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    let scope = LocalScope::new();
    eval_expr_with_scope(expr, event, funcs, &scope, arena)
}

/// Evaluate an expression against an Event, consulting `scope` for bare
/// identifier resolution. Bare `x` (not `workspace.x`) resolves to the
/// `let x = ...` binding currently in scope; if there is no such
/// binding, resolution falls through to Event metadata (`ingress`,
/// `egress`, `source`, `received_at`, `error`, `workspace`). Anything
/// else produces an "unknown identifier" error.
pub fn eval_expr_with_scope<'bump>(
    expr: &Expr,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    match &expr.kind {
        ExprKind::StringLit(s) => Ok(Value::String(arena.alloc_str(s))),
        ExprKind::Template(fragments) => {
            // Render template fragments against the current event.
            // Interpolated values are coerced to string via
            // value_to_string; Bytes interpolation is rejected per
            // Bytes design §3 — users must convert explicitly via
            // `to_string()`. The composed result lands in the arena
            // so subsequent assignments stay arena-local.
            let mut out = String::new();
            for frag in fragments {
                match frag {
                    TemplateFragment::Literal(s) => out.push_str(s),
                    TemplateFragment::Interp(expr) => {
                        let v = eval_expr_with_scope(expr, event, funcs, scope, arena)?;
                        if matches!(v, Value::Bytes(_)) {
                            bail!(
                                "cannot interpolate bytes into a string template (use to_string() first)"
                            );
                        }
                        out.push_str(&value_to_string(&v));
                    }
                }
            }
            Ok(Value::String(arena.alloc_str(&out)))
        }
        ExprKind::IntLit(n) => Ok(Value::Int(*n)),
        ExprKind::FloatLit(f) => Ok(Value::Float(*f)),
        ExprKind::BoolLit(b) => Ok(Value::Bool(*b)),
        ExprKind::Null => Ok(Value::Null),

        ExprKind::Ident(parts) => resolve_ident(parts, event, scope, arena),

        ExprKind::FuncCall {
            namespace,
            name,
            args,
        } => {
            let mut evaluated_args =
                bumpalo::collections::Vec::with_capacity_in(args.len(), arena.bump());
            for a in args {
                evaluated_args.push(eval_expr_with_scope(a, event, funcs, scope, arena)?);
            }
            funcs.call(namespace.as_deref(), name, &evaluated_args, event, arena)
        }

        ExprKind::BinOp(left, op, right) => {
            let lv = eval_expr_with_scope(left, event, funcs, scope, arena)?;
            let rv = eval_expr_with_scope(right, event, funcs, scope, arena)?;
            eval_bin_op(&lv, *op, &rv, arena)
        }

        ExprKind::UnaryOp(op, operand) => {
            let v = eval_expr_with_scope(operand, event, funcs, scope, arena)?;
            eval_unary_op(*op, &v)
        }

        ExprKind::HashLit(entries) => {
            let mut builder = ObjectBuilder::with_capacity(arena, entries.len());
            for (key, val_expr) in entries {
                let val = eval_expr_with_scope(val_expr, event, funcs, scope, arena)?;
                builder.push_str(key, val);
            }
            Ok(builder.finish())
        }

        ExprKind::ArrayLit(items) => {
            let mut builder = ArrayBuilder::with_capacity(arena, items.len());
            for item in items {
                builder.push(eval_expr_with_scope(item, event, funcs, scope, arena)?);
            }
            Ok(builder.finish())
        }

        ExprKind::PropertyAccess(base, path) => {
            let mut current = eval_expr_with_scope(base, event, funcs, scope, arena)?;
            for field in path {
                current = match current {
                    Value::Object(entries) => entries
                        .iter()
                        .find(|(k, _)| *k == field.as_str())
                        .map(|(_, v)| *v)
                        .unwrap_or(Value::Null),
                    // Per Bytes design §13, property traversal through
                    // a scalar (Bytes / String / number / bool) is an
                    // error — the analyzer flags it statically; at
                    // runtime we surface the same condition so dynamic
                    // data shapes don't silently return Null.
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
            let target = eval_expr_with_scope(scrutinee, event, funcs, scope, arena)?;
            for arm in arms {
                match &arm.pattern {
                    None => return eval_expr_with_scope(&arm.body, event, funcs, scope, arena),
                    Some(pat) => {
                        let pat_val = eval_expr_with_scope(pat, event, funcs, scope, arena)?;
                        if values_equal(&target, &pat_val) {
                            return eval_expr_with_scope(&arm.body, event, funcs, scope, arena);
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
fn values_equal(a: &Value<'_>, b: &Value<'_>) -> bool {
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
fn bytes_to_value<'bump>(bytes: &[u8], arena: &EventArena<'bump>) -> Value<'bump> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Value::String(arena.alloc_str(s)),
        Err(_) => Value::Bytes(arena.alloc_bytes(bytes)),
    }
}

fn resolve_ident<'bump>(
    parts: &[String],
    event: &BorrowedEvent<'bump>,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    // Single-segment idents: check let scope first, then Event metadata.
    // `workspace.*` must always be written explicitly — there is no
    // "bare field lookup" fallback into workspace.
    if parts.len() == 1
        && let Some(v) = scope.get(&parts[0])
    {
        return Ok(v);
    }

    match parts.first().map(|s| s.as_str()) {
        Some("ingress") => Ok(bytes_to_value(&event.ingress, arena)),
        Some("egress") => Ok(bytes_to_value(&event.egress, arena)),
        Some("received_at") => Ok(Value::Timestamp(event.received_at)),
        // `source` is an Object with `.ip` (String) and `.port` (Int).
        // Bare `source` returns the whole object so a renderer can write
        // `${source.ip}:${source.port}` for inject-compatible output.
        // Pre-0.5.6 this returned the IP as a flat String — operator
        // configs comparing `source == "10.0.0.1"` need to migrate to
        // `source.ip == "10.0.0.1"`.
        Some("source") if parts.len() == 1 => {
            let mut builder = ObjectBuilder::with_capacity(arena, 2);
            let ip_str = arena.alloc_str(&event.source.ip().to_string());
            builder.push("ip", Value::String(ip_str));
            builder.push("port", Value::Int(event.source.port() as i64));
            Ok(builder.finish())
        }
        Some("source") if parts.len() == 2 && parts[1] == "ip" => {
            Ok(Value::String(arena.alloc_str(&event.source.ip().to_string())))
        }
        Some("source") if parts.len() == 2 && parts[1] == "port" => {
            Ok(Value::Int(event.source.port() as i64))
        }
        Some("source") => bail!(
            "unknown ident path: source.{} — only source.ip / source.port are defined",
            parts[1..].join(".")
        ),
        Some("error") => {
            // `error` is available inside catch blocks, stored as workspace._error
            Ok(event.workspace_get("_error").unwrap_or(Value::Null))
        }
        Some("workspace") if parts.len() == 1 => {
            // `workspace` alone — return the whole workspace map as an
            // arena-backed object view. Each entry is already arena-
            // allocated; we just hand back a fresh slice in iteration
            // order so the caller can introspect the snapshot.
            let mut builder = ObjectBuilder::with_capacity(arena, event.workspace.len());
            for (k, v) in event.workspace.iter() {
                builder.push(*k, *v);
            }
            Ok(builder.finish())
        }
        Some("workspace") => {
            // `workspace.xxx.yyy` — direct lookup, no clone of entire map
            let rest = &parts[1..];
            resolve_workspace_direct(rest, event)
        }
        _ => {
            bail!("unknown identifier: {}", parts.join("."))
        }
    }
}

/// Direct lookup into `event.workspace` — no clone, just walks the
/// borrowed entries.
fn resolve_workspace_direct<'bump>(
    parts: &[String],
    event: &BorrowedEvent<'bump>,
) -> Result<Value<'bump>> {
    let first = event.workspace_get(&parts[0]).unwrap_or(Value::Null);
    if parts.len() == 1 {
        return Ok(first);
    }
    resolve_workspace_path(&parts[1..], first)
}

fn resolve_workspace_path<'bump>(parts: &[String], value: Value<'bump>) -> Result<Value<'bump>> {
    if parts.is_empty() {
        return Ok(value);
    }
    match value {
        Value::Object(entries) => {
            let next = entries
                .iter()
                .find(|(k, _)| *k == parts[0].as_str())
                .map(|(_, v)| *v)
                .unwrap_or(Value::Null);
            resolve_workspace_path(&parts[1..], next)
        }
        Value::Bytes(_) => bail!("cannot access field `{}` on a bytes value", parts[0]),
        _ => Ok(Value::Null),
    }
}

// ---------------------------------------------------------------------------
// Operators
// ---------------------------------------------------------------------------

fn eval_bin_op<'bump>(
    left: &Value<'bump>,
    op: BinOp,
    right: &Value<'bump>,
    arena: &EventArena<'bump>,
) -> Result<Value<'bump>> {
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
        BinOp::Add => add_values(left, right, arena),
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

fn eval_unary_op<'bump>(op: UnaryOp, val: &Value<'bump>) -> Result<Value<'bump>> {
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
pub fn values_match(left: &Value<'_>, right: &Value<'_>) -> bool {
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
pub fn is_truthy(v: &Value<'_>) -> bool {
    v.is_truthy()
}

/// String coercion used by templates, format() placeholders, and any
/// other user-facing primitive that needs a printable representation.
/// Bytes is not coerced — text helpers reject it upstream so we never
/// reach here with a Bytes value, but the fallback returns a placeholder
/// shape rather than a UTF-8-lossy string to make any bug surface
/// loudly.
pub fn value_to_string(v: &Value<'_>) -> String {
    match v {
        Value::String(s) => (*s).to_string(),
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
        Value::Object(entries) => {
            let mut s = String::from("{");
            for (i, (k, v)) in entries.iter().enumerate() {
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

fn value_to_f64(v: &Value<'_>) -> f64 {
    match v {
        Value::Int(n) => *n as f64,
        Value::Float(n) => *n,
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Bool(true) => 1.0,
        _ => 0.0,
    }
}

fn compare_values(left: &Value<'_>, right: &Value<'_>) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a.partial_cmp(b),
        (Value::Float(a), Value::Float(b)) => a.partial_cmp(b),
        (Value::Int(a), Value::Float(b)) => (*a as f64).partial_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
        _ => None,
    }
}

/// `+` operator. String concat (existing behaviour), Bytes concat (new
/// for v0.5.0 Bytes design §2), numeric otherwise. Mixed-type Bytes
/// participation is an error.
fn add_values<'bump>(
    left: &Value<'bump>,
    right: &Value<'bump>,
    arena: &EventArena<'bump>,
) -> Result<Value<'bump>> {
    if matches!(left, Value::Bytes(_)) || matches!(right, Value::Bytes(_)) {
        return match (left, right) {
            (Value::Bytes(a), Value::Bytes(b)) => {
                let mut buf = bumpalo::collections::Vec::with_capacity_in(
                    a.len() + b.len(),
                    arena.bump(),
                );
                buf.extend_from_slice(a);
                buf.extend_from_slice(b);
                Ok(Value::Bytes(buf.into_bump_slice()))
            }
            _ => bail!(
                "cannot concatenate {} and {} (only bytes + bytes is supported)",
                left.type_name(),
                right.type_name()
            ),
        };
    }
    if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
        let mut s = String::new();
        s.push_str(&value_to_string(left));
        s.push_str(&value_to_string(right));
        return Ok(Value::String(arena.alloc_str(&s)));
    }
    numeric_op("add", left, right, |a, b| a + b)
}

fn numeric_op<'bump>(
    op: &str,
    left: &Value<'bump>,
    right: &Value<'bump>,
    f: impl Fn(f64, f64) -> f64,
) -> Result<Value<'bump>> {
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
fn numeric_value_from_f64<'bump>(n: f64) -> Value<'bump> {
    if n.is_finite() && n.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&n) {
        Value::Int(n as i64)
    } else if n.is_finite() {
        Value::Float(n)
    } else {
        Value::Null
    }
}
