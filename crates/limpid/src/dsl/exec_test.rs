//! Unit tests for the DSL process statement executor.

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::Value;
    use std::net::SocketAddr;

    use crate::dsl::ast::*;
    use crate::dsl::exec::*;
    use crate::event::Event;
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

    /// No-op registry that passes events through unchanged.
    struct NoopRegistry;
    impl ProcessRegistry for NoopRegistry {
        fn call(
            &self,
            _name: &str,
            _args: &[Value],
            event: Event,
        ) -> Result<Option<Event>, ProcessError> {
            Ok(Some(event))
        }
    }

    /// Registry that always fails.
    struct FailRegistry;
    impl ProcessRegistry for FailRegistry {
        fn call(
            &self,
            _name: &str,
            _args: &[Value],
            _event: Event,
        ) -> Result<Option<Event>, ProcessError> {
            Err(ProcessError::Failed("test error".into()))
        }
    }

    #[test]
    fn test_exec_assign_workspace() {
        let event = make_event();
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["tag".into()]),
            e(ExprKind::StringLit("critical".into())),
        )];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["tag"], Value::String("critical".into()));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_drop() {
        let event = make_event();
        let stmts = vec![ProcessStatement::Drop];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(_) => panic!("expected drop"),
            ExecResult::Dropped => {} // ok
        }
    }

    #[test]
    fn test_exec_if_true_branch() {
        let event = make_event();
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
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["hit"], Value::Bool(true));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_if_else_branch() {
        let event = make_event();
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
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["branch"], Value::String("else".into()));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_process_error_passes_through() {
        let event = make_event();
        let stmts = vec![ProcessStatement::ProcessCall("failing".into(), vec![])];
        match exec_process_body(&stmts, event, &FailRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                // Event should pass through unchanged
                assert_eq!(&*ev.ingress, b"<134>test");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_try_catch_on_error() {
        let event = make_event();
        let stmts = vec![ProcessStatement::TryCatch(
            vec![ProcessStatement::ProcessCall("failing".into(), vec![])],
            vec![ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["caught".into()]),
                e(ExprKind::BoolLit(true)),
            )],
        )];
        // Note: with FailRegistry, the process call returns Err but exec.rs
        // handles it by passing through unchanged (not entering catch).
        // try/catch only catches errors from exec_process_body, not from
        // individual process calls (which are handled gracefully).
        // This is correct: try/catch wraps a body, process errors within
        // that body are handled individually.
        let result = exec_process_body(&stmts, event, &FailRegistry, &make_funcs()).unwrap();
        assert!(matches!(result, ExecResult::Continue(_)));
    }

    // ---- let bindings --------------------------------------------------

    #[test]
    fn let_binding_resolves_via_bare_ident_in_same_body() {
        // `let x = 7; workspace.y = x` — workspace.y becomes Number(7).
        let event = make_event();
        let stmts = vec![
            ProcessStatement::LetBinding("x".into(), e(ExprKind::IntLit(7))),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["y".into()]),
                e(ExprKind::Ident(vec!["x".into()])),
            ),
        ];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["y"], Value::Number(7.into()));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_shadows_prior_binding_with_same_name() {
        // `let x = 1; let x = 2; workspace.y = x` — workspace.y is 2.
        let event = make_event();
        let stmts = vec![
            ProcessStatement::LetBinding("x".into(), e(ExprKind::IntLit(1))),
            ProcessStatement::LetBinding("x".into(), e(ExprKind::IntLit(2))),
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["y".into()]),
                e(ExprKind::Ident(vec!["x".into()])),
            ),
        ];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["y"], Value::Number(2.into()));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_is_visible_inside_if_branch_declared_above() {
        let event = make_event();
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
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["tag"], Value::String("hit".into()));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_scope_does_not_leak_between_top_level_bodies() {
        // `exec_process_body` starts a fresh scope each call. Running
        // two bodies back-to-back must not carry x from the first into
        // the second — referencing `x` in the second body fails.
        let funcs = make_funcs();
        let event = make_event();
        let first = vec![ProcessStatement::LetBinding(
            "x".into(),
            e(ExprKind::IntLit(1)),
        )];
        let _ = exec_process_body(&first, event.clone(), &NoopRegistry, &funcs).unwrap();

        let second = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["y".into()]),
            e(ExprKind::Ident(vec!["x".into()])),
        )];
        let err = exec_process_body(&second, event, &NoopRegistry, &funcs).unwrap_err();
        assert!(
            err.to_string().contains("unknown identifier"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn let_is_referenced_in_template_interpolation() {
        let event = make_event();
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
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(&*ev.egress, b"hello web01");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn let_does_not_survive_try_catch_failure() {
        // let bindings introduced inside a try that later fails are
        // discarded before the catch runs.
        let event = make_event();
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
        let err = exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap_err();
        assert!(
            err.to_string().contains("unknown identifier"),
            "expected catch to fail resolving x, got: {}",
            err
        );
    }
}
