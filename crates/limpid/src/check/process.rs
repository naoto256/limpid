//! Static call-graph validation for `def process` declarations.
//!
//! Process bodies may call other named processes, but the resulting
//! graph must be acyclic. Rejecting cycles at config-load time keeps
//! execution bounded by the compiled configuration and makes each
//! invocation path a finite, statically known identity.

use std::collections::{HashMap, HashSet};

use crate::dsl::ast::{BranchBody, ProcessStatement};
use crate::pipeline::CompiledConfig;

use super::{DiagKind, Diagnostic, Level};

pub(super) fn check_process_cycles(config: &CompiledConfig, diagnostics: &mut Vec<Diagnostic>) {
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, process) in &config.processes {
        let mut callees = Vec::new();
        collect_process_callees(&process.body, config, &mut callees);
        callees.sort_unstable();
        callees.dedup();
        adjacency.insert(name.as_str(), callees);
    }

    let mut color: HashMap<&str, u8> = adjacency.keys().map(|name| (*name, 0)).collect();
    let mut stack = Vec::new();
    let mut reported = HashSet::new();
    let mut names: Vec<&str> = adjacency.keys().copied().collect();
    names.sort_unstable();
    for name in names {
        if color[&name] == 0 {
            visit_process(
                name,
                &adjacency,
                &mut color,
                &mut stack,
                &mut reported,
                diagnostics,
            );
        }
    }
}

fn collect_process_callees<'a>(
    statements: &'a [ProcessStatement],
    config: &'a CompiledConfig,
    out: &mut Vec<&'a str>,
) {
    for statement in statements {
        match statement {
            ProcessStatement::ProcessCall(name) => {
                if let Some((stored_name, _)) = config.processes.get_key_value(name) {
                    out.push(stored_name.as_str());
                }
            }
            ProcessStatement::If(chain) => {
                for (_, body) in &chain.branches {
                    collect_branch_callees(body, config, out);
                }
                if let Some(body) = &chain.else_body {
                    collect_branch_callees(body, config, out);
                }
            }
            ProcessStatement::Switch(_, arms) => {
                for arm in arms {
                    collect_branch_callees(&arm.body, config, out);
                }
            }
            ProcessStatement::TryCatch(try_body, catch_body) => {
                collect_process_callees(try_body, config, out);
                collect_process_callees(catch_body, config, out);
            }
            ProcessStatement::Assign(_, _)
            | ProcessStatement::LetBinding(_, _)
            | ProcessStatement::Drop
            | ProcessStatement::Error(_)
            | ProcessStatement::ExprStmt(_) => {}
        }
    }
}

fn collect_branch_callees<'a>(
    body: &'a [BranchBody],
    config: &'a CompiledConfig,
    out: &mut Vec<&'a str>,
) {
    for item in body {
        if let BranchBody::Process(statement) = item {
            collect_process_callees(std::slice::from_ref(statement), config, out);
        }
    }
}

fn visit_process<'a>(
    node: &'a str,
    adjacency: &HashMap<&'a str, Vec<&'a str>>,
    color: &mut HashMap<&'a str, u8>,
    stack: &mut Vec<&'a str>,
    reported: &mut HashSet<Vec<String>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    color.insert(node, 1);
    stack.push(node);
    if let Some(callees) = adjacency.get(node) {
        for &callee in callees {
            match color.get(callee).copied().unwrap_or(0) {
                0 => visit_process(callee, adjacency, color, stack, reported, diagnostics),
                1 => report_cycle(callee, stack, reported, diagnostics),
                _ => {}
            }
        }
    }
    stack.pop();
    color.insert(node, 2);
}

fn report_cycle(
    callee: &str,
    stack: &[&str],
    reported: &mut HashSet<Vec<String>>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let position = stack.iter().position(|name| *name == callee).unwrap_or(0);
    let cycle: Vec<String> = stack[position..]
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    let mut canonical = cycle.clone();
    let first = canonical
        .iter()
        .enumerate()
        .min_by_key(|(_, name)| name.as_str())
        .map(|(index, _)| index)
        .unwrap_or(0);
    canonical.rotate_left(first);
    if !reported.insert(canonical) {
        return;
    }

    let path = if cycle.len() == 1 {
        format!("`{}` calls itself", cycle[0])
    } else {
        format!("`{}` → `{}`", cycle.join("` → `"), cycle[0])
    };
    diagnostics.push(Diagnostic {
        level: Level::Error,
        kind: DiagKind::Other,
        message: format!(
            "process call cycle detected: {path}; recursion in `def process` is not supported"
        ),
        span: None,
        help: Some("rewrite the process calls to form an acyclic graph".to_owned()),
    });
}
