//! Placement-free edits of unordered regions: put an op where its operands
//! say, grow a declared port, and drop one a loop or a gate no longer carries.

use crate::region::defining_region;
use crate::{Context, Gamma, NonLocalExit, OpId, RegionId, Theta, TypeId, ValueId};

impl Context {
    /// Put `op` into the deepest region every operand is visible from: a
    /// state operand pins it to that operand's region, and otherwise the
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
        let chosen = match handle.state_operands().first() {
            Some(&state) => defining_region(self, state).expect("a state is defined in a region"),
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
    pub(crate) fn rename_region_results(
        &self,
        region: RegionId,
        old: ValueId,
        new: ValueId,
        except: &[OpId],
    ) {
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
                for result in &mut results {
                    if *result == old {
                        *result = new;
                    }
                }
                self.set_region_results(nested, results);
            }
        }
    }

    /// [`Context::rename_region_results`] for a set of renames, in one walk:
    /// finding the nested regions costs the whole subtree, so a rename per
    /// value is quadratic in the operations under `region`. A result named by
    /// `renames` follows the chain to the value nothing renames.
    pub(crate) fn rename_region_results_batch(
        &self,
        region: RegionId,
        renames: &std::collections::HashMap<ValueId, ValueId>,
    ) {
        if renames.is_empty() {
            return;
        }
        for nested in self.nested_regions(region) {
            let handle = self.get_region(nested);
            let mut results = handle.results();
            let mut renamed = false;
            for result in &mut results {
                while let Some(&next) = renames.get(result) {
                    *result = next;
                    renamed = true;
                }
            }
            if renamed {
                self.set_region_results(nested, results);
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
    /// carried value in every aligned range, after the values it carries and
    /// ahead of its states, or last when the port is a state itself. A loop's
    /// exit value defaults to the port; a gamma's arms must each name what
    /// they produce for it. Every non-local exit that leaves `op` gains the
    /// port of the region it sits in.
    pub(crate) fn grow_declared_port(
        &self,
        op: OpId,
        ty: TypeId,
        init: Option<ValueId>,
        mut latch: impl FnMut(RegionId, Option<ValueId>) -> Option<ValueId>,
    ) -> ValueId {
        let handle = self.get_op(op);
        // Every list keeps its values ahead of its states, so a value joins
        // its range after the last value there.
        let at = |list: &[ValueId], range: std::ops::Range<usize>| {
            if ty == TypeId::STATE {
                range.end
            } else {
                range.start + self.values_among(&list[range]).len()
            }
        };
        let port_of = |region: RegionId, index: usize| {
            let port = self.create_value(ty, None);
            self.insert_region_port(region, index, port.clone());
            port.id()
        };
        let feed = |region: RegionId, port: ValueId| {
            for exit in self.exits_leaving(&self.get_region(region).op_ids(), op) {
                self.append_operand(exit, port);
            }
        };
        if let Some(theta) = handle.clone().as_interface::<dyn Theta>() {
            let body = theta.body();
            let binding = theta.carried();
            let init = init.expect("a loop port carries a value in");
            let region = self.get_region(body);
            let ports: Vec<ValueId> = region.ports().iter().map(crate::Value::id).collect();
            let results = region.results();
            self.insert_operand_at(
                op,
                at(&handle.operands(), binding.operands.clone()),
                init,
                binding.operands.end,
            );
            let port = port_of(body, at(&ports, binding.ports));
            let carried = latch(body, Some(port)).unwrap_or(port);
            self.insert_region_result(body, at(&results, binding.exit), port);
            self.insert_region_result(body, at(&results, binding.continue_), carried);
            feed(body, port);
        } else if let Some(gamma) = handle.clone().as_interface::<dyn Gamma>() {
            let arms = gamma.arms();
            let binding = gamma.forwarded();
            if let Some(init) = init {
                self.insert_operand_at(
                    op,
                    at(&handle.operands(), binding.operands.clone()),
                    init,
                    binding.operands.end,
                );
            }
            for &arm in &arms {
                let region = self.get_region(arm);
                let ports: Vec<ValueId> = region.ports().iter().map(crate::Value::id).collect();
                let port = init.map(|_| port_of(arm, at(&ports, binding.ports.clone())));
                let produced = latch(arm, port).expect("every arm produces the port's value");
                self.insert_region_result(
                    arm,
                    at(&region.results(), binding.exit.clone()),
                    produced,
                );
                if let Some(port) = port {
                    feed(arm, port);
                }
            }
        } else {
            panic!("grow_declared_port needs a Theta or Gamma");
        }
        let result = self.create_value(ty, Some(op)).id();
        let results = handle.results();
        let binding = crate::binding::declared(&self.get_op(op)).expect("a loop or a gate");
        self.insert_result_at(op, at(&results, binding.results), result);
        result
    }

    /// Drop the `ordinal`-th state a loop or a gate carries, the inverse of
    /// [`Context::grow_port`] for a chain. The chain flows past the operation
    /// instead of through it, which is what a chain the body leaves alone was
    /// doing all along.
    ///
    /// The caller has already handed the port's readers what the operation was
    /// entered on and its result's readers that same state; what is left is the
    /// port list, and it goes.
    pub fn drop_state(&self, op: OpId, ordinal: usize) {
        let handle = self.get_op(op);
        let slot = crate::binding::state_slots(self, &handle)[ordinal];
        let mut removed: Vec<usize> = slot.continue_.into_iter().chain([slot.exit]).collect();
        removed.sort_unstable_by(|a, b| b.cmp(a));
        for region in handle.regions() {
            let mut results = self.get_region(region).results();
            for &at in &removed {
                results.remove(at);
            }
            self.set_region_results(region, results);
            self.remove_region_port(region, slot.port);
        }
        self.remove_operand(op, slot.operand);
        self.remove_result(op, slot.result);
    }
}
