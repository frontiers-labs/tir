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

use crate::analysis::affine::{AffineForm, AffineView, body_ops, carried};
use crate::attributes::Predicate;
use crate::builtin::{IntegerType, ops as b};
use crate::{
    Context, CountedLoop, OpId, Operation, OperationRef, PassError, RegionId, Rewriter, Theta,
    TypeId, Value, ValueId, scf,
};

use super::schedule::{Candidate, Level, divides_evenly, levels};
use super::strip_mine::strip_mine;

/// A counted nest in the shape the rebuild can state again.
pub(super) struct Nest {
    root: OpId,
    /// The unordered region the nest stands in, which the rebuilt one joins.
    region: RegionId,
    /// The bounds of each dimension, as values available where the nest stands.
    bounds: Vec<Bounds>,
    /// The ports each dimension's body reads its counter through.
    counters: Vec<Vec<ValueId>>,
    /// The innermost body: the region cloned into every copy, its arguments and
    /// what it yields, split into the ports counting with the loop and the
    /// chains.
    body: RegionId,
    body_arguments: Vec<ValueId>,
    body_counters: Vec<usize>,
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
        body_ops(context, innermost.op)?;

        let shape = carried(context, &root)?;
        let root_ports = ports(context, view.root, view.loops[0].counter)?;
        // The counter the nest leaves behind is not a counter of the rebuilt
        // nest, so nothing may be reading it.
        if root_ports
            .counters
            .iter()
            .any(|&port| is_used(context, view.root, shape.finals[port]))
        {
            return None;
        }

        Some(Self {
            root: view.root,
            region: context.parent_nodes_region(view.root)?,
            hoist: hoistable(context, view)?,
            bounds,
            counters: view
                .loops
                .iter()
                .map(|level| {
                    level
                        .counter
                        .into_iter()
                        .chain(level.counter_aliases.iter().copied())
                        .collect()
                })
                .collect(),
            body,
            body_arguments: inner_ports.arguments.clone(),
            body_counters: inner_ports.counters.clone(),
            body_states: inner_ports.states.clone(),
            entry_states: root_ports
                .states
                .iter()
                .map(|&port| shape.inits[port])
                .collect(),
            exit_states: root_ports
                .states
                .iter()
                .map(|&port| shape.finals[port])
                .collect(),
        })
    }
}

/// How a loop's carried ports divide: the ports counting with the loop, and
/// the memory chains. A port that is neither is a recurrence the rebuild would
/// have to carry through a new loop order, which v1 does not do.
struct Ports {
    arguments: Vec<ValueId>,
    counters: Vec<usize>,
    states: Vec<usize>,
}

fn ports(context: &Context, op: OpId, counter: Option<ValueId>) -> Option<Ports> {
    let handle = context.get_op(op);
    let arguments = carried(context, &handle)?.args;
    let counting = crate::analysis::affine::build::counter_ports(context, &handle);
    let mut states = Vec::new();
    let mut counters = Vec::new();
    for (port, &argument) in arguments.iter().enumerate() {
        if Some(argument) == counter || counting.contains(&argument) {
            counters.push(port);
        } else if context.get_value(argument).is_state() {
            states.push(port);
        } else {
            return None;
        }
    }
    Some(Ports {
        arguments,
        counters,
        states,
    })
}

/// Whether the level's chains cross the loop inside it untouched, in one order:
/// the argument each chain enters on is the inner loop's initial value for it,
/// and what the level yields for it is the inner loop's result.
fn chains_through(context: &Context, view: &AffineView, depth: usize, outer: &Ports) -> bool {
    let level = &view.loops[depth];
    let inner = view.loops[depth + 1].op;
    let (Some(ops), Some(outer_shape)) = (
        body_ops(context, level.op),
        carried(context, &context.get_op(level.op)),
    ) else {
        return false;
    };
    let yielded = outer_shape.latched;
    let Some(inner_shape) = carried(context, &context.get_op(inner)) else {
        return false;
    };
    let Some(ports) = ports(context, inner, view.loops[depth + 1].counter) else {
        return false;
    };
    // Nothing else may sit between the two: an op that merges chains would be
    // dropped by a rebuild that only threads them.
    let holds_only_the_inner_loop = ops
        .iter()
        .all(|&op| op == inner || crate::passes::is_pure_value(&context.get_op(op)));
    holds_only_the_inner_loop
        && ports.states.len() == outer.states.len()
        && outer.states.iter().zip(&ports.states).all(|(&out, &into)| {
            inner_shape.inits[into] == outer.arguments[out]
                && yielded[out] == inner_shape.finals[into]
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
        let site = self.nest.region;
        self.spell_bounds(site, &shape);
        self.hoist(site);

        let levels = levels(&shape);
        let states = self.nest.entry_states.clone();
        let left = self.emit(rewriter, &levels, 0, &mut HashMap::new(), states, site)?;

        for (&old, &new) in self.nest.exit_states.iter().zip(&left) {
            self.context.replace_value_uses(old, new);
            self.context.rename_region_results(site, old, new, &[]);
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
    fn hoist(&self, site: RegionId) {
        for op in self.nest.hoist.clone() {
            if let Some(block) = self.context.parent_block(op) {
                self.context.get_block(block).remove_op(op);
            } else if let Some(region) = self.context.parent_nodes_region(op) {
                self.context.remove_from_region(region, op);
            }
            self.context.add(site, op);
        }
    }

    /// Name every bound where the rebuilt nest will stand, so no loop counts
    /// between values the erased nest defined, and the step each tile loop
    /// takes: a whole tile of the dimension's own steps.
    fn spell_bounds(&mut self, site: RegionId, shape: &Candidate) {
        for dimension in 0..self.nest.bounds.len() {
            let bounds = &self.nest.bounds[dimension];
            let (lower, upper, stride, ty) = (
                bounds.lower,
                bounds.upper,
                bounds.stride,
                bounds.counter_type,
            );
            let lower = self.spell(site, lower, ty);
            let upper = self.spell(site, upper, ty);
            let step = literal_at(self.context, site, stride, ty);
            self.spelled.push((lower, upper, step));
            let tile = shape.tiles[dimension];
            if tile > 1 {
                let step = literal_at(self.context, site, stride * tile as i128, ty);
                self.tile_steps.insert(dimension, step);
            }
        }
    }

    fn spell(&self, site: RegionId, bound: Bound, ty: TypeId) -> ValueId {
        match bound {
            Bound::Literal(value) => literal_at(self.context, site, value, ty),
            Bound::Value(value) => value,
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
        site: RegionId,
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
        site: RegionId,
    ) -> Counted {
        let base = bound[&tile_base_key(dimension)];
        let step = self.tile_steps[&dimension];
        let ty = self.nest.bounds[dimension].counter_type;
        let upper = b::addi(self.context, base, step, ty).build();
        self.context.add(site, upper.id());
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
    /// One counted loop: a `scf.for` whose body is a fresh graph holding the
    /// counter, the chains, and the levels below.
    #[allow(clippy::too_many_arguments)]
    fn emit_loop(
        &mut self,
        rewriter: &mut Rewriter,
        levels: &[Level],
        index: usize,
        bound: &mut HashMap<usize, ValueId>,
        states: Vec<ValueId>,
        site: RegionId,
        counted: Counted,
    ) -> Result<Vec<ValueId>, PassError> {
        let Counted {
            dimension,
            key,
            lower,
            upper,
            step,
        } = counted;
        let context = self.context;
        let ty = self.nest.bounds[dimension].counter_type;
        let counter = context.create_value(ty, None);
        let mut ports = vec![counter.clone()];
        ports.extend(
            states
                .iter()
                .map(|_| context.create_value(TypeId::STATE, None)),
        );
        let dep_ports: Vec<ValueId> = ports[1..].iter().map(Value::id).collect();
        let body = context.create_nodes_region(ports, vec![], vec![]).id();
        let mut result_types = vec![ty];
        result_types.extend(states.iter().map(|_| TypeId::STATE));
        let loop_op = scf::ForOpBuilder::new(context)
            .lb(lower)
            .inits(states)
            .ub(upper)
            .step(step)
            .body(body)
            .result_types(result_types)
            .build();
        self.context.add(site, loop_op.id());
        if key == dimension {
            self.built.insert(dimension, loop_op.id());
        }

        let restored = bound.insert(key, counter.id());
        let left = self.emit(rewriter, levels, index + 1, bound, dep_ports.clone(), body)?;
        match restored {
            Some(previous) => bound.insert(key, previous),
            None => bound.remove(&key),
        };

        let boolean = IntegerType::new(context, 1);
        let compare = b::cmpi(context, counter.id(), upper, Predicate::Slt, boolean).build();
        context.add(body, compare.id());
        let advance = b::addi(context, counter.id(), step, ty).build();
        context.add(body, advance.id());
        let mut results = vec![compare.result(), advance.result()];
        results.extend(left);
        results.push(counter.id());
        results.extend(dep_ports);
        context.set_region_results(body, results);
        Ok(context.get_op(loop_op.id()).state_results().to_vec())
    }

    /// The bindings a copy of the innermost body reads its ports through: every
    /// counter the body reads is the loop that now counts its dimension, and
    /// each chain the body enters on is the port handed down to it.
    fn body_bindings(
        &self,
        bound: &HashMap<usize, ValueId>,
        states: &[ValueId],
    ) -> HashMap<ValueId, ValueId> {
        let mut bindings: HashMap<ValueId, ValueId> = self
            .nest
            .counters
            .iter()
            .enumerate()
            .flat_map(|(dimension, counters)| {
                counters
                    .iter()
                    .map(move |&counter| (counter, bound[&dimension]))
            })
            .collect();
        for (port, &argument) in self.nest.body_arguments.iter().enumerate() {
            let target = if self.nest.body_counters.contains(&port) {
                bound[&(self.nest.counters.len() - 1)]
            } else {
                states[self
                    .nest
                    .body_states
                    .iter()
                    .position(|&s| s == port)
                    .expect("a chain port")]
            };
            bindings.insert(argument, target);
        }
        bindings
    }

    /// Copy the innermost body under the counters the rebuilt nest gives it.
    fn emit_body(
        &mut self,
        rewriter: &mut Rewriter,
        bound: &HashMap<usize, ValueId>,
        states: Vec<ValueId>,
        site: RegionId,
    ) -> Result<Vec<ValueId>, PassError> {
        let bindings = self.body_bindings(bound, &states);
        let (ops, results) =
            crate::clone::clone_nodes_ops_into(self.context, self.nest.body, &bindings, site);
        // The chains the copy carries on are the states its loop named among
        // what the next iteration takes.
        let body = self.context.get_region(self.nest.body);
        let owner = self.context.get_op(body.parent_op().expect("a loop body"));
        let binding = crate::binding::declared(&owner).expect("a loop");
        let original = body.results();
        let left: Vec<ValueId> = binding
            .continue_
            .filter(|&index| self.context.get_value(original[index]).is_state())
            .map(|index| results[index])
            .collect();
        // The copy's own comparison and latch count a loop that is gone.
        erase_unread(self.context, rewriter, &ops)?;
        Ok(left)
    }
}

/// Erase the pure copies nothing reads: the comparison and the latch a copied
/// body computed for the loop it used to sit in.
pub(super) fn erase_unread(
    context: &Context,
    rewriter: &mut Rewriter,
    ops: &[OpId],
) -> Result<(), PassError> {
    // A region's results sit in no use list, so a value one names is read
    // without a user: an unordered region's results, and those of every
    // region nested in it, count as reads.
    let mut named: std::collections::HashSet<ValueId> = std::collections::HashSet::new();
    let mut seen: std::collections::HashSet<crate::RegionId> = std::collections::HashSet::new();
    for &op in ops {
        let Some(region) = context.parent_nodes_region(op) else {
            continue;
        };
        if seen.insert(region) {
            for nested in context.nested_regions(region) {
                named.extend(context.get_region(nested).results());
            }
        }
    }
    for &op in ops.iter().rev() {
        let instance = context.get_op(op);
        if instance.regions().is_empty()
            && instance.state_results().is_empty()
            && !instance.results().is_empty()
            && crate::passes::is_pure_value(&instance)
            && instance
                .results()
                .iter()
                .all(|&result| !context.is_used(result) && !named.contains(&result))
        {
            rewriter.erase_op(&OperationRef::new(instance))?;
        }
    }
    Ok(())
}

/// A literal spelled in `site`.
pub(super) fn literal_at(context: &Context, site: RegionId, value: i128, ty: TypeId) -> ValueId {
    let op = b::constant(context, value as i64, ty).build();
    context.add(site, op.id());
    op.result()
}

/// The key a tiled dimension's outer counter is kept under, apart from the
/// counter the body reads.
fn tile_base_key(dimension: usize) -> usize {
    usize::MAX - dimension
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
        let ops = body_ops(context, level.op)?;
        // The level's own latches go with the ports they step, which the
        // rebuild spells again, and so does the comparison a declared counted
        // loop pins; those are the operations here that name a counter.
        let counted = counted_shape(context, level.op);
        for &op in &ops {
            if counted.contains(&op) {
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

/// The operations a loop's shape pins: the latch of every port counting with
/// it, and the predicate a declared counted loop states.
fn counted_shape(context: &Context, op: OpId) -> Vec<OpId> {
    let handle = context.get_op(op);
    let mut ops = Vec::new();
    if let Some(theta) = handle.clone().as_interface::<dyn Theta>() {
        ops.extend(context.get_value(theta.predicate()).defining_op());
    }
    let counting = crate::analysis::affine::build::counter_ports(context, &handle);
    if let Some(shape) = carried(context, &handle) {
        for (port, argument) in shape.args.iter().enumerate() {
            if counting.contains(argument) {
                ops.extend(context.get_value(shape.latched[port]).defining_op());
            }
        }
    }
    ops
}

/// Whether `value` is defined outside the nest rooted at `root`.
fn outside(context: &Context, root: OpId, value: ValueId) -> bool {
    let mut op = match (
        context.block_of_argument(value),
        context.region_of_port(value),
    ) {
        (Some(block), _) => context
            .parent_region(block)
            .and_then(|region| context.get_region(region).parent_op()),
        (None, Some(region)) => context.get_region(region).parent_op(),
        (None, None) => context.get_value(value).defining_op(),
    };
    while let Some(current) = op {
        if current == root {
            return false;
        }
        op = context.parent_op(current);
    }
    true
}

/// Whether anything beside the nest itself names `value`: an operation, or
/// the result list of the region holding the nest, which no use list records.
fn is_used(context: &Context, root: OpId, value: ValueId) -> bool {
    context.users_of(value).into_iter().any(|user| user != root)
        || context
            .parent_nodes_region(root)
            .is_some_and(|region| context.get_region(region).results().contains(&value))
}
