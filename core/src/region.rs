use crate::{
    BlockId, Context, ContextIterator, GetFromContext, IdMap, OpId, Terminator, Value, ValueId,
};

id_newtype!(RegionId);

/// Which kind of body an operation declares for a region, as spelled in
/// `operation!`. `Any` accepts either, so the parser reads the kind off the
/// text and a walker has to ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionKind {
    Blocks,
    Nodes,
    Any,
}

/// A reference to a region: the context that owns it, and its id. Reads answer
/// with the region as it stands now; see [`crate::OpHandle`].
#[derive(Clone)]
pub struct RegionHandle {
    pub context: Context,
    pub id: RegionId,
}

impl std::fmt::Debug for RegionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RegionHandle").field(&self.id).finish()
    }
}

impl RegionHandle {
    fn context(&self) -> Context {
        let context = self.context.clone();
        debug_assert!(
            context.has_region(self.id),
            "handle to erased region {:?}",
            self.id
        );
        context
    }

    /// Whether this handle still names the region it was minted for; see
    /// [`crate::OpHandle::is_live`].
    pub fn is_live(&self) -> bool {
        self.context.has_region(self.id)
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
        self.context().region_blocks(self.id)
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
        if context.region_is_nodes(self.id) {
            return context.region_ops(self.id);
        }
        self.block_ids()
            .into_iter()
            .flat_map(|block| context.block_ops(block))
            .collect()
    }

    /// The region's arguments: its own for an unordered region, its entry
    /// block's for an ordered one — the same values either way, so a reader
    /// need not know which kind it holds.
    pub fn ports(&self) -> Vec<Value> {
        let context = self.context();
        if context.region_is_nodes(self.id) {
            return context.region_ports(self.id);
        }
        match self.block_ids().first() {
            Some(entry) => context.block_arguments(*entry),
            None => Vec::new(),
        }
    }

    /// The arguments that are not resource states.
    pub fn value_arguments(&self) -> Vec<Value> {
        let context = self.context();
        self.ports()
            .into_iter()
            .filter(|port| !context.is_state_type(port.ty()))
            .collect()
    }

    /// The resource states carried into the region.
    pub fn state_arguments(&self) -> Vec<Value> {
        let context = self.context();
        self.ports()
            .into_iter()
            .filter(|port| context.is_state_type(port.ty()))
            .collect()
    }

    /// The values an unordered region produces; empty for an ordered one,
    /// which binds its results through its terminator instead.
    pub fn results(&self) -> Vec<ValueId> {
        self.context().region_results(self.id)
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

pub(crate) fn defining_region(context: &Context, value: ValueId) -> Option<RegionId> {
    if let Some(region) = context.region_of_port(value) {
        return Some(region);
    }
    if let Some(block) = context.block_of_argument(value) {
        return context.parent_region(block);
    }
    context.region_of_op(context.get_value(value).defining_op()?)
}

pub(crate) fn topological_order(
    context: &Context,
    region: RegionId,
) -> Result<Vec<OpId>, crate::Error> {
    let reads = SiblingReads::of_region(context, region);
    topological_order_picking(context, region, &reads, |ready| ready.pop_first())
}

pub(crate) fn shuffled_topological_order(
    context: &Context,
    region: RegionId,
    rng: &mut crate::utils::Rng,
) -> Result<Vec<OpId>, crate::Error> {
    let reads = SiblingReads::of_region(context, region);
    topological_order_picking(context, region, &reads, |ready| {
        if ready.is_empty() {
            return None;
        }
        let pick = *ready.iter().nth(rng.below(ready.len()))?;
        ready.take(&pick)
    })
}

fn topological_order_picking(
    context: &Context,
    region: RegionId,
    reads: &SiblingReads,
    mut pick: impl FnMut(&mut std::collections::BTreeSet<OpId>) -> Option<OpId>,
) -> Result<Vec<OpId>, crate::Error> {
    let ops = context.get_region(region).op_ids();
    let mut readers: IdMap<OpId, Vec<OpId>> = IdMap::new();
    let mut pending: IdMap<OpId, usize> = IdMap::new();
    for &op in &ops {
        let mut read = 0;
        for producer in reads.of(op) {
            readers.entry(producer).or_default().push(op);
            read += 1;
        }
        pending.insert(op, read);
    }

    let mut ready: std::collections::BTreeSet<OpId> = pending
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
    let instance = context.get_op(cycle_member(reads, &pending));
    Err(crate::Error::VerificationError(format!(
        "{}.{} closes a dependency cycle",
        instance.dialect(),
        instance.name()
    )))
}

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

// A value defined under the reader has its producer in another region and
// orders nothing beside the reader.
#[derive(Default)]
pub(crate) struct SiblingReads {
    // Sorted.
    pairs: Vec<(OpId, OpId)>,
}

impl SiblingReads {
    pub(crate) fn under(
        context: &Context,
        root: RegionId,
        tests: impl IntoIterator<Item = (OpId, ValueId)>,
    ) -> Self {
        let mut pairs = context.sibling_reads(root, true);
        pairs.extend(
            tests
                .into_iter()
                .filter_map(|(reader, value)| context.sibling_read(reader, value)),
        );
        Self::sorted(pairs)
    }

    pub(crate) fn of_region(context: &Context, region: RegionId) -> Self {
        Self::sorted(context.sibling_reads(region, false))
    }

    fn sorted(mut pairs: Vec<(OpId, OpId)>) -> Self {
        pairs.sort_unstable();
        pairs.dedup();
        Self { pairs }
    }

    pub(crate) fn of(&self, op: OpId) -> impl Iterator<Item = OpId> + '_ {
        let start = self.pairs.partition_point(|pair| pair.0 < op);
        self.pairs[start..]
            .iter()
            .take_while(move |pair| pair.0 == op)
            .map(|pair| pair.1)
    }
}

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

fn cycle_member(reads: &SiblingReads, pending: &IdMap<OpId, usize>) -> OpId {
    let blocked = |op: &OpId| pending.get(op).is_some_and(|count| *count > 0);
    let start = pending
        .keys()
        .filter(|op| blocked(op))
        .min()
        .copied()
        .expect("a short order leaves an operation behind");

    let mut path = vec![start];
    loop {
        let next = reads
            .of(*path.last().unwrap())
            .filter(|input| blocked(input))
            .min()
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
