//! Op-keyed analysis caching, invalidated by version stamps.
//!
//! An [`Analysis`] is any type buildable from the IR rooted at an operation.
//! The [`AnalysisManager`] caches results per `(root op, analysis type)` and
//! hands them out as shared [`Rc`]s; an analysis fetches its dependencies
//! through the manager, so they are computed once and shared.
//!
//! A cached result records the [`Context::op_version`] it was built at and is
//! valid exactly while that version still stands. Since every structural edit
//! bumps the versions of the edited op and all of its ancestors, an analysis
//! keyed on a function root goes stale on any edit anywhere inside it, and on no
//! other edit. Dependencies need no bookkeeping: an analysis and everything it
//! built through the manager share the op, hence the version. Each
//! `(op, analysis)` holds at most one entry, replaced in place when it goes
//! stale, so stale results never accumulate.

use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{Context, OpId};

/// A result computable from the IR rooted at an operation, cacheable by an
/// [`AnalysisManager`].
pub trait Analysis: Sized + 'static {
    /// Build the analysis for the IR rooted at `op`. Fetch dependencies through
    /// `analyses` so they are cached and shared.
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self;
}

struct CacheEntry {
    result: Rc<dyn Any>,
    /// The op version this result describes.
    version: u32,
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

    /// The analysis for `op`, computed on first request and after any edit to
    /// `op`'s subtree.
    pub fn get<A: Analysis>(&self, context: &Context, op: impl Into<OpId>) -> Rc<A> {
        let op = op.into();
        let version = context.op_version(op);
        if let Some(result) = self.lookup::<A>(op, version) {
            return result;
        }

        // Built without holding the borrow so `build` can fetch dependencies.
        let result = Rc::new(A::build(self, context, op));
        // `build` may have edited nothing, but re-read the version rather than
        // trusting the pre-build one.
        self.cache.borrow_mut().insert(
            (op, TypeId::of::<A>()),
            CacheEntry {
                result: result.clone(),
                version: context.op_version(op),
            },
        );
        result
    }

    /// The analysis for `op` if a current result is cached; never computes.
    pub fn get_cached<A: Analysis>(&self, context: &Context, op: impl Into<OpId>) -> Option<Rc<A>> {
        let op = op.into();
        self.lookup::<A>(op, context.op_version(op))
    }

    /// The cached result for `op` at `version`, dropping it if it describes an
    /// older version.
    fn lookup<A: Analysis>(&self, op: OpId, version: u32) -> Option<Rc<A>> {
        let key = (op, TypeId::of::<A>());
        let mut cache = self.cache.borrow_mut();
        let entry = cache.get(&key)?;
        if entry.version != version {
            cache.remove(&key);
            return None;
        }
        Some(
            entry
                .result
                .clone()
                .downcast()
                .expect("cache entry type matches its key"),
        )
    }

    /// How many results the cache holds, for the `TIR_MEM_STATS` census.
    pub fn cached_count(&self) -> usize {
        self.cache.borrow().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Operand, Operation, builtin::ops, func::ops as func_ops};

    struct Simple;

    impl Analysis for Simple {
        fn build(_: &AnalysisManager, _: &Context, _: OpId) -> Self {
            Simple
        }
    }

    /// Depends on [`Simple`]: built through the manager, so both are keyed on the
    /// same op version and go stale together.
    struct Dependent;

    impl Analysis for Dependent {
        fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
            analyses.get::<Simple>(context, op);
            Dependent
        }
    }

    fn test_op(context: &Context) -> OpId {
        ops::module(context, None).build().id()
    }

    /// Appends an op to `module`'s body, bumping its version.
    fn edit(context: &Context, module: OpId) {
        let body = context.get_op(module).regions()[0];
        let block = context
            .get_region(body)
            .iter(context.clone())
            .next()
            .expect("module has an entry block");
        block.append(func_ops::r#return(context, Operand::none()).build().id());
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

        assert!(am.get_cached::<Simple>(&context, op).is_none());
        let built = am.get::<Simple>(&context, op);
        assert!(Rc::ptr_eq(
            &built,
            &am.get_cached::<Simple>(&context, op).unwrap()
        ));
    }

    #[test]
    fn an_edit_makes_the_cached_analysis_stale() {
        let context = Context::with_default_dialects();
        let op = test_op(&context);
        let am = AnalysisManager::new();
        let first = am.get::<Simple>(&context, op);

        edit(&context, op);

        assert!(am.get_cached::<Simple>(&context, op).is_none());
        assert!(!Rc::ptr_eq(&first, &am.get::<Simple>(&context, op)));
    }

    #[test]
    fn an_edit_elsewhere_leaves_the_cached_analysis_alone() {
        let context = Context::with_default_dialects();
        let op = test_op(&context);
        let other = test_op(&context);
        let am = AnalysisManager::new();
        let first = am.get::<Simple>(&context, op);

        edit(&context, other);

        assert!(Rc::ptr_eq(&first, &am.get::<Simple>(&context, op)));
    }

    #[test]
    fn a_dependency_goes_stale_with_its_dependent() {
        let context = Context::with_default_dialects();
        let op = test_op(&context);
        let am = AnalysisManager::new();

        am.get::<Dependent>(&context, op);
        assert!(am.get_cached::<Simple>(&context, op).is_some());

        edit(&context, op);

        assert!(am.get_cached::<Dependent>(&context, op).is_none());
        assert!(am.get_cached::<Simple>(&context, op).is_none());
    }

    #[test]
    fn stale_entries_do_not_accumulate() {
        let context = Context::with_default_dialects();
        let op = test_op(&context);
        let am = AnalysisManager::new();

        for _ in 0..8 {
            am.get::<Simple>(&context, op);
            edit(&context, op);
        }

        assert_eq!(am.cached_count(), 1);
    }
}
