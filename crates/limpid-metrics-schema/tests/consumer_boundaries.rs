use std::collections::{HashMap, HashSet};
use std::process::Command;

use syn::visit::Visit;

fn has_one_live_normal_dependency(dependencies: &[serde_json::Value]) -> bool {
    let matching = dependencies
        .iter()
        .filter(|dependency| dependency["name"] == "limpid-metrics-schema")
        .collect::<Vec<_>>();
    matching.len() == 1 && matching[0]["kind"].is_null() && matching[0]["target"].is_null()
}

fn type_ends_with(ty: &syn::Type, expected: &str) -> bool {
    let syn::Type::Reference(reference) = ty else {
        return false;
    };
    let syn::Type::Path(path) = reference.elem.as_ref() else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == expected)
}

#[derive(Default)]
struct FunctionBodyFacts {
    local_calls: HashSet<String>,
    serde_roundtrips: Vec<String>,
}

impl<'ast> Visit<'ast> for FunctionBodyFacts {
    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if path.path.segments.len() == 1 {
            self.local_calls
                .insert(path.path.segments[0].ident.to_string());
        }
        syn::visit::visit_expr_path(self, path);
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            let segments = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            if segments.len() == 1 {
                self.local_calls.insert(segments[0].clone());
            }
            if segments
                .first()
                .is_some_and(|segment| segment == "serde_json")
                && segments.last().is_some_and(|segment| {
                    matches!(
                        segment.as_str(),
                        "to_value" | "to_string" | "from_value" | "from_str"
                    )
                })
            {
                self.serde_roundtrips.push(segments.join("::"));
            }
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn type_mentions_serde_value(ty: &syn::Type) -> bool {
    struct Finder(bool);
    impl<'ast> Visit<'ast> for Finder {
        fn visit_path(&mut self, path: &'ast syn::Path) {
            let segments = path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            if segments.ends_with(&["serde_json".to_owned(), "Value".to_owned()]) {
                self.0 = true;
            }
            syn::visit::visit_path(self, path);
        }
    }
    let mut finder = Finder(false);
    finder.visit_type(ty);
    finder.0
}

fn has_wire_deserialize_shape(item: &syn::ItemStruct) -> bool {
    let derives_deserialize = item.attrs.iter().any(|attribute| {
        if !attribute.path().is_ident("derive") {
            return false;
        }
        attribute
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
            )
            .is_ok_and(|paths| {
                paths.iter().any(|path| {
                    path.segments
                        .last()
                        .is_some_and(|segment| segment.ident == "Deserialize")
                })
            })
    });
    if !derives_deserialize {
        return false;
    }
    let fields = item
        .fields
        .iter()
        .filter_map(|field| field.ident.as_ref().map(ToString::to_string))
        .collect::<HashSet<_>>();
    [
        ["schema", "metrics"].as_slice(),
        ["name", "help", "series"].as_slice(),
    ]
    .iter()
    .any(|shape| shape.iter().all(|field| fields.contains(*field)))
        || (fields.contains("labels")
            && (fields.contains("value")
                || (fields.contains("buckets")
                    && fields.contains("sum")
                    && fields.contains("count"))))
}

fn limpidctl_typed_renderer_violations(source: &str) -> Vec<String> {
    let file = syn::parse_file(source).unwrap();
    let functions = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Fn(function) => Some((function.sig.ident.to_string(), function)),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let mut violations = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Struct(item) if has_wire_deserialize_shape(item) => {
                Some(format!("local wire DTO {}", item.ident))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut pending = vec![
        "render_default_stats".to_owned(),
        "render_stats_details".to_owned(),
    ];
    let mut visited = HashSet::new();

    while let Some(name) = pending.pop() {
        if !visited.insert(name.clone()) {
            continue;
        }
        let Some(function) = functions.get(&name) else {
            violations.push(format!("missing renderer {name}"));
            continue;
        };
        if matches!(
            name.as_str(),
            "render_default_stats" | "render_stats_details"
        ) {
            let typed = function
                .sig
                .inputs
                .first()
                .is_some_and(|input| match input {
                    syn::FnArg::Typed(argument) => type_ends_with(&argument.ty, "MetricsSnapshot"),
                    syn::FnArg::Receiver(_) => false,
                });
            if !typed {
                violations.push(format!("{name} is not typed"));
            }
        }
        for input in &function.sig.inputs {
            if let syn::FnArg::Typed(argument) = input
                && type_mentions_serde_value(&argument.ty)
            {
                violations.push(format!("{name} accepts serde_json::Value"));
            }
        }
        if let syn::ReturnType::Type(_, ty) = &function.sig.output
            && type_mentions_serde_value(ty)
        {
            violations.push(format!("{name} returns serde_json::Value"));
        }
        let mut facts = FunctionBodyFacts::default();
        facts.visit_block(&function.block);
        violations.extend(
            facts
                .serde_roundtrips
                .into_iter()
                .map(|roundtrip| format!("{name} uses {roundtrip}")),
        );
        pending.extend(
            facts
                .local_calls
                .into_iter()
                .filter(|callee| functions.contains_key(callee)),
        );
    }
    violations
}

#[test]
fn dependency_guard_rejects_dev_only_and_target_specific_entries() {
    let normal = serde_json::json!({
        "name": "limpid-metrics-schema", "kind": null, "target": null
    });
    let dev = serde_json::json!({
        "name": "limpid-metrics-schema", "kind": "dev", "target": null
    });
    let targeted = serde_json::json!({
        "name": "limpid-metrics-schema", "kind": null, "target": "cfg(unix)"
    });
    assert!(has_one_live_normal_dependency(&[normal]));
    assert!(!has_one_live_normal_dependency(&[dev]));
    assert!(!has_one_live_normal_dependency(&[targeted]));
}

#[test]
fn limpidctl_typed_renderer_guard_rejects_roundtrip_bridges_and_local_wire_dtos() {
    let direct = r#"
        struct MetricsSnapshot;
        fn render_default_stats(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        fn render_stats_details(snapshot: &MetricsSnapshot) -> Option<String> {
            semantic(snapshot)
        }
        fn semantic(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
    "#;
    assert!(limpidctl_typed_renderer_violations(direct).is_empty());

    let bridge = r#"
        struct MetricsSnapshot;
        fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> { bridge(snapshot) }
        fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        fn bridge(snapshot: &MetricsSnapshot) -> Option<String> {
            let _ = serde_json::to_value(snapshot);
            Some(String::new())
        }
    "#;
    assert!(!limpidctl_typed_renderer_violations(bridge).is_empty());

    let legacy = r#"
        struct MetricsSnapshot;
        #[derive(serde::Deserialize)] struct LegacySnapshot { schema: u32, metrics: Vec<()> }
        fn render_default_stats(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
    "#;
    assert!(!limpidctl_typed_renderer_violations(legacy).is_empty());
}

#[test]
fn limpidctl_production_renderers_use_the_shared_type_without_a_legacy_bridge() {
    let source = include_str!("../../limpidctl/src/main.rs");
    let violations = limpidctl_typed_renderer_violations(source);
    assert!(violations.is_empty(), "{violations:?}");
}

#[test]
fn producer_and_both_consumers_have_a_live_normal_dependency() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

    let mut invalid = Vec::new();
    for package_name in ["limpid", "limpidctl", "limpid-prometheus"] {
        let package = metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|package| package["name"] == package_name)
            .unwrap();
        if !has_one_live_normal_dependency(package["dependencies"].as_array().unwrap()) {
            invalid.push(package_name);
        }
    }
    assert!(
        invalid.is_empty(),
        "not live normal dependencies: {invalid:?}"
    );
}
