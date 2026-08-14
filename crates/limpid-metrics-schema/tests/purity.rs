use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use syn::visit::Visit;

fn rust_sources(directory: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[derive(Default)]
struct RuntimeReferences {
    hits: Vec<String>,
    forbidden_aliases: HashSet<String>,
}

fn is_runtime_path(segments: &[String]) -> bool {
    if segments.first().is_some_and(|segment| segment == "limpid") {
        return true;
    }
    let Some(sync) = segments.windows(2).position(|pair| pair == ["std", "sync"]) else {
        return false;
    };
    segments[sync + 2..].iter().any(|segment| {
        matches!(
            segment.as_str(),
            "Arc"
                | "Weak"
                | "Mutex"
                | "RwLock"
                | "Condvar"
                | "Barrier"
                | "OnceLock"
                | "LazyLock"
                | "atomic"
        ) || segment.starts_with("Atomic")
    })
}

fn is_sync_namespace(segments: &[String]) -> bool {
    segments == ["std", "sync"] || segments == ["std", "sync", "self"]
}

fn collect_imports(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    aliases: &mut HashSet<String>,
    hits: &mut Vec<String>,
) {
    match tree {
        syn::UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            collect_imports(&path.tree, prefix, aliases, hits);
            prefix.pop();
        }
        syn::UseTree::Name(name) => {
            prefix.push(name.ident.to_string());
            if is_runtime_path(prefix) || is_sync_namespace(prefix) {
                aliases.insert(name.ident.to_string());
                hits.push(prefix.join("::"));
            }
            prefix.pop();
        }
        syn::UseTree::Rename(rename) => {
            prefix.push(rename.ident.to_string());
            if is_runtime_path(prefix) || is_sync_namespace(prefix) {
                aliases.insert(rename.rename.to_string());
                hits.push(prefix.join("::"));
            }
            prefix.pop();
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_imports(item, prefix, aliases, hits);
            }
        }
        syn::UseTree::Glob(_) => {
            if is_runtime_path(prefix) || is_sync_namespace(prefix) {
                hits.push(format!("{}::*", prefix.join("::")));
            }
        }
    }
}

impl<'ast> Visit<'ast> for RuntimeReferences {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>();
        if is_runtime_path(&segments)
            || segments
                .first()
                .is_some_and(|segment| self.forbidden_aliases.contains(segment))
        {
            self.hits.push(segments.join("::"));
        }
        syn::visit::visit_path(self, path);
    }

    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        if attribute.path().is_ident("serde") {
            let _ = attribute.parse_nested_meta(|meta| inspect_serde_meta(meta, &mut self.hits));
        }
        syn::visit::visit_attribute(self, attribute);
    }

    fn visit_lit_str(&mut self, literal: &'ast syn::LitStr) {
        if literal.value().starts_with("limpid_") {
            self.hits.push(literal.value());
        }
        syn::visit::visit_lit_str(self, literal);
    }
}

fn inspect_serde_meta(
    meta: syn::meta::ParseNestedMeta<'_>,
    hits: &mut Vec<String>,
) -> syn::Result<()> {
    if meta.path.is_ident("deny_unknown_fields") {
        hits.push("deny_unknown_fields".to_owned());
    }
    if meta.input.peek(syn::Token![=]) {
        let _: syn::Expr = meta.value()?.parse()?;
    } else if meta.input.peek(syn::token::Paren) {
        meta.parse_nested_meta(|nested| inspect_serde_meta(nested, hits))?;
    }
    Ok(())
}

fn inspect(source: &str) -> Vec<String> {
    let syntax = syn::parse_file(source).unwrap();
    let mut references = RuntimeReferences::default();
    for item in &syntax.items {
        if let syn::Item::Use(item_use) = item {
            collect_imports(
                &item_use.tree,
                &mut Vec::new(),
                &mut references.forbidden_aliases,
                &mut references.hits,
            );
        }
    }
    references.visit_file(&syntax);
    references.hits
}

#[test]
fn schema_crate_has_only_serde_as_a_normal_dependency() {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let package = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "limpid-metrics-schema")
        .unwrap();
    let normal = package["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|dependency| dependency["kind"].is_null())
        .map(|dependency| {
            (
                dependency["name"].as_str().unwrap(),
                dependency["target"].as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(normal, [("serde", None)]);
}

#[test]
fn every_schema_source_file_is_free_of_runtime_types_and_semantics() {
    let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&source_root, &mut files);
    files.sort();
    assert!(!files.is_empty());

    for file in files {
        let source = fs::read_to_string(&file).unwrap();
        assert!(inspect(&source).is_empty(), "{}", file.display());
    }
}

#[test]
fn purity_guard_allows_wire_collections_and_serde_derives() {
    let source = r#"
        // std::sync::Arc and limpid::metrics::Registry in comments are inert.
        #[derive(serde::Serialize, serde::Deserialize)]
        struct WireValue {
            labels: std::collections::BTreeMap<String, String>,
        }

        fn decode<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
            serde::Deserialize::deserialize(deserializer)
        }

        use serde::*;
        use std::collections::*;
        #[serde(rename = "wire", default)]
        struct Additive;
        #[serde(rename(serialize = "out", deserialize = "in"), bound(deserialize = "T: serde::Deserialize<'de>"))]
        struct Generic<T>(T);
    "#;
    assert!(inspect(source).is_empty());
}

#[test]
fn purity_guard_detects_each_forbidden_runtime_reference() {
    for source in [
        "fn leak(_: limpid::metrics::Registry) {}",
        "type Shared = std::sync::Arc<u64>;",
        "type Cell = std::sync::atomic::AtomicU64;",
        "fn build(_: limpid::metrics::RuntimeBuilder) {}",
        "use std::sync::Arc as Shared; struct State(Shared<u64>);",
        "use std::sync::{Mutex as Lock, RwLock}; struct State(Lock<u64>, RwLock<u64>);",
        "use std::sync::*; struct State(Arc<u64>);",
        "use std::sync as synchronization; struct State(synchronization::Arc<u64>);",
        "#[serde(deny_unknown_fields)] struct Closed;",
        "#[serde(rename = \"wire\", deny_unknown_fields)] struct Closed;",
        "#[serde(default, rename = \"wire\", deny_unknown_fields)] struct Closed;",
        "#[serde(rename(serialize = \"out\", deserialize = \"in\"), deny_unknown_fields)] struct Closed;",
        "#[serde(bound(deserialize = \"T: serde::Deserialize<'de>\"), deny_unknown_fields)] struct Closed<T>(T);",
        "const FAMILY: &str = \"limpid_runtime_total\";",
    ] {
        assert!(!inspect(source).is_empty(), "{source}");
    }
}
