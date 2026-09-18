//! Which variables are still needed where.
//!
//! Emission threads a variable through a region port only where the region
//! assigns it and something after the region reads it, so the ports a
//! structured operation grows are exactly the live ones.

use std::collections::BTreeSet;

use super::cfg::{Cfg, Edge, NodeId, Rhs, Src, Term, VarId, deep_operands};

pub struct Liveness {
    live_in: Vec<BTreeSet<VarId>>,
}

impl Liveness {
    /// The variables live on entry to `node`.
    pub fn at(&self, node: NodeId) -> &BTreeSet<VarId> {
        &self.live_in[node]
    }

    /// The variables live where `edge` leaves its node.
    pub fn along(&self, cfg: &Cfg, edge: &Edge) -> BTreeSet<VarId> {
        through(cfg, &self.live_in, edge)
    }
}

/// The variables live where `edge` leaves its node: what its target needs,
/// minus what the edge assigns, plus what those assignments read.
fn through(cfg: &Cfg, live_in: &[BTreeSet<VarId>], edge: &Edge) -> BTreeSet<VarId> {
    let mut live = live_in[edge.target].clone();
    for (var, _) in &edge.assigns {
        live.remove(var);
    }
    for (var, rhs) in &edge.assigns {
        if live_in[edge.target].contains(var)
            && let Rhs::Value(value) = rhs
            && let Some(var) = cfg.value_var.get(value).copied()
        {
            live.insert(var);
        }
    }
    live
}

pub fn compute(cfg: &Cfg) -> Liveness {
    let mut live_in = vec![BTreeSet::new(); cfg.nodes.len()];
    loop {
        let mut changed = false;
        for node in (0..cfg.nodes.len()).rev() {
            let computed = transfer(cfg, &live_in, node);
            if computed != live_in[node] {
                live_in[node] = computed;
                changed = true;
            }
        }
        if !changed {
            return Liveness { live_in };
        }
    }
}

fn var_of(cfg: &Cfg, value: crate::ValueId) -> Option<VarId> {
    cfg.value_var.get(&value).copied()
}

fn out(cfg: &Cfg, live_in: &[BTreeSet<VarId>], node: NodeId) -> BTreeSet<VarId> {
    let mut live = BTreeSet::new();
    for edge in cfg.edges(node) {
        live.extend(through(cfg, live_in, edge));
    }
    live
}

fn transfer(cfg: &Cfg, live_in: &[BTreeSet<VarId>], node: NodeId) -> BTreeSet<VarId> {
    let mut live = out(cfg, live_in, node);
    match &cfg.nodes[node].term {
        Term::Sink { op, args } => match args {
            Some(args) => live.extend(args.iter().copied()),
            None => {
                for value in deep_operands(&cfg.context, *op) {
                    live.extend(var_of(cfg, value));
                }
            }
        },
        Term::Cond { pred, .. } | Term::LoopTail { pred, .. } => match pred {
            Src::Value(value) => live.extend(var_of(cfg, *value)),
            Src::Var(var) => {
                live.insert(*var);
            }
        },
        Term::Dispatch { var, .. } => {
            live.insert(*var);
        }
        Term::Jump(_) => {}
    }
    for op in cfg.ops(node).into_iter().rev() {
        let instance = cfg.context.get_op(op);
        for result in instance.results() {
            if let Some(var) = var_of(cfg, result) {
                live.remove(&var);
            }
        }
        for value in deep_operands(&cfg.context, op) {
            live.extend(var_of(cfg, value));
        }
    }
    for (var, rhs) in cfg.nodes[node].assigns.iter().rev() {
        live.remove(var);
        if let Rhs::Value(value) = rhs {
            live.extend(var_of(cfg, *value));
        }
    }
    live
}
