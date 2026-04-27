//! DSL parser: converts `.limpid` text into AST using the pest grammar.

#![allow(clippy::redundant_closure)]

use anyhow::{Context, Result, bail};
use pest::Parser;
use pest::iterators::Pair;
use pest_derive::Parser;

use super::ast::*;
use super::span::Span;

#[derive(Parser)]
#[grammar = "dsl/limpid.pest"]
pub struct LimpidParser;

/// Parse a complete configuration string into a `Config` AST.
///
/// Spans in the resulting AST are tagged with `file_id = 0`. Callers
/// that need multi-file attribution (the include loader, eventually)
/// should use [`parse_config_with_file_id`] and feed the matching id
/// into the [`crate::dsl::span::SourceMap`].
#[allow(dead_code)] // used extensively by tests; kept public for eventual lib surface
pub fn parse_config(input: &str) -> Result<Config> {
    parse_config_with_file_id(input, 0)
}

/// Parse a complete configuration string, tagging every span with the
/// caller-supplied `file_id`.
pub fn parse_config_with_file_id(input: &str, file_id: u32) -> Result<Config> {
    let mut pairs = LimpidParser::parse(Rule::config, input).context("failed to parse DSL")?;

    let config_pair = pairs.next().unwrap();
    let mut definitions = Vec::new();
    let mut global_blocks = Vec::new();
    let mut includes = Vec::new();

    for pair in config_pair.into_inner() {
        match pair.as_rule() {
            Rule::top_level_item => {
                let inner = first_inner(pair)?;
                match inner.as_rule() {
                    Rule::definition => {
                        let def_inner = first_inner(inner)?;
                        let def = match def_inner.as_rule() {
                            Rule::def_input => {
                                Definition::Input(parse_input_def(def_inner, file_id)?)
                            }
                            Rule::def_output => {
                                Definition::Output(parse_output_def(def_inner, file_id)?)
                            }
                            Rule::def_process => {
                                Definition::Process(parse_process_def(def_inner, file_id)?)
                            }
                            Rule::def_pipeline => {
                                Definition::Pipeline(parse_pipeline_def(def_inner, file_id)?)
                            }
                            Rule::def_function => {
                                Definition::Function(parse_function_def(def_inner, file_id)?)
                            }
                            _ => unreachable!(
                                "unexpected definition rule: {:?}",
                                def_inner.as_rule()
                            ),
                        };
                        definitions.push(def);
                    }
                    Rule::include_directive => {
                        let path_pair = first_inner(inner)?;
                        includes.push(parse_string_lit(&path_pair));
                    }
                    Rule::global_block => {
                        global_blocks.push(parse_global_block(inner, file_id)?);
                    }
                    _ => {}
                }
            }
            Rule::EOI => {}
            _ => {}
        }
    }

    Ok(Config {
        definitions,
        global_blocks,
        includes,
    })
}

fn span_of(pair: &Pair<Rule>, file_id: u32) -> Span {
    let s = pair.as_span();
    Span::new(file_id, s.start(), s.end())
}

fn parse_global_block(pair: Pair<Rule>, file_id: u32) -> Result<GlobalBlock> {
    let mut inner = pair.into_inner();
    let name_pair = inner.next().unwrap();
    let name = name_pair.as_str().to_string();
    let properties = inner
        .map(|p| parse_property(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(GlobalBlock { name, properties })
}

// ---------------------------------------------------------------------------
// Input / Output
// ---------------------------------------------------------------------------

fn parse_input_def(pair: Pair<Rule>, file_id: u32) -> Result<InputDef> {
    let mut inner = pair.into_inner();
    let name_pair = inner.next().unwrap();
    let name = name_pair.as_str().to_string();
    let properties = inner
        .map(|p| parse_property(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(InputDef { name, properties })
}

fn parse_output_def(pair: Pair<Rule>, file_id: u32) -> Result<OutputDef> {
    let mut inner = pair.into_inner();
    let name_pair = inner.next().unwrap();
    let name = name_pair.as_str().to_string();
    let properties = inner
        .map(|p| parse_property(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(OutputDef { name, properties })
}

fn parse_property(pair: Pair<Rule>, file_id: u32) -> Result<Property> {
    let mut inner = pair.into_inner();
    let key_pair = inner.next().unwrap();
    let key = key_pair.as_str().to_string();

    let second = inner.next().unwrap();
    match second.as_rule() {
        Rule::property => {
            // nested block: key { property* }
            // We already consumed the key; remaining pairs are properties
            let mut props = vec![parse_property(second, file_id)?];
            for p in inner {
                props.push(parse_property(p, file_id)?);
            }
            Ok(Property::Block {
                key,
                properties: props,
            })
        }
        _ => {
            // key-value: key expr
            let value_span = Some(span_of(&second, file_id));
            let value = parse_expr_from_pair(second, file_id)?;
            Ok(Property::KeyValue {
                key,
                value,
                value_span,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Process definition
// ---------------------------------------------------------------------------

fn parse_process_def(pair: Pair<Rule>, file_id: u32) -> Result<ProcessDef> {
    let mut inner = pair.into_inner();
    let name_pair = inner.next().unwrap();
    let name = name_pair.as_str().to_string();
    let body = inner
        .map(|p| parse_process_stmt(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(ProcessDef { name, body })
}

fn parse_process_stmt(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let inner = first_inner(pair)?;
    match inner.as_rule() {
        Rule::process_drop => Ok(ProcessStatement::Drop),
        Rule::process_call => parse_process_call(inner, file_id),
        Rule::process_assign => parse_process_assign(inner, file_id),
        Rule::process_if => parse_process_if(inner, file_id),
        Rule::process_switch => parse_process_switch(inner, file_id),
        Rule::process_try_catch => parse_process_try_catch(inner, file_id),
        Rule::process_foreach => parse_process_foreach(inner, file_id),
        Rule::process_let => parse_process_let(inner, file_id),
        Rule::process_expr_stmt => parse_process_expr_stmt(inner, file_id),
        _ => bail!("unexpected process statement: {:?}", inner.as_rule()),
    }
}

fn parse_process_call(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let mut inner = pair.into_inner();
    let name = inner.next().unwrap().as_str().to_string();
    let args = if let Some(args_pair) = inner.next() {
        parse_func_args(args_pair, file_id)?
    } else {
        vec![]
    };
    Ok(ProcessStatement::ProcessCall(name, args))
}

fn parse_process_let(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let mut inner = pair.into_inner();
    let name = inner.next().unwrap().as_str().to_string();
    let expr = parse_expr_from_pair(inner.next().unwrap(), file_id)?;
    Ok(ProcessStatement::LetBinding(name, expr))
}

fn parse_process_expr_stmt(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let inner = first_inner(pair)?;
    let expr = parse_func_call_expr(inner, file_id)?;
    Ok(ProcessStatement::ExprStmt(expr))
}

fn parse_process_assign(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let mut inner = pair.into_inner();
    let target_pair = inner.next().unwrap();
    let target = parse_assign_target(target_pair)?;
    let expr = parse_expr_from_pair(inner.next().unwrap(), file_id)?;
    Ok(ProcessStatement::Assign(target, expr))
}

fn parse_assign_target(pair: Pair<Rule>) -> Result<AssignTarget> {
    let path_pair = first_inner(pair)?;
    let parts: Vec<String> = path_pair
        .into_inner()
        .map(|p| p.as_str().to_string())
        .collect();

    match parts.as_slice() {
        [single] if single == "egress" => Ok(AssignTarget::Egress),
        [first, rest @ ..] if first == "workspace" && !rest.is_empty() => {
            Ok(AssignTarget::Workspace(rest.to_vec()))
        }
        _ => bail!("invalid assign target: {:?}", parts),
    }
}

fn parse_process_if(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let if_chain = parse_if_chain_generic(pair, file_id, |p, fid| {
        let stmt = parse_process_stmt(p, fid)?;
        Ok(BranchBody::Process(stmt))
    })?;
    Ok(ProcessStatement::If(if_chain))
}

fn parse_process_switch(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let mut inner = pair.into_inner();
    let discriminant = parse_expr_from_pair(inner.next().unwrap(), file_id)?;
    let arms = inner
        .map(|arm| {
            parse_switch_arm_generic(arm, file_id, |p, fid| {
                Ok(BranchBody::Process(parse_process_stmt(p, fid)?))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ProcessStatement::Switch(discriminant, arms))
}

fn parse_process_try_catch(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let mut inner = pair.into_inner();
    let try_pair = inner.next().unwrap();
    let catch_pair = inner.next().unwrap();
    let try_body = try_pair
        .into_inner()
        .map(|p| parse_process_stmt(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    let catch_body = catch_pair
        .into_inner()
        .map(|p| parse_process_stmt(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(ProcessStatement::TryCatch(try_body, catch_body))
}

fn parse_process_foreach(pair: Pair<Rule>, file_id: u32) -> Result<ProcessStatement> {
    let mut inner = pair.into_inner();
    let iterable = parse_expr_from_pair(inner.next().unwrap(), file_id)?;
    let body = inner
        .map(|p| parse_process_stmt(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(ProcessStatement::ForEach(iterable, body))
}

// ---------------------------------------------------------------------------
// Pipeline definition
// ---------------------------------------------------------------------------

fn parse_pipeline_def(pair: Pair<Rule>, file_id: u32) -> Result<PipelineDef> {
    let mut inner = pair.into_inner();
    let name_pair = inner.next().unwrap();
    let name = name_pair.as_str().to_string();
    let body = inner
        .map(|p| parse_pipeline_stmt(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(PipelineDef { name, body })
}

/// Parse a `def function name(params) { expr }` definition.
///
/// Body is a single expression — for branching / mapping, the user
/// uses the expression-form `switch` (parsed via [`parse_switch_expr`]).
fn parse_function_def(pair: Pair<Rule>, file_id: u32) -> Result<FunctionDef> {
    let mut inner = pair.into_inner();
    let name_pair = inner.next().unwrap();
    let name = name_pair.as_str().to_string();
    let mut params = Vec::new();
    let mut body: Option<Expr> = None;
    for p in inner {
        match p.as_rule() {
            Rule::func_params => {
                params = p
                    .into_inner()
                    .map(|param| param.as_str().to_string())
                    .collect();
            }
            Rule::expr => {
                body = Some(parse_expr(p, file_id)?);
            }
            _ => bail!("unexpected rule in def_function: {:?}", p.as_rule()),
        }
    }
    let body = body.ok_or_else(|| anyhow::anyhow!("def function {} missing body", name))?;
    Ok(FunctionDef { name, params, body })
}

fn parse_pipeline_stmt(pair: Pair<Rule>, file_id: u32) -> Result<PipelineStatement> {
    let inner = first_inner(pair)?;
    match inner.as_rule() {
        Rule::pipeline_drop => Ok(PipelineStatement::Drop),
        Rule::pipeline_finish => Ok(PipelineStatement::Finish),
        Rule::pipeline_input => {
            let names: Vec<String> = inner.into_inner().map(|p| p.as_str().to_string()).collect();
            if names.is_empty() {
                bail!("pipeline input statement requires at least one input name");
            }
            Ok(PipelineStatement::Input(names))
        }
        Rule::pipeline_output => {
            let name = first_inner(inner)?.as_str().to_string();
            Ok(PipelineStatement::Output(name))
        }
        Rule::pipeline_process_chain => parse_pipeline_process_chain(inner, file_id),
        Rule::pipeline_if => parse_pipeline_if(inner, file_id),
        Rule::pipeline_switch => parse_pipeline_switch(inner, file_id),
        _ => bail!("unexpected pipeline statement: {:?}", inner.as_rule()),
    }
}

fn parse_pipeline_process_chain(pair: Pair<Rule>, file_id: u32) -> Result<PipelineStatement> {
    let elements = pair
        .into_inner()
        .map(|p| parse_chain_element(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(PipelineStatement::ProcessChain(elements))
}

fn parse_chain_element(pair: Pair<Rule>, file_id: u32) -> Result<ProcessChainElement> {
    let inner = first_inner(pair)?;
    match inner.as_rule() {
        Rule::process_ref => {
            let mut parts = inner.into_inner();
            let name = parts.next().unwrap().as_str().to_string();
            let args = if let Some(args_pair) = parts.next() {
                parse_func_args(args_pair, file_id)?
            } else {
                vec![]
            };
            Ok(ProcessChainElement::Named(name, args))
        }
        Rule::inline_process => {
            let body = inner
                .into_inner()
                .map(|p| parse_process_stmt(p, file_id))
                .collect::<Result<Vec<_>>>()?;
            Ok(ProcessChainElement::Inline(body))
        }
        _ => bail!("unexpected chain element: {:?}", inner.as_rule()),
    }
}

fn parse_pipeline_if(pair: Pair<Rule>, file_id: u32) -> Result<PipelineStatement> {
    let if_chain = parse_if_chain_generic(pair, file_id, |p, fid| {
        let stmt = parse_pipeline_stmt(p, fid)?;
        Ok(BranchBody::Pipeline(stmt))
    })?;
    Ok(PipelineStatement::If(if_chain))
}

fn parse_pipeline_switch(pair: Pair<Rule>, file_id: u32) -> Result<PipelineStatement> {
    let mut inner = pair.into_inner();
    let discriminant = parse_expr_from_pair(inner.next().unwrap(), file_id)?;
    let arms = inner
        .map(|arm| {
            parse_switch_arm_generic(arm, file_id, |p, fid| {
                Ok(BranchBody::Pipeline(parse_pipeline_stmt(p, fid)?))
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(PipelineStatement::Switch(discriminant, arms))
}

// ---------------------------------------------------------------------------
// Shared: if-chain and switch-arm parsing
// ---------------------------------------------------------------------------

/// Generic if/else-if/else parser.
/// Inner pairs alternate: expr, block, expr, block, ..., and optionally a lone block for else.
fn parse_if_chain_generic<F>(pair: Pair<Rule>, file_id: u32, mut parse_body: F) -> Result<IfChain>
where
    F: FnMut(Pair<Rule>, u32) -> Result<BranchBody>,
{
    let inner: Vec<Pair<Rule>> = pair.into_inner().collect();
    let mut branches = Vec::new();
    let mut else_body = None;
    let mut i = 0;

    while i < inner.len() {
        if inner[i].as_rule() == Rule::expr {
            let condition = parse_expr_from_pair(inner[i].clone(), file_id)?;
            i += 1;
            // Next is a block (process_block or pipeline_block)
            let block = inner[i].clone();
            i += 1;
            let body = block
                .into_inner()
                .map(|p| parse_body(p, file_id))
                .collect::<Result<Vec<_>>>()?;
            branches.push((condition, body));
        } else {
            // Else block (no condition)
            let block = inner[i].clone();
            i += 1;
            let body = block
                .into_inner()
                .map(|p| parse_body(p, file_id))
                .collect::<Result<Vec<_>>>()?;
            else_body = Some(body);
        }
    }

    Ok(IfChain {
        branches,
        else_body,
    })
}

fn parse_switch_arm_generic<F>(
    pair: Pair<Rule>,
    file_id: u32,
    mut parse_body: F,
) -> Result<SwitchArm>
where
    F: FnMut(Pair<Rule>, u32) -> Result<BranchBody>,
{
    let mut inner = pair.into_inner().peekable();

    // Check if first child is an expr (non-default arm) or a body stmt (default arm)
    let pattern = if inner.peek().map(|p| p.as_rule()) == Some(Rule::expr) {
        Some(parse_expr_from_pair(inner.next().unwrap(), file_id)?)
    } else {
        None
    };

    let body = inner
        .map(|p| parse_body(p, file_id))
        .collect::<Result<Vec<_>>>()?;

    Ok(SwitchArm { pattern, body })
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

fn parse_expr_from_pair(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    match pair.as_rule() {
        Rule::expr => parse_expr(pair, file_id),
        _ => parse_atom_or_unary(pair, file_id),
    }
}

/// Parse an `expr` rule: `unary_expr (bin_op unary_expr)*`
/// Uses a simple precedence climbing approach.
fn parse_expr(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    let mut inner: Vec<Pair<Rule>> = pair.into_inner().collect();

    if inner.len() == 1 {
        return parse_atom_or_unary(inner.remove(0), file_id);
    }

    // Build a flat list of (expr, op, expr, op, expr, ...)
    // Then apply precedence
    let mut operands = Vec::new();
    let mut operators = Vec::new();

    let mut i = 0;
    while i < inner.len() {
        if inner[i].as_rule() == Rule::bin_op {
            operators.push(parse_bin_op(&inner[i])?);
            i += 1;
        } else {
            operands.push(parse_atom_or_unary(inner[i].clone(), file_id)?);
            i += 1;
        }
    }

    // Apply precedence by folding
    fold_by_precedence(&mut operands, &mut operators)
}

fn precedence(op: &BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 5,
    }
}

fn fold_by_precedence(operands: &mut Vec<Expr>, operators: &mut Vec<BinOp>) -> Result<Expr> {
    if operands.len() == 1 {
        return Ok(operands.remove(0));
    }
    if operators.is_empty() {
        bail!(
            "malformed expression: no operators for {} operands",
            operands.len()
        );
    }

    // Find lowest precedence operator (rightmost for left-associativity)
    let min_prec = operators.iter().map(precedence).min().unwrap();
    // Find the *last* operator with that precedence (left-associative: fold left, so find first)
    let idx = operators
        .iter()
        .position(|op| precedence(op) == min_prec)
        .unwrap();

    let op = operators.remove(idx);
    let mut right_operands = operands.split_off(idx + 1);
    let mut right_operators = operators.split_off(idx);
    let left_operands = operands;
    let left_operators = operators;

    let left = fold_by_precedence(left_operands, left_operators)?;
    let right = fold_by_precedence(&mut right_operands, &mut right_operators)?;

    // The folded BinOp covers `[left.span.start, right.span.end)` on the
    // left operand's file. In single-file parsing both spans share a
    // file_id; we use the left's as authoritative.
    let span = Span::new(left.span.file_id, left.span.start, right.span.end);
    Ok(Expr::new(
        ExprKind::BinOp(Box::new(left), op, Box::new(right)),
        span,
    ))
}

fn parse_atom_or_unary(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    let span = span_of(&pair, file_id);
    match pair.as_rule() {
        Rule::unary_expr => {
            let mut inner = pair.into_inner();
            let first = inner.next().unwrap();
            if first.as_rule() == Rule::unary_op {
                let op = match first.as_str().trim() {
                    "not" => UnaryOp::Not,
                    "-" => UnaryOp::Neg,
                    other => bail!("unknown unary operator: {}", other),
                };
                let operand = parse_atom_or_unary(inner.next().unwrap(), file_id)?;
                Ok(Expr::new(ExprKind::UnaryOp(op, Box::new(operand)), span))
            } else {
                // It's a postfix_expr (atom with optional .field access)
                parse_postfix_expr(first, file_id)
            }
        }
        Rule::atom => parse_atom(pair, file_id),
        Rule::expr => parse_expr(pair, file_id),
        // Direct literal/ident matches from property values etc.
        Rule::string_lit => parse_string_lit_expr(&pair, file_id),
        Rule::integer_lit => Ok(Expr::new(ExprKind::IntLit(pair.as_str().parse()?), span)),
        Rule::float_lit => Ok(Expr::new(ExprKind::FloatLit(pair.as_str().parse()?), span)),
        Rule::bool_lit => Ok(Expr::new(ExprKind::BoolLit(pair.as_str() == "true"), span)),
        Rule::null_lit => Ok(Expr::new(ExprKind::Null, span)),
        Rule::ident_path => {
            let parts: Vec<String> = pair.into_inner().map(|p| p.as_str().to_string()).collect();
            Ok(Expr::new(ExprKind::Ident(parts), span))
        }
        Rule::ident => Ok(Expr::new(
            ExprKind::Ident(vec![pair.as_str().to_string()]),
            span,
        )),
        Rule::func_call => parse_func_call_expr(pair, file_id),
        Rule::hash_lit => parse_hash_lit(pair, file_id),
        Rule::array_lit => parse_array_lit(pair, file_id),
        _ => bail!("unexpected expression rule: {:?}", pair.as_rule()),
    }
}

fn parse_postfix_expr(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    let span = span_of(&pair, file_id);
    let mut inner = pair.into_inner();
    let atom_pair = inner.next().unwrap();
    let base = parse_atom(atom_pair, file_id)?;

    // Collect ".field" suffixes
    let suffixes: Vec<String> = inner.map(|p| p.as_str().to_string()).collect();

    if suffixes.is_empty() {
        Ok(base)
    } else {
        Ok(Expr::new(
            ExprKind::PropertyAccess(Box::new(base), suffixes),
            span,
        ))
    }
}

fn parse_atom(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    let span = span_of(&pair, file_id);
    let inner = first_inner(pair)?;
    match inner.as_rule() {
        Rule::expr => parse_expr(inner, file_id),
        Rule::switch_expr => parse_switch_expr(inner, file_id),
        Rule::func_call => parse_func_call_expr(inner, file_id),
        Rule::hash_lit => parse_hash_lit(inner, file_id),
        Rule::array_lit => parse_array_lit(inner, file_id),
        Rule::float_lit => Ok(Expr::new(ExprKind::FloatLit(inner.as_str().parse()?), span)),
        Rule::integer_lit => Ok(Expr::new(ExprKind::IntLit(inner.as_str().parse()?), span)),
        Rule::string_lit => parse_string_lit_expr(&inner, file_id),
        Rule::bool_lit => Ok(Expr::new(ExprKind::BoolLit(inner.as_str() == "true"), span)),
        Rule::null_lit => Ok(Expr::new(ExprKind::Null, span)),
        Rule::ident_path => {
            let parts: Vec<String> = inner.into_inner().map(|p| p.as_str().to_string()).collect();
            Ok(Expr::new(ExprKind::Ident(parts), span))
        }
        _ => bail!("unexpected atom: {:?}", inner.as_rule()),
    }
}

/// Parse `switch expr { pattern { body } default { body } }` — the
/// expression-form switch. Distinct from the statement-form switch
/// inside process / pipeline bodies (which expect arm bodies to be
/// statement lists, not single expressions).
fn parse_switch_expr(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    let span = span_of(&pair, file_id);
    let mut inner = pair.into_inner();
    let scrutinee_pair = inner.next().unwrap();
    let scrutinee = parse_expr(scrutinee_pair, file_id)?;
    let mut arms = Vec::new();
    for arm_pair in inner {
        let arm_inner: Vec<Pair<Rule>> = arm_pair.into_inner().collect();
        match arm_inner.len() {
            // `default { expr }` — single expression child, no pattern
            1 => {
                arms.push(SwitchExprArm {
                    pattern: None,
                    body: parse_expr(arm_inner.into_iter().next().unwrap(), file_id)?,
                });
            }
            // `pattern { expr }` — two expression children: pattern, body
            2 => {
                let mut iter = arm_inner.into_iter();
                let pat = parse_expr(iter.next().unwrap(), file_id)?;
                let body = parse_expr(iter.next().unwrap(), file_id)?;
                arms.push(SwitchExprArm {
                    pattern: Some(pat),
                    body,
                });
            }
            n => bail!("malformed switch_expr_arm: expected 1 or 2 children, got {}", n),
        }
    }
    Ok(Expr::new(
        ExprKind::SwitchExpr {
            scrutinee: Box::new(scrutinee),
            arms,
        },
        span,
    ))
}

fn parse_func_call_expr(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    // Grammar has two alternatives:
    //   ident "." ident "(" args ")"   — namespaced:  inner = [ns, name, args?]
    //   ident "(" args ")"             — flat:        inner = [name, args?]
    // We peek at how many leading `ident` pairs there are before the
    // `func_args` child. This is simpler than threading a silent tag
    // through the grammar.
    let span = span_of(&pair, file_id);
    let mut inner: Vec<Pair<Rule>> = pair.into_inner().collect();

    // Last child (if present) is `func_args`. Strip it off first.
    let args_pair = if inner
        .last()
        .map(|p| p.as_rule() == Rule::func_args)
        .unwrap_or(false)
    {
        Some(inner.pop().unwrap())
    } else {
        None
    };

    let (namespace, name) = match inner.as_slice() {
        [name_pair] => (None, name_pair.as_str().to_string()),
        [ns_pair, name_pair] => (
            Some(ns_pair.as_str().to_string()),
            name_pair.as_str().to_string(),
        ),
        _ => bail!(
            "malformed func_call: expected 1 or 2 ident pairs, got {}",
            inner.len()
        ),
    };

    let args = if let Some(args_pair) = args_pair {
        parse_func_args(args_pair, file_id)?
    } else {
        vec![]
    };

    Ok(Expr::new(
        ExprKind::FuncCall {
            namespace,
            name,
            args,
        },
        span,
    ))
}

fn parse_func_args(pair: Pair<Rule>, file_id: u32) -> Result<Vec<Expr>> {
    pair.into_inner()
        .map(|p| parse_expr_from_pair(p, file_id))
        .collect()
}

fn parse_hash_lit(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    let span = span_of(&pair, file_id);
    let entries = pair
        .into_inner()
        .map(|entry| {
            let mut inner = entry.into_inner();
            let key = inner.next().unwrap().as_str().to_string();
            let value = parse_expr_from_pair(inner.next().unwrap(), file_id)?;
            Ok((key, value))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Expr::new(ExprKind::HashLit(entries), span))
}

/// Parse an `array_lit` rule — `[a, b, c]`, `[]`, `[1, 2, 3,]`.
///
/// Each inner pair is an `expr`. Order of inner pairs follows source
/// order, which we preserve in the resulting `Vec<Expr>`. Callers
/// should treat this ordering as construction convenience only; the
/// DSL model is positionless (no `arr[n]` syntax to expose the index).
fn parse_array_lit(pair: Pair<Rule>, file_id: u32) -> Result<Expr> {
    let span = span_of(&pair, file_id);
    let items = pair
        .into_inner()
        .map(|p| parse_expr_from_pair(p, file_id))
        .collect::<Result<Vec<_>>>()?;
    Ok(Expr::new(ExprKind::ArrayLit(items), span))
}

/// Extract a string literal as a plain `String`.
///
/// Used in contexts where `${expr}` interpolation isn't meaningful
/// (e.g. `include` paths, which resolve at config-load time with no
/// event available). Interpolation fragments are rendered as literal
/// `${...}` text so the user sees something useful in error messages
/// rather than a mysteriously-collapsed path.
fn parse_string_lit(pair: &Pair<Rule>) -> String {
    let mut out = String::new();
    for frag in pair.clone().into_inner() {
        match frag.as_rule() {
            Rule::string_plain => process_plain_into(&mut out, frag.as_str()),
            Rule::string_interp => {
                // Preserve the raw source form; callers that forbid
                // interpolation here should validate out-of-band.
                out.push_str(frag.as_str());
            }
            _ => {}
        }
    }
    out
}

/// Parse a string literal as an expression, producing either a plain
/// `StringLit` (no interpolation) or a `Template` (one or more `${expr}`).
fn parse_string_lit_expr(pair: &Pair<Rule>, file_id: u32) -> Result<Expr> {
    let span = span_of(pair, file_id);
    let mut fragments: Vec<TemplateFragment> = Vec::new();
    let mut current_literal = String::new();

    for frag in pair.clone().into_inner() {
        match frag.as_rule() {
            Rule::string_plain => process_plain_into(&mut current_literal, frag.as_str()),
            Rule::string_interp => {
                if !current_literal.is_empty() {
                    fragments.push(TemplateFragment::Literal(std::mem::take(
                        &mut current_literal,
                    )));
                }
                let expr_pair = frag
                    .into_inner()
                    .find(|p| p.as_rule() == Rule::expr)
                    .ok_or_else(|| anyhow::anyhow!("empty `${{}}` interpolation"))?;
                let expr = parse_expr(expr_pair, file_id)?;
                fragments.push(TemplateFragment::Interp(expr));
            }
            _ => {}
        }
    }
    if !current_literal.is_empty() {
        fragments.push(TemplateFragment::Literal(current_literal));
    }

    // If there's no interpolation, collapse to a plain StringLit so
    // existing code that matches on ExprKind::StringLit stays ergonomic.
    let has_interp = fragments
        .iter()
        .any(|f| matches!(f, TemplateFragment::Interp(_)));
    if has_interp {
        Ok(Expr::new(ExprKind::Template(fragments), span))
    } else {
        let combined = fragments
            .into_iter()
            .filter_map(|f| match f {
                TemplateFragment::Literal(s) => Some(s),
                TemplateFragment::Interp(_) => None,
            })
            .collect::<String>();
        Ok(Expr::new(ExprKind::StringLit(combined), span))
    }
}

/// Process a `string_plain` span (atomic in the grammar) into `out`,
/// resolving common backslash escape sequences. `\${` yields a literal
/// `${`; other unknown escapes are passed through as-is.
fn process_plain_into(out: &mut String, s: &str) {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('$') => out.push('$'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
}

fn parse_bin_op(pair: &Pair<Rule>) -> Result<BinOp> {
    match pair.as_str().trim() {
        "==" => Ok(BinOp::Eq),
        "!=" => Ok(BinOp::Ne),
        "<=" => Ok(BinOp::Le),
        ">=" => Ok(BinOp::Ge),
        "<" => Ok(BinOp::Lt),
        ">" => Ok(BinOp::Gt),
        "and" => Ok(BinOp::And),
        "or" => Ok(BinOp::Or),
        "+" => Ok(BinOp::Add),
        "-" => Ok(BinOp::Sub),
        "*" => Ok(BinOp::Mul),
        "/" => Ok(BinOp::Div),
        "%" => Ok(BinOp::Mod),
        other => bail!("unknown binary operator: {}", other),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn first_inner(pair: Pair<Rule>) -> Result<Pair<Rule>> {
    pair.into_inner()
        .next()
        .ok_or_else(|| anyhow::anyhow!("expected at least one inner pair in grammar rule"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_input_def() {
        let input = r#"
def input fw_syslog {
    type syslog_udp
    bind "0.0.0.0:514"
    format "rfc5424"
    rate_limit 10000
}
"#;
        let config = parse_config(input).unwrap();
        assert_eq!(config.definitions.len(), 1);
        match &config.definitions[0] {
            Definition::Input(def) => {
                assert_eq!(def.name, "fw_syslog");
                assert_eq!(def.properties.len(), 4);
            }
            _ => panic!("expected Input definition"),
        }
    }

    #[test]
    fn test_parse_output_with_nested_block() {
        let input = r#"
def output ama {
    type unix_socket
    path "/var/opt/ama/socket"
    queue {
        type disk
        path "/var/lib/limpid/queues/ama"
        max_size "1GB"
    }
}
"#;
        let config = parse_config(input).unwrap();
        assert_eq!(config.definitions.len(), 1);
        match &config.definitions[0] {
            Definition::Output(def) => {
                assert_eq!(def.name, "ama");
                // type, path, queue block
                assert_eq!(def.properties.len(), 3);
            }
            _ => panic!("expected Output definition"),
        }
    }

    #[test]
    fn test_parse_process_def() {
        let input = r#"
def process parse_and_enrich {
    process parse_cef

    if workspace.src != null {
        process geoip("src")
    }

    if workspace.device_vendor == "HealthCheck" {
        drop
    }
}
"#;
        let config = parse_config(input).unwrap();
        assert_eq!(config.definitions.len(), 1);
        match &config.definitions[0] {
            Definition::Process(def) => {
                assert_eq!(def.name, "parse_and_enrich");
                assert_eq!(def.body.len(), 3);
            }
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_pipeline_with_chain() {
        let input = r#"
def pipeline ama_forward {
    input external_tcp
    process filter_fortinet_traffic | ama_prep
    output ama
    drop
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Pipeline(def) => {
                assert_eq!(def.name, "ama_forward");
                assert_eq!(def.body.len(), 4);
                // Check process chain has 2 elements
                match &def.body[1] {
                    PipelineStatement::ProcessChain(chain) => {
                        assert_eq!(chain.len(), 2);
                    }
                    _ => panic!("expected ProcessChain"),
                }
            }
            _ => panic!("expected Pipeline definition"),
        }
    }

    #[test]
    fn test_parse_pipeline_with_switch() {
        let input = r#"
def pipeline splunk_archive {
    input external_udp

    switch source {
        "192.0.2.1" {
            output firepower01
            drop
        }
        default {
            drop
        }
    }
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Pipeline(def) => {
                assert_eq!(def.name, "splunk_archive");
                match &def.body[1] {
                    PipelineStatement::Switch(_, arms) => {
                        assert_eq!(arms.len(), 2);
                        assert!(arms[0].pattern.is_some());
                        assert!(arms[1].pattern.is_none()); // default
                    }
                    _ => panic!("expected Switch"),
                }
            }
            _ => panic!("expected Pipeline definition"),
        }
    }

    #[test]
    fn test_parse_inline_process() {
        let input = r#"
def pipeline test {
    input external_tcp
    process filter | {
        if contains(ingress, "CEF:") {
            workspace.kind = "cef"
        }
    }
    drop
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Pipeline(def) => match &def.body[1] {
                PipelineStatement::ProcessChain(chain) => {
                    assert_eq!(chain.len(), 2);
                    assert!(matches!(&chain[0], ProcessChainElement::Named(..)));
                    assert!(matches!(&chain[1], ProcessChainElement::Inline(..)));
                }
                _ => panic!("expected ProcessChain"),
            },
            _ => panic!("expected Pipeline definition"),
        }
    }

    #[test]
    fn test_parse_pipeline_single_input_backward_compat() {
        // The legacy single-input form stays valid and lands as a 1-element Vec.
        let src = r#"
def pipeline p {
    input a
    drop
}
"#;
        let config = parse_config(src).unwrap();
        match &config.definitions[0] {
            Definition::Pipeline(def) => match &def.body[0] {
                PipelineStatement::Input(names) => {
                    assert_eq!(names, &vec!["a".to_string()]);
                }
                _ => panic!("expected Input"),
            },
            _ => panic!("expected Pipeline"),
        }
    }

    #[test]
    fn test_parse_pipeline_fan_in_two_inputs() {
        let src = r#"
def pipeline ha {
    input syslog_a, syslog_b
    drop
}
"#;
        let config = parse_config(src).unwrap();
        match &config.definitions[0] {
            Definition::Pipeline(def) => match &def.body[0] {
                PipelineStatement::Input(names) => {
                    assert_eq!(names, &vec!["syslog_a".to_string(), "syslog_b".to_string()]);
                }
                _ => panic!("expected Input"),
            },
            _ => panic!("expected Pipeline"),
        }
    }

    #[test]
    fn test_parse_pipeline_fan_in_three_inputs() {
        let src = r#"
def pipeline fanin3 {
    input a, b, c
    drop
}
"#;
        let config = parse_config(src).unwrap();
        match &config.definitions[0] {
            Definition::Pipeline(def) => match &def.body[0] {
                PipelineStatement::Input(names) => {
                    assert_eq!(
                        names,
                        &vec!["a".to_string(), "b".to_string(), "c".to_string()]
                    );
                }
                _ => panic!("expected Input"),
            },
            _ => panic!("expected Pipeline"),
        }
    }

    #[test]
    fn test_parse_try_catch() {
        let input = r#"
def process strict_parse {
    try {
        process parse_cef
    } catch {
        drop
    }
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::TryCatch(try_body, catch_body) => {
                    assert_eq!(try_body.len(), 1);
                    assert_eq!(catch_body.len(), 1);
                }
                _ => panic!("expected TryCatch"),
            },
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_expressions() {
        let input = r#"
def process test {
    if workspace.syslog_severity <= 3 and workspace.alert == true {
        workspace.priority = "critical"
    }
}
"#;
        let config = parse_config(input).unwrap();
        assert!(matches!(&config.definitions[0], Definition::Process(..)));
    }

    #[test]
    fn test_parse_hash_literal() {
        let input = r#"
def process test {
    workspace.location = {
        ip: workspace.src,
        country: workspace.geo_country
    }
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    _,
                    Expr {
                        kind: ExprKind::HashLit(entries),
                        ..
                    },
                ) => {
                    assert_eq!(entries.len(), 2);
                }
                _ => panic!("expected Assign with HashLit"),
            },
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_array_literal_empty() {
        let input = r#"
def process test {
    workspace.types = []
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    _,
                    Expr {
                        kind: ExprKind::ArrayLit(items),
                        ..
                    },
                ) => {
                    assert_eq!(items.len(), 0);
                }
                _ => panic!("expected Assign with ArrayLit"),
            },
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_array_literal_items() {
        let input = r#"
def process test {
    workspace.types = [1, "two", true, null, workspace.a]
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    _,
                    Expr {
                        kind: ExprKind::ArrayLit(items),
                        ..
                    },
                ) => {
                    assert_eq!(items.len(), 5);
                    assert!(matches!(items[0].kind, ExprKind::IntLit(1)));
                    assert!(matches!(items[1].kind, ExprKind::StringLit(ref s) if s == "two"));
                    assert!(matches!(items[2].kind, ExprKind::BoolLit(true)));
                    assert!(matches!(items[3].kind, ExprKind::Null));
                    assert!(matches!(items[4].kind, ExprKind::Ident(_)));
                }
                _ => panic!("expected Assign with ArrayLit"),
            },
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_array_literal_trailing_comma() {
        let input = r#"
def process test {
    workspace.xs = [1, 2, 3,]
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    _,
                    Expr {
                        kind: ExprKind::ArrayLit(items),
                        ..
                    },
                ) => {
                    assert_eq!(items.len(), 3);
                }
                _ => panic!("expected Assign with ArrayLit"),
            },
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_array_literal_nested() {
        let input = r#"
def process test {
    workspace.grid = [[1, 2], [3, 4]]
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    _,
                    Expr {
                        kind: ExprKind::ArrayLit(rows),
                        ..
                    },
                ) => {
                    assert_eq!(rows.len(), 2);
                    for row in rows {
                        assert!(matches!(row.kind, ExprKind::ArrayLit(_)));
                    }
                }
                _ => panic!("expected Assign with nested ArrayLit"),
            },
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_array_literal_in_hash() {
        let input = r#"
def process test {
    workspace.finding = { title: "x", types: ["sqli", "xss"] }
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    _,
                    Expr {
                        kind: ExprKind::HashLit(entries),
                        ..
                    },
                ) => {
                    let (_, types_expr) = entries.iter().find(|(k, _)| k == "types").unwrap();
                    assert!(matches!(types_expr.kind, ExprKind::ArrayLit(ref xs) if xs.len() == 2));
                }
                _ => panic!("expected Assign with HashLit containing ArrayLit"),
            },
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_multiple_definitions() {
        let input = r#"
def input fw_syslog {
    type syslog_udp
    bind "0.0.0.0:514"
}

def process tag_critical {
    if workspace.syslog_severity <= 3 {
        workspace.alert = true
    }
}

def output debug_log {
    type file
    path "/var/log/limpid/debug.log"
}

def pipeline firewall {
    input fw_syslog
    process tag_critical
    output debug_log
    drop
}
"#;
        let config = parse_config(input).unwrap();
        assert_eq!(config.definitions.len(), 4);
    }

    #[test]
    fn test_parse_func_call_in_expr() {
        let input = r#"
def process test {
    egress = to_json()
    workspace.name = lower(workspace.name)
    workspace.src = regex_extract(ingress, "src=(\S+)")
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => {
                assert_eq!(def.body.len(), 3);
                match &def.body[0] {
                    ProcessStatement::Assign(
                        AssignTarget::Egress,
                        Expr {
                            kind:
                                ExprKind::FuncCall {
                                    namespace,
                                    name,
                                    args,
                                },
                            ..
                        },
                    ) => {
                        assert!(namespace.is_none());
                        assert_eq!(name, "to_json");
                        assert_eq!(args.len(), 0);
                    }
                    _ => panic!("expected Assign with FuncCall"),
                }
            }
            _ => panic!("expected Process definition"),
        }
    }

    #[test]
    fn test_parse_not_expression() {
        let input = r#"
def pipeline test {
    input fw
    if not contains(ingress, "traffic") {
        output log
    }
    drop
}
"#;
        let config = parse_config(input).unwrap();
        assert!(matches!(&config.definitions[0], Definition::Pipeline(..)));
    }

    // ---- String template interpolation --------------------------------

    fn property_value<'a>(props: &'a [Property], key: &str) -> &'a Expr {
        for p in props {
            if let Property::KeyValue {
                key: k, value: v, ..
            } = p
                && k == key
            {
                return v;
            }
        }
        panic!("property {} not found", key);
    }

    #[test]
    fn plain_string_parses_as_string_lit() {
        let input = r#"
def output sink {
    type file
    path "/var/log/app.log"
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Output(def) => match &property_value(&def.properties, "path").kind {
                ExprKind::StringLit(s) => assert_eq!(s, "/var/log/app.log"),
                other => panic!("expected StringLit, got {:?}", other),
            },
            _ => panic!("expected Output definition"),
        }
    }

    #[test]
    fn interpolated_string_parses_as_template() {
        let input = r#"
def output sink {
    type file
    path "/var/log/${source}/${workspace.date}.log"
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Output(def) => match &property_value(&def.properties, "path").kind {
                ExprKind::Template(frags) => {
                    assert_eq!(frags.len(), 5);
                    // /var/log/
                    assert!(matches!(&frags[0], TemplateFragment::Literal(s) if s == "/var/log/"));
                    // ${source}
                    assert!(matches!(
                        &frags[1],
                        TemplateFragment::Interp(Expr { kind: ExprKind::Ident(parts), .. }) if parts == &vec!["source".to_string()]
                    ));
                    // /
                    assert!(matches!(&frags[2], TemplateFragment::Literal(s) if s == "/"));
                    // ${workspace.date}
                    assert!(matches!(
                        &frags[3],
                        TemplateFragment::Interp(Expr { kind: ExprKind::Ident(parts), .. })
                            if parts == &vec!["workspace".to_string(), "date".to_string()]
                    ));
                    // .log
                    assert!(matches!(&frags[4], TemplateFragment::Literal(s) if s == ".log"));
                }
                other => panic!("expected Template, got {:?}", other),
            },
            _ => panic!("expected Output definition"),
        }
    }

    #[test]
    fn escaped_dollar_brace_is_literal() {
        // `\${x}` should render as literal `${x}` (no interpolation)
        let input = r#"
def output sink {
    type file
    path "literal-\${x}-here"
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Output(def) => match &property_value(&def.properties, "path").kind {
                ExprKind::StringLit(s) => assert_eq!(s, "literal-${x}-here"),
                other => panic!("expected StringLit, got {:?}", other),
            },
            _ => panic!("expected Output definition"),
        }
    }

    #[test]
    fn interpolation_inside_func_call_expression() {
        // `"[${source}] ${egress}"` — template with multiple identifiers
        let input = r#"
def process annotate {
    egress = "[${source}] ${egress}"
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    AssignTarget::Egress,
                    Expr {
                        kind: ExprKind::Template(frags),
                        ..
                    },
                ) => {
                    assert_eq!(frags.len(), 4);
                    assert!(matches!(&frags[0], TemplateFragment::Literal(s) if s == "["));
                    assert!(matches!(
                        &frags[1],
                        TemplateFragment::Interp(Expr { kind: ExprKind::Ident(parts), .. }) if parts == &vec!["source".to_string()]
                    ));
                    assert!(matches!(&frags[2], TemplateFragment::Literal(s) if s == "] "));
                    assert!(matches!(
                        &frags[3],
                        TemplateFragment::Interp(Expr { kind: ExprKind::Ident(parts), .. }) if parts == &vec!["egress".to_string()]
                    ));
                }
                other => panic!("expected template assignment, got {:?}", other),
            },
            _ => panic!("expected Process definition"),
        }
    }

    // ---- let bindings --------------------------------------------------

    #[test]
    fn test_parse_let_binding() {
        let input = r#"
def process p {
    let x = 7
    workspace.y = x
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => {
                assert_eq!(def.body.len(), 2);
                match &def.body[0] {
                    ProcessStatement::LetBinding(
                        name,
                        Expr {
                            kind: ExprKind::IntLit(7),
                            ..
                        },
                    ) => {
                        assert_eq!(name, "x");
                    }
                    other => panic!("expected LetBinding, got {:?}", other),
                }
                match &def.body[1] {
                    ProcessStatement::Assign(
                        AssignTarget::Workspace(path),
                        Expr {
                            kind: ExprKind::Ident(parts),
                            ..
                        },
                    ) => {
                        assert_eq!(path, &vec!["y".to_string()]);
                        assert_eq!(parts, &vec!["x".to_string()]);
                    }
                    other => panic!("expected assign, got {:?}", other),
                }
            }
            _ => panic!("expected process"),
        }
    }

    #[test]
    fn test_parse_let_with_function_call_rhs() {
        let input = r#"
def process p {
    let m = regex_extract(ingress, "src=(\\S+)")
    workspace.src = m
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::LetBinding(
                    name,
                    Expr {
                        kind:
                            ExprKind::FuncCall {
                                namespace,
                                name: fname,
                                args,
                            },
                        ..
                    },
                ) => {
                    assert_eq!(name, "m");
                    assert!(namespace.is_none());
                    assert_eq!(fname, "regex_extract");
                    assert_eq!(args.len(), 2);
                }
                other => panic!("expected LetBinding with FuncCall, got {:?}", other),
            },
            _ => panic!("expected process"),
        }
    }

    // ---- dot namespace syntax (Block 3) --------------------------------

    #[test]
    fn test_parse_namespaced_func_call() {
        // Grammar should accept `<ns>.<fn>(args)` and build a FuncCall
        // with `namespace = Some("syslog")`. Block 3 only adds the
        // syntax + registry — no syslog.* function is registered yet,
        // which is fine because parsing is structural.
        let input = r#"
def process p {
    let h = syslog.parse(ingress)
    workspace.h = h
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::LetBinding(
                    name,
                    Expr {
                        kind:
                            ExprKind::FuncCall {
                                namespace,
                                name: fname,
                                args,
                            },
                        ..
                    },
                ) => {
                    assert_eq!(name, "h");
                    assert_eq!(namespace.as_deref(), Some("syslog"));
                    assert_eq!(fname, "parse");
                    assert_eq!(args.len(), 1);
                }
                other => panic!("expected namespaced FuncCall, got {:?}", other),
            },
            _ => panic!("expected process"),
        }
    }

    #[test]
    fn test_parse_namespaced_func_call_no_args() {
        // Zero-arg form: `foo.bar()`.
        let input = r#"
def process p {
    let x = foo.bar()
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::LetBinding(
                    _,
                    Expr {
                        kind:
                            ExprKind::FuncCall {
                                namespace,
                                name,
                                args,
                            },
                        ..
                    },
                ) => {
                    assert_eq!(namespace.as_deref(), Some("foo"));
                    assert_eq!(name, "bar");
                    assert!(args.is_empty());
                }
                other => panic!("expected namespaced FuncCall, got {:?}", other),
            },
            _ => panic!("expected process"),
        }
    }

    #[test]
    fn test_namespace_prefix_does_not_break_property_access() {
        // Regression: `workspace.foo` (no trailing `(`) must still parse
        // as a property access / ident_path, not as a malformed func
        // call. The grammar falls through to ident_path when there's
        // no `(`.
        let input = r#"
def process p {
    workspace.foo = workspace.bar
}
"#;
        let config = parse_config(input).unwrap();
        match &config.definitions[0] {
            Definition::Process(def) => match &def.body[0] {
                ProcessStatement::Assign(
                    AssignTarget::Workspace(path),
                    Expr {
                        kind: ExprKind::Ident(parts),
                        ..
                    },
                ) => {
                    assert_eq!(path, &vec!["foo".to_string()]);
                    assert_eq!(parts, &vec!["workspace".to_string(), "bar".to_string()]);
                }
                other => panic!("expected workspace assign, got {:?}", other),
            },
            _ => panic!("expected process"),
        }
    }
}
