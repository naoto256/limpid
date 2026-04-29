//! Per-event bump arena for the DSL `Value` tree.
//!
//! Every event entering [`crate::pipeline::run_pipeline`] gets a fresh
//! `EventArena` whose lifetime ends when the event finishes processing.
//! All transient `Value::Object` / `Value::Array` / `Value::String` /
//! `Value::Bytes` payloads allocate from this arena, so the
//! per-allocation `drop_in_place<Value>` chain (~23% of allocator
//! samples on the v0.5.7 D pipeline baseline) collapses into a single
//! chunk-group free at event end.
//!
//! D pipeline: 46.3k → 168k eps/core across the v0.6.0 perf milestone
//! (see CHANGELOG entry for the breakdown).

pub use bumpalo::Bump;

/// Thin wrapper around [`bumpalo::Bump`] scoped to a single event's
/// lifetime through the pipeline.
///
/// The `'bump` lifetime parameter is the arena's own lifetime — every
/// reference handed out by the helpers below lives at least as long
/// as the wrapper. Hold the `EventArena` (or a reference to it)
/// wherever a `Value<'bump>` (post-1c) needs to be constructed.
#[derive(Debug)]
pub struct EventArena<'bump> {
    bump: &'bump Bump,
}

impl<'bump> EventArena<'bump> {
    /// Wrap an externally-owned [`Bump`]. The caller (typically
    /// [`crate::pipeline::run_pipeline`]) keeps the `Bump` on the
    /// stack so its drop coincides with event-end cleanup.
    #[inline]
    pub fn new(bump: &'bump Bump) -> Self {
        Self { bump }
    }

    /// Direct access to the underlying `Bump`. Use this for
    /// `bumpalo::collections::Vec::new_in(arena.bump())` and other
    /// `bumpalo::collections` constructors.
    #[inline]
    pub fn bump(&self) -> &'bump Bump {
        self.bump
    }

    /// Copy `s` into the arena and return a `&'bump str`.
    #[inline]
    pub fn alloc_str(&self, s: &str) -> &'bump str {
        self.bump.alloc_str(s)
    }

    /// Copy `b` into the arena as raw bytes.
    #[inline]
    pub fn alloc_bytes(&self, b: &[u8]) -> &'bump [u8] {
        self.bump.alloc_slice_copy(b)
    }
}

/// Test-only macro that introduces `bump` and `arena` bindings into the
/// caller's scope. Use at the top of any test that needs to call
/// arena-threaded APIs (`eval_expr`, `FunctionRegistry::call`, etc.).
///
/// ```ignore
/// #[test]
/// fn my_test() {
///     test_arena!();
///     // `arena` is now in scope.
///     eval_expr(&expr, &event, &funcs, &arena).unwrap();
/// }
/// ```
///
/// Implemented as a macro (rather than a `with_arena(|a| ...)` helper)
/// so test bodies don't need to be wrapped in a closure or pay an
/// extra indentation level.
#[cfg(test)]
#[macro_export]
macro_rules! test_arena {
    () => {
        let bump = $crate::dsl::arena::Bump::new();
        let arena = $crate::dsl::arena::EventArena::new(&bump);
    };
}
