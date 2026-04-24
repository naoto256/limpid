//! AST types for the limpid DSL.
//!
//! Top-level structure: a config file is a sequence of `Definition`s.
//! Each definition is one of: input, output, process, or pipeline.

/// A complete configuration file.
#[derive(Debug, Clone)]
pub struct Config {
    pub definitions: Vec<Definition>,
    /// Global config blocks (e.g. `geoip { ... }`, `control { ... }`)
    pub global_blocks: Vec<GlobalBlock>,
    /// Include directives (e.g. `include "inputs/*.limpid"`)
    /// Populated by the parser, consumed and cleared by the config loader.
    pub includes: Vec<String>,
}

/// A top-level block without `def` keyword (global configuration).
#[derive(Debug, Clone)]
pub struct GlobalBlock {
    pub name: String,
    pub properties: Vec<Property>,
}

/// A top-level `def` statement.
#[derive(Debug, Clone)]
pub enum Definition {
    Input(InputDef),
    Output(OutputDef),
    Process(ProcessDef),
    Pipeline(PipelineDef),
}

// ---------------------------------------------------------------------------
// Input / Output definitions (declarative key-value)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct InputDef {
    pub name: String,
    pub properties: Vec<Property>,
}

#[derive(Debug, Clone)]
pub struct OutputDef {
    pub name: String,
    pub properties: Vec<Property>,
}

/// A key-value property or nested block inside input/output definitions.
#[derive(Debug, Clone)]
pub enum Property {
    /// `key value`  e.g. `type syslog_udp`, `bind "0.0.0.0:514"`
    KeyValue(String, Expr),
    /// `key { ... }` e.g. `tls { cert "..." }`, `queue { type disk }`
    Block(String, Vec<Property>),
}

// ---------------------------------------------------------------------------
// Process definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ProcessDef {
    pub name: String,
    pub body: Vec<ProcessStatement>,
}

/// Statements that can appear inside a process body.
#[derive(Debug, Clone)]
pub enum ProcessStatement {
    /// `workspace.xxx = expr`, `egress = expr`, `severity = expr`, etc.
    Assign(AssignTarget, Expr),
    /// `let <name> = expr` — introduce or shadow a local scratch binding
    /// visible for the rest of the enclosing process body. Locals are
    /// bare identifiers (`name`), distinct from `workspace.name`.
    LetBinding(String, Expr),
    /// `process name` or `process name(args...)`
    ProcessCall(String, Vec<Expr>),
    /// `drop`
    Drop,
    /// `if cond { ... } else if cond { ... } else { ... }`
    If(IfChain),
    /// `switch expr { "val" { ... } default { ... } }`
    Switch(Expr, Vec<SwitchArm>),
    /// `try { ... } catch { ... }`
    TryCatch(Vec<ProcessStatement>, Vec<ProcessStatement>),
    /// `foreach field_expr { ... }`
    ForEach(Expr, Vec<ProcessStatement>),
    /// Expression statement: `table_upsert(...)`, `table_delete(...)`, etc.
    /// Evaluates the expression and discards the result.
    ExprStmt(Expr),
}

// ---------------------------------------------------------------------------
// Pipeline definition
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct PipelineDef {
    pub name: String,
    pub body: Vec<PipelineStatement>,
}

/// Statements that can appear inside a pipeline body.
#[derive(Debug, Clone)]
pub enum PipelineStatement {
    /// `input name`
    Input(String),
    /// `process name1 | name2 | { ... }` — a chain of process references and inline blocks
    ProcessChain(Vec<ProcessChainElement>),
    /// `output name`
    Output(String),
    /// `drop` — explicit discard (counted as events_dropped)
    Drop,
    /// `finish` — explicit success termination (counted as events_finished)
    Finish,
    /// `if cond { ... } else if cond { ... } else { ... }`
    If(IfChain),
    /// `switch expr { ... }`
    Switch(Expr, Vec<SwitchArm>),
}

/// An element within a `process a | b | { ... }` chain in a pipeline.
#[derive(Debug, Clone)]
pub enum ProcessChainElement {
    /// Named process reference, optionally with arguments: `parse_cef`, `geoip("source")`
    Named(String, Vec<Expr>),
    /// Inline (anonymous) process block: `{ ... }`
    Inline(Vec<ProcessStatement>),
}

// ---------------------------------------------------------------------------
// Shared constructs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IfChain {
    /// (condition, body) pairs for `if` and `else if` branches
    pub branches: Vec<(Expr, Vec<BranchBody>)>,
    /// Optional `else` branch
    pub else_body: Option<Vec<BranchBody>>,
}

/// Branch body can contain either process-level or pipeline-level statements
/// depending on context. We use an enum to unify.
#[derive(Debug, Clone)]
pub enum BranchBody {
    Process(ProcessStatement),
    Pipeline(PipelineStatement),
}

#[derive(Debug, Clone)]
pub struct SwitchArm {
    /// `None` for `default`
    pub pattern: Option<Expr>,
    pub body: Vec<BranchBody>,
}

// ---------------------------------------------------------------------------
// Assign targets
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum AssignTarget {
    /// `egress`
    Egress,
    /// `severity`
    Severity,
    /// `facility`
    Facility,
    /// `workspace.xxx` or `workspace.xxx.yyy`
    Workspace(Vec<String>),
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Expr {
    /// String literal without interpolation: `"hello"`
    StringLit(String),
    /// String literal with `${expr}` interpolation: `"/log/${source}.log"`.
    /// When no interpolations are present, the parser emits `StringLit`
    /// instead, so reaching `Template` guarantees at least one `Interp`.
    Template(Vec<TemplateFragment>),
    /// Integer literal: `42`
    IntLit(i64),
    /// Float literal: `3.14`
    FloatLit(f64),
    /// Boolean literal: `true`, `false`
    BoolLit(bool),
    /// Null literal
    Null,
    /// Identifier or dotted path: `severity`, `workspace.src`, `source`, `error`
    Ident(Vec<String>),
    /// Function call.
    ///
    /// `namespace = None` is the flat primitive form (`parse_json(x)`,
    /// `lower(workspace.name)`). `namespace = Some("syslog")` is the
    /// dot-namespaced form (`syslog.parse(ingress)`) introduced in
    /// v0.3.0 Block 3; the registry dispatches on `(namespace, name)`.
    FuncCall {
        namespace: Option<String>,
        name: String,
        args: Vec<Expr>,
    },
    /// Binary operation: `a == b`, `a and b`, `a + b`, etc.
    BinOp(Box<Expr>, BinOp, Box<Expr>),
    /// Unary operation: `not expr`
    UnaryOp(UnaryOp, Box<Expr>),
    /// Hash literal: `{ key: value, key2: value2 }`
    HashLit(Vec<(String, Expr)>),
    /// Postfix property access: `geoip(x).country.name`
    PropertyAccess(Box<Expr>, Vec<String>),
}

#[derive(Debug, Clone)]
pub enum TemplateFragment {
    /// Literal text between interpolations (after escape processing).
    Literal(String),
    /// `${expr}` interpolation — evaluated against the event at render time.
    Interp(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    // Comparison
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    // Logical
    And,
    Or,
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}
