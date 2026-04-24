//! Process statement executor: runs DSL process statements against an Event.

use anyhow::{Result, bail};
use bytes::Bytes;
use serde_json::Value;

use super::ast::*;
use super::eval::{LocalScope, eval_expr_with_scope, is_truthy, value_to_string, values_match};
use crate::event::Event;
use crate::functions::FunctionRegistry;
use crate::modules::ProcessError;

/// Result of executing a process body.
#[derive(Debug)]
pub enum ExecResult {
    /// Event passed through (possibly mutated).
    Continue(Event),
    /// Event was dropped.
    Dropped,
}

/// A registry of named processes that can be called from DSL.
pub trait ProcessRegistry {
    fn call(
        &self,
        name: &str,
        args: &[Value],
        event: Event,
    ) -> std::result::Result<Option<Event>, ProcessError>;
}

/// Execute a sequence of process statements against an event.
///
/// Each call starts with a fresh [`LocalScope`] — `let` bindings do not
/// leak across process-body boundaries. This is intentional: callee
/// processes shouldn't see the caller's scratch material, and vice
/// versa. The only channel between caller and callee is the Event
/// itself (`workspace` and metadata).
pub fn exec_process_body(
    stmts: &[ProcessStatement],
    event: Event,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
) -> Result<ExecResult> {
    let mut scope = LocalScope::new();
    exec_stmts_with_scope(stmts, event, registry, funcs, &mut scope)
}

/// Run statements with the given local scope. `let` bindings mutate
/// `scope`; branch / loop bodies are run with the same scope so a
/// `let x` written above an `if` is visible inside (and below) the
/// branch. Branches do not introduce inner scopes — every `let` is
/// hoisted to the process body scope — which is the simplest useful
/// semantics and matches how users read the code top-to-bottom.
fn exec_stmts_with_scope(
    stmts: &[ProcessStatement],
    mut event: Event,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope,
) -> Result<ExecResult> {
    for stmt in stmts {
        match exec_process_stmt(stmt, event, registry, funcs, scope)? {
            ExecResult::Continue(e) => event = e,
            ExecResult::Dropped => return Ok(ExecResult::Dropped),
        }
    }
    Ok(ExecResult::Continue(event))
}

fn exec_process_stmt(
    stmt: &ProcessStatement,
    mut event: Event,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope,
) -> Result<ExecResult> {
    match stmt {
        ProcessStatement::Drop => Ok(ExecResult::Dropped),

        ProcessStatement::Assign(target, expr) => {
            let value = eval_expr_with_scope(expr, &event, funcs, scope)?;
            apply_assign(&mut event, target, value)?;
            Ok(ExecResult::Continue(event))
        }

        ProcessStatement::LetBinding(name, expr) => {
            let value = eval_expr_with_scope(expr, &event, funcs, scope)?;
            scope.bind(name, value);
            Ok(ExecResult::Continue(event))
        }

        ProcessStatement::ProcessCall(name, args) => {
            let evaluated_args: Vec<Value> = args
                .iter()
                .map(|a| eval_expr_with_scope(a, &event, funcs, scope))
                .collect::<Result<Vec<_>>>()?;

            // Clone before calling — required because registry.call takes ownership.
            // On error we restore from backup. Future optimization: pass &Event
            // and let the process clone only if it needs to mutate.
            // Callee processes start with their own fresh LocalScope
            // inside the registry implementation (see `exec_process_body`
            // above). Our `scope` here belongs to the caller and is
            // unaffected by the callee.
            let backup = event.clone();
            match registry.call(name, &evaluated_args, event) {
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
            exec_if_chain_process(if_chain, event, registry, funcs, scope)
        }

        ProcessStatement::Switch(discriminant, arms) => {
            let disc_val = eval_expr_with_scope(discriminant, &event, funcs, scope)?;
            for arm in arms {
                if arm.pattern.is_none() {
                    // default arm
                    return exec_branch_body_process(&arm.body, event, registry, funcs, scope);
                }
                let pattern_val =
                    eval_expr_with_scope(arm.pattern.as_ref().unwrap(), &event, funcs, scope)?;
                if values_match(&disc_val, &pattern_val) {
                    return exec_branch_body_process(&arm.body, event, registry, funcs, scope);
                }
            }
            // No arm matched, pass through
            Ok(ExecResult::Continue(event))
        }

        ProcessStatement::TryCatch(try_body, catch_body) => {
            // Clone event for try block so we can recover on error.
            // Snapshot the scope too — a failed try must not leak its
            // let bindings into the catch body; the catch gets the
            // scope the try started with.
            let event_backup = event.clone();
            let scope_backup = scope.clone();
            match exec_stmts_with_scope(try_body, event, registry, funcs, scope) {
                Ok(result) => Ok(result),
                Err(e) => {
                    *scope = scope_backup;
                    // Bind error message to `error` identifier (accessible via workspace._error)
                    let mut recovered = event_backup;
                    recovered
                        .workspace
                        .insert("_error".into(), serde_json::Value::String(e.to_string()));
                    let mut result =
                        exec_stmts_with_scope(catch_body, recovered, registry, funcs, scope);
                    // Clean up _error after catch body
                    if let Ok(ExecResult::Continue(ref mut evt)) = result {
                        evt.workspace.remove("_error");
                    }
                    result
                }
            }
        }

        ProcessStatement::ForEach(iterable_expr, body) => {
            let iterable = eval_expr_with_scope(iterable_expr, &event, funcs, scope)?;
            if let Value::Array(items) = iterable {
                for item in &items {
                    // Bind current item to `workspace._item` for access in body
                    event.workspace.insert("_item".into(), item.clone());
                    match exec_stmts_with_scope(body, event, registry, funcs, scope)? {
                        ExecResult::Continue(e) => event = e,
                        ExecResult::Dropped => return Ok(ExecResult::Dropped),
                    }
                }
                // Clean up loop variable
                event.workspace.remove("_item");
                Ok(ExecResult::Continue(event))
            } else {
                // Not an array, skip
                Ok(ExecResult::Continue(event))
            }
        }

        ProcessStatement::ExprStmt(expr) => {
            // Evaluate for side effects, discard result
            let _ = eval_expr_with_scope(expr, &event, funcs, scope)?;
            Ok(ExecResult::Continue(event))
        }
    }
}

fn exec_if_chain_process(
    if_chain: &IfChain,
    event: Event,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope,
) -> Result<ExecResult> {
    for (condition, body) in &if_chain.branches {
        let cond_val = eval_expr_with_scope(condition, &event, funcs, scope)?;
        if is_truthy(&cond_val) {
            return exec_branch_body_process(body, event, registry, funcs, scope);
        }
    }
    if let Some(else_body) = &if_chain.else_body {
        return exec_branch_body_process(else_body, event, registry, funcs, scope);
    }
    Ok(ExecResult::Continue(event))
}

fn exec_branch_body_process(
    body: &[BranchBody],
    mut event: Event,
    registry: &dyn ProcessRegistry,
    funcs: &FunctionRegistry,
    scope: &mut LocalScope,
) -> Result<ExecResult> {
    for item in body {
        match item {
            BranchBody::Process(stmt) => {
                match exec_process_stmt(stmt, event, registry, funcs, scope)? {
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

fn apply_assign(event: &mut Event, target: &AssignTarget, value: Value) -> Result<()> {
    match target {
        AssignTarget::Egress => {
            event.egress = Bytes::from(value_to_string(&value));
            Ok(())
        }
        AssignTarget::Severity => {
            event.severity = match &value {
                Value::Number(n) => {
                    let v = n.as_u64().ok_or_else(|| {
                        anyhow::anyhow!("severity must be a non-negative integer")
                    })?;
                    if v > 7 {
                        bail!("severity must be 0-7, got {}", v);
                    }
                    Some(v as u8)
                }
                Value::Null => None,
                _ => bail!("severity must be a number"),
            };
            sync_egress_pri(event);
            Ok(())
        }
        AssignTarget::Facility => {
            event.facility = match &value {
                Value::Number(n) => {
                    let v = n.as_u64().ok_or_else(|| {
                        anyhow::anyhow!("facility must be a non-negative integer")
                    })?;
                    if v > 23 {
                        bail!("facility must be 0-23, got {}", v);
                    }
                    Some(v as u8)
                }
                Value::Null => None,
                _ => bail!("facility must be a number"),
            };
            sync_egress_pri(event);
            Ok(())
        }
        AssignTarget::Workspace(path) => {
            set_workspace_path(&mut event.workspace, path, value);
            Ok(())
        }
    }
}

/// If event.egress starts with a valid `<PRI>`, rewrite it to reflect
/// the current facility/severity metadata.  Metadata fields that are
/// None fall back to the value already encoded in the PRI.
fn sync_egress_pri(event: &mut Event) {
    let msg = &event.egress;

    if msg.first() != Some(&b'<') {
        return;
    }
    let limit = msg.len().min(6);
    let gt_pos = match msg[..limit].iter().position(|&b| b == b'>') {
        Some(pos) if pos >= 2 => pos,
        _ => return,
    };

    let prival_bytes = &msg[1..gt_pos];
    let old_pri: u16 = match std::str::from_utf8(prival_bytes)
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(v) if v <= 191 => v,
        _ => return,
    };

    let old_facility = (old_pri / 8) as u8;
    let old_severity = (old_pri % 8) as u8;

    let facility = event.facility.unwrap_or(old_facility);
    let severity = event.severity.unwrap_or(old_severity);
    let new_pri = (facility as u16) * 8 + (severity as u16);

    if new_pri == old_pri {
        return;
    }

    let rest = &msg[gt_pos + 1..];
    let header = format!("<{}>", new_pri);
    let mut buf = Vec::with_capacity(header.len() + rest.len());
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(rest);
    event.egress = Bytes::from(buf);
}

fn set_workspace_path(
    workspace: &mut std::collections::HashMap<String, Value>,
    path: &[String],
    value: Value,
) {
    if path.len() == 1 {
        workspace.insert(path[0].clone(), value);
        return;
    }

    // Nested path: ensure intermediate objects exist
    let entry = workspace
        .entry(path[0].clone())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    if let Value::Object(map) = entry {
        set_object_path(map, &path[1..], value);
    }
}

fn set_object_path(map: &mut serde_json::Map<String, Value>, path: &[String], value: Value) {
    if path.len() == 1 {
        map.insert(path[0].clone(), value);
        return;
    }

    let entry = map
        .entry(path[0].clone())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    if let Value::Object(inner) = entry {
        set_object_path(inner, &path[1..], value);
    }
}
