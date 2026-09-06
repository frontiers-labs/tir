//! The control-flow graph the restructuring phases rewrite.
//!
//! A node holds the operations of one original block (or nothing, when
//! restructuring created it) plus a terminator describing how control leaves
//! it. Everything that has to survive a control transfer — a block argument, a
//! predicate the restructuring invents, a value read outside the node that
//! defines it — is a *variable*: the graph moves variables, the emission phase
//! turns them back into values by threading them through region ports.

use std::collections::{BTreeMap, BTreeSet};

use crate::{
    BlockId, BranchGuard, BranchTerminator, Context, OpId, PassError, RegionId, Terminator, TypeId,
    ValueId,
};

pub type NodeId = usize;
pub type VarId = usize;

/// What a variable is read from when an edge assigns it.
#[derive(Clone, Copy, Debug)]
pub enum Rhs {
    Value(ValueId),
    Const(i64),
}

/// Where a decision reads its predicate.
#[derive(Clone, Copy, Debug)]
pub enum Src {
    Value(ValueId),
    Var(VarId),
}

#[derive(Clone, Debug)]
pub struct Edge {
    pub target: NodeId,
    pub assigns: Vec<(VarId, Rhs)>,
}

impl Edge {
    pub fn new(target: NodeId) -> Self {
        Self {
            target,
            assigns: Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Term {
    /// Control leaves the region through this operation. `args` names the
    /// variables its operands read where several exits were merged into one.
    Sink {
        op: OpId,
        args: Option<Vec<VarId>>,
    },
    Jump(Edge),
    Cond {
        pred: Src,
        if_true: Edge,
        if_false: Edge,
    },
    Dispatch {
        var: VarId,
        arms: Vec<(i64, Edge)>,
        default: Edge,
    },
    /// The tail of a restructured loop: repeat the body or leave it.
    LoopTail {
        pred: Src,
        repeat: Edge,
        exit: Edge,
    },
}

#[derive(Clone, Debug)]
pub struct Node {
    /// The block whose operations run here; `None` for a node restructuring
    /// created, which only assigns predicates.
    pub block: Option<BlockId>,
    /// Assignments performed on entry to the node.
    pub assigns: Vec<(VarId, Rhs)>,
    pub term: Term,
    /// Set on the node standing for a whole loop.
    pub loop_body: Option<LoopId>,
}

pub type LoopId = usize;

/// A restructured loop: the node it is entered at carries its id, its body runs
/// from `body_entry` to `tail`, and the tail's exit edge says where control
/// continues.
#[derive(Clone, Debug)]
pub struct Loop {
    pub body_entry: NodeId,
    pub tail: NodeId,
}

pub struct Cfg {
    pub context: Context,
    pub nodes: Vec<Node>,
    pub var_types: Vec<TypeId>,
    pub loops: Vec<Loop>,
    pub entry: NodeId,
    /// The one node control leaves the region from.
    pub sink: NodeId,
    /// The variable an original value became, for the values read outside the
    /// node defining them.
    pub value_var: BTreeMap<ValueId, VarId>,
    /// The variable carrying memory order from node to node, when the graph
    /// constructed it; it is bound at the region's entry to the memory the
    /// region is entered with.
    pub chain: Option<VarId>,
}

impl Cfg {
    pub fn add_var(&mut self, ty: TypeId) -> VarId {
        self.var_types.push(ty);
        self.var_types.len() - 1
    }

    pub fn add_node(&mut self, node: Node) -> NodeId {
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    /// A node with no operations, jumping to `target`.
    pub fn add_synthetic(&mut self, target: NodeId) -> NodeId {
        self.add_node(Node {
            block: None,
            assigns: Vec::new(),
            term: Term::Jump(Edge::new(target)),
            loop_body: None,
        })
    }

    pub fn int_type(&self, width: u32) -> TypeId {
        crate::builtin::IntegerType::new(&self.context, width)
    }

    /// The edges leaving `node`, in a fixed order.
    pub fn edges(&self, node: NodeId) -> Vec<&Edge> {
        match &self.nodes[node].term {
            Term::Sink { .. } => vec![],
            Term::Jump(edge) => vec![edge],
            Term::Cond {
                if_true, if_false, ..
            } => vec![if_true, if_false],
            Term::Dispatch { arms, default, .. } => {
                arms.iter().map(|(_, edge)| edge).chain([default]).collect()
            }
            Term::LoopTail { repeat, exit, .. } => vec![repeat, exit],
        }
    }

    pub fn edges_mut(&mut self, node: NodeId) -> Vec<&mut Edge> {
        match &mut self.nodes[node].term {
            Term::Sink { .. } => vec![],
            Term::Jump(edge) => vec![edge],
            Term::Cond {
                if_true, if_false, ..
            } => vec![if_true, if_false],
            Term::Dispatch { arms, default, .. } => arms
                .iter_mut()
                .map(|(_, edge)| edge)
                .chain([default])
                .collect(),
            Term::LoopTail { repeat, exit, .. } => vec![repeat, exit],
        }
    }

    pub fn successors(&self, node: NodeId) -> Vec<NodeId> {
        self.edges(node).iter().map(|edge| edge.target).collect()
    }

    /// The edge control leaves `node` by once the structure has been read off:
    /// a loop is one node whose body is a region of its own, so it is left
    /// through its tail's exit edge.
    pub fn structural_edge_owner(&self, node: NodeId) -> NodeId {
        match self.nodes[node].loop_body {
            Some(id) => self.loops[id].tail,
            None => node,
        }
    }

    pub fn structural_successors(&self, node: NodeId) -> Vec<NodeId> {
        match self.nodes[node].loop_body {
            Some(id) => match &self.nodes[self.loops[id].tail].term {
                Term::LoopTail { exit, .. } => vec![exit.target],
                _ => unreachable!("a loop tail terminates its body"),
            },
            None => self.successors(node),
        }
    }

    /// Apply `edit` to every edge that leaves `node` structurally.
    pub fn edit_structural_edges(&mut self, node: NodeId, mut edit: impl FnMut(&mut Edge)) {
        let owner = self.structural_edge_owner(node);
        if owner != node {
            if let Term::LoopTail { exit, .. } = &mut self.nodes[owner].term {
                edit(exit);
            }
            return;
        }
        for edge in self.edges_mut(node) {
            edit(edge);
        }
    }

    /// The operations of `node`, its original terminator excluded: a branch is
    /// replaced by the structure, and an exit is emitted from its own statement.
    pub fn ops(&self, node: NodeId) -> Vec<OpId> {
        let Some(block) = self.nodes[node].block else {
            return vec![];
        };
        let mut ops = self.context.get_block(block).op_ids();
        ops.pop();
        ops
    }
}

/// Every value an operation and its nested regions read.
pub fn deep_operands(context: &Context, op: OpId) -> Vec<ValueId> {
    let mut operands = Vec::new();
    let mut pending = vec![op];
    while let Some(op) = pending.pop() {
        let instance = context.get_op(op);
        operands.extend(instance.operands().iter().copied());
        for region in instance.regions() {
            pending.extend(context.get_region(region).op_ids());
        }
    }
    operands
}

struct Builder<'a> {
    context: &'a Context,
    cfg: Cfg,
    node_of_block: BTreeMap<BlockId, NodeId>,
    arg_var: BTreeMap<ValueId, VarId>,
}

impl Cfg {
    /// Read `region`'s blocks into a graph. With `thread`, memory order is
    /// constructed while the blocks are still there to say what it is: every
    /// effect takes and leaves a dependency, and the chain crosses each edge as
    /// a variable the emission turns back into ports.
    pub fn build(context: &Context, region: RegionId, thread: bool) -> Result<Cfg, PassError> {
        let blocks = context.get_region(region).block_ids();
        let mut builder = Builder {
            context,
            cfg: Cfg {
                context: context.clone(),
                nodes: Vec::new(),
                var_types: Vec::new(),
                loops: Vec::new(),
                entry: 0,
                sink: 0,
                value_var: BTreeMap::new(),
                chain: None,
            },
            node_of_block: BTreeMap::new(),
            arg_var: BTreeMap::new(),
        };
        builder.create_nodes(&blocks);
        builder.create_argument_vars(&blocks);
        builder.build_terminators(&blocks)?;
        builder.add_preheader(blocks[0]);
        builder.unify_sinks()?;
        if thread {
            builder.thread_memory()?;
        }
        builder.create_value_vars();
        Ok(builder.cfg)
    }
}

impl Builder<'_> {
    fn create_nodes(&mut self, blocks: &[BlockId]) {
        for &block in blocks {
            let node = self.cfg.add_node(Node {
                block: Some(block),
                assigns: Vec::new(),
                term: Term::Jump(Edge::new(0)),
                loop_body: None,
            });
            self.node_of_block.insert(block, node);
        }
    }

    /// Every block argument becomes a variable, the entry block's parameters
    /// included: a branch back to the entry block gives them a second value,
    /// and the preheader binds them to the parameters themselves.
    fn create_argument_vars(&mut self, blocks: &[BlockId]) {
        for &block in blocks {
            for argument in self.context.get_block(block).arguments() {
                let var = self.cfg.add_var(argument.ty());
                self.arg_var.insert(argument.id(), var);
            }
        }
    }

    fn build_terminators(&mut self, blocks: &[BlockId]) -> Result<(), PassError> {
        for &block in blocks {
            let node = self.node_of_block[&block];
            let ops = self.context.get_block(block).op_ids();
            let op = *ops
                .last()
                .ok_or_else(|| unsupported("a block with no terminator"))?;
            self.cfg.nodes[node].term = self.terminator(op)?;
        }
        Ok(())
    }

    fn terminator(&self, op: OpId) -> Result<Term, PassError> {
        let instance = self.context.get_op(op);
        let Some(branch) = instance.clone().as_interface::<dyn BranchTerminator>() else {
            let leaves = instance
                .clone()
                .as_interface::<dyn Terminator>()
                .is_some_and(|terminator| terminator.successors().is_empty());
            return if leaves {
                Ok(Term::Sink { op, args: None })
            } else {
                Err(unsupported("a terminator that is not a branch"))
            };
        };
        let successors = branch.successor_operands();
        let edges = successors
            .iter()
            .map(|(block, arguments)| self.edge(*block, arguments))
            .collect::<Vec<_>>();
        match edges.len() {
            0 => Ok(Term::Sink { op, args: None }),
            1 => Ok(Term::Jump(edges[0].clone())),
            2 => {
                let guard = instance
                    .as_interface::<dyn BranchGuard>()
                    .ok_or_else(|| unsupported("a two-way branch without a guard"))?
                    .guarded_successors();
                let [(_, value, first_taken_when), (_, other, _)] = guard[..] else {
                    return Err(unsupported("a guard that does not describe both edges"));
                };
                if value != other {
                    return Err(unsupported("a two-way branch guarded by two values"));
                }
                let (if_true, if_false) = match first_taken_when {
                    true => (edges[0].clone(), edges[1].clone()),
                    false => (edges[1].clone(), edges[0].clone()),
                };
                Ok(Term::Cond {
                    pred: Src::Value(value),
                    if_true,
                    if_false,
                })
            }
            _ => Err(unsupported("a branch with more than two successors")),
        }
    }

    fn edge(&self, block: BlockId, arguments: &[ValueId]) -> Edge {
        let parameters = self.context.get_block(block).arguments().to_vec();
        Edge {
            target: self.node_of_block[&block],
            assigns: parameters
                .iter()
                .zip(arguments)
                .filter_map(|(parameter, &argument)| {
                    Some((*self.arg_var.get(&parameter.id())?, Rhs::Value(argument)))
                })
                .collect(),
        }
    }

    /// Give the entry block a predecessor-free node in front of it, so that a
    /// loop closing back onto the entry has somewhere to put its head. It binds
    /// the entry block's argument variables to the region's own arguments.
    fn add_preheader(&mut self, entry: BlockId) {
        let assigns = self
            .context
            .get_block(entry)
            .arguments()
            .iter()
            .map(|argument| (self.arg_var[&argument.id()], Rhs::Value(argument.id())))
            .collect();
        let entry = self.node_of_block[&entry];
        let preheader = self.cfg.add_synthetic(entry);
        self.cfg.nodes[preheader].assigns = assigns;
        self.cfg.entry = preheader;
    }

    /// One dependency chain through every block, as one variable: each block
    /// is entered on a dependency argument standing for it, threads its
    /// effects off that, and the memory the block leaves is the variable's
    /// next value, the way any result read past its node is. The exit exports
    /// the chain to the caller; where several exits were merged, the merged
    /// one reads the variable as it stands there.
    fn thread_memory(&mut self) -> Result<(), PassError> {
        let chain = self.cfg.add_var(TypeId::DEPENDENCY);
        for (block, node) in self.node_of_block.clone() {
            let entry = self.context.append_dep_block_argument(block).id();
            self.arg_var.insert(entry, chain);
            let leaving = super::deps::thread_block(self.context, block, entry)?;
            if let Term::Sink { op, .. } = &self.cfg.nodes[node].term {
                self.context.append_dep_operand(*op, leaving);
            }
            if leaving != entry {
                self.cfg.value_var.insert(leaving, chain);
            }
        }
        if let Term::Sink {
            args: Some(args), ..
        } = &mut self.cfg.nodes[self.cfg.sink].term
        {
            args.push(chain);
        }
        self.cfg.chain = Some(chain);
        Ok(())
    }

    /// Leave the region through one node: the extra exits keep their values by
    /// assigning them to variables the surviving exit reads.
    fn unify_sinks(&mut self) -> Result<(), PassError> {
        let sinks = (0..self.cfg.nodes.len())
            .filter(|&node| matches!(self.cfg.nodes[node].term, Term::Sink { .. }))
            .collect::<Vec<_>>();
        let (&first, rest) = sinks
            .split_first()
            .ok_or_else(|| unsupported("a region that never leaves"))?;
        let Term::Sink { op, .. } = self.cfg.nodes[first].term else {
            unreachable!()
        };
        if rest.is_empty() {
            self.cfg.sink = first;
            return Ok(());
        }

        let kept = self.context.get_op(op);
        let vars = kept
            .operands()
            .iter()
            .map(|&operand| self.cfg.add_var(self.context.get_value(operand).ty()))
            .collect::<Vec<_>>();
        let sink = self.cfg.add_node(Node {
            block: None,
            assigns: Vec::new(),
            term: Term::Sink {
                op,
                args: Some(vars.clone()),
            },
            loop_body: None,
        });
        self.cfg.sink = sink;
        for &node in &sinks {
            let Term::Sink { op, .. } = self.cfg.nodes[node].term else {
                unreachable!()
            };
            let exit = self.context.get_op(op);
            if exit.operands().len() != vars.len() || exit.name() != kept.name() {
                return Err(unsupported("a region with unlike exits"));
            }
            let assigns = vars
                .iter()
                .zip(exit.operands().iter())
                .map(|(&var, &operand)| (var, Rhs::Value(operand)))
                .collect();
            self.cfg.nodes[node].term = Term::Jump(Edge {
                target: sink,
                assigns,
            });
        }
        Ok(())
    }

    /// A value read outside the node defining it cannot stay a plain value: the
    /// structure may close the region it is defined in before the read happens.
    fn create_value_vars(&mut self) {
        let mut definition = BTreeMap::new();
        let mut reads: BTreeMap<ValueId, BTreeSet<NodeId>> = BTreeMap::new();
        for node in 0..self.cfg.nodes.len() {
            for op in self.cfg.ops(node) {
                for result in self.context.get_op(op).results() {
                    definition.insert(result, node);
                }
            }
        }
        for node in 0..self.cfg.nodes.len() {
            let mut record = |value: ValueId| {
                reads.entry(value).or_default().insert(node);
            };
            for op in self.cfg.ops(node) {
                for value in deep_operands(self.context, op) {
                    record(value);
                }
            }
            match &self.cfg.nodes[node].term {
                // An exit whose operands were unified reads variables, not the
                // values the operands still name.
                Term::Sink { op, args: None } => {
                    for value in deep_operands(self.context, *op) {
                        record(value);
                    }
                }
                Term::Cond {
                    pred: Src::Value(value),
                    ..
                } => record(*value),
                _ => {}
            }
            for edge in self.cfg.edges(node) {
                for &(_, rhs) in &edge.assigns {
                    if let Rhs::Value(value) = rhs {
                        reads.entry(value).or_default().insert(node);
                    }
                }
            }
        }

        for (value, nodes) in reads {
            let Some(&defined) = definition.get(&value) else {
                continue;
            };
            if nodes.iter().all(|&node| node == defined) {
                continue;
            }
            let var = self.cfg.add_var(self.context.get_value(value).ty());
            self.cfg.value_var.insert(value, var);
        }
        for (&argument, &var) in &self.arg_var {
            self.cfg.value_var.insert(argument, var);
        }
    }
}

pub fn unsupported(what: &str) -> PassError {
    PassError::InvalidRuleSet(format!("restructure does not support {what}"))
}
