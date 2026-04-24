//! Unit tests for the DSL expression evaluator.

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use serde_json::Value;
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
        e.workspace.insert("count".into(), Value::Number(42.into()));
        e.workspace.insert("sev".into(), Value::Number(3.into()));
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
            Value::Number(99.into())
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
            Value::Number(42.into())
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
        let expr = e(ExprKind::FuncCall {
            namespace: None,
            name: "to_json".into(),
            args: vec![],
        });
        let result = eval_expr(&expr, &ev, &f).unwrap();
        let s = result.as_str().unwrap();
        assert!(s.contains("\"workspace\""));
        assert!(s.contains("\"src\":\"192.168.1.1\""));
    }

    #[test]
    fn test_is_truthy() {
        assert!(!is_truthy(&Value::Null));
        assert!(!is_truthy(&Value::Bool(false)));
        assert!(is_truthy(&Value::Bool(true)));
        assert!(!is_truthy(&Value::String(String::new())));
        assert!(is_truthy(&Value::String("x".into())));
        assert!(!is_truthy(&Value::Number(0.into())));
        assert!(is_truthy(&Value::Number(1.into())));
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
        assert!(values_match(
            &Value::Number(42.into()),
            &Value::Number(42.into())
        ));
    }
}
