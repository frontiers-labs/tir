//! Hash-consing e-graph with congruence closure. Based on egg
//! (<https://github.com/egraphs-good/egg>, MIT License, Copyright Max Willsey).

mod eclass;
mod enode;
mod extract;
mod pattern;
mod rewrite;
mod runner;
mod telemetry;

use std::collections::{HashMap, HashSet};

use tir_adt::{FxBuildHasher, IndexMap, ScopedDisjointSet};

type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;
/// Hash-cons table: [`ENode::hash_cons`] bucket -> `[(canonical node, class)]`.
/// Never iterated, so a hash map keeps it deterministic.
type Memo<L> = FxHashMap<u64, Vec<(L, Id)>>;

pub use eclass::*;
pub use enode::*;
pub use extract::*;
pub use pattern::*;
pub use rewrite::*;
pub use runner::*;
pub use telemetry::{RoundStats, Timer, report_saturation};

/// Per-round frontier of semi-naive saturation: the change log of the previous
/// round closed upward, cached by the pattern heights a rule set asks for.
pub struct Delta {
    /// `levels[h]` is Δ_h; grown on demand, each from the one below it.
    levels: Vec<Vec<Id>>,
}

impl Delta {
    /// Seed from a [`EGraph::take_changed`] drain.
    pub fn new(changed: Vec<Id>) -> Self {
        Self {
            levels: vec![changed],
        }
    }

    /// Nothing changed, so no rule can match anywhere new.
    pub fn is_empty(&self) -> bool {
        self.levels[0].is_empty()
    }

    /// Size of the change log this frontier grew from.
    pub fn len(&self) -> usize {
        self.levels[0].len()
    }

    /// Size of the deepest frontier asked for so far — levels only grow, so this
    /// is the widest set any rule searched.
    pub fn frontier(&self) -> usize {
        self.levels.last().expect("seeded level").len()
    }

    /// Δ_height, ascending.
    pub fn at<L: ENode>(&mut self, eg: &EGraph<L>, height: usize) -> &[Id] {
        while self.levels.len() <= height {
            let below = self.levels.last().expect("seeded level");
            self.levels.push(eg.delta(below, 1));
        }
        &self.levels[height]
    }
}

/// E-class id. Non-canonical after unions — pass through [`EGraph::find`] before comparing.
#[derive(Clone, Copy, Hash, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct Id(u32);

impl Id {
    pub fn index(self) -> usize {
        self.0 as usize
    }

    pub fn from_raw(raw: u32) -> Self {
        Id(raw)
    }
}

pub struct EGraph<L: ENode> {
    /// Class-id equivalence; the sole authority on equality (all comparison flows
    /// through [`Self::find`]). Scoped unions layer here, discarded by `pop_context`.
    unionfind: ScopedDisjointSet,
    /// Base hash-cons. Collisions only share a bucket; identity is `matches` + equal children.
    memo: Memo<L>,
    /// Canonical base class id -> e-class; absorbed ids removed on `union`. Scoped
    /// unions never touch it, so `pop_context` restores it for free. Insertion order
    /// is ascending id, which is the class iteration order callers observe.
    classes: IndexMap<Id, EClass<L>>,
    /// Running `classes` node total, so [`Self::total_size`] costs nothing.
    total_nodes: usize,
    /// [`ENode::op_key`] bucket -> class ids holding such a node, so
    /// [`Self::classes_with_op`] skips classes a concrete-rooted pattern can't match.
    /// Append-only, caller-dedup'd: over-approximates, never misses a live class.
    classes_by_op: IndexMap<u64, Vec<Id>>,
    /// Classes touched by a `union` since the last `rebuild`, awaiting repair.
    pending: Vec<Id>,
    /// Scope overlay, live only inside a scope. One `scope_members` frame per open
    /// context holds that scope's partition (base reps grouped under their scoped
    /// rep, merged groups only); `scope_memo` stacks one hash-cons per context so a
    /// nested `pop_context` restores the enclosing table. Base `classes`/`memo` stay
    /// immutable underneath.
    scope_members: Vec<FxHashMap<Id, Vec<Id>>>,
    /// Read-side aggregation of the innermost frame, refreshed by [`Self::refresh_view`].
    view: ScopeView<L>,
    scope_memo: Vec<Memo<L>>,
    /// Undo log of base insertions per open context: `make_class` still writes the
    /// new class into base `classes`/`classes_by_op`/children's `parents` while scoped
    /// (so the overlay, which reads base, sees it), but `pop_context` reverts exactly
    /// these so a popped scope leaves the base structurally identical. One frame per
    /// context; nested pops revert only the innermost.
    scope_created: Vec<Vec<ScopeCreated>>,
    /// Scoped facts: canonical class -> the constant it is assumed to evaluate to.
    /// `scope_assumed` logs, per open context, each key it wrote with the entry it
    /// overwrote, so `pop_context` restores exactly the enclosing table.
    assumed: FxHashMap<Id, L>,
    scope_assumed: Vec<Vec<(Id, Option<L>)>>,
    /// Classes changed since the last [`Self::take_changed`], possibly
    /// non-canonical — semi-naive saturation's frontier. A round unions and
    /// repairs the same class many times, so entries are deduplicated as they are
    /// logged rather than by sorting a log the size of the round's work.
    changed: Vec<Id>,
    /// Per class id, the [`Self::changed_epoch`] it was last logged in.
    changed_at: Vec<u32>,
    /// Bumped by every drain, so the stamps of earlier epochs read as absent.
    changed_epoch: u32,
    /// Set when something changed that `changed` cannot name (a fresh graph, or a
    /// driver that stopped on a limit), which the drain reports as "everything".
    changed_all: bool,
    /// The change log as each open scope found it. A scope leaves the base graph
    /// structurally identical — its unions are layered, the classes it mints are
    /// reverted — so what changed since the last drain is, at the pop, exactly
    /// what it was at the push. Saving it beats declaring "everything": the base
    /// keeps the fixpoint it had reached, and the scope's own rounds still see
    /// their own changes.
    scope_changed: Vec<(Vec<Id>, bool)>,
}

/// Aggregated read view of the innermost scope, as of the last refresh. Only merged
/// groups are materialized; every other class is read straight from the base.
struct ScopeView<L: ENode> {
    /// Scoped rep -> its aggregated e-class.
    classes: FxHashMap<Id, EClass<L>>,
    /// What [`EGraph::classes`] emits at a merged group's base class: `Some(rep)` at
    /// the group's first member, `None` (skip) at the rest.
    plan: FxHashMap<Id, Option<Id>>,
    /// Base classes present at the refresh. Classes minted afterwards stay invisible
    /// to the view until the next one, as the old full re-aggregation had them.
    watermark: usize,
    num_classes: usize,
    total_size: usize,
}

impl<L: ENode> ScopeView<L> {
    fn clear(&mut self) {
        self.classes.clear();
        self.plan.clear();
        self.watermark = 0;
        self.num_classes = 0;
        self.total_size = 0;
    }
}

impl<L: ENode> Default for ScopeView<L> {
    fn default() -> Self {
        Self {
            classes: FxHashMap::default(),
            plan: FxHashMap::default(),
            watermark: 0,
            num_classes: 0,
            total_size: 0,
        }
    }
}

/// One base class minted by [`EGraph::make_class`] inside a scope, with what its
/// insertion mutated so [`EGraph::pop_context`] can undo it.
struct ScopeCreated {
    id: Id,
    op_key: u64,
    /// Child classes that received a parent back-edge for this class.
    parents_on: Vec<Id>,
}

impl<L: ENode> Default for EGraph<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L: ENode> EGraph<L> {
    pub fn new() -> Self {
        Self {
            unionfind: ScopedDisjointSet::new(0),
            memo: Memo::default(),
            classes: IndexMap::new(),
            total_nodes: 0,
            classes_by_op: IndexMap::new(),
            pending: Vec::new(),
            scope_members: Vec::new(),
            view: ScopeView::default(),
            scope_memo: Vec::new(),
            scope_created: Vec::new(),
            assumed: FxHashMap::default(),
            scope_assumed: Vec::new(),
            changed: Vec::new(),
            changed_at: Vec::new(),
            changed_epoch: 1,
            changed_all: true,
            scope_changed: Vec::new(),
        }
    }

    /// Note that `id`'s class changed, at most once per epoch.
    fn log_change(&mut self, id: Id) {
        if self.changed_at.len() <= id.index() {
            self.changed_at.resize(id.index() + 1, 0);
        }
        if self.changed_at[id.index()] != self.changed_epoch {
            self.changed_at[id.index()] = self.changed_epoch;
            self.changed.push(id);
        }
    }

    /// Drain the change log: the canonical classes changed since the previous
    /// call, ascending and deduplicated; `None` means "every class".
    pub fn take_changed(&mut self) -> Option<Vec<Id>> {
        let all = std::mem::replace(&mut self.changed_all, false);
        let mut changed = std::mem::take(&mut self.changed);
        // Every stamp of this epoch is now stale. Wrapping past the epoch a stamp
        // still holds would resurrect it, so the wrap clears them instead.
        self.changed_epoch = self.changed_epoch.wrapping_add(1);
        if self.changed_epoch == 0 {
            self.changed_at.fill(0);
            self.changed_epoch = 1;
        }
        if all {
            return None;
        }
        // Logging deduplicates by raw id; canonicalizing merges more of them.
        for id in &mut changed {
            *id = self.find(*id);
        }
        changed.sort_unstable();
        changed.dedup();
        Some(changed)
    }

    /// Report "everything" from the next [`Self::take_changed`]. A driver that
    /// stopped on a limit rather than at a fixpoint calls this: the matches it
    /// never reached are not named by the change log.
    pub fn mark_all_changed(&mut self) {
        self.changed_all = true;
    }

    pub fn is_empty(&self) -> bool {
        self.classes.is_empty()
    }

    /// Total number of e-nodes across all (current-scope) classes.
    pub fn total_size(&self) -> usize {
        if self.in_scope() {
            self.view.total_size
        } else {
            self.total_nodes
        }
    }

    pub fn num_classes(&self) -> usize {
        if self.in_scope() {
            self.view.num_classes
        } else {
            self.classes.len()
        }
    }

    fn in_scope(&self) -> bool {
        self.unionfind.depth() > 0
    }

    /// Enter an assumption scope: unions until the matching `pop_context` are local;
    /// base classes and hash-cons stay untouched.
    pub fn push_context(&mut self) {
        let frame = self.scope_members.last().cloned().unwrap_or_default();
        self.unionfind.push_context();
        self.scope_memo.push(Memo::default());
        self.scope_created.push(Vec::new());
        self.scope_assumed.push(Vec::new());
        self.scope_members.push(frame);
        self.scope_changed
            .push((self.changed.clone(), self.changed_all));
        self.refresh_view();
    }

    /// Leave the scope, discarding its unions, overlay, and any classes added inside
    /// it; the enclosing scope (or base) is restored without a rebuild.
    pub fn pop_context(&mut self) {
        if let Some(created) = self.scope_created.pop() {
            self.undo_created(created);
        }
        for (key, previous) in self.scope_assumed.pop().into_iter().flatten().rev() {
            match previous {
                Some(node) => self.assumed.insert(key, node),
                None => self.assumed.remove(&key),
            };
        }
        self.unionfind.pop_context();
        self.scope_memo.pop();
        self.scope_members.pop();
        // Drop union work queued against classes that vanished with the scope; a
        // survivor kept here would panic the next base `repair`.
        self.pending = self
            .pending
            .iter()
            .copied()
            .filter(|&id| self.classes.contains_key(&self.find(id)))
            .collect();
        self.restore_changed();
        if self.in_scope() {
            self.refresh_view();
        } else {
            self.view.clear();
        }
    }

    /// Put the change log back the way the just-popped scope found it, dropping
    /// everything logged inside it. A fresh epoch invalidates the scope's stamps
    /// in one step; the restored ids are then re-stamped, so the log and the
    /// stamps agree again.
    fn restore_changed(&mut self) {
        let (changed, all) = self.scope_changed.pop().expect("open scope");
        self.changed_epoch = self.changed_epoch.wrapping_add(1);
        if self.changed_epoch == 0 {
            self.changed_at.fill(0);
            self.changed_epoch = 1;
        }
        for id in &changed {
            self.changed_at[id.index()] = self.changed_epoch;
        }
        self.changed = changed;
        self.changed_all = all;
    }

    /// Revert the base insertions logged for a just-popped scope: remove each class's
    /// `classes_by_op` entry, the parent back-edges it pushed onto surviving children,
    /// and the class itself.
    fn undo_created(&mut self, created: Vec<ScopeCreated>) {
        for c in created {
            for child in c.parents_on {
                if let Some(class) = self.classes.get_mut(&child) {
                    class.parents.retain(|(_, pc)| *pc != c.id);
                }
            }
            if let Some(bucket) = self.classes_by_op.get_mut(&c.op_key) {
                bucket.retain(|&id| id != c.id);
                if bucket.is_empty() {
                    self.classes_by_op.remove(&c.op_key);
                }
            }
            if let Some(class) = self.classes.remove(&c.id) {
                self.total_nodes -= class.nodes.len();
            }
        }
    }

    /// Canonicalize `id` to its class root.
    pub fn find(&self, id: Id) -> Id {
        Id::from_raw(self.unionfind.find(id.0))
    }

    pub fn connected(&self, a: Id, b: Id) -> bool {
        self.find(a) == self.find(b)
    }

    pub fn class(&self, id: Id) -> &EClass<L> {
        let root = self.find(id);
        // Fall back to base for a class the view does not merge, or one added since
        // the last scope rebuild.
        (!self.view.classes.is_empty())
            .then(|| self.view.classes.get(&root))
            .flatten()
            .or_else(|| self.classes.get(&root))
            .expect("live e-class")
    }

    pub fn classes(&self) -> impl Iterator<Item = &EClass<L>> + '_ {
        let scoped = self.in_scope();
        let base = (!scoped)
            .then(|| self.classes.values())
            .into_iter()
            .flatten();
        let view = scoped
            .then(|| {
                self.classes
                    .values()
                    .take(self.view.watermark)
                    .filter_map(|class| match self.view.plan.get(&class.id) {
                        Some(rep) => rep.map(|rep| &self.view.classes[&rep]),
                        None => Some(class),
                    })
            })
            .into_iter()
            .flatten();
        base.chain(view)
    }

    /// Canonical current-scope classes holding a node in `op` bucket, each once.
    /// Over-approximates — callers confirm with [`ENode::matches`].
    pub fn classes_with_op(&self, op: u64) -> Vec<Id> {
        let Some(ids) = self.classes_by_op.get(&op) else {
            return Vec::new();
        };
        let mut seen: HashSet<Id, FxBuildHasher> =
            HashSet::with_capacity_and_hasher(ids.len(), FxBuildHasher::default());
        ids.iter()
            .map(|&id| self.find(id))
            .filter(|&root| seen.insert(root))
            .collect()
    }

    /// E-nodes of `id`'s class; child ids may be non-canonical — resolve with [`Self::find`].
    pub fn nodes(&self, id: Id) -> &[L] {
        self.class(id).nodes()
    }

    /// The base class ids the scope partition groups under the scoped-canonical
    /// `id` (as [`Self::aggregate_scope`] builds it). Empty when no scope is open
    /// or `id` is not a scoped representative — then `id` is itself the base rep.
    /// Side tables built against the base graph are keyed by base reps, so a query
    /// made under a scope aggregates over this slice.
    pub fn scope_members(&self, id: Id) -> &[Id] {
        self.scope_members
            .last()
            .and_then(|frame| frame.get(&id))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Canonical classes the open scopes changed: the ones their unions merged,
    /// the ones minted inside them, and transitively every class holding a node
    /// with such a child — a parent's e-nodes re-canonicalize through a merge, so
    /// a pattern rooted there can match under the scope and not in the base graph.
    /// This is therefore the whole set a scoped re-search must revisit: outside it
    /// the reachable sub-graph is the base one node for node. Ascending id, so a
    /// caller's search order is reproducible. Empty with no scope open.
    pub fn scope_dirty(&self) -> Vec<Id> {
        let seeds: Vec<Id> = self
            .scope_created
            .iter()
            .flatten()
            .map(|created| created.id)
            .chain(self.assumed.keys().copied())
            .chain(
                self.scope_members
                    .last()
                    .into_iter()
                    .flat_map(|frame| frame.keys().copied()),
            )
            .collect();
        self.close_upward(seeds, None)
    }

    /// `changed` closed upward `height` times over parent edges, ascending: the
    /// classes a pattern of that height can newly match at. A class outside it has
    /// an unchanged downward cone to that depth, so its matches are the previous
    /// round's and were applied then.
    pub fn delta(&self, changed: &[Id], height: usize) -> Vec<Id> {
        self.close_upward(changed.to_vec(), Some(height))
    }

    /// `seeds` and everything reachable upward from them over parent edges in at
    /// most `levels` steps (unbounded when `None`), ascending. Side tables and
    /// back-edges are keyed by base reps, so a merged group's parents are the
    /// union of its members'.
    fn close_upward(&self, seeds: Vec<Id>, levels: Option<usize>) -> Vec<Id> {
        let mut seen: HashSet<Id, FxBuildHasher> = HashSet::default();
        let mut frontier: Vec<Id> = seeds
            .into_iter()
            .map(|id| self.find(id))
            .filter(|&id| seen.insert(id))
            .collect();
        let mut closure = frontier.clone();
        let mut level = 0;
        while !frontier.is_empty() && levels.is_none_or(|max| level < max) {
            let mut next = Vec::new();
            for id in frontier.drain(..) {
                let members = match self.scope_members(id) {
                    [] => std::slice::from_ref(&id),
                    members => members,
                };
                for member in members {
                    let Some(class) = self.classes.get(member) else {
                        continue;
                    };
                    for &(_, parent) in &class.parents {
                        let parent = self.find(parent);
                        if seen.insert(parent) {
                            closure.push(parent);
                            next.push(parent);
                        }
                    }
                }
            }
            frontier = next;
            level += 1;
        }
        closure.sort_unstable();
        closure
    }

    /// Assume, inside the current scope, that `class` evaluates to constant `node`.
    /// A fact, not a merge: the class keeps its own identity and parents, so only
    /// its users see a change. Panics with no scope open — an unscoped assumption
    /// would never be popped.
    pub fn assume_const(&mut self, class: Id, node: L) {
        let frame = self.scope_assumed.last_mut().expect("open scope");
        let root = Id::from_raw(self.unionfind.find(class.0));
        let previous = self.assumed.insert(root, node);
        frame.push((root, previous));
        self.log_change(root);
    }

    /// The constant `class` is assumed to evaluate to in the open scopes, if any.
    pub fn assumed_const(&self, class: Id) -> Option<&L> {
        self.assumed.get(&self.find(class))
    }

    /// The classes the open scopes assume to evaluate to `node`.
    pub fn assumed_classes<'a>(&'a self, node: &'a L) -> impl Iterator<Item = Id> + 'a {
        self.assumed
            .iter()
            .filter(move |(_, assumed)| assumed.matches(node))
            .map(|(&class, _)| class)
    }

    /// Intern `node`, returning its e-class. A non-unique node equal to an existing
    /// one shares its class; otherwise (always for unique nodes) a fresh class.
    pub fn add(&mut self, mut node: L) -> Id {
        self.canonicalize(&mut node);
        if !node.is_unique()
            && let Some(existing) = self.memo_find(&node)
        {
            return existing;
        }
        self.make_class(node)
    }

    /// Class of an already-interned `node`, or `None` (never inserts; always `None` for unique).
    pub fn lookup(&self, node: &L) -> Option<Id> {
        if node.is_unique() {
            return None;
        }
        let mut node = node.clone();
        self.canonicalize(&mut node);
        self.memo_find(&node)
    }

    /// Merge the classes of `a` and `b`, returning the survivor. Congruence repair
    /// deferred to [`Self::rebuild`].
    pub fn union(&mut self, a: Id, b: Id) -> Id {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return ra;
        }
        telemetry::count_merge();
        let survivor = Id::from_raw(self.unionfind.union(ra.0, rb.0));
        let absorbed = if survivor == ra { rb } else { ra };
        self.rekey_assumption(absorbed, survivor);
        if let Some(frame) = self.scope_members.last_mut() {
            // Scope overlay only; base classes stay intact for `pop_context`.
            let taken = frame.remove(&absorbed).unwrap_or_else(|| vec![absorbed]);
            frame
                .entry(survivor)
                .or_insert_with(|| vec![survivor])
                .extend(taken);
        } else {
            let mut taken = self.classes.remove(&absorbed).expect("absorbed e-class");
            let surv = self.classes.get_mut(&survivor).expect("surviving e-class");
            surv.nodes.append(&mut taken.nodes);
            surv.parents.append(&mut taken.parents);
        }
        self.pending.push(survivor);
        self.log_change(survivor);
        survivor
    }

    /// Move a fact keyed on a just-absorbed class under the survivor, logged as
    /// writes of the current scope so its pop puts both keys back. The survivor's
    /// own fact wins when both carry one.
    fn rekey_assumption(&mut self, absorbed: Id, survivor: Id) {
        let Some(node) = self.assumed.remove(&absorbed) else {
            return;
        };
        let frame = self.scope_assumed.last_mut().expect("open scope");
        frame.push((absorbed, Some(node.clone())));
        if self.assumed.contains_key(&survivor) {
            return;
        }
        self.assumed.insert(survivor, node);
        frame.push((survivor, None));
        self.log_change(survivor);
    }

    /// Saturate in place with `rules`. Each iteration searches all rules against one
    /// snapshot, then applies and rebuilds — a node born this iteration is visible
    /// only to the next. Stops at a fixpoint (no class/node-count change) or a limit.
    pub fn saturate<'a, S>(
        &mut self,
        rules: impl IntoIterator<Item = &'a Rewrite<L, S>>,
        iter_limit: usize,
        node_limit: usize,
    ) where
        L: 'a,
        S: Clone + PartialEq + 'a,
    {
        let timer = telemetry::Timer::start();
        let rules: Vec<&Rewrite<L, S>> = rules.into_iter().collect();
        let mut delta = self.take_changed().map(Delta::new);
        let mut iters = 0;
        loop {
            let size = self.total_size();
            if iters >= iter_limit || size >= node_limit {
                // Not a fixpoint: the matches this stop left unreached are not in
                // the change log, so the next saturation may not trust it.
                self.mark_all_changed();
                break;
            }
            let before = (self.num_classes(), size);

            let mut stats = RoundStats::start(delta.as_ref());
            let searched: Vec<_> = rules
                .iter()
                .map(|rule| {
                    let frontier = delta.as_mut().filter(|_| rule.cone_bounded());
                    let roots = rule.lhs.round_roots(self, None, frontier);
                    stats.searched(roots.len(), delta.as_ref());
                    (*rule, rule.lhs.search_roots(self, roots))
                })
                .collect();
            for (rule, matches) in &searched {
                for m in matches {
                    stats.apply(self, |eg| rule.apply_match(eg, m));
                }
            }
            self.rebuild();
            stats.finish();

            iters += 1;
            delta = self.take_changed().map(Delta::new);
            if delta.as_ref().is_some_and(Delta::is_empty) {
                break;
            }
            if (self.num_classes(), self.total_size()) == before {
                break;
            }
        }
        timer.finish();
    }

    /// Restore congruence to a fixpoint after a batch of unions, re-canonicalizing
    /// the hash-cons. Each round dedups pending to canonical reps first: without it a
    /// survivor queued many times by `union` would re-`repair` its growing parent
    /// list each time, making rebuild quadratic. Rounds run until one adds nothing.
    pub fn rebuild(&mut self) {
        if self.in_scope() {
            self.rebuild_scope();
            return;
        }
        while !self.pending.is_empty() {
            let mut todo = std::mem::take(&mut self.pending);
            for id in &mut todo {
                *id = self.find(*id);
            }
            todo.sort_unstable();
            todo.dedup();
            for id in todo {
                self.repair(id);
            }
        }
    }

    /// Congruence repair inside a scope, base `classes`/`memo` read-only: walk the
    /// base parents each touched scope class covers, canonicalize through the scope,
    /// and union collisions in a fresh scope hash-cons. Fixpoint, then re-aggregate.
    fn rebuild_scope(&mut self) {
        // Scope hash-cons accumulated across rounds; per-round dedup avoids the same
        // quadratic the base path avoids.
        let mut memo: Memo<L> = Memo::default();
        while !self.pending.is_empty() {
            let mut todo = std::mem::take(&mut self.pending);
            for rep in &mut todo {
                *rep = self.find(*rep);
            }
            todo.sort_unstable();
            todo.dedup();
            for rep in todo {
                let rep = self.find(rep);
                let members = self
                    .scope_members
                    .last()
                    .and_then(|frame| frame.get(&rep))
                    .cloned()
                    .unwrap_or_else(|| vec![rep]);
                for base_rep in members {
                    let Some(class) = self.classes.get(&base_rep) else {
                        continue;
                    };
                    for (mut p_node, p_class) in class.parents.clone() {
                        if p_node.is_unique() {
                            continue;
                        }
                        self.canonicalize(&mut p_node);
                        let p_class = self.find(p_class);
                        let bucket = memo.entry(p_node.hash_cons()).or_default();
                        let congruent = bucket
                            .iter()
                            .find(|(stored, _)| is_congruent(stored, &p_node))
                            .map(|&(_, id)| id);
                        match congruent {
                            Some(other) => {
                                let other = self.find(other);
                                if other != p_class {
                                    self.union(other, p_class);
                                }
                            }
                            None => bucket.push((p_node, p_class)),
                        }
                    }
                }
            }
        }
        self.refresh_view();
    }

    /// Re-aggregate the read view from the innermost scope frame. Only merged groups
    /// are materialized, so the cost is the size of the assumption's collapse, not of
    /// the graph. Members are sorted so a group's e-nodes concatenate in base order.
    fn refresh_view(&mut self) {
        let mut frame = self.scope_members.pop().expect("open scope");
        self.view.classes.clear();
        self.view.plan.clear();
        let mut absorbed = 0;
        for (&rep, members) in frame.iter_mut() {
            members.sort_unstable();
            members.dedup();
            let mut nodes = Vec::new();
            for member in members.iter() {
                if let Some(class) = self.classes.get(member) {
                    nodes.extend(class.nodes.iter().cloned());
                }
            }
            self.view.plan.insert(members[0], Some(rep));
            for &member in &members[1..] {
                self.view.plan.insert(member, None);
            }
            absorbed += members.len() - 1;
            self.view.classes.insert(
                rep,
                EClass {
                    id: rep,
                    nodes,
                    parents: Vec::new(),
                },
            );
        }
        self.view.watermark = self.classes.len();
        self.view.num_classes = self.view.watermark - absorbed;
        self.view.total_size = self.total_nodes;
        self.scope_members.push(frame);
    }

    fn class_mut(&mut self, id: Id) -> &mut EClass<L> {
        let root = self.find(id);
        self.classes.get_mut(&root).expect("live e-class")
    }

    /// Rewrite a node's children to their roots; returns whether any changed.
    fn canonicalize(&self, node: &mut L) -> bool {
        let mut changed = false;
        for child in node.children_mut() {
            let root = self.find(*child);
            if root != *child {
                *child = root;
                changed = true;
            }
        }
        changed
    }

    /// Class of a canonical `node` in the memo, or `None`. Scopes innermost-first,
    /// then the base hash-cons.
    fn memo_find(&self, node: &L) -> Option<Id> {
        for memo in self.scope_memo.iter().rev() {
            if let Some(id) = Self::bucket_lookup(memo, node) {
                return Some(self.find(id));
            }
        }
        Self::bucket_lookup(&self.memo, node).map(|id| self.find(id))
    }

    fn bucket_lookup(memo: &Memo<L>, node: &L) -> Option<Id> {
        memo.get(&node.hash_cons())?
            .iter()
            .find(|(stored, _)| is_congruent(stored, node))
            .map(|&(_, id)| id)
    }

    /// Insert/update the memo entry for a canonical `node`, in the innermost open
    /// scope's hash-cons while scoped (base untouched).
    fn memo_insert(&mut self, node: L, id: Id) {
        let memo = self.scope_memo.last_mut().unwrap_or(&mut self.memo);
        let bucket = memo.entry(node.hash_cons()).or_default();
        match bucket
            .iter_mut()
            .find(|(stored, _)| is_congruent(stored, &node))
        {
            Some(slot) => slot.1 = id,
            None => bucket.push((node, id)),
        }
    }

    /// Drop the memo entry for a (possibly stale) `node`, if present.
    fn memo_remove(&mut self, node: &L) {
        let key = node.hash_cons();
        let Some(bucket) = self.memo.get_mut(&key) else {
            return;
        };
        if let Some(pos) = bucket
            .iter()
            .position(|(stored, _)| is_congruent(stored, node))
        {
            bucket.swap_remove(pos);
        }
        if bucket.is_empty() {
            self.memo.remove(&key);
        }
    }

    /// Fresh singleton class for a canonical `node`: register it as a parent of each
    /// distinct child class and (unless unique) memoize it.
    fn make_class(&mut self, node: L) -> Id {
        let id = Id::from_raw(self.unionfind.push());
        let op_key = node.op_key();
        self.classes_by_op.entry(op_key).or_default().push(id);
        let mut seen: Vec<Id> = Vec::new();
        for &child in node.children() {
            let child = self.find(child);
            if !seen.contains(&child) {
                seen.push(child);
                self.classes
                    .get_mut(&child)
                    .expect("child e-class")
                    .parents
                    .push((node.clone(), id));
            }
        }
        if !node.is_unique() {
            self.memo_insert(node.clone(), id);
        }
        self.classes.insert(id, EClass::new(id, node));
        self.total_nodes += 1;
        telemetry::count_add();
        self.log_change(id);
        // Inside a scope, log this base insertion so `pop_context` can revert it.
        if let Some(frame) = self.scope_created.last_mut() {
            frame.push(ScopeCreated {
                id,
                op_key,
                parents_on: seen,
            });
        }
        id
    }

    /// Congruence repair for one class: re-canonicalize its `parents`, refresh their
    /// memo entries, and union any now structurally equal (queuing more via `union`).
    fn repair(&mut self, id: Id) {
        telemetry::count_repair();
        let id = self.find(id);
        let parents = std::mem::take(&mut self.class_mut(id).parents);

        for (p_node, _) in &parents {
            if !p_node.is_unique() {
                self.memo_remove(p_node);
            }
        }

        let mut new_parents: Vec<(L, Id)> = Vec::with_capacity(parents.len());
        let mut index: FxHashMap<u64, Vec<usize>> = FxHashMap::default();
        for (mut p_node, p_class) in parents {
            if self.canonicalize(&mut p_node) {
                self.log_change(p_class);
            }
            let p_class = self.find(p_class);
            if p_node.is_unique() {
                new_parents.push((p_node, p_class));
                continue;
            }
            let slot = index.entry(p_node.hash_cons()).or_default();
            let congruent = slot
                .iter()
                .copied()
                .find(|&i| is_congruent(&new_parents[i].0, &p_node));
            match congruent {
                Some(i) => {
                    let kept = new_parents[i].1;
                    self.union(kept, p_class);
                }
                None => {
                    slot.push(new_parents.len());
                    self.memo_insert(p_node.clone(), p_class);
                    new_parents.push((p_node, p_class));
                }
            }
        }

        // Extend, don't assign: a `union` above may have appended parents to this
        // class; an assignment would drop them. Duplicates dedup on the next pass.
        let root = self.find(id);
        self.class_mut(root).parents.extend(new_parents);
    }
}

/// Structural congruence: same operator ([`ENode::matches`]) and equal canonical children.
fn is_congruent<L: ENode>(stored: &L, probe: &L) -> bool {
    stored.matches(probe) && stored.children() == probe.children()
}
