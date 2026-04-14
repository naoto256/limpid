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
    use crate::modules::ProcessError;

    fn make_event() -> Event {
        let mut e = Event::new(
            Bytes::from("<134>test"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        e.severity = Some(5);
        e
    }

    fn make_funcs() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = crate::functions::table::TableStore::new();
        crate::functions::register_builtins(&mut reg, table_store);
        reg
    }

    /// No-op registry that passes events through unchanged.
    struct NoopRegistry;
    impl ProcessRegistry for NoopRegistry {
        fn call(&self, _name: &str, _args: &[Value], event: Event) -> Result<Option<Event>, ProcessError> {
            Ok(Some(event))
        }
    }

    /// Registry that always fails.
    struct FailRegistry;
    impl ProcessRegistry for FailRegistry {
        fn call(&self, _name: &str, _args: &[Value], _event: Event) -> Result<Option<Event>, ProcessError> {
            Err(ProcessError::Failed("test error".into()))
        }
    }

    #[test]
    fn test_exec_assign_severity() {
        let event = make_event();
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Severity,
            Expr::IntLit(3),
        )];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => assert_eq!(e.severity, Some(3)),
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_assign_field() {
        let event = make_event();
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Field(vec!["tag".into()]),
            Expr::StringLit("critical".into()),
        )];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                assert_eq!(e.fields["tag"], Value::String("critical".into()));
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
                Expr::BoolLit(true),
                vec![BranchBody::Process(ProcessStatement::Assign(
                    AssignTarget::Field(vec!["hit".into()]),
                    Expr::BoolLit(true),
                ))],
            )],
            else_body: None,
        })];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                assert_eq!(e.fields["hit"], Value::Bool(true));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_if_else_branch() {
        let event = make_event();
        let stmts = vec![ProcessStatement::If(IfChain {
            branches: vec![(
                Expr::BoolLit(false),
                vec![BranchBody::Process(ProcessStatement::Assign(
                    AssignTarget::Field(vec!["branch".into()]),
                    Expr::StringLit("if".into()),
                ))],
            )],
            else_body: Some(vec![BranchBody::Process(ProcessStatement::Assign(
                AssignTarget::Field(vec!["branch".into()]),
                Expr::StringLit("else".into()),
            ))]),
        })];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                assert_eq!(e.fields["branch"], Value::String("else".into()));
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_exec_process_error_passes_through() {
        let event = make_event();
        let stmts = vec![ProcessStatement::ProcessCall("failing".into(), vec![])];
        match exec_process_body(&stmts, event, &FailRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                // Event should pass through unchanged
                assert_eq!(e.severity, Some(5));
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
                AssignTarget::Field(vec!["caught".into()]),
                Expr::BoolLit(true),
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

    // --- PRI sync on facility/severity assignment ---

    #[test]
    fn test_facility_assign_rewrites_pri() {
        // <185> = facility 23, severity 1
        let mut event = Event::new(
            Bytes::from("<185>msg"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        event.facility = None;
        event.severity = None;

        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Facility,
            Expr::IntLit(16),
        )];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                assert_eq!(e.facility, Some(16));
                // new PRI = 16*8 + 1(old severity) = 129
                assert_eq!(&*e.message, b"<129>msg");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_severity_assign_rewrites_pri() {
        // <185> = facility 23, severity 1
        let mut event = Event::new(
            Bytes::from("<185>msg"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        event.facility = None;
        event.severity = None;

        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Severity,
            Expr::IntLit(6),
        )];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                assert_eq!(e.severity, Some(6));
                // new PRI = 23(old facility)*8 + 6 = 190
                assert_eq!(&*e.message, b"<190>msg");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_facility_and_severity_assign() {
        // <185> = facility 23, severity 1
        let mut event = Event::new(
            Bytes::from("<185>msg"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        event.facility = None;
        event.severity = None;

        let stmts = vec![
            ProcessStatement::Assign(AssignTarget::Facility, Expr::IntLit(16)),
            ProcessStatement::Assign(AssignTarget::Severity, Expr::IntLit(6)),
        ];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                // 16*8 + 6 = 134
                assert_eq!(&*e.message, b"<134>msg");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }

    #[test]
    fn test_pri_rewrite_no_op_without_pri() {
        // Message without PRI — assignment should not add one
        let event = Event::new(
            Bytes::from("no-pri-msg"),
            "10.0.0.1:514".parse::<SocketAddr>().unwrap(),
        );
        let stmts = vec![ProcessStatement::Assign(
            AssignTarget::Facility,
            Expr::IntLit(16),
        )];
        match exec_process_body(&stmts, event, &NoopRegistry, &make_funcs()).unwrap() {
            ExecResult::Continue(e) => {
                assert_eq!(e.facility, Some(16));
                assert_eq!(&*e.message, b"no-pri-msg");
            }
            ExecResult::Dropped => panic!("unexpected drop"),
        }
    }
}
