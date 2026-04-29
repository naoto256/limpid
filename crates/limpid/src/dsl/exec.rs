//! Process statement executor: runs DSL process statements against a borrowed event.
//!
//! Operates exclusively on the per-event arena form
//! ([`BorrowedEvent<'bump>`]); the heap-owned [`crate::event::OwnedEvent`]
//! never enters this module. Boundary conversions happen at the
//! pipeline level (`pipeline::run_pipeline` entry/exit).

use anyhow::{Result, bail};
use bytes::Bytes;
use thiserror::Error;

use super::arena::EventArena;
use super::ast::*;
use super::eval::{LocalScope, eval_expr_with_scope, value_to_string, values_match};
use super::value::Value;
use crate::event::BorrowedEvent;
use crate::functions::FunctionRegistry;

/// Error type returned by `ProcessRegistry::call`.
///
/// Kept narrow on purpose: the executor only needs to distinguish
/// "this process failed" (recoverable — the caller passes the event
/// through unchanged) from "we reached the end of the body normally".
#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("process failed: {0}")]
    Failed(String),
}

/// Result of executing a process body.
pub enum ExecResult<'bump> {
    /// Event passed through (possibly mutated).
    Continue(BorrowedEvent<'bump>),
    /// Event was dropped.
    Dropped,
}

/// A registry of named processes that can be called from DSL.
pub trait ProcessRegistry {
    fn call<'bump>(
        &self,
        name: &str,
        args: &[Value<'bump>],
        event: BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
    ) -> std::result::Result<Option<BorrowedEvent<'bump>>, ProcessError>;
}

/// Execute a sequence of process statements against an event.
///
/// Each call starts with a fresh [`LocalScope`] — `let` bindings do not
/// leak across process-body boundaries. This is intentional: callee
/// processes shouldn't see the caller's scratch material, and vice
/// versa. The only channel between caller and callee is the Event
/// itself (`workspace` and metadata).
pub fn exec_process_body<'bump>(
    stmts: &[ProcessStatement],
    event: BorrowedEvent<'bump>,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    arena: &'bump EventArena<'bump>,
) -> Result<ExecResult<'bump>> {
    let mut scope = LocalScope::new();
    exec_stmts_with_scope(stmts, event, registry, funcs, &mut scope, arena)
}

/// Run statements with the given local scope. `let` bindings mutate
/// `scope`; branch / loop bodies are run with the same scope so a
/// `let x` written above an `if` is visible inside (and below) the
/// branch. Branches do not introduce inner scopes — every `let` is
/// hoisted to the process body scope — which is the simplest useful
/// semantics and matches how users read the code top-to-bottom.
fn exec_stmts_with_scope<'bump>(
    stmts: &[ProcessStatement],
    mut event: BorrowedEvent<'bump>,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<ExecResult<'bump>> {
    for stmt in stmts {
        match exec_process_stmt(stmt, event, registry, funcs, scope, arena)? {
            ExecResult::Continue(e) => event = e,
            ExecResult::Dropped => return Ok(ExecResult::Dropped),
        }
    }
    Ok(ExecResult::Continue(event))
}

fn exec_process_stmt<'bump>(
    stmt: &ProcessStatement,
    mut event: BorrowedEvent<'bump>,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<ExecResult<'bump>> {
    match stmt {
        ProcessStatement::Drop => Ok(ExecResult::Dropped),

        ProcessStatement::Error(msg_expr) => {
            // Render the optional message expression to a string and
            // bubble up as `Err` — the pipeline-level ProcessChain arm
            // catches this and routes the event to the error_log
            // exactly like a runtime process error. If we're inside a
            // `try` block, the catch body sees the message via
            // `workspace._error` (same exposure as any runtime error);
            // otherwise the message lands in the DLQ entry's `reason`.
            let msg = match msg_expr {
                Some(e) => value_to_string(&eval_expr_with_scope(
                    e, &event, funcs, scope, arena,
                )?),
                None => "explicit error routing".to_string(),
            };
            anyhow::bail!("{}", msg);
        }

        ProcessStatement::Assign(target, expr) => {
            let value = eval_expr_with_scope(expr, &event, funcs, scope, arena)?;
            apply_assign(&mut event, target, value, arena)?;
            Ok(ExecResult::Continue(event))
        }

        ProcessStatement::LetBinding(name, expr) => {
            let value = eval_expr_with_scope(expr, &event, funcs, scope, arena)?;
            scope.bind(name, value);
            Ok(ExecResult::Continue(event))
        }

        ProcessStatement::ProcessCall(name, args) => {
            let mut evaluated_args =
                bumpalo::collections::Vec::with_capacity_in(args.len(), arena.bump());
            for a in args {
                evaluated_args.push(eval_expr_with_scope(a, &event, funcs, scope, arena)?);
            }

            // Snapshot the workspace before consumption — registry.call
            // takes the event by value, so the Err arm needs to
            // restore from a backup. We snapshot only what the catch
            // path actually inspects (workspace contents) plus the
            // metadata identity; ingress / egress are `Bytes` (cheap
            // refcount clone). Callee processes start with their own
            // fresh LocalScope inside the registry implementation
            // (see `exec_process_body` above). Our `scope` here belongs
            // to the caller and is unaffected by the callee.
            let backup = clone_borrowed_event(&event, arena);
            match registry.call(name, &evaluated_args, event, arena) {
                Ok(Some(e)) => Ok(ExecResult::Continue(e)),
                Ok(None) => Ok(ExecResult::Dropped),
                Err(e) => {
                    tracing::debug!(
                        "process '{}' failed: {} — passing event through unchanged",
                        name,
                        e
                    );
                    Ok(ExecResult::Continue(backup))
                }
            }
        }

        ProcessStatement::If(if_chain) => {
            exec_if_chain_process(if_chain, event, registry, funcs, scope, arena)
        }

        ProcessStatement::Switch(discriminant, arms) => {
            let disc_val = eval_expr_with_scope(discriminant, &event, funcs, scope, arena)?;
            for arm in arms {
                if arm.pattern.is_none() {
                    // default arm
                    return exec_branch_body_process(
                        &arm.body, event, registry, funcs, scope, arena,
                    );
                }
                let pattern_val = eval_expr_with_scope(
                    arm.pattern.as_ref().unwrap(),
                    &event,
                    funcs,
                    scope,
                    arena,
                )?;
                if values_match(&disc_val, &pattern_val) {
                    return exec_branch_body_process(
                        &arm.body, event, registry, funcs, scope, arena,
                    );
                }
            }
            // No arm matched, pass through
            Ok(ExecResult::Continue(event))
        }

        ProcessStatement::TryCatch(try_body, catch_body) => {
            // Snapshot event for try block so we can recover on error.
            // Snapshot the scope too — a failed try must not leak its
            // let bindings into the catch body; the catch gets the
            // scope the try started with.
            let event_backup = clone_borrowed_event(&event, arena);
            let scope_backup = scope.clone();
            match exec_stmts_with_scope(try_body, event, registry, funcs, scope, arena) {
                Ok(result) => Ok(result),
                Err(e) => {
                    *scope = scope_backup;
                    // Bind error message to `error` identifier
                    // (accessible via workspace._error). The message
                    // lives in the arena like every other workspace
                    // string.
                    let mut recovered = event_backup;
                    let msg = arena.alloc_str(&e.to_string());
                    recovered.workspace_set_str(arena, "_error", Value::String(msg));
                    let mut result = exec_stmts_with_scope(
                        catch_body, recovered, registry, funcs, scope, arena,
                    );
                    // Clean up _error after catch body
                    if let Ok(ExecResult::Continue(ref mut evt)) = result {
                        evt.workspace_remove("_error");
                    }
                    result
                }
            }
        }

        ProcessStatement::ForEach(iterable_expr, body) => {
            let iterable = eval_expr_with_scope(iterable_expr, &event, funcs, scope, arena)?;
            if let Value::Array(items) = iterable {
                for item in items.iter() {
                    // Bind current item to `workspace._item` for access
                    // in body. The key is a static string literal so
                    // it can ride straight into the arena slot list.
                    event.workspace_set_str(arena, "_item", *item);
                    match exec_stmts_with_scope(body, event, registry, funcs, scope, arena)? {
                        ExecResult::Continue(e) => event = e,
                        ExecResult::Dropped => return Ok(ExecResult::Dropped),
                    }
                }
                event.workspace_remove("_item");
                Ok(ExecResult::Continue(event))
            } else {
                // Not an array, skip
                Ok(ExecResult::Continue(event))
            }
        }

        ProcessStatement::ExprStmt(expr) => {
            // Bare expression statement.
            //
            // - Object return → merge top-level keys into
            //   event.workspace (same semantic the old built-in parser
            //   processes had, now delivered by pure DSL functions like
            //   `parse_json(egress)` or `syslog.parse(ingress)`).
            // - Null return → silently accepted (for side-effect-only
            //   functions such as `table_upsert()` that don't produce a
            //   meaningful value).
            // - Anything else → error. Writing `to_json()` or
            //   `contains(...)` as a bare statement discards the result
            //   and is almost always a bug.
            let result = eval_expr_with_scope(expr, &event, funcs, scope, arena)?;
            match result {
                Value::Object(entries) => {
                    for (k, v) in entries.iter() {
                        // Both `k` (already arena-allocated by the
                        // builder that produced this object) and `v`
                        // (arena-backed `Value`) ride straight into
                        // the workspace slot list.
                        event.workspace_set(*k, *v);
                    }
                }
                Value::Null => {}
                other => bail!(
                    "bare expression statement must return Object or Null; got {}",
                    other.type_name()
                ),
            }
            Ok(ExecResult::Continue(event))
        }
    }
}

fn exec_if_chain_process<'bump>(
    if_chain: &IfChain,
    event: BorrowedEvent<'bump>,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<ExecResult<'bump>> {
    for (condition, body) in &if_chain.branches {
        let cond_val = eval_expr_with_scope(condition, &event, funcs, scope, arena)?;
        if cond_val.is_truthy() {
            return exec_branch_body_process(body, event, registry, funcs, scope, arena);
        }
    }
    if let Some(else_body) = &if_chain.else_body {
        return exec_branch_body_process(else_body, event, registry, funcs, scope, arena);
    }
    Ok(ExecResult::Continue(event))
}

fn exec_branch_body_process<'bump>(
    body: &[BranchBody],
    mut event: BorrowedEvent<'bump>,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope<'bump>,
    arena: &'bump EventArena<'bump>,
) -> Result<ExecResult<'bump>> {
    for item in body {
        match item {
            BranchBody::Process(stmt) => {
                match exec_process_stmt(stmt, event, registry, funcs, scope, arena)? {
                    ExecResult::Continue(e) => event = e,
                    ExecResult::Dropped => return Ok(ExecResult::Dropped),
                }
            }
            BranchBody::Pipeline(_) => {
                bail!("pipeline statement found in process context")
            }
        }
    }
    Ok(ExecResult::Continue(event))
}

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

fn apply_assign<'bump>(
    event: &mut BorrowedEvent<'bump>,
    target: &AssignTarget,
    value: Value<'bump>,
    arena: &EventArena<'bump>,
) -> Result<()> {
    match target {
        AssignTarget::Egress => {
            // Egress crosses the per-event arena boundary: the output
            // sink consumes a `Bytes` after the event leaves
            // `run_pipeline`, so we must lift any arena-allocated
            // payload out via `Bytes::copy_from_slice`. UTF-8 round-trip
            // via `String::into_bytes` would corrupt non-text payloads
            // (protobuf, raw binary, etc) — see v0.5.0 Bytes design §3.
            event.egress = match value {
                Value::Bytes(b) => Bytes::copy_from_slice(b),
                Value::String(s) => Bytes::copy_from_slice(s.as_bytes()),
                other => Bytes::from(value_to_string(&other)),
            };
            Ok(())
        }
        AssignTarget::Workspace(path) => {
            set_workspace_path(event, path, value, arena);
            Ok(())
        }
    }
}

/// Top-level workspace assignment. Single-segment paths drop straight
/// into the workspace slot list; multi-segment paths build (or
/// traverse) intermediate `Object` entries in the arena, then place the
/// terminal value at the leaf.
fn set_workspace_path<'bump>(
    event: &mut BorrowedEvent<'bump>,
    path: &[String],
    value: Value<'bump>,
    arena: &EventArena<'bump>,
) {
    if path.len() == 1 {
        event.workspace_set_str(arena, &path[0], value);
        return;
    }

    // Nested path: lift the existing entry (if any) into a freshly
    // built sub-tree with the leaf assignment applied, then write back.
    let head = path[0].as_str();
    let existing = event.workspace_get(head);
    let updated = set_object_path(existing, &path[1..], value, arena);
    event.workspace_set_str(arena, head, updated);
}

/// Recursive helper for nested workspace assignment. Builds a fresh
/// `Value::Object` slice with one slot replaced; intermediate entries
/// not on the assignment path are forwarded by-value (`Value` is
/// `Copy`, so this is a register copy, not a deep walk).
fn set_object_path<'bump>(
    current: Option<Value<'bump>>,
    path: &[String],
    value: Value<'bump>,
    arena: &EventArena<'bump>,
) -> Value<'bump> {
    if path.is_empty() {
        return value;
    }
    let head = path[0].as_str();
    let existing_entries: &[(&str, Value<'bump>)] = match current {
        Some(Value::Object(entries)) => entries,
        _ => &[],
    };

    // Capacity = existing + 1 (room for a new key at the leaf level).
    let mut builder =
        super::value::ObjectBuilder::with_capacity(arena, existing_entries.len() + 1);
    let mut placed = false;
    for (k, v) in existing_entries.iter() {
        if *k == head {
            let next = set_object_path(Some(*v), &path[1..], value, arena);
            builder.push(*k, next);
            placed = true;
        } else {
            builder.push(*k, *v);
        }
    }
    if !placed {
        let key_in = arena.alloc_str(head);
        let next = set_object_path(None, &path[1..], value, arena);
        builder.push(key_in, next);
    }
    builder.finish()
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Snapshot a borrowed event for the catch-path / process-error
/// recovery sites. Workspace entries are duplicated into the same arena
/// (still cheap — keys are already `&'bump str`, `Value` is `Copy`); the
/// `Bytes` payloads ride along by refcount clone.
fn clone_borrowed_event<'bump>(
    src: &BorrowedEvent<'bump>,
    arena: &'bump EventArena<'bump>,
) -> BorrowedEvent<'bump> {
    let mut workspace =
        bumpalo::collections::Vec::with_capacity_in(src.workspace.len(), arena.bump());
    for (k, v) in src.workspace.iter() {
        workspace.push((*k, *v));
    }
    BorrowedEvent {
        received_at: src.received_at,
        source: src.source,
        ingress: src.ingress.clone(),
        egress: src.egress.clone(),
        workspace,
    }
}
