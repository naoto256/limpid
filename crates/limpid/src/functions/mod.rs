//! Function registry: maps function names to implementations.
//!
//! All expression functions (built-in and future dynamic plugins) register
//! themselves here. The DSL evaluator resolves function calls through the
//! registry instead of hardcoded match arms.
//!
//! This is the extension point for future dynamic (.so) function loading.
//!
//! # Closure shape
//!
//! Every primitive is stored as a [`for<'bump> Fn(&'bump EventArena<'bump>,
//! &[Value<'bump>], &BorrowedEvent<'bump>) -> Result<Value<'bump>>`].
//! The higher-ranked `'bump` lets the registry hold a single closure
//! type while every individual call binds the per-event arena's
//! lifetime — primitives can allocate arena-backed strings / objects
//! without the registry caring about their lifetime parameter.
//!
//! # Layout
//!
//! - [`primitives`] — flat-namespace, schema-agnostic functions,
//!   grouped by concern: case (`lower`, `upper`), string predicates
//!   (`contains`, `starts_with`, `ends_with`), regex (`regex_*`),
//!   time (`timestamp`, `strftime`, `strptime`), env
//!   (`hostname`, `version`), enrichment (`geoip`, `table_*`), hashing
//!   (`md5` / `sha1` / `sha256`), serialisation (`to_json`, `to_bytes`,
//!   `to_string`, `to_int`), parsers (`parse_json`, `parse_kv`,
//!   `csv_parse`, `regex_parse`), arrays (`find_by`, `len`, `append`,
//!   `prepend`). One file per function (or per tightly-related group
//!   such as `hashes.rs` / `string_predicates.rs`) so `mod.rs` does
//!   not become a megafile.
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
pub mod otlp;
pub mod primitives;
pub mod syslog;
pub mod table;

use std::collections::HashMap;

use anyhow::Result;

use crate::dsl::arena::EventArena;
use crate::dsl::ast::FunctionDef;
use crate::dsl::value::Value;
use crate::event::BorrowedEvent;
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

/// Closure storage type. The higher-ranked `for<'bump>` is the key
/// piece: the registry holds one heterogeneous map of closures, but
/// every individual call instantiates `'bump` with the active per-event
/// arena's lifetime — primitives produce `Value<'bump>` that lives
/// exactly as long as the arena does.
pub type ExprFn = Box<
    dyn for<'bump> Fn(
            &'bump EventArena<'bump>,
            &[Value<'bump>],
            &BorrowedEvent<'bump>,
        ) -> Result<Value<'bump>>
        + Send
        + Sync,
>;

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
    /// User-defined `def function` declarations. Stored separately from
    /// `functions` (the closure table for built-ins) because their
    /// bodies need to recurse through `eval_expr_with_scope`, which in
    /// turn needs the registry — a closure that captured `&self` would
    /// be self-referencing, so dispatch through `call()` resolves the
    /// FunctionDef and evaluates the body in-place instead.
    user_definitions: HashMap<String, FunctionDef>,
}

impl FunctionRegistry {
    pub fn new() -> Self {
        Self {
            functions: HashMap::new(),
            signatures: HashMap::new(),
            parsers: HashMap::new(),
            user_definitions: HashMap::new(),
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
        F: for<'bump> Fn(
                &'bump EventArena<'bump>,
                &[Value<'bump>],
                &BorrowedEvent<'bump>,
            ) -> Result<Value<'bump>>
            + Send
            + Sync
            + 'static,
    {
        self.functions.insert((None, name.to_string()), Box::new(f));
    }

    /// Register a flat primitive function together with its static
    /// signature. The analyzer consults the signature for arg / return
    /// type checking; the implementation closure is identical to the
    /// no-sig form.
    pub fn register_with_sig<F>(&mut self, name: &str, sig: FunctionSig, f: F)
    where
        F: for<'bump> Fn(
                &'bump EventArena<'bump>,
                &[Value<'bump>],
                &BorrowedEvent<'bump>,
            ) -> Result<Value<'bump>>
            + Send
            + Sync
            + 'static,
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
        F: for<'bump> Fn(
                &'bump EventArena<'bump>,
                &[Value<'bump>],
                &BorrowedEvent<'bump>,
            ) -> Result<Value<'bump>>
            + Send
            + Sync
            + 'static,
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
        F: for<'bump> Fn(
                &'bump EventArena<'bump>,
                &[Value<'bump>],
                &BorrowedEvent<'bump>,
            ) -> Result<Value<'bump>>
            + Send
            + Sync
            + 'static,
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
    /// a sig — only `parse_json` still goes through bare `register`
    /// today, and `register_parser` supplies a sig for it — skip the
    /// check and keep their historical hand-rolled arity guards.
    pub fn call<'bump>(
        &self,
        namespace: Option<&str>,
        name: &str,
        args: &[Value<'bump>],
        event: &BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
    ) -> Result<Value<'bump>> {
        // User-defined `def function` declarations dispatch first. They
        // live in the flat namespace only (no `ns.fn` form for now).
        if namespace.is_none()
            && let Some(fn_def) = self.user_definitions.get(name)
        {
            return self.call_user_function(fn_def, args, event, arena);
        }

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
        f(arena, args, event)
    }

    /// Register a user-defined `def function` declaration. Both the
    /// FunctionDef itself (consulted by [`call`] to evaluate the body)
    /// and a synthesized `Any^arity -> Any` signature (consumed by the
    /// analyzer's arity check) are stored. The body's purity check
    /// happens elsewhere — see `check::function::check_function_def`.
    pub fn register_user_function(&mut self, fn_def: FunctionDef) {
        let arity = fn_def.params.len();
        let sig = FunctionSig::fixed(&vec![FieldType::Any; arity], FieldType::Any);
        let key = (None, fn_def.name.clone());
        self.signatures.insert(key, sig);
        self.user_definitions.insert(fn_def.name.clone(), fn_def);
    }

    /// Evaluate a user-defined function body with `args` bound to the
    /// declared parameters. Called from [`call`] when dispatch hits a
    /// user-defined function. The Event is threaded through so nested
    /// expressions (which the analyzer guarantees don't read it
    /// directly) can call into other primitives that do — e.g. a
    /// `def function` that calls `regex_match` still routes through
    /// the primitive's standard signature.
    fn call_user_function<'bump>(
        &self,
        fn_def: &FunctionDef,
        args: &[Value<'bump>],
        event: &BorrowedEvent<'bump>,
        arena: &'bump EventArena<'bump>,
    ) -> Result<Value<'bump>> {
        if args.len() != fn_def.params.len() {
            anyhow::bail!(
                "function {}() expects {} argument(s), got {}",
                fn_def.name,
                fn_def.params.len(),
                args.len()
            );
        }
        let mut scope = crate::dsl::eval::LocalScope::new();
        for (param, val) in fn_def.params.iter().zip(args.iter()) {
            scope.bind(param, *val);
        }
        // Execute let bindings in declaration order. Each `let x = expr`
        // is a (re)assignment to `x` in the same local scope; `LocalScope::bind`
        // overwrites any prior value, which is the only update mechanism
        // available inside a function body.
        for fl in &fn_def.body.lets {
            let v = crate::dsl::eval::eval_expr_with_scope(&fl.value, event, self, &scope, arena)?;
            scope.bind(&fl.name, v);
        }
        crate::dsl::eval::eval_expr_with_scope(&fn_def.body.ret, event, self, &scope, arena)
    }
}

impl Default for FunctionRegistry {
    fn default() -> Self {
        Self::new()
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
    otlp::register(reg);
}

/// Install every `def function` declaration from a compiled config
/// into `reg`. Mirrors [`register_builtins`] for user-authored DSL
/// functions: callers (runtime startup, `--test-pipeline`, the
/// analyzer's own registry) get a single call site instead of an
/// open-coded loop.
pub fn register_user_functions(
    reg: &mut FunctionRegistry,
    config: &crate::pipeline::CompiledConfig,
) {
    for fn_def in config.functions.values() {
        reg.register_user_function(fn_def.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::arena::EventArena;
    use crate::event::OwnedEvent;
    use crate::functions::table::TableStore;
    use bytes::Bytes;
    use std::net::SocketAddr;

    fn make_registry() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = TableStore::from_configs(vec![]).unwrap();
        register_builtins(&mut reg, table_store);
        reg
    }

    fn dummy_owned() -> OwnedEvent {
        OwnedEvent::new(
            Bytes::from("test"),
            "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        )
    }

    fn ts_value(s: &str) -> Value<'static> {
        Value::Timestamp(
            chrono::DateTime::parse_from_rfc3339(s)
                .unwrap()
                .with_timezone(&chrono::Utc),
        )
    }

    #[test]
    fn strftime_formats_rfc3339_input() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    ts_value("2026-04-19T10:30:45+00:00"),
                    Value::String(arena.alloc_str("%Y/%m/%d %H:%M:%S")),
                ],
                &bevent,
                &arena,
            )
            .unwrap();
        assert_eq!(result, Value::String("2026/04/19 10:30:45"));
    }

    #[test]
    fn strftime_bsd_syslog_format() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    ts_value("2026-04-19T05:07:09+00:00"),
                    Value::String(arena.alloc_str("%b %e %H:%M:%S")),
                ],
                &bevent,
                &arena,
            )
            .unwrap();
        assert_eq!(result, Value::String("Apr 19 05:07:09"));
    }

    #[test]
    fn strftime_utc_timezone() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    ts_value("2026-04-19T10:30:45+09:00"),
                    Value::String(arena.alloc_str("%Y-%m-%dT%H:%M:%S%z")),
                    Value::String(arena.alloc_str("UTC")),
                ],
                &bevent,
                &arena,
            )
            .unwrap();
        assert_eq!(result, Value::String("2026-04-19T01:30:45+0000"));
    }

    #[test]
    fn strftime_fixed_offset() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let result = reg
            .call(
                None,
                "strftime",
                &[
                    ts_value("2026-04-19T10:30:45+00:00"),
                    Value::String(arena.alloc_str("%H:%M")),
                    Value::String(arena.alloc_str("+09:00")),
                ],
                &bevent,
                &arena,
            )
            .unwrap();
        assert_eq!(result, Value::String("19:30"));
    }

    #[test]
    fn strftime_rejects_string_input() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(
                None,
                "strftime",
                &[
                    Value::String(arena.alloc_str("2026-04-19T10:30:45+00:00")),
                    Value::String(arena.alloc_str("%Y")),
                ],
                &bevent,
                &arena,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("must be a timestamp"),
            "got: {}",
            err
        );
    }

    #[test]
    fn strftime_rejects_bad_timezone() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(
                None,
                "strftime",
                &[
                    ts_value("2026-04-19T10:30:45+00:00"),
                    Value::String(arena.alloc_str("%Y")),
                    Value::String(arena.alloc_str("bogus")),
                ],
                &bevent,
                &arena,
            )
            .unwrap_err();
        assert!(err.to_string().contains("invalid timezone"));
    }

    #[test]
    fn strftime_rejects_wrong_arity() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(
                None,
                "strftime",
                &[ts_value("2026-04-19T10:30:45+00:00")],
                &bevent,
                &arena,
            )
            .unwrap_err();
        assert!(err.to_string().contains("2 to 3 arguments"));
    }

    #[test]
    fn arity_fixed_accepts_exact_arg_count() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let ok = reg.call(
            None,
            "contains",
            &[
                Value::String(arena.alloc_str("hello")),
                Value::String(arena.alloc_str("ell")),
            ],
            &bevent,
            &arena,
        );
        assert!(ok.is_ok(), "fixed arity with correct count should succeed");
    }

    #[test]
    fn arity_fixed_rejects_too_few_args() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(
                None,
                "contains",
                &[Value::String(arena.alloc_str("hello"))],
                &bevent,
                &arena,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("contains() expects 2 arguments, got 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn arity_fixed_rejects_too_many_args() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(
                None,
                "contains",
                &[
                    Value::String(arena.alloc_str("a")),
                    Value::String(arena.alloc_str("b")),
                    Value::String(arena.alloc_str("c")),
                ],
                &bevent,
                &arena,
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
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(None, "to_int", &[], &bevent, &arena)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("to_int() expects 1 argument, got 0"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn arity_optional_accepts_min_and_max() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let tsv = ts_value("2026-04-19T10:30:45+00:00");
        let fmt = Value::String(arena.alloc_str("%H:%M"));
        let tz = Value::String(arena.alloc_str("UTC"));
        assert!(reg.call(None, "strftime", &[tsv, fmt], &bevent, &arena).is_ok());
        assert!(reg.call(None, "strftime", &[tsv, fmt, tz], &bevent, &arena).is_ok());
    }

    #[test]
    fn arity_optional_rejects_below_min_and_above_max() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let tsv = ts_value("2026-04-19T10:30:45+00:00");
        let fmt = Value::String(arena.alloc_str("%H:%M"));
        let tz = Value::String(arena.alloc_str("UTC"));
        let err_below = reg
            .call(None, "strftime", &[tsv], &bevent, &arena)
            .unwrap_err()
            .to_string();
        assert!(
            err_below.contains("2 to 3 arguments, got 1"),
            "unexpected error: {err_below}"
        );
        let err_above = reg
            .call(None, "strftime", &[tsv, fmt, tz, tz], &bevent, &arena)
            .unwrap_err()
            .to_string();
        assert!(
            err_above.contains("2 to 3 arguments, got 4"),
            "unexpected error: {err_above}"
        );
    }

    #[test]
    fn arity_check_applies_to_namespaced_functions() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(
                Some("syslog"),
                "strip_pri",
                &[
                    Value::String(arena.alloc_str("x")),
                    Value::String(arena.alloc_str("y")),
                ],
                &bevent,
                &arena,
            )
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("syslog.strip_pri() expects 1 argument, got 2"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn register_in_and_dispatch_namespaced() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let mut reg = FunctionRegistry::new();
        reg.register_in("_test_block3", "passthrough", |_arena, args, _e| {
            Ok(args.first().copied().unwrap_or(Value::Null))
        });
        let result = reg
            .call(
                Some("_test_block3"),
                "passthrough",
                &[Value::String(arena.alloc_str("hi"))],
                &bevent,
                &arena,
            )
            .unwrap();
        assert_eq!(result, Value::String("hi"));
    }

    #[test]
    fn unknown_namespace_error_message() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let err = reg
            .call(Some("not_a_real_namespace"), "parse", &[], &bevent, &arena)
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
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let mut reg = FunctionRegistry::new();
        reg.register_in("_test_block3", "known", |_arena, _a, _e| Ok(Value::Null));
        let err = reg
            .call(Some("_test_block3"), "missing", &[], &bevent, &arena)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unknown function '_test_block3.missing'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn flat_primitive_regression_still_callable() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let reg = make_registry();
        let result = reg
            .call(
                None,
                "lower",
                &[Value::String(arena.alloc_str("HELLO"))],
                &bevent,
                &arena,
            )
            .unwrap();
        assert_eq!(result, Value::String("hello"));
    }

    #[test]
    fn namespace_and_flat_do_not_collide() {
        let bump = bumpalo::Bump::new();
        let arena = EventArena::new(&bump);
        let owned = dummy_owned();
        let bevent = owned.view_in(&arena);
        let mut reg = FunctionRegistry::new();
        reg.register("ping", |arena, _a, _e| {
            Ok(Value::String(arena.alloc_str("flat")))
        });
        reg.register_in("_test_block3", "ping", |arena, _a, _e| {
            Ok(Value::String(arena.alloc_str("namespaced")))
        });
        assert_eq!(
            reg.call(None, "ping", &[], &bevent, &arena).unwrap(),
            Value::String("flat")
        );
        assert_eq!(
            reg.call(Some("_test_block3"), "ping", &[], &bevent, &arena)
                .unwrap(),
            Value::String("namespaced")
        );
    }

}
