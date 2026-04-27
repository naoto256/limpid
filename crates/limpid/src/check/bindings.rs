//! Name-resolution scope used by the analyzer.
//!
//! `Bindings` tracks every identifier the analyzer can see at a given
//! pipeline point: reserved event idents (always present, fixed type),
//! `workspace.*` paths (added by parser merges and explicit
//! `workspace.x = expr` assignments), and `let` locals (process-body
//! scratch slots). All three live in one structure so name lookup is a
//! single call regardless of which surface the user is reaching for.
//!
//! Scope semantics:
//!
//! - `workspace.*` survives across process bodies in a pipeline. A
//!   binding from `parse_syslog` in one process is visible from a later
//!   process.
//! - `let` locals are process-body scoped: introduced by `let x = ...`,
//!   shadow earlier `let`s of the same name, dropped at the end of the
//!   process body. The analyzer mirrors this with [`Bindings::push_let_scope`]
//!   / [`Bindings::pop_let_scope`].
//! - Branch joins (`if`/`switch`/`try`/`catch`) intersect bindings:
//!   only paths bound on every branch survive, with their types
//!   merged via [`FieldType::union`].
//!
//! The analyzer never rejects unknown `workspace.*` reads outright (the
//! runtime returns `Null`), but Phase 2 emits a warning when an output
//! template references a workspace key that no upstream module produces.

use std::collections::HashMap;

use crate::modules::schema::FieldType;

/// Single source of truth for "what name resolves to what type" at a
/// particular point in the pipeline.
#[derive(Debug, Clone, Default)]
pub struct Bindings {
    /// `workspace.*` paths joined with '.', e.g. `"workspace.syslog_msg"`.
    workspace: HashMap<String, FieldType>,
    /// `true` after a wildcarded parser ran with no defaults — the
    /// analyzer can no longer tell which workspace keys exist, so any
    /// `workspace.*` read is admitted without flagging.
    wildcard: bool,
    /// Stack of `let` scopes; the innermost (top) scope wins on lookup.
    let_scopes: Vec<HashMap<String, FieldType>>,
}

impl Bindings {
    pub fn new() -> Self {
        Self::default()
    }

    // ----- workspace -------------------------------------------------------

    /// Bind `workspace.<path>` to `ty`. If the path was already bound,
    /// the new type replaces the old. Callers emit any type-conflict
    /// diagnostic separately — `Bindings` only stores facts.
    pub fn bind_workspace(&mut self, path: &[String], ty: FieldType) {
        self.workspace.insert(path.join("."), ty);
    }

    /// Get the type of a `workspace.<path>` if bound. Does not consult
    /// the wildcard flag — callers that want "is this readable at all?"
    /// should use [`Bindings::workspace_visible`].
    pub fn get_workspace(&self, path: &[String]) -> Option<&FieldType> {
        self.workspace.get(&path.join("."))
    }

    /// Iterate over every bound `workspace.*` key in dotted form
    /// (`"workspace.foo.bar"`). Used by the suggestion engine to find
    /// near-match candidates for unbound references.
    pub fn workspace_keys(&self) -> impl Iterator<Item = &String> {
        self.workspace.keys()
    }

    /// True when `workspace.<path>` is either explicitly bound, bound
    /// via an ancestor (Object), or admitted by the wildcard flag.
    pub fn workspace_visible(&self, path: &[String]) -> bool {
        if self.wildcard {
            return true;
        }
        let joined = path.join(".");
        self.workspace
            .keys()
            .any(|p| p == &joined || joined.starts_with(&format!("{}.", p)))
    }

    /// Mark the workspace as wildcarded — the analyzer no longer knows
    /// which keys exist, so subsequent `workspace.*` reads are admitted
    /// silently. Used after a parser whose output set is data-driven
    /// (parse_json, parse_kv) ran without HashLit defaults.
    pub fn set_workspace_wildcard(&mut self) {
        self.wildcard = true;
    }

    #[allow(dead_code)] // currently consulted only from tests; kept on the
                        // public surface because the wildcard flag is part
                        // of the binding-state contract (parser-effect
                        // analysis flips it; downstream readers may grow
                        // uses for it).
    pub fn is_workspace_wildcard(&self) -> bool {
        self.wildcard
    }

    /// Drop every workspace binding (used for full-replacement
    /// assignments — currently unreachable in v0.3.0 since `workspace =
    /// expr` isn't a parser-allowed form, but kept for symmetry with
    /// the rest of the API and for future dot-namespace flexibility).
    #[allow(dead_code)] // reserved for `workspace = expr` full-replacement (future)
    pub fn clear_workspace(&mut self) {
        self.workspace.clear();
        self.wildcard = false;
    }

    // ----- let scopes ------------------------------------------------------

    /// Open a new `let` scope. Call at the start of every process body /
    /// branch where introduced lets must not leak.
    pub fn push_let_scope(&mut self) {
        self.let_scopes.push(HashMap::new());
    }

    /// Close the innermost `let` scope. Bindings introduced since the
    /// matching `push_let_scope` are dropped.
    pub fn pop_let_scope(&mut self) {
        self.let_scopes.pop();
    }

    /// Bind a `let name = expr` in the innermost scope. Shadows any
    /// outer scope binding of the same name for the remainder of this
    /// scope's lifetime.
    pub fn bind_let(&mut self, name: &str, ty: FieldType) {
        if let Some(top) = self.let_scopes.last_mut() {
            top.insert(name.to_string(), ty);
        }
        // No active scope is a programming error at the analyzer level;
        // we silently drop rather than panic so a malformed traversal
        // doesn't crash the whole check.
    }

    /// Look up a `let` binding in the active scopes. Innermost first.
    pub fn get_let(&self, name: &str) -> Option<&FieldType> {
        self.let_scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
    }

    // ----- branch intersection --------------------------------------------

    /// Intersect this `Bindings` with `other`, keeping only workspace
    /// paths present in both and unioning their types. Used at the
    /// join point of every control-flow construct (if/switch/try-catch).
    ///
    /// `let` scopes are not intersected — branches inside a process
    /// body share the surrounding `let` scope; nothing introduced
    /// inside a branch escapes.
    pub fn intersect_with(&mut self, other: &Self) {
        let mut merged: HashMap<String, FieldType> = HashMap::new();
        for (k, t_self) in &self.workspace {
            if let Some(t_other) = other.workspace.get(k) {
                merged.insert(k.clone(), FieldType::union(t_self.clone(), t_other.clone()));
            }
        }
        self.workspace = merged;
        // Wildcard only survives if both sides wildcarded — otherwise
        // the precise side's known keys win.
        self.wildcard = self.wildcard && other.wildcard;
    }
}

/// Intersect a set of branch outcomes into a single result. If the
/// slice is empty, returns an empty `Bindings`.
pub fn intersect_branches(branches: &[Bindings]) -> Bindings {
    let mut iter = branches.iter();
    let Some(first) = iter.next() else {
        return Bindings::new();
    };
    let mut result = first.clone();
    for b in iter {
        result.intersect_with(b);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_bind_and_lookup() {
        let mut b = Bindings::new();
        b.bind_workspace(&["workspace".into(), "x".into()], FieldType::String);
        assert_eq!(
            b.get_workspace(&["workspace".into(), "x".into()]),
            Some(&FieldType::String)
        );
        assert!(b.workspace_visible(&["workspace".into(), "x".into()]));
    }

    #[test]
    fn workspace_ancestor_visibility() {
        // Binding `workspace.user` as Object makes nested reads pass
        // visibility checks — the analyzer can't know the inner shape.
        let mut b = Bindings::new();
        b.bind_workspace(&["workspace".into(), "user".into()], FieldType::Object);
        assert!(b.workspace_visible(&["workspace".into(), "user".into(), "id".into()]));
    }

    #[test]
    fn wildcard_admits_anything() {
        let mut b = Bindings::new();
        b.set_workspace_wildcard();
        assert!(b.workspace_visible(&["workspace".into(), "anything".into()]));
    }

    #[test]
    fn let_scope_lifecycle() {
        let mut b = Bindings::new();
        b.push_let_scope();
        b.bind_let("x", FieldType::String);
        assert_eq!(b.get_let("x"), Some(&FieldType::String));
        b.pop_let_scope();
        assert_eq!(b.get_let("x"), None);
    }

    #[test]
    fn let_inner_shadows_outer() {
        let mut b = Bindings::new();
        b.push_let_scope();
        b.bind_let("x", FieldType::String);
        b.push_let_scope();
        b.bind_let("x", FieldType::Int);
        assert_eq!(b.get_let("x"), Some(&FieldType::Int));
        b.pop_let_scope();
        assert_eq!(b.get_let("x"), Some(&FieldType::String));
    }

    #[test]
    fn intersect_keeps_only_common_paths_and_unions_types() {
        let mut a = Bindings::new();
        a.bind_workspace(&["workspace".into(), "shared".into()], FieldType::Int);
        a.bind_workspace(&["workspace".into(), "only_a".into()], FieldType::String);

        let mut b = Bindings::new();
        b.bind_workspace(&["workspace".into(), "shared".into()], FieldType::String);
        b.bind_workspace(&["workspace".into(), "only_b".into()], FieldType::Bool);

        a.intersect_with(&b);
        assert_eq!(
            a.get_workspace(&["workspace".into(), "shared".into()]),
            Some(&FieldType::Union(vec![FieldType::Int, FieldType::String]))
        );
        assert!(
            a.get_workspace(&["workspace".into(), "only_a".into()])
                .is_none()
        );
        assert!(
            a.get_workspace(&["workspace".into(), "only_b".into()])
                .is_none()
        );
    }

    #[test]
    fn wildcard_intersects_to_precise_side() {
        let mut a = Bindings::new();
        a.set_workspace_wildcard();

        let mut b = Bindings::new();
        b.bind_workspace(&["workspace".into(), "x".into()], FieldType::String);

        a.intersect_with(&b);
        assert!(!a.is_workspace_wildcard());
        // The wildcard side had no concrete bindings to intersect with,
        // so the precise side's `workspace.x` is dropped — paths must
        // be bound on *both* sides.
        assert!(a.get_workspace(&["workspace".into(), "x".into()]).is_none());
    }
}
