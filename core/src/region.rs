use crate::{
    BlockId, Context, ContextIterator, GetFromContext, OpId, Terminator, Value, ValueId,
    context::ContextRef,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RegionId(u32);

/// What a region holds, and how what it holds is ordered.
///
/// An ordered region is a control-flow graph: its blocks run one after another
/// and hand control on through terminators. An unordered region is a dependence
/// graph: nothing but the def-use edges between its operations says what runs
/// before what, so `ops` is insertion order and is never read as meaning.
#[derive(Debug)]
pub enum RegionBody {
    Blocks(Vec<BlockId>),
    Nodes {
        /// The region's own arguments, values first and dependencies trailing.
        /// An ordered region has these too — they are its entry block's
        /// arguments; see [`RegionHandle::ports`].
        ports: Vec<Value>,
        dep_ports: u32,
        ops: Vec<OpId>,
        /// The values the region produces, in the order the enclosing operation
        /// binds them, with the dependencies it hands on trailing.
        results: Vec<ValueId>,
        dep_results: u32,
    },
}

/// A region's storage record, living densely in the context's region slab and
/// edited in place through [`Context`] under its write lock. Reads go through
/// [`RegionHandle`].
#[derive(Debug)]
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

    pub(crate) fn new_nodes(
        ports: Vec<Value>,
        dep_ports: usize,
        ops: Vec<OpId>,
        results: Vec<ValueId>,
        dep_results: usize,
    ) -> Region {
        Region {
            body: RegionBody::Nodes {
                ports,
                dep_ports: dep_ports as u32,
                ops,
                results,
                dep_results: dep_results as u32,
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
    pub context: ContextRef,
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
        let context = self.context.upgrade();
        #[cfg(debug_assertions)]
        context.assert_region_generation(self.id, self.generation);
        context
    }

    /// Whether this handle still names the region it was minted for; see
    /// [`crate::OpHandle::is_live`].
    pub fn is_live(&self) -> bool {
        self.context.upgrade().region_generation(self.id) == self.generation
    }

    pub fn id(&self) -> RegionId {
        self.id
    }

    /// The operation owning this region, if it has been attached to one.
    pub fn parent_op(&self) -> Option<OpId> {
        self.context().region_parent_op(self.id)
    }

    /// Whether this region holds an unordered graph rather than blocks.
    pub fn is_nodes(&self) -> bool {
        self.context().region_is_nodes(self.id)
    }

    pub fn add_block(&self, id: BlockId) {
        self.context().add_block_to_region(self.id, id);
    }

    pub fn remove_block(&self, id: BlockId) -> bool {
        self.context().remove_block_from_region(self.id, id)
    }

    pub fn block_ids(&self) -> Vec<BlockId> {
        self.context().region_block_ids(self.id)
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
        self.context().region_op_ids(self.id)
    }

    /// The region's arguments, values first and dependencies trailing: its own
    /// for an unordered region, its entry block's for an ordered one — the same
    /// values either way, so a reader need not know which kind it holds.
    pub fn ports(&self) -> Vec<Value> {
        self.context().region_ports(self.id)
    }

    /// The arguments that carry a value.
    pub fn value_arguments(&self) -> Vec<Value> {
        let mut ports = self.ports();
        ports.truncate(ports.len() - self.context().region_dep_counts(self.id).0);
        ports
    }

    /// The arguments that are dependencies.
    pub fn dep_arguments(&self) -> Vec<Value> {
        let ports = self.ports();
        ports[ports.len() - self.context().region_dep_counts(self.id).0..].to_vec()
    }

    /// The values an unordered region produces, dependencies trailing; empty
    /// for an ordered one, which binds its results through its
    /// [`crate::RegionExit`] operations instead.
    pub fn results(&self) -> Vec<ValueId> {
        self.context().region_results(self.id)
    }

    /// The results that carry a value.
    pub fn value_results(&self) -> Vec<ValueId> {
        let mut results = self.results();
        results.truncate(results.len() - self.context().region_dep_counts(self.id).1);
        results
    }

    /// The results that are dependencies.
    pub fn dep_results(&self) -> Vec<ValueId> {
        let results = self.results();
        results[results.len() - self.context().region_dep_counts(self.id).1..].to_vec()
    }

    /// Replace the whole block list at once. Only [`Context::replace_region_contents`]
    /// uses this: it owns the parent bookkeeping and the single version bump the
    /// swap is allowed to make, which the per-block mutators above would each repeat.
    pub(crate) fn set_blocks(&self, blocks: Vec<BlockId>) {
        self.context().set_region_blocks(self.id, blocks);
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
        for &value in ops
            .iter()
            .flat_map(|op| context.get_op(*op).operands())
            .collect::<Vec<_>>()
            .iter()
            .chain(self.results().iter())
        {
            self.verify_in_scope(context, value)?;
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
    while let Some(op) = ready.pop_first() {
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

/// Every value an operation reads, its nested regions included: what a region
/// holds is one node of its dependence graph, whatever it holds inside. A
/// value a nested region names as its result outright is read too.
pub(crate) fn values_read(context: &Context, op: OpId) -> Vec<ValueId> {
    let instance = context.get_op(op);
    let mut values = instance.operands().to_vec();
    for region in instance.regions() {
        let region = context.get_region(region);
        values.extend(region.results());
        for child in region.op_ids() {
            values.extend(values_read(context, child));
        }
    }
    values
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

impl RegionId {
    pub(crate) fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn number(self) -> u32 {
        self.0
    }

    pub fn from_number(n: u32) -> Self {
        Self(n)
    }

    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }

    /// The hive handle backing this id.
    pub(crate) fn raw(self) -> u32 {
        self.0
    }
}

impl GetFromContext for RegionId {
    type Item = RegionHandle;

    fn get_from_context(&self, context: &Context) -> Self::Item {
        context.get_region(*self)
    }
}
