//! Emission: the statement tree becomes structured operations in one block.
//!
//! Every original operation is *moved*, never copied. What the graph carries as
//! variables becomes values again here: a variable read after the region that
//! assigns it turns into a port of the structured operation — a result of an
//! `scf.if`/`scf.switch`, a carried port of an `scf.while` — and the constants
//! the dispatch and continuation predicates need are materialized where they
//! are assigned.

use std::collections::BTreeMap;

use super::branches::Stmt;
use super::cfg::{Cfg, NodeId, Rhs, Src, Term, VarId, unsupported};
use super::liveness::Liveness;
use super::ports::Ports;
use crate::builtin::ops as b;
use crate::{BlockId, Context, OpId, Operation, PassError, RegionId, TypeId, Value, ValueId, scf};

type Env = BTreeMap<VarId, ValueId>;

pub fn emit(
    context: &Context,
    region: RegionId,
    cfg: &Cfg,
    tree: &[Stmt],
    live: &Liveness,
) -> Result<(), PassError> {
    let entry = context.get_region(region).block_ids()[0];
    let arguments = context.get_block(entry).arguments().to_vec();
    let types = arguments.iter().map(Value::ty).collect::<Vec<_>>();

    let mut staged = context.stage_region();
    let block = staged.append_block(&types);
    for (index, argument) in arguments.iter().enumerate() {
        let replacement = staged.block_argument(block, index).id();
        staged.replace_value(argument.id(), replacement);
    }

    let emitter = Emitter {
        context,
        cfg,
        live,
        ports: Ports { context, cfg, live },
    };
    let mut env = Env::new();
    emitter.statements(tree, block, &mut env)?;
    context.replace_region_contents(region, staged);
    Ok(())
}

struct Emitter<'a> {
    context: &'a Context,
    cfg: &'a Cfg,
    live: &'a Liveness,
    ports: Ports<'a>,
}

impl Emitter<'_> {
    fn statements(
        &self,
        statements: &[Stmt],
        block: BlockId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        for statement in statements {
            self.statement(statement, block, env)?;
        }
        Ok(())
    }

    fn statement(&self, statement: &Stmt, block: BlockId, env: &mut Env) -> Result<(), PassError> {
        match statement {
            Stmt::Node(node) => self.node(*node, block, env),
            Stmt::Assign(assigns) => self.assign(assigns, block, env),
            Stmt::Exit { op, args } => self.exit(*op, args.as_deref(), block, env),
            Stmt::If {
                pred,
                then_arm,
                else_arm,
                continuation,
            } => self.conditional(
                Decision::If(*pred),
                &[then_arm, else_arm],
                *continuation,
                block,
                env,
            ),
            Stmt::Switch {
                var,
                arms,
                default,
                continuation,
            } => {
                let cases = arms.iter().map(|(case, _)| *case).collect::<Vec<_>>();
                let mut bodies = arms
                    .iter()
                    .map(|(_, arm)| arm.as_slice())
                    .collect::<Vec<_>>();
                bodies.push(default);
                self.conditional(
                    Decision::Switch(*var, cases),
                    &bodies,
                    *continuation,
                    block,
                    env,
                )
            }
            Stmt::Loop { id, body } => self.loop_op(*id, body, block, env),
        }
    }

    /// Move a node's operations into `block`, retargeting the operands whose
    /// definitions the structure has since closed away.
    fn node(&self, node: NodeId, block: BlockId, env: &mut Env) -> Result<(), PassError> {
        let assigns = self.cfg.nodes[node].assigns.clone();
        self.assign(&assigns, block, env)?;
        for op in self.cfg.ops(node) {
            self.bind_undefined_reads(op, block, env)?;
            self.retarget_operands(op, env);
            self.context.get_block(block).append(op);
            for result in self.context.get_op(op).results() {
                if let Some(&var) = self.cfg.value_var.get(&result) {
                    env.insert(var, result);
                }
            }
        }
        Ok(())
    }

    fn exit(
        &self,
        op: OpId,
        args: Option<&[VarId]>,
        block: BlockId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        match args {
            Some(args) => {
                let args = self.ports.deps_last(args);
                let operands = self.port_values(&args, block, env)?;
                self.context
                    .set_op_operands(op, operands, self.ports.dep_count(&args));
            }
            None => {
                self.bind_undefined_reads(op, block, env)?;
                self.retarget_operands(op, env);
            }
        }
        self.context.get_block(block).append(op);
        Ok(())
    }

    /// Assignments happen at once: every right-hand side is read before any
    /// variable takes its new value.
    fn assign(
        &self,
        assigns: &[(VarId, Rhs)],
        block: BlockId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        let mut written = Vec::with_capacity(assigns.len());
        for &(var, rhs) in assigns {
            let value = match rhs {
                Rhs::Value(value) => self.resolve(env, value),
                Rhs::Const(constant) => self.constant(block, constant, self.cfg.var_types[var])?,
            };
            written.push((var, value));
        }
        env.extend(written);
        Ok(())
    }

    fn conditional(
        &self,
        decision: Decision,
        arms: &[&[Stmt]],
        continuation: NodeId,
        block: BlockId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        let ports = self.ports.ports(arms, self.live.at(continuation).clone());
        let types = self.ports.value_types(&ports);
        let deps = self.ports.dep_count(&ports);
        let regions = arms
            .iter()
            .map(|arm| self.arm(arm, &ports, env))
            .collect::<Result<Vec<_>, _>>()?;

        let op = match decision {
            Decision::If(pred) => {
                let condition = self.read_src(block, env, pred)?;
                let mut gate = scf::IfOpBuilder::new(self.context)
                    .condition(condition)
                    .then_body(regions[0])
                    .else_body(regions[1])
                    .result_types(types);
                for _ in 0..deps {
                    gate = gate.dep_result();
                }
                gate.build().id()
            }
            Decision::Switch(var, cases) => {
                let predicate = self.read(block, env, var)?;
                let mut gate = scf::SwitchOpBuilder::new(self.context)
                    .predicate(predicate)
                    .cases(cases)
                    .arms(regions)
                    .result_types(types);
                for _ in 0..deps {
                    gate = gate.dep_result();
                }
                gate.build().id()
            }
        };
        self.context.get_block(block).append(op);
        self.bind_results(op, &ports, env);
        Ok(())
    }

    /// One arm of a conditional: its own region, yielding a value for every
    /// port, taken from the arm where it assigned one and from the enclosing
    /// scope where it did not.
    fn arm(&self, arm: &[Stmt], ports: &[VarId], env: &Env) -> Result<RegionId, PassError> {
        let region = self.context.create_region();
        let block = self.context.create_block(vec![]);
        region.add_block(block.id());
        let mut inner = env.clone();
        self.statements(arm, block.id(), &mut inner)?;
        let yielded = self.port_values(ports, block.id(), &inner)?;
        block.append(self.yield_ports(yielded, self.ports.dep_count(ports)).id());
        Ok(region.id())
    }

    fn loop_op(
        &self,
        id: super::cfg::LoopId,
        body: &[Stmt],
        block: BlockId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        let tail = self.cfg.loops[id].tail;
        let Term::LoopTail { pred, .. } = self.cfg.nodes[tail].term.clone() else {
            return Err(unsupported("a loop whose tail moved"));
        };
        let ports = self.ports.loop_ports(id, body);
        let deps = self.ports.dep_count(&ports);

        let condition_region = self.context.create_region();
        let condition_block = self.port_block(&ports);
        condition_region.add_block(condition_block.id());
        let mut inner = env.clone();
        for (port, argument) in ports.iter().zip(condition_block.arguments()) {
            inner.insert(*port, argument.id());
        }
        self.statements(body, condition_block.id(), &mut inner)?;
        let repeat = self.read_src(condition_block.id(), &inner, pred)?;
        let carried = self.port_values(&ports, condition_block.id(), &inner)?;
        let (values, dep_values) = carried.split_at(carried.len() - deps);
        let mut condition = scf::ConditionOpBuilder::new(self.context)
            .condition(repeat)
            .values(values.to_vec());
        for &dep in dep_values {
            condition = condition.dep_operand(dep);
        }
        condition_block.append(condition.build().id());

        // The body region only hands the carried values back: the iteration
        // itself lives in the condition region, which is what makes the loop
        // tail-controlled.
        let body_region = self.context.create_region();
        let body_block = self.port_block(&ports);
        body_region.add_block(body_block.id());
        let latched = body_block.arguments().iter().map(Value::id).collect();
        body_block.append(self.yield_ports(latched, deps).id());

        let inits = self.port_values(&ports, block, env)?;
        let (values, dep_values) = inits.split_at(inits.len() - deps);
        let mut loop_op = scf::WhileOpBuilder::new(self.context)
            .condition_region(condition_region.id())
            .body(body_region.id())
            .inits(values.to_vec())
            .result_types(self.ports.value_types(&ports));
        for &dep in dep_values {
            loop_op = loop_op.dep_operand(dep).dep_result();
        }
        let op = loop_op.build().id();
        self.context.get_block(block).append(op);
        self.bind_results(op, &ports, env);
        Ok(())
    }

    fn bind_results(&self, op: OpId, ports: &[VarId], env: &mut Env) {
        let results = self.context.get_op(op).results().to_vec();
        for (&port, result) in ports.iter().zip(results) {
            env.insert(port, result);
        }
    }

    /// A block entered on `ports`: one argument per value, then one dependency
    /// per chain.
    fn port_block(&self, ports: &[VarId]) -> crate::BlockHandle {
        let deps = self.ports.dep_count(ports);
        let arguments = self
            .ports
            .value_types(ports)
            .into_iter()
            .chain(std::iter::repeat_n(TypeId::DEPENDENCY, deps))
            .map(|ty| self.context.create_value(ty, None))
            .collect();
        self.context.create_block_with_dependencies(arguments, deps)
    }

    /// One value per port, read from `env` or invented where nothing reaching
    /// this point ever assigned the variable.
    /// A yield of `values`, the trailing `deps` of which are dependencies.
    fn yield_ports(&self, values: Vec<ValueId>, deps: usize) -> scf::YieldOp {
        let (values, dep_values) = values.split_at(values.len() - deps);
        let mut yield_op = scf::YieldOpBuilder::new(self.context).values(values.to_vec());
        for &dep in dep_values {
            yield_op = yield_op.dep_operand(dep);
        }
        yield_op.build()
    }

    fn port_values(
        &self,
        ports: &[VarId],
        block: BlockId,
        env: &Env,
    ) -> Result<Vec<ValueId>, PassError> {
        ports
            .iter()
            .map(|&port| match env.get(&port) {
                Some(&value) => Ok(value),
                None => self.constant(block, 0, self.cfg.var_types[port]),
            })
            .collect()
    }

    /// A variable read where nothing assigned it belongs to code no path
    /// reaches — the value it named is gone with the blocks that held it, so
    /// the read is bound to an invented one.
    fn bind_undefined_reads(
        &self,
        op: OpId,
        block: BlockId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        for value in super::cfg::deep_operands(self.context, op) {
            let Some(&var) = self.cfg.value_var.get(&value) else {
                continue;
            };
            if env.contains_key(&var) {
                continue;
            }
            let invented = self.constant(block, 0, self.cfg.var_types[var])?;
            env.insert(var, invented);
        }
        Ok(())
    }

    fn read(&self, block: BlockId, env: &Env, var: VarId) -> Result<ValueId, PassError> {
        match env.get(&var) {
            Some(&value) => Ok(value),
            None => self.constant(block, 0, self.cfg.var_types[var]),
        }
    }

    fn read_src(&self, block: BlockId, env: &Env, src: Src) -> Result<ValueId, PassError> {
        match src {
            Src::Value(value) => Ok(self.resolve(env, value)),
            Src::Var(var) => self.read(block, env, var),
        }
    }

    /// What `value` is called where it is being read: the variable's current
    /// value where the structure routed it through a port, the value itself
    /// where it is still in scope.
    fn resolve(&self, env: &Env, value: ValueId) -> ValueId {
        match self.cfg.value_var.get(&value) {
            Some(var) => env.get(var).copied().unwrap_or(value),
            None => value,
        }
    }

    fn retarget_operands(&self, op: OpId, env: &Env) {
        let mut pending = vec![op];
        while let Some(op) = pending.pop() {
            let instance = self.context.get_op(op);
            for (index, &operand) in instance.operands().iter().enumerate() {
                let resolved = self.resolve(env, operand);
                if resolved != operand {
                    self.context.set_op_operand(op, index, resolved);
                }
            }
            for region in instance.regions() {
                for block in self.context.get_region(region).block_ids() {
                    pending.extend(self.context.get_block(block).op_ids());
                }
            }
        }
    }

    fn constant(&self, block: BlockId, value: i64, ty: TypeId) -> Result<ValueId, PassError> {
        if !is_integer(self.context, ty) {
            return Err(unsupported(&format!(
                "a value of type {} it would have to invent",
                self.context.type_to_string(ty)
            )));
        }
        let op = b::ConstantOpBuilder::new(self.context)
            .value(value)
            .result_type(ty)
            .build();
        let id = op.id();
        self.context.get_block(block).append(id);
        Ok(self.context.get_op(id).results()[0])
    }
}

/// Whether `ty` is an integer, the only type this pass knows how to invent a
/// value of.
pub(super) fn is_integer(context: &Context, ty: TypeId) -> bool {
    let data = context.get_type_data(ty);
    (data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<crate::builtin::IntegerType>()
        .is_some()
}

enum Decision {
    If(Src),
    Switch(VarId, Vec<i64>),
}
