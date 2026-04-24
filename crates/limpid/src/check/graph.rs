//! Flow visualization (`limpid --check --graph[=<format>]`).
//!
//! Renders a compiled config's pipeline structure — input nodes, process
//! chain elements, and output nodes — in one of three formats:
//!
//! - **Mermaid** (default): `flowchart LR` with a subgraph per pipeline.
//!   Pastes directly into GitHub markdown / mdbook.
//! - **DOT** (Graphviz): `digraph` with a `cluster_*` subgraph per
//!   pipeline. Feeds `dot -Tsvg` / `xdot`.
//! - **ASCII** tree: one tree per pipeline. Terminal-friendly.
//!
//! Scope is intentionally minimal: only `input`, `process`, and `output`
//! nodes are drawn. Control-flow statements (`if` / `switch` / `drop` /
//! `finish`) are skipped at this commit — adding them is a future
//! extension once the minimal surface is stable.
//!
//! The output is written to stdout by the CLI; analyzer diagnostics stay
//! on stderr, so `--graph` can be piped into a file or viewer without
//! losing the `--check` report.

use anyhow::{Result, bail};

use crate::dsl::ast::{PipelineStatement, ProcessChainElement};
use crate::pipeline::CompiledConfig;

/// Requested output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphFormat {
    Mermaid,
    Dot,
    Ascii,
}

impl GraphFormat {
    /// Parse a CLI-facing format string. `None` (bare `--graph` with no
    /// value) defaults to Mermaid so pasting into GitHub markdown "just
    /// works" without choosing a format.
    pub fn parse(raw: Option<&str>) -> Result<Self> {
        let Some(s) = raw else {
            return Ok(Self::Mermaid);
        };
        match s {
            "mermaid" => Ok(Self::Mermaid),
            "dot" => Ok(Self::Dot),
            "ascii" => Ok(Self::Ascii),
            other => bail!(
                "unknown graph format: {}. Expected: mermaid, dot, ascii",
                other
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Intermediate representation
// ---------------------------------------------------------------------------

/// A single node in a pipeline's flow graph. `id` is a
/// renderer-independent, unique-within-pipeline identifier; `label` is
/// the display string. Keeping the two separate lets us escape labels
/// per renderer (Mermaid quoting rules differ from DOT).
#[derive(Debug, Clone)]
struct Node {
    id: String,
    label: String,
    kind: NodeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeKind {
    Input,
    Process,
    Output,
}

/// One pipeline's structure reduced to (nodes, edges). Edges are
/// (from_id, to_id); the renderer decides how to draw them.
#[derive(Debug, Clone)]
struct PipelineGraph {
    name: String,
    nodes: Vec<Node>,
    edges: Vec<(String, String)>,
}

/// Walk one pipeline and produce its flow graph. "Frontier" = the set of
/// node ids that the next statement will connect from. For fan-in
/// (multiple inputs), the frontier starts with every input; the next
/// process chain element fans back in to a single downstream node.
///
/// Control-flow statements are skipped at this commit — they would
/// require expressing branch nodes and join points, which is out of
/// scope for the minimal surface.
fn build_pipeline_graph(name: &str, pipeline: &crate::dsl::ast::PipelineDef) -> PipelineGraph {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut frontier: Vec<String> = Vec::new();
    let mut proc_counter = 0usize;

    for stmt in &pipeline.body {
        match stmt {
            PipelineStatement::Input(input_names) => {
                // Multiple inputs in one statement (`input a, b`) all
                // become independent source nodes sharing the next
                // downstream edge.
                for input_name in input_names {
                    let id = format!("{}_in_{}", name, sanitize(input_name));
                    nodes.push(Node {
                        id: id.clone(),
                        label: format!("input {}", input_name),
                        kind: NodeKind::Input,
                    });
                    frontier.push(id);
                }
            }
            PipelineStatement::ProcessChain(elements) => {
                for elem in elements {
                    proc_counter += 1;
                    let (id, label) = match elem {
                        ProcessChainElement::Named(pname, _args) => (
                            format!("{}_proc_{}_{}", name, proc_counter, sanitize(pname)),
                            format!("process {}", pname),
                        ),
                        ProcessChainElement::Inline(_) => (
                            format!("{}_proc_{}", name, proc_counter),
                            format!("process {}", proc_counter),
                        ),
                    };
                    nodes.push(Node {
                        id: id.clone(),
                        label,
                        kind: NodeKind::Process,
                    });
                    for from in frontier.drain(..) {
                        edges.push((from, id.clone()));
                    }
                    frontier.push(id);
                }
            }
            PipelineStatement::Output(out_name) => {
                let id = format!("{}_out_{}", name, sanitize(out_name));
                nodes.push(Node {
                    id: id.clone(),
                    label: format!("output {}", out_name),
                    kind: NodeKind::Output,
                });
                for from in frontier.drain(..) {
                    edges.push((from, id.clone()));
                }
                frontier.push(id);
            }
            // Drop / Finish / If / Switch: skipped at this commit. A
            // future extension can draw control-flow branches as
            // diamond-shaped nodes with labeled edges.
            _ => {}
        }
    }

    PipelineGraph {
        name: name.to_string(),
        nodes,
        edges,
    }
}

/// Sanitize an arbitrary name into an id fragment safe across all three
/// renderers (Mermaid/DOT both tolerate `[A-Za-z0-9_]`, ASCII doesn't
/// care). Keeps alnum + underscore, collapses everything else to `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Render `compiled`'s pipelines in the requested format. Pipelines are
/// emitted in a stable order (sorted by name) so repeated invocations
/// produce diffable output regardless of `HashMap` iteration order.
pub fn render_graph(compiled: &CompiledConfig, format: GraphFormat) -> String {
    let mut names: Vec<&String> = compiled.pipelines.keys().collect();
    names.sort();

    let graphs: Vec<PipelineGraph> = names
        .into_iter()
        .map(|n| build_pipeline_graph(n, &compiled.pipelines[n]))
        .collect();

    match format {
        GraphFormat::Mermaid => render_mermaid(&graphs),
        GraphFormat::Dot => render_dot(&graphs),
        GraphFormat::Ascii => render_ascii(&graphs),
    }
}

// ---------------------------------------------------------------------------
// Mermaid renderer
// ---------------------------------------------------------------------------

fn render_mermaid(graphs: &[PipelineGraph]) -> String {
    let mut out = String::from("flowchart LR\n");
    for g in graphs {
        out.push_str(&format!(
            "  subgraph {}[\"pipeline {}\"]\n",
            sanitize(&g.name),
            mermaid_escape(&g.name),
        ));
        for node in &g.nodes {
            out.push_str(&format!(
                "    {}[\"{}\"]\n",
                node.id,
                mermaid_escape(&node.label),
            ));
        }
        for (a, b) in &g.edges {
            out.push_str(&format!("    {} --> {}\n", a, b));
        }
        out.push_str("  end\n");
    }
    out
}

/// Mermaid labels go inside `"..."`. The literal backslash, double
/// quote, and backtick break mermaid; escape them. We don't need to
/// handle every edge case — pipeline/process names in limpid are
/// bare-ident by grammar, so this guards against user-chosen input
/// names that happen to contain special chars.
fn mermaid_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// DOT renderer
// ---------------------------------------------------------------------------

fn render_dot(graphs: &[PipelineGraph]) -> String {
    let mut out = String::from("digraph {\n  rankdir=LR;\n");
    for g in graphs {
        out.push_str(&format!("  subgraph cluster_{} {{\n", sanitize(&g.name)));
        out.push_str(&format!(
            "    label=\"pipeline {}\";\n",
            dot_escape(&g.name)
        ));
        for node in &g.nodes {
            out.push_str(&format!(
                "    {} [label=\"{}\"];\n",
                node.id,
                dot_escape(&node.label),
            ));
        }
        for (a, b) in &g.edges {
            out.push_str(&format!("    {} -> {};\n", a, b));
        }
        out.push_str("  }\n");
    }
    out.push_str("}\n");
    out
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// ASCII renderer
// ---------------------------------------------------------------------------

/// One tree per pipeline. Inputs are collapsed into a single
/// `inputs: a, b` line when there's more than one (the fan-in case);
/// process and output nodes get their own tree rows. Last row of each
/// pipeline uses `└─` to close the tree cleanly.
fn render_ascii(graphs: &[PipelineGraph]) -> String {
    let mut out = String::new();
    for (i, g) in graphs.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("pipeline {}\n", g.name));

        // Collect rows; we need to know which is last before emitting
        // the `├─` / `└─` prefix.
        let mut rows: Vec<String> = Vec::new();
        let inputs: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == NodeKind::Input)
            .map(|n| n.label.as_str())
            .collect();
        if !inputs.is_empty() {
            if inputs.len() == 1 {
                rows.push(inputs[0].to_string());
            } else {
                // Strip the `input ` prefix for the combined line.
                let names: Vec<&str> = inputs
                    .iter()
                    .map(|s| s.strip_prefix("input ").unwrap_or(s))
                    .collect();
                rows.push(format!("inputs: {}", names.join(", ")));
            }
        }
        for n in &g.nodes {
            if n.kind == NodeKind::Process || n.kind == NodeKind::Output {
                rows.push(n.label.clone());
            }
        }

        let last = rows.len().saturating_sub(1);
        for (idx, row) in rows.iter().enumerate() {
            let prefix = if idx == last { "└─ " } else { "├─ " };
            out.push_str(&format!("{}{}\n", prefix, row));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsl::parser::parse_config;

    fn compile(src: &str) -> CompiledConfig {
        let cfg = parse_config(src).expect("parse");
        CompiledConfig::from_config(cfg).expect("compile")
    }

    #[test]
    fn format_parse_defaults_and_errors() {
        assert_eq!(GraphFormat::parse(None).unwrap(), GraphFormat::Mermaid);
        assert_eq!(
            GraphFormat::parse(Some("mermaid")).unwrap(),
            GraphFormat::Mermaid
        );
        assert_eq!(GraphFormat::parse(Some("dot")).unwrap(), GraphFormat::Dot);
        assert_eq!(
            GraphFormat::parse(Some("ascii")).unwrap(),
            GraphFormat::Ascii
        );
        let err = GraphFormat::parse(Some("svg")).unwrap_err().to_string();
        assert!(err.contains("unknown graph format"), "err: {}", err);
        assert!(err.contains("mermaid, dot, ascii"), "err: {}", err);
    }

    const SINGLE: &str = r#"
def input a { type tcp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def pipeline p {
    input a
    process { workspace.x = "y" }
    output o
}
"#;

    const FANIN: &str = r#"
def input a { type tcp bind "0.0.0.0:514" }
def input b { type udp bind "0.0.0.0:514" }
def output o { type stdout template "x" }
def process parse { workspace.x = "y" }
def pipeline p {
    input a, b
    process parse
    output o
}
"#;

    const TWO_PIPELINES: &str = r#"
def input a { type tcp bind "0.0.0.0:514" }
def input b { type tcp bind "0.0.0.0:515" }
def output o1 { type stdout template "x" }
def output o2 { type stdout template "x" }
def pipeline first { input a; output o1 }
def pipeline second { input b; output o2 }
"#;

    #[test]
    fn mermaid_basic_has_flowchart_and_subgraph() {
        let cfg = compile(SINGLE);
        let s = render_graph(&cfg, GraphFormat::Mermaid);
        assert!(s.starts_with("flowchart LR"), "got:\n{}", s);
        assert!(s.contains("subgraph p[\"pipeline p\"]"), "got:\n{}", s);
        assert!(s.contains("\"input a\""), "got:\n{}", s);
        assert!(s.contains("\"output o\""), "got:\n{}", s);
        assert!(s.contains(" --> "), "got:\n{}", s);
        assert!(s.contains("  end"), "got:\n{}", s);
    }

    #[test]
    fn mermaid_fanin_renders_both_sources() {
        let cfg = compile(FANIN);
        let s = render_graph(&cfg, GraphFormat::Mermaid);
        assert!(s.contains("\"input a\""), "got:\n{}", s);
        assert!(s.contains("\"input b\""), "got:\n{}", s);
        // Both inputs should have edges to the same process node.
        let arrow_count = s.matches(" --> ").count();
        // a -> parse, b -> parse, parse -> o = 3 edges total
        assert_eq!(arrow_count, 3, "got:\n{}", s);
    }

    #[test]
    fn dot_basic_has_digraph_cluster_and_rankdir() {
        let cfg = compile(SINGLE);
        let s = render_graph(&cfg, GraphFormat::Dot);
        assert!(s.starts_with("digraph {"), "got:\n{}", s);
        assert!(s.contains("rankdir=LR;"), "got:\n{}", s);
        assert!(s.contains("subgraph cluster_p {"), "got:\n{}", s);
        assert!(s.contains("label=\"pipeline p\""), "got:\n{}", s);
        assert!(s.contains(" -> "), "got:\n{}", s);
        assert!(s.trim_end().ends_with('}'), "got:\n{}", s);
    }

    #[test]
    fn ascii_basic_renders_tree() {
        let cfg = compile(SINGLE);
        let s = render_graph(&cfg, GraphFormat::Ascii);
        assert!(s.contains("pipeline p"), "got:\n{}", s);
        assert!(s.contains("input a"), "got:\n{}", s);
        assert!(s.contains("output o"), "got:\n{}", s);
        assert!(s.contains("└─ "), "got:\n{}", s);
    }

    #[test]
    fn ascii_fanin_collapses_multiple_inputs() {
        let cfg = compile(FANIN);
        let s = render_graph(&cfg, GraphFormat::Ascii);
        assert!(s.contains("inputs: a, b"), "got:\n{}", s);
        assert!(s.contains("process parse"), "got:\n{}", s);
    }

    #[test]
    fn multiple_pipelines_render_separately() {
        let cfg = compile(TWO_PIPELINES);

        let m = render_graph(&cfg, GraphFormat::Mermaid);
        assert!(m.contains("\"pipeline first\""), "got:\n{}", m);
        assert!(m.contains("\"pipeline second\""), "got:\n{}", m);

        let d = render_graph(&cfg, GraphFormat::Dot);
        assert!(d.contains("cluster_first"), "got:\n{}", d);
        assert!(d.contains("cluster_second"), "got:\n{}", d);

        let a = render_graph(&cfg, GraphFormat::Ascii);
        assert!(a.contains("pipeline first"), "got:\n{}", a);
        assert!(a.contains("pipeline second"), "got:\n{}", a);
        // Separator blank line between the two trees.
        assert!(a.contains("\n\npipeline second"), "got:\n{}", a);
    }
}
