//! Branch restructuring: the acyclic graph loop restructuring leaves becomes a
//! tree of conditionals.
//!
//! A branch is structured against its *continuation*: the node where its arms
//! come back together. Where the arms come back together in several places, a
//! continuation variable names which one an arm wanted and one dispatch node
//! joins them, so no node is ever emitted twice — the tree below holds each
//! node at most once, which the emission asserts.

use std::collections::{BTreeMap, BTreeSet};

use super::cfg::{Cfg, Edge, LoopId, Node, NodeId, Rhs, Src, Term, VarId, unsupported};
use crate::PassError;

#[derive(Clone, Debug)]
pub enum Stmt {
    /// The operations of a node, preceded by the assignments it makes on entry.
    Node(NodeId),
    Assign(Vec<(VarId, Rhs)>),
    If {
        pred: Src,
        then_arm: Vec<Stmt>,
        else_arm: Vec<Stmt>,
        continuation: NodeId,
    },
    Switch {
        var: VarId,
        arms: Vec<(i64, Vec<Stmt>)>,
        default: Vec<Stmt>,
        continuation: NodeId,
    },
    Loop {
        id: LoopId,
        body: Vec<Stmt>,
    },
}

pub fn restructure(cfg: &mut Cfg) -> Result<Vec<Stmt>, PassError> {
    let mut structurer = Structurer {
        cfg,
        emitted: BTreeSet::new(),
    };
    let sink = structurer.cfg.sink;
    let entry = structurer.cfg.entry;
    let mut tree = structurer.region(entry, sink)?;
    structurer.claim(sink)?;
    tree.push(Stmt::Node(sink));
    Ok(tree)
}

struct Structurer<'a> {
    cfg: &'a mut Cfg,
    emitted: BTreeSet<NodeId>,
}

impl Structurer<'_> {
    fn claim(&mut self, node: NodeId) -> Result<(), PassError> {
        if self.emitted.insert(node) {
            return Ok(());
        }
        Err(unsupported("a graph that would need a node twice"))
    }

    /// The statements running from `start` until control reaches `stop`.
    fn region(&mut self, start: NodeId, stop: NodeId) -> Result<Vec<Stmt>, PassError> {
        let mut statements = Vec::new();
        let mut node = start;
        loop {
            if node == stop {
                return Ok(statements);
            }
            self.claim(node)?;
            statements.push(Stmt::Node(node));
            if let Some(id) = self.cfg.nodes[node].loop_body {
                let (body, exit) = self.loop_region(id)?;
                statements.push(Stmt::Loop { id, body });
                statements.push(Stmt::Assign(exit.assigns));
                node = exit.target;
                continue;
            }
            match self.cfg.nodes[node].term.clone() {
                // The one sink is where every region stops, so reaching it is
                // reaching a node the tree would have to hold twice.
                Term::Sink { .. } => return Err(unsupported("an exit inside a region")),
                Term::Jump(edge) => {
                    statements.push(Stmt::Assign(edge.assigns));
                    node = edge.target;
                }
                Term::LoopTail { .. } => {
                    return Err(unsupported("a loop tail outside its body"));
                }
                Term::Cond { .. } | Term::Dispatch { .. } => {
                    let continuation = self.continuation(node, stop);
                    statements.push(self.branch(node, continuation)?);
                    node = continuation;
                }
            }
        }
    }

    /// The body of a loop and the edge leaving it.
    fn loop_region(&mut self, id: LoopId) -> Result<(Vec<Stmt>, Edge), PassError> {
        let entry = self.cfg.loops[id].body_entry;
        let tail = self.cfg.loops[id].tail;
        let mut body = self.region(entry, tail)?;
        self.claim(tail)?;
        body.push(Stmt::Node(tail));
        let Term::LoopTail { exit, .. } = self.cfg.nodes[tail].term.clone() else {
            return Err(unsupported("a loop whose tail moved"));
        };
        Ok((body, exit))
    }

    fn branch(&mut self, node: NodeId, continuation: NodeId) -> Result<Stmt, PassError> {
        Ok(match self.cfg.nodes[node].term.clone() {
            Term::Cond {
                pred,
                if_true,
                if_false,
            } => Stmt::If {
                pred,
                then_arm: self.arm(if_true, continuation)?,
                else_arm: self.arm(if_false, continuation)?,
                continuation,
            },
            Term::Dispatch { var, arms, default } => Stmt::Switch {
                var,
                arms: arms
                    .into_iter()
                    .map(|(case, edge)| Ok((case, self.arm(edge, continuation)?)))
                    .collect::<Result<Vec<_>, PassError>>()?,
                default: self.arm(default, continuation)?,
                continuation,
            },
            _ => unreachable!("only a decision is structured as a branch"),
        })
    }

    fn arm(&mut self, edge: Edge, continuation: NodeId) -> Result<Vec<Stmt>, PassError> {
        let mut arm = vec![Stmt::Assign(edge.assigns)];
        arm.extend(self.region(edge.target, continuation)?);
        Ok(arm)
    }

    /// Where the arms of the branch at `node` come back together, inserting a
    /// dispatch node when they come back together in more than one place.
    fn continuation(&mut self, node: NodeId, stop: NodeId) -> NodeId {
        let arms = self.cfg.structural_successors(node);
        let reach = arms
            .iter()
            .map(|&arm| self.reachable(arm, stop))
            .collect::<Vec<_>>();
        let mut count: BTreeMap<NodeId, usize> = BTreeMap::new();
        for set in &reach {
            for &member in set {
                *count.entry(member).or_default() += 1;
            }
        }
        let shared = count
            .iter()
            .filter(|&(_, &times)| times > 1)
            .map(|(&member, _)| member)
            .collect::<BTreeSet<_>>();
        if shared.is_empty() {
            return stop;
        }
        let exclusive = count
            .keys()
            .copied()
            .filter(|member| !shared.contains(member))
            .collect::<BTreeSet<_>>();
        let joins = shared
            .iter()
            .copied()
            .filter(|&member| {
                arms.contains(&member)
                    || exclusive
                        .iter()
                        .any(|&source| self.cfg.structural_successors(source).contains(&member))
            })
            .collect::<Vec<_>>();
        match joins.len() {
            1 => joins[0],
            _ => self.join(node, &joins, &exclusive),
        }
    }

    /// One dispatch node standing for every place the arms rejoin at.
    fn join(&mut self, node: NodeId, joins: &[NodeId], exclusive: &BTreeSet<NodeId>) -> NodeId {
        let width = if joins.len() == 2 { 1 } else { 32 };
        let var = self.cfg.add_var(self.cfg.int_type(width));
        let (&default, cases) = joins.split_last().expect("a branch rejoins somewhere");
        let dispatch = self.cfg.add_node(Node {
            block: None,
            assigns: Vec::new(),
            term: Term::Dispatch {
                var,
                arms: cases
                    .iter()
                    .enumerate()
                    .map(|(index, &target)| (index as i64, Edge::new(target)))
                    .collect(),
                default: Edge::new(default),
            },
            loop_body: None,
        });
        for source in exclusive.iter().copied().chain([node]) {
            self.cfg.edit_structural_edges(source, |edge| {
                let Some(index) = joins.iter().position(|&join| join == edge.target) else {
                    return;
                };
                edge.target = dispatch;
                edge.assigns.push((var, Rhs::Const(index as i64)));
            });
        }
        dispatch
    }

    /// The nodes control can reach from `from` without passing `stop`, which is
    /// included but not left.
    fn reachable(&self, from: NodeId, stop: NodeId) -> BTreeSet<NodeId> {
        let mut seen = BTreeSet::from([from]);
        let mut pending = vec![from];
        while let Some(node) = pending.pop() {
            if node == stop {
                continue;
            }
            for successor in self.cfg.structural_successors(node) {
                if seen.insert(successor) {
                    pending.push(successor);
                }
            }
        }
        seen
    }
}
