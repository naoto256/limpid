//! Levenshtein-based "did you mean ...?" suggestions for analyzer diagnostics.
//!
//! When the analyzer flags an unbound `workspace.*` reference or an
//! unknown function call, it consults this module for the closest known
//! candidate. Threshold is `max(2, len/3)` so short identifiers tolerate
//! at most two typos, longer ones get more latitude. We never suggest
//! when nothing's close — silence beats a wrong guess.

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
    let mut candidates: Vec<String> = bindings.workspace_keys().cloned().collect();
    // Reserved event idents are always present — surface them too in
    // case the user wrote `workspace.ingress` etc. (a common pattern
    // confusion).
    for r in ["ingress", "egress", "source", "received_at", "error"] {
        candidates.push(r.to_string());
    }
    pick_best(needle, &candidates)
}

/// Find the closest registered function name (flat namespace only) to
/// `needle`. Namespaced typos (`foo.bar`) are out of scope for this
/// pass — user-defined namespaces aren't enumerable here, and we'd
/// rather emit nothing than a misleading flat-namespace guess.
pub fn near_function_name(needle: &str, registry: &FunctionRegistry) -> Option<String> {
    let candidates: Vec<String> = registry.flat_function_names().collect();
    pick_best(needle, &candidates)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn pick_best(needle: &str, candidates: &[String]) -> Option<String> {
    let threshold = (needle.len() / 3).max(2);
    let mut best: Option<(String, usize)> = None;
    for cand in candidates {
        if cand == needle {
            // Exact match — by construction we only get here when the
            // analyzer already concluded the name is missing, so this
            // branch is defensive (e.g. case-collisions on case-sensitive
            // platforms). Skip rather than recommend the same string.
            continue;
        }
        let d = levenshtein(needle, cand);
        if d > threshold {
            continue;
        }
        match &best {
            None => best = Some((cand.clone(), d)),
            Some((bname, bd)) => {
                // Prefer lower distance; tie-break alphabetically for
                // deterministic test output.
                if d < *bd || (d == *bd && cand < bname) {
                    best = Some((cand.clone(), d));
                }
            }
        }
    }
    best.map(|(n, _)| n)
}

/// Standard Levenshtein edit distance. O(|a|*|b|) time, O(min(|a|,|b|))
/// memory. Case-sensitive on purpose — DSL identifiers are
/// case-sensitive and `User` vs `user` is meaningful.
pub fn levenshtein(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.is_empty() {
        return b_chars.len();
    }
    if b_chars.is_empty() {
        return a_chars.len();
    }
    let (short, long) = if a_chars.len() <= b_chars.len() {
        (&a_chars, &b_chars)
    } else {
        (&b_chars, &a_chars)
    };
    let mut prev: Vec<usize> = (0..=short.len()).collect();
    let mut curr = vec![0usize; short.len() + 1];
    for (i, lc) in long.iter().enumerate() {
        curr[0] = i + 1;
        for (j, sc) in short.iter().enumerate() {
            let cost = if lc == sc { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[short.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("foo", "foo"), 0);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", ""), 3);
        assert_eq!(levenshtein("hostname", "hostnmae"), 2);
    }

    #[test]
    fn pick_best_prefers_closer_then_alpha() {
        let cands = vec!["alpha".to_string(), "beta".to_string(), "alphz".to_string()];
        // "alphq" → "alpha" and "alphz" both d=1; alphabetic tie-break wins
        assert_eq!(pick_best("alphq", &cands), Some("alpha".to_string()));
    }

    #[test]
    fn pick_best_silent_when_too_far() {
        let cands = vec!["completely_different".to_string()];
        assert_eq!(pick_best("foo", &cands), None);
    }

    #[test]
    fn pick_best_threshold_scales_with_length() {
        // length 12 / 3 = 4 → tolerates four typos
        let cands = vec!["abcdefghijkl".to_string()];
        assert_eq!(
            pick_best("abcdefqrstkl", &cands),
            Some("abcdefghijkl".to_string())
        );
    }
}
