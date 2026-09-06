//! Placement-free edits of unordered regions: put an op where its operands
//! say, grow a declared port, wrap operations in a structured op, and splice
//! one whose predicate is known back out.

use crate::builtin::{ConstantOpBuilder, IntegerType};
use crate::region::{defining_region, topological_order};
use crate::scf::{LoopOpBuilder, Switch2OpBuilder};
use crate::{
    ConstantLike, Context, Error, ExitTarget, Gamma, NonLocalExit, OpId, Operation, RegionId,
    Theta, TypeId, ValueId,
};

/// Which structured op [`Context::wrap`] builds around a set of operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wrap {
    Theta,
    Gamma,
}

impl Context {
    /// Put `op` into the deepest region every operand is visible from: a
    /// dependency operand pins it to that operand's region, and otherwise the
    /// innermost of the operands' regions; every other operand region must
    /// enclose or be the one chosen. Answers the region chosen. An op with no
    /// operand names no region to join, and operands of sibling regions are
    /// visible from none.
    pub fn add_auto(&self, op: OpId) -> RegionId {
        let handle = self.get_op(op);
        let regions: Vec<RegionId> = handle
            .operands()
            .iter()
            .filter_map(|&value| defining_region(self, value))
            .collect();
        let chosen = match handle.dep_operands().first() {
            Some(&dep) => defining_region(self, dep).expect("a dependency is defined in a region"),
            None => regions
                .iter()
                .copied()
                .max_by_key(|&region| self.region_ancestors(region).len())
                .expect("an operation with no operand names no region to join"),
        };
        let chain = self.region_ancestors(chosen);
        assert!(
            regions.iter().all(|region| chain.contains(region)),
            "operands of sibling regions are visible from no region"
        );
        self.add(chosen, op);
        chosen
    }

    /// `region` and every region enclosing it, innermost first.
    fn region_ancestors(&self, region: RegionId) -> Vec<RegionId> {
        let mut chain = vec![region];
        let mut current = region;
        while let Some(parent) = self
            .get_region(current)
            .parent_op()
            .and_then(|op| self.region_of_op(op))
        {
            chain.push(parent);
            current = parent;
        }
        chain
    }

    /// Whether `op` is one of `roots` or sits under one, at any depth.
    fn op_under(&self, op: OpId, roots: &[OpId]) -> bool {
        let mut current = Some(op);
        while let Some(op) = current {
            if roots.contains(&op) {
                return true;
            }
            current = self.parent_op(op);
        }
        false
    }

    /// Every op under `roots`, the roots included.
    fn subtree_ops(&self, roots: &[OpId]) -> Vec<OpId> {
        let mut found = roots.to_vec();
        let mut index = 0;
        while index < found.len() {
            for region in self.get_op(found[index]).regions() {
                found.extend(self.get_region(region).op_ids());
            }
            index += 1;
        }
        found
    }

    /// `region` and every region nested in it.
    pub(crate) fn nested_regions(&self, region: RegionId) -> Vec<RegionId> {
        let ops = self.get_region(region).op_ids();
        let mut regions = vec![region];
        regions.extend(
            self.subtree_ops(&ops)
                .iter()
                .flat_map(|&op| self.get_op(op).regions()),
        );
        regions
    }

    /// Region results sit in no use list; rename `old` to `new` in the result
    /// list of `region` and every region nested in it, except regions under
    /// `except`, which keep naming the value they define.
    fn rename_region_results(&self, region: RegionId, old: ValueId, new: ValueId, except: &[OpId]) {
        for nested in self.nested_regions(region) {
            let handle = self.get_region(nested);
            if handle
                .parent_op()
                .is_some_and(|owner| self.op_under(owner, except))
            {
                continue;
            }
            let mut results = handle.results();
            if results.contains(&old) {
                let deps = handle.dep_results().len();
                for result in &mut results {
                    if *result == old {
                        *result = new;
                    }
                }
                self.set_region_results(nested, results, deps);
            }
        }
    }

    /// The non-local exits under `roots` that leave `target`.
    fn exits_leaving(&self, roots: &[OpId], target: OpId) -> Vec<OpId> {
        self.subtree_ops(roots)
            .into_iter()
            .filter(|&op| {
                self.get_op(op).has_interface::<dyn NonLocalExit>()
                    && crate::analysis::exits::resolve_exit_target(self, op).ok() == Some(target)
            })
            .collect()
    }

    /// [`Context::grow_port`] for an op with a declared binding: one more
    /// carried value, or dependency when `dependency`, at the end of every
    /// aligned range. A loop's exit value defaults to the port; a gamma's arms
    /// must each name what they produce for it. Every non-local exit that
    /// leaves `op` gains the port of the region it sits in.
    pub(crate) fn grow_declared_port(
        &self,
        op: OpId,
        ty: TypeId,
        init: Option<ValueId>,
        mut latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
        dependency: bool,
    ) -> ValueId {
        let handle = self.get_op(op);
        let deps_in = handle.dep_operands().len();
        let port_of = |region: RegionId, index: usize| {
            let port = self.create_value(ty, None);
            self.insert_region_port(region, index, port.clone(), dependency);
            port.id()
        };
        let feed = |region: RegionId, port: ValueId| {
            for exit in self.exits_leaving(&self.get_region(region).op_ids(), op) {
                if dependency {
                    self.append_dep_operand(exit, port);
                } else {
                    self.append_operand(exit, port);
                }
            }
        };
        if let Some(theta) = handle.clone().as_interface::<dyn Theta>() {
            let body = theta.body();
            let binding = theta.carried();
            let init = init.expect("a loop port carries a value in");
            let (operands, ports, continue_, exit) = if dependency {
                (deps_in, deps_in, deps_in, 2 * deps_in)
            } else {
                (
                    binding.operands.end,
                    binding.ports.end,
                    binding.continue_.end,
                    binding.exit.end,
                )
            };
            self.insert_operand_at(op, operands, init, dependency);
            let port = port_of(body, ports);
            let carried = latch(body, Some(port)).unwrap_or(port);
            self.insert_region_result(body, exit, port, dependency);
            self.insert_region_result(body, continue_, carried, dependency);
            feed(body, port);
        } else if let Some(gamma) = handle.clone().as_interface::<dyn Gamma>() {
            let arms = gamma.arms();
            let binding = gamma.forwarded();
            let (operands, ports, joined) = if dependency {
                (deps_in, deps_in, handle.dep_results().len())
            } else {
                (binding.operands.end, binding.ports.end, binding.exit.end)
            };
            if let Some(init) = init {
                self.insert_operand_at(op, operands, init, dependency);
            }
            for &arm in &arms {
                let port = init.map(|_| port_of(arm, ports));
                let produced = latch(arm, port).expect("every arm produces the port's value");
                self.insert_region_result(arm, joined, produced, dependency);
                if let Some(port) = port {
                    feed(arm, port);
                }
            }
        } else {
            panic!("grow_declared_port needs a Theta or Gamma");
        }
        if dependency {
            self.append_dep_result(op)
        } else {
            self.append_result(op, ty)
        }
    }

    /// Move `ops`, held by the unordered `region`, into a new structured op
    /// placed in `region`. A gamma with one arm and a constant predicate takes
    /// any value the ops produce for the outside as a joined result. A loop
    /// with a constant-false predicate carries nothing, so it refuses ops whose
    /// values the outside reads: grow ports for those after wrapping. Either
    /// refuses ops holding an exit the new op would capture.
    pub fn wrap(&self, region: RegionId, ops: &[OpId], kind: Wrap) -> Result<OpId, Error> {
        let captured = match kind {
            Wrap::Theta => ExitTarget::InnermostLoop,
            Wrap::Gamma => ExitTarget::InnermostSwitch,
        };
        if let Some(exit) = self.subtree_ops(ops).into_iter().find(|&op| {
            self.get_op(op)
                .as_interface::<dyn NonLocalExit>()
                .is_some_and(|exit| exit.target() == captured)
        }) {
            return Err(Error::VerificationError(format!(
                "{}.{} would leave the wrapping op instead of its target",
                self.get_op(exit).dialect(),
                self.get_op(exit).name()
            )));
        }
        let escaping = self.escaping_values(region, ops);
        if kind == Wrap::Theta && !escaping.is_empty() {
            return Err(Error::VerificationError(format!(
                "%{} would leave the wrapped loop, which carries nothing",
                escaping[0].number()
            )));
        }
        for &op in ops {
            self.remove_from_region(region, op);
        }
        match kind {
            Wrap::Gamma => {
                let deps = escaping
                    .iter()
                    .filter(|&&value| self.get_value(value).is_dependency())
                    .count();
                let arm = self.create_nodes_region(vec![], 0, ops.to_vec(), escaping.clone(), deps);
                let predicate = ConstantOpBuilder::new(self)
                    .value(0)
                    .result_type(IntegerType::new(self, 32))
                    .build();
                self.add(region, predicate.id());
                let mut builder = Switch2OpBuilder::new(self)
                    .predicate(predicate.result())
                    .inputs(vec![])
                    .arms(vec![arm.id()])
                    .result_types(
                        escaping[..escaping.len() - deps]
                            .iter()
                            .map(|&value| self.get_value(value).ty())
                            .collect(),
                    );
                for _ in 0..deps {
                    builder = builder.dep_result();
                }
                let switch = builder.build();
                self.add(region, switch.id());
                for (&old, &new) in escaping.iter().zip(switch.handle().results().iter()) {
                    for user in self.uses_of(old) {
                        if !self.op_under(user.op, &[switch.id()]) {
                            self.set_op_operand(user.op, user.index, new);
                        }
                    }
                    self.rename_region_results(region, old, new, &[switch.id()]);
                }
                Ok(switch.id())
            }
            Wrap::Theta => {
                let body = self.create_nodes_region(vec![], 0, ops.to_vec(), vec![], 0);
                let predicate = ConstantOpBuilder::new(self)
                    .value(0)
                    .result_type(IntegerType::new(self, 1))
                    .build();
                self.add(body.id(), predicate.id());
                self.set_region_results(body.id(), vec![predicate.result()], 0);
                let loop_op = LoopOpBuilder::new(self)
                    .inits(vec![])
                    .body(body.id())
                    .result_types(vec![])
                    .build();
                self.add(region, loop_op.id());
                Ok(loop_op.id())
            }
        }
    }

    /// The values `ops` define that something outside them reads: an operand
    /// of an op not under them, or a result of a region not under them.
    /// Values first, dependencies trailing, in definition order.
    fn escaping_values(&self, region: RegionId, ops: &[OpId]) -> Vec<ValueId> {
        let naming_regions: Vec<RegionId> = self
            .nested_regions(region)
            .into_iter()
            .filter(|&nested| {
                nested == region
                    || !self
                        .get_region(nested)
                        .parent_op()
                        .is_some_and(|owner| self.op_under(owner, ops))
            })
            .collect();
        let mut escaping: Vec<ValueId> = ops
            .iter()
            .flat_map(|&op| self.get_op(op).results())
            .filter(|&value| {
                naming_regions
                    .iter()
                    .any(|&nested| self.get_region(nested).results().contains(&value))
                    || self
                        .uses_of(value)
                        .iter()
                        .any(|user| !self.op_under(user.op, ops))
            })
            .collect();
        escaping.sort_by_key(|&value| self.get_value(value).is_dependency());
        escaping
    }

    /// The integer a constant-producing op gives `value`, if one defines it.
    fn constant_of(&self, value: ValueId) -> Option<i64> {
        let definition = self.get_value(value).defining_op()?;
        self.get_op(definition)
            .as_interface::<dyn ConstantLike>()
            .map(|constant| constant.constant_value().to_i64())
    }

    /// The ports of `region` paired with the operands of `op` they read:
    /// values through the binding's ranges, dependencies whole.
    fn port_inits(
        &self,
        op: &crate::OpHandle,
        region: RegionId,
        ports: std::ops::Range<usize>,
        operands: std::ops::Range<usize>,
    ) -> Vec<(ValueId, ValueId)> {
        let region = self.get_region(region);
        region.value_arguments()[ports]
            .iter()
            .map(crate::Value::id)
            .zip(op.value_operands()[operands].iter().copied())
            .chain(
                region
                    .dep_arguments()
                    .iter()
                    .map(crate::Value::id)
                    .zip(op.dep_operands().iter().copied()),
            )
            .collect()
    }

    /// Splice a structured op whose choice is known back into its region: a
    /// gamma with a constant predicate gives way to the arm it selects, and a
    /// loop with a constant-false predicate to its body, which runs once. The
    /// ports read their inits, the results read the arm's or exit values, and
    /// the op goes away. An exit that leaves the op has nowhere left to go, so
    /// an op holding one is refused.
    pub fn unwrap(&self, op: OpId) -> Result<(), Error> {
        let handle = self.get_op(op);
        let spelled = format!("{}.{}", handle.dialect(), handle.name());
        let (chosen, ports, results) =
            if let Some(gamma) = handle.clone().as_interface::<dyn Gamma>() {
                let arms = gamma.arms();
                let index = self.constant_of(gamma.predicate()).ok_or_else(|| {
                    Error::VerificationError(format!("{spelled} predicate is not a constant"))
                })?;
                let arm = arms[usize::try_from(index).unwrap_or(0).min(arms.len() - 1)];
                let binding = gamma.forwarded();
                let ports = self.port_inits(&handle, arm, binding.ports, binding.operands);
                let results = handle
                    .results()
                    .iter()
                    .copied()
                    .zip(self.get_region(arm).results())
                    .collect::<Vec<_>>();
                (arm, ports, results)
            } else if let Some(theta) = handle.clone().as_interface::<dyn Theta>() {
                if self.constant_of(theta.predicate()) != Some(0) {
                    return Err(Error::VerificationError(format!(
                        "{spelled} predicate is not constant false"
                    )));
                }
                let body = theta.body();
                let binding = theta.carried();
                let region = self.get_region(body);
                let dep_results = region.dep_results();
                let ports = self.port_inits(&handle, body, binding.ports, binding.operands);
                let results = handle
                    .value_results()
                    .iter()
                    .copied()
                    .zip(region.value_results()[binding.exit].iter().copied())
                    .chain(
                        handle
                            .dep_results()
                            .iter()
                            .copied()
                            .zip(dep_results[dep_results.len() / 2..].iter().copied()),
                    )
                    .collect::<Vec<_>>();
                (body, ports, results)
            } else {
                return Err(Error::VerificationError(format!(
                    "{spelled} declares no binding to unwrap"
                )));
            };
        if !self.exits_leaving(&[op], op).is_empty() {
            return Err(Error::VerificationError(format!(
                "{spelled} is left by a non-local exit and cannot be spliced away"
            )));
        }
        let parent = (self.parent_nodes_region(op), self.parent_block(op));
        let root = match parent {
            (Some(region), _) => region,
            (None, Some(block)) => self.parent_region(block).expect("a block sits in a region"),
            (None, None) => {
                return Err(Error::VerificationError(format!(
                    "{spelled} sits in no region to splice into"
                )));
            }
        };

        let order = topological_order(self, chosen)?;
        for &moved in &order {
            self.remove_from_region(chosen, moved);
        }
        match parent {
            (Some(region), _) => {
                for &moved in &order {
                    self.add(region, moved);
                }
            }
            (_, Some(block)) => {
                let block = self.get_block(block);
                let at = block
                    .op_ids()
                    .iter()
                    .position(|&held| held == op)
                    .expect("an op sits in its block");
                for (offset, &moved) in order.iter().enumerate() {
                    block.insert(at + offset, moved);
                }
            }
            (None, None) => unreachable!("checked above"),
        }
        for &(port, init) in &ports {
            self.replace_value_uses(port, init);
            self.rename_region_results(root, port, init, &[]);
        }
        for &(result, produced) in &results {
            let produced = ports
                .iter()
                .find(|(port, _)| *port == produced)
                .map_or(produced, |(_, init)| *init);
            self.replace_value_uses(result, produced);
            self.rename_region_results(root, result, produced, &[]);
        }
        match parent {
            (Some(region), _) => self.remove_from_region(region, op),
            (_, Some(block)) => {
                self.get_block(block).remove_op(op);
            }
            (None, None) => unreachable!("checked above"),
        }
        self.remove_operation(op);
        Ok(())
    }
}
