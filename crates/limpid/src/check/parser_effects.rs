//! Effects of bare parser-call statements on the analyzer's binding
//! state.
//!
//! A statement like `syslog.parse(ingress)` or `parse_json(ingress, {…})`
//! produces workspace fields at runtime; the analyzer mirrors that by
//! merging the parser's declared `produces` schema (and any
//! user-supplied `defaults` HashLit) into the current `Bindings`.
//! Data-driven parsers without explicit defaults widen workspace to a
//! wildcard so downstream `workspace.*` reads remain admissible.

use crate::dsl::ast::{Expr, ExprKind};
use crate::dsl::field_schema::FieldType;
use crate::functions::FunctionRegistry;

use super::bindings::Bindings;

pub(super) fn apply_parser_effects(
    namespace: Option<&str>,
    name: &str,
    args: &[Expr],
    registry: &FunctionRegistry,
    bindings: &mut Bindings,
) {
    if namespace.is_none()
        && name == "csv_parse"
        && args.len() == 2
        && !registry.is_user_function(name)
        && registry.signature(None, name).is_some()
    {
        apply_csv_parse_effect(args, bindings);
        return;
    }

    let Some(info) = registry.parser(namespace, name) else {
        // Not a parser — nothing to merge into workspace. Side-effect-
        // only functions (`table_upsert`, `table_delete`) return Null
        // and contribute nothing; that's intentional silence.
        return;
    };

    // Static produces: bind each declared `(workspace.key, type)` pair.
    for spec in &info.produces {
        bindings.bind_workspace(&spec.path, spec.ty.clone());
    }
    for path in &info.scoped_wildcards {
        bindings.set_workspace_scoped_wildcard(path);
    }

    // Defaults arg (HashLit): every declared key becomes a workspace
    // binding too, with type inferred from the literal value. This is
    // the "user-declared schema" knob that lets parse_json / parse_kv
    // narrow the wildcard to a precise key set. Parsers whose defaults
    // position depends on the type of earlier args (parse_kv) supply a
    // shape-aware extractor; everyone else falls back to the
    // index-list scan, which takes the first HashLit at any declared
    // slot.
    let defaults_entries = if let Some(extractor) = info.defaults_arg_extractor {
        extractor(args)
    } else {
        info.defaults_arg_indices.iter().find_map(|&i| {
            if let Some(Expr {
                kind: ExprKind::HashLit(entries),
                ..
            }) = args.get(i)
            {
                Some(entries)
            } else {
                None
            }
        })
    };
    if let Some(entries) = defaults_entries {
        for (k, v) in entries {
            let path = vec!["workspace".to_string(), k.clone()];
            bindings.bind_workspace(&path, literal_type(v));
        }
    } else if info.wildcards {
        // Data-driven parser called without explicit defaults — widen
        // workspace to wildcard so downstream `workspace.*` reads are
        // admitted (we no longer know which keys exist).
        bindings.set_workspace_wildcard();
    }
}

/// `csv_parse` derives its output keys from the literal `field_names`
/// argument rather than a registry-level parser schema. Mirror only the
/// names the DSL can address as `workspace.<ident>`; dynamic or malformed
/// declarations remain unknown instead of widening workspace.
fn apply_csv_parse_effect(args: &[Expr], bindings: &mut Bindings) {
    let Some(Expr {
        kind: ExprKind::ArrayLit(field_names),
        ..
    }) = args.get(1)
    else {
        return;
    };

    let nullable_string = FieldType::union(FieldType::String, FieldType::Null);
    for field_name in field_names {
        let ExprKind::StringLit(field_name) = &field_name.kind else {
            continue;
        };
        if !is_dsl_identifier(field_name) {
            continue;
        }

        let path = vec!["workspace".to_string(), field_name.clone()];
        bindings.bind_workspace(&path, nullable_string.clone());
    }
}

fn is_dsl_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Best-effort type from a literal-shaped expression. Used for
/// HashLit defaults inference in parser calls; non-literal entries
/// fall through to `Any`.
fn literal_type(e: &Expr) -> FieldType {
    match &e.kind {
        ExprKind::StringLit(_) | ExprKind::Template(_) => FieldType::String,
        ExprKind::IntLit(_) => FieldType::Int,
        ExprKind::FloatLit(_) => FieldType::Float,
        ExprKind::BoolLit(_) => FieldType::Bool,
        ExprKind::Null => FieldType::Null,
        ExprKind::HashLit(_) => FieldType::Object,
        _ => FieldType::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::ast::Expr;
    use crate::dsl::parser::parse_config;
    use crate::functions::FunctionRegistry;
    use crate::functions::table::TableStore;
    use crate::pipeline::CompiledConfig;

    fn registry() -> FunctionRegistry {
        let mut reg = FunctionRegistry::new();
        let table_store = TableStore::from_configs(vec![]).unwrap();
        crate::functions::register_builtins(&mut reg, table_store);
        reg
    }

    fn shadowed_registry() -> FunctionRegistry {
        let mut reg = registry();
        let config = parse_config("def function csv_parse(text, names) { null }").unwrap();
        let compiled = CompiledConfig::from_config(config).unwrap();
        crate::functions::register_user_functions(&mut reg, &compiled);
        assert!(reg.is_user_function("csv_parse"));
        reg
    }

    fn ident(name: &str) -> Expr {
        Expr::spanless(ExprKind::Ident(vec![name.to_string()]))
    }

    fn string(s: &str) -> Expr {
        Expr::spanless(ExprKind::StringLit(s.to_string()))
    }

    fn hash(entries: &[(&str, Expr)]) -> Expr {
        let owned = entries
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        Expr::spanless(ExprKind::HashLit(owned))
    }

    fn array(items: Vec<Expr>) -> Expr {
        Expr::spanless(ExprKind::ArrayLit(items))
    }

    fn ws(key: &str) -> Vec<String> {
        vec!["workspace".to_string(), key.to_string()]
    }

    #[test]
    fn csv_parse_literal_field_names_bind_nullable_strings() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![
            ident("ingress"),
            array(vec![string("host"), string("status")]),
        ];

        apply_parser_effects(None, "csv_parse", &args, &reg, &mut bindings);

        let nullable_string = FieldType::union(FieldType::String, FieldType::Null);
        assert_eq!(bindings.get_workspace(&ws("host")), Some(&nullable_string));
        assert_eq!(
            bindings.get_workspace(&ws("status")),
            Some(&nullable_string)
        );
        assert!(!bindings.is_workspace_wildcard());
    }

    #[test]
    fn csv_parse_literal_field_names_skip_unaddressable_entries() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![
            ident("ingress"),
            array(vec![
                string("first"),
                string(""),
                string("a.b"),
                string("has space"),
                string("1leading"),
                Expr::spanless(ExprKind::IntLit(7)),
                string("_last"),
            ]),
        ];

        apply_parser_effects(None, "csv_parse", &args, &reg, &mut bindings);

        let nullable_string = FieldType::union(FieldType::String, FieldType::Null);
        assert_eq!(bindings.get_workspace(&ws("first")), Some(&nullable_string));
        assert_eq!(bindings.get_workspace(&ws("_last")), Some(&nullable_string));
        for skipped in ["", "a.b", "has space", "1leading"] {
            assert_eq!(bindings.get_workspace(&ws(skipped)), None, "{skipped}");
        }
        assert!(!bindings.is_workspace_wildcard());
    }

    #[test]
    fn csv_parse_dynamic_or_namespaced_fields_do_not_create_bindings() {
        let reg = registry();

        for (namespace, fields) in [
            (None, ident("field_names")),
            (None, string("not-an-array")),
            (Some("custom"), array(vec![string("invented")])),
        ] {
            let mut bindings = Bindings::new();
            let args = vec![ident("ingress"), fields];
            apply_parser_effects(namespace, "csv_parse", &args, &reg, &mut bindings);
            assert_eq!(bindings.get_workspace(&ws("invented")), None);
            assert!(!bindings.is_workspace_wildcard());
        }
    }

    #[test]
    fn csv_parse_duplicate_names_keep_one_honest_binding() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![
            ident("ingress"),
            array(vec![string("value"), string("value")]),
        ];

        apply_parser_effects(None, "csv_parse", &args, &reg, &mut bindings);

        assert_eq!(
            bindings.get_workspace(&ws("value")),
            Some(&FieldType::union(FieldType::String, FieldType::Null))
        );
        assert_eq!(bindings.workspace_keys().count(), 1);
    }

    #[test]
    fn user_csv_parse_shadow_does_not_fabricate_builtin_fields() {
        let reg = shadowed_registry();
        let mut bindings = Bindings::new();
        let args = vec![ident("ingress"), array(vec![string("host")])];

        apply_parser_effects(None, "csv_parse", &args, &reg, &mut bindings);

        assert_eq!(bindings.get_workspace(&ws("host")), None);
        assert!(!bindings.is_workspace_wildcard());
    }

    #[test]
    fn csv_parse_requires_registered_builtin_and_exact_arity() {
        let fields = array(vec![string("host")]);

        let unregistered = FunctionRegistry::new();
        let mut bindings = Bindings::new();
        apply_parser_effects(
            None,
            "csv_parse",
            &[ident("ingress"), fields.clone()],
            &unregistered,
            &mut bindings,
        );
        assert_eq!(bindings.get_workspace(&ws("host")), None);

        let reg = registry();
        for args in [
            vec![],
            vec![ident("ingress")],
            vec![ident("ingress"), fields.clone(), string("extra")],
        ] {
            let mut bindings = Bindings::new();
            apply_parser_effects(None, "csv_parse", &args, &reg, &mut bindings);
            assert_eq!(bindings.get_workspace(&ws("host")), None, "{args:?}");
            assert!(!bindings.is_workspace_wildcard());
        }
    }

    // parse_kv 3-arg form: defaults sits at args[2]. The pre-fix
    // analyzer hard-coded args[1] only and silently missed this slot,
    // wildcard-widening workspace instead of pinning the declared keys.
    #[test]
    fn parse_kv_three_arg_form_narrows_workspace_from_defaults() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![
            ident("ingress"),
            string("="),
            hash(&[("user", string("")), ("host", string(""))]),
        ];
        apply_parser_effects(None, "parse_kv", &args, &reg, &mut bindings);
        assert_eq!(
            bindings.get_workspace(&ws("user")),
            Some(&FieldType::String)
        );
        assert_eq!(
            bindings.get_workspace(&ws("host")),
            Some(&FieldType::String)
        );
        assert!(
            !bindings.is_workspace_wildcard(),
            "explicit defaults should pin the key set, not wildcard"
        );
    }

    // parse_kv 2-arg form: defaults sits at args[1]. Both call shapes
    // are accepted at runtime, so the analyzer must accept both too.
    #[test]
    fn parse_kv_two_arg_form_narrows_workspace_from_defaults() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![ident("ingress"), hash(&[("user", string(""))])];
        apply_parser_effects(None, "parse_kv", &args, &reg, &mut bindings);
        assert_eq!(
            bindings.get_workspace(&ws("user")),
            Some(&FieldType::String)
        );
        assert!(!bindings.is_workspace_wildcard());
    }

    // parse_kv with a non-string second arg + HashLit third arg is
    // not a valid runtime call shape — `parse_kv_impl` bails because
    // `args[1]` must be a String separator when `args[2]` is present
    // (`(Some(Object), Some(_))` falls into the catch-all bail arm).
    // The shape-aware extractor refuses to narrow from either
    // HashLit; the wildcard fallback then keeps downstream
    // `workspace.*` reads admissible at `--check` time, but no
    // *specific* key set is pinned. Pre-fix, the index-list scan
    // picked up the HashLit at `args[1]` and narrowed the workspace
    // to {ignored}, so downstream reads of `workspace.user` would
    // fail `--check` for a call that would have bailed at runtime
    // anyway.
    #[test]
    fn parse_kv_invalid_three_arg_shape_falls_back_to_wildcard() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![
            ident("ingress"),
            hash(&[("ignored", string(""))]),
            hash(&[("user", string(""))]),
        ];
        apply_parser_effects(None, "parse_kv", &args, &reg, &mut bindings);
        assert!(
            bindings.is_workspace_wildcard(),
            "invalid call shape must not pin a key set; fall back to wildcard"
        );
    }

    // parse_kv with no defaults HashLit anywhere → wildcard, matching
    // the pre-fix behaviour for data-driven parsers.
    #[test]
    fn parse_kv_without_defaults_widens_to_wildcard() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![ident("ingress"), string("=")];
        apply_parser_effects(None, "parse_kv", &args, &reg, &mut bindings);
        assert!(bindings.is_workspace_wildcard());
    }

    // regex_parse declares `defaults_arg_indices = &[]` (no defaults
    // slot at all). It's still `wildcards = true`, so calling it must
    // fall to the wildcard branch even when the call site passes a
    // stray HashLit — the scan walks zero slots and never sees it.
    // The trailing HashLit here is deliberate: it would be picked up
    // by parse_kv's `&[1, 2]`, and pinning it for regex_parse would
    // be a registry-contract violation.
    #[test]
    fn regex_parse_widens_to_wildcard_and_ignores_stray_hashlit() {
        let reg = registry();
        let mut bindings = Bindings::new();
        let args = vec![
            ident("ingress"),
            string("(?P<user>\\w+)"),
            hash(&[("user", string(""))]),
        ];
        apply_parser_effects(None, "regex_parse", &args, &reg, &mut bindings);
        assert!(bindings.is_workspace_wildcard());
        assert!(bindings.get_workspace(&ws("user")).is_none());
    }

    #[test]
    fn cef_parse_wildcards_only_the_extension_subtree() {
        let reg = registry();
        let mut bindings = Bindings::new();
        apply_parser_effects(
            Some("cef"),
            "parse",
            &[ident("ingress")],
            &reg,
            &mut bindings,
        );
        assert!(!bindings.is_workspace_wildcard());
        assert!(bindings.is_workspace_path_wildcard(&[
            "workspace".into(),
            "extension".into(),
            "src".into(),
        ]));
        assert!(!bindings.is_workspace_path_wildcard(&["workspace".into(), "unrelated".into(),]));
    }
}
