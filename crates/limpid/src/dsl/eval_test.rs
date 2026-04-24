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
        e.severity = Some(3);
        e.facility = Some(16);
        e.workspace
            .insert("src".into(), Value::String("192.168.1.1".into()));
        e.workspace.insert("count".into(), Value::Number(42.into()));
        e
    }

    fn make_funcs() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = crate::functions::table::TableStore::from_configs(vec![]).unwrap();
        crate::functions::register_builtins(&mut reg, table_store);
        reg
    }

    #[test]
    fn test_eval_literals() {
        let e = make_event();
        let f = make_funcs();
        assert_eq!(
            eval_expr(&Expr::StringLit("hello".into()), &e, &f).unwrap(),
            Value::String("hello".into())
        );
        assert_eq!(
            eval_expr(&Expr::IntLit(99), &e, &f).unwrap(),
            Value::Number(99.into())
        );
        assert_eq!(
            eval_expr(&Expr::BoolLit(true), &e, &f).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(eval_expr(&Expr::Null, &e, &f).unwrap(), Value::Null);
    }

    #[test]
    fn test_eval_ident_workspace() {
        let e = make_event();
        let f = make_funcs();
        assert_eq!(
            eval_expr(&Expr::Ident(vec!["severity".into()]), &e, &f).unwrap(),
            Value::Number(3.into())
        );
        assert_eq!(
            eval_expr(&Expr::Ident(vec!["facility".into()]), &e, &f).unwrap(),
            Value::Number(16.into())
        );
        assert_eq!(
            eval_expr(&Expr::Ident(vec!["workspace".into(), "src".into()]), &e, &f).unwrap(),
            Value::String("192.168.1.1".into())
        );
    }

    #[test]
    fn test_eval_unknown_ident_errors() {
        let e = make_event();
        let f = make_funcs();
        assert!(eval_expr(&Expr::Ident(vec!["typo_field".into()]), &e, &f).is_err());
    }

    #[test]
    fn test_eval_binop_comparison() {
        let e = make_event();
        let f = make_funcs();
        // severity (3) <= 3 → true
        let expr = Expr::BinOp(
            Box::new(Expr::Ident(vec!["severity".into()])),
            BinOp::Le,
            Box::new(Expr::IntLit(3)),
        );
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Bool(true));

        // severity (3) > 5 → false
        let expr = Expr::BinOp(
            Box::new(Expr::Ident(vec!["severity".into()])),
            BinOp::Gt,
            Box::new(Expr::IntLit(5)),
        );
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_eval_add_string_concat() {
        let e = make_event();
        let f = make_funcs();

        // String + String → concat
        let expr = Expr::BinOp(
            Box::new(Expr::StringLit("hello ".into())),
            BinOp::Add,
            Box::new(Expr::StringLit("world".into())),
        );
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("hello world".into())
        );

        // Mixed String + Number → both coerced to string
        let expr = Expr::BinOp(
            Box::new(Expr::StringLit("count=".into())),
            BinOp::Add,
            Box::new(Expr::IntLit(42)),
        );
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("count=42".into())
        );

        // Number + String → same
        let expr = Expr::BinOp(
            Box::new(Expr::IntLit(42)),
            BinOp::Add,
            Box::new(Expr::StringLit(" ms".into())),
        );
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("42 ms".into())
        );

        // Number + Number still numeric (no regression). numeric_op uses f64
        // internally, so the result is Number(7.0), not Number(7).
        let expr = Expr::BinOp(
            Box::new(Expr::IntLit(3)),
            BinOp::Add,
            Box::new(Expr::IntLit(4)),
        );
        let result = eval_expr(&expr, &e, &f).unwrap();
        assert_eq!(result.as_f64(), Some(7.0));

        // Chained: "a" + "b" + "c" (left-associative)
        let expr = Expr::BinOp(
            Box::new(Expr::BinOp(
                Box::new(Expr::StringLit("a".into())),
                BinOp::Add,
                Box::new(Expr::StringLit("b".into())),
            )),
            BinOp::Add,
            Box::new(Expr::StringLit("c".into())),
        );
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("abc".into())
        );
    }

    #[test]
    fn test_eval_binop_logical() {
        let e = make_event();
        let f = make_funcs();
        // true and false → false
        let expr = Expr::BinOp(
            Box::new(Expr::BoolLit(true)),
            BinOp::And,
            Box::new(Expr::BoolLit(false)),
        );
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Bool(false));

        // true or false → true
        let expr = Expr::BinOp(
            Box::new(Expr::BoolLit(true)),
            BinOp::Or,
            Box::new(Expr::BoolLit(false)),
        );
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_not() {
        let e = make_event();
        let f = make_funcs();
        let expr = Expr::UnaryOp(UnaryOp::Not, Box::new(Expr::BoolLit(true)));
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_eval_contains() {
        let e = make_event();
        let f = make_funcs();
        let expr = Expr::FuncCall {
            namespace: None,
            name: "contains".into(),
            args: vec![
                Expr::Ident(vec!["ingress".into()]),
                Expr::StringLit("test".into()),
            ],
        };
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Bool(true));
    }

    #[test]
    fn test_eval_template() {
        let e = make_event();
        let f = make_funcs();
        // "[${severity}] from ${workspace.src}"
        let expr = Expr::Template(vec![
            TemplateFragment::Literal("[".into()),
            TemplateFragment::Interp(Expr::Ident(vec!["severity".into()])),
            TemplateFragment::Literal("] from ".into()),
            TemplateFragment::Interp(Expr::Ident(vec!["workspace".into(), "src".into()])),
        ]);
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("[3] from 192.168.1.1".into())
        );
    }

    #[test]
    fn test_eval_template_missing_interp_empty() {
        let e = make_event();
        let f = make_funcs();
        let expr = Expr::Template(vec![
            TemplateFragment::Literal("prefix-".into()),
            TemplateFragment::Interp(Expr::Ident(vec!["workspace".into(), "missing".into()])),
            TemplateFragment::Literal("-suffix".into()),
        ]);
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("prefix--suffix".into())
        );
    }

    #[test]
    fn test_eval_lower_upper() {
        let e = make_event();
        let f = make_funcs();
        let lower = Expr::FuncCall {
            namespace: None,
            name: "lower".into(),
            args: vec![Expr::StringLit("HELLO".into())],
        };
        assert_eq!(
            eval_expr(&lower, &e, &f).unwrap(),
            Value::String("hello".into())
        );

        let upper = Expr::FuncCall {
            namespace: None,
            name: "upper".into(),
            args: vec![Expr::StringLit("hello".into())],
        };
        assert_eq!(
            eval_expr(&upper, &e, &f).unwrap(),
            Value::String("HELLO".into())
        );
    }

    #[test]
    fn test_eval_to_json() {
        let e = make_event();
        let f = make_funcs();
        let expr = Expr::FuncCall {
            namespace: None,
            name: "to_json".into(),
            args: vec![],
        };
        let result = eval_expr(&expr, &e, &f).unwrap();
        let s = result.as_str().unwrap();
        assert!(s.contains("\"severity\":3"));
        assert!(s.contains("\"facility\":16"));
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
        let e = make_event();
        let f = make_funcs();
        // "hello" < "world" should be false (non-numeric)
        let expr = Expr::BinOp(
            Box::new(Expr::StringLit("hello".into())),
            BinOp::Lt,
            Box::new(Expr::StringLit("world".into())),
        );
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Bool(false));
    }

    #[test]
    fn test_property_access_on_hash() {
        let e = make_event();
        let f = make_funcs();
        // { country: "JP", city: "Tokyo" }.country → "JP"
        let hash = Expr::HashLit(vec![
            ("country".into(), Expr::StringLit("JP".into())),
            ("city".into(), Expr::StringLit("Tokyo".into())),
        ]);
        let expr = Expr::PropertyAccess(Box::new(hash), vec!["country".into()]);
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("JP".into())
        );
    }

    #[test]
    fn test_property_access_chained() {
        let e = make_event();
        let f = make_funcs();
        // { geo: { country: "JP" } }.geo.country → "JP"
        let inner_hash = Expr::HashLit(vec![("country".into(), Expr::StringLit("JP".into()))]);
        let outer_hash = Expr::HashLit(vec![("geo".into(), inner_hash)]);
        let expr = Expr::PropertyAccess(Box::new(outer_hash), vec!["geo".into(), "country".into()]);
        assert_eq!(
            eval_expr(&expr, &e, &f).unwrap(),
            Value::String("JP".into())
        );
    }

    #[test]
    fn test_property_access_missing_field() {
        let e = make_event();
        let f = make_funcs();
        let hash = Expr::HashLit(vec![("country".into(), Expr::StringLit("JP".into()))]);
        let expr = Expr::PropertyAccess(Box::new(hash), vec!["missing".into()]);
        assert_eq!(eval_expr(&expr, &e, &f).unwrap(), Value::Null);
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
