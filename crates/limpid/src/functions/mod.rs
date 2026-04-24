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
//!   `table_*`, `md5`/`sha1`/`sha256`, `to_json`, `to_int`, `find_by`,
//!   `csv_parse`, `len`, `append`, `prepend`). One file per function
//!   (or per tightly-related group such as `hashes.rs`) so `mod.rs`
//!   does not become a megafile.
//! - [`syslog`] / [`cef`] — schema-specific namespaces (`syslog.*`,
//!   `cef.*`). See Design Principle 5 in `design-principles.md`;
//!   future schema namespaces (`ocsf.*` composers, etc.) follow the
//!   same layout.
//! - [`geoip`] / [`table`] — backing stores (DB reader, table store)
//!   used by the corresponding primitives. These are *not* the DSL
//!   registration; the registration lives in `primitives::geoip` /
//!   `primitives::table`.

pub mod cef;
pub mod geoip;
pub mod primitives;
pub mod syslog;
pub mod table;

use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::event::Event;
use crate::modules::schema::{FieldSpec, FieldType};

// ---------------------------------------------------------------------------
// Function signatures
// ---------------------------------------------------------------------------

/// Argument arity shape for a built-in function.
///
/// Functions are registered with a small, declarative signature so the
/// analyzer can type-check call sites without re-implementing every
/// function's hand-rolled arity logic.
///
/// A `Variadic` variant used to live here but was never populated by
/// any built-in; it is deliberately omitted until a variadic function
/// is actually needed, at which point reintroducing the arm is a
/// non-breaking enum extension.
#[derive(Debug, Clone)]
pub enum Arity {
    /// Exactly `args.len()` positional args, all required. The signature's
    /// `args` slice length is the arity.
    Fixed,
    /// First `required` args mandatory; trailing args optional. The
    /// signature's `args` slice declares types for every slot up to the
    /// maximum, including optional ones.
    Optional { required: usize },
}

/// Static signature for a built-in function. Threaded into the registry
/// at registration time (alongside the implementation closure) so the
/// analyzer can pull it back via [`FunctionRegistry::signature`] during
/// type checking. Functions registered without a sig are treated as
/// `Any -> Any` (no type checking; no false positives).
#[derive(Debug, Clone)]
pub struct FunctionSig {
    pub args: Vec<FieldType>,
    pub arity: Arity,
    pub ret: FieldType,
}

impl FunctionSig {
    /// Convenience: fixed positional signature `(arg1, arg2, ...) -> ret`.
    pub fn fixed(args: &[FieldType], ret: FieldType) -> Self {
        Self {
            args: args.to_vec(),
            arity: Arity::Fixed,
            ret,
        }
    }

    /// Convenience: `(required..optional) -> ret`. `args.len()` is the
    /// maximum, `required` the minimum.
    pub fn optional(args: &[FieldType], required: usize, ret: FieldType) -> Self {
        Self {
            args: args.to_vec(),
            arity: Arity::Optional { required },
            ret,
        }
    }
}

// ---------------------------------------------------------------------------
// Parser trait
// ---------------------------------------------------------------------------

/// Static metadata about a parser-style function (one that returns a
/// `Value::Object` whose keys merge into `event.workspace`).
///
/// Parsers carry richer schema than ordinary functions: the analyzer
/// needs to know *which keys* a bare `parse_xxx(text)` call contributes
/// to workspace, with *what types*, so downstream `workspace.*` references
/// type-check. `produces` is the static answer; `wildcards = true` means
/// the key set is data-driven (e.g. `parse_json`) and the analyzer should
/// fall back to wildcard unless the caller pins a schema via the
/// optional defaults HashLit argument.
///
/// Parsers are registered alongside their implementation; the analyzer
/// looks them up via [`FunctionRegistry::parser`].
#[derive(Debug, Clone)]
pub struct ParserInfo {
    pub namespace: Option<&'static str>,
    pub name: &'static str,
    /// `(workspace key, type)` pairs the parser is known to emit. Empty
    /// when `wildcards = true` and there's no static structure.
    pub produces: Vec<FieldSpec>,
    /// True when the output key set is determined by the input rather
    /// than statically known (parse_json, parse_kv, parse_cef
    /// extensions, etc.).
    pub wildcards: bool,
}

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
    /// Optional signature alongside the function implementation. Populated
    /// by `register_with_sig` / `register_in_with_sig`. Functions without
    /// a signature here are treated as `Any -> Any` by the analyzer.
    signatures: HashMap<FnKey, FunctionSig>,
    /// Parser metadata for parser-style functions. Distinct from
    /// `signatures` because parsers carry workspace-effect schema in
    /// addition to argument types.
    parsers: HashMap<FnKey, ParserInfo>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
            signatures: HashMap::new(),
            parsers: HashMap::new(),
        }
    }

    /// Register a flat primitive function without a stand-alone signature.
    ///
    /// Used by parser-style primitives (`parse_json`, `parse_kv`) that
    /// pair this with `register_parser` — the parser registration
    /// installs the `(String, Object?) -> Object` sig, so supplying one
    /// here would be redundant. New primitives with fixed shapes should
    /// prefer [`register_with_sig`](Self::register_with_sig) so the
    /// central arity / arg-type check in [`call`](Self::call) kicks in.
    pub fn register<F>(&mut self, name: &str, f: F)
    where
        F: Fn(&[Value], &Event) -> Result<Value> + Send + Sync + 'static,
    {
        self.functions.insert((None, name.to_string()), Box::new(f));
    }

    /// Register a flat primitive function together with its static
    /// signature. The analyzer consults the signature for arg / return
    /// type checking; the implementation closure is identical to the
    /// no-sig form.
    pub fn register_with_sig<F>(&mut self, name: &str, sig: FunctionSig, f: F)
    where
        F: Fn(&[Value], &Event) -> Result<Value> + Send + Sync + 'static,
    {
        let key = (None, name.to_string());
        self.functions.insert(key.clone(), Box::new(f));
        self.signatures.insert(key, sig);
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

    /// Sig-aware namespaced registration. Mirrors `register_with_sig`
    /// but for `ns.fn(...)` dispatch. All `syslog.*` / `cef.*` registrations
    /// that have a fixed shape go through here so the registry's arity
    /// check fires uniformly.
    pub fn register_in_with_sig<F>(&mut self, namespace: &str, name: &str, sig: FunctionSig, f: F)
    where
        F: Fn(&[Value], &Event) -> Result<Value> + Send + Sync + 'static,
    {
        let key = (Some(namespace.to_string()), name.to_string());
        self.functions.insert(key.clone(), Box::new(f));
        self.signatures.insert(key, sig);
    }

    /// Register parser metadata. Call after `register_*` so the
    /// analyzer can find both the function and its schema.
    ///
    /// If the caller has **not** already installed a `FunctionSig` via
    /// `register_with_sig` / `register_in_with_sig`, this installs the
    /// default parser shape `(String, Object?) -> Object` — the one
    /// shared by `parse_json` / `parse_kv` / `syslog.parse` / `cef.parse`.
    /// Parsers with a different call shape (e.g. `regex_parse(target,
    /// pattern)` which takes `(String, String)`) should register their
    /// own sig first and rely on this method's `entry().or_insert_with`
    /// guard to keep it intact.
    pub fn register_parser(&mut self, info: ParserInfo) {
        let key = (info.namespace.map(str::to_string), info.name.to_string());
        self.signatures.entry(key.clone()).or_insert_with(|| {
            FunctionSig::optional(
                &[FieldType::String, FieldType::Object],
                1,
                FieldType::Object,
            )
        });
        self.parsers.insert(key, info);
    }

    /// Iterate every flat (un-namespaced) function name registered.
    /// Used by the analyzer's suggestion engine for typo recovery; the
    /// namespaced functions aren't included because those typos require
    /// `ns.name`-aware matching that the suggester doesn't model yet.
    pub fn flat_function_names(&self) -> impl Iterator<Item = String> + '_ {
        self.functions.keys().filter_map(|(ns, name)| {
            if ns.is_none() {
                Some(name.clone())
            } else {
                None
            }
        })
    }

    /// Look up the static signature for a function. Returns `None` if
    /// the function was registered without one (analyzer treats those
    /// as fully wildcarded — no type checking).
    pub fn signature(&self, namespace: Option<&str>, name: &str) -> Option<&FunctionSig> {
        let key = (namespace.map(str::to_string), name.to_string());
        self.signatures.get(&key)
    }

    /// Look up parser metadata. `Some` only for parser-style functions
    /// (parse_json / parse_kv / cef.parse / syslog.parse etc.).
    pub fn parser(&self, namespace: Option<&str>, name: &str) -> Option<&ParserInfo> {
        let key = (namespace.map(str::to_string), name.to_string());
        self.parsers.get(&key)
    }

    /// Iterate over every registered parser. Used by the analyzer to
    /// drive the `process_expr_stmt` merge rule for bare `parse_*(text)`
    /// statements.
    #[allow(dead_code)] // exposed for future `--check --list-parsers` and Phase 3 UX
    pub fn parsers(&self) -> impl Iterator<Item = &ParserInfo> {
        self.parsers.values()
    }

    /// Dispatch a function call. `namespace = None` hits the flat
    /// primitive registry; `Some(ns)` hits the namespaced registry.
    /// Missing entries produce distinct error messages so users can tell
    /// "unknown namespace" from "unknown function in namespace".
    ///
    /// Arity validation runs here (single source of truth) for every
    /// function that has a registered [`FunctionSig`]. Functions without
    /// a sig — only `parse_json` / `parse_kv` still go through `register`
    /// today, and `register_parser` supplies a sig for them — skip the
    /// check and keep their historical hand-rolled arity guards.
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
        if let Some(sig) = self.signatures.get(&key) {
            validate_arity(namespace, name, sig, args.len())?;
        }
        f(args, event)
    }
}

/// Central arity check, shared by every function that has a `FunctionSig`.
///
/// Pulled out of `call` so individual primitives don't repeat the check —
/// 20+ hand-rolled `if args.len() != N { bail!(...) }` blocks used to sit
/// in each closure; the sig is the single source of truth now.
fn validate_arity(
    namespace: Option<&str>,
    name: &str,
    sig: &FunctionSig,
    actual: usize,
) -> Result<()> {
    let ok = match sig.arity {
        Arity::Fixed => actual == sig.args.len(),
        Arity::Optional { required } => actual >= required && actual <= sig.args.len(),
    };
    if ok {
        return Ok(());
    }
    let prefix = match namespace {
        Some(ns) => format!("{}.{}", ns, name),
        None => name.to_string(),
    };
    let expected = match sig.arity {
        Arity::Fixed => {
            let n = sig.args.len();
            if n == 1 {
                "1 argument".to_string()
            } else {
                format!("{} arguments", n)
            }
        }
        Arity::Optional { required } => {
            let max = sig.args.len();
            if required == max {
                format!("{} arguments", required)
            } else {
                format!("{} to {} arguments", required, max)
            }
        }
    };
    Err(anyhow::anyhow!(
        "{}() expects {}, got {}",
        prefix,
        expected,
        actual
    ))
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
        assert!(err.to_string().contains("2 to 3 arguments"));
    }

    // ---- central arity validation ----------------------------------------
    //
    // Every function with a registered `FunctionSig` goes through
    // `validate_arity` before dispatch. These tests pin the behaviour of
    // each Arity variant plus the error message format so the consolidated
    // path cannot silently regress.

    #[test]
    fn arity_fixed_accepts_exact_arg_count() {
        let reg = make_registry();
        let e = dummy_event();
        // `contains(String, String) -> Bool` is Arity::Fixed with 2 args.
        let ok = reg.call(
            None,
            "contains",
            &[Value::String("hello".into()), Value::String("ell".into())],
            &e,
        );
        assert!(ok.is_ok(), "fixed arity with correct count should succeed");
    }

    #[test]
    fn arity_fixed_rejects_too_few_args() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(None, "contains", &[Value::String("hello".into())], &e)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("contains() expects 2 arguments, got 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn arity_fixed_rejects_too_many_args() {
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                None,
                "contains",
                &[
                    Value::String("a".into()),
                    Value::String("b".into()),
                    Value::String("c".into()),
                ],
                &e,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("contains() expects 2 arguments, got 3"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn arity_fixed_singular_message_for_single_arg() {
        // `to_int(x) -> Int` is Arity::Fixed with 1 arg — verify the
        // message uses "1 argument" (singular) rather than "1 arguments".
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(None, "to_int", &[], &e)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("to_int() expects 1 argument, got 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn arity_optional_accepts_min_and_max() {
        // `strftime(value, fmt[, tz])` — Optional { required: 2 }, max 3.
        let reg = make_registry();
        let e = dummy_event();
        let ts = Value::String("2026-04-19T10:30:45+00:00".into());
        let fmt = Value::String("%H:%M".into());
        let tz = Value::String("UTC".into());
        // at minimum
        assert!(reg.call(None, "strftime", &[ts.clone(), fmt.clone()], &e).is_ok());
        // at maximum
        assert!(reg.call(None, "strftime", &[ts, fmt, tz], &e).is_ok());
    }

    #[test]
    fn arity_optional_rejects_below_min_and_above_max() {
        let reg = make_registry();
        let e = dummy_event();
        let ts = Value::String("2026-04-19T10:30:45+00:00".into());
        let fmt = Value::String("%H:%M".into());
        let tz = Value::String("UTC".into());
        let err_below = reg
            .call(None, "strftime", &[ts.clone()], &e)
            .unwrap_err()
            .to_string();
        assert!(
            err_below.contains("2 to 3 arguments, got 1"),
            "unexpected error: {err_below}"
        );
        let err_above = reg
            .call(
                None,
                "strftime",
                &[ts, fmt, tz.clone(), tz],
                &e,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err_above.contains("2 to 3 arguments, got 4"),
            "unexpected error: {err_above}"
        );
    }

    #[test]
    fn arity_check_applies_to_namespaced_functions() {
        // Namespaced functions registered via `register_in_with_sig` must
        // go through the same validation path. `syslog.strip_pri` takes 1
        // argument (Arity::Fixed); too many should be rejected uniformly.
        let reg = make_registry();
        let e = dummy_event();
        let err = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[Value::String("x".into()), Value::String("y".into())],
                &e,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("syslog.strip_pri() expects 1 argument, got 2"),
            "unexpected error: {err}"
        );
    }

    // ---- format() ---------------------------------------------------------

    #[test]
    fn format_expands_event_level_placeholders() {
        let reg = make_registry();
        let e = dummy_event();
        let result = reg
            .call(
                None,
                "format",
                &[Value::String("[%{source}] %{egress}".into())],
                &e,
            )
            .unwrap();
        // egress defaults to the raw bytes ("test") in dummy_event
        let s = result.as_str().unwrap().to_string();
        assert!(s.ends_with("] test"));
        assert!(s.starts_with("["));
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
