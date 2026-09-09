//! Memory order constructed over ordered blocks, before the order is gone:
//! `docs/design/ir.md` §6.2 states which chains an effect names and §6.3 how
//! reads fork off a change.
//!
//! What the construction adds to that: a change's result is split only where
//! something names one of its chains on its own, since a run of changes
//! crossing the same chains would split and join the same set at every step,
//! which orders nothing the first join did not.

use std::collections::{BTreeMap, BTreeSet};

use crate::analysis::objects::{Base, accessed_only, object_base};
use crate::analysis::{Effect, access_of, effect_of};
use crate::state::{JoinOpBuilder, SplitOpBuilder};
use crate::{BlockId, Context, OpHandle, OpId, Operation, PassError, RegionId, TypeId, ValueId};

use super::cfg::unsupported;

/// The memory one chain stands for: an object the analysis can name, or
/// everything whose provenance it cannot.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum ChainKey {
    Object(Base),
    World,
}

/// The chains a region's memory is threaded on, and the chains each effect
/// touches — its own first, so a consumer walking an access's chain finds it
/// at index zero of the join it takes and of the split it leaves.
pub struct Plan {
    keys: Vec<ChainKey>,
    touched: BTreeMap<OpId, Vec<usize>>,
}

impl Plan {
    /// How many chains the region is threaded on.
    pub fn chains(&self) -> usize {
        self.keys.len()
    }

    /// The chains `ops` name between them: what a block has to be entered on,
    /// which is rarely every chain the function is threaded on.
    pub fn carried(&self, ops: &[OpId]) -> BTreeSet<usize> {
        ops.iter()
            .filter_map(|op| self.touched.get(op))
            .flatten()
            .copied()
            .collect()
    }
}

/// Read the chains `region` needs: one per object its accesses name, plus the
/// world where anything reaches memory the analysis cannot read back.
pub fn plan(context: &Context, region: RegionId) -> Plan {
    let ops = crate::analysis::regions::region_ops(context, region);
    let effects: Vec<(OpId, Option<Base>)> = ops
        .iter()
        .map(|&op| (op, context.get_op(op)))
        .filter(|(_, handle)| effect_of(handle).is_some())
        .map(|(op, handle)| {
            let base = access_of(&handle).and_then(|access| object_base(context, access.location));
            (op, base)
        })
        .collect();
    let objects: BTreeSet<Base> = effects.iter().filter_map(|&(_, base)| base).collect();
    let world = effects.iter().any(|(_, base)| base.is_none());
    let keys: Vec<ChainKey> = objects
        .into_iter()
        .map(ChainKey::Object)
        .chain(world.then_some(ChainKey::World))
        .collect();

    let private: Vec<bool> = keys
        .iter()
        .map(|key| match key {
            ChainKey::Object(base) => is_private(context, *base),
            ChainKey::World => false,
        })
        .collect();
    let touched: BTreeMap<OpId, Vec<usize>> = effects
        .into_iter()
        .map(|(op, base)| (op, touched_chains(&keys, &private, base)))
        .collect();
    let (keys, mut touched) = merge_indistinguishable(keys, touched);
    for &op in &ops {
        let handle = context.get_op(op);
        if !super::is_ordered_counted_loop(context, &handle) {
            continue;
        }
        let carried: BTreeSet<usize> = crate::analysis::regions::subtree_ops(context, &handle)
            .into_iter()
            .filter_map(|inner| touched.get(&inner))
            .flatten()
            .copied()
            .collect();
        touched.insert(op, carried.into_iter().collect());
    }
    Plan { keys, touched }
}

/// Two chains no effect ever tells apart are one chain. Every effect that
/// names either names both, so their accesses are already ordered against each
/// other and merging them states nothing new — it just spares the merge and the
/// split that crossing them would otherwise cost at every effect. An escaped
/// allocation and the world are the pair this is usually about.
fn merge_indistinguishable(
    keys: Vec<ChainKey>,
    touched: BTreeMap<OpId, Vec<usize>>,
) -> (Vec<ChainKey>, BTreeMap<OpId, Vec<usize>>) {
    // One bit per effect per chain: which effects name a chain is what tells it
    // apart from another, and a bitset says that in a word rather than a tree.
    let words = touched.len().div_ceil(64);
    let mut names = vec![0u64; words * keys.len()];
    for (effect, chains) in touched.values().enumerate() {
        for &chain in chains {
            names[chain * words + effect / 64] |= 1 << (effect % 64);
        }
    }
    let names = |chain: usize| &names[chain * words..(chain + 1) * words];
    let mut kept: Vec<usize> = Vec::new();
    let mut merged = Vec::with_capacity(keys.len());
    for chain in 0..keys.len() {
        merged.push(
            kept.iter()
                .position(|&other| names(other) == names(chain))
                .unwrap_or_else(|| {
                    kept.push(chain);
                    kept.len() - 1
                }),
        );
    }
    if kept.len() == keys.len() {
        return (keys, touched);
    }
    let keys = kept.iter().map(|&chain| keys[chain]).collect();
    let touched = touched
        .into_iter()
        .map(|(op, chains)| {
            let mut mapped: Vec<usize> = Vec::with_capacity(chains.len());
            for chain in chains {
                if !mapped.contains(&merged[chain]) {
                    mapped.push(merged[chain]);
                }
            }
            (op, mapped)
        })
        .collect();
    (keys, touched)
}

/// Whether nothing but the object's own accesses can reach it: a fresh
/// allocation or a parameter the λ declares free of aliases, whose address
/// never leaves those accesses. No pointer of unknown origin and no callee
/// names it, so its chain carries the world's effects on no account.
fn is_private(context: &Context, base: Base) -> bool {
    match base {
        Base::Alloca(pointer)
        | Base::Param {
            pointer,
            noalias: true,
        } => accessed_only(context, pointer),
        _ => false,
    }
}

/// The chains an effect on `base` touches, its own first: every chain whose
/// object it may alias. An effect naming no object reaches the world and every
/// object the world can reach; an effect on a private object reaches nothing
/// else, and nothing else reaches it.
fn touched_chains(keys: &[ChainKey], private: &[bool], base: Option<Base>) -> Vec<usize> {
    let own = keys.iter().position(|key| match (base, key) {
        (Some(base), ChainKey::Object(object)) => base == *object,
        (None, ChainKey::World) => true,
        _ => false,
    });
    let owned_private = own.is_some_and(|own| private[own]);
    let rest = (0..keys.len()).filter(|&index| {
        Some(index) != own
            && match (base, keys[index]) {
                (Some(base), ChainKey::Object(object)) => !base.distinct(object),
                (Some(_), ChainKey::World) => !owned_private,
                (None, _) => !private[index],
            }
    });
    own.into_iter().chain(rest).collect()
}

/// Whether `region` needs a chain constructed: something in it touches memory,
/// and nothing already names a dependency, which would make a second order
/// over the one that is there.
pub fn wants_chain(context: &Context, region: RegionId) -> bool {
    let ops: Vec<OpHandle> = crate::analysis::regions::region_ops(context, region)
        .into_iter()
        .map(|op| context.get_op(op))
        .collect();
    let threaded = ops
        .iter()
        .any(|op| !op.state_operands().is_empty() || !op.state_results().is_empty());
    !threaded
        && ops
            .iter()
            .any(|op| !matches!(effect(context, op), Ok(None)))
}

/// Thread `block`'s operations, its terminator excluded, off the state each
/// chain is entered on, and answer the memory each chain leaves the block with.
/// Joins go before the operation that takes them, splits after the one that
/// leaves them.
pub fn thread_block(
    context: &Context,
    block: BlockId,
    entries: &BTreeMap<usize, ValueId>,
    plan: &Plan,
) -> Result<BTreeMap<usize, ValueId>, PassError> {
    let ops = context.get_block(block).op_ids();
    let (&terminator, body) = ops
        .split_last()
        .ok_or_else(|| unsupported("a block with no terminator"))?;
    let mut chains = Chains {
        context,
        states: entries
            .iter()
            .map(|(&chain, &written)| {
                (
                    chain,
                    ChainState {
                        written,
                        reads: Vec::new(),
                    },
                )
            })
            .collect(),
        shared: None,
    };
    for &op in body {
        let handle = context.get_op(op);
        // A counted loop the frontend raised carries one dependency port per
        // chain its body touches, so it takes one dependency operand per chain
        // rather than the one state a change merges them into.
        if super::is_ordered_counted_loop(context, &handle) {
            let touched = plan.touched[&op].clone();
            if touched.is_empty() {
                continue;
            }
            let mut observed = Vec::with_capacity(touched.len());
            for &chain in &touched {
                observed.push(chains.close_fork(chain, op)?);
            }
            let body = handle.regions()[0];
            let [body_block] = context.get_region(body).block_ids()[..] else {
                return Err(unsupported("a counted loop whose body is a graph"));
            };
            let ports: BTreeMap<usize, ValueId> = touched
                .iter()
                .map(|&chain| {
                    (
                        chain,
                        context
                            .append_block_argument(body_block, TypeId::STATE)
                            .id(),
                    )
                })
                .collect();
            let leaving = thread_block(context, body_block, &ports, plan)?;
            let latch = *context.get_block(body_block).op_ids().last().unwrap();
            for &chain in &touched {
                context.append_operand(latch, leaving[&chain]);
            }
            // A chain the loop carries is one more init, in the group the
            // binding ranges over, not a trailing operand after the bounds.
            for state in observed {
                let for_op = crate::scf::ForOp::from_op_instance(context.get_op(op));
                let end = crate::Theta::carried(&for_op).operands.end;
                context.insert_operand_at(op, end, state, end);
            }
            for &chain in &touched {
                let published = context.append_result(op, TypeId::STATE);
                chains.state(chain)?.written = published;
            }
            continue;
        }
        match effect(context, &handle)? {
            None => {}
            Some(Effect::Read) => {
                let own = plan.touched[&op][0];
                let state = chains.state(own)?;
                context.append_operand(op, state.written);
                let left = context.append_result(op, TypeId::STATE);
                chains.state(own)?.reads.push(left);
            }
            Some(Effect::Change) => {
                let touched = plan.touched[&op].clone();
                let observed = chains.settle(&touched, op)?;
                context.append_operand(op, observed);
                let published = context.append_result(op, TypeId::STATE);
                chains.split(&touched, op, published)?;
            }
        }
    }
    let leaving: Vec<usize> = chains.states.keys().copied().collect();
    leaving
        .into_iter()
        .map(|chain| Ok((chain, chains.close_fork(chain, terminator)?)))
        .collect()
}

/// Whether two effects cross the same chains, however each orders them: a
/// state standing for a set of chains answers either.
fn same_chains(held: &[usize], wanted: &[usize]) -> bool {
    held.len() == wanted.len() && wanted.iter().all(|chain| held.contains(chain))
}

/// What `op` does to memory, refusing an effect nested where the conversion
/// has no port to carry it through.
fn effect(context: &Context, op: &OpHandle) -> Result<Option<Effect>, PassError> {
    if let Some(effect) = effect_of(op) {
        return Ok(Some(effect));
    }
    let nested = crate::analysis::regions::subtree_ops(context, op)
        .into_iter()
        .any(|inner| effect_of(&context.get_op(inner)).is_some());
    if nested {
        return Err(unsupported(&format!(
            "memory effects inside {}.{}",
            op.dialect(),
            op.name()
        )));
    }
    Ok(None)
}

struct ChainState {
    written: ValueId,
    reads: Vec<ValueId>,
}

/// One state left by a change that crossed several chains, standing for all of
/// them until something names one on its own.
struct Shared {
    chains: Vec<usize>,
    published: ValueId,
    after: OpId,
}

struct Chains<'a> {
    context: &'a Context,
    states: BTreeMap<usize, ChainState>,
    shared: Option<Shared>,
}

impl Chains<'_> {
    fn state(&mut self, chain: usize) -> Result<&mut ChainState, PassError> {
        self.name_shared()?;
        self.states
            .get_mut(&chain)
            .ok_or_else(|| unsupported("an effect on a chain its region does not carry"))
    }

    /// Name each chain a shared state stands for again, which is what a
    /// `state.split` is for. Called where a chain is wanted on its own, so a
    /// state no effect ever takes apart is never split.
    fn name_shared(&mut self) -> Result<(), PassError> {
        let Some(shared) = self.shared.take() else {
            return Ok(());
        };
        let split = SplitOpBuilder::new(self.context)
            .state(shared.published)
            .states(shared.chains.len())
            .build();
        self.insert(split.id(), shared.after, 1);
        for (&chain, &state) in shared.chains.iter().zip(&split.states()) {
            self.state(chain)?.written = state;
        }
        Ok(())
    }

    /// The memory each of `chains` stands at once the reads forked off it are
    /// closed, joined into one state where the effect crosses several.
    ///
    /// A state left by a change that crossed exactly these chains already
    /// stands for their memory: splitting it into names this effect would only
    /// join back says nothing, so the effect takes it as it is.
    ///
    /// Two changes cross the same chains only where their objects reach each
    /// other — the same object, two parameters, or two effects on memory of
    /// unknown provenance — so a reader that follows the state back to one
    /// chain of the pair still meets every access that may alias either.
    fn settle(&mut self, chains: &[usize], before: OpId) -> Result<ValueId, PassError> {
        if let Some(shared) = &self.shared
            && same_chains(&shared.chains, chains)
        {
            return Ok(shared.published);
        }
        let mut settled = Vec::with_capacity(chains.len());
        for &chain in chains {
            settled.push(self.close_fork(chain, before)?);
        }
        Ok(match settled[..] {
            [one] => one,
            _ => self.merge(&settled, before),
        })
    }

    /// The memory after every read of one chain's open fork: the last change
    /// where none forked off it, the one read's state where one did, their
    /// join otherwise.
    fn close_fork(&mut self, chain: usize, before: OpId) -> Result<ValueId, PassError> {
        let state = self.state(chain)?;
        let reads = std::mem::take(&mut state.reads);
        let written = state.written;
        let settled = match reads.len() {
            0 => written,
            1 => reads[0],
            _ => self.merge(&reads, before),
        };
        self.state(chain)?.written = settled;
        Ok(settled)
    }

    /// A `state.join` of `states`, placed where the operation taking it is.
    fn merge(&self, states: &[ValueId], before: OpId) -> ValueId {
        let join = JoinOpBuilder::new(self.context)
            .states(states.to_vec())
            .state_result()
            .build();
        self.insert(join.id(), before, 0);
        join.result()
    }

    /// The memory the change that left `published` stands for: its own chain
    /// where it crossed one, all the chains it crossed otherwise, held until
    /// something names one of them on its own ([`Self::name_shared`]).
    fn split(
        &mut self,
        chains: &[usize],
        after: OpId,
        published: ValueId,
    ) -> Result<(), PassError> {
        if let [one] = chains[..] {
            self.state(one)?.written = published;
            return Ok(());
        }
        self.shared = Some(Shared {
            chains: chains.to_vec(),
            published,
            after,
        });
        Ok(())
    }

    /// Place `op` `offset` operations past `anchor` in the block holding it.
    fn insert(&self, op: OpId, anchor: OpId, offset: usize) {
        let block = self
            .context
            .get_block(self.context.parent_block(anchor).expect("an op in a block"));
        let at = block
            .op_ids()
            .iter()
            .position(|&held| held == anchor)
            .expect("the op sits in its block");
        block.insert(at + offset, op);
    }
}
