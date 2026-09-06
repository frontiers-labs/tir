//! Reading a counted nest into an [`AffineView`].

use super::*;

pub(super) fn build(context: &Context, root: OpId) -> Option<AffineView> {
    let nest = counted_nest(context, root)?;
    let mut builder = Builder::new(context, nest);
    builder.domain();
    builder.body();
    builder.ports();
    Some(builder.finish())
}

pub(super) struct Builder<'a> {
    pub(super) context: &'a Context,
    pub(super) layout: Option<DataLayout>,
    pub(super) nest: Vec<OpId>,
    /// Every value the nest defines, so a value outside it is a parameter.
    pub(super) interior: HashSet<ValueId>,
    pub(super) loops: Vec<Loop>,
    /// The iterations each depth runs, for the wrap check.
    pub(super) iteration_ranges: Vec<(i128, i128)>,
    /// Which depth's counter a carried port names.
    pub(super) counters: HashMap<ValueId, usize>,
    pub(super) symbols: Vec<ValueId>,
    pub(super) symbol_ranges: Vec<(i128, i128)>,
    pub(super) accesses: Vec<Access>,
    pub(super) ports: Vec<Port>,
    pub(super) opaque: bool,
}

impl<'a> Builder<'a> {
    fn new(context: &'a Context, nest: Vec<OpId>) -> Self {
        Self {
            layout: DataLayout::for_op(context, nest[0]),
            interior: interior_values(context, nest[0]),
            context,
            nest,
            loops: Vec::new(),
            iteration_ranges: Vec::new(),
            counters: HashMap::new(),
            symbols: Vec::new(),
            symbol_ranges: Vec::new(),
            accesses: Vec::new(),
            ports: Vec::new(),
            opaque: false,
        }
    }

    fn finish(self) -> AffineView {
        let pairs = self.pairs();
        AffineView {
            root: self.nest[0],
            loops: self.loops,
            symbols: self.symbols,
            accesses: self.accesses,
            pairs,
            ports: self.ports,
            opaque: self.opaque,
        }
    }

    /// The bounds of every loop, outermost first. A bound is read where the loop
    /// stands, so it names only counters outside it — which are already read by
    /// the time it is.
    fn domain(&mut self) {
        for depth in 0..self.nest.len() {
            let op = self.context.get_op(self.nest[depth]);
            let counted = op
                .clone()
                .as_interface::<dyn CountedLoop>()
                .expect("a counted nest holds counted loops");
            let width = self.width_of(counted.lower_bound());
            let lower = self.bound(counted.lower_bound());
            let upper = self.bound(counted.upper_bound());
            let step = self.bound(counted.step());
            let trip = trip_count(&lower, &upper, &step);
            self.iteration_ranges.push(iteration_range(trip, width));
            let mut counting = counter_ports(self.context, &op);
            for &port in &counting {
                self.counters.insert(port, depth);
            }
            let counter = (!counting.is_empty()).then(|| counting.remove(0));
            self.loops.push(Loop {
                op: self.nest[depth],
                lower,
                upper,
                step,
                width,
                counter,
                counter_aliases: counting,
                trip,
            });
        }
    }

    /// A bound reads as an affine form or as a parameter of its own: a loop whose
    /// upper bound is an expression the walk cannot follow still counts, it just
    /// counts to something the view can only name.
    fn bound(&mut self, value: ValueId) -> AffineForm {
        match self.form(value) {
            Some((form, false)) => form,
            _ => AffineForm::symbol(self.symbol(value)),
        }
    }

    /// Every access of the innermost body, and whatever there stops the view from
    /// describing it.
    fn body(&mut self) {
        let Some(ops) = body_ops(self.context, *self.nest.last().expect("a rooted nest")) else {
            self.opaque = true;
            return;
        };
        self.scan(&ops, false);
    }

    fn scan(&mut self, ops: &[OpId], guarded: bool) {
        for &op_id in ops {
            let op = self.context.get_op(op_id);
            if let Some(read) = op.clone().as_interface::<dyn MemoryRead>() {
                let access = self.access(
                    op_id,
                    false,
                    read.read_location(),
                    read.state_operand(),
                    self.context.get_value(read.read_value()).ty(),
                    guarded,
                );
                self.accesses.push(access);
            } else if let Some(write) = op.clone().as_interface::<dyn MemoryWrite>() {
                let access = self.access(
                    op_id,
                    true,
                    write.write_location(),
                    write.state_operand(),
                    self.context.get_value(write.written_value()).ty(),
                    guarded,
                );
                self.accesses.push(access);
            } else if op.clone().as_op::<scf::IfOp>().is_some() || op.has_interface::<dyn Gamma>() {
                for region in op.regions().to_vec() {
                    let region = self.context.get_region(region);
                    if region.is_nodes() {
                        self.scan(&region.op_ids(), true);
                    } else {
                        for block in region.block_ids() {
                            let mut ops = self.context.get_block(block).op_ids();
                            ops.pop();
                            self.scan(&ops, true);
                        }
                    }
                }
            } else if !op.regions().is_empty() || self.touches_memory(&op) {
                self.opaque = true;
            }
        }
    }

    /// Whether an operation names a memory state without saying what it does to
    /// the memory: a call, a copy, an export.
    fn touches_memory(&self, op: &OpHandle) -> bool {
        if op.is::<JoinOp>() {
            return false;
        }
        if op.is::<SplitOp>() {
            return true;
        }
        !op.dep_operands().is_empty()
    }

    fn access(
        &mut self,
        op: OpId,
        write: bool,
        address: ValueId,
        state: Option<ValueId>,
        ty: TypeId,
        guarded: bool,
    ) -> Access {
        let (base, offset) = self.address(address);
        let chain = state.and_then(|state| chain_root(self.context, state));
        if chain.is_none() {
            self.opaque = true;
        }
        let extent = self
            .layout
            .as_ref()
            .and_then(|layout| layout.size_in_bits(self.context, ty))
            .filter(|bits| bits % 8 == 0)
            .map(|bits| u64::from(bits / 8));
        if extent.is_none() {
            self.opaque = true;
        }
        Access {
            op,
            write,
            chain: chain.unwrap_or(base),
            base,
            offset: match &offset {
                Some((form, _)) => Offset::Affine(form.clone()),
                None => Offset::NonAffine,
            },
            extent: extent.unwrap_or(1),
            guarded,
            wrapping: offset.is_some_and(|(_, wrapping)| wrapping),
        }
    }

    /// The object an address is derived from and how far into it the access
    /// lands: `ptradd` walks back to the object, and every step it added is a
    /// form of its own.
    fn address(&mut self, address: ValueId) -> (ValueId, Option<(AffineForm, bool)>) {
        let mut steps = Vec::new();
        let mut base = address;
        while let Some(op) = self.definition(base).filter(|op| op.is::<PtrAddOp>()) {
            steps.push(op.operands()[1]);
            base = op.operands()[0];
        }
        if self.interior.contains(&base) {
            return (base, None);
        }
        let mut offset = AffineForm::default();
        let mut wrapping = false;
        for step in steps {
            let Some((form, step_wraps)) = self.form(step) else {
                return (base, None);
            };
            offset = offset.add(&form);
            wrapping |= step_wraps;
        }
        (base, Some((offset, wrapping)))
    }

    /// `value` as an affine form, and whether the arithmetic reaching it could
    /// leave the region its source width holds.
    fn form(&mut self, value: ValueId) -> Option<(AffineForm, bool)> {
        if let Some(&depth) = self.counters.get(&value) {
            return self.iteration(depth).map(|form| (form, false));
        }
        // A literal names its own value wherever it is spelled; anything else
        // the nest was entered with is a parameter, whatever computes it.
        if let Some(literal) = self.integer(value) {
            return Some((AffineForm::constant(literal), false));
        }
        if !self.interior.contains(&value) {
            return Some((AffineForm::symbol(self.symbol(value)), false));
        }
        let op = self.definition(value)?;
        let (form, mut wrapping) = self.combine(&op)?;
        wrapping |= self.escapes_width(&form, value);
        Some((form, wrapping))
    }

    /// Depth `depth`'s counter on iteration `i`: `lower + step·i`. The recurrence
    /// holds only while the counter stays in its width, so a loop whose last
    /// value leaves it — or whose step is not a positive literal, so that value
    /// cannot be told — has no form for its counter.
    fn iteration(&self, depth: usize) -> Option<AffineForm> {
        let level = &self.loops[depth];
        let step = level.step.as_constant().filter(|&step| step > 0)?;
        match level.trip {
            Some(trip) => {
                let last = level.lower.as_constant()? + step * trip;
                AffineForm::fits(level.width, last, last).then_some(())?;
            }
            None if step == 1 => {}
            None => return None,
        }
        Some(level.lower.add(&AffineForm::counter(depth).scale(step)))
    }

    /// The last iteration of depth `depth`, as a form: a literal where the trip
    /// count is one, and `upper - lower - 1` where a unit step makes the count
    /// the span itself.
    pub(super) fn last_iteration(&self, depth: usize) -> Option<AffineForm> {
        let level = &self.loops[depth];
        match level.trip {
            Some(trip) => Some(AffineForm::constant(trip - 1)),
            None if level.step.as_constant() == Some(1) => {
                Some(level.upper.sub(&level.lower).sub(&AffineForm::constant(1)))
            }
            None => None,
        }
    }

    /// The form an operation builds out of its operands'.
    fn combine(&mut self, op: &OpHandle) -> Option<(AffineForm, bool)> {
        if let Some(literal) = op.clone().as_interface::<dyn ConstantLike>() {
            return Some((
                AffineForm::constant(i128::from(literal.constant_value().to_i64())),
                false,
            ));
        }
        let operands = op.operands().to_vec();
        if op.is::<ExtSIOp>() || op.is::<TruncIOp>() {
            return self.form(operands[0]);
        }
        if op.is::<AddIOp>() || op.is::<SubIOp>() {
            let (left, left_wraps) = self.form(operands[0])?;
            let (right, right_wraps) = self.form(operands[1])?;
            let form = if op.is::<AddIOp>() {
                left.add(&right)
            } else {
                left.sub(&right)
            };
            return Some((form, left_wraps || right_wraps));
        }
        if op.is::<MulIOp>() || op.is::<ShlIOp>() {
            let scale = |factor: i128| {
                if op.is::<ShlIOp>() {
                    (0..127).contains(&factor).then(|| 1i128 << factor)
                } else {
                    Some(factor)
                }
            };
            // A shift names its amount second; a product may name its constant
            // either way round.
            let sides: &[(usize, usize)] = if op.is::<ShlIOp>() {
                &[(0, 1)]
            } else {
                &[(0, 1), (1, 0)]
            };
            for &(variable, literal) in sides {
                let Some(factor) = self.integer(operands[literal]).and_then(&scale) else {
                    continue;
                };
                let (form, wraps) = self.form(operands[variable])?;
                return Some((form.checked_scale(factor)?, wraps));
            }
        }
        None
    }

    /// Whether a form can name something the width it is computed in cannot hold.
    fn escapes_width(&self, form: &AffineForm, value: ValueId) -> bool {
        let width = self.width_of(value);
        let (low, high) = form.range(&self.iteration_ranges, &self.symbol_ranges);
        !AffineForm::fits(width, low, high)
    }

    /// The index of a value the nest was entered with, registering it the first
    /// time it is met.
    fn symbol(&mut self, value: ValueId) -> usize {
        if let Some(index) = self.symbols.iter().position(|&other| other == value) {
            return index;
        }
        self.symbols.push(value);
        self.symbol_ranges
            .push(width_range(self.source_width(value)));
        self.symbols.len() - 1
    }

    /// The width a value was computed in before it was sign-extended, which is
    /// the range it can actually take.
    fn source_width(&self, mut value: ValueId) -> u32 {
        while let Some(op) = self.definition(value).filter(|op| op.is::<ExtSIOp>()) {
            value = op.operands()[0];
        }
        self.width_of(value)
    }

    /// What the innermost loop's carried ports do across iterations.
    fn ports(&mut self) {
        let op = self
            .context
            .get_op(*self.nest.last().expect("a rooted nest"));
        let Some(carried) = carried(self.context, &op) else {
            return;
        };
        let (args, latched, inits) = (carried.args, carried.latched, carried.inits);
        let value_ports = args.len() - op.dep_results().len().min(args.len());
        for port in 0..value_ports {
            self.ports.push(Port {
                arg: args[port],
                recurrence: self.recurrence(args[port], latched[port], inits[port]),
            });
        }
    }

    fn recurrence(&self, arg: ValueId, latched: ValueId, init: ValueId) -> Recurrence {
        let Some(op) = self.definition(latched) else {
            return Recurrence::Other;
        };
        let operands = op.operands().to_vec();
        if operands.len() != 2 {
            return Recurrence::Other;
        }
        let other = match (operands[0] == arg, operands[1] == arg) {
            (true, _) => operands[1],
            (_, true) => operands[0],
            _ => return Recurrence::Other,
        };
        if op.is::<AddIOp>()
            && let Some(step) = self.integer(other)
        {
            return Recurrence::Induction { init, step };
        }
        match reduction_of(&op) {
            // The accumulated value must not be the port itself: `p + p` doubles
            // it every iteration, which no reduction operator states.
            Some(reduction) if !self.reaches(other, arg) => Recurrence::Reduction(reduction),
            _ => Recurrence::Other,
        }
    }

    /// Whether `value` is computed from `port`, counting what the regions of an
    /// operation on the way read: a gate threads nothing through its operands.
    fn reaches(&self, value: ValueId, port: ValueId) -> bool {
        let mut pending = vec![value];
        let mut seen = HashSet::new();
        while let Some(current) = pending.pop() {
            if current == port {
                return true;
            }
            if !seen.insert(current) {
                continue;
            }
            let Some(op) = self.definition(current) else {
                continue;
            };
            pending.extend(op.operands().iter().copied());
            for region in crate::passes::regions_under(self.context, op.id) {
                for inner in self.context.get_region(region).op_ids() {
                    pending.extend(self.context.get_op(inner).operands().iter().copied());
                }
            }
        }
        false
    }

    fn integer(&self, value: ValueId) -> Option<i128> {
        integer(self.context, value)
    }

    fn definition(&self, value: ValueId) -> Option<OpHandle> {
        definition(self.context, value)
    }

    fn width_of(&self, value: ValueId) -> u32 {
        let ty = self.context.get_value(value).ty();
        integer_width(self.context, ty).unwrap_or(64)
    }
}

/// The carried port a loop's body reads its counter through: the one that
/// starts at the lower bound and gains the step every iteration, which is the
/// recurrence `CountedLoop` states and raising establishes.
pub(crate) fn counter_port(context: &Context, op: &OpHandle) -> Option<ValueId> {
    counter_ports(context, op).first().copied()
}

/// Every carried port counting with the loop: the declared induction first,
/// then each port entered on the lower bound and stepped by the step.
pub(crate) fn counter_ports(context: &Context, op: &OpHandle) -> Vec<ValueId> {
    let (Some(counted), Some(carried)) = (
        op.clone().as_interface::<dyn CountedLoop>(),
        carried(context, op),
    ) else {
        return Vec::new();
    };
    let (args, inits, latched) = (carried.args, carried.inits, carried.latched);
    let mut ports: Vec<ValueId> = counted
        .induction()
        .and_then(|induction| args.get(induction).copied())
        .into_iter()
        .collect();
    for port in 0..args.len() {
        if inits[port] == counted.lower_bound()
            && gains(context, latched[port], args[port], counted.step())
            && !ports.contains(&args[port])
        {
            ports.push(args[port]);
        }
    }
    ports
}

/// Whether `latched` is `carried + step`, either way round.
fn gains(context: &Context, latched: ValueId, carried: ValueId, step: ValueId) -> bool {
    let Some(op) = definition(context, latched).filter(|op| op.is::<AddIOp>()) else {
        return false;
    };
    let operands = op.operands().to_vec();
    let gained = match (operands[0] == carried, operands[1] == carried) {
        (true, _) => operands[1],
        (_, true) => operands[0],
        _ => return false,
    };
    gained == step
        || integer(context, gained).is_some() && integer(context, gained) == integer(context, step)
}

fn integer(context: &Context, value: ValueId) -> Option<i128> {
    let literal = definition(context, value)?.as_interface::<dyn ConstantLike>()?;
    Some(i128::from(literal.constant_value().to_i64()))
}

fn definition(context: &Context, value: ValueId) -> Option<OpHandle> {
    context
        .get_value(value)
        .defining_op()
        .map(|op| context.get_op(op))
}

/// The width of an integer type, or `None` for anything else.
fn integer_width(context: &Context, ty: TypeId) -> Option<u32> {
    (context.get_type_data(ty).as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .map(IntegerType::width)
}

fn width_range(width: u32) -> (i128, i128) {
    if width == 0 || width > 127 {
        return (i128::MIN / 4, i128::MAX / 4);
    }
    let bound = 1i128 << (width - 1);
    (-bound, bound - 1)
}

/// The iterations a depth runs: the trip count where the bounds spell one, and
/// otherwise as many as a counter of that width can be stepped through.
fn iteration_range(trip: Option<i128>, width: u32) -> (i128, i128) {
    match trip {
        Some(trip) => (0, (trip - 1).max(0)),
        None if width == 0 || width > 126 => (0, i128::MAX / 4),
        None => (0, (1i128 << width) - 1),
    }
}

fn trip_count(lower: &AffineForm, upper: &AffineForm, step: &AffineForm) -> Option<i128> {
    let (lower, upper, step) = (
        lower.as_constant()?,
        upper.as_constant()?,
        step.as_constant()?,
    );
    if step <= 0 {
        return None;
    }
    if upper <= lower {
        return Some(0);
    }
    Some((upper - lower).div_euclid(step) + i128::from((upper - lower).rem_euclid(step) != 0))
}

/// The operator a carried port accumulates under, of those the vocabulary
/// spells: `min` and `max` are not operations here, so no port reduces by them.
fn reduction_of(op: &OpHandle) -> Option<Reduction> {
    [
        (op.is::<AddIOp>(), Reduction::Add),
        (op.is::<MulIOp>(), Reduction::Mul),
        (op.is::<AndIOp>(), Reduction::And),
        (op.is::<OrIOp>(), Reduction::Or),
        (op.is::<XOrIOp>(), Reduction::Xor),
    ]
    .into_iter()
    .find_map(|(matched, reduction)| matched.then_some(reduction))
}

/// Every value defined inside the nest.
fn interior_values(context: &Context, root: OpId) -> HashSet<ValueId> {
    let mut values = HashSet::new();
    let mut pending = vec![root];
    while let Some(op_id) = pending.pop() {
        for region in context.get_op(op_id).regions().to_vec() {
            let region = context.get_region(region);
            values.extend(region.ports().iter().map(crate::Value::id));
            for op in region.op_ids() {
                values.extend(context.get_op(op).results().iter().copied());
                pending.push(op);
            }
        }
    }
    values
}

/// The state a chain starts at: the memory the function was entered with, or the
/// allocation that opened it. Two accesses are of one memory exactly when the
/// walk back through the ports, forks and merges reaches the same state.
pub(crate) fn chain_root(context: &Context, state: ValueId) -> Option<ValueId> {
    chain_root_memo(context, state, &mut HashMap::new())
}

/// [`chain_root`] remembering every state's root: a chain forks and merges
/// again at every gate, so the walk without memory is exponential in the
/// gates it crosses.
fn chain_root_memo(
    context: &Context,
    state: ValueId,
    memo: &mut HashMap<ValueId, Option<ValueId>>,
) -> Option<ValueId> {
    if let Some(&root) = memo.get(&state) {
        return root;
    }
    let root = chain_root_walk(context, state, memo);
    memo.insert(state, root);
    root
}

fn chain_root_walk(
    context: &Context,
    state: ValueId,
    memo: &mut HashMap<ValueId, Option<ValueId>>,
) -> Option<ValueId> {
    let mut current = state;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current) {
            return None;
        }
        if context.is_block_argument(current) || context.region_of_port(current).is_some() {
            current = incoming(context, current)?;
            continue;
        }
        let Some(op) = context
            .get_value(current)
            .defining_op()
            .map(|op| context.get_op(op))
        else {
            return Some(current);
        };
        if op.is::<JoinOp>() {
            let mut roots = op
                .operands()
                .iter()
                .map(|&operand| chain_root_memo(context, operand, memo))
                .collect::<Option<BTreeSet<_>>>()?;
            return (roots.len() == 1).then(|| roots.pop_first().expect("one root"));
        }
        if op.is::<SplitOp>() {
            return None;
        }
        let observed = op
            .clone()
            .as_interface::<dyn MemoryRead>()
            .and_then(|read| read.state_operand())
            .or_else(|| {
                op.clone()
                    .as_interface::<dyn MemoryWrite>()
                    .and_then(|write| write.state_operand())
            });
        if let Some(observed) = observed {
            current = observed;
            continue;
        }
        if op.has_interface::<dyn Theta>() {
            // A dependency result of a theta is entered on the dependency
            // operand at the same index; a value result on the init.
            let deps = op.dep_results();
            if let Some(port) = deps.iter().position(|&r| r == current) {
                current = op.dep_operands()[port];
                continue;
            }
            let carried = carried(context, &op)?;
            let port = carried.finals.iter().position(|&r| r == current)?;
            current = carried.inits[port];
            continue;
        }
        if let Some(gamma) = op.clone().as_interface::<dyn Gamma>() {
            let deps = op.dep_results();
            let port = deps.iter().position(|&r| r == current)?;
            let mut roots = gamma
                .arms()
                .iter()
                .map(|&arm| {
                    chain_root_memo(context, *context.get_region(arm).dep_results().get(port)?, memo)
                })
                .collect::<Option<BTreeSet<_>>>()?;
            return (roots.len() == 1).then(|| roots.pop_first().expect("one root"));
        }
        if let Some(carried) = op.clone().as_interface::<dyn LoopLike>() {
            let port = carried.finals().iter().position(|&r| r == current)?;
            current = carried.inits()[port];
            continue;
        }
        if let Some(gate) = op.clone().as_interface::<dyn Conditional>() {
            let port = op.results().iter().position(|&r| r == current)?;
            let mut roots = op
                .regions()
                .iter()
                .map(|&region| {
                    chain_root_memo(context, *gate.region_yields(region).get(port)?, memo)
                })
                .collect::<Option<BTreeSet<_>>>()?;
            return (roots.len() == 1).then(|| roots.pop_first().expect("one root"));
        }
        return Some(current);
    }
}

/// The value a region entry argument stands for outside the region.
fn incoming(context: &Context, argument: ValueId) -> Option<ValueId> {
    if let Some(region) = context.region_of_port(argument) {
        let handle = context.get_region(region);
        let owner = context.get_op(handle.parent_op()?);
        let deps: Vec<ValueId> = handle
            .dep_arguments()
            .iter()
            .map(crate::Value::id)
            .collect();
        if let Some(index) = deps.iter().position(|&port| port == argument) {
            return owner.dep_operands().get(index).copied();
        }
        let values: Vec<ValueId> = handle
            .value_arguments()
            .iter()
            .map(crate::Value::id)
            .collect();
        let index = values.iter().position(|&port| port == argument)?;
        if let Some(theta) = owner.clone().as_interface::<dyn Theta>() {
            let binding = theta.carried();
            return owner
                .value_operands()
                .get(binding.operands.start + index - binding.ports.start)
                .copied();
        }
        let gamma = owner.clone().as_interface::<dyn Gamma>()?;
        let binding = gamma.forwarded();
        return owner
            .value_operands()
            .get(binding.operands.start + index - binding.ports.start)
            .copied();
    }
    let block = context.block_of_argument(argument)?;
    let region = context.parent_region(block)?;
    let op_id = context.get_region(region).parent_op()?;
    let op = context.get_op(op_id);
    if let Some(carried) = op.clone().as_interface::<dyn LoopLike>() {
        let port = carried.carried_args().iter().position(|&a| a == argument)?;
        return carried.inits().get(port).copied();
    }
    // A gate threads what it was given into each arm, so the arms' arguments are
    // the tail of its operands.
    let arguments = context.get_block(block).arguments().len();
    let index = context
        .get_block(block)
        .arguments()
        .iter()
        .position(|a| a.id() == argument)?;
    op.operands()
        .len()
        .checked_sub(arguments)
        .and_then(|offset| op.operands().get(offset + index).copied())
}
