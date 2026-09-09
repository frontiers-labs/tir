//! Which variables a structured operation carries: those its regions assign
//! and something after it reads, states trailing the values.

use std::collections::BTreeSet;

use super::branches::Stmt;
use super::cfg::{LoopId, Term, VarId};
use super::emit_nodes::Emitter;
use crate::TypeId;

impl Emitter<'_> {
    /// The variables a structured operation has to carry: those its regions
    /// assign and something after it reads.
    pub(super) fn ports(&self, arms: &[&[Stmt]], needed: BTreeSet<VarId>) -> Vec<VarId> {
        let mut assigned = BTreeSet::new();
        for arm in arms {
            assigned.extend(self.assigned(arm));
        }
        let ports: Vec<VarId> = assigned.intersection(&needed).copied().collect();
        self.deps_last(&ports)
    }

    /// The variables a loop carries: what its body needs and what leaves it.
    pub(super) fn loop_ports(&self, id: LoopId, body: &[Stmt]) -> Vec<VarId> {
        let tail = self.cfg.loops[id].tail;
        let mut needed = self.live.at(self.cfg.loops[id].body_entry).clone();
        if let Term::LoopTail { exit, .. } = &self.cfg.nodes[tail].term {
            needed.extend(self.live.along(self.cfg, exit));
        }
        self.ports(&[body], needed)
    }

    /// `ports` with the states moved after the values: the order every port
    /// list keeps its groups in.
    pub(super) fn deps_last(&self, ports: &[VarId]) -> Vec<VarId> {
        let is_state = |var: &VarId| self.cfg.var_types[*var] == TypeId::STATE;
        let mut ordered: Vec<VarId> = ports.iter().copied().filter(|var| !is_state(var)).collect();
        ordered.extend(ports.iter().copied().filter(is_state));
        ordered
    }

    /// The ports that are memory states.
    pub(super) fn state_ports(&self, ports: &[VarId]) -> Vec<VarId> {
        ports
            .iter()
            .copied()
            .filter(|&var| self.cfg.var_types[var] == TypeId::STATE)
            .collect()
    }

    /// The type of every port, `!state` for the chains among them.
    pub(super) fn port_types(&self, ports: &[VarId]) -> Vec<TypeId> {
        ports.iter().map(|&var| self.cfg.var_types[var]).collect()
    }

    /// The variables a statement tree leaves with a new value.
    pub(super) fn assigned(&self, statements: &[Stmt]) -> BTreeSet<VarId> {
        let mut assigned = BTreeSet::new();
        for statement in statements {
            match statement {
                Stmt::Node(node) => {
                    assigned.extend(self.cfg.nodes[*node].assigns.iter().map(|(var, _)| *var));
                    for op in self.cfg.ops(*node) {
                        for result in self.context.get_op(op).results() {
                            assigned.extend(self.cfg.value_var.get(&result).copied());
                        }
                    }
                }
                Stmt::Assign(assigns) => assigned.extend(assigns.iter().map(|(var, _)| *var)),
                Stmt::If {
                    then_arm,
                    else_arm,
                    continuation,
                    ..
                } => assigned
                    .extend(self.ports(&[then_arm, else_arm], self.live.at(*continuation).clone())),
                Stmt::Switch {
                    arms,
                    default,
                    continuation,
                    ..
                } => {
                    let bodies = arms
                        .iter()
                        .map(|(_, arm)| arm.as_slice())
                        .chain([default.as_slice()])
                        .collect::<Vec<_>>();
                    assigned.extend(self.ports(&bodies, self.live.at(*continuation).clone()));
                }
                Stmt::Loop { id, body } => assigned.extend(self.loop_ports(*id, body)),
            }
        }
        assigned
    }
}
