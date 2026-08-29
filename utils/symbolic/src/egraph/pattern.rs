use std::sync::OnceLock;

use smallvec::SmallVec;
use tir_adt::{APFloat, APInt};
use tir_relational::{Atom, Plan, Query};

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
    /// The query this pattern is, compiled on first search and dropped by any
    /// edit. `None` inside once the language turns out to have no term for a
    /// literal the pattern demands, which no graph can match.
    lowered: OnceLock<Option<Lowered<N>>>,
}

/// A pattern as the engine evaluates it: one query variable per pattern node, so
/// a match's bindings are indexed by pattern node and the legality hook still
/// speaks of pattern nodes.
#[derive(Clone, Debug)]
struct Lowered<N: ENode> {
    plan: Plan<N>,
    /// Capture holes in the order the nest reaches them, which is the order the
    /// substitution lists them in.
    captures: Vec<Id>,
    /// Per pattern node, the node whose variable it shares: itself, or the first
    /// hole that named the same capture. A rule may write one name twice
    /// (`gamma(c, x, x)`), and those occurrences are one variable.
    alias: Vec<Id>,
    /// The inverse, minus the canonical node itself: what else the legality hook
    /// must be asked about when a variable binds.
    shared: Vec<Vec<Id>>,
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
            lowered: OnceLock::new(),
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
        self.lowered = OnceLock::new();
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
        self.lowered = OnceLock::new();
        id
    }
}

impl<N: ENode, S: PartialEq> Pattern<N, S> {
    /// The compiled query, or `None` when a literal the pattern demands has no
    /// term in the language — a pattern no graph can match.
    fn lowered(&self) -> Option<&Lowered<N>> {
        self.lowered.get_or_init(|| self.lower()).as_ref()
    }

    fn lower(&self) -> Option<Lowered<N>> {
        let mut captures = Vec::new();
        let mut alias: Vec<Id> = (0..self.nodes.len() as u32).map(Id::from_raw).collect();
        let mut shared = vec![Vec::new(); self.nodes.len()];
        self.name(
            self.root,
            &mut vec![false; self.nodes.len()],
            &mut captures,
            &mut alias,
            &mut shared,
        );

        let mut atoms = Vec::new();
        self.emit(
            self.root,
            &mut vec![false; self.nodes.len()],
            &alias,
            &mut atoms,
        )?;
        Some(Lowered {
            plan: Plan::compile(Query {
                vars: self.nodes.len() as u32,
                root: alias[self.root.index()].0,
                atoms,
            }),
            captures,
            alias,
            shared,
        })
    }

    /// Name the variables: a hole is one variable, and a rule that writes the
    /// same name twice (`gamma(c, x, x)`) gets one variable at both nodes — the
    /// equality the goal stack used to enforce through the substitution.
    fn name(
        &self,
        node: Id,
        seen: &mut [bool],
        captures: &mut Vec<Id>,
        alias: &mut [Id],
        shared: &mut [Vec<Id>],
    ) {
        if std::mem::replace(&mut seen[node.index()], true) {
            return;
        }
        match &self.nodes[node.index()] {
            PatternNode::Var(hole @ Var::Symbol(_)) => {
                let first = captures.iter().copied().find(|&first| {
                    matches!(&self.nodes[first.index()], PatternNode::Var(other) if other == hole)
                });
                match first {
                    Some(first) => {
                        alias[node.index()] = first;
                        shared[first.index()].push(node);
                    }
                    None => captures.push(node),
                }
            }
            PatternNode::Var(_) => {}
            PatternNode::Node(template) => {
                for &child in template.children() {
                    self.name(child, seen, captures, alias, shared);
                }
            }
        }
    }

    /// One atom per template node and per literal hole, in the order a
    /// depth-first walk from the root reaches them: the order the goal stack
    /// popped them in, and so the order the plan steps them in.
    fn emit(
        &self,
        node: Id,
        seen: &mut [bool],
        alias: &[Id],
        atoms: &mut Vec<Atom<N>>,
    ) -> Option<()> {
        if std::mem::replace(&mut seen[alias[node.index()].index()], true) {
            return Some(());
        }
        match &self.nodes[node.index()] {
            PatternNode::Var(Var::Symbol(_)) => {}
            PatternNode::Var(Var::Int(value)) => atoms.push(Atom::Literal {
                value: N::from_int(value.clone())?,
                class: node.0,
            }),
            PatternNode::Var(Var::Float(value)) => atoms.push(Atom::Literal {
                value: N::from_float(value.clone())?,
                class: node.0,
            }),
            PatternNode::Node(template) => {
                atoms.push(Atom::Node {
                    template: template.clone(),
                    args: template
                        .children()
                        .iter()
                        .map(|&child| alias[child.index()].0)
                        .collect(),
                    class: node.0,
                });
                for &child in template.children() {
                    self.emit(child, seen, alias, atoms)?;
                }
            }
        }
        Some(())
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
    /// instcombine applies.
    ///
    /// Sound only when the pattern is the whole match predicate
    /// ([`Rewrite::unconditional`](super::Rewrite::unconditional)), which is a
    /// stronger claim than the one that licenses narrowing the roots: an
    /// applier that declined left no trace, and what changes its mind may be the
    /// content of a class the pattern bound as a hole, which no e-node the match
    /// binds records.
    ///
    /// `allowed` is part of that predicate. A hook that rejects on class content
    /// is an applier that declines by another route, so `only_new` requires one
    /// that answers the same way whatever the graph has done since.
    pub fn search_roots_delta(
        &self,
        eg: &EGraph<N>,
        roots: impl IntoIterator<Item = Id>,
        allowed: &dyn Fn(Id, Id) -> bool,
        only_new: bool,
    ) -> Vec<EMatch<S>> {
        let Some(lowered) = self.lowered() else {
            return Vec::new();
        };
        // A variable two pattern nodes share is asked about under both names:
        // the hook answers per pattern node, and both occurrences carried one.
        let ask = |node: tir_relational::Var, class: Id| {
            let node = Id::from_raw(node);
            allowed(node, class)
                && lowered.shared[node.index()]
                    .iter()
                    .all(|&same| allowed(same, class))
        };
        lowered
            .plan
            .search(eg, roots, &ask, only_new)
            .into_iter()
            .map(|matched| {
                let mut bindings = matched.bindings;
                for (node, &named) in lowered.alias.iter().enumerate() {
                    if named.index() != node {
                        bindings[node] = bindings[named.index()];
                    }
                }
                EMatch {
                    root: matched.root,
                    subst: Substitution {
                        vec: lowered
                            .captures
                            .iter()
                            .map(|&hole| {
                                let PatternNode::Var(var) = &self.nodes[hole.index()] else {
                                    unreachable!("a capture is a hole")
                                };
                                (var.clone(), bindings[hole.index()].expect("bound"))
                            })
                            .collect(),
                    },
                    bindings,
                }
            })
            .collect()
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
