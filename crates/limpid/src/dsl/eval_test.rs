//! Unit tests for the DSL expression evaluator.

#[cfg(test)]
mod tests {
    use crate::dsl::value::Value;
    use bytes::Bytes;
    use std::net::SocketAddr;

    use crate::dsl::ast::*;
    use crate::dsl::eval::*;
    use crate::event::Event;
    use crate::functions::FunctionRegistry;

    fn make_event() -> Event {
        let mut e = Event::new(
            Bytes::from("<134>test message"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        e.workspace
            .insert("src".into(), Value::String("192.168.1.1".into()));
        e.workspace.insert("count".into(), Value::Int(42));
        e.workspace.insert("sev".into(), Value::Int(3));
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
        let ev = make_event();
        let f = make_funcs();
        assert_eq!(
            eval_expr(&e(ExprKind::StringLit("hello".into())), &ev, &f).unwrap(),
            Value::String("hello".into())
        );
        assert_eq!(
            eval_expr(&e(ExprKind::IntLit(99)), &ev, &f).unwrap(),
            Value::Int(99)
        );
        assert_eq!(
            eval_expr(&e(ExprKind::BoolLit(true)), &ev, &f).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(eval_expr(&e(ExprKind::Null), &ev, &f).unwrap(), Value::Null);
    }

    #[test]
    fn test_eval_ident_workspace() {
        let ev = make_event();
        let f = make_funcs();
        assert_eq!(
            eval_expr(
                &e(ExprKind::Ident(vec!["workspace".into(), "src".into()])),
                &ev,
                &f
            )
            .unwrap(),
            Value::String("192.168.1.1".into())
        );
        assert_eq!(
            eval_expr(
                &e(ExprKind::Ident(vec!["workspace".into(), "count".into()])),
                &ev,
                &f
            )
            .unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn test_eval_unknown_ident_errors() {
        let ev = make_event();
        let f = make_funcs();
        assert!(eval_expr(&e(ExprKind::Ident(vec!["typo_field".into()])), &ev, &f).is_err());
    }

    #[test]
    fn test_eval_binop_comparison() {
        let ev = make_event();
        let f = make_funcs();
        // workspace.sev (3) <= 3 → true
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::Ident(vec!["workspace".into(), "sev".into()]))),
            BinOp::Le,
            Box::new(e(ExprKind::IntLit(3))),
        ));
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Bool(true));

        // workspace.sev (3) > 5 → false
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::Ident(vec!["workspace".into(), "sev".into()]))),
            BinOp::Gt,
            Box::new(e(ExprKind::IntLit(5))),
        ));
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_eval_add_string_concat() {
        let ev = make_event();
        let f = make_funcs();

        // String + String → concat
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::StringLit("hello ".into()))),
            BinOp::Add,
            Box::new(e(ExprKind::StringLit("world".into()))),
        ));
        assert_eq!(
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("hello world".into())
        );

        // Mixed String + Number → both coerced to string
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::StringLit("count=".into()))),
            BinOp::Add,
            Box::new(e(ExprKind::IntLit(42))),
        ));
        assert_eq!(
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("count=42".into())
        );

        // Number + String → same
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::IntLit(42))),
            BinOp::Add,
            Box::new(e(ExprKind::StringLit(" ms".into()))),
        ));
        assert_eq!(
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("42 ms".into())
        );

        // Number + Number still numeric (no regression). numeric_op uses f64
        // internally, so the result is Number(7.0), not Number(7).
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::IntLit(3))),
            BinOp::Add,
            Box::new(e(ExprKind::IntLit(4))),
        ));
        let result = eval_expr(&expr, &ev, &f).unwrap();
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
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("abc".into())
        );
    }

    #[test]
    fn test_eval_binop_logical() {
        let ev = make_event();
        let f = make_funcs();
        // true and false → false
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::BoolLit(true))),
            BinOp::And,
            Box::new(e(ExprKind::BoolLit(false))),
        ));
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Bool(false));

        // true or false → true
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::BoolLit(true))),
            BinOp::Or,
            Box::new(e(ExprKind::BoolLit(false))),
        ));
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_not() {
        let ev = make_event();
        let f = make_funcs();
        let expr = e(ExprKind::UnaryOp(
            UnaryOp::Not,
            Box::new(e(ExprKind::BoolLit(true))),
        ));
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_eval_contains() {
        let ev = make_event();
        let f = make_funcs();
        let expr = e(ExprKind::FuncCall {
            namespace: None,
            name: "contains".into(),
            args: vec![
                e(ExprKind::Ident(vec!["ingress".into()])),
                e(ExprKind::StringLit("test".into())),
            ],
        });
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_template() {
        let ev = make_event();
        let f = make_funcs();
        // "[${workspace.sev}] from ${workspace.src}"
        let expr = e(ExprKind::Template(vec![
            TemplateFragment::Literal("[".into()),
            TemplateFragment::Interp(e(ExprKind::Ident(vec!["workspace".into(), "sev".into()]))),
            TemplateFragment::Literal("] from ".into()),
            TemplateFragment::Interp(e(ExprKind::Ident(vec!["workspace".into(), "src".into()]))),
        ]));
        assert_eq!(
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("[3] from 192.168.1.1".into())
        );
    }

    #[test]
    fn test_eval_template_missing_interp_empty() {
        let ev = make_event();
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
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("prefix--suffix".into())
        );
    }

    #[test]
    fn test_eval_lower_upper() {
        let ev = make_event();
        let f = make_funcs();
        let lower = e(ExprKind::FuncCall {
            namespace: None,
            name: "lower".into(),
            args: vec![e(ExprKind::StringLit("HELLO".into()))],
        });
        assert_eq!(
            eval_expr(&lower, &ev, &f).unwrap(),
            Value::String("hello".into())
        );

        let upper = e(ExprKind::FuncCall {
            namespace: None,
            name: "upper".into(),
            args: vec![e(ExprKind::StringLit("hello".into()))],
        });
        assert_eq!(
            eval_expr(&upper, &ev, &f).unwrap(),
            Value::String("HELLO".into())
        );
    }

    #[test]
    fn test_eval_to_json() {
        let ev = make_event();
        let f = make_funcs();
        // 0.5.0+: to_json requires an explicit value. Pass `workspace` to
        // serialise the workspace map (the most common operator pattern).
        let expr = e(ExprKind::FuncCall {
            namespace: None,
            name: "to_json".into(),
            args: vec![e(ExprKind::Ident(vec!["workspace".into()]))],
        });
        let result = eval_expr(&expr, &ev, &f).unwrap();
        let s = result.as_str().unwrap();
        assert!(s.contains("\"src\":\"192.168.1.1\""));
    }

    #[test]
    fn test_is_truthy() {
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&Value::Bool(false)));
        assert!(is_truthy(&Value::Bool(true)));
        assert!(!is_truthy(&Value::String(String::new())));
        assert!(is_truthy(&Value::String("x".into())));
        assert!(!is_truthy(&Value::Int(0)));
        assert!(is_truthy(&Value::Int(1)));
    }

    #[test]
    fn test_non_numeric_comparison_returns_false() {
        let ev = make_event();
        let f = make_funcs();
        // "hello" < "world" should be false (non-numeric)
        let expr = e(ExprKind::BinOp(
            Box::new(e(ExprKind::StringLit("hello".into()))),
            BinOp::Lt,
            Box::new(e(ExprKind::StringLit("world".into()))),
        ));
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_property_access_on_hash() {
        let ev = make_event();
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
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("JP".into())
        );
    }

    #[test]
    fn test_property_access_chained() {
        let ev = make_event();
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
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("JP".into())
        );
    }

    #[test]
    fn test_property_access_missing_field() {
        let ev = make_event();
        let f = make_funcs();
        let hash = e(ExprKind::HashLit(vec![(
            "country".into(),
            e(ExprKind::StringLit("JP".into())),
        )]));
        let expr = e(ExprKind::PropertyAccess(
            Box::new(hash),
            vec!["missing".into()],
        ));
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Null);
    }

    #[test]
    fn test_values_match_fn() {
        assert!(values_match(
            &Value::String("a".into()),
            &Value::String("a".into())
        ));
        assert!(!values_match(
            &Value::String("a".into()),
            &Value::String("b".into())
        ));
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
        let ev = make_event();
        let f = make_funcs();
        assert_eq!(
            eval_expr(&e(ExprKind::ArrayLit(vec![])), &ev, &f).unwrap(),
            Value::Array(vec![])
        );
    }

    #[test]
    fn test_eval_array_literal_scalars() {
        let ev = make_event();
        let f = make_funcs();
        let expr = e(ExprKind::ArrayLit(vec![
            e(ExprKind::IntLit(1)),
            e(ExprKind::IntLit(2)),
            e(ExprKind::IntLit(3)),
        ]));
        assert_eq!(
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::Array(vec![Value::Int(1), Value::Int(2), Value::Int(3),])
        );
    }

    #[test]
    fn test_eval_array_literal_mixed_types() {
        let ev = make_event();
        let f = make_funcs();
        let expr = e(ExprKind::ArrayLit(vec![
            e(ExprKind::IntLit(1)),
            e(ExprKind::StringLit("two".into())),
            e(ExprKind::BoolLit(true)),
            e(ExprKind::Null),
        ]));
        assert_eq!(
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::Array(vec![
                Value::Int(1),
                Value::String("two".into()),
                Value::Bool(true),
                Value::Null,
            ])
        );
    }

    #[test]
    fn test_eval_array_literal_resolves_workspace_refs() {
        let ev = make_event();
        let f = make_funcs();
        let expr = e(ExprKind::ArrayLit(vec![
            e(ExprKind::Ident(vec!["workspace".into(), "src".into()])),
            e(ExprKind::Ident(vec!["workspace".into(), "count".into()])),
        ]));
        assert_eq!(
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::Array(vec![Value::String("192.168.1.1".into()), Value::Int(42),])
        );
    }

    #[test]
    fn test_eval_array_literal_nested() {
        let ev = make_event();
        let f = make_funcs();
        let row = |a, b| {
            e(ExprKind::ArrayLit(vec![
                e(ExprKind::IntLit(a)),
                e(ExprKind::IntLit(b)),
            ]))
        };
        let grid = e(ExprKind::ArrayLit(vec![row(1, 2), row(3, 4)]));
        assert_eq!(
            eval_expr(&grid, &ev, &f).unwrap(),
            Value::Array(vec![
                Value::Array(vec![Value::Int(1), Value::Int(2)]),
                Value::Array(vec![Value::Int(3), Value::Int(4)]),
            ])
        );
    }

    #[test]
    fn test_eval_array_inside_hash_literal() {
        let ev = make_event();
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
        let out = eval_expr(&expr, &ev, &f).unwrap();
        let obj = out.as_object().unwrap();
        assert_eq!(
            obj.get("types").unwrap(),
            &Value::Array(vec![
                Value::String("sqli".into()),
                Value::String("xss".into())
            ])
        );
    }

    // ---- SwitchExpr -------------------------------------------------------

    #[test]
    fn switch_expr_picks_matching_arm() {
        let ev = make_event();
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
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("tcp".into())
        );
    }

    #[test]
    fn switch_expr_falls_to_default() {
        let ev = make_event();
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
            eval_expr(&expr, &ev, &f).unwrap(),
            Value::String("unknown".into())
        );
    }

    #[test]
    fn switch_expr_no_match_no_default_returns_null() {
        let ev = make_event();
        let f = make_funcs();
        let expr = e(ExprKind::SwitchExpr {
            scrutinee: Box::new(e(ExprKind::IntLit(99))),
            arms: vec![crate::dsl::ast::SwitchExprArm {
                pattern: Some(e(ExprKind::IntLit(6))),
                body: e(ExprKind::StringLit("tcp".into())),
            }],
        });
        assert_eq!(eval_expr(&expr, &ev, &f).unwrap(), Value::Null);
    }

    // ---- User-defined `def function` end-to-end --------------------------

    #[test]
    fn user_function_call_returns_body_value() {
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
        let result = funcs.call(None, "double", &[Value::Int(21)], &ev).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn user_function_arity_mismatch_at_call() {
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
        // The dispatch path is responsible for the central arity
        // check (via the synthesized `Any^2 -> Any` signature). Pass
        // 1 arg and expect a clear error.
        let err = funcs
            .call(None, "needs_two", &[Value::Int(1)], &ev)
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
        assert_eq!(
            funcs
                .call(None, "normalize_proto", &[Value::Int(6)], &ev)
                .unwrap(),
            Value::String("tcp".into())
        );
        assert_eq!(
            funcs
                .call(None, "normalize_proto", &[Value::Int(17)], &ev)
                .unwrap(),
            Value::String("udp".into())
        );
        assert_eq!(
            funcs
                .call(None, "normalize_proto", &[Value::Int(99)], &ev)
                .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn user_function_calling_user_function_works() {
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
                    })],
                }),
            },
        });

        let ev = make_event();
        assert_eq!(
            funcs
                .call(None, "quadruple", &[Value::Int(5)], &ev)
                .unwrap(),
            Value::Int(20)
        );
    }

    #[test]
    fn user_function_with_let_bindings() {
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
        let result = funcs.call(None, "f", &[Value::Int(10)], &ev).unwrap();
        assert_eq!(result, Value::Int(21)); // 10 * 2 + 1
    }

    #[test]
    fn user_function_let_reassignment_overwrites() {
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
        let result = funcs.call(None, "f", &[Value::Int(7)], &ev).unwrap();
        assert_eq!(result, Value::Int(21));
    }
}
