//! Unit tests for the DSL process statement executor.

#[cfg(test)]
mod tests {
    use crate::dsl::value::{OwnedValue, Value};
    use bytes::Bytes;
    use std::net::SocketAddr;

    use crate::dsl::ast::*;
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
    fn test_exec_process_error_passes_through() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::ProcessCall("failing".into(), vec![])];
        match exec_process_body(&stmts, bevent, &FailRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                // Event should pass through unchanged
                assert_eq!(&*ev.ingress, b"<134>test");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_try_catch_on_error() {
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
        // Note: with FailRegistry, the process call returns Err but exec.rs
        // handles it by passing through unchanged (not entering catch).
        // try/catch only catches errors from exec_process_body, not from
        // individual process calls (which are handled gracefully).
        // This is correct: try/catch wraps a body, process errors within
        // that body are handled individually.
        let result =
            exec_process_body(&stmts, bevent, &FailRegistry, &make_funcs(), &arena).unwrap();
        assert!(matches!(result, ExecResult::Continue(_)));
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
        let event = make_event();
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
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["x"], Value::Int(7));
                assert_eq!(ev.workspace["y"], Value::Int(9));
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
        let event = make_event();
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
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace["miss"], Value::Null);
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
        let _ =
            exec_process_body(&first, event.view_in(&arena), &NoopRegistry, &funcs, &arena)
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
    // function registry dispatch for len / append / prepend / find_by).
    // ------------------------------------------------------------------------

    fn call_fn(name: &str, args: Vec<Expr>) -> Expr {
        e(ExprKind::FuncCall {
            namespace: None,
            name: name.into(),
            args,
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
    fn test_exec_find_by_over_literal_array_of_objects() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        // workspace.found = find_by([{t:"a", n:1}, {t:"b", n:2}], "t", "b")
        let event = make_event();
        let bevent = event.view_in(&arena);
        let obj = |t: &str, n: i64| {
            e(ExprKind::HashLit(vec![
                ("t".into(), e(ExprKind::StringLit(t.into()))),
                ("n".into(), e(ExprKind::IntLit(n))),
            ]))
        };
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Workspace(vec!["found".into()]),
            call_fn(
                "find_by",
                vec![
                    e(ExprKind::ArrayLit(vec![obj("a", 1), obj("b", 2)])),
                    e(ExprKind::StringLit("t".into())),
                    e(ExprKind::StringLit("b".into())),
                ],
            ),
        )];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                let found = ev.workspace_get("found").unwrap();
                assert_eq!(found.get("t"), Some(Value::String("b")));
                assert_eq!(found.get("n"), Some(Value::Int(2)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
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

    /// Concern 5 (normal exit): on natural ForEach termination the
    /// loop variable `workspace._item` is removed before the event
    /// continues. A downstream process must not be able to observe the
    /// last iteration's item via that magic key.
    #[test]
    fn foreach_clears_item_after_normal_exit() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::ForEach(
            e(ExprKind::ArrayLit(vec![
                e(ExprKind::IntLit(1)),
                e(ExprKind::IntLit(2)),
            ])),
            // Body: copy the current item into workspace.last so we
            // know the loop ran. The cleanup assertion below targets
            // _item, which is the implementation-defined loop key.
            vec![ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["last".into()]),
                e(ExprKind::Ident(vec!["workspace".into(), "_item".into()])),
            )],
        )];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("last"), Some(Value::Int(2)));
                assert!(
                    ev.workspace_get("_item").is_none(),
                    "_item must be cleared after the loop body completes"
                );
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    /// Concern 5 (drop path): when the ForEach body drops mid-iteration
    /// the loop variable cleanup is not run, but the entire event is
    /// discarded by the caller, so no observable leak escapes the
    /// pipeline. This test pins that drop wins over cleanup; if the
    /// implementation were ever changed to keep iterating after a drop
    /// the test would break.
    #[test]
    fn foreach_drop_short_circuits_iteration() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![ProcessStatement::ForEach(
            e(ExprKind::ArrayLit(vec![
                e(ExprKind::IntLit(1)),
                e(ExprKind::IntLit(2)),
                e(ExprKind::IntLit(3)),
            ])),
            // First iteration drops; later iterations must not run.
            vec![ProcessStatement::Drop],
        )];
        let result =
            exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap();
        assert!(matches!(result, ExecResult::Dropped));
    }

    /// Concern 5 (let persistence): a `let` declared inside the
    /// ForEach body persists across iterations because `exec.rs` runs
    /// each iteration with the same outer scope (no inner scope per
    /// iteration). The body sees the previous iteration's binding;
    /// rebinding via `let x = ...` shadows. This is consistent with
    /// the no-inner-scopes rule for `if` (concern 4) and is the
    /// intended behaviour, but worth pinning so a future refactor
    /// does not silently change it.
    #[test]
    fn let_inside_foreach_body_persists_across_iterations() {
        let _bump = ::bumpalo::Bump::new();
        let arena = crate::dsl::arena::EventArena::new(&_bump);
        let event = make_event();
        let bevent = event.view_in(&arena);
        let stmts = vec![
            // Initial sentinel — `acc` exists in scope before the loop.
            ProcessStatement::LetBinding("acc".into(), e(ExprKind::IntLit(0))),
            ProcessStatement::ForEach(
                e(ExprKind::ArrayLit(vec![
                    e(ExprKind::IntLit(1)),
                    e(ExprKind::IntLit(2)),
                    e(ExprKind::IntLit(3)),
                ])),
                vec![
                    // acc = acc + workspace._item — exercises both
                    // cross-iteration persistence and the bare-ident
                    // resolution into the let scope.
                    ProcessStatement::LetBinding(
                        "acc".into(),
                        e(ExprKind::BinOp(
                            Box::new(e(ExprKind::Ident(vec!["acc".into()]))),
                            BinOp::Add,
                            Box::new(e(ExprKind::Ident(vec!["workspace".into(), "_item".into()]))),
                        )),
                    ),
                ],
            ),
            // After the loop, `acc` should be 0 + 1 + 2 + 3 = 6.
            ProcessStatement::Assign(
                AssignTarget::Workspace(vec!["sum".into()]),
                e(ExprKind::Ident(vec!["acc".into()])),
            ),
        ];
        match exec_process_body(&stmts, bevent, &NoopRegistry, &make_funcs(), &arena).unwrap() {
            ExecResult::Continue(ev) => {
                assert_eq!(ev.workspace_get("sum"), Some(Value::Int(6)));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }
}
