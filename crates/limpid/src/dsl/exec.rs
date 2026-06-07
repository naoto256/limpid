//! Process statement executor: runs DSL process statements against a borrowed event.
//!
//! Operates exclusively on the per-event arena form
//! ([`BorrowedEvent<'bump>`]); the heap-owned [`crate::event::OwnedEvent`]
//! never enters this module. Boundary conversions happen at the
//! pipeline level (`pipeline::run_pipeline` entry/exit).

use anyhow::{bail, Result};
use bytes::Bytes;
use thiserror::Error;

use super::arena::EventArena;
use super::ast::*;
use super::eval::{eval_expr_with_scope, value_to_string, values_match, LocalScope};
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
                Some(e) => value_to_string(&eval_expr_with_scope(e, &event, funcs, scope, arena)?),
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

            // Callee processes start with their own fresh LocalScope
            // inside the registry implementation (see `exec_process_body`
            // above). Our `scope` here belongs to the caller and is
            // unaffected by the callee.
            //
            // Propagation: a sub-process error (explicit `error`
            // keyword OR runtime evaluation error) bubbles up so the
            // pipeline-level handler routes the event to error_log,
            // and the rest of the pipeline (including downstream
            // `process X | compose_ocsf` stages) does NOT run on the
            // half-failed event. Pre-fix this arm swallowed the Err
            // and continued the event with a snapshot, which made
            // `error` from inside a sub-process invisible to the
            // pipeline boundary — downstream stages would then run
            // on whatever workspace state the sub-process had set
            // before erroring (typically empty), producing
            // confusing secondary errors like
            // `compose_ocsf: unsupported class_uid`.
            //
            // Operators who want fail-soft on a particular call
            // wrap it in `try { process foo } catch { ... }`; the
            // `try`/`catch` arm below catches Err and runs the
            // recovery body with the original error message exposed
            // via `workspace._error`.
            registry
                .call(name, &evaluated_args, event, arena)
                .map(|opt_event| match opt_event {
                    Some(e) => ExecResult::Continue(e),
                    None => ExecResult::Dropped,
                })
                .map_err(|e| anyhow::anyhow!(e))
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
                    let mut result =
                        exec_stmts_with_scope(catch_body, recovered, registry, funcs, scope, arena);
                    // Clean up _error after catch body
                    if let Ok(ExecResult::Continue(ref mut evt)) = result {
                        evt.workspace_remove("_error");
                    }
                    result
                }
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
                        event.workspace_set(k, *v);
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
    let mut builder = super::value::ObjectBuilder::with_capacity(arena, existing_entries.len() + 1);
    let mut placed = false;
    for (k, v) in existing_entries.iter() {
        if *k == head {
            let next = set_object_path(Some(*v), &path[1..], value, arena);
            builder.push(k, next);
            placed = true;
        } else {
            builder.push(k, *v);
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

#[cfg(test)]
mod tests {
    use crate::dsl::value::{OwnedValue, Value};
    use bytes::Bytes;
    use std::net::SocketAddr;

    use crate::dsl::exec::*;
    use crate::event::{BorrowedEvent, Event};
    use crate::functions::FunctionRegistry;

    fn make_event() -> Event {
        Event::new(
            Bytes::from("<134>test"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        )
    }

    fn make_funcs() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = crate::functions::table::TableStore::from_configs(vec![]).unwrap();
        crate::functions::register_builtins(&mut reg, table_store);
        reg
    }

    /// Spanless [`Expr`] construction shortcut — see `eval_test::tests::e`.
    fn e(kind: ExprKind) -> Expr {
        Expr::spanless(kind)
    }

    /// Test helper: assert that `exec_process_body` returned `Err` and
    /// return that error. `ExecResult` does not implement `Debug`, so the
    /// usual `unwrap_err` / `expect_err` shortcuts don't apply — pattern
    /// matching is the equivalent.
    fn expect_exec_err(res: anyhow::Result<ExecResult<'_>>) -> anyhow::Error {
        match res {
            Ok(_) => panic!("expected Err from exec_process_body"),
            Err(e) => e,
        }
    }

    /// No-op registry that passes events through unchanged.
    struct NoopRegistry;
    impl ProcessRegistry for NoopRegistry {
        fn call<'bump>(
            &self,
            _name: &str,
            _args: &[Value<'bump>],
            event: BorrowedEvent<'bump>,
            _arena: &'bump crate::dsl::arena::EventArena<'bump>,
        ) -> Result<Option<BorrowedEvent<'bump>>, ProcessError> {
            Ok(Some(event))
        }
    }

    /// Registry that always fails.
    struct FailRegistry;
    impl ProcessRegistry for FailRegistry {
        fn call<'bump>(
            &self,
            _name: &str,
            _args: &[Value<'bump>],
            _event: BorrowedEvent<'bump>,
            _arena: &'bump crate::dsl::arena::EventArena<'bump>,
        ) -> Result<Option<BorrowedEvent<'bump>>, ProcessError> {
            Err(ProcessError::Failed("test error".into()))
        }
    }

    #[test]
    fn test_exec_assign_workspace() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["tag".into()]),
            e(ExprKind::StringLit("critical".into())),
        )];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("tag"), Some(Value::String("critical")));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_drop() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::Drop];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(_) => panic!("expected drop"),
            ExecResult::Dropped => {} // ok
        }
    }

    #[test]
    fn test_exec_error_with_message_bubbles_up() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `error "msg"` should produce an Err whose Display contains the
        // rendered message. The pipeline-level handler then turns this
        // into a DLQ entry — same path as a runtime process error.
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::Error(Some(e(ExprKind::StringLit(
            "explicit failure".into(),
        ))))];
        let res = exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena);
        let err = expect_exec_err(res);
        assert!(
            err.to_string().contains("explicit failure"),
            "expected message to surface, got: {}",
            err
        );
    }

    #[test]
    fn test_exec_error_without_message_uses_default() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::Error(None)];
        let err = expect_exec_err(exec_process_body(
            &stmts,
            bevent,
            &NoopRegistry,
            &make_funcs(),
            &arena,
        ));
        // Default message is operator-readable; assert on a stable
        // substring rather than the full string so cosmetic tweaks
        // don't churn the test.
        assert!(
            err.to_string().contains("explicit error"),
            "expected default message, got: {}",
            err
        );
    }

    #[test]
    fn test_exec_error_with_interpolated_message() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `error "subtype ${workspace.kind} unsupported"` must render the
        // interpolation against the current event before bubbling.
        use crate::dsl::ast::TemplateFragment;
        let mut event = make_event();
        event
            .workspace
            .insert("kind".into(), OwnedValue::String("foo".into()));
        let bevent = event.view_in(&arena);
        let template = e(ExprKind::Template(vec![
            TemplateFragment::Literal("subtype ".into()),
            TemplateFragment::Interp(e(ExprKind::Ident(vec!["workspace".into(), "kind".into()]))),
            TemplateFragment::Literal(" unsupported".into()),
        ]));
        let stmts = vec![ProcessStatement::Error(Some(template))];
        let err = expect_exec_err(exec_process_body(
            &stmts,
            bevent,
            &NoopRegistry,
            &make_funcs(),
            &arena,
        ));
        assert!(
            err.to_string().contains("subtype foo unsupported"),
            "expected interpolated message, got: {}",
            err
        );
    }

    #[test]
    fn test_exec_if_true_branch() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::If(IfChain {
            branches: vec![(
                e(ExprKind::BoolLit(true)),
                vec![BranchBody::Process(ProcessStatement::Assign(
                    AssignTarget::Workspace(vec!["hit".into()]),
                    e(ExprKind::BoolLit(true)),
                ))],
            )],
            else_body: None,
        })];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("hit"), Some(Value::Bool(true)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_if_else_branch() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::If(IfChain {
            branches: vec![(
                e(ExprKind::BoolLit(false)),
                vec![BranchBody::Process(ProcessStatement::Assign(
                    AssignTarget::Workspace(vec!["branch".into()]),
                    e(ExprKind::StringLit("if".into())),
                ))],
            )],
            else_body: Some(vec![BranchBody::Process(ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["branch".into()]),
                e(ExprKind::StringLit("else".into())),
            ))]),
        })];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("branch"), Some(Value::String("else")));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_process_error_propagates_to_caller() {
        // A sub-process error MUST propagate up to the caller's
        // pipeline boundary so the event routes to error_log and the
        // rest of the pipe (`process X | compose_Y`) is skipped.
        // Pre-fix this returned `Ok(Continue(backup))` and silently
        // continued the event through the pipeline with a snapshot of
        // its workspace, which made `error` from inside a sub-process
        // invisible at the pipeline boundary — downstream stages then
        // ran on whatever workspace state the sub-process had set
        // before erroring (typically empty), producing confusing
        // secondary errors like `compose_ocsf: unsupported class_uid`.
        // Operators who want fail-soft on a particular call wrap it
        // in `try { process foo } catch { ... }` (covered by
        // `test_exec_try_catch_on_error` below).
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::ProcessCall("failing".into(), vec![])];
        let result = exec_process_body(&stmts, bevent, &FailRegistry, &make_funcs(), &arena);
        match result {
            Err(e) => assert!(
                e.to_string().contains("test error"),
                "expected propagated error to mention `test error`, got: {}",
                e
            ),
            Ok(_) => panic!("sub-process error should propagate as Err"),
        }
    }

    #[test]
    fn test_exec_try_catch_on_error() {
        // `try { process failing } catch { workspace.caught = true }`
        // — the failing sub-process's error propagates up to the
        // surrounding try/catch, which runs the recovery body.
        // Pre-fix the sub-process error was swallowed at the
        // ProcessCall arm so the catch body never ran (the test
        // pre-fix asserted only `Continue(_)` because of that, which
        // hid the bug); post-fix the catch body runs and sets
        // `workspace.caught = true`.
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::TryCatch(
            vec![ProcessStatement::ProcessCall("failing".into(), vec![])],
            vec![ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["caught".into()]),
                e(ExprKind::BoolLit(true)),
            )],
        )];
        match exec_process_body(&stmts, bevent, &FailRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(
                    ev.workspace_get("caught"),
                    Some(Value::Bool(true)),
                    "catch body should have run after sub-process error"
                );
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    // ---- let bindings --------------------------------------------------

    #[test]
    fn let_binding_resolves_via_bare_ident_in_same_body() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `let x = 7; workspace.y = x` — workspace.y becomes Number(7).
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::LetBinding("x".into(), e(ExprKind::IntLit(7))),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["y".into()]),
                e(ExprKind::Ident(vec!["x".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("y"), Some(Value::Int(7)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_binding_object_value_supports_dot_access() {
        // `let f = { a: 7, b: 9 }; workspace.x = f.a; workspace.y = f.b`
        // — dot-access on a let-bound Object resolves through the
        // local scope and walks into the Object the same way
        // workspace.x.y would. Regression test for the gap that made
        // `let f = regex_parse(...); f.user` fail at runtime with
        // "unknown identifier: f.user".
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let obj = e(ExprKind::HashLit(vec![
            ("a".into(), e(ExprKind::IntLit(7))),
            ("b".into(), e(ExprKind::IntLit(9))),
        ]));
        let stmts = vec![
            ProcessStatement::LetBinding("f".into(), obj),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["x".into()]),
                e(ExprKind::Ident(vec!["f".into(), "a".into()])),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["y".into()]),
                e(ExprKind::Ident(vec!["f".into(), "b".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("x"), Some(Value::Int(7)));
                assert_eq!(ev.workspace_get("y"), Some(Value::Int(9)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_binding_object_dot_access_missing_key_yields_null() {
        // `let f = { a: 1 }; workspace.miss = f.nonexistent` — the
        // walker should yield Null (not error) so callers can treat
        // missing keys with coalesce / explicit null comparisons,
        // matching the workspace.* path-walker contract.
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let obj = e(ExprKind::HashLit(vec![(
            "a".into(),
            e(ExprKind::IntLit(1)),
        )]));
        let stmts = vec![
            ProcessStatement::LetBinding("f".into(), obj),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["miss".into()]),
                e(ExprKind::Ident(vec!["f".into(), "nonexistent".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("miss"), Some(Value::Null));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_binding_object_supports_nested_dot_path() {
        // `let f = { a: { b: 7 } }; workspace.x = f.a.b` — multi-segment
        // path access (>2 segments) walks into a nested Object the
        // same way `workspace.x.y.z` does.
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let inner = e(ExprKind::HashLit(vec![(
            "b".into(),
            e(ExprKind::IntLit(7)),
        )]));
        let outer = e(ExprKind::HashLit(vec![("a".into(), inner)]));
        let stmts = vec![
            ProcessStatement::LetBinding("f".into(), outer),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["x".into()]),
                e(ExprKind::Ident(vec!["f".into(), "a".into(), "b".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("x"), Some(Value::Int(7)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_binding_rebound_object_replaces_prior_value() {
        // `let f = { a: 1 }; let f = { a: 2 }; workspace.x = f.a` —
        // shadowing / re-binding follows the same `bind` semantics
        // for Object values as for scalars (covered by
        // `let_shadows_prior_binding_with_same_name`); dot-access on
        // the latest binding sees the new Object.
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let obj1 = e(ExprKind::HashLit(vec![(
            "a".into(),
            e(ExprKind::IntLit(1)),
        )]));
        let obj2 = e(ExprKind::HashLit(vec![(
            "a".into(),
            e(ExprKind::IntLit(2)),
        )]));
        let stmts = vec![
            ProcessStatement::LetBinding("f".into(), obj1),
            ProcessStatement::LetBinding("f".into(), obj2),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["x".into()]),
                e(ExprKind::Ident(vec!["f".into(), "a".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("x"), Some(Value::Int(2)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_shadows_prior_binding_with_same_name() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `let x = 1; let x = 2; workspace.y = x` — workspace.y is 2.
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::LetBinding("x".into(), e(ExprKind::IntLit(1))),
            ProcessStatement::LetBinding("x".into(), e(ExprKind::IntLit(2))),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["y".into()]),
                e(ExprKind::Ident(vec!["x".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("y"), Some(Value::Int(2)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_is_visible_inside_if_branch_declared_above() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::LetBinding("m".into(), e(ExprKind::StringLit("hit".into()))),
            ProcessStatement::If(IfChain {
                branches: vec![(
                    e(ExprKind::BoolLit(true)),
                    vec![BranchBody::Process(ProcessStatement::Assign(
                        AssignTarget::Workspace(vec!["tag".into()]),
                        e(ExprKind::Ident(vec!["m".into()])),
                    ))],
                )],
                else_body: None,
            }),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("tag"), Some(Value::String("hit")));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_scope_does_not_leak_between_top_level_bodies() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // `exec_process_body` starts a fresh scope each call. Running
        // two bodies back-to-back must not carry x from the first into
        // the second — referencing `x` in the second body fails.
        let funcs = make_funcs();
        let event = make_event();
        let first = vec![ProcessStatement::LetBinding(
            "x".into(),
            e(ExprKind::IntLit(1)),
        )];
        let _ = exec_process_body(&first, event.view_in(&arena), &NoopRegistry, &funcs, &arena)
            .unwrap();

        let second = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["y".into()]),
            e(ExprKind::Ident(vec!["x".into()])),
        )];
        let err = expect_exec_err(exec_process_body(
            &second,
            event.view_in(&arena),
            &NoopRegistry,
            &funcs,
            &arena,
        ));
        assert!(
            err.to_string().contains("unknown identifier"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn let_is_referenced_in_template_interpolation() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::LetBinding("host".into(), e(ExprKind::StringLit("web01".into()))),
            ProcessStatement::Assign(
                AssignTarget::Egress,
                e(ExprKind::Template(vec![
                    TemplateFragment::Literal("hello ".into()),
                    TemplateFragment::Interp(e(ExprKind::Ident(vec!["host".into()]))),
                ])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(&*ev.egress, b"hello web01");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_does_not_survive_try_catch_failure() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // let bindings introduced inside a try that later fails are
        // discarded before the catch runs.
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::TryCatch(
            vec![
                ProcessStatement::LetBinding("x".into(), e(ExprKind::IntLit(9))),
                // Force an error: `unknown identifier` on bare `nope`
                ProcessStatement::Assign(
                    AssignTarget::Workspace(vec!["y".into()]),
                    e(ExprKind::Ident(vec!["nope".into()])),
                ),
            ],
            vec![
                // x should NOT be in scope here because the try failed.
                ProcessStatement::Assign(
                    AssignTarget::Workspace(vec!["recovered".into()]),
                    e(ExprKind::Ident(vec!["x".into()])),
                ),
            ],
        )];
        let err = expect_exec_err(exec_process_body(
            &stmts,
            bevent,
            &NoopRegistry,
            &make_funcs(),
            &arena,
        ));
        assert!(
            err.to_string().contains("unknown identifier"),
            "expected catch to fail resolving x, got: {}",
            err
        );
    }

    // ------------------------------------------------------------------------
    // Array literal + primitives E2E — these exercise the full evaluator
    // path (ExprKind::ArrayLit through exec_process_body's Assign arm,
    // function registry dispatch for len / append / prepend / find).
    // ------------------------------------------------------------------------

    fn call_fn(name: &str, args: Vec<Expr>) -> Expr {
        e(ExprKind::FuncCall {
            namespace: None,
            name: name.into(),
            args,
            block_arg: None,
        })
    }

    fn block_call(name: &str, args: Vec<Expr>, params: Vec<&str>, ret: Expr) -> Expr {
        e(ExprKind::FuncCall {
            namespace: None,
            name: name.into(),
            args,
            block_arg: Some(Box::new(BlockArg {
                params: params.into_iter().map(str::to_string).collect(),
                body: FuncBody { lets: vec![], ret },
            })),
        })
    }

    #[test]
    fn test_exec_array_literal_into_workspace() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["types".into()]),
            e(ExprKind::ArrayLit(vec![
                e(ExprKind::StringLit("sqli".into())),
                e(ExprKind::StringLit("xss".into())),
            ])),
        )];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(
                    ev.workspace_get("types").unwrap().to_owned_value(),
                    OwnedValue::Array(vec![
                        OwnedValue::String("sqli".into()),
                        OwnedValue::String("xss".into()),
                    ])
                );
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_len_over_array_literal() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["n".into()]),
            call_fn(
                "len",
                vec![e(ExprKind::ArrayLit(vec![
                    e(ExprKind::IntLit(1)),
                    e(ExprKind::IntLit(2)),
                    e(ExprKind::IntLit(3)),
                ]))],
            ),
        )];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("n"), Some(Value::Int(3)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_append_grows_array() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // workspace.xs = [1, 2]
        // workspace.xs = append(workspace.xs, 3)
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["xs".into()]),
                e(ExprKind::ArrayLit(vec![
                    e(ExprKind::IntLit(1)),
                    e(ExprKind::IntLit(2)),
                ])),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["xs".into()]),
                call_fn(
                    "append",
                    vec![
                        e(ExprKind::Ident(vec!["workspace".into(), "xs".into()])),
                        e(ExprKind::IntLit(3)),
                    ],
                ),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(
                    ev.workspace_get("xs").unwrap().to_owned_value(),
                    OwnedValue::Array(vec![
                        OwnedValue::Int(1),
                        OwnedValue::Int(2),
                        OwnedValue::Int(3),
                    ])
                );
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_prepend_grows_array_at_front() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["xs".into()]),
                e(ExprKind::ArrayLit(vec![
                    e(ExprKind::IntLit(2)),
                    e(ExprKind::IntLit(3)),
                ])),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["xs".into()]),
                call_fn(
                    "prepend",
                    vec![
                        e(ExprKind::Ident(vec!["workspace".into(), "xs".into()])),
                        e(ExprKind::IntLit(1)),
                    ],
                ),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(
                    ev.workspace_get("xs").unwrap().to_owned_value(),
                    OwnedValue::Array(vec![
                        OwnedValue::Int(1),
                        OwnedValue::Int(2),
                        OwnedValue::Int(3),
                    ])
                );
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_block_array_primitives_transform_filter_find_reduce() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let nums = e(ExprKind::ArrayLit(vec![
            e(ExprKind::IntLit(1)),
            e(ExprKind::IntLit(2)),
            e(ExprKind::IntLit(3)),
            e(ExprKind::IntLit(4)),
        ]));
        let n = || e(ExprKind::Ident(vec!["n".into()]));
        let stmts = vec![
            ProcessStatement::Assign(AssignTarget::Workspace(vec!["nums".into()]), nums),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["doubled".into()]),
                block_call(
                    "map",
                    vec![e(ExprKind::Ident(vec!["workspace".into(), "nums".into()]))],
                    vec!["n"],
                    e(ExprKind::BinOp(
                        Box::new(n()),
                        BinOp::Mul,
                        Box::new(e(ExprKind::IntLit(2))),
                    )),
                ),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["evens".into()]),
                block_call(
                    "filter",
                    vec![e(ExprKind::Ident(vec!["workspace".into(), "nums".into()]))],
                    vec!["n"],
                    e(ExprKind::BinOp(
                        Box::new(e(ExprKind::BinOp(
                            Box::new(n()),
                            BinOp::Mod,
                            Box::new(e(ExprKind::IntLit(2))),
                        ))),
                        BinOp::Eq,
                        Box::new(e(ExprKind::IntLit(0))),
                    )),
                ),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["found".into()]),
                block_call(
                    "find",
                    vec![e(ExprKind::Ident(vec!["workspace".into(), "nums".into()]))],
                    vec!["n"],
                    e(ExprKind::BinOp(
                        Box::new(n()),
                        BinOp::Gt,
                        Box::new(e(ExprKind::IntLit(2))),
                    )),
                ),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["total".into()]),
                block_call(
                    "reduce",
                    vec![
                        e(ExprKind::Ident(vec!["workspace".into(), "nums".into()])),
                        e(ExprKind::IntLit(0)),
                    ],
                    vec!["acc", "n"],
                    e(ExprKind::BinOp(
                        Box::new(e(ExprKind::Ident(vec!["acc".into()]))),
                        BinOp::Add,
                        Box::new(n()),
                    )),
                ),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(
                    ev.workspace_get("doubled").unwrap().to_owned_value(),
                    OwnedValue::Array(vec![
                        OwnedValue::Int(2),
                        OwnedValue::Int(4),
                        OwnedValue::Int(6),
                        OwnedValue::Int(8),
                    ])
                );
                assert_eq!(
                    ev.workspace_get("evens").unwrap().to_owned_value(),
                    OwnedValue::Array(vec![OwnedValue::Int(2), OwnedValue::Int(4)])
                );
                assert_eq!(ev.workspace_get("found"), Some(Value::Int(3)));
                assert_eq!(ev.workspace_get("total"), Some(Value::Int(10)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_array_helper_primitives_cover_shape_and_reductions() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let xs = || {
            e(ExprKind::ArrayLit(vec![
                e(ExprKind::IntLit(1)),
                e(ExprKind::IntLit(2)),
                e(ExprKind::IntLit(2)),
                e(ExprKind::IntLit(3)),
            ]))
        };
        let stmts = vec![
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["first".into()]),
                call_fn("first", vec![xs()]),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["last".into()]),
                call_fn("last", vec![xs()]),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["distinct".into()]),
                call_fn("distinct", vec![xs()]),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["concat".into()]),
                call_fn(
                    "concat",
                    vec![
                        e(ExprKind::ArrayLit(vec![e(ExprKind::IntLit(1))])),
                        e(ExprKind::ArrayLit(vec![
                            e(ExprKind::IntLit(2)),
                            e(ExprKind::IntLit(3)),
                        ])),
                    ],
                ),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["sum".into()]),
                call_fn("sum", vec![xs()]),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["max".into()]),
                call_fn("max", vec![xs()]),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["min".into()]),
                call_fn("min", vec![xs()]),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["named".into()]),
                call_fn(
                    "entitle",
                    vec![
                        e(ExprKind::ArrayLit(vec![
                            e(ExprKind::StringLit("alice".into())),
                            e(ExprKind::IntLit(7)),
                        ])),
                        e(ExprKind::ArrayLit(vec![
                            e(ExprKind::StringLit("user".into())),
                            e(ExprKind::StringLit("score".into())),
                        ])),
                    ],
                ),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["score".into()]),
                call_fn(
                    "path",
                    vec![
                        e(ExprKind::Ident(vec!["workspace".into(), "named".into()])),
                        e(ExprKind::StringLit("score".into())),
                    ],
                ),
            ),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["is_array".into()]),
                call_fn("is_array", vec![xs()]),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("first"), Some(Value::Int(1)));
                assert_eq!(ev.workspace_get("last"), Some(Value::Int(3)));
                assert_eq!(
                    ev.workspace_get("distinct").unwrap().to_owned_value(),
                    OwnedValue::Array(vec![
                        OwnedValue::Int(1),
                        OwnedValue::Int(2),
                        OwnedValue::Int(3),
                    ])
                );
                assert_eq!(
                    ev.workspace_get("concat").unwrap().to_owned_value(),
                    OwnedValue::Array(vec![
                        OwnedValue::Int(1),
                        OwnedValue::Int(2),
                        OwnedValue::Int(3),
                    ])
                );
                assert_eq!(ev.workspace_get("sum"), Some(Value::Int(8)));
                assert_eq!(ev.workspace_get("max"), Some(Value::Int(3)));
                assert_eq!(ev.workspace_get("min"), Some(Value::Int(1)));
                assert_eq!(ev.workspace_get("score"), Some(Value::Int(7)));
                assert_eq!(ev.workspace_get("is_array"), Some(Value::Bool(true)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_path_rejects_integer_array_index_escape_hatch() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["bad".into()]),
            call_fn(
                "path",
                vec![
                    e(ExprKind::ArrayLit(vec![e(ExprKind::StringLit("x".into()))])),
                    e(ExprKind::IntLit(0)),
                ],
            ),
        )];
        let err = expect_exec_err(exec_process_body(
            &stmts,
            bevent,
            &NoopRegistry,
            &make_funcs(),
            &arena,
        ));
        assert!(
            err.to_string().contains("rejects integer keys"),
            "expected path integer-key rejection, got: {}",
            err
        );
    }

    // ------------------------------------------------------------------------
    // Process-layer behaviour pin tests (added 2026-04-25 during the v0.5.0
    // OTLP / Bytes refactor). Each test exercises one of the five process
    // areas flagged for triage: try/catch error binding, drop chain
    // semantics, Bytes-in-Object merge, let-scope hoisting, ForEach loop
    // variable lifetime. The goal is not new behaviour but to pin the
    // current shape so a later refactor cannot quietly drift.
    // ------------------------------------------------------------------------

    /// Concern 1: inside a `catch { ... }` body the bare `error` ident
    /// must resolve to a string carrying the error that triggered the
    /// recovery. The implementation routes this through
    /// `workspace._error` (set in exec.rs before running the catch
    /// body) and the resolver in eval.rs maps the bare `error` ident
    /// onto that slot. This test pins the user-visible binding.
    #[test]
    fn catch_body_sees_error_message_via_bare_ident() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::TryCatch(
            // try: force a runtime error by referencing an unknown
            // identifier — eval.rs::resolve_ident bails with
            // "unknown identifier".
            vec![ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["x".into()]),
                e(ExprKind::Ident(vec!["nope_not_a_thing".into()])),
            )],
            // catch: copy the bare `error` ident into workspace.captured
            // so we can assert on the recovered message.
            vec![ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["captured".into()]),
                e(ExprKind::Ident(vec!["error".into()])),
            )],
        )];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                let msg = match ev.workspace_get("captured") {
                    Some(Value::String(s)) => s.to_string(),
                    other => panic!("expected captured to be a string, got {:?}", other),
                };
                assert!(
                    msg.contains("unknown identifier"),
                    "catch should bind the original error message; got {msg:?}"
                );
                // Cleanup invariant: `_error` is removed before the
                // event continues so a downstream `error` access does
                // not see a stale message.
                assert!(
                    ev.workspace_get("_error").is_none(),
                    "_error should be cleared after catch body"
                );
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    /// Concern 2 (inline form): `drop` inside an inline body
    /// short-circuits subsequent statements. The chain-level
    /// `process A | B | C` form delegates each `Inline(body)` element
    /// to `exec_process_body`, so this test covers the inline-element
    /// path; the named-process path is exercised elsewhere via
    /// `ProcessRegistry::call` returning `Ok(None)`.
    #[test]
    fn drop_short_circuits_inline_statements() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["before".into()]),
                e(ExprKind::IntLit(1)),
            ),
            ProcessStatement::Drop,
            // This must NOT execute — if it did, the assertion would
            // fail because the body returned ExecResult::Dropped (no
            // Continue event) and we never see workspace.after.
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["after".into()]),
                e(ExprKind::IntLit(2)),
            ),
        ];
        let result =
            exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap();
        assert!(matches!(result, ExecResult::Dropped));
    }

    /// Concern 3: a bare expression statement that yields a
    /// `Value::Object` merges the top-level keys into `workspace`.
    /// After the v0.5.0 Bytes refactor, Object values can carry
    /// `Value::Bytes`, and the merge must not coerce or reject those
    /// — workspace stores them verbatim. Subsequent text primitives
    /// would error if they touched the bytes, but storage itself is
    /// fine.
    #[test]
    fn expr_stmt_merges_bytes_value_into_workspace() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        // Build `{ payload: <bytes>, label: "ok" }` as an inline
        // HashLit and run it as a bare expression statement.
        let stmts = vec![ProcessStatement::ExprStmt(e(ExprKind::HashLit(vec![
            (
                "payload".into(),
                // No DSL syntax for bytes literals, so route through
                // `to_bytes(...)` which returns Value::Bytes.
                e(ExprKind::FuncCall {
                    namespace: None,
                    name: "to_bytes".into(),
                    args: vec![e(ExprKind::StringLit("hi".into()))],
                    block_arg: None,
                }),
            ),
            ("label".into(), e(ExprKind::StringLit("ok".into()))),
        ])))];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                match ev.workspace_get("payload") {
                    Some(Value::Bytes(b)) => assert_eq!(b, b"hi"),
                    other => panic!("expected workspace.payload to be Bytes, got {:?}", other),
                }
                assert_eq!(ev.workspace_get("label"), Some(Value::String("ok")));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    /// Concern 4: a `let` introduced inside an `if` branch is hoisted
    /// to the surrounding process body — there are no inner scopes.
    /// Code reading top-to-bottom matches what executes, and this
    /// matches the behaviour documented on `exec_stmts_with_scope`.
    #[test]
    fn let_inside_if_branch_leaks_to_outer_scope() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            ProcessStatement::If(IfChain {
                branches: vec![(
                    e(ExprKind::BoolLit(true)),
                    vec![BranchBody::Process(ProcessStatement::LetBinding(
                        "x".into(),
                        e(ExprKind::IntLit(7)),
                    ))],
                )],
                else_body: None,
            }),
            // After the if, `x` is still in scope. If branches had
            // their own inner scope this assignment would error.
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["y".into()]),
                e(ExprKind::Ident(vec!["x".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("y"), Some(Value::Int(7)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }
}
