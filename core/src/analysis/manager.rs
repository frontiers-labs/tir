//! Op-keyed analysis caching with MLIR-style invalidation.
//!
//! An [`Analysis`] is any type buildable from the IR rooted at an operation.
//! The [`AnalysisManager`] caches results per `(root op, analysis type)` and
//! hands them out as shared [`Rc`]s; an analysis fetches its dependencies
//! through the manager, so they are computed once and shared.
//!
//! Invalidation follows MLIR: after a pass runs, everything it did not
//! explicitly claim in its returned [`PreservedAnalyses`] is dropped from the
//! cache. An analysis whose validity also hinges on another analysis overrides
//! [`Analysis::is_invalidated`] to require its dependencies preserved too.
//! Invalidation is whole-cache rather than per-op: a pass mutating one
//! function drops results for every op, trading precision for simplicity.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::{Context, OpId};

/// A result computable from the IR rooted at an operation, cacheable by an
/// [`AnalysisManager`].
pub trait Analysis: Sized + 'static {
    /// Build the analysis for the IR rooted at `op`. Fetch dependencies through
    /// `analyses` so they are cached and shared.
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self;

    /// Whether a cached result went stale given what a pass preserved. The
    /// default survives only when this analysis itself was preserved; override
    /// to additionally require dependencies.
    fn is_invalidated(&self, preserved: &PreservedAnalyses) -> bool {
        !preserved.is_preserved::<Self>()
    }
}

/// The analyses a pass left intact, reported from [`crate::Pass::run`]. Start
/// from [`Self::none`] and [`Self::preserve`] what the pass provably kept
/// valid, or return [`Self::all`] when the IR was not touched.
#[derive(Default)]
pub struct PreservedAnalyses {
    all: bool,
    preserved: HashSet<TypeId>,
}

impl PreservedAnalyses {
    /// Nothing survives: the safe report for any pass that mutated the IR.
    pub fn none() -> Self {
        Self::default()
    }

    /// Everything survives: the report for a pass that changed nothing.
    pub fn all() -> Self {
        Self {
            all: true,
            preserved: HashSet::new(),
        }
    }

    /// Mark `A` as still valid.
    pub fn preserve<A: Analysis>(mut self) -> Self {
        self.preserved.insert(TypeId::of::<A>());
        self
    }

    pub fn is_preserved<A: Analysis>(&self) -> bool {
        self.all || self.preserved.contains(&TypeId::of::<A>())
    }
}

struct CacheEntry {
    result: Rc<dyn Any>,
    /// Monomorphized `A::is_invalidated`, so invalidation can consult the
    /// type-erased entry.
    is_invalidated: fn(&dyn Any, &PreservedAnalyses) -> bool,
}

/// Caches analysis results per `(root op, analysis type)` for one pass-manager
/// run. `Rc` keeps the manager single-threaded, matching the sequential pass
/// pipeline.
#[derive(Default)]
pub struct AnalysisManager {
    // `RefCell` lets `Analysis::build` recursively fetch dependencies through
    // the same `&self`.
    cache: RefCell<HashMap<(OpId, TypeId), CacheEntry>>,
}

impl AnalysisManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// The analysis for `op`, computed on first request.
    pub fn get<A: Analysis>(&self, context: &Context, op: impl Into<OpId>) -> Rc<A> {
        let op = op.into();
        let key = (op, TypeId::of::<A>());
        if let Some(entry) = self.cache.borrow().get(&key) {
            return entry
                .result
                .clone()
                .downcast()
                .expect("cache entry type matches its key");
        }

        // Built without holding the borrow so `build` can fetch dependencies.
        let result = Rc::new(A::build(self, context, op));
        self.cache.borrow_mut().insert(
            key,
            CacheEntry {
                result: result.clone(),
                is_invalidated: |result, preserved| {
                    result
                        .downcast_ref::<A>()
                        .expect("cache entry type matches its key")
                        .is_invalidated(preserved)
                },
            },
        );
        result
    }

    /// The analysis for `op` if it is already cached; never computes.
    pub fn get_cached<A: Analysis>(&self, op: impl Into<OpId>) -> Option<Rc<A>> {
        let key = (op.into(), TypeId::of::<A>());
        self.cache.borrow().get(&key).map(|entry| {
            entry
                .result
                .clone()
                .downcast()
                .expect("cache entry type matches its key")
        })
    }

    /// Drop every cached result invalidated under `preserved`.
    pub fn invalidate(&self, preserved: &PreservedAnalyses) {
        if preserved.all {
            return;
        }
        self.cache
            .borrow_mut()
            .retain(|_, entry| !(entry.is_invalidated)(entry.result.as_ref(), preserved));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Operation, builtin::ops};

    struct Simple;

    impl Analysis for Simple {
        fn build(_: &AnalysisManager, _: &Context, _: OpId) -> Self {
            Simple
        }
    }

    /// Depends on [`Simple`]: built through the manager, invalidated with it.
    struct Dependent;

    impl Analysis for Dependent {
        fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
            analyses.get::<Simple>(context, op);
            Dependent
        }

        fn is_invalidated(&self, preserved: &PreservedAnalyses) -> bool {
            !preserved.is_preserved::<Self>() || !preserved.is_preserved::<Simple>()
        }
    }

    fn test_op(context: &Context) -> OpId {
        ops::module(context, None).build().id()
    }

    #[test]
    fn caches_per_op() {
        let context = Context::with_default_dialects();
        let a = test_op(&context);
        let b = test_op(&context);
        let am = AnalysisManager::new();

        let first = am.get::<Simple>(&context, a);
        assert!(Rc::ptr_eq(&first, &am.get::<Simple>(&context, a)));
        assert!(!Rc::ptr_eq(&first, &am.get::<Simple>(&context, b)));
    }

    #[test]
    fn get_cached_never_computes() {
        let context = Context::with_default_dialects();
        let op = test_op(&context);
        let am = AnalysisManager::new();

        assert!(am.get_cached::<Simple>(op).is_none());
        let built = am.get::<Simple>(&context, op);
        assert!(Rc::ptr_eq(&built, &am.get_cached::<Simple>(op).unwrap()));
    }

    #[test]
    fn invalidation_respects_preserved_set() {
        let context = Context::with_default_dialects();
        let op = test_op(&context);
        let am = AnalysisManager::new();
        let first = am.get::<Simple>(&context, op);

        am.invalidate(&PreservedAnalyses::all());
        assert!(Rc::ptr_eq(&first, &am.get::<Simple>(&context, op)));

        am.invalidate(&PreservedAnalyses::none().preserve::<Simple>());
        assert!(Rc::ptr_eq(&first, &am.get::<Simple>(&context, op)));

        am.invalidate(&PreservedAnalyses::none());
        assert!(am.get_cached::<Simple>(op).is_none());
    }

    #[test]
    fn dependency_populates_cache_and_invalidates_together() {
        let context = Context::with_default_dialects();
        let op = test_op(&context);
        let am = AnalysisManager::new();

        am.get::<Dependent>(&context, op);
        assert!(am.get_cached::<Simple>(op).is_some());

        // Preserving the dependent alone is not enough: its dependency died.
        am.invalidate(&PreservedAnalyses::none().preserve::<Dependent>());
        assert!(am.get_cached::<Dependent>(op).is_none());
    }
}
