use crate::analysis::defuse::PhysReg;
use tir::{AnalysisManager, Context, OpId, Pass, PassError};

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
/// keyed by the class name used in [`crate::attributes::RegisterAttr`] operands.
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

#[derive(Clone, Default)]
pub struct RegisterAllocationPass;
impl RegisterAllocationPass {
    pub fn new() -> Self {
        Self
    }
}
crate::register_pass!(RegisterAllocationPass, "register-allocation");

impl Pass for RegisterAllocationPass {
    fn name(&self) -> &'static str {
        "register-allocation"
    }
    fn run(
        &mut self,
        _op: OpId,
        _context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        Ok(())
    }
}
