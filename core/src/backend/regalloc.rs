//! Target-independent register allocation.
//!
//! The allocator works on machine IR produced by instruction selection, where a
//! register operand is an SSA operand or result whose type is its register
//! class. It lowers fixed registers and the calling convention to pins and
//! copies, spills once (Belady per block), splits live ranges before
//! constrained instructions, then assigns registers and coalesces copies in one
//! PBQP solve ([`crate::backend::pbqp`]). It then places the copies that edges
//! and split points still need, erases copies whose ends share a register, and
//! inserts the frame: callee-saved saves, stack-argument loads and the stack
//! adjustment. Operands keep naming values: the chosen registers are written
//! onto the function's `asm.symbol` as a [`crate::backend::RegAssignment`],
//! which assembly printing and encoding read.
//!
//! Register files come from [`RegisterInfo`]; allocation order and calling
//! convention policy come from the selected [`crate::backend::abi::AbiInfo`].

use std::collections::HashMap;
use std::rc::Rc;

use tir::attributes::AttributeValue;
use tir::{
    AnalysisManager, BlockId, Context, OpId, Operation, OperationRef, Pass, PassError, PassTarget,
    ValueId,
};

use crate::backend::abi;
use crate::backend::constmat::rematerializable;
use crate::backend::liveness::{self, Classes, PhysReg};
use crate::backend::pbqp::{self, INF_COST};
use crate::backend::prealloc;
use crate::backend::registers::fresh_reg;
use crate::backend::{
    RegSlot, SymbolOp, VirtualCallOp, VirtualIndirectCallOp, VirtualReturnOp, symbol_body_blocks,
};
use crate::ptr::AllocaOp;

mod belady;
mod constraints;
mod copies;
mod db;
mod live;
mod split;

use db::RegAllocDb;

/// Architectural metadata for one register class.
#[derive(Debug, Clone)]
pub struct RegClassInfo {
    pub name: &'static str,
    /// The dialect whose TMDL description declares this class. Names the
    /// register-class type a value of this class carries (`!x86_64.GPR`).
    pub dialect: &'static str,
    /// The physical register file this class draws from — the root of its TMDL
    /// inheritance chain. Classes that share a file (e.g. AArch64 `GPR` and
    /// `GPRsp`, which differ only in whether encoding 31 is `xzr` or `sp`) name the
    /// same physical register at a given index, so the allocator treats their
    /// indices as aliases. A standalone class is its own file.
    pub file: &'static str,
    /// The file indices this class can encode, ascending. A class that views only
    /// part of its file (x86 `GPR32low`, whose REX-free encodings reach eax..edi)
    /// lists just those indices, and the allocator hands out nothing else.
    pub registers: &'static [u16],
    /// How many consecutive file indices one register of this class covers.
    /// 1 for ordinary classes; an RVV LMUL>1 group class covers 2/4/8 (e.g.
    /// `VRM2` index 8 is the architectural pair v8..v9).
    pub group_width: u16,
    /// Where this class's architectural view sits in its storage element and
    /// whether a write merges into it (see [`RegisterView`]).
    pub view: RegisterView,
    /// Renders one of this class's registers by encoding index, so a physical
    /// register prints as its assembly name wherever it appears.
    pub print_name: crate::backend::asm_desc::RegisterNamePrinter,
}

/// Two class records describe the same class when their architecture does; the
/// name printer is a rendering detail and is not compared.
impl PartialEq for RegClassInfo {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.dialect == other.dialect
            && self.file == other.file
            && self.registers == other.registers
            && self.group_width == other.group_width
            && self.view == other.view
    }
}

impl Eq for RegClassInfo {}

/// The [`RegClassInfo::print_name`] of a class with no assembly names, used by
/// tests and by classes a target does not print.
pub fn no_register_name(_index: u16, _prefer_abi: bool) -> Option<String> {
    None
}

/// A handle to a register class: a pointer to its `'static` [`RegClassInfo`].
///
/// Register classes are per-dialect statics emitted by the TMDL backend — the
/// generated `RegClass::X.id()` and `register_info().classes` point at the same
/// table — so a class's identity is the identity of that pointer. Equality and
/// hashing are by pointer; ordering is by name so codegen that sorts physical
/// registers stays deterministic across builds. Derefs to [`RegClassInfo`], so a
/// its architectural properties read directly through the handle.
#[derive(Clone, Copy)]
pub struct RegClassId(&'static RegClassInfo);

impl RegClassId {
    pub const fn new(info: &'static RegClassInfo) -> Self {
        RegClassId(info)
    }

    pub fn info(self) -> &'static RegClassInfo {
        self.0
    }

    pub fn name(self) -> &'static str {
        self.0.name
    }

    /// The dialect this class belongs to (see [`RegClassInfo::dialect`]).
    pub fn dialect(self) -> &'static str {
        self.0.dialect
    }

    /// The physical register file this class draws from (see [`RegClassInfo::file`]).
    pub fn file(self) -> &'static str {
        self.0.file
    }

    /// Whether this class can encode file index `index` (see
    /// [`RegClassInfo::registers`]).
    pub fn contains(self, index: u16) -> bool {
        self.0.registers.contains(&index)
    }

    /// Whether both classes name the same physical registers at equal indices: one
    /// file, one view offset (an x86 high-byte class views its file at bit 8 and
    /// shares no register with an offset-0 class) and one group width. Only then
    /// can a value be constrained by both at once.
    pub fn shares_view_with(self, other: RegClassId) -> bool {
        self.file() == other.file()
            && self.view.bit_offset == other.view.bit_offset
            && self.group_width == other.group_width
    }

    /// Whether every register this class can encode is also encodable by `other`
    /// through the same architectural view — so a value constrained to both must
    /// come from this, the narrower, class.
    pub fn is_subclass_of(self, other: RegClassId) -> bool {
        self.shares_view_with(other) && self.registers.iter().all(|index| other.contains(*index))
    }

    /// The span of file indices a register of this class at `index` covers: its
    /// file, start index, and group width (RVV LMUL>1 groups cover 2/4/8).
    pub fn span(self, index: u16) -> (&'static str, u16, u16) {
        (self.0.file, index, self.0.group_width.max(1))
    }

    /// Whether a register of this class at `index` overlaps `other` at
    /// `other_index`: same file and intersecting index spans. For width-1 classes
    /// this is file+index equality; a group register (RVV `VRM2` v8..v9) overlaps
    /// every register it covers, and aliasing classes over one file (`GPR`/`GPRsp`
    /// index 7) overlap at equal indices.
    pub fn overlaps(self, index: u16, other: RegClassId, other_index: u16) -> bool {
        let (fa, sa, wa) = self.span(index);
        let (fb, sb, wb) = other.span(other_index);
        fa == fb && sa < sb + wb && sb < sa + wa
    }
}

impl std::ops::Deref for RegClassId {
    type Target = RegClassInfo;
    fn deref(&self) -> &RegClassInfo {
        self.0
    }
}

impl PartialEq for RegClassId {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.0, other.0)
    }
}

impl Eq for RegClassId {}

impl std::hash::Hash for RegClassId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::ptr::hash(self.0, state);
    }
}

impl PartialOrd for RegClassId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RegClassId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.name.cmp(other.0.name)
    }
}

impl std::fmt::Debug for RegClassId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RegClassId({})", self.0.name)
    }
}

/// How a register class's architectural view maps onto its storage element.
/// `bit_offset` is where the view starts within the element (x86 high-byte `ah`
/// begins at bit 8); `merge` preserves the element's untouched bits on write
/// (x86 8/16-bit writes) rather than zero-extending the value across the whole
/// element (the default, matching x86 32-bit and AArch64 scalar-FP writes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegisterView {
    pub bit_offset: u32,
    pub merge: bool,
}

/// The register file of a target: every allocatable (and reserved) register class,
/// keyed by the class name used in [`RegisterAttr`] operands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterInfo {
    pub classes: &'static [RegClassInfo],
}

impl RegisterInfo {
    pub fn class(&self, name: &str) -> Option<RegClassId> {
        self.classes
            .iter()
            .find(|c| c.name == name)
            .map(RegClassId::new)
    }

    /// Whether two physical registers overlap: same file and intersecting index
    /// spans. A group register (RVV `VRM2` v8..v9) overlaps every register it
    /// covers; aliasing classes over one file (`GPR`/`GPRsp` index 7) overlap at
    /// equal indices. Delegates to [`RegClassId::overlaps`].
    pub fn phys_overlap(&self, a: &PhysReg, b: &PhysReg) -> bool {
        a.0.overlaps(a.1, b.0, b.1)
    }

    pub fn default_integer_class(&self, abi: &crate::backend::abi::AbiInfo) -> Option<RegClassId> {
        self.default_class(abi, crate::backend::abi::ValueKind::Int)
    }

    /// The class a value of `kind` lives in when no instruction naming it has
    /// pinned a narrower one: the file the ABI passes such values through.
    pub fn default_class(
        &self,
        abi: &crate::backend::abi::AbiInfo,
        kind: crate::backend::abi::ValueKind,
    ) -> Option<RegClassId> {
        abi.args
            .iter()
            .find(|sequence| sequence.kind == kind)
            .and_then(|sequence| sequence.regs.first())
            .map(|register| register.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegAllocError {
    /// A virtual register has no register left to take (e.g. an over-constrained
    /// pre-coloring). Carries the offending vreg id.
    Infeasible(u32),
    /// A virtual register is referenced through register classes that cannot both
    /// be honored (see [`Classes::class_conflicts`]). Carries the offending vreg.
    ClassConflict(u32),
}

// Assign every value of `db` a register in one PBQP solve (paper § 3).
//
// Each value is a node whose alternatives are the allocatable registers of its
// class. Its color vector forbids what it is live across a clobber of and
// prefers a register a copy moves it to or from; interference rows forbid two
// values one register, and affinity rows price the copy their ends would need
// apart. A value at a constrained instruction keeps only the registers
// `restricted` leaves it (paper § 4.3). Spilling ran before, so every point
// fits its file, and the solve in `order`, a reversed perfect elimination
// order with each constrained instruction's values grouped where the first of
// them appears, finds a color for every node.
fn allocate_with_affinities(
    info: &RegisterInfo,
    abi: &crate::backend::abi::AbiInfo,
    db: &RegAllocDb,
    classes: &Classes,
    precolor: &HashMap<u32, PhysReg>,
    restricted: &HashMap<u32, constraints::Set>,
    order: &[u32],
) -> Result<HashMap<u32, PhysReg>, RegAllocError> {
    let mut node_of: HashMap<u32, usize> = HashMap::with_capacity(order.len());
    for &id in order {
        let next = node_of.len();
        node_of.entry(id).or_insert(next);
    }
    let mut nodes: Vec<u32> = vec![0; node_of.len()];
    for (&id, &node) in &node_of {
        nodes[node] = id;
    }
    let number = |id: u32| db.value_of(id).number();

    let mut forbidden: HashMap<usize, Vec<PhysReg>> = HashMap::new();
    let forbid = db.table(db::FORBID);
    for (&register, &value) in forbid.column(1).iter().zip(forbid.column(2)) {
        if let Some(&node) = node_of.get(&value) {
            forbidden
                .entry(node)
                .or_default()
                .push(db.registers[register as usize]);
        }
    }
    let mut preferences: HashMap<usize, Vec<(PhysReg, u64)>> = HashMap::new();
    let prefer = db.table(db::PREFER);
    for ((&weight, &value), &register) in prefer
        .column(1)
        .iter()
        .zip(prefer.column(2))
        .zip(prefer.tags())
    {
        if let Some(&node) = node_of.get(&value) {
            let register = db.registers[register as usize];
            preferences
                .entry(node)
                .or_default()
                .push((register, u64::from(weight)));
        }
    }

    let default_class = info.default_integer_class(abi);
    // Each node's registers, resolved to physical registers. Nodes of one class
    // and constraint share a list, and the interference matrix between two
    // lists is built once.
    let mut orders: HashMap<RegClassId, Vec<PhysReg>> = HashMap::new();
    let mut lists: Vec<Vec<PhysReg>> = Vec::new();
    let mut list_ids: HashMap<Vec<PhysReg>, usize> = HashMap::new();
    let mut node_list = Vec::with_capacity(nodes.len());
    let mut problem = pbqp::Problem::default();
    for (node, &id) in nodes.iter().enumerate() {
        let vreg = number(id);
        let class = resolve_class(classes, precolor, default_class, vreg)?;
        // A vreg referenced through several classes over one view (an x86 value
        // that is both a REX-free operand and a SIB index) is allocatable only
        // from the indices all of them encode.
        let allowed = classes.allowed_indices.get(&vreg);
        let restricted = restricted.get(&vreg);
        let registers: Vec<PhysReg> = orders
            .entry(class)
            .or_insert_with(|| allocation_order(abi, class))
            .iter()
            .copied()
            .filter(|(_, index)| allowed.is_none_or(|allowed| allowed.contains(index)))
            .filter(|&(_, index)| restricted.is_none_or(|&set| set >> index & 1 == 1))
            .collect();
        let list = *list_ids.entry(registers).or_insert_with_key(|registers| {
            lists.push(registers.clone());
            lists.len() - 1
        });
        node_list.push(list);
        let registers = &lists[list];
        let forbidden = forbidden.get(&node).map_or(&[][..], Vec::as_slice);
        let mut costs = node_costs(info, registers, precolor.get(&vreg), forbidden);
        for &(preferred, weight) in preferences.get(&node).into_iter().flatten() {
            for (cost, actual) in costs.iter_mut().zip(registers) {
                if *cost < INF_COST && actual.0.span(actual.1) != preferred.0.span(preferred.1) {
                    *cost = cost.saturating_add(weight).min(INF_COST);
                }
            }
        }
        if costs.iter().all(|&c| c >= INF_COST) {
            return Err(RegAllocError::Infeasible(vreg));
        }
        problem.add_node(costs);
    }

    let mut matrices: HashMap<(usize, usize), Option<Rc<pbqp::Matrix>>> = HashMap::new();
    let interfere = db.table(db::INTERFERE);
    for (&a, &b) in interfere.column(1).iter().zip(interfere.column(2)) {
        let (Some(&u), Some(&v)) = (node_of.get(&a), node_of.get(&b)) else {
            continue;
        };
        // The problem keeps an edge's rows on its lower node; ordering the ends
        // here spares it a transposed copy of the shared matrix.
        let (u, v) = (u.min(v), u.max(v));
        let (lu, lv) = (node_list[u], node_list[v]);
        let matrix = matrices
            .entry((lu, lv))
            .or_insert_with(|| interference_matrix(info, &lists[lu], &lists[lv]).map(Rc::new));
        if let Some(matrix) = matrix {
            problem.add_edge(u, v, matrix.clone());
        }
    }
    let affinity = db.table(db::AFFINITY);
    for ((&weight, &a), &b) in affinity
        .column(1)
        .iter()
        .zip(affinity.column(2))
        .zip(affinity.column(3))
    {
        let (Some(&u), Some(&v)) = (node_of.get(&a), node_of.get(&b)) else {
            continue;
        };
        if u != v {
            let (u, v) = (u.min(v), u.max(v));
            let matrix = affinity_matrix(
                &lists[node_list[u]],
                &lists[node_list[v]],
                u64::from(weight),
            );
            problem.add_edge(u, v, Rc::new(matrix));
        }
    }

    if crate::memstats::enabled() {
        crate::memstats::pbqp_census(
            "regalloc",
            problem.node_count(),
            problem.edge_count(),
            problem.matrix_bytes(),
        );
    }
    crate::backend::pbqp_dump::dump(
        crate::backend::pbqp_dump::PbqpTaskKind::RegAlloc,
        |w, kind| problem.write_json(w, kind),
    );

    let ranked: Vec<usize> = (0..nodes.len()).collect();
    let solution = problem.solve(&ranked);
    if let Some(&node) = solution.infeasible.first() {
        return Err(RegAllocError::Infeasible(number(nodes[node])));
    }
    Ok(nodes
        .iter()
        .enumerate()
        .map(|(node, &id)| (number(id), lists[node_list[node]][solution.choices[node]]))
        .collect())
}

// The values of `db` in coloring order, a reversed perfect elimination order
// of the SSA interference graph (paper § 2.1, § 3.4): blocks in a pre-order
// walk of the dominator tree, each block's parameters and then its
// definitions in program order. A definition comes after every value live
// at it.
fn coloring_order(context: &Context, db: &RegAllocDb, walk: &[usize]) -> Vec<u32> {
    let mut order = Vec::new();
    for &block in walk {
        order.extend_from_slice(&db.params[block]);
        for &op in &db.ops[block] {
            for value in liveness::op_regs(&context.get_op(op)).defs {
                if let Some(id) = db.value_id(value) {
                    order.push(id);
                }
            }
        }
    }
    order
}

fn affinity_matrix(left: &[PhysReg], right: &[PhysReg], weight: u64) -> pbqp::Matrix {
    let mut matrix = pbqp::Matrix::zero(left.len(), right.len());
    for (i, l) in left.iter().enumerate() {
        for (j, r) in right.iter().enumerate() {
            if l.0.span(l.1) != r.0.span(r.1) {
                matrix.set(i, j, weight);
            }
        }
    }
    matrix
}

// Determine the register class a virtual register must be allocated from: the
// intersection of the class its operands constrain it to and its pinned
// register's class, falling back to the target's default integer class.
//
// A pin says *which* register the value lives in; the operand class says which
// registers the instruction that reads it can encode. Both hold, so the narrower
// wins — widening to the pin's class would hand out an index the encoding drops,
// silently naming a different register. A pin that the narrow class cannot encode
// leaves no legal color and fails allocation loudly.
fn resolve_class(
    classes: &Classes,
    precolor: &HashMap<u32, PhysReg>,
    default_class: Option<RegClassId>,
    vreg: u32,
) -> Result<RegClassId, RegAllocError> {
    if classes.class_conflicts.contains_key(&vreg) {
        return Err(RegAllocError::ClassConflict(vreg));
    }
    let constraint = classes.vreg_class.get(&vreg).copied();
    let pinned = precolor.get(&vreg).map(|(class, _)| *class);
    match (constraint, pinned) {
        (Some(constraint), Some(pinned)) if constraint.is_subclass_of(pinned) => Ok(constraint),
        (_, Some(pinned)) => Ok(pinned),
        (Some(constraint), None) => Ok(constraint),
        (None, None) => default_class.ok_or(RegAllocError::Infeasible(vreg)),
    }
}

fn node_costs(
    info: &RegisterInfo,
    registers: &[PhysReg],
    pinned: Option<&PhysReg>,
    forbidden: &[PhysReg],
) -> Vec<u64> {
    registers
        .iter()
        .map(|p| {
            if let Some(target) = pinned {
                // Pinned vregs accept only their target register. Compare by
                // physical identity so a precolor reached through one class
                // (e.g. an ABI `GPR` arg) matches an alternative in an aliasing
                // class (`GPRsp`). A pin on a register the vreg is also live
                // across a clobber of (e.g. an incoming argument that survives
                // a call) is unsatisfiable: every alternative goes infinite so
                // allocation fails loudly instead of silently producing a
                // clobbered value.
                let conflict = forbidden.iter().any(|f| info.phys_overlap(f, target));
                return if !conflict && p.0.span(p.1) == target.0.span(target.1) {
                    0
                } else {
                    INF_COST
                };
            }
            if forbidden.iter().any(|f| info.phys_overlap(f, p)) {
                INF_COST
            } else {
                0
            }
        })
        .collect()
}

fn allocation_order(abi: &crate::backend::abi::AbiInfo, class: RegClassId) -> Vec<PhysReg> {
    let mut result = Vec::new();
    for register in abi.caller_saved.iter().chain(abi.callee_saved) {
        if register.0.file() == class.file()
            && class.contains(register.1)
            && register.1 % class.group_width.max(1) == 0
        {
            let candidate = (class, register.1);
            // Frames are SP-relative. The ABI's optional frame pointer is an
            // ordinary callee-saved register in this allocator.
            let is_role = std::iter::once(&abi.sp)
                .chain(abi.ra.iter())
                .chain(abi.reserved)
                .any(|role| candidate.0.overlaps(candidate.1, role.0, role.1));
            if !is_role && !result.contains(&candidate) {
                result.push(candidate);
            }
        }
    }
    result
}

// Build the interference matrix between two register lists, or `None` if they
// share no physical register (so they can never conflict and no edge is
// needed). Two alternatives conflict when they overlap: the same register
// through aliasing classes (`GPR`/`GPRsp` index 7), or a group register
// covering another (`VRM2` v8..v9 vs `VR` v9).
fn interference_matrix(
    info: &RegisterInfo,
    left: &[PhysReg],
    right: &[PhysReg],
) -> Option<pbqp::Matrix> {
    let mut matrix = pbqp::Matrix::zero(left.len(), right.len());
    let mut any = false;
    for (i, l) in left.iter().enumerate() {
        for (j, r) in right.iter().enumerate() {
            if info.phys_overlap(l, r) {
                matrix.set(i, j, INF_COST);
                any = true;
            }
        }
    }
    any.then_some(matrix)
}

// ---------------------------------------------------------------------------
// Target interface + allocation pass
// ---------------------------------------------------------------------------

/// Target-specific knowledge the allocation pass needs but cannot derive from the
/// register file alone: the spill frame layout and the instructions that move a
/// register to and from a stack slot. The register file itself comes from
/// [`TargetRegAlloc::register_info`], which backends wire to their generated
/// `register_info()`.
pub trait TargetRegAlloc: Send + Sync {
    fn register_info(&self) -> RegisterInfo;

    /// Whether cloning this symbol materialization at another location, with
    /// fresh independent relocation, produces the identical value.
    fn is_symbol_materialization(&self, _op: &tir::OpHandle) -> bool {
        false
    }

    /// Bytes needed to spill one register, allocated at the ABI slot alignment.
    fn spill_slot_size(&self, _class: RegClassId, abi: &crate::backend::abi::AbiInfo) -> u32 {
        abi.stack.slot_size
    }

    /// Build a store of `value` (of class `class`) to `[frame + offset]`.
    fn emit_spill_store(
        &self,
        context: &Context,
        value: ValueId,
        class: RegClassId,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Build a load from `[frame + offset]` defining `value`, which the caller
    /// has already created with the class's type.
    fn emit_spill_reload(
        &self,
        context: &Context,
        value: ValueId,
        class: RegClassId,
        frame: &PhysReg,
        offset: i64,
    ) -> Box<dyn Operation>;

    /// Build a register-to-register copy of `src` into `dst` (both of class
    /// `class`). Either end is a value the copy defines or reads, or a physical
    /// register named directly. Only reached on targets whose instructions have
    /// tied (two-address) operands or a calling convention, so the default
    /// panics.
    fn emit_copy(
        &self,
        context: &Context,
        class: RegClassId,
        dst: crate::backend::RegSlot,
        src: crate::backend::RegSlot,
    ) -> Box<dyn Operation> {
        let _ = (context, class, dst, src);
        unimplemented!("this target has tied operands but no copy emitter")
    }

    /// Prologue instructions reserving a frame of `size` bytes (e.g. `addi sp, sp,
    /// -size`) and saving the callee-saved registers the allocation used, each at
    /// its reserved `[frame + offset]` slot. Inserted at the top of the entry block
    /// when the frame is non-empty.
    fn emit_prologue(
        &self,
        _context: &Context,
        _abi: &crate::backend::abi::AbiInfo,
        _size: u32,
        _saves: &[(PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    /// Epilogue instructions restoring the saved callee-saved registers and
    /// releasing the frame, inserted before each terminator.
    fn emit_epilogue(
        &self,
        _context: &Context,
        _abi: &crate::backend::abi::AbiInfo,
        _size: u32,
        _saves: &[(PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    /// Build instruction(s) that materialize `[frame + offset]` into the virtual
    /// register `dst`.
    fn emit_frame_address(
        &self,
        _context: &Context,
        _dst: ValueId,
        class: RegClassId,
        _frame: &PhysReg,
        _offset: i64,
    ) -> Result<Vec<Box<dyn Operation>>, PassError> {
        Err(PassError::InvalidRuleSet(format!(
            "stack allocation addresses are not supported for register class {}",
            class.name()
        )))
    }
}

/// A register allocation pass. Runs on each `asm.symbol` op produced by instruction
/// selection, after Buchwald, Zwinkau and Bersch, "SSA-based Register
/// Allocation with PBQP" (CC 2011): block arguments stay SSA, liveness and
/// interference are relations of one engine database per function, spilling
/// runs once before assignment, and one PBQP solve in a reversed perfect
/// elimination order assigns every value and coalesces copies. Where every
/// value went is recorded in the symbol's [`crate::backend::RegAssignment`].
pub struct RegisterAllocationPass {
    target: Box<dyn TargetRegAlloc>,
    abi: &'static crate::backend::abi::AbiInfo,
    rules: live::Rules,
    caches: tir_relational::saturate::Caches,
    hierarchy: constraints::Hierarchy,
    /// The database the last allocation read, for the next to fill again.
    spare: Option<tir_relational::Database<(), 1>>,
}

impl RegisterAllocationPass {
    pub fn with_abi(
        target: Box<dyn TargetRegAlloc>,
        abi: &'static crate::backend::abi::AbiInfo,
    ) -> Self {
        let hierarchy = constraints::Hierarchy::new(&target.register_info(), abi);
        Self {
            target,
            abi,
            rules: live::Rules::new(),
            caches: Default::default(),
            hierarchy,
            spare: None,
        }
    }
}

impl Pass for RegisterAllocationPass {
    fn name(&self) -> &'static str {
        "register-allocation"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<SymbolOp>()
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let info = self.target.register_info();
        let blocks = symbol_body_blocks(context, op.op());
        if blocks.is_empty() {
            return Ok(());
        }

        let mut precolor = self.lower_fixed_registers(context, op, &blocks)?;
        let mut scan = BodyScan::of(context, &blocks)?;
        let dominance = crate::backend::machine_cfg::dominance(&blocks, &scan.successors);
        // A copy left standing runs as often as its block does.
        let depths = crate::backend::machine_cfg::loop_depths(&blocks, &dominance);
        let weights: Vec<u64> = blocks
            .iter()
            .map(|block| 10u64.saturating_pow(depths.get(block).copied().unwrap_or(0)))
            .collect();

        let mut frame = FramePlan::new(self.abi);
        frame.reserve_outgoing(scan.outgoing_size);
        let stack_allocas = scan.stack_allocas(&mut frame);
        self.rematerialize_stack_allocas(context, &blocks, &scan, &stack_allocas)?;
        // Rematerializing left the allocations naming nothing, so they go now
        // rather than after allocation: a definition the function still holds is
        // a live range the allocator may pick to spill, and the spill code it
        // would write names a value this erasure is about to retire.
        erase_stack_allocas(context, &stack_allocas)?;

        let mut db = self.database(context, &blocks, &scan, &weights, &precolor)?;
        let mut classes = Classes::of(context, &blocks);
        let mut remat = rematerializable(context, &blocks, &classes, self.target.as_ref());
        // One spill batch, then the function is read again (paper § 2.1).
        let spilled = belady::spill(
            self, context, &blocks, &dominance, &classes, &remat, &mut db, &mut frame,
        )?;
        if spilled {
            self.spare = Some(db.into_database());
            db = self.database(context, &blocks, &scan, &weights, &precolor)?;
        }
        // Every live range is split before a constrained instruction (paper
        // § 4). After spilling: only what is in registers there is split.
        let mut constrained = self.hierarchy.constrained_ops(context, &db, &precolor);
        scan.parallel = split::split(
            self,
            context,
            &blocks,
            &scan.successors,
            &dominance,
            &mut db,
            &constrained,
            &mut precolor,
        )?;
        if !scan.parallel.is_empty() {
            self.spare = Some(db.into_database());
            db = self.database(context, &blocks, &scan, &weights, &precolor)?;
            constrained = self.hierarchy.constrained_ops(context, &db, &precolor);
        }
        if spilled || !scan.parallel.is_empty() {
            classes = Classes::of(context, &blocks);
            remat = rematerializable(context, &blocks, &classes, self.target.as_ref());
        }
        let db = self.graph(db);
        let walk = dominance.preorder();
        let restricted = constraints::restrict(
            &self.hierarchy,
            context,
            &db,
            &constrained,
            &walk,
            &classes,
            &precolor,
            info.default_integer_class(self.abi),
        )?;
        let order = constraints::color_order(&restricted, &coloring_order(context, &db, &walk));
        let mut assignment = allocate_with_affinities(
            &info,
            self.abi,
            &db,
            &classes,
            &precolor,
            &restricted.sets,
            &order,
        )
        .map_err(|e| PassError::InvalidRuleSet(format!("register allocation failed: {e:?}")))?;
        let constants = copies::constants(context, &blocks, remat);
        copies::place_edge_copies(
            context,
            self.target.as_ref(),
            self.abi,
            &db,
            &constants,
            &mut assignment,
            &mut frame,
        )?;
        copies::place_parallel_copies(
            context,
            self.target.as_ref(),
            self.abi,
            &db,
            &constants,
            &mut assignment,
            &mut frame,
        )?;
        self.spare = Some(db.into_database());

        for &(copy, _) in &scan.copies {
            if !context.has_operation(copy) {
                continue;
            }
            let physical = |slot| match slot {
                RegSlot::Value(value) => assignment.get(&value.number()).copied(),
                RegSlot::Phys(phys) => Some(phys),
            };
            let erasable = allocation_copy_endpoints(context, copy).is_some_and(|(src, dst)| {
                matches!((physical(src), physical(dst)), (Some(src), Some(dst))
                    if src.0.span(src.1) == dst.0.span(dst.1))
            });
            if erasable {
                // Keep the assigned destination value for its users and later
                // two-address definitions, including spill stores.
                context.erase_op_keeping_results(&op_ref_in(context, copy))?;
            } else if !copies::replay_constant_copy(context, &constants, &assignment, copy)? {
                strip_attr(context, copy, prealloc::COALESCABLE_COPY_ATTR);
                strip_attr(context, copy, crate::backend::FULL_REGISTER_COPY_ATTR);
            }
        }
        // Preserve the callee-saved registers the allocation used for this
        // function's caller. Frame-slot targets reserve a normal frame slot;
        // push/pop targets keep saves outside the stable frame area.
        let saves = callee_saved_slots(&assignment, &mut frame, self.abi.callee_saved);

        let frame_size = frame.prologue_adjustment(scan.has_calls, saves.len());
        let stack_args = collect_stack_arg_loads(context, &blocks)?;
        self.insert_incoming_stack_arg_loads(
            context,
            &blocks,
            &assignment,
            &stack_args,
            &frame,
            frame_size,
            saves.len(),
        )?;
        // Record the final assignment after spilling and copy removal.
        // Coalescing and spilling retire values; the map describes the ones the
        // function still names.
        let map: crate::backend::RegAssignment = assignment
            .iter()
            .map(|(&vreg, &register)| (ValueId::from_number(vreg), register))
            .filter(|(value, _)| context.has_value(*value))
            .collect();
        let mut attrs = op.op().attributes().to_vec();
        attrs.push(context.named_attribute(crate::backend::ASSIGNMENT_ATTR, map.to_attribute()));
        context.set_op_attributes(op.op().id, attrs);
        if frame_size > 0 || !saves.is_empty() {
            self.insert_frame(context, &blocks, frame_size, &saves)?;
        }

        Ok(())
    }
}

impl RegisterAllocationPass {
    fn database(
        &mut self,
        context: &Context,
        blocks: &[BlockId],
        scan: &BodyScan,
        weights: &[u64],
        precolor: &HashMap<u32, PhysReg>,
    ) -> Result<RegAllocDb, PassError> {
        let index_of: HashMap<BlockId, usize> = blocks
            .iter()
            .enumerate()
            .map(|(index, &block)| (block, index))
            .collect();
        let mut copies: Vec<db::SeededCopy> = scan
            .copies
            .iter()
            .filter_map(|&(op, block)| {
                let (src, dst) = allocation_copy_endpoints(context, op)?;
                Some(db::SeededCopy {
                    block: index_of[&block],
                    src,
                    dst,
                })
            })
            .collect();
        for &(op, block) in &scan.parallel {
            let op = context.get_op(op);
            for (&src, dst) in op.operands().iter().zip(op.results()) {
                copies.push(db::SeededCopy {
                    block: index_of[&block],
                    src: RegSlot::Value(src),
                    dst: RegSlot::Value(dst),
                });
            }
        }
        let pins: HashMap<ValueId, PhysReg> = precolor
            .iter()
            .map(|(&vreg, &register)| (ValueId::from_number(vreg), register))
            .collect();
        let allocatable: Vec<PhysReg> = self
            .abi
            .caller_saved
            .iter()
            .chain(self.abi.callee_saved)
            .copied()
            .collect();
        let allocatable = |register: PhysReg| {
            allocatable
                .iter()
                .any(|known| known.0.overlaps(known.1, register.0, register.1))
        };
        let mut db = RegAllocDb::seed(
            context,
            &db::Seed {
                blocks,
                weights,
                copies: &copies,
                pins: &pins,
                allocatable: &allocatable,
            },
            self.spare.take(),
        );
        db.saturate(&self.rules.liveness, &mut self.caches);
        Ok(db)
    }

    fn graph(&mut self, mut db: RegAllocDb) -> RegAllocDb {
        db.saturate(&self.rules.graph, &mut self.caches);
        db
    }

    fn frame_register(&self) -> PhysReg {
        self.abi.sp
    }

    // A replay of `def_id` — a definition whose single effect is defining
    // `spilled` with an immediate — not yet placed, and the fresh register it
    // defines: a reload that spends no stack slot.
    fn rematerialize(
        context: &Context,
        def_id: OpId,
        spilled: ValueId,
    ) -> Result<(OpId, ValueId), PassError> {
        let replayed = crate::clone_op(context, def_id);
        let fresh = context
            .get_op(replayed)
            .results()
            .first()
            .copied()
            .ok_or_else(|| {
                PassError::InvalidRuleSet(format!(
                    "rematerialized definition of %{} defines no value",
                    spilled.number()
                ))
            })?;
        Ok((replayed, fresh))
    }

    // Replace every use of an `alloca` result with a frame address computed
    // immediately before that use. Rematerializing keeps each address live for a
    // single instruction instead of spanning the whole function, which is what
    // lets bodies with many address-taken locals allocate at all: an alloca
    // value cannot be spilled (nothing would write the slot), so a long-lived
    // one pins a register forever.
    fn rematerialize_stack_allocas(
        &self,
        context: &Context,
        blocks: &[BlockId],
        scan: &BodyScan,
        allocas: &[StackAlloca],
    ) -> Result<(), PassError> {
        if allocas.is_empty() {
            return Ok(());
        }
        let default_class = self.target.register_info().default_integer_class(self.abi);
        let frame_register = self.frame_register();
        let mut sites = Vec::new();
        for alloca in allocas {
            let class = scan
                .class_of(context, alloca.value)
                .or(default_class)
                .ok_or_else(|| {
                    PassError::InvalidRuleSet(format!(
                        "stack allocation %{} has no register class",
                        alloca.value.number()
                    ))
                })?;
            sites.push((alloca, class));
        }
        for &block in blocks {
            for op_id in context.get_block(block).op_ids() {
                if !context.has_operation(op_id) {
                    continue;
                }
                let operands = context.get_op(op_id).operands().to_vec();
                for &(alloca, class) in &sites {
                    if op_id == alloca.op_id || !operands.contains(&alloca.value) {
                        continue;
                    }
                    let fresh = fresh_reg(context, class);
                    let target = op_ref_in(context, op_id);
                    for address in self.target.emit_frame_address(
                        context,
                        fresh,
                        class,
                        &frame_register,
                        alloca.offset,
                    )? {
                        context.insert_op_before(&target, address.as_ref())?;
                    }
                    for (index, operand) in operands.iter().enumerate() {
                        if *operand == alloca.value {
                            context.set_op_operand(op_id, index, fresh);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // Turn the calling convention and each instruction's pre-coloring
    // constraints into point constraints: a value the function receives or
    // returns, or an instruction must read or write, in a fixed register is
    // copied in or out right there, and the pin moves to the copy's own value.
    // A register is a constraint at one instruction, not a home for a whole
    // live range, so nothing else has to avoid it.
    fn lower_fixed_registers(
        &self,
        context: &Context,
        op: &OperationRef,
        blocks: &[BlockId],
    ) -> Result<HashMap<u32, PhysReg>, PassError> {
        let mut precolor = HashMap::new();
        self.lower_arguments(context, op, blocks, &mut precolor)?;
        self.lower_returns(context, blocks, &mut precolor)?;
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                if op.attr(crate::backend::PINS_ATTR).is_none() {
                    continue;
                }
                let op_ref = op_ref_in(context, op_id);
                for slot in crate::backend::reg_slots(&op) {
                    let crate::backend::RegSlot::Value(value) = slot.slot else {
                        continue;
                    };
                    let Some((class, index)) = crate::backend::slot_pin(&op, slot.port.name) else {
                        continue;
                    };
                    let Some(position) = slot.position else {
                        continue;
                    };
                    let fixed = fresh_reg(context, class);
                    let copy = if slot.port.def {
                        let copy = self.target.emit_copy(
                            context,
                            class,
                            crate::backend::RegSlot::Value(value),
                            crate::backend::RegSlot::Value(fixed),
                        );
                        insert_after(context, block_id, op_id, copy.as_ref())?;
                        context.set_op_result(op_id, position, fixed);
                        copy
                    } else {
                        let copy = self.target.emit_copy(
                            context,
                            class,
                            crate::backend::RegSlot::Value(fixed),
                            crate::backend::RegSlot::Value(value),
                        );
                        context.insert_op_before(&op_ref, copy.as_ref())?;
                        context.set_op_operand(op_id, position, fixed);
                        copy
                    };
                    prealloc::mark_op(
                        context,
                        copy.id(),
                        prealloc::COALESCABLE_COPY_ATTR,
                        AttributeValue::Bool(true),
                    );
                    precolor.insert(fixed.number(), (class, index));
                }
                strip_attr(context, op_id, crate::backend::PINS_ATTR);
            }
        }
        Ok(precolor)
    }

    fn lower_arguments(
        &self,
        context: &Context,
        op: &OperationRef,
        blocks: &[BlockId],
        precolor: &mut HashMap<u32, PhysReg>,
    ) -> Result<(), PassError> {
        let Some(AttributeValue::Array(args)) = op.op().attr("arg_regs") else {
            return Ok(());
        };
        let result_address = match op.op().attr("result_address") {
            Some(AttributeValue::Value(value)) => Some(value),
            _ => None,
        };
        let info = self.target.register_info();
        let plan = abi::plan_arguments(context, &info, self.abi, &args, result_address)?;
        let entry = context
            .get_block(blocks[0])
            .op_ids()
            .first()
            .map(|&first| op_ref_in(context, first))
            .ok_or_else(|| {
                PassError::InvalidRuleSet(
                    "function with arguments has no entry operation".to_string(),
                )
            })?;
        for &(incoming, register) in &plan.pins {
            let body = fresh_reg(context, register.0);
            // Rename first: the copy is the one op that must keep reading the
            // pinned value, so it is built after the rename is done.
            context.replace_value_uses(incoming, body);
            self.insert_coalescable_copy(context, &entry, register.0, body, incoming)?;
            if let Some(previous) = precolor.insert(incoming.number(), register)
                && previous != register
            {
                return Err(PassError::InvalidRuleSet(format!(
                    "argument %{} is pinned to conflicting registers {previous:?} and {register:?}",
                    incoming.number()
                )));
            }
        }
        // A stack argument gets a real definition so spilling can split it like
        // any value; the frame step patches its offset once the frame is known.
        for &(value, class, stack_index) in &plan.stack_args {
            let load = self
                .target
                .emit_spill_reload(context, value, class, &self.abi.sp, 0);
            let load_id = load.id();
            context.insert_op_before(&entry, load.as_ref())?;
            prealloc::mark_op(
                context,
                load_id,
                ABI_STACK_INDEX_ATTR,
                AttributeValue::UInt(stack_index as u64),
            );
        }
        Ok(())
    }

    fn lower_returns(
        &self,
        context: &Context,
        blocks: &[BlockId],
        precolor: &mut HashMap<u32, PhysReg>,
    ) -> Result<(), PassError> {
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                let ret = context.get_op(op_id);
                if !ret.is::<VirtualReturnOp>() {
                    continue;
                }
                let ret_ref = op_ref_in(context, op_id);
                let mut next_slot = HashMap::new();
                for (operand_index, value) in ret.operands().iter().copied().enumerate() {
                    let kind = abi::value_kind(context, self.abi, value);
                    let Some((file_class, index)) =
                        abi::next_return_register(self.abi, kind, &mut next_slot)
                    else {
                        continue;
                    };
                    let class = match crate::backend::value_class(context, value) {
                        Some(class) if class.file() == file_class.file() => class,
                        _ => file_class,
                    };
                    let outgoing = fresh_reg(context, class);
                    self.insert_coalescable_copy(context, &ret_ref, class, outgoing, value)?;
                    context.set_op_operand(op_id, operand_index, outgoing);
                    precolor.insert(outgoing.number(), (class, index));
                }
            }
        }
        Ok(())
    }

    fn insert_coalescable_copy(
        &self,
        context: &Context,
        before: &OperationRef,
        class: RegClassId,
        dst: ValueId,
        src: ValueId,
    ) -> Result<(), PassError> {
        let copy = self
            .target
            .emit_copy(context, class, RegSlot::Value(dst), RegSlot::Value(src));
        let copy_id = copy.id();
        context.insert_op_before(before, copy.as_ref())?;
        prealloc::mark_op(
            context,
            copy_id,
            prealloc::COALESCABLE_COPY_ATTR,
            AttributeValue::Bool(true),
        );
        Ok(())
    }

    fn insert_frame(
        &self,
        context: &Context,
        blocks: &[BlockId],
        size: u32,
        saves: &[(PhysReg, i64)],
    ) -> Result<(), PassError> {
        if let Some(&entry) = blocks.first() {
            let op_ids = context.get_block(entry).op_ids();
            if let Some(&first) = op_ids.first() {
                let target = op_ref_in(context, first);
                for op in self.target.emit_prologue(context, self.abi, size, saves) {
                    context.insert_op_before(&target, op.as_ref())?;
                }
            }
        }
        for &block_id in blocks {
            for op_id in context.get_block(block_id).op_ids() {
                if !context.get_op(op_id).is::<VirtualReturnOp>() {
                    continue;
                }
                let target = op_ref_in(context, op_id);
                for op in self.target.emit_epilogue(context, self.abi, size, saves) {
                    context.insert_op_before(&target, op.as_ref())?;
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_incoming_stack_arg_loads(
        &self,
        context: &Context,
        blocks: &[BlockId],
        assignment: &HashMap<u32, PhysReg>,
        args: &[IncomingStackArg],
        frame_plan: &FramePlan,
        frame_size: u32,
        pushed_saves: usize,
    ) -> Result<(), PassError> {
        if args.is_empty() {
            return Ok(());
        }
        if blocks.is_empty() {
            return Ok(());
        }
        let frame_register = self.frame_register();
        // The incoming argument area is a memory of its own, and nothing in this
        // function writes it: the loads all read the state it was entered with
        // and are ordered against nothing, including each other.
        let mut incoming = SlotChain::default();
        for arg in args {
            let load_id = arg.load;
            let op = context.get_op(load_id);
            let value = op.results().first().copied().ok_or_else(|| {
                PassError::InvalidRuleSet(format!(
                    "stack argument load for %{} has no virtual definition",
                    arg.value.number()
                ))
            })?;
            let dst = assignment[&value.number()];
            if dst.0.file() != arg.class.file() || dst.0.group_width != arg.class.group_width {
                return Err(PassError::InvalidRuleSet(format!(
                    "stack argument %{} assigned to {:?}, expected class {}",
                    arg.value.number(),
                    dst,
                    arg.class.name()
                )));
            }
            let offset =
                frame_plan.incoming_stack_arg_offset(frame_size, pushed_saves, arg.stack_index);
            let load =
                self.target
                    .emit_spill_reload(context, value, dst.0, &frame_register, offset);
            let target = op_ref_in(context, load_id);
            incoming.observe(context, &target, load.as_ref())?;
            context.replace_op(&target, load.as_ref())?;
        }
        Ok(())
    }
}

/// Tracks stack-slot assignment and owns the target-neutral ABI layout
/// formulas for the stable frame area.
struct FramePlan {
    align: u32,
    slot_size: u32,
    save_style: crate::backend::abi::SaveStyle,
    next_offset: i64,
}

impl FramePlan {
    fn new(abi: &crate::backend::abi::AbiInfo) -> Self {
        Self {
            align: abi.stack.align,
            slot_size: abi.stack.slot_size,
            save_style: abi.stack.save_style,
            next_offset: 0,
        }
    }

    fn reserve_outgoing(&mut self, size: u32) {
        self.next_offset = self.next_offset.max(i64::from(size));
    }

    fn alloc_slot(&mut self) -> i64 {
        self.alloc(self.slot_size, self.slot_size)
    }

    fn alloc_stack_allocation(&mut self, size: u32, align: u32) -> i64 {
        self.alloc(size, align)
    }

    fn alloc_callee_save(&mut self) -> i64 {
        if self.save_style == crate::backend::abi::SaveStyle::FrameSlots {
            self.alloc_slot()
        } else {
            0
        }
    }

    fn alloc(&mut self, size: u32, align: u32) -> i64 {
        let align = i64::from(align.max(1));
        self.next_offset = ((self.next_offset + align - 1) / align) * align;
        let offset = self.next_offset;
        self.next_offset += i64::from(size);
        offset
    }

    fn stable_size(&self) -> u32 {
        align_to(self.next_offset as u32, self.align)
    }

    fn prologue_adjustment(&self, has_calls: bool, pushed_saves: usize) -> u32 {
        let stable_size = self.stable_size();
        if self.save_style != crate::backend::abi::SaveStyle::PushPop || !has_calls {
            return stable_size;
        }
        stable_size
            + align_delta(
                pushed_saves as u32 * self.slot_size + stable_size,
                self.align,
                self.slot_size % self.align.max(1),
            )
    }

    fn incoming_stack_arg_offset(
        &self,
        prologue_adjustment: u32,
        pushed_saves: usize,
        stack_index: usize,
    ) -> i64 {
        let slot = i64::from(self.slot_size);
        match self.save_style {
            crate::backend::abi::SaveStyle::FrameSlots => {
                i64::from(prologue_adjustment) + stack_index as i64 * slot
            }
            crate::backend::abi::SaveStyle::PushPop => {
                pushed_saves as i64 * slot
                    + i64::from(prologue_adjustment)
                    + slot
                    + stack_index as i64 * slot
            }
        }
    }
}

fn align_to(size: u32, align: u32) -> u32 {
    if size == 0 {
        return 0;
    }
    let align = align.max(1);
    size.div_ceil(align) * align
}

fn align_delta(offset: u32, align: u32, desired_remainder: u32) -> u32 {
    let align = align.max(1);
    (desired_remainder + align - offset % align) % align
}

struct StackAlloca {
    op_id: OpId,
    value: ValueId,
    offset: i64,
}

// Erase the allocations, keeping the chains they rooted. A slot's memory is its
// own — the allocation is what said so — so with the op gone the chain starts at
// a `state.entry_state : !state<memory>` of its own instead, and the accesses on it stay ordered
// against each other and against nothing else.
fn erase_stack_allocas(context: &Context, allocas: &[StackAlloca]) -> Result<(), PassError> {
    for alloca in allocas {
        if !context.has_operation(alloca.op_id) {
            continue;
        }
        let op_ref = op_ref_in(context, alloca.op_id);
        if let Some(published) = op_ref.op().state_results().first().copied() {
            let root = tir::state::EntryStateOpBuilder::new(context)
                .state_result(tir::builtin::StateType::memory(context))
                .build();
            let root_state = root.result();
            context.insert_op_before(&op_ref, &root)?;
            context.replace_value_uses(published, root_state);
        }
        context.erase_op(&op_ref)?;
    }
    Ok(())
}

pub(crate) fn op_ref_in(context: &Context, op_id: OpId) -> OperationRef {
    OperationRef::new(context.get_op(op_id))
}

/// One frame slot's memory, as the spill rewrite lays it down inside a block.
///
/// A slot is a memory of its own — the allocator made it, and nothing outside
/// the function can reach it — so its accesses are ordered against each other
/// and against nothing else, in the shape every other chain has: any number of
/// reloads observe what the last store left, unordered among themselves, and the
/// next store takes `state.join` of what they published, which is the edge that
/// orders it after every one of them.
///
/// The chain is per block and rooted where its first access lands. What ran
/// before a block is the control flow, not an edge: a root shared across blocks
/// would claim an order between them that nothing wrote down, and would be two
/// futures for one memory the moment two blocks stored to the slot.
#[derive(Default)]
struct SlotChain {
    /// The memory the slot holds, once something in this block has named it.
    written: Option<ValueId>,
    /// What the reloads since that store published, unordered among themselves.
    read: Vec<ValueId>,
}

impl SlotChain {
    // Order `reload` after the store that filled the slot, and against nothing
    // else: it observes what that store left and publishes the same memory.
    fn observe(
        &mut self,
        context: &Context,
        before: &OperationRef,
        reload: &dyn Operation,
    ) -> Result<(), PassError> {
        let observed = self.root(context, before)?;
        self.read
            .push(tir::backend::put_on_chain(context, reload, observed));
        Ok(())
    }

    // Order `store` after every reload since the last one, through the join of
    // what they published; the memory it leaves is what the slot holds next.
    fn change(
        &mut self,
        context: &Context,
        before: &OperationRef,
        store: &dyn Operation,
    ) -> Result<(), PassError> {
        let observed = self.root(context, before)?;
        // Merging one memory is that memory, as it is where the chains are
        // drawn: only a fork of several reloads needs a join to close it.
        let taken = match std::mem::take(&mut self.read).as_slice() {
            [] => observed,
            [only] => *only,
            states => {
                let join = tir::state::JoinOpBuilder::new(context)
                    .states(states.to_vec())
                    .state_result(context.get_value(states[0]).ty())
                    .build();
                let merged = join.result();
                context.insert_op_before(before, &join)?;
                merged
            }
        };
        self.written = Some(tir::backend::put_on_chain(context, store, taken));
        Ok(())
    }

    // The memory the slot holds, opening the block's chain at a
    // `state.entry_state : !state<memory>` where this is its first access.
    fn root(&mut self, context: &Context, before: &OperationRef) -> Result<ValueId, PassError> {
        if let Some(written) = self.written {
            return Ok(written);
        }
        let root = tir::state::EntryStateOpBuilder::new(context)
            .state_result(tir::builtin::StateType::memory(context))
            .build();
        let state = root.result();
        context.insert_op_before(before, &root)?;
        self.written = Some(state);
        Ok(state)
    }
}

// Insert `new_op` immediately after `op_id` in its block (before the following op,
// or appended if `op_id` is last — which spill stores never are).
fn insert_after(
    context: &Context,
    block_id: BlockId,
    op_id: OpId,
    new_op: &dyn Operation,
) -> Result<(), PassError> {
    let op_ids = context.get_block(block_id).op_ids();
    let pos = op_ids.iter().position(|&id| id == op_id);
    match pos.and_then(|p| op_ids.get(p + 1).copied()) {
        Some(next) => {
            let target = op_ref_in(context, next);
            context.insert_op_before(&target, new_op)
        }
        None => Err(PassError::RewriteFailed(op_id)),
    }
}

// The callee-saved physical registers the allocation actually used, each paired
// with a freshly reserved frame slot. A callee-saved register belongs to the
// caller; if this function colors a value into one it must save and restore it
// around the body. Deterministic order (by class then index) keeps codegen
// stable.
fn callee_saved_slots(
    assignment: &HashMap<u32, PhysReg>,
    frame: &mut FramePlan,
    abi_callee_saved: &[PhysReg],
) -> Vec<(PhysReg, i64)> {
    // Keyed by the ABI's own spelling of the register: a value allocated
    // through a narrower view of one (x86 `ebx` for `rbx`) names the same
    // physical register, and it is saved once, in full.
    let mut regs: Vec<PhysReg> = assignment
        .values()
        .filter_map(|p| {
            abi_callee_saved
                .iter()
                .find(|candidate| p.0.overlaps(p.1, candidate.0, candidate.1))
                .copied()
        })
        .collect();
    regs.sort();
    regs.dedup();
    regs.into_iter()
        .map(|p| (p, frame.alloc_callee_save()))
        .collect()
}

/// An `alloca` the scan found, before the frame gives it an offset.
struct ScannedAlloca {
    op_id: OpId,
    value: ValueId,
    size: u32,
    align: u32,
}

/// One walk of the function body, taken before allocation rewrites anything.
/// Everything allocation needs to know about the body as selection left it:
///
/// - marked virtual copies and full-register machine copies. Their current
///   endpoints are resolved each round and before erasure after spilling;
/// - the outgoing call frame the body's calls need, and whether it calls at all;
/// - the stack allocations, which the frame plan then places;
/// - the class each value is first named through, for a value whose own type
///   does not name one (an `alloca` address);
/// - the control-flow successors of each block, for the loop nesting that
///   prices spills. A machine block may hold several branch-shaped ops — a mid-block
///   conditional jump for the taken edge plus a trailing virtual branch for the
///   fallthrough — so a block's successors are the union of
///   `Terminator::successors` over every op it contains, not just its last op's.
///   Spills insert ops within blocks but never add or remove edges, so this
///   holds for every round.
#[derive(Default)]
struct BodyScan {
    copies: Vec<(OpId, BlockId)>,
    /// The parallel copies splitting live ranges before constrained
    /// instructions; each pair is a copy.
    parallel: Vec<(OpId, BlockId)>,
    outgoing_size: u32,
    has_calls: bool,
    allocas: Vec<ScannedAlloca>,
    slot_classes: HashMap<ValueId, Option<RegClassId>>,
    successors: HashMap<BlockId, Vec<BlockId>>,
}

impl BodyScan {
    fn of(context: &Context, blocks: &[BlockId]) -> Result<Self, PassError> {
        let mut scan = BodyScan::default();
        for &block_id in blocks {
            let mut successors: Vec<BlockId> = Vec::new();
            for op_id in context.get_block(block_id).op_ids() {
                let op = context.get_op(op_id);
                if op.attr(prealloc::COALESCABLE_COPY_ATTR).is_some() {
                    copy_endpoints(context, op_id).ok_or_else(|| {
                        PassError::InvalidRuleSet(format!(
                            "coalescable copy {op_id:?} does not move one virtual register to another"
                        ))
                    })?;
                    scan.copies.push((op_id, block_id));
                } else if allocation_copy_endpoints(context, op_id).is_some() {
                    scan.copies.push((op_id, block_id));
                }

                let outgoing = op
                    .clone()
                    .as_op::<VirtualCallOp>()
                    .map(|call| call.outgoing_stack_size())
                    .or_else(|| {
                        op.clone()
                            .as_op::<VirtualIndirectCallOp>()
                            .map(|call| call.outgoing_stack_size())
                    });
                if let Some(outgoing) = outgoing {
                    scan.has_calls = true;
                    let outgoing = u32::try_from(outgoing).map_err(|_| {
                        PassError::InvalidRuleSet(
                            "outgoing call frame exceeds 32-bit size".to_string(),
                        )
                    })?;
                    scan.outgoing_size = scan.outgoing_size.max(outgoing);
                }

                if let Some(allocation) = op.clone().as_op::<AllocaOp>()
                    && let Some(value) = op.results().first().copied()
                {
                    scan.allocas.push(ScannedAlloca {
                        op_id,
                        value,
                        size: allocation.size() as u32,
                        align: allocation.align() as u32,
                    });
                }

                for slot in crate::backend::reg_slots(&op) {
                    if let crate::backend::RegSlot::Value(value) = slot.slot {
                        scan.slot_classes.entry(value).or_insert(slot.port.class);
                    }
                }

                if let Some(term) = op.as_interface::<dyn tir::Terminator>() {
                    for succ in term.successors() {
                        if !successors.contains(&succ) {
                            successors.push(succ);
                        }
                    }
                }
            }
            scan.successors.insert(block_id, successors);
        }
        Ok(scan)
    }

    fn stack_allocas(&self, frame: &mut FramePlan) -> Vec<StackAlloca> {
        self.allocas
            .iter()
            .map(|alloca| StackAlloca {
                op_id: alloca.op_id,
                value: alloca.value,
                offset: frame.alloc_stack_allocation(alloca.size, alloca.align),
            })
            .collect()
    }

    // The register class `value` is read through: its own, or the class of the
    // first register slot naming it.
    fn class_of(&self, context: &Context, value: ValueId) -> Option<RegClassId> {
        crate::backend::value_class(context, value)
            .or_else(|| self.slot_classes.get(&value).copied().flatten())
    }
}

/// The `(source, destination)` virtual registers of a copy op: its first read
/// and first written virtual register.
pub(crate) fn copy_endpoints(context: &Context, op_id: OpId) -> Option<(u32, u32)> {
    let regs = liveness::op_regs(&context.get_op(op_id));
    let src = regs.uses.first()?.number();
    let dst = regs.defs.first()?.number();
    Some((src, dst))
}

// Resolve the named ports after any spill splitting. Marked synthetic copies
// retain their existing virtual-register contract, including narrow views.
fn allocation_copy_endpoints(context: &Context, op_id: OpId) -> Option<(RegSlot, RegSlot)> {
    if !context.has_operation(op_id) {
        return None;
    }
    let op = context.get_op(op_id);
    if op.attr(prealloc::COALESCABLE_COPY_ATTR).is_some() {
        let (src, dst) = copy_endpoints(context, op_id)?;
        return Some((
            RegSlot::Value(ValueId::from_number(src)),
            RegSlot::Value(ValueId::from_number(dst)),
        ));
    }
    if let Some(AttributeValue::Array(ports)) = op.attr(crate::backend::FULL_REGISTER_COPY_ATTR) {
        let [AttributeValue::Str(src), AttributeValue::Str(dst)] = ports.as_ref() else {
            return None;
        };
        return Some((
            crate::backend::reg_slot(&op, src.as_ref())?,
            crate::backend::reg_slot(&op, dst.as_ref())?,
        ));
    }
    let machine = op
        .clone()
        .as_interface::<dyn crate::backend::MachineInstruction>()?;
    let copy = machine.info().copy?;
    Some((
        crate::backend::reg_slot(&op, copy.src)?,
        crate::backend::reg_slot(&op, copy.dst)?,
    ))
}

fn strip_attr(context: &Context, op_id: OpId, name: &str) {
    let stripped = context.sym(name);
    let mut attrs = context.get_op(op_id).attributes().to_vec();
    attrs.retain(|attr| Some(attr.name) != stripped);
    context.set_op_attributes(op_id, attrs);
}

/// Marks the placeholder load of an incoming stack argument and carries its
/// stack slot index. The frame step patches the real offset in once the frame
/// layout is known.
const ABI_STACK_INDEX_ATTR: &str = "abi_stack_index";

struct IncomingStackArg {
    value: ValueId,
    class: RegClassId,
    stack_index: usize,
    load: OpId,
}

// The placeholder loads `ABI_STACK_INDEX_ATTR` marks, one per incoming
// stack argument.
fn collect_stack_arg_loads(
    context: &Context,
    blocks: &[BlockId],
) -> Result<Vec<IncomingStackArg>, PassError> {
    let mut args = Vec::new();
    for &block_id in blocks {
        for op_id in context.get_block(block_id).op_ids() {
            let op = context.get_op(op_id);
            let Some(stack_index) = op.attr(ABI_STACK_INDEX_ATTR).and_then(|value| match value {
                AttributeValue::UInt(index) => Some(index as usize),
                _ => None,
            }) else {
                continue;
            };
            let value = op.results().first().copied().ok_or_else(|| {
                PassError::InvalidRuleSet(
                    "stack argument load has no virtual definition".to_string(),
                )
            })?;
            let class = crate::backend::value_class(context, value).ok_or_else(|| {
                PassError::InvalidRuleSet(
                    "stack argument load defines a value with no register class".to_string(),
                )
            })?;
            args.push(IncomingStackArg {
                value,
                class,
                stack_index,
                load: op_id,
            });
        }
    }
    Ok(args)
}
