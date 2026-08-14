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
use super::ast::{
    BinOp, BlockArg, BranchBody, Expr, ExprKind, IfChain, SwitchStmtArm, TemplateFragment, UnaryOp,
};
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
            block_arg,
        } => {
            // Block-arg primitives (`map` / `filter` / `find` / `reduce`)
            // need access to the unevaluated body so it can be re-evaluated
            // per element with a fresh local binding. These dispatch
            // before ordinary primitive evaluation so the arg-evaluation
            // loop below never runs for them.
            if let Some(block) = block_arg {
                return eval_block_primitive(
                    namespace.as_deref(),
                    name,
                    args,
                    block,
                    event,
                    funcs,
                    scope,
                    arena,
                );
            }
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

/// Evaluate a primitive call that carries a trailing block argument.
///
/// Only `map` / `filter` / `find` / `reduce` accept block-args (other
/// names bail with a clear error). Each primitive evaluates its collection
/// argument up-front and dispatches on its runtime `Array` / `Object` type.
/// The block body is then re-evaluated per element or entry with its
/// arguments pushed onto a child copy of the caller's [`LocalScope`]. Locals
/// introduced inside the block body do not leak back to the caller; each
/// iteration runs against a fresh clone.
#[allow(clippy::too_many_arguments)]
fn eval_block_primitive<'bump>(
    namespace: Option<&str>,
    name: &str,
    args: &[Expr],
    block: &BlockArg,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    if let Some(ns) = namespace {
        bail!(
            "block-arg primitives are flat-namespaced; {}.{} does not accept a `{{ |x| ... }}` block",
            ns,
            name
        );
    }
    match name {
        "map" => eval_map(args, block, event, funcs, scope, arena),
        "filter" => eval_filter(args, block, event, funcs, scope, arena),
        "find" => eval_find(args, block, event, funcs, scope, arena),
        "reduce" => eval_reduce(args, block, event, funcs, scope, arena),
        other => bail!(
            "function `{}` does not accept a block argument (only map / filter / find / reduce do)",
            other
        ),
    }
}

/// Evaluate a block body with a child scope where `params` are bound to
/// `values`. Used per-iteration by map/filter/find/reduce. Returns the
/// value of the trailing return expression.
fn eval_block_body<'bump>(
    block: &BlockArg,
    values: &[Value<'bump>],
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    parent_scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    if values.len() != block.params.len() {
        bail!(
            "block arity mismatch: declared {} parameter(s) but caller bound {}",
            block.params.len(),
            values.len()
        );
    }
    let mut scope = parent_scope.clone();
    for (param, val) in block.params.iter().zip(values.iter()) {
        scope.bind(param, *val);
    }
    for fl in &block.body.lets {
        let v = eval_expr_with_scope(&fl.value, event, funcs, &scope, arena)?;
        scope.bind(&fl.name, v);
    }
    eval_expr_with_scope(&block.body.ret, event, funcs, &scope, arena)
}

fn expect_collection_arg<'bump>(
    name: &str,
    args: &[Expr],
    expected_args: usize,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    if args.len() != expected_args {
        bail!(
            "{}() with a block expects {} argument(s), got {}",
            name,
            expected_args,
            args.len()
        );
    }
    let v = eval_expr_with_scope(&args[0], event, funcs, scope, arena)?;
    match v {
        Value::Array(_) | Value::Object(_) | Value::Null => Ok(v),
        other => bail!(
            "{}() expects an array or object as its first argument, got {}",
            name,
            other.type_name()
        ),
    }
}

fn expect_block_arity(
    name: &str,
    block: &BlockArg,
    collection_type: &str,
    expected: usize,
    example: &str,
) -> Result<()> {
    if block.params.len() != expected {
        bail!(
            "{}() over an {} requires exactly {} block parameter(s) (`{}`); got {}",
            name,
            collection_type,
            expected,
            example,
            block.params.len()
        );
    }
    Ok(())
}

fn eval_map<'bump>(
    args: &[Expr],
    block: &BlockArg,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    match expect_collection_arg("map", args, 1, event, funcs, scope, arena)? {
        Value::Array(items) => {
            expect_block_arity("map", block, "array", 1, "{ |x| ... }")?;
            let mut out = ArrayBuilder::with_capacity(arena, items.len());
            for item in items {
                out.push(eval_block_body(
                    block,
                    &[*item],
                    event,
                    funcs,
                    scope,
                    arena,
                )?);
            }
            Ok(out.finish())
        }
        Value::Object(entries) => {
            expect_block_arity("map", block, "object", 2, "{ |key, value| ... }")?;
            let mut out = ArrayBuilder::with_capacity(arena, entries.len());
            for (key, value) in entries {
                out.push(eval_block_body(
                    block,
                    &[Value::String(key), *value],
                    event,
                    funcs,
                    scope,
                    arena,
                )?);
            }
            Ok(out.finish())
        }
        Value::Null => {
            expect_block_arity("map", block, "array", 1, "{ |x| ... }")?;
            Ok(ArrayBuilder::new(arena).finish())
        }
        _ => unreachable!("expect_collection_arg accepts only array, object, or null"),
    }
}

fn eval_filter<'bump>(
    args: &[Expr],
    block: &BlockArg,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    match expect_collection_arg("filter", args, 1, event, funcs, scope, arena)? {
        Value::Array(items) => {
            expect_block_arity("filter", block, "array", 1, "{ |x| ... }")?;
            let mut out = ArrayBuilder::new(arena);
            for item in items {
                if eval_block_body(block, &[*item], event, funcs, scope, arena)?.is_truthy() {
                    out.push(*item);
                }
            }
            Ok(out.finish())
        }
        Value::Object(entries) => {
            expect_block_arity("filter", block, "object", 2, "{ |key, value| ... }")?;
            let mut out = ObjectBuilder::with_capacity(arena, entries.len());
            for (key, value) in entries {
                if eval_block_body(
                    block,
                    &[Value::String(key), *value],
                    event,
                    funcs,
                    scope,
                    arena,
                )?
                .is_truthy()
                {
                    out.push(key, *value);
                }
            }
            Ok(out.finish())
        }
        Value::Null => {
            expect_block_arity("filter", block, "array", 1, "{ |x| ... }")?;
            Ok(ArrayBuilder::new(arena).finish())
        }
        _ => unreachable!("expect_collection_arg accepts only array, object, or null"),
    }
}

fn eval_find<'bump>(
    args: &[Expr],
    block: &BlockArg,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    match expect_collection_arg("find", args, 1, event, funcs, scope, arena)? {
        Value::Array(items) => {
            expect_block_arity("find", block, "array", 1, "{ |x| ... }")?;
            for item in items {
                if eval_block_body(block, &[*item], event, funcs, scope, arena)?.is_truthy() {
                    return Ok(*item);
                }
            }
            Ok(Value::Null)
        }
        Value::Object(entries) => {
            expect_block_arity("find", block, "object", 2, "{ |key, value| ... }")?;
            for (key, value) in entries {
                if eval_block_body(
                    block,
                    &[Value::String(key), *value],
                    event,
                    funcs,
                    scope,
                    arena,
                )?
                .is_truthy()
                {
                    let mut pair = ArrayBuilder::with_capacity(arena, 2);
                    pair.push(Value::String(key));
                    pair.push(*value);
                    return Ok(pair.finish());
                }
            }
            Ok(Value::Null)
        }
        Value::Null => {
            expect_block_arity("find", block, "array", 1, "{ |x| ... }")?;
            Ok(Value::Null)
        }
        _ => unreachable!("expect_collection_arg accepts only array, object, or null"),
    }
}

fn eval_reduce<'bump>(
    args: &[Expr],
    block: &BlockArg,
    event: &BorrowedEvent<'bump>,
    funcs: &FunctionRegistry,
    scope: &LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<Value<'bump>> {
    let collection = expect_collection_arg("reduce", args, 2, event, funcs, scope, arena)?;
    match collection {
        Value::Array(_) | Value::Null => {
            expect_block_arity("reduce", block, "array", 2, "{ |acc, x| ... }")?;
        }
        Value::Object(_) => {
            expect_block_arity("reduce", block, "object", 3, "{ |acc, key, value| ... }")?;
        }
        _ => unreachable!("expect_collection_arg accepts only array, object, or null"),
    }
    let mut acc = eval_expr_with_scope(&args[1], event, funcs, scope, arena)?;
    match collection {
        Value::Array(items) => {
            for item in items {
                acc = eval_block_body(block, &[acc, *item], event, funcs, scope, arena)?;
            }
        }
        Value::Object(entries) => {
            for (key, value) in entries {
                acc = eval_block_body(
                    block,
                    &[acc, Value::String(key), *value],
                    event,
                    funcs,
                    scope,
                    arena,
                )?;
            }
        }
        Value::Null => {}
        _ => unreachable!("expect_collection_arg accepts only array, object, or null"),
    }
    Ok(acc)
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
    // Idents whose first segment is a let-bound name resolve through
    // the local scope first. For multi-segment paths like `f.a.b`, the
    // scope produces the root value and the remaining segments walk
    // into it via the same `resolve_workspace_path` Object / Array
    // walker used for `workspace.x.y.z` — so `let f = regex_parse(...)`
    // followed by `f.user` reads the named capture group from the
    // returned Object.
    //
    // `workspace.*` must always be written explicitly — there is no
    // "bare field lookup" fallback into the workspace map; this scope
    // path only fires when the first segment matches a `let`-bound
    // name in the current process / function scope.
    if let Some(root) = scope.get(&parts[0]) {
        if parts.len() == 1 {
            return Ok(root);
        }
        return resolve_workspace_path(&parts[1..], root);
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
        Some("source") if parts.len() == 2 && parts[1] == "ip" => Ok(Value::String(
            arena.alloc_str(&event.source.ip().to_string()),
        )),
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
                builder.push(k, *v);
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
        BinOp::Sub => numeric_op(BinOp::Sub, left, right, |a, b| a - b),
        BinOp::Mul => numeric_op(BinOp::Mul, left, right, |a, b| a * b),
        BinOp::Div => numeric_op(
            BinOp::Div,
            left,
            right,
            |a, b| if b != 0.0 { a / b } else { 0.0 },
        ),
        BinOp::Mod => numeric_op(
            BinOp::Mod,
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

/// Select the matching arm body of a statement-form `switch`.
///
/// Walks `arms` in source order and returns the body of the first arm
/// whose pattern equals `disc_val` (per [`values_match`]). A `default`
/// arm (`pattern.is_none()`) is returned immediately if encountered —
/// `--check` enforces `default` as the last arm so only a trailing
/// default can be hit this way, matching the documented
/// `docs/src/dsl-syntax.md` dispatch contract.
///
/// Returns `None` when no pattern matched and no `default` is present.
///
/// Pure dispatch logic: the caller supplies `eval_pattern` to evaluate
/// each arm's pattern expression against its execution context (with or
/// without a `LocalScope`, with whichever funcs/arena), and is
/// responsible for executing the returned body. Lets the pipeline and
/// process executors share one first-match algorithm without coupling
/// to either context.
#[allow(dead_code)]
pub fn select_switch_arm<'bump, 'a, F>(
    disc_val: &Value<'bump>,
    arms: &'a [SwitchStmtArm],
    eval_pattern: F,
) -> Result<Option<&'a [BranchBody]>>
where
    F: FnMut(&Expr) -> Result<Value<'bump>>,
{
    Ok(select_switch_arm_with_ordinal(disc_val, arms, eval_pattern)?.map(|(_, body)| body))
}

pub(crate) fn select_switch_arm_with_ordinal<'bump, 'a, F>(
    disc_val: &Value<'bump>,
    arms: &'a [SwitchStmtArm],
    mut eval_pattern: F,
) -> Result<Option<(usize, &'a [BranchBody])>>
where
    F: FnMut(&Expr) -> Result<Value<'bump>>,
{
    for (ordinal, arm) in arms.iter().enumerate() {
        let Some(pattern) = arm.pattern.as_ref() else {
            return Ok(Some((ordinal, &arm.body)));
        };
        if values_match(disc_val, &eval_pattern(pattern)?) {
            return Ok(Some((ordinal, &arm.body)));
        }
    }
    Ok(None)
}

/// Select the matching branch body of an `if`/`else if`/`else` chain.
///
/// Walks `chain.branches` in source order and returns the first body
/// whose condition evaluates truthy (per [`Value::is_truthy`]). Falls
/// back to `chain.else_body` if no condition matched, returning `None`
/// when both produce nothing.
///
/// Same pure-dispatch shape as [`select_switch_arm`]: the caller
/// supplies `eval_condition` and executes the returned body.
#[allow(dead_code)]
pub fn select_if_branch<'bump, 'a, F>(
    chain: &'a IfChain,
    eval_condition: F,
) -> Result<Option<&'a [BranchBody]>>
where
    F: FnMut(&Expr) -> Result<Value<'bump>>,
{
    Ok(select_if_branch_with_ordinal(chain, eval_condition)?.map(|selection| selection.body))
}

/// Chosen branch plus its zero-based ordinal, so the caller can
/// index the parallel metric plan without re-matching AST branches.
/// `ordinal` is `None` for the else branch.
pub(crate) struct SelectedIfBranch<'a> {
    pub(crate) ordinal: Option<usize>,
    pub(crate) body: &'a [BranchBody],
}

pub(crate) fn select_if_branch_with_ordinal<'bump, 'a, F>(
    chain: &'a IfChain,
    mut eval_condition: F,
) -> Result<Option<SelectedIfBranch<'a>>>
where
    F: FnMut(&Expr) -> Result<Value<'bump>>,
{
    for (ordinal, (condition, body)) in chain.branches.iter().enumerate() {
        if eval_condition(condition)?.is_truthy() {
            return Ok(Some(SelectedIfBranch {
                ordinal: Some(ordinal),
                body,
            }));
        }
    }
    Ok(chain.else_body.as_deref().map(|body| SelectedIfBranch {
        ordinal: None,
        body,
    }))
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
                let mut buf =
                    bumpalo::collections::Vec::with_capacity_in(a.len() + b.len(), arena.bump());
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
    numeric_op(BinOp::Add, left, right, |a, b| a + b)
}

fn arithmetic_labels(op: BinOp) -> (&'static str, &'static str) {
    match op {
        BinOp::Add => ("add", "+"),
        BinOp::Sub => ("subtract", "-"),
        BinOp::Mul => ("multiply", "*"),
        BinOp::Div => ("divide", "/"),
        BinOp::Mod => ("modulo", "%"),
        _ => unreachable!("numeric_op only accepts arithmetic operators"),
    }
}

fn exact_int_op(op: BinOp, left: i64, right: i64) -> Result<i64> {
    let result = match op {
        BinOp::Add => left.checked_add(right),
        BinOp::Sub => left.checked_sub(right),
        BinOp::Mul => left.checked_mul(right),
        BinOp::Div if right == 0 => Some(0),
        BinOp::Div => left.checked_div(right),
        BinOp::Mod if right == 0 => Some(0),
        BinOp::Mod => left.checked_rem(right),
        _ => unreachable!("exact_int_op only accepts arithmetic operators"),
    };
    let (_, symbol) = arithmetic_labels(op);
    result.ok_or_else(|| {
        anyhow::anyhow!(
            "integer overflow for operator `{symbol}`: {left} {symbol} {right} exceeds i64"
        )
    })
}

/// Shared arithmetic kernel for `+ - * / %`.
///
/// Two `Int` operands stay on checked i64 arithmetic. Any overflow is
/// an evaluation error; exact integer input never falls back to f64.
/// Other numeric/coercible combinations retain the historical f64 path.
fn numeric_op<'bump>(
    op: BinOp,
    left: &Value<'bump>,
    right: &Value<'bump>,
    f: impl Fn(f64, f64) -> f64,
) -> Result<Value<'bump>> {
    let (action, _) = arithmetic_labels(op);
    if matches!(left, Value::Bytes(_)) || matches!(right, Value::Bytes(_)) {
        bail!("cannot {} a bytes value", action);
    }
    if let (Value::Int(a), Value::Int(b)) = (left, right) {
        return exact_int_op(op, *a, *b).map(Value::Int);
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
    let i64_min = i64::MIN as f64;
    let i64_max_exclusive = -i64_min;
    if n.is_finite() && n.fract() == 0.0 && n >= i64_min && n < i64_max_exclusive {
        Value::Int(n as i64)
    } else if n.is_finite() {
        Value::Float(n)
    } else {
        Value::Null
    }
}

#[cfg(test)]
mod tests {
    use crate::dsl::value::{OwnedValue, Value};
    use bytes::Bytes;
    use std::net::SocketAddr;

    use crate::dsl::arena::EventArena;
    use crate::dsl::ast::*;
    use crate::dsl::eval::*;
    use crate::event::OwnedEvent;
    use crate::functions::FunctionRegistry;

    fn make_event() -> OwnedEvent {
        let mut e = OwnedEvent::new(
            Bytes::from("<134>test message"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        e.workspace
            .insert("src".into(), OwnedValue::String("192.168.1.1".into()));
        e.workspace.insert("count".into(), OwnedValue::Int(42));
        e.workspace.insert("sev".into(), OwnedValue::Int(3));
        e
    }

    fn make_funcs() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = crate::functions::table::TableStore::from_configs(vec![]).unwrap();
        crate::functions::register_builtins(&mut reg, table_store);
        reg
    }

    /// Spanless [`Expr`] construction shortcut used throughout the test
    /// module: `e(ExprKind::IntLit(7))` is equivalent to
    /// `Expr::spanless(ExprKind::IntLit(7))` and avoids the need to
    /// invoke `.into()` at every call site.
    fn e(kind: ExprKind) -> Expr {
        Expr::spanless(kind)
    }

    #[test]
    fn test_eval_literals() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        assert_eq!(
            eval_expr(&e(ExprKind::StringLit("hello".into())), &bev, &f, &arena).unwrap(),
            Value::String("hello")
        );
        assert_eq!(
            eval_expr(&e(ExprKind::IntLit(99)), &bev, &f, &arena).unwrap(),
            Value::Int(99)
        );
        assert_eq!(
            eval_expr(&e(ExprKind::BoolLit(true)), &bev, &f, &arena).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            eval_expr(&e(ExprKind::Null), &bev, &f, &arena).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn test_eval_ident_workspace() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        assert_eq!(
            eval_expr(
                &e(ExprKind::Ident(vec!["workspace".into(), "src".into()])),
                &bev,
                &f,
                &arena
            )
            .unwrap(),
            Value::String("192.168.1.1")
        );
        assert_eq!(
            eval_expr(
                &e(ExprKind::Ident(vec!["workspace".into(), "count".into()])),
                &bev,
                &f,
                &arena
            )
            .unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn test_eval_unknown_ident_errors() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        assert!(
            eval_expr(
                &e(ExprKind::Ident(vec!["typo_field".into()])),
                &bev,
                &f,
                &arena
            )
            .is_err()
        );
    }

    #[test]
    fn test_eval_binop_comparison() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // workspace.sev (3) <= 3 → true
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::Ident(vec!["workspace".into(), "sev".into()]))),
            BinOp::Le,
            Box::new(e(ExprKind::IntLit(3))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::Bool(true)
        );

        // workspace.sev (3) > 5 → false
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::Ident(vec!["workspace".into(), "sev".into()]))),
            BinOp::Gt,
            Box::new(e(ExprKind::IntLit(5))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_eval_add_string_concat() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();

        // String + String → concat
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::StringLit("hello ".into()))),
            BinOp::Add,
            Box::new(e(ExprKind::StringLit("world".into()))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("hello world")
        );

        // Mixed String + Number → both coerced to string
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::StringLit("count=".into()))),
            BinOp::Add,
            Box::new(e(ExprKind::IntLit(42))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("count=42")
        );

        // Number + String → same
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::IntLit(42))),
            BinOp::Add,
            Box::new(e(ExprKind::StringLit(" ms".into()))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("42 ms")
        );

        // Int + Int stays numeric and exact.
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::IntLit(3))),
            BinOp::Add,
            Box::new(e(ExprKind::IntLit(4))),
        ));
        let result = eval_expr(&expr, &bev, &f, &arena).unwrap();
        assert_eq!(result.as_f64(), Some(7.0));

        // Chained: "a" + "b" + "c" (left-associative)
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::BinOp(
                Box::new(e(ExprKind::StringLit("a".into()))),
                BinOp::Add,
                Box::new(e(ExprKind::StringLit("b".into()))),
            ))),
            BinOp::Add,
            Box::new(e(ExprKind::StringLit("c".into()))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("abc")
        );
    }

    #[test]
    fn test_eval_binop_exact_int_arithmetic() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let eval = |left, op, right| {
            let expr = e(ExprKind::BinOp(
                Box::new(e(ExprKind::IntLit(left))),
                op,
                Box::new(e(ExprKind::IntLit(right))),
            ));
            eval_expr(&expr, &bev, &f, &arena)
        };

        assert_eq!(
            eval(1_750_000_000_123_456_789, BinOp::Div, 1_000_000).unwrap(),
            Value::Int(1_750_000_000_123)
        );
        assert_eq!(eval(5, BinOp::Div, 2).unwrap(), Value::Int(2));
        assert_eq!(eval(-5, BinOp::Div, 2).unwrap(), Value::Int(-2));
        assert_eq!(eval(7, BinOp::Mod, 3).unwrap(), Value::Int(1));
        assert_eq!(eval(-7, BinOp::Mod, 3).unwrap(), Value::Int(-1));
        assert_eq!(eval(7, BinOp::Div, 0).unwrap(), Value::Int(0));
        assert_eq!(eval(7, BinOp::Mod, 0).unwrap(), Value::Int(0));

        assert_eq!(
            eval(9_007_199_254_740_992, BinOp::Add, 1).unwrap(),
            Value::Int(9_007_199_254_740_993)
        );
        assert_eq!(
            eval(9_007_199_254_740_993, BinOp::Sub, 1).unwrap(),
            Value::Int(9_007_199_254_740_992)
        );
        assert_eq!(
            eval(1_750_000_000_123, BinOp::Mul, 1_000_000).unwrap(),
            Value::Int(1_750_000_000_123_000_000)
        );

        let mixed = e(ExprKind::BinOp(
            Box::new(e(ExprKind::IntLit(5))),
            BinOp::Div,
            Box::new(e(ExprKind::FloatLit(2.0))),
        ));
        assert_eq!(
            eval_expr(&mixed, &bev, &f, &arena).unwrap(),
            Value::Float(2.5)
        );
    }

    #[test]
    fn test_eval_binop_int_overflow_errors() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();

        for (left, op, right, symbol) in [
            (i64::MAX, BinOp::Add, 1, "+"),
            (i64::MIN, BinOp::Sub, 1, "-"),
            (i64::MAX, BinOp::Mul, 2, "*"),
            (i64::MIN, BinOp::Div, -1, "/"),
            (i64::MIN, BinOp::Mod, -1, "%"),
        ] {
            let expr = e(ExprKind::BinOp(
                Box::new(e(ExprKind::IntLit(left))),
                op,
                Box::new(e(ExprKind::IntLit(right))),
            ));
            let message = eval_expr(&expr, &bev, &f, &arena).unwrap_err().to_string();
            assert!(message.contains("integer overflow"), "{message}");
            assert!(
                message.contains(&format!("operator `{symbol}`")),
                "{message}"
            );
        }
    }

    #[test]
    fn test_numeric_value_from_f64_excludes_positive_i64_limit() {
        let positive_limit = -(i64::MIN as f64);
        assert_eq!(
            numeric_value_from_f64(i64::MIN as f64),
            Value::Int(i64::MIN)
        );
        assert_eq!(
            numeric_value_from_f64(positive_limit),
            Value::Float(positive_limit)
        );
    }

    #[test]
    fn test_eval_text_exact_int_arithmetic_above_f64_limit() {
        use crate::dsl::parser::parse_config;
        let src = r#"
def pipeline p {
    input a
    process inline | {
        workspace.event_time_ms = 1750000000123456789 / 1000000
    }
    drop
}
"#;
        let config = parse_config(src).unwrap();
        let pipeline = match &config.definitions[0] {
            Definition::Pipeline(def) => def,
            _ => panic!("expected Pipeline"),
        };
        let chain = match &pipeline.body[1] {
            PipelineStatement::ProcessChain(chain) => chain,
            _ => panic!("expected ProcessChain"),
        };
        let stmts = match &chain[1] {
            ProcessChainElement::Inline(stmts) => stmts,
            _ => panic!("expected Inline"),
        };
        let expr = match &stmts[0] {
            ProcessStatement::Assign(_, expr) => expr,
            _ => panic!("expected Assign"),
        };

        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        assert_eq!(
            eval_expr(expr, &bev, &f, &arena).unwrap(),
            Value::Int(1_750_000_000_123)
        );
    }

    #[test]
    fn test_eval_text_to_value_left_associative_div() {
        // End-to-end regression: parse `10 / 2 / 5` from source text and
        // confirm the *value* is 1, not 25. The pre-fix parser produced a
        // right-associative tree that evaluated to `10 / (2 / 5) = 25` —
        // the existing chained-add test built the AST by hand and missed
        // this whole path. Div is non-commutative, so the tree shape is
        // visible in the final scalar.
        use crate::dsl::parser::parse_config;
        let src = r#"
def pipeline p {
    input a
    process inline | {
        workspace.x = 10 / 2 / 5
    }
    drop
}
"#;
        let config = parse_config(src).unwrap();
        let pipeline = match &config.definitions[0] {
            Definition::Pipeline(def) => def,
            _ => panic!("expected Pipeline"),
        };
        let chain = match &pipeline.body[1] {
            PipelineStatement::ProcessChain(chain) => chain,
            _ => panic!("expected ProcessChain"),
        };
        let stmts = match &chain[1] {
            ProcessChainElement::Inline(stmts) => stmts,
            _ => panic!("expected Inline"),
        };
        let expr = match &stmts[0] {
            ProcessStatement::Assign(_, expr) => expr,
            _ => panic!("expected Assign"),
        };

        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let result = eval_expr(expr, &bev, &f, &arena).unwrap();
        assert_eq!(
            result.as_f64(),
            Some(1.0),
            "10 / 2 / 5 must be (10/2)/5 = 1, not 10/(2/5) = 25"
        );
    }

    #[test]
    fn test_eval_binop_logical() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // true and false → false
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::BoolLit(true))),
            BinOp::And,
            Box::new(e(ExprKind::BoolLit(false))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::Bool(false)
        );

        // true or false → true
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::BoolLit(true))),
            BinOp::Or,
            Box::new(e(ExprKind::BoolLit(false))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_eval_not() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::UnaryOp(
            UnaryOp::Not,
            Box::new(e(ExprKind::BoolLit(true))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_eval_contains() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::FuncCall {
            namespace: None,
            name: "contains".into(),
            args: vec![
                e(ExprKind::Ident(vec!["ingress".into()])),
                e(ExprKind::StringLit("test".into())),
            ],
            block_arg: None,
        });
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn test_eval_template() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // "[${workspace.sev}] from ${workspace.src}"
        let expr = e(ExprKind::Template(vec![
            TemplateFragment::Literal("[".into()),
            TemplateFragment::Interp(e(ExprKind::Ident(vec!["workspace".into(), "sev".into()]))),
            TemplateFragment::Literal("] from ".into()),
            TemplateFragment::Interp(e(ExprKind::Ident(vec!["workspace".into(), "src".into()]))),
        ]));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("[3] from 192.168.1.1")
        );
    }

    #[test]
    fn test_eval_template_missing_interp_empty() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::Template(vec![
            TemplateFragment::Literal("prefix-".into()),
            TemplateFragment::Interp(e(ExprKind::Ident(vec![
                "workspace".into(),
                "missing".into(),
            ]))),
            TemplateFragment::Literal("-suffix".into()),
        ]));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("prefix--suffix")
        );
    }

    #[test]
    fn test_eval_lower_upper() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let lower = e(ExprKind::FuncCall {
            namespace: None,
            name: "lower".into(),
            args: vec![e(ExprKind::StringLit("HELLO".into()))],
            block_arg: None,
        });
        assert_eq!(
            eval_expr(&lower, &bev, &f, &arena).unwrap(),
            Value::String("hello")
        );

        let upper = e(ExprKind::FuncCall {
            namespace: None,
            name: "upper".into(),
            args: vec![e(ExprKind::StringLit("hello".into()))],
            block_arg: None,
        });
        assert_eq!(
            eval_expr(&upper, &bev, &f, &arena).unwrap(),
            Value::String("HELLO")
        );
    }

    #[test]
    fn test_eval_to_json() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // 0.5.0+: to_json requires an explicit value. Pass `workspace` to
        // serialise the workspace map (the most common operator pattern).
        let expr = e(ExprKind::FuncCall {
            namespace: None,
            name: "to_json".into(),
            args: vec![e(ExprKind::Ident(vec!["workspace".into()]))],
            block_arg: None,
        });
        let result = eval_expr(&expr, &bev, &f, &arena).unwrap();
        let s = result.as_str().unwrap();
        assert!(s.contains("\"src\":\"192.168.1.1\""));
    }

    #[test]
    fn test_is_truthy() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::Bool(false).is_truthy());
        assert!(Value::Bool(true).is_truthy());
        assert!(!Value::String("").is_truthy());
        assert!(Value::String("x").is_truthy());
        assert!(!Value::Int(0).is_truthy());
        assert!(Value::Int(1).is_truthy());
    }

    #[test]
    fn test_non_numeric_comparison_returns_false() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // "hello" < "world" should be false (non-numeric)
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::StringLit("hello".into()))),
            BinOp::Lt,
            Box::new(e(ExprKind::StringLit("world".into()))),
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::Bool(false)
        );
    }

    #[test]
    fn test_property_access_on_hash() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // { country: "JP", city: "Tokyo" }.country → "JP"
        let hash = e(ExprKind::HashLit(vec![
            ("country".into(), e(ExprKind::StringLit("JP".into()))),
            ("city".into(), e(ExprKind::StringLit("Tokyo".into()))),
        ]));
        let expr = e(ExprKind::PropertyAccess(
            Box::new(hash),
            vec!["country".into()],
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("JP")
        );
    }

    #[test]
    fn test_property_access_chained() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // { geo: { country: "JP" } }.geo.country → "JP"
        let inner_hash = e(ExprKind::HashLit(vec![(
            "country".into(),
            e(ExprKind::StringLit("JP".into())),
        )]));
        let outer_hash = e(ExprKind::HashLit(vec![("geo".into(), inner_hash)]));
        let expr = e(ExprKind::PropertyAccess(
            Box::new(outer_hash),
            vec!["geo".into(), "country".into()],
        ));
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("JP")
        );
    }

    #[test]
    fn test_property_access_missing_field() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let hash = e(ExprKind::HashLit(vec![(
            "country".into(),
            e(ExprKind::StringLit("JP".into())),
        )]));
        let expr = e(ExprKind::PropertyAccess(
            Box::new(hash),
            vec!["missing".into()],
        ));
        assert_eq!(eval_expr(&expr, &bev, &f, &arena).unwrap(), Value::Null);
    }

    #[test]
    fn test_values_match_fn() {
        let _bump = ::bumpalo::Bump::new();
        let _arena = EventArena::new(&_bump);
        assert!(values_match(&Value::String("a"), &Value::String("a")));
        assert!(!values_match(&Value::String("a"), &Value::String("b")));
        assert!(values_match(&Value::Int(42), &Value::Int(42)));
    }

    // ----- Array literal -----------------------------------------------------
    //
    // The DSL models arrays as positionless collections (see
    // docs/src/processing/user-defined.md). Literals are the one place
    // where element order is visible; these tests pin down the
    // order-preservation guarantee and confirm mixed types / nesting
    // work.

    #[test]
    fn test_eval_array_literal_empty() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        assert_eq!(
            eval_expr(&e(ExprKind::ArrayLit(vec![])), &bev, &f, &arena).unwrap(),
            Value::empty_array()
        );
    }

    #[test]
    fn test_eval_array_literal_scalars() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::ArrayLit(vec![
            e(ExprKind::IntLit(1)),
            e(ExprKind::IntLit(2)),
            e(ExprKind::IntLit(3)),
        ]));
        let expected = OwnedValue::Array(vec![
            OwnedValue::Int(1),
            OwnedValue::Int(2),
            OwnedValue::Int(3),
        ])
        .view_in(&arena);
        assert_eq!(eval_expr(&expr, &bev, &f, &arena).unwrap(), expected);
    }

    #[test]
    fn test_eval_array_literal_mixed_types() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::ArrayLit(vec![
            e(ExprKind::IntLit(1)),
            e(ExprKind::StringLit("two".into())),
            e(ExprKind::BoolLit(true)),
            e(ExprKind::Null),
        ]));
        let expected = OwnedValue::Array(vec![
            OwnedValue::Int(1),
            OwnedValue::String("two".into()),
            OwnedValue::Bool(true),
            OwnedValue::Null,
        ])
        .view_in(&arena);
        assert_eq!(eval_expr(&expr, &bev, &f, &arena).unwrap(), expected);
    }

    #[test]
    fn test_eval_array_literal_resolves_workspace_refs() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::ArrayLit(vec![
            e(ExprKind::Ident(vec!["workspace".into(), "src".into()])),
            e(ExprKind::Ident(vec!["workspace".into(), "count".into()])),
        ]));
        let expected = OwnedValue::Array(vec![
            OwnedValue::String("192.168.1.1".into()),
            OwnedValue::Int(42),
        ])
        .view_in(&arena);
        assert_eq!(eval_expr(&expr, &bev, &f, &arena).unwrap(), expected);
    }

    #[test]
    fn test_eval_array_literal_nested() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let row = |a, b| {
            e(ExprKind::ArrayLit(vec![
                e(ExprKind::IntLit(a)),
                e(ExprKind::IntLit(b)),
            ]))
        };
        let grid = e(ExprKind::ArrayLit(vec![row(1, 2), row(3, 4)]));
        let expected = OwnedValue::Array(vec![
            OwnedValue::Array(vec![OwnedValue::Int(1), OwnedValue::Int(2)]),
            OwnedValue::Array(vec![OwnedValue::Int(3), OwnedValue::Int(4)]),
        ])
        .view_in(&arena);
        assert_eq!(eval_expr(&grid, &bev, &f, &arena).unwrap(), expected);
    }

    #[test]
    fn test_eval_array_inside_hash_literal() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::HashLit(vec![
            ("title".into(), e(ExprKind::StringLit("finding".into()))),
            (
                "types".into(),
                e(ExprKind::ArrayLit(vec![
                    e(ExprKind::StringLit("sqli".into())),
                    e(ExprKind::StringLit("xss".into())),
                ])),
            ),
        ]));
        let out = eval_expr(&expr, &bev, &f, &arena).unwrap();
        let obj = out.as_object().unwrap();
        let types = obj
            .iter()
            .find(|(k, _)| *k == "types")
            .map(|(_, v)| *v)
            .unwrap();
        let expected_types = OwnedValue::Array(vec![
            OwnedValue::String("sqli".into()),
            OwnedValue::String("xss".into()),
        ])
        .view_in(&arena);
        assert_eq!(types, expected_types);
    }

    // ---- SwitchExpr -------------------------------------------------------

    #[test]
    fn switch_expr_picks_matching_arm() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        // switch 6 { 6 { "tcp" } 17 { "udp" } default { null } }
        let expr = e(ExprKind::SwitchExpr {
            scrutinee: Box::new(e(ExprKind::IntLit(6))),
            arms: vec![
                crate::dsl::ast::SwitchExprArm {
                    pattern: Some(e(ExprKind::IntLit(6))),
                    body: e(ExprKind::StringLit("tcp".into())),
                },
                crate::dsl::ast::SwitchExprArm {
                    pattern: Some(e(ExprKind::IntLit(17))),
                    body: e(ExprKind::StringLit("udp".into())),
                },
                crate::dsl::ast::SwitchExprArm {
                    pattern: None,
                    body: e(ExprKind::Null),
                },
            ],
        });
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("tcp")
        );
    }

    #[test]
    fn switch_expr_falls_to_default() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::SwitchExpr {
            scrutinee: Box::new(e(ExprKind::IntLit(99))),
            arms: vec![
                crate::dsl::ast::SwitchExprArm {
                    pattern: Some(e(ExprKind::IntLit(6))),
                    body: e(ExprKind::StringLit("tcp".into())),
                },
                crate::dsl::ast::SwitchExprArm {
                    pattern: None,
                    body: e(ExprKind::StringLit("unknown".into())),
                },
            ],
        });
        assert_eq!(
            eval_expr(&expr, &bev, &f, &arena).unwrap(),
            Value::String("unknown")
        );
    }

    #[test]
    fn switch_expr_no_match_no_default_returns_null() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let f = make_funcs();
        let expr = e(ExprKind::SwitchExpr {
            scrutinee: Box::new(e(ExprKind::IntLit(99))),
            arms: vec![crate::dsl::ast::SwitchExprArm {
                pattern: Some(e(ExprKind::IntLit(6))),
                body: e(ExprKind::StringLit("tcp".into())),
            }],
        });
        assert_eq!(eval_expr(&expr, &bev, &f, &arena).unwrap(), Value::Null);
    }

    // ---- User-defined `def function` end-to-end --------------------------

    #[test]
    fn user_function_call_returns_body_value() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        // Register a user function `double(x) { x * 2 }` and call it
        // via the same registry path the parser-built call sites use.
        use crate::dsl::ast::FunctionDef;

        let mut funcs = make_funcs();
        let body = e(ExprKind::BinOp(
            Box::new(e(ExprKind::Ident(vec!["x".into()]))),
            BinOp::Mul,
            Box::new(e(ExprKind::IntLit(2))),
        ));
        funcs.register_user_function(FunctionDef {
            name: "double".into(),
            params: vec!["x".into()],
            body: crate::dsl::ast::FuncBody {
                lets: vec![],
                ret: body,
            },
        });

        let ev = make_event();
        let bev = ev.view_in(&arena);
        let result = funcs
            .call(None, "double", &[Value::Int(21)], &bev, &arena)
            .unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn user_function_arity_mismatch_at_call() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        use crate::dsl::ast::FunctionDef;

        let mut funcs = make_funcs();
        funcs.register_user_function(FunctionDef {
            name: "needs_two".into(),
            params: vec!["a".into(), "b".into()],
            body: crate::dsl::ast::FuncBody {
                lets: vec![],
                ret: e(ExprKind::Ident(vec!["a".into()])),
            },
        });

        let ev = make_event();
        let bev = ev.view_in(&arena);
        // The dispatch path is responsible for the central arity
        // check (via the synthesized `Any^2 -> Any` signature). Pass
        // 1 arg and expect a clear error.
        let err = funcs
            .call(None, "needs_two", &[Value::Int(1)], &bev, &arena)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("needs_two"),
            "expected function name in arity error: {}",
            err
        );
    }

    #[test]
    fn user_function_with_switch_body_maps_correctly() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        use crate::dsl::ast::{FunctionDef, SwitchExprArm};

        let mut funcs = make_funcs();
        // normalize_proto(num) — the canonical mapping use case.
        let body = e(ExprKind::SwitchExpr {
            scrutinee: Box::new(e(ExprKind::Ident(vec!["num".into()]))),
            arms: vec![
                SwitchExprArm {
                    pattern: Some(e(ExprKind::IntLit(6))),
                    body: e(ExprKind::StringLit("tcp".into())),
                },
                SwitchExprArm {
                    pattern: Some(e(ExprKind::IntLit(17))),
                    body: e(ExprKind::StringLit("udp".into())),
                },
                SwitchExprArm {
                    pattern: None,
                    body: e(ExprKind::Null),
                },
            ],
        });
        funcs.register_user_function(FunctionDef {
            name: "normalize_proto".into(),
            params: vec!["num".into()],
            body: crate::dsl::ast::FuncBody {
                lets: vec![],
                ret: body,
            },
        });

        let ev = make_event();
        let bev = ev.view_in(&arena);
        assert_eq!(
            funcs
                .call(None, "normalize_proto", &[Value::Int(6)], &bev, &arena)
                .unwrap(),
            Value::String("tcp")
        );
        assert_eq!(
            funcs
                .call(None, "normalize_proto", &[Value::Int(17)], &bev, &arena)
                .unwrap(),
            Value::String("udp")
        );
        assert_eq!(
            funcs
                .call(None, "normalize_proto", &[Value::Int(99)], &bev, &arena)
                .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn user_function_calling_user_function_works() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        use crate::dsl::ast::FunctionDef;

        let mut funcs = make_funcs();
        funcs.register_user_function(FunctionDef {
            name: "double".into(),
            params: vec!["x".into()],
            body: crate::dsl::ast::FuncBody {
                lets: vec![],
                ret: e(ExprKind::BinOp(
                    Box::new(e(ExprKind::Ident(vec!["x".into()]))),
                    BinOp::Mul,
                    Box::new(e(ExprKind::IntLit(2))),
                )),
            },
        });
        funcs.register_user_function(FunctionDef {
            name: "quadruple".into(),
            params: vec!["x".into()],
            body: crate::dsl::ast::FuncBody {
                lets: vec![],
                ret: e(ExprKind::FuncCall {
                    namespace: None,
                    name: "double".into(),
                    args: vec![e(ExprKind::FuncCall {
                        namespace: None,
                        name: "double".into(),
                        args: vec![e(ExprKind::Ident(vec!["x".into()]))],
                        block_arg: None,
                    })],
                    block_arg: None,
                }),
            },
        });

        let ev = make_event();
        let bev = ev.view_in(&arena);
        assert_eq!(
            funcs
                .call(None, "quadruple", &[Value::Int(5)], &bev, &arena)
                .unwrap(),
            Value::Int(20)
        );
    }

    #[test]
    fn user_function_with_let_bindings() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        use crate::dsl::ast::{FuncBody, FuncLet, FunctionDef};

        let mut funcs = make_funcs();
        // def function f(x) { let p = x * 2; let q = p + 1; q }
        funcs.register_user_function(FunctionDef {
            name: "f".into(),
            params: vec!["x".into()],
            body: FuncBody {
                lets: vec![
                    FuncLet {
                        name: "p".into(),
                        value: e(ExprKind::BinOp(
                            Box::new(e(ExprKind::Ident(vec!["x".into()]))),
                            BinOp::Mul,
                            Box::new(e(ExprKind::IntLit(2))),
                        )),
                    },
                    FuncLet {
                        name: "q".into(),
                        value: e(ExprKind::BinOp(
                            Box::new(e(ExprKind::Ident(vec!["p".into()]))),
                            BinOp::Add,
                            Box::new(e(ExprKind::IntLit(1))),
                        )),
                    },
                ],
                ret: e(ExprKind::Ident(vec!["q".into()])),
            },
        });

        let ev = make_event();
        let bev = ev.view_in(&arena);
        let result = funcs
            .call(None, "f", &[Value::Int(10)], &bev, &arena)
            .unwrap();
        assert_eq!(result, Value::Int(21)); // 10 * 2 + 1
    }

    #[test]
    fn user_function_let_reassignment_overwrites() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        use crate::dsl::ast::{FuncBody, FuncLet, FunctionDef};

        let mut funcs = make_funcs();
        // def function f(x) { let v = x; let v = v * 3; v }
        // The second `let v = ...` reassigns `v` in the same local
        // scope (semantically: assignment to the same variable).
        funcs.register_user_function(FunctionDef {
            name: "f".into(),
            params: vec!["x".into()],
            body: FuncBody {
                lets: vec![
                    FuncLet {
                        name: "v".into(),
                        value: e(ExprKind::Ident(vec!["x".into()])),
                    },
                    FuncLet {
                        name: "v".into(),
                        value: e(ExprKind::BinOp(
                            Box::new(e(ExprKind::Ident(vec!["v".into()]))),
                            BinOp::Mul,
                            Box::new(e(ExprKind::IntLit(3))),
                        )),
                    },
                ],
                ret: e(ExprKind::Ident(vec!["v".into()])),
            },
        });

        let ev = make_event();
        let bev = ev.view_in(&arena);
        let result = funcs
            .call(None, "f", &[Value::Int(7)], &bev, &arena)
            .unwrap();
        assert_eq!(result, Value::Int(21));
    }

    #[test]
    fn source_ip_resolves_to_string() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event(); // source = "10.0.0.1:514"
        let bev = ev.view_in(&arena);
        let funcs = make_funcs();
        let v = eval_expr(
            &e(ExprKind::Ident(vec!["source".into(), "ip".into()])),
            &bev,
            &funcs,
            &arena,
        )
        .unwrap();
        assert_eq!(v, Value::String("10.0.0.1"));
    }

    #[test]
    fn source_port_resolves_to_int() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        let ev = make_event(); // source = "10.0.0.1:514"
        let bev = ev.view_in(&arena);
        let funcs = make_funcs();
        let v = eval_expr(
            &e(ExprKind::Ident(vec!["source".into(), "port".into()])),
            &bev,
            &funcs,
            &arena,
        )
        .unwrap();
        assert_eq!(v, Value::Int(514));
    }

    #[test]
    fn bare_source_resolves_to_object() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        // Bare `source` returns the whole `{ ip, port }` map so it can
        // be passed around or serialised as a unit. This is the breaking
        // change from 0.5.5 (where bare `source` was a flat IP String).
        let ev = make_event(); // source = "10.0.0.1:514"
        let bev = ev.view_in(&arena);
        let funcs = make_funcs();
        let v = eval_expr(
            &e(ExprKind::Ident(vec!["source".into()])),
            &bev,
            &funcs,
            &arena,
        )
        .unwrap();
        match v {
            Value::Object(map) => {
                let lookup = |k: &str| map.iter().find(|(kk, _)| *kk == k).map(|(_, vv)| *vv);
                assert_eq!(lookup("ip"), Some(Value::String("10.0.0.1")));
                assert_eq!(lookup("port"), Some(Value::Int(514)));
            }
            other => panic!("expected Object for bare source, got {:?}", other),
        }
    }

    #[test]
    fn source_unknown_path_errors() {
        let _bump = ::bumpalo::Bump::new();
        let arena = EventArena::new(&_bump);
        // Only `source.ip` and `source.port` are defined paths.
        let ev = make_event();
        let bev = ev.view_in(&arena);
        let funcs = make_funcs();
        let err = eval_expr(
            &e(ExprKind::Ident(vec!["source".into(), "host".into()])),
            &bev,
            &funcs,
            &arena,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("source.host") || err.to_string().contains("only source.ip"),
            "expected helpful error, got: {}",
            err
        );
    }
}
