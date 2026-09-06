//! Emission into unordered regions: the statement tree becomes `scf.switch2`,
//! `scf.loop` and `scf.for2` nodes.
//!
//! Every original operation is *moved*, never copied. A variable read after
//! the structure that assigns it is a joined result of a `scf.switch2` or a
//! carried port of an `scf.loop`; a region reads everything else from the
//! scope enclosing it, so a gamma forwards nothing. The exit becomes the
//! region's results rather than an operation, since an unordered region holds
//! no terminator.

use std::collections::BTreeMap;

use super::branches::Stmt;
use super::cfg::{Cfg, LoopId, NodeId, Rhs, Src, Term, VarId, unsupported};
use super::liveness::Liveness;
use super::ports::Ports;
use crate::attributes::Predicate;
use crate::builtin::{AddIOpBuilder, CmpIOpBuilder, ConstantOpBuilder, IntegerType};
use crate::state::EntryStateOpBuilder;
use crate::{
    Context, CountedLoop, OpId, Operation, PassError, RegionId, TypeId, Value, ValueId, scf,
};

type Env = BTreeMap<VarId, ValueId>;

pub fn emit(
    context: &Context,
    region: RegionId,
    cfg: &Cfg,
    tree: &[Stmt],
    live: &Liveness,
) -> Result<(), PassError> {
    let entry = context.get_region(region).entry_block();
    let parameters = context.get_block(entry).value_arguments();
    let ports: Vec<Value> = parameters
        .iter()
        .map(|parameter| context.create_value(parameter.ty(), None))
        .collect();
    let body = context
        .create_nodes_region(ports.clone(), 0, vec![], vec![], 0)
        .id();

    let mut env = Env::new();
    for (parameter, port) in parameters.iter().zip(&ports) {
        env.insert(cfg.value_var[&parameter.id()], port.id());
    }
    if let Some(chain) = cfg.chain {
        let root = EntryStateOpBuilder::new(context).dep_result().build();
        context.add(body, root.id());
        env.insert(chain, root.result());
    }

    let emitter = Emitter {
        context,
        cfg,
        live,
        ports: Ports { context, cfg, live },
    };
    emitter.statements(tree, body, &mut env)?;
    context.replace_region_with_nodes(region, body);
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
        region: RegionId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        for statement in statements {
            self.statement(statement, region, env)?;
        }
        Ok(())
    }

    fn statement(
        &self,
        statement: &Stmt,
        region: RegionId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        match statement {
            Stmt::Node(node) => self.node(*node, region, env),
            Stmt::Assign(assigns) => self.assign(assigns, region, env),
            Stmt::Exit { op, args } => self.exit(*op, args.as_deref(), region, env),
            Stmt::If {
                pred,
                then_arm,
                else_arm,
                continuation,
            } => self.conditional(
                Decision::If(*pred),
                &[else_arm, then_arm],
                *continuation,
                region,
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
                    region,
                    env,
                )
            }
            Stmt::Loop { id, body } => self.loop_op(*id, body, region, env),
        }
    }

    /// Move a node's operations into `region`, retargeting the operands whose
    /// definitions the structure has since closed away.
    fn node(&self, node: NodeId, region: RegionId, env: &mut Env) -> Result<(), PassError> {
        let assigns = self.cfg.nodes[node].assigns.clone();
        self.assign(&assigns, region, env)?;
        for op in self.cfg.ops(node) {
            self.bind_undefined_reads(op, region, env)?;
            self.retarget_operands(op, env);
            let results = self.context.get_op(op).results().to_vec();
            let placed = if self.context.get_op(op).is::<scf::ForOp>() {
                self.counted_loop(op)?
            } else {
                let block = self.context.parent_block(op).expect("an op of a block");
                self.context.get_block(block).remove_op(op);
                op
            };
            self.context.add(region, placed);
            let placed = self.context.get_op(placed).results().to_vec();
            for (result, now) in results.into_iter().zip(placed) {
                if let Some(&var) = self.cfg.value_var.get(&result) {
                    env.insert(var, now);
                }
            }
        }
        Ok(())
    }

    /// The exit leaves nothing behind but the values it carried: they are the
    /// region's results, with the memory it hands back trailing.
    fn exit(
        &self,
        op: OpId,
        args: Option<&[VarId]>,
        region: RegionId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        let (results, deps) = match args {
            Some(args) => {
                let args = self.ports.deps_last(args);
                (
                    self.port_values(&args, region, env)?,
                    self.ports.dep_count(&args),
                )
            }
            None => {
                self.bind_undefined_reads(op, region, env)?;
                let handle = self.context.get_op(op);
                let results = handle
                    .operands()
                    .iter()
                    .map(|&operand| self.resolve(env, operand))
                    .collect();
                (results, handle.dep_operands().len())
            }
        };
        self.context.set_region_results(region, results, deps);
        Ok(())
    }

    /// Assignments happen at once: every right-hand side is read before any
    /// variable takes its new value.
    fn assign(
        &self,
        assigns: &[(VarId, Rhs)],
        region: RegionId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        let mut written = Vec::with_capacity(assigns.len());
        for &(var, rhs) in assigns {
            let value = match rhs {
                Rhs::Value(value) => self.resolve(env, value),
                Rhs::Const(constant) => self.constant(region, constant, self.cfg.var_types[var])?,
            };
            written.push((var, value));
        }
        env.extend(written);
        Ok(())
    }

    /// A γ: the predicate indexes the arms, so a conditional's arms go false
    /// first and a dispatch's cases are its arm indices. The arms read values
    /// from the enclosing scope, but a chain they consume enters each arm as a
    /// dependency port of its own: two arms changing one state would read as a
    /// fork the order forbids, when they are alternatives.
    fn conditional(
        &self,
        decision: Decision,
        arms: &[&[Stmt]],
        continuation: NodeId,
        region: RegionId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        let ports = self.ports.ports(arms, self.live.at(continuation).clone());
        let deps = self.ports.dep_count(&ports);
        let chains = &ports[ports.len() - deps..];
        let dep_inits = self.port_values(chains, region, env)?;
        let regions = arms
            .iter()
            .map(|arm| self.arm(arm, &ports, chains, env))
            .collect::<Result<Vec<_>, _>>()?;
        let predicate = match decision {
            Decision::If(pred) => self.read_src(region, env, pred)?,
            Decision::Switch(var, cases) => {
                if cases
                    .iter()
                    .enumerate()
                    .any(|(index, &case)| case != index as i64)
                {
                    return Err(unsupported(
                        "a dispatch whose cases are not its arm indices",
                    ));
                }
                self.read(region, env, var)?
            }
        };
        let mut gate = scf::Switch2OpBuilder::new(self.context)
            .predicate(predicate)
            .inputs(vec![])
            .arms(regions)
            .result_types(self.ports.value_types(&ports));
        for dep in dep_inits {
            gate = gate.dep_operand(dep).dep_result();
        }
        let op = gate.build().id();
        self.context.add(region, op);
        self.bind_results(op, &ports, env);
        Ok(())
    }

    /// One arm of a gamma: its own region, entered on a port per chain and
    /// producing a value for every port, taken from the arm where it assigned
    /// one and from the enclosing scope where it did not.
    fn arm(
        &self,
        arm: &[Stmt],
        ports: &[VarId],
        chains: &[VarId],
        env: &Env,
    ) -> Result<RegionId, PassError> {
        let dep_ports: Vec<Value> = chains
            .iter()
            .map(|_| self.context.create_value(TypeId::DEPENDENCY, None))
            .collect();
        let region = self
            .context
            .create_nodes_region(dep_ports.clone(), chains.len(), vec![], vec![], 0)
            .id();
        let mut inner = env.clone();
        for (&chain, port) in chains.iter().zip(&dep_ports) {
            inner.insert(chain, port.id());
        }
        self.statements(arm, region, &mut inner)?;
        let produced = self.port_values(ports, region, &inner)?;
        self.context
            .set_region_results(region, produced, self.ports.dep_count(ports));
        Ok(region)
    }

    /// A θ: the body runs once per iteration and names the repeat predicate,
    /// then the values the next iteration carries, then the values the loop
    /// leaves with. Restructuring made every loop tail-controlled, so both
    /// groups are the variables as the tail finds them.
    fn loop_op(
        &self,
        id: LoopId,
        body: &[Stmt],
        region: RegionId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        let tail = self.cfg.loops[id].tail;
        let Term::LoopTail { pred, .. } = self.cfg.nodes[tail].term.clone() else {
            return Err(unsupported("a loop whose tail moved"));
        };
        let ports = self.ports.loop_ports(id, body);
        let deps = self.ports.dep_count(&ports);
        let port_values: Vec<Value> = ports
            .iter()
            .map(|&var| self.context.create_value(self.cfg.var_types[var], None))
            .collect();
        let body_region = self
            .context
            .create_nodes_region(port_values.clone(), deps, vec![], vec![], 0)
            .id();
        let mut inner = env.clone();
        for (&var, port) in ports.iter().zip(&port_values) {
            inner.insert(var, port.id());
        }
        self.statements(body, body_region, &mut inner)?;
        let repeat = self.read_src(body_region, &inner, pred)?;
        let carried = self.port_values(&ports, body_region, &inner)?;
        let (values, carried_deps) = carried.split_at(carried.len() - deps);
        let mut results = vec![repeat];
        results.extend_from_slice(values);
        results.extend_from_slice(values);
        results.extend_from_slice(carried_deps);
        results.extend_from_slice(carried_deps);
        self.context
            .set_region_results(body_region, results, 2 * deps);

        let inits = self.port_values(&ports, region, env)?;
        let (values, dep_inits) = inits.split_at(inits.len() - deps);
        let mut loop_op = scf::LoopOpBuilder::new(self.context)
            .inits(values.to_vec())
            .body(body_region)
            .result_types(self.ports.value_types(&ports));
        for &dep in dep_inits {
            loop_op = loop_op.dep_operand(dep).dep_result();
        }
        let op = loop_op.build().id();
        self.context.add(region, op);
        self.bind_results(op, &ports, env);
        Ok(())
    }

    /// An `scf.for` the frontend raised becomes `scf.for2`: the counter is
    /// port 0, the carried arguments follow, and the body's yield says what
    /// the next iteration carries. The old operation stays in its block, to
    /// go with it.
    fn counted_loop(&self, op: OpId) -> Result<OpId, PassError> {
        let context = self.context;
        let for_op = scf::ForOp::from_op_instance(context.get_op(op));
        let [block] = context.get_region(for_op.handle().regions()[0]).block_ids()[..] else {
            return Err(unsupported("a counted loop whose body is a graph"));
        };
        let block = context.get_block(block);
        let arguments = block.arguments();
        let deps = block.dep_arguments().len();
        let ops = block.op_ids();
        let (&latch, body_ops) = ops
            .split_last()
            .ok_or_else(|| unsupported("a loop body with no terminator"))?;

        let counter_type = context.get_value(for_op.lower_bound()).ty();
        let mut ports = vec![context.create_value(counter_type, None)];
        ports.extend(
            arguments
                .iter()
                .map(|argument| context.create_value(argument.ty(), None)),
        );
        let body = context
            .create_nodes_region(ports.clone(), deps, vec![], vec![], 0)
            .id();
        for (argument, port) in arguments.iter().zip(&ports[1..]) {
            context.replace_value_uses(argument.id(), port.id());
        }
        for &inner in body_ops {
            block.remove_op(inner);
            let placed = if context.get_op(inner).is::<scf::ForOp>() {
                block.append(inner);
                self.counted_loop(inner)?
            } else {
                inner
            };
            context.add(body, placed);
        }
        let counter = ports[0].id();
        let compare = CmpIOpBuilder::new(context)
            .lhs(counter)
            .rhs(for_op.upper_bound())
            .predicate(Predicate::Slt)
            .result_type(IntegerType::new(context, 1))
            .build();
        context.add(body, compare.id());
        let advance = AddIOpBuilder::new(context)
            .lhs(counter)
            .rhs(for_op.step())
            .result_type(counter_type)
            .build();
        context.add(body, advance.id());

        let yielded = context.get_op(latch);
        let mut results = vec![compare.result(), advance.result()];
        results.extend(yielded.value_operands().iter().copied());
        results.extend(ports[..ports.len() - deps].iter().map(Value::id));
        results.extend(yielded.dep_operands().iter().copied());
        results.extend(ports[ports.len() - deps..].iter().map(Value::id));
        context.set_region_results(body, results, 2 * deps);

        let old = for_op.handle();
        let mut result_types = vec![counter_type];
        result_types.extend(
            old.value_results()
                .iter()
                .map(|&result| context.get_value(result).ty()),
        );
        let mut builder = scf::For2OpBuilder::new(context)
            .lb(for_op.lower_bound())
            .inits(old.value_operands()[3..].to_vec())
            .ub(for_op.upper_bound())
            .step(for_op.step())
            .body(body)
            .result_types(result_types);
        for dep in old.dep_operands() {
            builder = builder.dep_operand(dep).dep_result();
        }
        let raised = builder.build();
        let handle = raised.handle();
        for (&was, &now) in old.value_results().iter().zip(&handle.value_results()[1..]) {
            context.replace_value_uses(was, now);
        }
        for (&was, &now) in old.dep_results().iter().zip(handle.dep_results().iter()) {
            context.replace_value_uses(was, now);
        }
        Ok(raised.id())
    }

    fn bind_results(&self, op: OpId, ports: &[VarId], env: &mut Env) {
        let results = self.context.get_op(op).results().to_vec();
        for (&port, result) in ports.iter().zip(results) {
            env.insert(port, result);
        }
    }

    /// One value per port, read from `env` or invented where nothing reaching
    /// this point ever assigned the variable.
    fn port_values(
        &self,
        ports: &[VarId],
        region: RegionId,
        env: &Env,
    ) -> Result<Vec<ValueId>, PassError> {
        ports
            .iter()
            .map(|&port| self.read(region, env, port))
            .collect()
    }

    /// A variable read where nothing assigned it belongs to code no path
    /// reaches: the value it named is gone with the blocks that held it, so
    /// the read is bound to an invented one.
    fn bind_undefined_reads(
        &self,
        op: OpId,
        region: RegionId,
        env: &mut Env,
    ) -> Result<(), PassError> {
        for value in super::cfg::deep_operands(self.context, op) {
            let Some(&var) = self.cfg.value_var.get(&value) else {
                continue;
            };
            if env.contains_key(&var) {
                continue;
            }
            let invented = self.constant(region, 0, self.cfg.var_types[var])?;
            env.insert(var, invented);
        }
        Ok(())
    }

    fn read(&self, region: RegionId, env: &Env, var: VarId) -> Result<ValueId, PassError> {
        match env.get(&var) {
            Some(&value) => Ok(value),
            None => self.constant(region, 0, self.cfg.var_types[var]),
        }
    }

    fn read_src(&self, region: RegionId, env: &Env, src: Src) -> Result<ValueId, PassError> {
        match src {
            Src::Value(value) => Ok(self.resolve(env, value)),
            Src::Var(var) => self.read(region, env, var),
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
                pending.extend(self.context.get_region(region).op_ids());
            }
        }
    }

    fn constant(&self, region: RegionId, value: i64, ty: TypeId) -> Result<ValueId, PassError> {
        if !super::emit::is_integer(self.context, ty) {
            return Err(unsupported(&format!(
                "a value of type {} it would have to invent",
                self.context.type_to_string(ty)
            )));
        }
        let op = ConstantOpBuilder::new(self.context)
            .value(value)
            .result_type(ty)
            .build();
        self.context.add(region, op.id());
        Ok(op.result())
    }
}

enum Decision {
    If(Src),
    Switch(VarId, Vec<i64>),
}
