use crate::{BlockId, Context, ContextIterator, GetFromContext, OpId, Terminator, Value, ValueId};

id_newtype!(RegionId);

/// What a region holds, and how what it holds is ordered.
///
/// An ordered region is a control-flow graph: its blocks run one after another
/// and hand control on through terminators. An unordered region is a dependence
/// graph: nothing but the def-use edges between its operations says what runs
/// before what, so `ops` is insertion order and is never read as meaning.
#[derive(Debug, Clone)]
pub enum RegionBody {
    Blocks(Vec<BlockId>),
    Nodes {
        /// The region's own arguments. An ordered region has these too — they
        /// are its entry block's arguments; see [`RegionHandle::ports`].
        ports: Vec<Value>,
        ops: Vec<OpId>,
        /// The values the region produces, in the order the enclosing operation
        /// binds them.
        results: Vec<ValueId>,
    },
}

/// A region's storage record, living densely in the context's region slab and
/// edited in place through [`Context`] under its write lock. Reads go through
/// [`RegionHandle`].
#[derive(Debug, Clone)]
pub struct Region {
    body: RegionBody,
    parent_op: OpId,
}

/// Which kind of body an operation declares for a region, as spelled in
/// `operation!`. `Any` accepts either, so the parser reads the kind off the
/// text and a walker has to ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionKind {
    Blocks,
    Nodes,
    Any,
}

impl Region {
    pub(crate) fn new() -> Region {
        Region {
            body: RegionBody::Blocks(vec![]),
            parent_op: OpId::invalid(),
        }
    }

    pub(crate) fn new_nodes(ports: Vec<Value>, ops: Vec<OpId>, results: Vec<ValueId>) -> Region {
        Region {
            body: RegionBody::Nodes {
                ports,
                ops,
                results,
            },
            parent_op: OpId::invalid(),
        }
    }

    pub(crate) fn heap_bytes(&self) -> usize {
        match &self.body {
            RegionBody::Blocks(blocks) => blocks.capacity() * std::mem::size_of::<BlockId>(),
            RegionBody::Nodes {
                ports,
                ops,
                results,
                ..
            } => {
                ports.capacity() * std::mem::size_of::<Value>()
                    + ops.capacity() * std::mem::size_of::<OpId>()
                    + results.capacity() * std::mem::size_of::<ValueId>()
            }
        }
    }

    pub(crate) fn set_parent_op(&mut self, op: OpId) {
        self.parent_op = op;
    }

    /// The operation owning this region, if it has been attached to one.
    pub(crate) fn parent_op(&self) -> Option<OpId> {
        (self.parent_op != OpId::invalid()).then_some(self.parent_op)
    }

    pub(crate) fn body(&self) -> &RegionBody {
        &self.body
    }

    pub(crate) fn body_mut(&mut self) -> &mut RegionBody {
        &mut self.body
    }

    pub(crate) fn blocks(&self) -> &[BlockId] {
        match &self.body {
            RegionBody::Blocks(blocks) => blocks,
            RegionBody::Nodes { .. } => &[],
        }
    }

    pub(crate) fn blocks_mut(&mut self) -> &mut Vec<BlockId> {
        match &mut self.body {
            RegionBody::Blocks(blocks) => blocks,
            RegionBody::Nodes { .. } => panic!("an unordered region holds no blocks"),
        }
    }
}

/// A reference to a region: the context that owns it, and its id. Reads answer
/// with the region as it stands now; see [`crate::OpHandle`].
#[derive(Clone)]
pub struct RegionHandle {
    pub context: Context,
    pub(crate) generation: u32,
    pub id: RegionId,
}

impl std::fmt::Debug for RegionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RegionHandle").field(&self.id).finish()
    }
}

impl RegionHandle {
    /// The owning context, after checking this handle still names its own region.
    fn context(&self) -> Context {
        let context = self.context.clone();
        debug_assert_eq!(
            context.region_generation(self.id),
            self.generation,
            "handle to erased region {:?}",
            self.id
        );
        context
    }

    /// Whether this handle still names the region it was minted for; see
    /// [`crate::OpHandle::is_live`].
    pub fn is_live(&self) -> bool {
        self.context.region_generation(self.id) == self.generation
    }

    pub fn id(&self) -> RegionId {
        self.id
    }

    /// The operation owning this region, if it has been attached to one.
    pub fn parent_op(&self) -> Option<OpId> {
        self.context().with_region(self.id, Region::parent_op)
    }

    /// Whether this region holds an unordered graph rather than blocks.
    pub fn is_nodes(&self) -> bool {
        self.context().with_region(self.id, |region| {
            matches!(region.body(), RegionBody::Nodes { .. })
        })
    }

    pub fn add_block(&self, id: BlockId) {
        self.context().add_block_to_region(self.id, id);
    }

    pub fn remove_block(&self, id: BlockId) -> bool {
        self.context().remove_block_from_region(self.id, id)
    }

    pub fn block_ids(&self) -> Vec<BlockId> {
        self.context()
            .with_region(self.id, |region| region.blocks().to_vec())
    }

    /// The entry block of an ordered region: where control enters and where its
    /// arguments live.
    pub fn entry_block(&self) -> BlockId {
        self.block_ids()[0]
    }

    /// Every operation the region holds, in no particular order for an
    /// unordered region and in block order for an ordered one. What a walk of
    /// the region's contents iterates, whichever kind it is.
    pub fn op_ids(&self) -> Vec<OpId> {
        let context = self.context();
        let (ops, blocks) = context.with_region(self.id, |region| match region.body() {
            RegionBody::Nodes { ops, .. } => (ops.clone(), Vec::new()),
            RegionBody::Blocks(blocks) => (Vec::new(), blocks.clone()),
        });
        if blocks.is_empty() {
            return ops;
        }
        blocks
            .into_iter()
            .flat_map(|block| context.get_block(block).op_ids())
            .collect()
    }

    /// The region's arguments: its own for an unordered region, its entry
    /// block's for an ordered one — the same values either way, so a reader
    /// need not know which kind it holds.
    pub fn ports(&self) -> Vec<Value> {
        let context = self.context();
        let (ports, entry) = context.with_region(self.id, |region| match region.body() {
            RegionBody::Nodes { ports, .. } => (ports.clone(), None),
            RegionBody::Blocks(blocks) => (Vec::new(), blocks.first().copied()),
        });
        match entry {
            Some(entry) => context.get_block(entry).arguments(),
            None => ports,
        }
    }

    /// The arguments that are not memory states.
    pub fn value_arguments(&self) -> Vec<Value> {
        self.ports()
            .into_iter()
            .filter(|port| !port.is_state())
            .collect()
    }

    /// The arguments that are memory states.
    pub fn state_arguments(&self) -> Vec<Value> {
        self.ports().into_iter().filter(Value::is_state).collect()
    }

    /// The values an unordered region produces; empty for an ordered one,
    /// which binds its results through its terminator instead.
    pub fn results(&self) -> Vec<ValueId> {
        self.context()
            .with_region(self.id, |region| match region.body() {
                RegionBody::Nodes { results, .. } => results.clone(),
                RegionBody::Blocks(_) => Vec::new(),
            })
    }

    /// The results that are not memory states.
    pub fn value_results(&self) -> Vec<ValueId> {
        self.context().values_among(&self.results()).to_vec()
    }

    /// The results that are memory states.
    pub fn state_results(&self) -> Vec<ValueId> {
        self.context().states_among(&self.results()).to_vec()
    }

    pub fn iter(&self, context: Context) -> ContextIterator<BlockId> {
        ContextIterator::new(context, self.block_ids())
    }

    pub fn verify(&self, context: &Context) -> Result<(), crate::Error> {
        if self.is_nodes() {
            return self.verify_nodes(context);
        }
        for block_id in self.block_ids() {
            let block = context.get_block(block_id);
            let ops = block.op_ids();
            if ops.is_empty() {
                return Err(crate::Error::VerificationError(
                    "basic blocks must have at least one operation".to_string(),
                ));
            }

            let op = ops.last().unwrap().get_from_context(context);
            if op.as_interface::<dyn Terminator>().is_none() {
                return Err(crate::Error::VerificationError(
                    "basic blocks must end with a terminator".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// An unordered region evaluates by demand, so nothing may transfer control
    /// inside it and its dependencies must admit an evaluation order.
    fn verify_nodes(&self, context: &Context) -> Result<(), crate::Error> {
        let ops = self.op_ids();
        for op in &ops {
            let instance = context.get_op(*op);
            if instance.clone().as_interface::<dyn Terminator>().is_some() {
                return Err(crate::Error::VerificationError(format!(
                    "{}.{} is a terminator, which an unordered region has no place for",
                    instance.dialect(),
                    instance.name()
                )));
            }
        }
        for op in &ops {
            let instance = context.get_op(*op);
            for value in instance.operands() {
                self.verify_in_scope(context, value).map_err(|error| {
                    crate::Error::VerificationError(format!(
                        "{}.{} reads {error}",
                        instance.dialect(),
                        instance.name()
                    ))
                })?;
            }
        }
        for value in self.results() {
            self.verify_in_scope(context, value).map_err(|error| {
                crate::Error::VerificationError(format!("a region result names {error}"))
            })?;
        }
        topological_order(context, self.id).map(|_| ())
    }

    /// An unordered region reads only what it or an enclosing region defines:
    /// nothing puts it in sequence with a sibling, so naming a sibling's value
    /// would be naming something that need never have run.
    fn verify_in_scope(&self, context: &Context, value: ValueId) -> Result<(), crate::Error> {
        if !context.has_value(value) {
            return Err(crate::Error::VerificationError(format!(
                "%{} does not exist",
                value.number()
            )));
        }
        let Some(owner) = defining_region(context, value) else {
            return Ok(());
        };
        let mut scope = Some(self.id);
        while let Some(region) = scope {
            if region == owner {
                return Ok(());
            }
            scope = context
                .get_region(region)
                .parent_op()
                .and_then(|op| context.region_of_op(op));
        }
        Err(crate::Error::VerificationError(format!(
            "%{} is defined outside the unordered region reading it",
            value.number()
        )))
    }
}

/// The region a value belongs to: the one whose port it is, the one holding the
/// block whose argument it is, or the one holding the operation defining it.
pub(crate) fn defining_region(context: &Context, value: ValueId) -> Option<RegionId> {
    if let Some(region) = context.region_of_port(value) {
        return Some(region);
    }
    if let Some(block) = context.block_of_argument(value) {
        return context.parent_region(block);
    }
    context.region_of_op(context.get_value(value).defining_op()?)
}

/// The order an unordered region's operations evaluate in: every operation
/// after the ones it reads, and among those the one with the smallest id first.
///
/// The tie-break makes the order a function of the graph alone, so printing a
/// parsed region and parsing the result again is a fixed point. Operations left
/// over at the end sit on a dependency cycle, which has no evaluation order at
/// all; the error names one of the operations closing it.
pub(crate) fn topological_order(
    context: &Context,
    region: RegionId,
) -> Result<Vec<OpId>, crate::Error> {
    topological_order_picking(context, region, |ready| ready.pop_first())
}

/// [`topological_order`] with the tie among ready operations broken by `rng`
/// instead of by id: one random linearization of the same graph.
pub(crate) fn shuffled_topological_order(
    context: &Context,
    region: RegionId,
    rng: &mut crate::utils::Rng,
) -> Result<Vec<OpId>, crate::Error> {
    topological_order_picking(context, region, |ready| {
        if ready.is_empty() {
            return None;
        }
        let pick = *ready.iter().nth(rng.below(ready.len()))?;
        ready.take(&pick)
    })
}

/// [`topological_order`] with the tie among ready operations broken by
/// insertion order: the order the region was built in, which a converter
/// leaves as source order.
pub(crate) fn insertion_topological_order(
    context: &Context,
    region: RegionId,
) -> Result<Vec<OpId>, crate::Error> {
    let inserted: std::collections::HashMap<OpId, usize> = context
        .get_region(region)
        .op_ids()
        .into_iter()
        .enumerate()
        .map(|(index, op)| (op, index))
        .collect();
    topological_order_picking(context, region, |ready| {
        let pick = ready.iter().copied().min_by_key(|op| inserted[op])?;
        ready.take(&pick)
    })
}

fn topological_order_picking(
    context: &Context,
    region: RegionId,
    mut pick: impl FnMut(&mut std::collections::BTreeSet<OpId>) -> Option<OpId>,
) -> Result<Vec<OpId>, crate::Error> {
    use std::collections::{BTreeSet, HashMap, HashSet};

    let ops: HashSet<OpId> = context.get_region(region).op_ids().into_iter().collect();
    let mut inputs: HashMap<OpId, Vec<OpId>> = HashMap::new();
    let mut readers: HashMap<OpId, Vec<OpId>> = HashMap::new();
    let mut pending: HashMap<OpId, usize> = HashMap::new();
    for &op in &ops {
        let read: Vec<OpId> = values_read(context, op)
            .into_iter()
            .filter_map(|value| context.get_value(value).defining_op())
            .filter(|producer| ops.contains(producer))
            .collect();
        for producer in &read {
            readers.entry(*producer).or_default().push(op);
        }
        pending.insert(op, read.len());
        inputs.insert(op, read);
    }

    let mut ready: BTreeSet<OpId> = pending
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(op, _)| *op)
        .collect();
    let mut order = Vec::with_capacity(ops.len());
    while let Some(op) = pick(&mut ready) {
        order.push(op);
        for reader in readers.get(&op).into_iter().flatten() {
            let count = pending.get_mut(reader).expect("reader of a region op");
            *count -= 1;
            if *count == 0 {
                ready.insert(*reader);
            }
        }
    }

    if order.len() == ops.len() {
        return Ok(order);
    }
    let instance = context.get_op(cycle_member(&inputs, &pending));
    Err(crate::Error::VerificationError(format!(
        "{}.{} closes a dependency cycle",
        instance.dialect(),
        instance.name()
    )))
}

/// Every value an operation reads from the scope holding it, its nested
/// regions included: what a region holds is one node of its dependence graph,
/// whatever it holds inside. A value a nested region names as its result is
/// read only when the region forwards it from outside; one the region produces
/// is a definition, and naming it here would let a caller entering a block on
/// what it reads mint an argument for a value defined inside the loop, cutting
/// the chain that produced it.
pub(crate) fn values_read(context: &Context, op: OpId) -> Vec<ValueId> {
    let instance = context.get_op(op);
    let mut values = instance.operands().to_vec();
    // The operands above stand as they are: an operation naming its own result
    // is the shortest dependency cycle there is, and subtracting what this
    // operation defines would hide it from the order that reports one.
    let mut nested = Vec::new();
    let mut defined = std::collections::HashSet::new();
    for region in instance.regions() {
        collect_region_reads(context, region, &mut nested, &mut defined);
    }
    nested.retain(|value| !defined.contains(value));
    values.extend(nested);
    values
}

/// What `region` and everything under it reads, and what the same span defines.
fn collect_region_reads(
    context: &Context,
    region: RegionId,
    values: &mut Vec<ValueId>,
    defined: &mut std::collections::HashSet<ValueId>,
) {
    let handle = context.get_region(region);
    values.extend(handle.results());
    defined.extend(handle.ports().iter().map(crate::Value::id));
    for child in handle.op_ids() {
        let child = context.get_op(child);
        values.extend(child.operands().iter().copied());
        defined.extend(child.results());
        for nested in child.regions() {
            collect_region_reads(context, nested, values, defined);
        }
    }
}

/// One operation actually on a cycle, rather than merely downstream of one:
/// walk back through the operations Kahn left pending until the walk repeats
/// itself, and answer with the smallest id it went round.
fn cycle_member(
    inputs: &std::collections::HashMap<OpId, Vec<OpId>>,
    pending: &std::collections::HashMap<OpId, usize>,
) -> OpId {
    let blocked = |op: &OpId| pending.get(op).is_some_and(|count| *count > 0);
    let start = pending
        .keys()
        .filter(|op| blocked(op))
        .min()
        .copied()
        .expect("a short order leaves an operation behind");

    let mut path = vec![start];
    loop {
        let next = inputs[path.last().unwrap()]
            .iter()
            .filter(|input| blocked(input))
            .min()
            .copied()
            .expect("an operation Kahn left pending reads one it also left");
        if let Some(from) = path.iter().position(|op| *op == next) {
            return path[from..]
                .iter()
                .copied()
                .min()
                .expect("a cycle is walked");
        }
        path.push(next);
    }
}

impl GetFromContext for RegionId {
    type Item = RegionHandle;

    fn get_from_context(&self, context: &Context) -> Self::Item {
        context.get_region(*self)
    }
}
