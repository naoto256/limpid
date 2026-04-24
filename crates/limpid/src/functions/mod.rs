//! Function registry: maps function names to implementations.
//!
//! All expression functions (built-in and future dynamic plugins) register
//! themselves here. The DSL evaluator resolves function calls through the
//! registry instead of hardcoded match arms.
//!
//! This is the extension point for future dynamic (.so) function loading.
//!
//! # Layout
//!
//! - [`primitives`] — flat-namespace, schema-agnostic functions
//!   (`contains`, `lower`, `regex_*`, `strftime`, `format`, `geoip`,
//!   `table_*`, `md5`/`sha1`/`sha256`, `to_json`). One file per
//!   function (or per tightly-related group) so `mod.rs` does not
//!   become a megafile.
//! - [`geoip`] / [`table`] — backing stores (DB reader, table store)
//!   used by the corresponding primitives. These are *not* the DSL
//!   registration; the registration lives in `primitives::geoip` /
//!   `primitives::table`.
//!
//! Schema-specific namespaces (`syslog.*`, `cef.*`, …) will live as
//! sibling modules to `primitives` once Block 4 introduces them.

pub mod cef;
pub mod geoip;
pub mod primitives;
pub mod syslog;
pub mod table;

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::event::Event;

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

type ExprFn = Box<dyn Fn(&[Value], &Event) -> Result<Value> + Send + Sync>;

/// Registry key: `(namespace, name)`. `namespace = None` is the flat
/// primitive namespace (`parse_json`, `regex_*`, `strftime`, ...).
/// `namespace = Some("syslog")` and friends are the dot-namespaced
/// form introduced in v0.3.0 Block 3.
type FnKey = (Option<String>, String);

pub struct FunctionRegistry {
    functions: HashMap<FnKey, ExprFn>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
        }
    }

    /// Register a flat primitive function (no namespace).
    /// Equivalent to `register_in(None, name, f)`, kept as the legacy API
    /// so the existing built-in registration path is untouched.
    pub fn register<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[Value], &Event) -> Result<Value> + Send + Sync + 'static,
    {
        self.functions.insert((None, name.to_string()), Box::new(f));
    }

    /// Register a namespaced function, callable as `<namespace>.<name>(...)`
    /// in the DSL. Block 3 introduced the dispatch path; Block 4
    /// populated the first real namespaces (`syslog`, `cef`). Future
    /// work will add `ocsf.*` composers.
    pub fn register_in<F>(&mut self, namespace: &str, name: &str, f: F)
    where
        F: Fn(&[Value], &Event) -> Result<Value> + Send + Sync + 'static,
    {
        self.functions
            .insert((Some(namespace.to_string()), name.to_string()), Box::new(f));
    }

    /// Dispatch a function call. `namespace = None` hits the flat
    /// primitive registry; `Some(ns)` hits the namespaced registry.
    /// Missing entries produce distinct error messages so users can tell
    /// "unknown namespace" from "unknown function in namespace".
    pub fn call(
        &self,
        namespace: Option<&str>,
        name: &str,
        args: &[Value],
        event: &Event,
    ) -> Result<Value> {
        let key = (namespace.map(str::to_string), name.to_string());
        let f = self.functions.get(&key).ok_or_else(|| match namespace {
            None => anyhow::anyhow!("unknown function: {}", name),
            Some(ns) => {
                if self.functions.keys().any(|(n, _)| n.as_deref() == Some(ns)) {
                    anyhow::anyhow!("unknown function '{}.{}' in namespace '{}'", ns, name, ns)
                } else {
                    anyhow::anyhow!("unknown function namespace: '{}'", ns)
                }
            }
        })?;
        f(args, event)
    }
}

// ---------------------------------------------------------------------------
// Built-in function registration
// ---------------------------------------------------------------------------

/// Register all built-in expression functions.
///
/// Currently delegates to [`primitives::register`]; once schema-specific
/// namespaces (`syslog.*`, `cef.*`, …) land, this is the single place
/// that wires each namespace into the registry.
pub fn register_builtins(reg: &mut FunctionRegistry, table_store: table::TableStore) {
    primitives::register(reg, table_store);
    syslog::register(reg);
    cef::register(reg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::table::TableStore;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn make_registry() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut reg, table_store);
        reg
    }

    fn dummy_event() -> Event {
        Event::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    #[test]
    fn strftime_formats_rfc3339_input() {
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+00:00".into()),
                    Value::String("%Y/%m/%d %H:%M:%S".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("2026/04/19 10:30:45".into()));
    }

    #[test]
    fn strftime_bsd_syslog_format() {
        // Reproduce the old `prepend_timestamp` default format.
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    Value::String("2026-04-19T05:07:09+00:00".into()),
                    Value::String("%b %e %H:%M:%S".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("Apr 19 05:07:09".into()));
    }

    #[test]
    fn strftime_utc_timezone() {
        let reg = make_registry();
        let e = dummy_event();
        // Input is +09:00; force to UTC.
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+09:00".into()),
                    Value::String("%Y-%m-%dT%H:%M:%S%z".into()),
                    Value::String("UTC".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("2026-04-19T01:30:45+0000".into()));
    }

    #[test]
    fn strftime_fixed_offset() {
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+00:00".into()),
                    Value::String("%H:%M".into()),
                    Value::String("+09:00".into()),
                ],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("19:30".into()));
    }

    #[test]
    fn strftime_rejects_invalid_rfc3339() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                None,
                "strftime",
                &[
                    Value::String("not-a-timestamp".into()),
                    Value::String("%Y".into()),
                ],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid RFC3339"));
    }

    #[test]
    fn strftime_rejects_bad_timezone() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                None,
                "strftime",
                &[
                    Value::String("2026-04-19T10:30:45+00:00".into()),
                    Value::String("%Y".into()),
                    Value::String("bogus".into()),
                ],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid timezone"));
    }

    #[test]
    fn strftime_rejects_wrong_arity() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                None,
                "strftime",
                &[Value::String("2026-04-19T10:30:45+00:00".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("2 or 3 arguments"));
    }

    // ---- format() ---------------------------------------------------------

    #[test]
    fn format_expands_event_level_placeholders() {
        let reg = make_registry();
        let mut e = dummy_event();
        e.severity = Some(3);
        e.facility = Some(16);
        let result = reg
            .call(
                None,
                "format",
                &[Value::String("[%{severity}] %{egress}".into())],
                &e,
            )
            .unwrap();
        // egress defaults to the raw bytes ("test") in dummy_event
        assert_eq!(result, Value::String("[3] test".into()));
    }

    #[test]
    fn format_expands_explicit_workspace_placeholder() {
        let reg = make_registry();
        let mut e = dummy_event();
        e.workspace
            .insert("host".into(), serde_json::Value::String("web01".into()));
        let result = reg
            .call(
                None,
                "format",
                &[Value::String("host=%{workspace.host}".into())],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("host=web01".into()));
    }

    #[test]
    fn format_rejects_bare_shorthand() {
        // `%{pid}` used to silently fall back to workspace.pid. Now it
        // must be an error so typos don't become empty strings.
        let reg = make_registry();
        let mut e = dummy_event();
        e.workspace
            .insert("pid".into(), serde_json::Value::String("42".into()));
        let err = reg
            .call(None, "format", &[Value::String("pid=%{pid}".into())], &e)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unknown placeholder"),
            "unexpected error: {}",
            msg
        );
        assert!(
            msg.contains("workspace.pid"),
            "error should suggest `workspace.pid`, got: {}",
            msg
        );
    }

    // ---- dot namespace dispatch (Block 3) -------------------------------

    #[test]
    fn register_in_and_dispatch_namespaced() {
        // Block 3 mechanism check: a fake namespace + function registered
        // via `register_in` should dispatch through `call(Some(ns), name)`.
        let mut reg = FunctionRegistry::new();
        reg.register_in("_test_block3", "passthrough", |args, _e| {
            Ok(args.first().cloned().unwrap_or(Value::Null))
        });
        let e = dummy_event();
        let result = reg
            .call(
                Some("_test_block3"),
                "passthrough",
                &[Value::String("hi".into())],
                &e,
            )
            .unwrap();
        assert_eq!(result, Value::String("hi".into()));
    }

    #[test]
    fn unknown_namespace_error_message() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(Some("not_a_real_namespace"), "parse", &[], &e)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown function namespace"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains("not_a_real_namespace"),
            "expected ns name in error: {err}"
        );
    }

    #[test]
    fn unknown_function_in_existing_namespace() {
        let mut reg = FunctionRegistry::new();
        reg.register_in("_test_block3", "known", |_a, _e| Ok(Value::Null));
        let e = dummy_event();
        let err = reg
            .call(Some("_test_block3"), "missing", &[], &e)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown function '_test_block3.missing'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn flat_primitive_regression_still_callable() {
        // Regression: after the registry became (namespace, name)-keyed,
        // existing flat primitives must still dispatch via `call(None, ...)`.
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(None, "lower", &[Value::String("HELLO".into())], &e)
            .unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn namespace_and_flat_do_not_collide() {
        // Same short name registered in two different namespaces should
        // remain independently addressable.
        let mut reg = FunctionRegistry::new();
        reg.register("ping", |_a, _e| Ok(Value::String("flat".into())));
        reg.register_in("_test_block3", "ping", |_a, _e| {
            Ok(Value::String("namespaced".into()))
        });
        let e = dummy_event();
        assert_eq!(
            reg.call(None, "ping", &[], &e).unwrap(),
            Value::String("flat".into())
        );
        assert_eq!(
            reg.call(Some("_test_block3"), "ping", &[], &e).unwrap(),
            Value::String("namespaced".into())
        );
    }

    #[test]
    fn format_error_suggests_explicit_form_for_typos() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                None,
                "format",
                &[Value::String("x=%{nope_not_a_thing}".into())],
                &e,
            )
            .unwrap_err();
        assert!(err.to_string().contains("workspace.nope_not_a_thing"));
    }
}
