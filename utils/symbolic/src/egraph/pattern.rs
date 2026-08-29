use std::collections::HashSet;

use smallvec::SmallVec;
use tir_adt::{APFloat, APInt, FxBuildHasher};

use crate::egraph::{Delta, EGraph, ENode, Id};

#[derive(Debug, Clone, PartialEq, PartialOrd, Ord, Eq, Hash)]
pub enum Var<S> {
    Symbol(S),
    Int(APInt),
    Float(APFloat),
}

/// A mapping from pattern variables to the e-classes they bound to during a match.
#[derive(Debug, Clone, Eq, PartialEq, Ord, Hash, PartialOrd)]
pub struct Substitution<S> {
    pub(crate) vec: SmallVec<[(Var<S>, Id); 4]>,
}

impl<S> Default for Substitution<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S> Substitution<S> {
    pub fn new() -> Self {
        Self {
            vec: SmallVec::new(),
        }
    }
}

impl<S> Substitution<S> {
    /// The bound variables and their classes, in binding order.
    pub fn entries(&self) -> impl Iterator<Item = (&Var<S>, Id)> {
        self.vec.iter().map(|(var, id)| (var, *id))
    }
}

impl<S: PartialEq> Substitution<S> {
    pub fn insert(&mut self, var: Var<S>, id: Id) -> Option<Id> {
        for pair in &mut self.vec {
            if var == pair.0 {
                return Some(core::mem::replace(&mut pair.1, id));
            }
        }
        self.vec.push((var, id));
        None
    }

    pub fn get(&self, var: &Var<S>) -> Option<Id> {
        self.vec
            .iter()
            .find(|pair| &pair.0 == var)
            .map(|pair| pair.1)
    }
}

/// One node of a [`Pattern`]: a template operator or a hole.
#[derive(Debug, Clone)]
pub enum PatternNode<N: ENode, S> {
    /// Template e-node; child ids are pattern-local indices into `nodes`, not e-class ids.
    Node(N),
    /// Leaf hole: `Symbol` binds any class; `Int`/`Float` match that constant unbound.
    Var(Var<S>),
}

/// Structural pattern over `N`: search template (LHS) and, via [`Self::instantiate`], rewrite RHS. Nodes stored bottom-up, so a child's index is always smaller than its parent's.
#[derive(Debug, Clone)]
pub struct Pattern<N: ENode, S> {
    nodes: Vec<PatternNode<N, S>>,
    /// Template levels below each node; children precede their parents, so it is
    /// filled in one pass as nodes are pushed.
    heights: Vec<usize>,
    root: Id,
}

/// One match of a [`Pattern`] against an e-graph: the matched e-class, the
/// variable bindings that made it match, and the e-class bound to every pattern
/// node (for callers that need interior bindings, e.g. instruction selection's
/// PBQP cover).
#[derive(Debug, Clone)]
pub struct EMatch<S> {
    pub root: Id,
    pub subst: Substitution<S>,
    /// Per-pattern-node bound class, indexed by pattern node; `None` only for a
    /// node unreachable from the root. Inline: a round emits one of these per
    /// match, and on `core_main.c` instcombine emits a hundred thousand.
    pub bindings: SmallVec<[Option<Id>; 8]>,
}

impl<S> EMatch<S> {
    /// The e-class bound to `node`; panics for a node unreachable from the root.
    pub fn binding(&self, node: Id) -> Id {
        self.bindings[node.index()].expect("pattern node not reached from the root")
    }
}

impl<N: ENode, S> Default for Pattern<N, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<N: ENode, S> Pattern<N, S> {
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            heights: Vec::new(),
            root: Id::from_raw(0),
        }
    }

    /// Add a hole; its id is the root until a later `add`/`var` or [`Self::set_root`].
    pub fn var(&mut self, var: Var<S>) -> Id {
        self.push(PatternNode::Var(var))
    }

    /// Add a template node. Wire its children to ids returned by earlier calls.
    pub fn add(&mut self, node: N) -> Id {
        self.push(PatternNode::Node(node))
    }

    pub fn set_root(&mut self, root: Id) {
        self.root = root;
    }

    pub fn root(&self) -> Id {
        self.root
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: Id) -> &PatternNode<N, S> {
        &self.nodes[id.index()]
    }

    /// Template levels below the root: how deep under a matched class this pattern
    /// binds. A hole or a childless template has height 0.
    pub fn height(&self) -> usize {
        self.heights[self.root.index()]
    }

    /// The classes a saturation round searches this pattern at: `scope` when the
    /// caller narrowed the saturation, otherwise everything [`Self::search`] would
    /// visit — then, once a round has a frontier, only the classes in it at this
    /// pattern's height. A class outside the frontier has an unchanged cone down to
    /// the pattern's depth, so its matches are the previous round's, applied then.
    pub fn round_roots(
        &self,
        eg: &EGraph<N>,
        scope: Option<&[Id]>,
        delta: Option<&mut Delta>,
    ) -> Vec<Id> {
        let mut roots = match scope {
            Some(scope) => scope.to_vec(),
            None => self.roots_all(eg),
        };
        if let Some(delta) = delta {
            let frontier = delta.at(eg, self.height());
            roots.retain(|&root| frontier.binary_search(&eg.find(root)).is_ok());
        }
        roots
    }

    /// The classes [`Self::search`] visits: those holding the root operator, or
    /// every class for a bare-variable root.
    fn roots_all(&self, eg: &EGraph<N>) -> Vec<Id> {
        match &self.nodes[self.root.index()] {
            PatternNode::Node(template) => eg.classes_with_op(template.op_key()),
            _ => eg.class_ids().collect(),
        }
    }

    fn push(&mut self, node: PatternNode<N, S>) -> Id {
        let id = Id::from_raw(self.nodes.len() as u32);
        let height = match &node {
            PatternNode::Var(_) => 0,
            PatternNode::Node(template) => template
                .children()
                .iter()
                .map(|child| self.heights[child.index()] + 1)
                .max()
                .unwrap_or(0),
        };
        self.nodes.push(node);
        self.heights.push(height);
        self.root = id;
        id
    }
}

impl<N: ENode, S: Clone + PartialEq> Pattern<N, S> {
    /// Every match across the e-graph; an operator-rooted pattern visits only classes holding that operator ([`EGraph::classes_with_op`]), a bare-variable root every class.
    pub fn search(&self, eg: &EGraph<N>) -> Vec<EMatch<S>> {
        self.search_with_legality(eg, &|_, _| true)
    }

    /// Like [`Pattern::search`], but `allowed(pattern_node, class)` prunes any
    /// branch binding a disallowed pair — the hook for caller-side operand
    /// constraints and match legality (e.g. instruction selection's register /
    /// immediate / width requirements).
    pub fn search_with_legality(
        &self,
        eg: &EGraph<N>,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<EMatch<S>> {
        self.search_roots_with_legality(eg, self.roots_all(eg), allowed)
    }

    /// Match only the requested root classes.
    pub fn search_roots(
        &self,
        eg: &EGraph<N>,
        roots: impl IntoIterator<Item = Id>,
    ) -> Vec<EMatch<S>> {
        self.search_roots_with_legality(eg, roots, &|_, _| true)
    }

    /// Like [`Self::search_roots`], with caller-side legality pruning.
    pub fn search_roots_with_legality(
        &self,
        eg: &EGraph<N>,
        roots: impl IntoIterator<Item = Id>,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<EMatch<S>> {
        self.search_roots_delta(eg, roots, allowed, false)
    }

    /// Like [`Self::search_roots_with_legality`], and when `only_new` is set,
    /// emitting only the matches that bind an e-node the previous round created
    /// or re-canonicalized.
    ///
    /// The rest existed one round earlier, at a root this pattern was searched
    /// at then — the frontier holds a class as long as anything in its cone
    /// moves — so they were applied then, and applying them again instantiates
    /// terms that hash-cons back onto themselves. That is what the round
    /// counters call a no-op, and on `core_main.c` it is 99 % of the matches
    /// instcombine applies. Sound only under the same condition that licenses
    /// narrowing the roots at all: the pattern must be the whole match
    /// predicate ([`Rewrite::cone_bounded`](super::Rewrite::cone_bounded)).
    pub fn search_roots_delta(
        &self,
        eg: &EGraph<N>,
        roots: impl IntoIterator<Item = Id>,
        allowed: &dyn Fn(Id, Id) -> bool,
        only_new: bool,
    ) -> Vec<EMatch<S>> {
        let mut search = Search {
            eg,
            allowed,
            // A pattern of nothing but holes binds no e-node, so freshness has
            // nothing to read and the filter would drop every match. Nor is
            // there anything to read under a scope: a scoped rebuild leaves the
            // base rows alone by design, so a match the hypothesis newly enabled
            // binds e-nodes that are all older than the round.
            only_new: only_new && self.nodes.iter().any(|n| matches!(n, PatternNode::Node(_))),
            goals: Vec::new(),
            subst: SmallVec::new(),
            bound: SmallVec::from_elem(None, self.nodes.len()),
            fresh: 0,
            out: Vec::new(),
        };
        let mut seen: HashSet<Id, FxBuildHasher> = HashSet::default();
        for root in roots {
            let root = eg.find(root);
            if !seen.insert(root) {
                continue;
            }
            search.goals.push((self.root, root));
            self.solve(&mut search, root);
            search.goals.clear();
        }
        search.out
    }

    /// Depth-first backtracking e-matcher: pops one goal off the `(pattern node,
    /// e-class)` stack, explores every solution restoring the search state
    /// between branches, then restores the goal for the caller.
    fn solve<'p>(&'p self, s: &mut Search<'p, '_, N, S>, root: Id) {
        let eg = s.eg;
        let Some((pat, class)) = s.goals.pop() else {
            if !s.only_new || s.fresh > 0 {
                s.out.push(EMatch {
                    root,
                    subst: Substitution {
                        vec: s.subst.iter().map(|&(v, id)| (v.clone(), id)).collect(),
                    },
                    bindings: s.bound.clone(),
                });
            }
            return;
        };
        let class = eg.find(class);
        // A pattern node shared by several parents (a DAG pattern) must bind the
        // same class at every occurrence.
        let previous = s.bound[pat.index()];
        let consistent = previous.is_none_or(|existing| eg.find(existing) == class);
        if consistent && (s.allowed)(pat, class) {
            s.bound[pat.index()] = Some(class);
            let mark = s.goals.len();
            match &self.nodes[pat.index()] {
                PatternNode::Var(var @ Var::Symbol(_)) => {
                    match s.subst.iter().find(|(v, _)| *v == var).map(|&(_, id)| id) {
                        Some(prior) if eg.find(prior) != class => {}
                        Some(_) => self.solve(s, root),
                        None => {
                            s.subst.push((var, class));
                            self.solve(s, root);
                            s.subst.pop();
                        }
                    }
                }
                PatternNode::Var(Var::Int(v)) => {
                    self.solve_const(s, root, N::from_int(v.clone()), class);
                }
                PatternNode::Var(Var::Float(v)) => {
                    self.solve_const(s, root, N::from_float(v.clone()), class);
                }
                PatternNode::Node(template) => {
                    let tchildren = template.children();
                    for row in eg.rows(class) {
                        let node_children = eg.children(row);
                        if !template.matches_template(eg.node(row))
                            || tchildren.len() != node_children.len()
                        {
                            continue;
                        }
                        // A commutative binary operator matches in both operand
                        // orders.
                        let orders = if eg.node(row).commutative() && node_children.len() == 2 {
                            2
                        } else {
                            1
                        };
                        let fresh = usize::from(eg.row_is_new(row));
                        s.fresh += fresh;
                        for order in 0..orders {
                            for (slot, pc) in tchildren.iter().enumerate().rev() {
                                let ec = if order == 1 {
                                    node_children[1 - slot]
                                } else {
                                    node_children[slot]
                                };
                                s.goals.push((*pc, eg.find(ec)));
                            }
                            self.solve(s, root);
                            s.goals.truncate(mark);
                        }
                        s.fresh -= fresh;
                    }
                }
            }
            s.bound[pat.index()] = previous;
        }
        s.goals.push((pat, class));
    }

    /// Continue only if `class` holds `target` as a literal or is assumed to
    /// evaluate to it. An assumption is a fact rather than an e-node, so it has
    /// no round to be new in and always counts as fresh — a rule reading one
    /// must never be skipped as already applied.
    fn solve_const<'p>(
        &'p self,
        s: &mut Search<'p, '_, N, S>,
        root: Id,
        target: Option<N>,
        class: Id,
    ) {
        let Some(target) = target else { return };
        let eg = s.eg;
        if eg.assumed_const(class).is_some_and(|n| target.matches(n)) {
            s.fresh += 1;
            self.solve(s, root);
            s.fresh -= 1;
            return;
        }
        let Some(row) = eg
            .rows(class)
            .find(|&row| eg.children(row).is_empty() && target.matches(eg.node(row)))
        else {
            return;
        };
        let fresh = usize::from(eg.row_is_new(row));
        s.fresh += fresh;
        self.solve(s, root);
        s.fresh -= fresh;
    }

    /// Build this pattern into `eg` under `subst`, returning the root e-class.
    pub fn instantiate(&self, eg: &mut EGraph<N>, subst: &Substitution<S>) -> Id {
        let mut ids: Vec<Id> = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let id = match node {
                PatternNode::Var(var @ Var::Symbol(_)) => {
                    subst.get(var).expect("unbound pattern variable")
                }
                PatternNode::Var(Var::Int(v)) => {
                    let node = N::from_int(v.clone()).expect("language has no integer constants");
                    eg.add(node)
                }
                PatternNode::Var(Var::Float(v)) => {
                    let node = N::from_float(v.clone()).expect("language has no float constants");
                    eg.add(node)
                }
                PatternNode::Node(template) => {
                    let mut node = template.clone();
                    for child in node.children_mut() {
                        *child = ids[child.index()];
                    }
                    eg.add(node)
                }
            };
            ids.push(id);
        }
        ids[self.root.index()]
    }
}

/// State one [`Pattern::search_roots_delta`] threads through the backtracking.
/// Bindings borrow the pattern's `Var`s; names are cloned only when a full match
/// is emitted.
struct Search<'p, 'e, N: ENode, S> {
    eg: &'e EGraph<N>,
    allowed: &'e dyn Fn(Id, Id) -> bool,
    only_new: bool,
    goals: Vec<(Id, Id)>,
    subst: SmallVec<[(&'p Var<S>, Id); 4]>,
    bound: SmallVec<[Option<Id>; 8]>,
    /// How many e-nodes of the partial match the previous round touched.
    fresh: usize,
    out: Vec<EMatch<S>>,
}
