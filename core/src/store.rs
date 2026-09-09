//! Entity storage shared by the frozen base and an overlay's delta: hives for
//! ops, values, blocks and regions, the runs their ports live in, the use
//! lists threaded through those runs, and the parent indices.
//!
//! One numeric id space covers both stores. A base store addresses an id by
//! its raw number; a delta store holds ids at or past its `frontier` (created
//! in the overlay) at `id - frontier`, and a base id it shadows through a
//! translation table. Ids are bump-allocated and never reused, so an entity
//! created in an overlay takes its final id at creation and a commit appends
//! it to the base without renaming anything.

use tir_adt::{Hive, Sym};

use crate::run::{AttrRuns, EntryId, NO_ENTRY, Runs};
use crate::{
    Block, BlockId, OpId, OpInstance, Region, RegionId, Value, ValueId,
    attributes::{AttributeValue, NamedAttribute},
};

/// No handle: a base id the delta does not shadow.
pub(crate) const NO_HANDLE: u32 = u32::MAX;

/// A generation no live handle carries: the slot was erased.
pub(crate) const ERASED: u32 = u32::MAX;

/// Read an entry from a side table indexed by a handle.
pub(crate) fn slab_get<T>(slab: &[Option<T>], idx: usize) -> Option<&T> {
    slab.get(idx).and_then(Option::as_ref)
}

/// Insert into a side table at a handle, growing the table as needed.
pub(crate) fn slab_put<T>(slab: &mut Vec<Option<T>>, idx: usize, val: T) {
    if idx >= slab.len() {
        slab.resize_with(idx + 1, || None);
    }
    slab[idx] = Some(val);
}

pub(crate) fn clear_slot<T>(slab: &mut [Option<T>], idx: usize) {
    if let Some(slot) = slab.get_mut(idx) {
        *slot = None;
    }
}

/// What holds an operation: a block of an ordered region, or an unordered
/// region directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parent {
    Block(BlockId),
    Region(RegionId),
}

/// The first id of each kind an overlay may create: the base hive's next
/// handle when the overlay opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Frontier {
    pub ops: u32,
    pub values: u32,
    pub blocks: u32,
    pub regions: u32,
}

/// One id kind's addressing in a delta store: the ids it created sit at
/// `id - frontier`, and the base ids it shadows go through a table.
#[derive(Default)]
struct Kind {
    frontier: u32,
    /// Handle of each entity this delta created, by `id - frontier`.
    local: Vec<u32>,
    /// Handle of the copy held for a base id, by that id.
    shadow: Vec<u32>,
}

impl Kind {
    fn new(frontier: u32) -> Self {
        Kind {
            frontier,
            local: Vec::new(),
            shadow: Vec::new(),
        }
    }

    fn handle(&self, raw: u32) -> Option<u32> {
        if raw >= self.frontier {
            self.local.get((raw - self.frontier) as usize).copied()
        } else {
            self.shadow
                .get(raw as usize)
                .copied()
                .filter(|h| *h != NO_HANDLE)
        }
    }

    fn owns(&self, raw: u32) -> bool {
        raw >= self.frontier
    }

    fn shadows(&self, raw: u32) -> bool {
        !self.owns(raw) && self.handle(raw).is_some()
    }

    fn record_shadow(&mut self, raw: u32, handle: u32) {
        if raw as usize >= self.shadow.len() {
            self.shadow.resize(raw as usize + 1, NO_HANDLE);
        }
        self.shadow[raw as usize] = handle;
    }

    fn unshadow(&mut self, raw: u32) {
        if let Some(slot) = self.shadow.get_mut(raw as usize) {
            *slot = NO_HANDLE;
        }
    }

    /// The id the next created entity takes.
    fn next_id(&self) -> u32 {
        self.frontier + self.local.len() as u32
    }

    fn shadowed(&self) -> Vec<(u32, u32)> {
        self.shadow
            .iter()
            .enumerate()
            .filter(|(_, h)| **h != NO_HANDLE)
            .map(|(raw, h)| (raw as u32, *h))
            .collect()
    }

    /// The ids this delta created, in creation order, erased ones included.
    fn locals(&self) -> Vec<u32> {
        (0..self.local.len() as u32)
            .map(|i| self.frontier + i)
            .collect()
    }
}

struct DeltaTables {
    ops: Kind,
    values: Kind,
    blocks: Kind,
    regions: Kind,
}

pub(crate) struct Store {
    /// Present for a delta store; a base store addresses an id by its number.
    delta: Option<DeltaTables>,
    pub(crate) ops: Hive<OpInstance>,
    pub(crate) values: Hive<Value>,
    pub(crate) regions: Hive<Region>,
    pub(crate) blocks: Hive<Block>,
    /// The epoch each op, block and region was created in, by handle; a base
    /// store fills these at commit. A handle minted in that epoch is live
    /// while the table still says so.
    pub(crate) op_epoch: Vec<u32>,
    pub(crate) block_epoch: Vec<u32>,
    pub(crate) region_epoch: Vec<u32>,
    /// Reverse index from an operation handle to whatever holds it.
    pub(crate) op_parent: Vec<Option<Parent>>,
    /// Reverse index from a block handle to the region that holds it.
    pub(crate) block_parent: Vec<Option<RegionId>>,
    /// Def-site index for block arguments, by value handle.
    pub(crate) value_block: Vec<Option<BlockId>>,
    /// Def-site index for the ports of an unordered region, by value handle.
    pub(crate) value_region: Vec<Option<RegionId>>,
    pub(crate) runs: Runs,
    pub(crate) attr_runs: AttrRuns,
    /// Use lists: raw value id → the first operand entry naming it.
    pub(crate) first_use: Vec<u32>,
}

impl Store {
    pub(crate) fn base() -> Self {
        Self::new(None)
    }

    pub(crate) fn delta(frontier: Frontier) -> Self {
        Self::new(Some(DeltaTables {
            ops: Kind::new(frontier.ops),
            values: Kind::new(frontier.values),
            blocks: Kind::new(frontier.blocks),
            regions: Kind::new(frontier.regions),
        }))
    }

    fn new(delta: Option<DeltaTables>) -> Self {
        Store {
            delta,
            ops: Hive::new(),
            values: Hive::new(),
            regions: Hive::new(),
            blocks: Hive::new(),
            op_epoch: Vec::new(),
            block_epoch: Vec::new(),
            region_epoch: Vec::new(),
            op_parent: Vec::new(),
            block_parent: Vec::new(),
            value_block: Vec::new(),
            value_region: Vec::new(),
            runs: Runs::default(),
            attr_runs: AttrRuns::default(),
            first_use: Vec::new(),
        }
    }

    /// The ids the next overlay over this base may create.
    pub(crate) fn frontier(&self) -> Frontier {
        Frontier {
            ops: self.ops.next_handle(),
            values: self.values.next_handle(),
            blocks: self.blocks.next_handle(),
            regions: self.regions.next_handle(),
        }
    }

    fn kind_ops(&self) -> Option<&Kind> {
        self.delta.as_ref().map(|d| &d.ops)
    }

    fn kind_values(&self) -> Option<&Kind> {
        self.delta.as_ref().map(|d| &d.values)
    }

    fn kind_blocks(&self) -> Option<&Kind> {
        self.delta.as_ref().map(|d| &d.blocks)
    }

    fn kind_regions(&self) -> Option<&Kind> {
        self.delta.as_ref().map(|d| &d.regions)
    }

    fn tables(&mut self) -> &mut DeltaTables {
        self.delta.as_mut().expect("a base shadows nothing")
    }

    pub(crate) fn owns_op(&self, id: OpId) -> bool {
        self.kind_ops().is_some_and(|k| k.owns(id.raw()))
    }

    pub(crate) fn owns_value(&self, id: ValueId) -> bool {
        self.kind_values().is_some_and(|k| k.owns(id.number()))
    }

    pub(crate) fn owns_block(&self, id: BlockId) -> bool {
        self.kind_blocks().is_some_and(|k| k.owns(id.raw()))
    }

    pub(crate) fn owns_region(&self, id: RegionId) -> bool {
        self.kind_regions().is_some_and(|k| k.owns(id.raw()))
    }

    pub(crate) fn op_handle(&self, id: OpId) -> Option<u32> {
        self.kind_ops()
            .map_or(Some(id.raw()), |k| k.handle(id.raw()))
    }

    pub(crate) fn value_handle(&self, id: ValueId) -> Option<u32> {
        self.kind_values()
            .map_or(Some(id.number()), |k| k.handle(id.number()))
    }

    pub(crate) fn block_handle(&self, id: BlockId) -> Option<u32> {
        self.kind_blocks()
            .map_or(Some(id.raw()), |k| k.handle(id.raw()))
    }

    pub(crate) fn region_handle(&self, id: RegionId) -> Option<u32> {
        self.kind_regions()
            .map_or(Some(id.raw()), |k| k.handle(id.raw()))
    }

    /// Whether a delta holds its own copy of the base entity `id`.
    pub(crate) fn shadows_op(&self, id: OpId) -> bool {
        self.kind_ops().is_some_and(|k| k.shadows(id.raw()))
    }

    pub(crate) fn shadows_value(&self, id: ValueId) -> bool {
        self.kind_values().is_some_and(|k| k.shadows(id.number()))
    }

    pub(crate) fn shadows_block(&self, id: BlockId) -> bool {
        self.kind_blocks().is_some_and(|k| k.shadows(id.raw()))
    }

    pub(crate) fn shadows_region(&self, id: RegionId) -> bool {
        self.kind_regions().is_some_and(|k| k.shadows(id.raw()))
    }

    /// Store a copy of the base op `instance` under its own id.
    pub(crate) fn shadow_op(&mut self, instance: OpInstance) -> u32 {
        let id = instance.id;
        let handle = self.ops.insert(instance);
        self.tables().ops.record_shadow(id.raw(), handle);
        handle
    }

    pub(crate) fn shadow_value(&mut self, value: Value) -> u32 {
        let id = value.id();
        let handle = self.values.insert(value);
        self.tables().values.record_shadow(id.number(), handle);
        handle
    }

    pub(crate) fn shadow_block(&mut self, id: BlockId, block: Block) -> u32 {
        let handle = self.blocks.insert(block);
        self.tables().blocks.record_shadow(id.raw(), handle);
        handle
    }

    pub(crate) fn shadow_region(&mut self, id: RegionId, region: Region) -> u32 {
        let handle = self.regions.insert(region);
        self.tables().regions.record_shadow(id.raw(), handle);
        handle
    }

    /// The base ids this delta shadows, by kind, with the handle each copy has.
    pub(crate) fn shadowed_ops(&self) -> Vec<(OpId, u32)> {
        self.kind_ops()
            .map(Kind::shadowed)
            .unwrap_or_default()
            .into_iter()
            .map(|(raw, h)| (OpId::new(raw), h))
            .collect()
    }

    pub(crate) fn shadowed_values(&self) -> Vec<(ValueId, u32)> {
        self.kind_values()
            .map(Kind::shadowed)
            .unwrap_or_default()
            .into_iter()
            .map(|(raw, h)| (ValueId::from_number(raw), h))
            .collect()
    }

    pub(crate) fn shadowed_blocks(&self) -> Vec<(BlockId, u32)> {
        self.kind_blocks()
            .map(Kind::shadowed)
            .unwrap_or_default()
            .into_iter()
            .map(|(raw, h)| (BlockId::new(raw), h))
            .collect()
    }

    pub(crate) fn shadowed_regions(&self) -> Vec<(RegionId, u32)> {
        self.kind_regions()
            .map(Kind::shadowed)
            .unwrap_or_default()
            .into_iter()
            .map(|(raw, h)| (RegionId::new(raw), h))
            .collect()
    }

    /// Forget the shadow of base op `id`; its copy stays in the hive until the
    /// caller removes it.
    pub(crate) fn unshadow_op(&mut self, id: OpId) {
        if let Some(tables) = &mut self.delta {
            tables.ops.unshadow(id.raw());
        }
    }

    pub(crate) fn unshadow_value(&mut self, id: ValueId) {
        if let Some(tables) = &mut self.delta {
            tables.values.unshadow(id.number());
        }
    }

    pub(crate) fn unshadow_block(&mut self, id: BlockId) {
        if let Some(tables) = &mut self.delta {
            tables.blocks.unshadow(id.raw());
        }
    }

    pub(crate) fn unshadow_region(&mut self, id: RegionId) {
        if let Some(tables) = &mut self.delta {
            tables.regions.unshadow(id.raw());
        }
    }

    /// Store a new op, minting its id from the next handle.
    pub(crate) fn insert_op(&mut self, make: impl FnOnce(OpId) -> OpInstance) -> OpId {
        let id = OpId::new(
            self.kind_ops()
                .map_or(self.ops.next_handle(), Kind::next_id),
        );
        let handle = self.ops.insert(make(id));
        if let Some(tables) = &mut self.delta {
            tables.ops.local.push(handle);
        }
        id
    }

    pub(crate) fn insert_value(&mut self, make: impl FnOnce(ValueId) -> Value) -> ValueId {
        let id = ValueId::from_number(
            self.kind_values()
                .map_or(self.values.next_handle(), Kind::next_id),
        );
        let handle = self.values.insert(make(id));
        if let Some(tables) = &mut self.delta {
            tables.values.local.push(handle);
        }
        id
    }

    pub(crate) fn insert_block(&mut self, block: Block) -> BlockId {
        let id = BlockId::new(
            self.kind_blocks()
                .map_or(self.blocks.next_handle(), Kind::next_id),
        );
        let handle = self.blocks.insert(block);
        if let Some(tables) = &mut self.delta {
            tables.blocks.local.push(handle);
        }
        id
    }

    pub(crate) fn insert_region(&mut self, region: Region) -> RegionId {
        let id = RegionId::new(
            self.kind_regions()
                .map_or(self.regions.next_handle(), Kind::next_id),
        );
        let handle = self.regions.insert(region);
        if let Some(tables) = &mut self.delta {
            tables.regions.local.push(handle);
        }
        id
    }

    /// Spend one id without storing anything: a local entity erased before
    /// commit still occupies its number, so the ids after it line up.
    pub(crate) fn skip_op(&mut self) {
        self.ops.skip();
    }

    pub(crate) fn skip_value(&mut self) {
        self.values.skip();
    }

    pub(crate) fn skip_block(&mut self) {
        self.blocks.skip();
    }

    pub(crate) fn skip_region(&mut self) {
        self.regions.skip();
    }

    /// The ids this delta created, in creation order, erased ones included.
    pub(crate) fn local_ops(&self) -> Vec<OpId> {
        self.kind_ops()
            .map(Kind::locals)
            .unwrap_or_default()
            .into_iter()
            .map(OpId::new)
            .collect()
    }

    pub(crate) fn local_values(&self) -> Vec<ValueId> {
        self.kind_values()
            .map(Kind::locals)
            .unwrap_or_default()
            .into_iter()
            .map(ValueId::from_number)
            .collect()
    }

    pub(crate) fn local_blocks(&self) -> Vec<BlockId> {
        self.kind_blocks()
            .map(Kind::locals)
            .unwrap_or_default()
            .into_iter()
            .map(BlockId::new)
            .collect()
    }

    pub(crate) fn local_regions(&self) -> Vec<RegionId> {
        self.kind_regions()
            .map(Kind::locals)
            .unwrap_or_default()
            .into_iter()
            .map(RegionId::new)
            .collect()
    }

    pub(crate) fn op(&self, id: OpId) -> Option<&OpInstance> {
        self.ops.get(self.op_handle(id)?)
    }

    pub(crate) fn op_mut(&mut self, id: OpId) -> Option<&mut OpInstance> {
        let handle = self.op_handle(id)?;
        self.ops.get_mut(handle)
    }

    pub(crate) fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.get(self.block_handle(id)?)
    }

    pub(crate) fn block_mut(&mut self, id: BlockId) -> Option<&mut Block> {
        let handle = self.block_handle(id)?;
        self.blocks.get_mut(handle)
    }

    pub(crate) fn region(&self, id: RegionId) -> Option<&Region> {
        self.regions.get(self.region_handle(id)?)
    }

    pub(crate) fn region_mut(&mut self, id: RegionId) -> Option<&mut Region> {
        let handle = self.region_handle(id)?;
        self.regions.get_mut(handle)
    }

    pub(crate) fn value(&self, id: ValueId) -> Option<&Value> {
        self.values.get(self.value_handle(id)?)
    }

    pub(crate) fn value_mut(&mut self, id: ValueId) -> Option<&mut Value> {
        let handle = self.value_handle(id)?;
        self.values.get_mut(handle)
    }

    pub(crate) fn op_parent(&self, id: OpId) -> Option<Parent> {
        let handle = self.op_handle(id)?;
        slab_get(&self.op_parent, handle as usize).copied()
    }

    pub(crate) fn set_op_parent(&mut self, id: OpId, parent: Option<Parent>) {
        let handle = self.op_handle(id).expect("live op");
        match parent {
            Some(parent) => slab_put(&mut self.op_parent, handle as usize, parent),
            None => clear_slot(&mut self.op_parent, handle as usize),
        }
    }

    pub(crate) fn block_parent(&self, id: BlockId) -> Option<RegionId> {
        let handle = self.block_handle(id)?;
        slab_get(&self.block_parent, handle as usize).copied()
    }

    pub(crate) fn set_block_parent(&mut self, id: BlockId, parent: Option<RegionId>) {
        let handle = self.block_handle(id).expect("live block");
        match parent {
            Some(parent) => slab_put(&mut self.block_parent, handle as usize, parent),
            None => clear_slot(&mut self.block_parent, handle as usize),
        }
    }

    pub(crate) fn value_block(&self, id: ValueId) -> Option<BlockId> {
        let handle = self.value_handle(id)?;
        slab_get(&self.value_block, handle as usize).copied()
    }

    pub(crate) fn set_value_block(&mut self, id: ValueId, block: Option<BlockId>) {
        let handle = self.value_handle(id).expect("live value");
        match block {
            Some(block) => slab_put(&mut self.value_block, handle as usize, block),
            None => clear_slot(&mut self.value_block, handle as usize),
        }
    }

    pub(crate) fn value_region(&self, id: ValueId) -> Option<RegionId> {
        let handle = self.value_handle(id)?;
        slab_get(&self.value_region, handle as usize).copied()
    }

    pub(crate) fn set_value_region(&mut self, id: ValueId, region: Option<RegionId>) {
        let handle = self.value_handle(id).expect("live value");
        match region {
            Some(region) => slab_put(&mut self.value_region, handle as usize, region),
            None => clear_slot(&mut self.value_region, handle as usize),
        }
    }

    /// Drop `id`'s storage: its run, its attributes and its slot. The caller
    /// has unlinked its operands.
    pub(crate) fn erase_op(&mut self, id: OpId) {
        let Some(handle) = self.op_handle(id) else {
            return;
        };
        if let Some(instance) = self.ops.get(handle) {
            let (run, attrs) = (instance.run, instance.attrs);
            self.runs.free(run);
            self.attr_runs.free(attrs);
            self.ops.remove(handle);
            clear_slot(&mut self.op_parent, handle as usize);
            if let Some(epoch) = self.op_epoch.get_mut(handle as usize) {
                *epoch = ERASED;
            }
        }
        self.unshadow_op(id);
    }

    pub(crate) fn erase_block(&mut self, id: BlockId) {
        let Some(handle) = self.block_handle(id) else {
            return;
        };
        if self.blocks.get(handle).is_some() {
            self.blocks.remove(handle);
            clear_slot(&mut self.block_parent, handle as usize);
            if let Some(epoch) = self.block_epoch.get_mut(handle as usize) {
                *epoch = ERASED;
            }
        }
        self.unshadow_block(id);
    }

    pub(crate) fn erase_region(&mut self, id: RegionId) {
        let Some(handle) = self.region_handle(id) else {
            return;
        };
        if self.regions.get(handle).is_some() {
            self.regions.remove(handle);
            if let Some(epoch) = self.region_epoch.get_mut(handle as usize) {
                *epoch = ERASED;
            }
        }
        self.unshadow_region(id);
    }

    pub(crate) fn erase_value(&mut self, id: ValueId) {
        let Some(handle) = self.value_handle(id) else {
            return;
        };
        if self.values.get(handle).is_some() {
            self.values.remove(handle);
            clear_slot(&mut self.value_block, handle as usize);
            clear_slot(&mut self.value_region, handle as usize);
        }
        self.unshadow_value(id);
    }

    /// Record that `id` was created in `epoch`, so a handle minted then reads
    /// as live.
    pub(crate) fn stamp_op(&mut self, id: OpId, epoch: u32) {
        let handle = self.op_handle(id).expect("live op") as usize;
        if handle >= self.op_epoch.len() {
            self.op_epoch.resize(handle + 1, ERASED);
        }
        self.op_epoch[handle] = epoch;
    }

    pub(crate) fn stamp_block(&mut self, id: BlockId, epoch: u32) {
        let handle = self.block_handle(id).expect("live block") as usize;
        if handle >= self.block_epoch.len() {
            self.block_epoch.resize(handle + 1, ERASED);
        }
        self.block_epoch[handle] = epoch;
    }

    pub(crate) fn stamp_region(&mut self, id: RegionId, epoch: u32) {
        let handle = self.region_handle(id).expect("live region") as usize;
        if handle >= self.region_epoch.len() {
            self.region_epoch.resize(handle + 1, ERASED);
        }
        self.region_epoch[handle] = epoch;
    }

    pub(crate) fn op_epoch(&self, id: OpId) -> u32 {
        self.op_handle(id)
            .and_then(|h| self.op_epoch.get(h as usize).copied())
            .unwrap_or(ERASED)
    }

    pub(crate) fn block_epoch(&self, id: BlockId) -> u32 {
        self.block_handle(id)
            .and_then(|h| self.block_epoch.get(h as usize).copied())
            .unwrap_or(ERASED)
    }

    pub(crate) fn region_epoch(&self, id: RegionId) -> u32 {
        self.region_handle(id)
            .and_then(|h| self.region_epoch.get(h as usize).copied())
            .unwrap_or(ERASED)
    }

    /// `op`'s ports, split into the three groups the run holds back to back.
    pub(crate) fn ports(&self, op: OpId) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        let Some(instance) = self.op(op) else {
            return (Vec::new(), Vec::new(), Vec::new());
        };
        let (operands, results, regions) = (
            instance.operand_count as usize,
            instance.result_count as usize,
            instance.region_count as usize,
        );
        let entries = self.runs.entries(instance.run);
        let ids = |range: std::ops::Range<usize>| entries[range].iter().map(|e| e.id).collect();
        (
            ids(0..operands),
            ids(operands..operands + results),
            ids(operands + results..operands + results + regions),
        )
    }

    /// Replace `op`'s ports wholesale, growing its run to the next size class
    /// when the three groups no longer fit. Every operand use is relinked, so
    /// the caller does no bookkeeping of its own.
    pub(crate) fn set_ports(
        &mut self,
        op: OpId,
        operands: &[u32],
        results: &[u32],
        regions: &[u32],
    ) {
        self.unlink_operands(op);
        let needed = operands.len() + results.len() + regions.len();
        let Some(instance) = self.op(op) else {
            return;
        };
        let mut run = instance.run;
        if run.capacity() < needed {
            let live = instance.port_count();
            run = self.runs.grow(op, run, live, needed);
        }
        let entries = self.runs.entries_mut(run);
        for (entry, id) in entries
            .iter_mut()
            .zip(operands.iter().chain(results).chain(regions))
        {
            entry.reset(*id);
        }
        let instance = self.op_mut(op).expect("live op");
        instance.run = run;
        instance.operand_count = operands.len() as u16;
        instance.result_count = results.len() as u16;
        instance.region_count = regions.len() as u16;
        self.link_operands(op);
    }

    pub(crate) fn operands_of(&self, instance: &OpInstance) -> crate::operation::ValueIds {
        self.runs.entries(instance.run)[..instance.operand_count as usize]
            .iter()
            .map(|entry| ValueId::from_number(entry.id))
            .collect()
    }

    pub(crate) fn results_of(&self, instance: &OpInstance) -> crate::operation::ValueIds {
        let start = instance.operand_count as usize;
        let end = start + instance.result_count as usize;
        self.runs.entries(instance.run)[start..end]
            .iter()
            .map(|entry| ValueId::from_number(entry.id))
            .collect()
    }

    pub(crate) fn regions_of(&self, instance: &OpInstance) -> crate::operation::RegionIds {
        let start = (instance.operand_count + instance.result_count) as usize;
        let end = start + instance.region_count as usize;
        self.runs.entries(instance.run)[start..end]
            .iter()
            .map(|entry| RegionId::new(entry.id))
            .collect()
    }

    pub(crate) fn attrs_of(&self, instance: &OpInstance) -> &[NamedAttribute] {
        self.attr_runs
            .get(instance.attrs, instance.attr_count as usize)
    }

    pub(crate) fn op_attrs(&self, op: OpId) -> &[NamedAttribute] {
        self.op(op).map(|i| self.attrs_of(i)).unwrap_or(&[])
    }

    pub(crate) fn op_attrs_mut(&mut self, op: OpId) -> &mut [NamedAttribute] {
        let Some(instance) = self.op(op) else {
            return &mut [];
        };
        let (attrs, count) = (instance.attrs, instance.attr_count as usize);
        self.attr_runs.get_mut(attrs, count)
    }

    pub(crate) fn set_op_attrs(&mut self, op: OpId, attributes: Vec<NamedAttribute>) {
        let Some(instance) = self.op(op) else {
            return;
        };
        let old = instance.attrs;
        let count = attributes.len() as u16;
        let attrs = self.attr_runs.alloc(attributes);
        self.attr_runs.free(old);
        let instance = self.op_mut(op).expect("live op");
        instance.attrs = attrs;
        instance.attr_count = count;
    }

    /// Point `op`'s `index`-th operand slot at `new`, moving the slot from one
    /// value's use list to the other's.
    pub(crate) fn replace_operand_at(&mut self, op: OpId, index: usize, new: ValueId) {
        let entry = self.entry_of(op, index);
        let old = ValueId::from_number(self.runs.entry(entry).id);
        self.unlink_use(old, entry);
        self.runs.entry_mut(entry).id = new.number();
        self.link_use(new, entry);
    }

    pub(crate) fn replace_result_at(&mut self, op: OpId, index: usize, new: ValueId) {
        let offset = self.op(op).expect("live op").operand_count as usize + index;
        let entry = self.entry_of(op, offset);
        self.runs.entry_mut(entry).id = new.number();
    }

    /// Move `op`'s run to a class holding `needed` entries, if its own is too
    /// small. Entry addresses change with the move, so the operand use lists
    /// are rebuilt across it.
    fn reserve_ports(&mut self, op: OpId, needed: usize) {
        let instance = self.op(op).expect("live op");
        let (run, live) = (instance.run, instance.port_count());
        if run.capacity() >= needed {
            return;
        }
        self.unlink_operands(op);
        let grown = self.runs.grow(op, run, live, needed);
        self.op_mut(op).expect("live op").run = grown;
        self.link_operands(op);
    }

    /// Append `value` to `op`'s results. Results sit in no use list, so only
    /// the ports after the slot move.
    pub(crate) fn append_result_port(&mut self, op: OpId, value: ValueId) {
        let instance = self.op(op).expect("live op");
        let at = (instance.operand_count + instance.result_count) as usize;
        self.insert_port(op, at, value.number());
        self.op_mut(op).expect("live op").result_count += 1;
    }

    /// Put `value` at operand position `index`, shifting the operands after it
    /// along. Appending at the end is one slot write; anything else moves
    /// linked entries, so the run is rewritten and every use relinked.
    pub(crate) fn insert_operand(&mut self, op: OpId, index: usize, value: ValueId) {
        let count = self.op(op).expect("live op").operand_count as usize;
        if index == count {
            self.insert_port(op, index, value.number());
            self.op_mut(op).expect("live op").operand_count += 1;
            let entry = self.entry_of(op, index);
            self.link_use(value, entry);
            return;
        }
        let (mut operands, results, regions) = self.ports(op);
        operands.insert(index, value.number());
        self.set_ports(op, &operands, &results, &regions);
    }

    /// Drop the operand at `index`; see [`Store::insert_operand`].
    pub(crate) fn remove_operand(&mut self, op: OpId, index: usize) {
        let (mut operands, results, regions) = self.ports(op);
        operands.remove(index);
        self.set_ports(op, &operands, &results, &regions);
    }

    /// Grow or shrink the last operand segment by `delta`, where the op tracks
    /// segments: an appended or dropped value operand belongs to the trailing
    /// variadic group.
    pub(crate) fn adjust_last_segment(&mut self, op: OpId, segment_sizes: Sym, delta: i64) {
        if let Some(sizes) = self.segment_sizes_mut(op, segment_sizes)
            && let Some(AttributeValue::UInt(last)) = sizes.last_mut()
        {
            *last = (*last as i64 + delta) as u64;
        }
    }

    /// The `operand_segment_sizes` an op with a variadic group records, for
    /// editing; `None` for a fixed-arity op.
    fn segment_sizes_mut(&mut self, op: OpId, segment_sizes: Sym) -> Option<&mut [AttributeValue]> {
        match &mut self
            .op_attrs_mut(op)
            .iter_mut()
            .find(|attribute| attribute.name == segment_sizes)?
            .value
        {
            AttributeValue::Array(sizes) => Some(sizes),
            _ => None,
        }
    }

    /// Grow by one the declared operand group whose operands ended at
    /// `index` before the insert, the last such group when several end there
    /// (an empty variadic group after a fixed one). A fixed-arity op tracks
    /// no segments.
    pub(crate) fn grow_segment_ending_at(&mut self, op: OpId, segment_sizes: Sym, index: usize) {
        let Some(sizes) = self.segment_sizes_mut(op, segment_sizes) else {
            return;
        };
        let mut end = 0;
        let mut chosen = None;
        for (position, size) in sizes.iter().enumerate() {
            if let AttributeValue::UInt(size) = size {
                end += *size as usize;
                if end == index {
                    chosen = Some(position);
                }
            }
        }
        if let Some(AttributeValue::UInt(size)) =
            chosen.and_then(|position| sizes.get_mut(position))
        {
            *size += 1;
        }
    }

    /// Shrink by one the declared operand group holding the operand at
    /// `index`. A fixed-arity op tracks no segments.
    pub(crate) fn shrink_segment_holding(&mut self, op: OpId, segment_sizes: Sym, index: usize) {
        let Some(sizes) = self.segment_sizes_mut(op, segment_sizes) else {
            return;
        };
        let mut start = 0;
        for size in sizes.iter_mut() {
            if let AttributeValue::UInt(size) = size {
                if index < start + *size as usize {
                    *size -= 1;
                    return;
                }
                start += *size as usize;
            }
        }
    }

    /// Insert `id` at port position `at`, shifting the ports after it along.
    /// Callers pass an `at` no earlier than the end of the operand group, so
    /// the shifted entries are results and regions, which sit in no use list.
    pub(crate) fn insert_port(&mut self, op: OpId, at: usize, id: u32) {
        let count = self.op(op).expect("live op").port_count();
        debug_assert!(at >= self.op(op).expect("live op").operand_count as usize);
        self.reserve_ports(op, count + 1);
        let run = self.op(op).expect("live op").run;
        let entries = self.runs.entries_mut(run);
        entries[at..=count].rotate_right(1);
        entries[at].reset(id);
    }

    /// Record every operand slot of `op` under the value it holds.
    pub(crate) fn link_operands(&mut self, op: OpId) {
        for (index, value) in self.operand_slots(op) {
            let entry = self.entry_of(op, index);
            self.link_use(value, entry);
        }
    }

    /// Forget every operand slot of `op`. Reads `op`'s storage, so it runs
    /// before the op is erased or its operands are rewritten wholesale.
    pub(crate) fn unlink_operands(&mut self, op: OpId) {
        for (index, value) in self.operand_slots(op) {
            let entry = self.entry_of(op, index);
            self.unlink_use(value, entry);
        }
    }

    /// `op`'s operands paired with their slot indices, copied out so the run
    /// they live in can be borrowed mutably.
    fn operand_slots(&self, op: OpId) -> Vec<(usize, ValueId)> {
        let Some(instance) = self.op(op) else {
            return Vec::new();
        };
        self.runs.entries(instance.run)[..instance.operand_count as usize]
            .iter()
            .enumerate()
            .map(|(index, entry)| (index, ValueId::from_number(entry.id)))
            .collect()
    }

    /// The address of `op`'s `index`-th port.
    fn entry_of(&self, op: OpId, index: usize) -> EntryId {
        self.op(op).expect("live op").run.entry(index)
    }

    /// The op and port index an entry address names.
    pub(crate) fn locate(&self, entry: EntryId) -> (OpId, usize) {
        let op = self.runs.entry(entry).owner;
        let start = self.op(op).expect("live op").run.start();
        (op, entry.raw() as usize - start)
    }

    /// Splice `entry` onto the front of the use list of `value`.
    fn link_use(&mut self, value: ValueId, entry: EntryId) {
        if value.index() >= self.first_use.len() {
            self.first_use.resize(value.index() + 1, NO_ENTRY);
        }
        let head = self.first_use[value.index()];
        {
            let slot = self.runs.entry_mut(entry);
            slot.prev = NO_ENTRY;
            slot.next = head;
        }
        if head != NO_ENTRY {
            self.runs.entry_mut(EntryId::from_raw(head)).prev = entry.raw();
        }
        self.first_use[value.index()] = entry.raw();
    }

    /// Splice `entry` out of the use list of `value`.
    fn unlink_use(&mut self, value: ValueId, entry: EntryId) {
        if value.index() >= self.first_use.len() {
            return;
        }
        let (prev, next) = {
            let slot = self.runs.entry(entry);
            (slot.prev, slot.next)
        };
        if prev == NO_ENTRY {
            if self.first_use[value.index()] != entry.raw() {
                return;
            }
            self.first_use[value.index()] = next;
        } else {
            self.runs.entry_mut(EntryId::from_raw(prev)).next = next;
        }
        if next != NO_ENTRY {
            self.runs.entry_mut(EntryId::from_raw(next)).prev = prev;
        }
        let slot = self.runs.entry_mut(entry);
        slot.prev = NO_ENTRY;
        slot.next = NO_ENTRY;
    }

    /// The head of `value`'s use list, or [`NO_ENTRY`] if nothing names it.
    pub(crate) fn use_head(&self, value: ValueId) -> u32 {
        self.first_use
            .get(value.index())
            .copied()
            .unwrap_or(NO_ENTRY)
    }

    /// The entries naming `value`, newest first.
    pub(crate) fn use_entries(&self, value: ValueId) -> impl Iterator<Item = EntryId> + '_ {
        let mut current = self.use_head(value);
        std::iter::from_fn(move || {
            let entry = (current != NO_ENTRY).then(|| EntryId::from_raw(current))?;
            current = self.runs.entry(entry).next;
            Some(entry)
        })
    }

    /// Every operand slot naming `value`, oldest first: the list is threaded
    /// front-first, so the walk is reversed to restore the order the slots were
    /// recorded in.
    pub(crate) fn uses(&self, value: ValueId) -> Vec<crate::Use> {
        let mut uses: Vec<crate::Use> = self
            .use_entries(value)
            .map(|entry| {
                let (op, index) = self.locate(entry);
                crate::Use::new(op, index)
            })
            .collect();
        uses.reverse();
        uses
    }

    /// Move every use of `old` onto `new`, in this store's lists, keeping the
    /// order they were recorded in.
    pub(crate) fn move_uses(&mut self, old: ValueId, new: ValueId) -> Vec<OpId> {
        let entries: Vec<EntryId> = self.use_entries(old).collect();
        let mut owners = Vec::with_capacity(entries.len());
        for entry in entries.into_iter().rev() {
            self.unlink_use(old, entry);
            self.runs.entry_mut(entry).id = new.number();
            self.link_use(new, entry);
            owners.push(self.runs.entry(entry).owner);
        }
        owners
    }

    pub(crate) fn recycle(&mut self) {
        self.runs.recycle();
        self.attr_runs.recycle();
    }

    /// Bytes the store holds beyond its entity hives: the `Vec`s blocks and
    /// regions own.
    pub(crate) fn owned_heap_bytes(&self) -> (usize, usize) {
        let blocks: usize = self
            .blocks
            .handles()
            .filter_map(|handle| self.blocks.get(handle))
            .map(Block::heap_bytes)
            .sum();
        let regions: usize = self
            .regions
            .handles()
            .filter_map(|handle| self.regions.get(handle))
            .map(Region::heap_bytes)
            .sum();
        (blocks, regions)
    }

    /// Bytes the store holds in every arena and hive.
    pub(crate) fn bytes(&self) -> usize {
        let (blocks, regions) = self.owned_heap_bytes();
        self.ops.bytes()
            + self.values.bytes()
            + self.blocks.bytes()
            + self.regions.bytes()
            + self.runs.census().1
            + self.attr_runs.census().1
            + blocks
            + regions
    }
}
