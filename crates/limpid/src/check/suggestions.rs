//! Levenshtein-based "did you mean ...?" suggestions for analyzer diagnostics.
//!
//! When the analyzer flags an unbound `workspace.*` reference or an
//! unknown function call, it consults this module for the closest known
//! candidate. Threshold and tie-break rules live in
//! [`crate::dsl::schema::nearest`] — this module is a thin domain
//! adapter that hands the appropriate candidate set to that routine
//! (workspace bindings for ident typos, function-registry names for
//! call-site typos).

use crate::dsl::schema::nearest;
use crate::functions::FunctionRegistry;

use super::bindings::Bindings;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Find the closest currently-bound `workspace.*` path to `needle`. The
/// `needle` should be the full dotted form (`workspace.foo.bar`); we
/// match against the bindings' stored joined-form keys. Returns `None`
/// when nothing falls within the typo threshold.
pub fn near_workspace_path(needle: &str, bindings: &Bindings) -> Option<String> {
    let workspace: Vec<String> = bindings.workspace_keys().cloned().collect();
    // Reserved event idents are always present — surface them too in
    // case the user wrote `workspace.ingress` etc. (a common pattern
    // confusion).
    let reserved = ["ingress", "egress", "source", "received_at", "error"];
    nearest(
        needle,
        workspace
            .iter()
            .map(|s| s.as_str())
            .chain(reserved.iter().copied()),
    )
}

/// Find the closest registered function name (flat namespace only) to
/// `needle`. Namespaced typos (`foo.bar`) are out of scope for this
/// pass — user-defined namespaces aren't enumerable here, and we'd
/// rather emit nothing than a misleading flat-namespace guess.
pub fn near_function_name(needle: &str, registry: &FunctionRegistry) -> Option<String> {
    let candidates: Vec<String> = registry.flat_function_names().collect();
    nearest(needle, candidates.iter().map(|s| s.as_str()))
}

#[cfg(test)]
mod tests {
    use crate::dsl::schema::nearest;

    #[test]
    fn nearest_prefers_closer_then_alpha() {
        let cands = ["alpha", "beta", "alphz"];
        // "alphq" → "alpha" and "alphz" both d=1; alphabetic tie-break wins
        assert_eq!(
            nearest("alphq", cands.iter().copied()),
            Some("alpha".to_string())
        );
    }

    #[test]
    fn nearest_silent_when_too_far() {
        let cands = ["completely_different"];
        assert_eq!(nearest("foo", cands.iter().copied()), None);
    }

    #[test]
    fn nearest_threshold_scales_with_length() {
        // length 12 / 3 = 4 → tolerates four typos
        let cands = ["abcdefghijkl"];
        assert_eq!(
            nearest("abcdefqrstkl", cands.iter().copied()),
            Some("abcdefghijkl".to_string())
        );
    }
}
