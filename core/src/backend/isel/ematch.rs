//! Pattern matching over the semantic e-graph: a DAG [`Pattern`] is solved against every e-class, binding
//! *every* pattern node (not just variables) so the PBQP cover can internalize interior matches; honors
//! operand `Register`/`Immediate` constraints, commutative operators, and a legality predicate.

use tir::Context;
use tir::graph::{Matchable, NodeId, OperandConstraint, Pattern, PatternExpr};
use tir_symbolic::egraph::{ENode, Id};

use super::node::{SemEGraph, SemNode};

/// One match of a [`Pattern`]: the matched root class and the e-class bound to each pattern node.
#[derive(Clone, Debug)]
pub struct EMatch {
    root: Id,
    bindings: Vec<Id>,
}

impl EMatch {
    pub fn root(&self) -> Id {
        self.root
    }

    pub fn binding(&self, pattern_node: NodeId) -> Id {
        self.bindings[pattern_node.index()]
    }
}

/// Every match of `pattern` across the whole e-graph.
pub(crate) fn ematch<A>(
    eg: &SemEGraph,
    ctx: &Context,
    pattern: &Pattern<SemNode, A>,
) -> Vec<EMatch> {
    ematch_with_legality(eg, ctx, pattern, &|_, _| true)
}

/// Every match of `pattern`, filtered by `allowed(pattern_node, class)`: a disallowed binding prunes that branch.
pub(crate) fn ematch_with_legality<A>(
    eg: &SemEGraph,
    ctx: &Context,
    pattern: &Pattern<SemNode, A>,
    allowed: &dyn Fn(NodeId, Id) -> bool,
) -> Vec<EMatch> {
    let Some(root) = pattern.root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for class in eg.classes() {
        let class = class.id();
        for binding in solve(eg, ctx, pattern, root, class, allowed) {
            out.push(EMatch {
                root: class,
                bindings: binding.into_iter().map(Option::unwrap).collect(),
            });
        }
    }
    out
}

/// The child e-classes of an e-node, canonicalized.
fn node_children(eg: &SemEGraph, node: &SemNode) -> Vec<Id> {
    node.children().iter().map(|&c| eg.find(c)).collect()
}

fn solve<A>(
    eg: &SemEGraph,
    ctx: &Context,
    pattern: &Pattern<SemNode, A>,
    pattern_node: NodeId,
    class: Id,
    allowed: &dyn Fn(NodeId, Id) -> bool,
) -> Vec<Vec<Option<Id>>> {
    let class = eg.find(class);

    if !allowed(pattern_node, class) {
        return Vec::new();
    }

    // The single solution that binds this pattern node to `class` and nothing else.
    let bind_self = || {
        let mut b = vec![None; pattern.len()];
        b[pattern_node.index()] = Some(class);
        vec![b]
    };

    match pattern.get_node(pattern_node) {
        PatternExpr::Boundary => {
            if boundary_ok(eg, pattern, pattern_node, class) {
                bind_self()
            } else {
                Vec::new()
            }
        }
        PatternExpr::Any => bind_self(),
        PatternExpr::Leaf => {
            if class_has_leaf(eg, ctx, class) {
                bind_self()
            } else {
                Vec::new()
            }
        }
        PatternExpr::Node(template) => {
            let children = pattern.children(pattern_node).to_vec();
            let commutative = template.is_commutative() && children.len() == 2;
            let mut results = Vec::new();

            for node in eg.nodes(class) {
                let node_children = node_children(eg, node);
                if node_children.len() != children.len() {
                    continue;
                }
                if !node.matches_pattern(template, ctx) {
                    continue;
                }

                let orders: &[Vec<Id>] = &if commutative {
                    vec![
                        node_children.clone(),
                        vec![node_children[1], node_children[0]],
                    ]
                } else {
                    vec![node_children]
                };

                for order in orders {
                    for combo in solve_children(eg, ctx, pattern, &children, order, allowed) {
                        let mut b = combo;
                        match b[pattern_node.index()] {
                            Some(existing) if existing != class => continue,
                            _ => b[pattern_node.index()] = Some(class),
                        }
                        results.push(b);
                    }
                }
            }
            results
        }
    }
}

fn solve_children<A>(
    eg: &SemEGraph,
    ctx: &Context,
    pattern: &Pattern<SemNode, A>,
    pattern_children: &[NodeId],
    class_children: &[Id],
    allowed: &dyn Fn(NodeId, Id) -> bool,
) -> Vec<Vec<Option<Id>>> {
    let mut acc: Vec<Vec<Option<Id>>> = vec![vec![None; pattern.len()]];
    for (&pc, &cc) in pattern_children.iter().zip(class_children.iter()) {
        let child_solutions = solve(eg, ctx, pattern, pc, cc, allowed);
        let mut next = Vec::new();
        for base in &acc {
            for sol in &child_solutions {
                if let Some(merged) = merge_bindings(base, sol) {
                    next.push(merged);
                }
            }
        }
        acc = next;
        if acc.is_empty() {
            break;
        }
    }
    acc
}

fn boundary_ok<A>(
    eg: &SemEGraph,
    pattern: &Pattern<SemNode, A>,
    pattern_node: NodeId,
    class: Id,
) -> bool {
    match pattern.operand_constraint(pattern_node) {
        Some(OperandConstraint::Register) => eg.nodes(class).iter().any(|n| !n.is_constant()),
        Some(OperandConstraint::Immediate) => eg.nodes(class).iter().any(|n| n.is_constant()),
        None => true,
    }
}

fn class_has_leaf(eg: &SemEGraph, ctx: &Context, class: Id) -> bool {
    eg.nodes(class).iter().any(|n| n.is_leaf(ctx))
}

fn merge_bindings(a: &[Option<Id>], b: &[Option<Id>]) -> Option<Vec<Option<Id>>> {
    let mut out = a.to_vec();
    for (slot, &value) in out.iter_mut().zip(b.iter()) {
        match (*slot, value) {
            (Some(x), Some(y)) if x != y => return None,
            (None, Some(y)) => *slot = Some(y),
            _ => {}
        }
    }
    Some(out)
}
