use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
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

fn is_serde_constructor(name: &str) -> bool {
    name.starts_with("from_") || name.starts_with("to_")
}

#[derive(Default)]
struct SerdeJsonImports {
    namespaces: HashSet<String>,
    constructors: HashSet<String>,
    glob: bool,
}

fn collect_serde_json_imports(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    imports: &mut SerdeJsonImports,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_serde_json_imports(&path.tree, prefix, imports);
            prefix.pop();
        }
        syn::UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            if prefix.as_slice() == ["serde_json"] || prefix.as_slice() == ["serde_json", "self"] {
                imports.namespaces.insert("serde_json".to_owned());
            } else if prefix.first().is_some_and(|part| part == "serde_json")
                && prefix.last().is_some_and(|part| is_serde_constructor(part))
            {
                imports.constructors.insert(name.ident.to_string());
            }
            prefix.pop();
        }
        syn::UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            if prefix.as_slice() == ["serde_json"] || prefix.as_slice() == ["serde_json", "self"] {
                imports.namespaces.insert(rename.rename.to_string());
            } else if prefix.first().is_some_and(|part| part == "serde_json")
                && prefix.last().is_some_and(|part| is_serde_constructor(part))
            {
                imports.constructors.insert(rename.rename.to_string());
            }
            prefix.pop();
        }
        syn::UseTree::Group(group) => {
            for tree in &group.items {
                collect_serde_json_imports(tree, prefix, imports);
            }
        }
        syn::UseTree::Glob(_) => {
            if prefix.as_slice() == ["serde_json"] {
                imports.glob = true;
            }
        }
    }
}

struct FunctionBodyFacts<'a> {
    local_calls: HashSet<String>,
    serde_roundtrips: Vec<String>,
    serde_imports: &'a SerdeJsonImports,
    top_level_functions: &'a HashSet<String>,
}

impl<'a> FunctionBodyFacts<'a> {
    fn new(serde_imports: &'a SerdeJsonImports, top_level_functions: &'a HashSet<String>) -> Self {
        Self {
            local_calls: HashSet::new(),
            serde_roundtrips: Vec::new(),
            serde_imports,
            top_level_functions,
        }
    }
}

impl<'ast> Visit<'ast> for FunctionBodyFacts<'_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(path) = call.func.as_ref() {
            let segments = path
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>();
            if segments.len() == 1
                && segments[0]
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_lowercase())
            {
                self.local_calls.insert(segments[0].clone());
            }
            let qualified = segments.len() >= 2
                && segments
                    .first()
                    .is_some_and(|segment| self.serde_imports.namespaces.contains(segment))
                && segments
                    .last()
                    .is_some_and(|name| is_serde_constructor(name));
            let bare = segments.len() == 1
                && segments.first().is_some_and(|name| {
                    self.serde_imports.constructors.contains(name)
                        || (self.serde_imports.glob && is_serde_constructor(name))
                });
            if qualified || bare {
                self.serde_roundtrips.push(segments.join("::"));
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if path.qself.is_none()
            && path.path.segments.len() == 1
            && let Some(name) = path
                .path
                .segments
                .first()
                .map(|segment| segment.ident.to_string())
            && self.top_level_functions.contains(&name)
        {
            self.local_calls.insert(name);
        }
        syn::visit::visit_expr_path(self, path);
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
    let top_level_functions = functions.keys().cloned().collect::<HashSet<_>>();
    let mut serde_imports = SerdeJsonImports::default();
    serde_imports.namespaces.insert("serde_json".to_owned());
    for item in &file.items {
        if let syn::Item::Use(item) = item {
            collect_serde_json_imports(&item.tree, &mut Vec::new(), &mut serde_imports);
        }
    }
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
        let mut facts = FunctionBodyFacts::new(&serde_imports, &top_level_functions);
        facts.visit_block(&function.block);
        violations.extend(
            facts
                .serde_roundtrips
                .into_iter()
                .map(|roundtrip| format!("{name} uses {roundtrip}")),
        );
        for callee in facts.local_calls {
            if functions.contains_key(&callee) {
                pending.push(callee);
            } else if !serde_imports.constructors.contains(&callee)
                && !(serde_imports.glob && is_serde_constructor(&callee))
            {
                violations.push(format!("{name} has unresolved top-level helper {callee}"));
            }
        }
    }
    violations
}

fn top_level_function_edges(source: &str, function_name: &str) -> Option<HashSet<String>> {
    let file = syn::parse_file(source).ok()?;
    let functions = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Fn(function) => Some((function.sig.ident.to_string(), function)),
            _ => None,
        })
        .collect::<HashMap<_, _>>();
    let top_level_functions = functions.keys().cloned().collect::<HashSet<_>>();
    let mut serde_imports = SerdeJsonImports::default();
    serde_imports.namespaces.insert("serde_json".to_owned());
    for item in &file.items {
        if let syn::Item::Use(item) = item {
            collect_serde_json_imports(&item.tree, &mut Vec::new(), &mut serde_imports);
        }
    }
    let function = functions.get(function_name)?;
    let mut facts = FunctionBodyFacts::new(&serde_imports, &top_level_functions);
    facts.visit_block(&function.block);
    Some(facts.local_calls)
}

fn manifest_entries<'a>(manifest: &'a str, section: &str, key: &str) -> Vec<&'a str> {
    let mut current_section = "";
    let mut entries = Vec::new();
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap().trim();
        if line.starts_with('[') && line.ends_with(']') {
            current_section = line.trim_matches(['[', ']']);
        } else if current_section == section
            && let Some((entry_key, value)) = line.split_once('=')
            && entry_key.trim() == key
        {
            entries.push(value.trim());
        }
    }
    entries
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
fn limpidctl_top_level_renderer_guard_rejects_roundtrip_bridges_and_local_wire_dtos() {
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

    for bridge in [
        r#"
            struct MetricsSnapshot;
            use serde_json as json;
            fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
                bridge(snapshot)
            }
            fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
            fn bridge(snapshot: &MetricsSnapshot) -> Option<String> {
                let _ = json::to_string_pretty(snapshot);
                Some(String::new())
            }
        "#,
        r#"
            struct MetricsSnapshot;
            use serde_json::from_slice as decode;
            fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
                bridge(snapshot)
            }
            fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
            fn bridge(_: &MetricsSnapshot) -> Option<String> {
                let _: Result<MetricsSnapshot, _> = decode(b"{}");
                Some(String::new())
            }
        "#,
        r#"
            struct MetricsSnapshot;
            use serde_json::{from_reader, to_writer};
            fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
                bridge(snapshot)
            }
            fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
            fn bridge(_: &MetricsSnapshot) -> Option<String> {
                let _: Result<MetricsSnapshot, _> = from_reader(&b"{}"[..]);
                let _ = to_writer(Vec::new(), &());
                Some(String::new())
            }
        "#,
        r#"
            struct MetricsSnapshot;
            use serde_json::*;
            fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
                bridge(snapshot)
            }
            fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
            fn bridge(snapshot: &MetricsSnapshot) -> Option<String> {
                let _ = to_vec(snapshot);
                Some(String::new())
            }
        "#,
        r#"
            struct MetricsSnapshot;
            fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
                bridge(snapshot)
            }
            fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
            fn bridge(snapshot: &MetricsSnapshot) -> Option<String> {
                let _: Result<MetricsSnapshot, _> = serde_json::from_slice(b"{}");
                let _ = serde_json::to_vec(snapshot);
                Some(String::new())
            }
        "#,
    ] {
        assert!(!limpidctl_typed_renderer_violations(bridge).is_empty());
    }

    let custom_names = r#"
        struct MetricsSnapshot;
        fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
            semantic(snapshot)
        }
        fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        fn semantic(_: &MetricsSnapshot) -> Option<String> {
            custom_from_snapshot();
            custom_to_text();
            Some(String::new())
        }
        fn custom_from_snapshot() {}
        fn custom_to_text() {}
    "#;
    assert!(limpidctl_typed_renderer_violations(custom_names).is_empty());

    let function_item_bridge = r#"
        struct MetricsSnapshot;
        fn render_default_stats(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        fn render_stats_details(snapshot: &MetricsSnapshot) -> Option<String> {
            let _ = [snapshot].iter().map(helper);
            Some(String::new())
        }
        fn helper(snapshot: &&MetricsSnapshot) -> String {
            serde_json::to_string(snapshot).unwrap()
        }
    "#;
    assert!(!limpidctl_typed_renderer_violations(function_item_bridge).is_empty());

    let typed_function_items = r#"
        struct MetricsSnapshot;
        fn render_default_stats(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        fn render_stats_details(snapshot: &MetricsSnapshot) -> Option<String> {
            let mapper: fn(String) -> String = std::convert::identity;
            let _ = [snapshot].iter().map(helper).map(mapper);
            Some(String::new())
        }
        fn helper(_: &&MetricsSnapshot) -> String { String::new() }
    "#;
    assert!(limpidctl_typed_renderer_violations(typed_function_items).is_empty());

    for incomplete in [
        r#"
            struct MetricsSnapshot;
            fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        "#,
        r#"
            struct MetricsSnapshot;
            fn render_default_stats(snapshot: &MetricsSnapshot) -> Option<String> {
                missing_helper(snapshot)
            }
            fn render_stats_details(_: &MetricsSnapshot) -> Option<String> { Some(String::new()) }
        "#,
    ] {
        assert!(!limpidctl_typed_renderer_violations(incomplete).is_empty());
    }
}

#[test]
fn limpidctl_production_renderers_use_the_shared_type_without_a_legacy_bridge() {
    let source = include_str!("../../limpidctl/src/main.rs");
    let violations = limpidctl_typed_renderer_violations(source);
    assert!(violations.is_empty(), "{violations:?}");
    let detail_edges = top_level_function_edges(source, "render_stats_details").unwrap();
    assert!(detail_edges.contains("parse_detail_metric"));
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

#[test]
fn consumers_inherit_the_schema_dependency_from_the_workspace() {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace = crate_root.join("../..");
    let schema = fs::read_to_string(crate_root.join("Cargo.toml")).unwrap();
    let schema_versions = manifest_entries(&schema, "package", "version");
    assert_eq!(schema_versions.len(), 1);
    let schema_version = schema_versions[0].trim_matches('"');

    let root = fs::read_to_string(workspace.join("Cargo.toml")).unwrap();
    let root_entries = manifest_entries(&root, "workspace.dependencies", "limpid-metrics-schema");
    assert_eq!(root_entries.len(), 1);
    let root_entry = root_entries[0].replace(char::is_whitespace, "");
    assert!(root_entry.contains(&format!("version=\"{schema_version}\"")));
    assert!(root_entry.contains("path=\"crates/limpid-metrics-schema\""));

    for manifest in [
        "crates/limpid/Cargo.toml",
        "crates/limpidctl/Cargo.toml",
        "crates/limpid-prometheus/Cargo.toml",
    ] {
        let source = fs::read_to_string(workspace.join(manifest)).unwrap();
        let entries = manifest_entries(&source, "dependencies", "limpid-metrics-schema");
        assert_eq!(entries.len(), 1, "{manifest}");
        assert_eq!(
            entries[0].replace(char::is_whitespace, ""),
            "{workspace=true}",
            "{manifest}"
        );
    }
}
