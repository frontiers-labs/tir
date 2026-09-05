//! Destruction of structured control flow, at emission.
//!
//! The regions of a function that has already selected become blocks of the
//! function's own region: an arm, a loop body and a loop's test are moved there
//! whole, so every instruction emission put in them keeps its place, and what a
//! region yields rides the edge leaving it. Only the joins and the chain a
//! multi-case gate tests through are new blocks.
//!
//! Everything is read off the operations' interfaces — [`Conditional`] for a
//! gate's arms and cases, [`GuardedLoop`] for how a loop is entered,
//! [`CountedLoop`] for the counter a counted loop advances. The values the
//! branches read were selected with the blocks holding them and arrive here as
//! [`AuxSlot`]s.

use std::collections::HashMap;
use tir::BlockHandle;

use tir::{
    BlockId, Conditional, Context, CountedLoop, EntryGuard, GuardedLoop, LoopLike, OpHandle, OpId,
    OperationRef, PassError, RegionId, Rewriter, ValueId,
    analysis::scopes::{RegionExit, loop_scope, region_exit_kind},
    builtin::TokenType,
};

use super::builder::AuxSlot;
use super::emit::{AuxEmit, GuardBranch};
use super::{BranchEmitters, Rule, RuleKind};

/// Create a block forwarding `args` to `dest` through the target's unconditional
/// branch, appended to `region`. Returns its id, which a conditional branch
/// targets in place of `dest`: a taken edge has no operand slot of its own to
/// carry assignments in, so they are performed on a block only that edge reaches.
pub(crate) fn trampoline(
    context: &Context,
    region: Option<RegionId>,
    dest: BlockId,
    args: &[ValueId],
    emitters: &BranchEmitters,
) -> BlockId {
    let block = context.create_block(vec![]);
    block.append((emitters.uncond)(context, dest, args).id());
    if let Some(region) = region {
        context.get_region(region).add_block(block.id());
    }
    block.id()
}

/// Where a loop's `break` and `continue` edges land once it is a CFG.
#[derive(Clone, Copy)]
struct LoopTargets {
    break_dest: BlockId,
    continue_dest: BlockId,
}

/// The edge a region block that computes nothing stands for. Such a block is
/// never added to the function: an edge into it is an edge to where it goes,
/// with the arguments that edge carries taking the place of its parameters.
struct Forward {
    dest: BlockId,
    args: Vec<ValueId>,
    parameters: Vec<ValueId>,
}

impl Forward {
    fn resolve(&self, incoming: &[ValueId]) -> (BlockId, Vec<ValueId>) {
        let substitution: HashMap<ValueId, ValueId> = self
            .parameters
            .iter()
            .copied()
            .zip(incoming.iter().copied())
            .collect();
        let args = self
            .args
            .iter()
            .map(|value| substitution.get(value).copied().unwrap_or(*value))
            .collect();
        (self.dest, args)
    }
}

pub(crate) struct Destructor<'a> {
    context: &'a Context,
    emitters: &'a BranchEmitters,
    /// What selection left each IR value as.
    emitted: &'a HashMap<ValueId, ValueId>,
    region_values: &'a HashMap<(OpId, AuxSlot), AuxEmit>,
    rules: &'a [Rule],
    region: RegionId,
    targets: HashMap<ValueId, LoopTargets>,
}

impl<'a> Destructor<'a> {
    pub(crate) fn new(
        context: &'a Context,
        emitters: &'a BranchEmitters,
        emitted: &'a HashMap<ValueId, ValueId>,
        region_values: &'a HashMap<(OpId, AuxSlot), AuxEmit>,
        rules: &'a [Rule],
        region: RegionId,
    ) -> Self {
        Self {
            context,
            emitters,
            emitted,
            region_values,
            rules,
            region,
            targets: HashMap::new(),
        }
    }

    /// Destruct every structured operation of the function, outermost first: a
    /// nested one is reached again once its region is a block of the function.
    pub(crate) fn run(&mut self, rewriter: &mut Rewriter) -> Result<(), PassError> {
        while let Some((block, op)) = self.next_structured() {
            self.lower_one(rewriter, block, op)?;
        }
        Ok(())
    }

    fn next_structured(&self) -> Option<(BlockHandle, OpHandle)> {
        for block in self
            .context
            .get_region(self.region)
            .iter(self.context.clone())
        {
            for op_id in block.op_ids() {
                let op = self.context.get_op(op_id);
                if !op.regions().is_empty() && op.clone().as_interface::<dyn LoopLike>().is_some()
                    || !op.regions().is_empty()
                        && op.clone().as_interface::<dyn Conditional>().is_some()
                {
                    return Some((block, op));
                }
            }
        }
        None
    }

    fn lower_one(
        &mut self,
        rewriter: &mut Rewriter,
        block: BlockHandle,
        op: OpHandle,
    ) -> Result<(), PassError> {
        if let Some(conditional) = op.clone().as_interface::<dyn Conditional>() {
            return self.lower_gate(rewriter, block, &op, conditional.as_ref());
        }
        let guard = op
            .clone()
            .as_interface::<dyn GuardedLoop>()
            .ok_or_else(|| Self::decline(&op, "loop states no entry guard"))?;
        match guard.entry_guard() {
            EntryGuard::Region { .. } => self.lower_tested_loop(rewriter, block, &op),
            EntryGuard::Less { .. } => self.lower_counted_loop(rewriter, block, &op),
            EntryGuard::AlwaysTaken => Err(Self::decline(&op, "loop is never left")),
        }
    }

    fn decline(op: &OpHandle, reason: &str) -> PassError {
        PassError::InvalidRuleSet(format!("cannot destruct {}: {reason}", op.name()))
    }

    /// A gate becomes the chain of tests its cases describe: each case branches to
    /// its arm, the last falling through to the arm no case names. Every arm exits
    /// to the continuation, which reads what the gate produced as parameters.
    fn lower_gate(
        &mut self,
        rewriter: &mut Rewriter,
        block: BlockHandle,
        op: &OpHandle,
        conditional: &dyn Conditional,
    ) -> Result<(), PassError> {
        let cases = conditional.case_values();
        let Some(default) = cases.iter().position(|(_, case)| case.is_none()) else {
            return Err(Self::decline(op, "no arm runs when no case matches"));
        };
        let (tests, default_reached) = self.live_chain(op, cases.len(), default)?;
        // An arm no edge can reach is left in the operation's own region, so
        // erasing the operation takes it — and everything nested in it — away.
        let mut arms: Vec<Option<BlockHandle>> = vec![None; cases.len()];
        for index in tests
            .iter()
            .copied()
            .chain(default_reached.then_some(default))
        {
            arms[index] = Some(
                self.move_body(cases[index].0)
                    .ok_or_else(|| Self::decline(op, "an arm has no block"))?,
            );
        }
        // Every arm reads the same inputs — the operation's trailing operands.
        let inputs = self.entry_arguments(op, arms.iter().flatten().next().unwrap());
        let continuation = self.split_after(rewriter, &block, op);
        let mut edges: Vec<Option<(BlockId, Vec<ValueId>)>> = Vec::new();
        for arm in &arms {
            edges.push(match arm {
                Some(arm) => Some(self.seal_into(rewriter, arm, continuation.id(), &inputs)?),
                None => None,
            });
        }
        self.add_block(continuation.id());
        self.erase(rewriter, op)?;

        let Some((&last, leading)) = tests.split_last() else {
            let (dest, args) = edges[default].as_ref().unwrap();
            self.jump(&block, *dest, args);
            return Ok(());
        };
        // Each case but the last is tested in a block of its own, the one before
        // it falling through to it; the last falls through to the arm no case names.
        let mut current = block;
        for &index in leading {
            let next = self.context.create_block(vec![]);
            self.add_block(next.id());
            let (taken, taken_args) = edges[index].as_ref().unwrap();
            let test = self.aux(op, AuxSlot::Test(index))?;
            // The next test in the chain is a block of its own with no
            // parameters: what an arm reads it reads from the operation's own
            // block, so only an edge into an *arm* carries the inputs.
            self.branch(&current, test, *taken, taken_args, next.id(), &[])?;
            current = next;
        }
        let (taken, taken_args) = edges[last].as_ref().unwrap();
        let Some((fallthrough, fallthrough_args)) = edges[default].as_ref() else {
            // The last test holds, so the arm no case names is not reached.
            self.jump(&current, *taken, taken_args);
            return Ok(());
        };
        let test = self.aux(op, AuxSlot::Test(last))?;
        self.branch(
            &current,
            test,
            *taken,
            taken_args,
            *fallthrough,
            fallthrough_args,
        )
    }

    /// The cases a gate still tests, and whether the arm no case names is
    /// reached. A test the arm's own scope decided either takes its case — no
    /// later case and no default can run — or excludes it from the chain.
    fn live_chain(
        &self,
        op: &OpHandle,
        arms: usize,
        default: usize,
    ) -> Result<(Vec<usize>, bool), PassError> {
        let mut chain = Vec::new();
        for index in (0..arms).filter(|index| *index != default) {
            match self.aux(op, AuxSlot::Test(index))? {
                AuxEmit::Decided(false) => {}
                AuxEmit::Decided(true) => {
                    chain.push(index);
                    return Ok((chain, false));
                }
                _ => chain.push(index),
            }
        }
        Ok((chain, true))
    }

    /// A loop tested before each iteration becomes its test region followed by its
    /// body: the test's own terminator branches into the body with what it forwards
    /// and out to the continuation with the same, and the body's edges return to the
    /// test.
    fn lower_tested_loop(
        &mut self,
        rewriter: &mut Rewriter,
        block: BlockHandle,
        op: &OpHandle,
    ) -> Result<(), PassError> {
        let test = self
            .move_body(op.regions()[0])
            .ok_or_else(|| Self::decline(op, "loop test has no block"))?;
        let (body, scope) = self
            .move_loop_body(op.regions()[1])
            .ok_or_else(|| Self::decline(op, "loop body has no block"))?;
        let inits = self.mapped(&op.operands());
        let continuation = self.split_after(rewriter, &block, op);
        self.add_block(test.id());
        if let Some(scope) = scope {
            self.targets.insert(
                scope,
                LoopTargets {
                    break_dest: continuation.id(),
                    continue_dest: test.id(),
                },
            );
        }
        let terminator = self.terminator(&test)?;
        let ports = body.arguments().len();
        let forwarded = self.mapped(&terminator.operands()[terminator.operands().len() - ports..]);
        let (taken_dest, taken_args) = self.seal_into(rewriter, &body, test.id(), &forwarded)?;
        self.add_block(continuation.id());
        self.erase(rewriter, op)?;
        self.jump(&block, test.id(), &inits);

        let taken = self.aux(op, AuxSlot::Test(0))?;
        let branch_block = test.clone();
        rewriter.erase_op(&OperationRef::new(terminator))?;
        self.branch(
            &branch_block,
            taken,
            taken_dest,
            &taken_args,
            continuation.id(),
            &forwarded,
        )
    }

    /// A counted loop is destructed rotated: the zero-trip guard branches into the
    /// body, which advances the counter and tests it again on its own back edge. The
    /// counter enters the body as its last value argument, ahead of the chains the
    /// loop carries — a value only this destruction mints, which is why it is not
    /// a port of the loop.
    fn lower_counted_loop(
        &mut self,
        rewriter: &mut Rewriter,
        block: BlockHandle,
        op: &OpHandle,
    ) -> Result<(), PassError> {
        let counted = op
            .clone()
            .as_interface::<dyn CountedLoop>()
            .ok_or_else(|| Self::decline(op, "counted loop states no bounds"))?;
        let lower = counted.lower_bound();
        let loop_like = op
            .clone()
            .as_interface::<dyn LoopLike>()
            .ok_or_else(|| Self::decline(op, "loop carries nothing"))?;
        let latched = loop_like.latched();
        let (body, scope) = self
            .move_loop_body(op.regions()[0])
            .ok_or_else(|| Self::decline(op, "loop body has no block"))?;
        if let Some(scope) = scope
            && self.leaves_through(&body, scope, RegionExit::Continue)
        {
            return Err(Self::decline(
                op,
                "a counted loop advanced in its own body cannot be continued into",
            ));
        }
        let inits = self.mapped(&loop_like.inits());
        let deps = op.dep_operands().len();
        let continuation = self.split_after(rewriter, &block, op);
        self.add_block(body.id());
        self.add_block(continuation.id());
        if let Some(scope) = scope {
            // Only `break` can reach this: a `continue` was declined above,
            // because the advance sits in the body and has no block to jump to.
            self.targets.insert(
                scope,
                LoopTargets {
                    break_dest: continuation.id(),
                    continue_dest: body.id(),
                },
            );
        }
        self.erase(rewriter, op)?;

        // Selection minted the counter as the body's last value argument only
        // where no carried port already counted; where one did, the ports are
        // the whole edge.
        let minted = body.arguments().len() > inits.len();
        let mut entry = inits.clone();
        if minted {
            entry.insert(entry.len() - deps, self.value(lower));
        }
        self.branch(
            &block,
            self.aux(op, AuxSlot::Entry)?,
            body.id(),
            &entry,
            continuation.id(),
            &inits,
        )?;

        let mut back = self.mapped(&latched);
        if minted {
            back.insert(back.len() - deps, self.aux_value(op, AuxSlot::Advance)?);
        }
        let exit = self.mapped(&latched);
        let terminator = self.terminator(&body)?;
        let body_block = self.context.get_block(body.id());
        rewriter.erase_op(&OperationRef::new(terminator))?;
        self.branch(
            &body_block,
            self.aux(op, AuxSlot::Latch)?,
            body.id(),
            &back,
            continuation.id(),
            &exit,
        )
    }

    /// Give a region block the edge its own terminator stood for: a yield falls
    /// through to `fallthrough`, an exit goes to the target its scope names. A
    /// block that leaves the function keeps its terminator.
    ///
    /// A block left with nothing but that edge is not a block at all, and is
    /// reported as the [`Forward`] its predecessors take instead — the join, the
    /// trampoline and the header a jump-only block would otherwise sit in front
    /// of are exactly the blocks destruction must not mint.
    fn seal(
        &self,
        rewriter: &mut Rewriter,
        block: &BlockHandle,
        fallthrough: BlockId,
    ) -> Result<Option<Forward>, PassError> {
        let block = self.context.get_block(block.id());
        let terminator = self.terminator(&block)?;
        let (dest, args) = match region_exit_kind(&terminator) {
            Some(RegionExit::Yield) => (fallthrough, terminator.operands().to_vec()),
            Some(kind) => {
                let targets = self
                    .targets
                    .get(&terminator.operands()[0])
                    .ok_or_else(|| Self::decline(&terminator, "exit names no enclosing loop"))?;
                let dest = match kind {
                    RegionExit::Break => targets.break_dest,
                    _ => targets.continue_dest,
                };
                (dest, terminator.operands()[1..].to_vec())
            }
            None => return Ok(None),
        };
        let args = self.mapped(&args);
        let terminator = OperationRef::new(terminator);
        if block.op_ids().len() == 1 {
            rewriter.erase_op(&terminator)?;
            return Ok(Some(Forward {
                dest,
                args,
                parameters: block.arguments().iter().map(|value| value.id()).collect(),
            }));
        }
        let jump = (self.emitters.uncond)(self.context, dest, &args);
        rewriter.replace_op(&terminator, jump.as_ref())?;
        Ok(None)
    }

    /// Seal a region block and give the function the block, where one is left.
    /// Returns where an edge carrying `incoming` into it really goes.
    fn seal_into(
        &self,
        rewriter: &mut Rewriter,
        block: &BlockHandle,
        fallthrough: BlockId,
        incoming: &[ValueId],
    ) -> Result<(BlockId, Vec<ValueId>), PassError> {
        match self.seal(rewriter, block, fallthrough)? {
            Some(forward) => Ok(forward.resolve(incoming)),
            None => {
                self.add_block(block.id());
                Ok((block.id(), incoming.to_vec()))
            }
        }
    }

    /// Whether `body` holds an exit of `kind` leaving `scope`.
    fn leaves_through(&self, body: &BlockHandle, scope: ValueId, kind: RegionExit) -> bool {
        body.op_ids().into_iter().any(|op_id| {
            let op = self.context.get_op(op_id);
            region_exit_kind(&op) == Some(kind) && op.operands().first() == Some(&scope)
        })
    }

    /// Branch on a selected test: the taken edge goes to `taken`, through a block
    /// only it reaches where it carries assignments, and the untaken one falls
    /// through carrying its own.
    fn branch(
        &self,
        block: &BlockHandle,
        test: &AuxEmit,
        taken: BlockId,
        taken_args: &[ValueId],
        fallthrough: BlockId,
        fallthrough_args: &[ValueId],
    ) -> Result<(), PassError> {
        // Both arms of a gate can lead to the same place with the same values —
        // arms that compute nothing sealing onto the gate's own join, say. There
        // is nothing to decide, so no test and no trampoline are emitted.
        if taken == fallthrough && taken_args == fallthrough_args {
            self.jump(block, fallthrough, fallthrough_args);
            return Ok(());
        }
        // A test the block's assumptions decided leaves one reachable edge.
        if let AuxEmit::Decided(holds) = test {
            let (dest, args) = if *holds {
                (taken, taken_args)
            } else {
                (fallthrough, fallthrough_args)
            };
            self.jump(block, dest, args);
            return Ok(());
        }
        // A taken edge has no operand slot to carry assignments in, so one that
        // performs any goes through a block only it reaches. A dependency
        // argument is not an assignment — nothing moves for it, and the
        // parameter names the chain the join is entered on — so an edge
        // carrying only those needs no block of its own.
        let deps = self.context.get_block(taken).dep_arguments().len();
        let target = if taken_args.len() > deps {
            trampoline(
                self.context,
                Some(self.region),
                taken,
                taken_args,
                self.emitters,
            )
        } else {
            taken
        };
        let AuxEmit::Branch(branch) = test else {
            return Err(PassError::InvalidRuleSet(
                "a gate test did not select a branch".to_string(),
            ));
        };
        match branch {
            GuardBranch::Nonzero { condition } => {
                for op in (self.emitters.cond_nonzero)(self.context, *condition, target) {
                    block.append(op.id());
                }
            }
            GuardBranch::Fused { rule_index, m } => {
                let rule = &self.rules[*rule_index];
                let RuleKind::CondBranch { target_symbol } = rule.kind else {
                    return Err(PassError::InvalidRuleSet(
                        "a fused gate test is not a conditional branch".to_string(),
                    ));
                };
                let mut m = m.clone();
                m.rebind_block(target_symbol, target);
                let request = super::EmitRequest {
                    op: None,
                    results: &[],
                    result_ty: None,
                    state: None,
                };
                if let Some(prelude) = rule.prelude_emit {
                    block.append(prelude(self.context, &request, &m)?.id());
                }
                block.append((rule.emit_fn)(self.context, &request, &m)?.id());
            }
        }
        self.jump(block, fallthrough, fallthrough_args);
        Ok(())
    }

    fn jump(&self, block: &BlockHandle, dest: BlockId, args: &[ValueId]) {
        block.append((self.emitters.uncond)(self.context, dest, args).id());
    }

    fn aux(&self, op: &OpHandle, slot: AuxSlot) -> Result<&'a AuxEmit, PassError> {
        self.region_values
            .get(&(op.id, slot))
            .ok_or_else(|| Self::decline(op, &format!("{slot:?} was not selected")))
    }

    /// The register a counter's advance landed in.
    fn aux_value(&self, op: &OpHandle, slot: AuxSlot) -> Result<ValueId, PassError> {
        match self.aux(op, slot)? {
            AuxEmit::Value(value) => Ok(*value),
            AuxEmit::Branch(_) | AuxEmit::Decided(_) => {
                Err(Self::decline(op, "a counter advance is not a branch"))
            }
        }
    }

    fn value(&self, value: ValueId) -> ValueId {
        self.emitted.get(&value).copied().unwrap_or(value)
    }

    fn mapped(&self, values: &[ValueId]) -> Vec<ValueId> {
        values.iter().map(|&value| self.value(value)).collect()
    }

    /// The inputs a gate forwards into each arm: its trailing operands, one per
    /// entry argument, the mapping the seeder read the arms under.
    fn entry_arguments(&self, op: &OpHandle, arm: &BlockHandle) -> Vec<ValueId> {
        let first = op.operands().len().saturating_sub(arm.arguments().len());
        self.mapped(&op.operands()[first..])
    }

    fn terminator(&self, block: &BlockHandle) -> Result<OpHandle, PassError> {
        let block = self.context.get_block(block.id());
        block
            .op_ids()
            .last()
            .map(|&op| self.context.get_op(op))
            .ok_or_else(|| PassError::InvalidRuleSet("a region block is empty".to_string()))
    }

    fn add_block(&self, block: BlockId) {
        self.context.get_region(self.region).add_block(block);
    }

    /// Take a region's block out of it, so erasing the operation cannot reclaim it.
    fn move_body(&self, region: RegionId) -> Option<BlockHandle> {
        let block = self
            .context
            .get_region(region)
            .iter(self.context.clone())
            .next()?;
        self.context.get_region(region).remove_block(block.id());
        Some(self.context.get_block(block.id()))
    }

    /// A loop body, ready to be a block: the token its exits name is not a value of
    /// a CFG, so the argument carrying it goes.
    fn move_loop_body(&self, region: RegionId) -> Option<(BlockHandle, Option<ValueId>)> {
        let scope = loop_scope(self.context, region);
        let body = self.move_body(region)?;
        let token = TokenType::new(self.context);
        if let Some(index) = body
            .arguments()
            .iter()
            .position(|argument| argument.ty() == token)
        {
            self.context.remove_block_argument(body.id(), index);
        }
        Some((self.context.get_block(body.id()), scope))
    }

    /// Split `block` after `op`, the continuation adopting what the operation
    /// produced as its parameters. Adopted, not renamed: the tiles already
    /// emitted name those values, and a rename reaches only the readers that
    /// exist when it runs.
    fn split_after(
        &self,
        rewriter: &mut Rewriter,
        block: &BlockHandle,
        op: &OpHandle,
    ) -> BlockHandle {
        let block = self.context.get_block(block.id());
        let position = block.op_ids().iter().position(|id| *id == op.id).unwrap();
        let continuation = rewriter.split_block(block.id(), position + 1);
        for result in op.value_results() {
            self.context.adopt_block_argument(continuation.id(), result);
        }
        for result in op.dep_results() {
            self.context
                .adopt_dep_block_argument(continuation.id(), result);
        }
        self.context.get_block(continuation.id())
    }

    fn erase(&self, rewriter: &mut Rewriter, op: &OpHandle) -> Result<(), PassError> {
        rewriter.erase_op(&OperationRef::new(op.clone()))
    }
}
