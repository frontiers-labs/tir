//! Building the nest a schedule names.
//!
//! The nest is rebuilt rather than edited: a chosen permutation and tiling say
//! which loop counts what and how far, and the loops that state it are fresh.
//! Only the innermost body survives, cloned once per copy the tiling asks for,
//! with each dimension's counter rebound to the loop that now counts it.
//!
//! What the rebuild may assume is checked first ([`Nest::read`]): every loop
//! carries its counter and the memory chains and nothing else, the chains run
//! straight through every level in one order, and the bounds are decided before
//! the nest runs. A nest that is anything else is left byte-identical.
//!
//! A tiled dimension whose trip count the tiles do not divide is rebuilt
//! untiled and strip-mined afterwards, which is where its remainder comes from.

use std::collections::HashMap;

use crate::analysis::affine::{AffineForm, AffineView};
use crate::builtin::ops as b;
use crate::{
    BlockHandle, Context, CountedLoop, LoopLike, OpId, Operation, OperationRef, PassError,
    RegionId, Rewriter, TypeId, Value, ValueId, scf,
};

use super::schedule::{Candidate, Level, divides_evenly, levels};
use super::strip_mine::strip_mine;

/// A counted nest in the shape the rebuild can state again.
pub(super) struct Nest {
    root: OpId,
    /// The bounds of each dimension, as values available where the nest stands.
    bounds: Vec<Bounds>,
    /// The counter each dimension's body reads, where the body reads one.
    counters: Vec<Option<ValueId>>,
    /// The innermost body: the region cloned into every copy, its arguments and
    /// what it yields, split into the counter port and the chains.
    body: RegionId,
    body_arguments: Vec<ValueId>,
    body_counter: Option<usize>,
    body_states: Vec<usize>,
    /// The loop-invariant operations the outer bodies spell, which the copied
    /// body may name and the rebuild would otherwise erase under it.
    hoist: Vec<OpId>,
    /// The states the nest is entered with and the results it leaves them in.
    entry_states: Vec<ValueId>,
    exit_states: Vec<ValueId>,
}

/// A bound of the rebuilt nest. The nest is rectangular, so every bound is
/// decided before it runs — but the operand the original loop named it with may
/// be spelled inside the nest, and the rebuild erases that. What survives is the
/// form, which names a literal or a value the nest was entered with.
#[derive(Clone, Copy)]
enum Bound {
    Literal(i128),
    Value(ValueId),
}

struct Bounds {
    lower: Bound,
    upper: Bound,
    /// What the counter gains each iteration, which tiling multiplies.
    stride: i128,
    counter_type: TypeId,
    counter_width: u32,
}

/// A bound as the rebuild can spell it again, or nothing.
fn bound_of(view: &AffineView, form: &AffineForm) -> Option<Bound> {
    if let Some(literal) = form.as_constant() {
        return Some(Bound::Literal(literal));
    }
    let named: Vec<usize> = (0..view.symbols.len())
        .filter(|&index| form.symbol_coefficient(index) != 0)
        .collect();
    match named[..] {
        [only] if form.constant_term() == 0 && form.symbol_coefficient(only) == 1 => {
            Some(Bound::Value(view.symbols[only]))
        }
        _ => None,
    }
}

impl Nest {
    /// Whether the tiles a candidate asks for can be counted in the widths the
    /// nest counts in: a tiled dimension's outer loop steps by a whole tile, and
    /// a step the counter's own width cannot hold is a step it would wrap on.
    pub(super) fn admits(&self, candidate: &Candidate) -> bool {
        self.bounds.iter().enumerate().all(|(dimension, bounds)| {
            let tile = candidate.tiles[dimension] as i128;
            let step = bounds.stride * tile;
            tile == 1 || AffineForm::fits(bounds.counter_width, step, step)
        })
    }

    /// Read the nest, or refuse it.
    pub(super) fn read(context: &Context, view: &AffineView) -> Option<Self> {
        let mut bounds = Vec::new();
        let mut states: Option<usize> = None;
        for (depth, level) in view.loops.iter().enumerate() {
            let op = context.get_op(level.op);
            let counted = op.clone().as_interface::<dyn CountedLoop>()?;
            let ports = ports(context, level.op, level.counter)?;
            if *states.get_or_insert(ports.states.len()) != ports.states.len() {
                return None;
            }
            if depth + 1 < view.loops.len() && !chains_through(context, view, depth, &ports) {
                return None;
            }
            let counter_type = context.get_value(counted.lower_bound()).ty();
            bounds.push(Bounds {
                lower: bound_of(view, &level.lower)?,
                upper: bound_of(view, &level.upper)?,
                stride: level.step.as_constant().filter(|&s| s > 0)?,
                counter_type,
                counter_width: level.width,
            });
        }

        let root = context.get_op(view.root);
        let innermost = view.loops.last()?;
        let inner_ports = ports(context, innermost.op, innermost.counter)?;
        let body = *context.get_op(innermost.op).regions().last()?;
        crate::analysis::affine::body_block(context, innermost.op)?;

        let carried = root.clone().as_interface::<dyn LoopLike>()?;
        let root_ports = ports(context, view.root, view.loops[0].counter)?;
        // The counter the nest leaves behind is not a counter of the rebuilt
        // nest, so nothing may be reading it.
        if root_ports
            .counter
            .is_some_and(|port| is_used(context, view.root, carried.finals()[port]))
        {
            return None;
        }

        Some(Self {
            root: view.root,
            hoist: hoistable(context, view)?,
            bounds,
            counters: view.loops.iter().map(|level| level.counter).collect(),
            body,
            body_arguments: inner_ports.arguments.clone(),
            body_counter: inner_ports.counter,
            body_states: inner_ports.states.clone(),
            entry_states: root_ports
                .states
                .iter()
                .map(|&port| carried.inits()[port])
                .collect(),
            exit_states: root_ports
                .states
                .iter()
                .map(|&port| carried.finals()[port])
                .collect(),
        })
    }
}

/// How a loop's carried ports divide: the counter, and the memory chains. A port
/// that is neither is a recurrence the rebuild would have to carry through a new
/// loop order, which v1 does not do.
struct Ports {
    arguments: Vec<ValueId>,
    counter: Option<usize>,
    states: Vec<usize>,
}

fn ports(context: &Context, op: OpId, counter: Option<ValueId>) -> Option<Ports> {
    let deps = context.get_op(op).dep_results().len();
    let carried = context.get_op(op).as_interface::<dyn LoopLike>()?;
    let arguments = carried.carried_args();
    // A loop whose body opens a token scope holds a `break` or a `continue`, and
    // the copy would name a scope the rebuilt loop does not open.
    let block = crate::analysis::affine::body_block(context, op)?;
    if context.get_block(block).arguments().len() != arguments.len() {
        return None;
    }
    let mut states = Vec::new();
    let mut counter_port = None;
    for (port, &argument) in arguments.iter().enumerate() {
        if Some(argument) == counter {
            counter_port = Some(port);
        } else if port >= arguments.len() - deps {
            states.push(port);
        } else {
            return None;
        }
    }
    Some(Ports {
        arguments,
        counter: counter_port,
        states,
    })
}

/// Whether the level's chains cross the loop inside it untouched, in one order:
/// the argument each chain enters on is the inner loop's initial value for it,
/// and what the level yields for it is the inner loop's result.
fn chains_through(context: &Context, view: &AffineView, depth: usize, outer: &Ports) -> bool {
    let level = &view.loops[depth];
    let inner = view.loops[depth + 1].op;
    let Some(block) = crate::analysis::affine::body_block(context, level.op) else {
        return false;
    };
    let block = context.get_block(block);
    let terminator = *block.op_ids().last().expect("a body is terminated");
    let yielded = context.get_op(terminator).operands().to_vec();
    let Some(carried) = context.get_op(inner).as_interface::<dyn LoopLike>() else {
        return false;
    };
    let Some(ports) = ports(context, inner, view.loops[depth + 1].counter) else {
        return false;
    };
    // Nothing else may sit between the two: an op that merges chains would be
    // dropped by a rebuild that only threads them.
    let holds_only_the_inner_loop = block.op_ids().iter().all(|&op| {
        op == inner || op == terminator || crate::passes::is_pure_value(&context.get_op(op))
    });
    holds_only_the_inner_loop
        && ports.states.len() == outer.states.len()
        && outer.states.iter().zip(&ports.states).all(|(&out, &into)| {
            carried.inits()[into] == outer.arguments[out]
                && yielded[out] == context.get_op(inner).results()[into]
        })
}

/// One loop of the rebuilt nest, ready to be built: which dimension it counts,
/// the name its counter is left under for the levels below — a tiled dimension's
/// outer loop leaves a tile base, not the counter the body reads — and what it
/// counts between.
struct Counted {
    dimension: usize,
    key: usize,
    lower: ValueId,
    upper: ValueId,
    step: ValueId,
}

/// Where the next operation goes.
enum Site {
    Before(OperationRef),
    Append(BlockHandle),
}

pub(super) struct Lowering<'a> {
    context: &'a Context,
    nest: Nest,
    candidate: Candidate,
    /// Per dimension, the values the rebuilt loops count between.
    spelled: Vec<(ValueId, ValueId, ValueId)>,
    /// Per tiled dimension, the step its tile loop takes.
    tile_steps: HashMap<usize, ValueId>,
    /// Per dimension counted plainly, the loop that counts it.
    built: HashMap<usize, OpId>,
}

impl<'a> Lowering<'a> {
    pub(super) fn new(context: &'a Context, nest: Nest, candidate: Candidate) -> Self {
        Self {
            context,
            nest,
            candidate,
            spelled: Vec::new(),
            tile_steps: HashMap::new(),
            built: HashMap::new(),
        }
    }

    /// Replace the nest with the one the candidate names.
    pub(super) fn run(
        &mut self,
        rewriter: &mut Rewriter,
        view: &AffineView,
    ) -> Result<(), PassError> {
        let target = OperationRef::new(self.context.get_op(self.nest.root));
        let remainder = self
            .candidate
            .sole_tiled()
            .filter(|&d| !divides_evenly(view, d, self.candidate.tiles[d]));
        let mut shape = self.candidate.clone();
        if let Some(d) = remainder {
            shape.tiles[d] = 1;
        }
        self.spell_bounds(rewriter, &target, &shape)?;
        self.hoist(&target);

        let levels = levels(&shape);
        let mut site = Site::Before(target.clone());
        let states = self.nest.entry_states.clone();
        let left = self.emit(rewriter, &levels, 0, &mut HashMap::new(), states, &mut site)?;

        for (&old, &new) in self.nest.exit_states.iter().zip(&left) {
            self.context.replace_value_uses(old, new);
        }
        rewriter.erase_op(&target)?;
        if let Some(d) = remainder {
            strip_mine(
                self.context,
                rewriter,
                self.built[&d],
                self.candidate.tiles[d] as i128,
            )?;
        }
        Ok(())
    }

    /// Move the levels' loop-invariant operations out ahead of the nest, so what
    /// the copied body names is still defined once the nest is gone.
    fn hoist(&self, target: &OperationRef) {
        let mut site = Site::Before(target.clone());
        for op in self.nest.hoist.clone() {
            if let Some(block) = self.context.parent_block(op) {
                self.context.get_block(block).remove_op(op);
            }
            self.place(&mut site, op);
        }
    }

    /// Name every bound where the rebuilt nest will stand, so no loop counts
    /// between values the erased nest defined, and the step each tile loop
    /// takes: a whole tile of the dimension's own steps.
    fn spell_bounds(
        &mut self,
        rewriter: &mut Rewriter,
        target: &OperationRef,
        shape: &Candidate,
    ) -> Result<(), PassError> {
        for dimension in 0..self.nest.bounds.len() {
            let bounds = &self.nest.bounds[dimension];
            let (lower, upper, stride, ty) = (
                bounds.lower,
                bounds.upper,
                bounds.stride,
                bounds.counter_type,
            );
            let lower = self.spell(rewriter, target, lower, ty)?;
            let upper = self.spell(rewriter, target, upper, ty)?;
            let step = literal(self.context, rewriter, target, stride, ty)?;
            self.spelled.push((lower, upper, step));
            let tile = shape.tiles[dimension];
            if tile > 1 {
                let step = literal(self.context, rewriter, target, stride * tile as i128, ty)?;
                self.tile_steps.insert(dimension, step);
            }
        }
        Ok(())
    }

    fn spell(
        &self,
        rewriter: &mut Rewriter,
        target: &OperationRef,
        bound: Bound,
        ty: TypeId,
    ) -> Result<ValueId, PassError> {
        match bound {
            Bound::Literal(value) => literal(self.context, rewriter, target, value, ty),
            Bound::Value(value) => Ok(value),
        }
    }

    /// Build level `index` and everything under it, and hand back the chains it
    /// leaves behind.
    fn emit(
        &mut self,
        rewriter: &mut Rewriter,
        levels: &[Level],
        index: usize,
        bound: &mut HashMap<usize, ValueId>,
        states: Vec<ValueId>,
        site: &mut Site,
    ) -> Result<Vec<ValueId>, PassError> {
        let Some(level) = levels.get(index) else {
            return self.emit_body(rewriter, bound, states, site);
        };
        match *level {
            Level::Plain(dimension) => {
                let counted = self.plain(dimension);
                self.emit_loop(rewriter, levels, index, bound, states, site, counted)
            }
            Level::TileOuter(dimension) => {
                let tiles = self.whole_tiles(dimension);
                self.emit_loop(rewriter, levels, index, bound, states, site, tiles)
            }
            Level::TileInner(dimension) => {
                let tile = self.one_tile(dimension, bound, site);
                self.emit_loop(rewriter, levels, index, bound, states, site, tile)
            }
        }
    }

    /// The dimension as it was: from its own lower bound to its own upper one.
    fn plain(&self, dimension: usize) -> Counted {
        let (lower, upper, step) = self.spelled[dimension];
        Counted {
            dimension,
            key: dimension,
            lower,
            upper,
            step,
        }
    }

    /// The tiles of a tiled dimension: one iteration per tile.
    fn whole_tiles(&self, dimension: usize) -> Counted {
        let (lower, upper, _) = self.spelled[dimension];
        Counted {
            dimension,
            key: tile_base_key(dimension),
            lower,
            upper,
            step: self.tile_steps[&dimension],
        }
    }

    /// One tile of a tiled dimension: from where its outer loop stands to one
    /// tile further.
    fn one_tile(
        &self,
        dimension: usize,
        bound: &HashMap<usize, ValueId>,
        site: &mut Site,
    ) -> Counted {
        let base = bound[&tile_base_key(dimension)];
        let step = self.tile_steps[&dimension];
        let ty = self.nest.bounds[dimension].counter_type;
        let upper = b::addi(self.context, base, step, ty).build();
        self.place(site, upper.id());
        Counted {
            dimension,
            key: dimension,
            lower: base,
            upper: upper.result(),
            step: self.spelled[dimension].2,
        }
    }

    /// Build one loop and fill it with the levels below.
    #[allow(clippy::too_many_arguments)]
    fn emit_loop(
        &mut self,
        rewriter: &mut Rewriter,
        levels: &[Level],
        index: usize,
        bound: &mut HashMap<usize, ValueId>,
        states: Vec<ValueId>,
        site: &mut Site,
        counted: Counted,
    ) -> Result<Vec<ValueId>, PassError> {
        let Counted {
            dimension,
            key,
            lower,
            upper,
            step,
        } = counted;
        let ty = self.nest.bounds[dimension].counter_type;
        let counter = self.context.create_value(ty, None);
        let arguments: Vec<Value> = std::iter::once(counter.clone())
            .chain(
                states
                    .iter()
                    .map(|_| self.context.create_value(TypeId::DEPENDENCY, None)),
            )
            .collect();
        let inner_states: Vec<ValueId> = arguments[1..].iter().map(Value::id).collect();
        let block = self
            .context
            .create_block_with_dependencies(arguments, states.len());
        let region = self.context.create_region();
        region.add_block(block.id());

        let mut builder = scf::ForOpBuilder::new(self.context)
            .lower_bound(lower)
            .upper_bound(upper)
            .step(step)
            .inits(vec![lower])
            .result_types(vec![ty])
            .body(region.id());
        for &state in &states {
            builder = builder.dep_operand(state).dep_result();
        }
        let loop_op = builder.build();
        self.place(site, loop_op.id());
        if key == dimension {
            self.built.insert(dimension, loop_op.id());
        }

        let restored = bound.insert(key, counter.id());
        let mut inner = Site::Append(self.context.get_block(block.id()));
        let left = self.emit(rewriter, levels, index + 1, bound, inner_states, &mut inner)?;
        match restored {
            Some(previous) => bound.insert(key, previous),
            None => bound.remove(&key),
        };

        let latch = b::addi(self.context, counter.id(), step, ty).build();
        self.context.get_block(block.id()).append(latch.id());
        let mut terminator = scf::r#yield(self.context, vec![latch.result()]);
        for state in left {
            terminator = terminator.dep_operand(state);
        }
        let terminator = terminator.build();
        self.context.get_block(block.id()).append(terminator.id());

        let results = self.context.get_op(loop_op.id()).results().to_vec();
        Ok(results[1..].to_vec())
    }

    /// Copy the innermost body under the counters the rebuilt nest gives it.
    fn emit_body(
        &mut self,
        rewriter: &mut Rewriter,
        bound: &HashMap<usize, ValueId>,
        states: Vec<ValueId>,
        site: &mut Site,
    ) -> Result<Vec<ValueId>, PassError> {
        // Every counter the body reads is the loop that now counts its dimension,
        // and each chain the body enters on is the port handed down to it.
        let mut bindings: HashMap<ValueId, ValueId> = self
            .nest
            .counters
            .iter()
            .enumerate()
            .filter_map(|(dimension, counter)| Some(((*counter)?, bound[&dimension])))
            .collect();
        for (port, &argument) in self.nest.body_arguments.iter().enumerate() {
            let target = match self.nest.body_counter {
                Some(counter) if counter == port => bound[&(self.nest.counters.len() - 1)],
                _ => {
                    states[self
                        .nest
                        .body_states
                        .iter()
                        .position(|&s| s == port)
                        .expect("a chain port")]
                }
            };
            bindings.insert(argument, target);
        }
        let copy = crate::clone_region_with_mapping(self.context, self.nest.body, &bindings);
        let block = self
            .context
            .get_block(self.context.get_region(copy).block_ids()[0]);

        let terminator = *block.op_ids().last().expect("a body is terminated");
        let operands = self.context.get_op(terminator).operands().to_vec();
        let left: Vec<ValueId> = self
            .nest
            .body_states
            .iter()
            .map(|&port| operands[port])
            .collect();
        rewriter.erase_op(&OperationRef::new(self.context.get_op(terminator)))?;
        // The body stepped the counter for the loop it used to sit in; the loop
        // that now counts that dimension steps it, so the copy's latch is left
        // over. Dropping it here rather than leaving it to `dce` keeps what the
        // unroller measures the size of honest.
        if let Some(latch) = self
            .nest
            .body_counter
            .and_then(|port| self.context.get_value(operands[port]).defining_op())
            .filter(|&latch| block.op_ids().contains(&latch))
            .filter(|&latch| !names(self.context, &block, latch))
        {
            rewriter.erase_op(&OperationRef::new(self.context.get_op(latch)))?;
        }
        let Site::Append(destination) = site else {
            unreachable!("a body is built inside the loop that runs it");
        };
        rewriter.splice_block(block.id(), destination.id());
        rewriter.erase_block(block.id());
        Ok(left)
    }

    /// Put an operation where the site says, keeping the order calls arrive in.
    fn place(&self, site: &mut Site, op: OpId) {
        match site {
            Site::Before(target) => {
                let block = self.context.get_block(
                    target
                        .op()
                        .parent_block()
                        .expect("the nest sits in a block"),
                );
                let position = block
                    .op_ids()
                    .iter()
                    .position(|&other| other == target.op().id)
                    .expect("the nest sits in the block holding it");
                block.insert(position, op);
            }
            Site::Append(block) => block.append(op),
        }
    }
}

/// The key a tiled dimension's outer counter is kept under, apart from the
/// counter the body reads.
fn tile_base_key(dimension: usize) -> usize {
    usize::MAX - dimension
}

/// A literal spelled ahead of `target`.
pub(super) fn literal(
    context: &Context,
    rewriter: &mut Rewriter,
    target: &OperationRef,
    value: i128,
    ty: TypeId,
) -> Result<ValueId, PassError> {
    let op = b::constant(context, value as i64, ty).build();
    rewriter.insert_op_before(target, &op)?;
    Ok(op.result())
}

/// The operations the levels above the innermost body hold, in the order they
/// run. They are pure — `chains_through` admitted the level only if they were —
/// so moving them out of the nest is what makes the copied body's references to
/// them survive the rebuild. An operation naming something the nest defines is
/// not loop-invariant, and refuses the whole rebuild.
fn hoistable(context: &Context, view: &AffineView) -> Option<Vec<OpId>> {
    let mut hoist: Vec<OpId> = Vec::new();
    let mut spelled: Vec<ValueId> = Vec::new();
    for level in &view.loops[..view.loops.len() - 1] {
        let block = context.get_block(crate::analysis::affine::body_block(context, level.op)?);
        let ops = block.op_ids();
        // The level's own latch goes with the port it steps, which the rebuild
        // spells again; it is the one operation here that names a counter.
        let latch = latch_of(context, level.op, level.counter);
        for &op in &ops[..ops.len().saturating_sub(1)] {
            if Some(op) == latch {
                continue;
            }
            if context.get_op(op).regions().is_empty() {
                let operands = context.get_op(op).operands().to_vec();
                if !operands
                    .iter()
                    .all(|value| spelled.contains(value) || outside(context, view.root, *value))
                {
                    return None;
                }
                spelled.extend(context.get_op(op).results().iter().copied());
                hoist.push(op);
            }
        }
    }
    Some(hoist)
}

/// Whether anything left in `block` reads what `op` defines.
fn names(context: &Context, block: &BlockHandle, op: OpId) -> bool {
    let results = context.get_op(op).results().to_vec();
    block
        .op_ids()
        .iter()
        .filter(|&&other| other != op)
        .any(|&other| {
            context
                .get_op(other)
                .operands()
                .iter()
                .any(|operand| results.contains(operand))
        })
}

/// The operation stepping a loop's counter.
fn latch_of(context: &Context, op: OpId, counter: Option<ValueId>) -> Option<OpId> {
    let carried = context.get_op(op).as_interface::<dyn LoopLike>()?;
    let port = carried
        .carried_args()
        .iter()
        .position(|&argument| Some(argument) == counter)?;
    context.get_value(carried.latched()[port]).defining_op()
}

/// Whether `value` is defined outside the nest rooted at `root`.
fn outside(context: &Context, root: OpId, value: ValueId) -> bool {
    let mut op = match context.block_of_argument(value) {
        Some(block) => context
            .parent_region(block)
            .and_then(|region| context.get_region(region).parent_op()),
        None => context.get_value(value).defining_op(),
    };
    while let Some(current) = op {
        if current == root {
            return false;
        }
        op = context.parent_op(current);
    }
    true
}

/// Whether anything beside the nest itself names `value`.
fn is_used(context: &Context, root: OpId, value: ValueId) -> bool {
    context.users_of(value).into_iter().any(|user| user != root)
}
