use crate::utils::StableHashMap;
use crate::{Span, Type};
use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum RegisterTrait {
    HardwiredZero,
    ReturnAddress,
    CallerSaved,
    CalleeSaved,
    StackPointer,
    ProgramCounter,
    GlobalPointer,
    ThreadPointer,
    /// Reserved by the platform ABI: never allocated (e.g. AArch64 x18, the
    /// platform register). Like the pointer traits, it excludes the register from
    /// the allocation order.
    Reserved,
    /// Carries an incoming argument under the calling convention. Argument order
    /// follows the register index order within the class (a0 before a1, ...).
    Argument,
    /// Holds an outgoing return value under the calling convention.
    ReturnValue,
    Temporary,
    Saved,
    /// A condition-code bit (x86 EFLAGS `zf`, AArch64 PSTATE `z`): written as a
    /// side effect by compare-style instructions and read by conditional-branch
    /// guards. Marks the class for flag-branch rule derivation.
    StatusFlag,
    /// Holds IEEE binary floating-point values. Marks the class so instruction
    /// selection types its patterns with float types and keeps float and
    /// integer operands from binding across register files.
    Float,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Register {
    pub name: String,
    pub alias: Option<String>,
    /// Explicit encoding index (`index = 0xC00`), for registers whose
    /// architectural number is not derivable from the name (e.g. CSRs).
    pub index: Option<u16>,
    /// Explicit calling-convention argument position (`arg = 0`), for ABIs whose
    /// argument order does not match register-index order (e.g. the x86-64 System V
    /// order rdi, rsi, rdx, rcx, r8, r9). When any argument register in a class sets
    /// this, the class's argument list is ordered by it instead of by index.
    pub arg_order: Option<u16>,
    pub traits: Vec<RegisterTrait>,
    pub subregisters: Vec<Register>,
    #[serde(skip_serializing)]
    pub span: Span,
}

impl Register {
    /// The register's canonical encoding index: the explicit `index` when
    /// declared, otherwise the trailing number in the name (`x5` -> 5).
    pub fn encoding_index(&self) -> Option<u16> {
        self.index.or_else(|| parse_trailing_index(&self.name))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegisterRange {
    pub start: String,
    pub end: String,
    pub alias_pattern: Option<String>,
    pub traits: Vec<RegisterTrait>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum RegisterDef {
    Single(Register),
    Range(RegisterRange),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RegisterClass {
    pub name: String,
    pub for_isas: Vec<String>,
    /// Name of the register class this one inherits from, if any. A derived class
    /// shares the base's physical register file (the same encoding indices name the
    /// same registers) but may add registers and override individual encoding slots
    /// — e.g. AArch64 `GPRsp : GPR` redefines slot 31 as `sp` instead of `xzr`.
    /// Resolved (flattened into `parameters`/`registers`) by
    /// [`resolve_register_class_inheritance`] before any analysis runs.
    pub base: Option<String>,
    /// Explicit physical register file this class draws from, decoupling file
    /// sharing (for allocation aliasing) from register inheritance. Unlike `base`,
    /// it does not import the named class's registers — the class keeps only its
    /// own — so a class can alias a subset of another file at chosen indices.
    /// x86 high bytes (`ah`/`ch`/`dh`/`bh`) use this: they overlap `rax`..`rbx`
    /// (file `GPR`, indices 0..3) but their own encoding differs, so inheriting
    /// `GPR`'s full register list would be wrong.
    pub file: Option<String>,
    #[serde(serialize_with = "serialize_params")]
    pub parameters: StableHashMap<String, (Type, Option<Expr>)>,
    pub registers: Vec<RegisterDef>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterNameTables {
    pub parse_names: Vec<(String, u16)>,
    pub isa_names: Vec<(u16, String)>,
    pub abi_names: Vec<(u16, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegisterAllocationMetadata {
    /// Allocatable register indices in preferred allocation order.
    pub allocation_order: Vec<u16>,
    pub caller_saved: Vec<u16>,
    pub callee_saved: Vec<u16>,
    /// Argument registers in calling-convention order.
    pub arguments: Vec<u16>,
    pub return_values: Vec<u16>,
    /// Indices reserved by the ABI and never allocated.
    pub reserved: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum IsaRequirement {
    Single(String),
    Any(Vec<String>),
    All(Vec<String>),
}

/// Architectural trap-entry sequence, defined once per ISA: how a synchronous
/// exception updates state. A `trap(args...)` call in a behavior inlines it in
/// the SMT model with `params` bound to the call arguments (missing trailing
/// arguments read as zero); the simulator routes `trap` to the machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct TrapHandler {
    pub params: Vec<String>,
    pub body: Expr,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Isa {
    pub name: String,
    pub requires: Option<IsaRequirement>,
    #[serde(serialize_with = "serialize_params")]
    pub parameters: StableHashMap<String, (Type, Option<Expr>)>,
    pub trap_handler: Option<TrapHandler>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Template {
    pub name: String,
    pub for_isas: Vec<String>,
    pub parent_template: Option<String>,
    #[serde(serialize_with = "serialize_params")]
    pub params: StableHashMap<String, (Type, Option<Expr>)>,
    pub operands: Vec<(String, Type)>,
    pub encoding: Vec<EncodingArm>,
    pub asm: Option<Expr>,
    /// Scheduling-class membership shared by derived instructions that declare no
    /// `schedule` of their own (resolved by
    /// [`crate::utils::resolve_effective_schedule_for_instruction`]).
    pub schedule: Option<Schedule>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Instruction {
    pub name: String,
    pub for_isas: Vec<String>,
    pub parent_template: Option<String>,
    #[serde(serialize_with = "serialize_params")]
    pub params: StableHashMap<String, (Type, Option<Expr>)>,
    pub operands: Vec<(String, Type)>,
    pub encoding: Vec<EncodingArm>,
    pub asm: Option<Expr>,
    pub behavior: Expr,
    /// Performance model membership: the scheduling classes this
    /// instruction belongs to. `None` when the instruction carries no `schedule`
    /// block; consumers fall back to a default scheduling class.
    pub schedule: Option<Schedule>,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// The `schedule { ... }` block of an instruction. Declares only *membership* in
/// machine-independent scheduling classes ([`SchedClassDecl`]); the concrete cost
/// (latency, resources) is supplied per-machine by [`UnitBind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Schedule {
    pub classes: Vec<String>,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// A top-level `sched_class` declaration: a machine-independent scheduling-class
/// identity that instructions reference and machines bind to concrete cost. The
/// optional defaults are resource-agnostic and feed the compiler cost model when
/// no specific machine is selected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchedClassDecl {
    pub name: String,
    pub default_latency: Option<i64>,
    pub default_throughput: Option<i64>,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// One functional unit / issue resource declared by a [`Machine`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineUnit {
    pub name: String,
    /// Number of parallel units of this resource.
    pub units: i64,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// How a pipeline stage handles data hazards. Mirrors
/// [`tir::backend::sched::Protection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Protection {
    Protected,
    Unprotected,
    Hard,
}

/// One named stage of a [`Machine`]'s pipeline. Its position in the pipeline list
/// is its cycle offset from issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PipelinePhase {
    pub name: String,
    pub protection: Protection,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// A machine's binding of one [`SchedClassDecl`] to concrete cost on that machine.
///
/// Timing is either scalar (`latency`) or phase-based (`reads`/`writes` naming
/// pipeline phases); the latter desugars to `latency = cycle(writes) -
/// cycle(reads)` with a non-zero read cycle. Scalar `latency = N` is equivalent
/// to reading at cycle 0 and writing at cycle N.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UnitBind {
    pub unit: String,
    pub latency: Option<i64>,
    pub throughput: Option<i64>,
    /// Pipeline phase at which source operands are read (phase-based form).
    pub reads: Option<String>,
    /// Pipeline phase at which the result is written (phase-based form).
    pub writes: Option<String>,
    /// Resources (by [`MachineUnit`] name) this unit occupies.
    pub uses: Vec<String>,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// A machine's per-instruction cost override (the LLVM `InstRW` analogue): it
/// supersedes the `sched_class`-based resolution for one specific instruction on this
/// machine. Carries the same timing fields as a [`UnitBind`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MachineOverride {
    /// The overridden instruction, by its TMDL `instruction` name.
    pub instruction: String,
    pub latency: Option<i64>,
    pub throughput: Option<i64>,
    pub reads: Option<String>,
    pub writes: Option<String>,
    pub uses: Vec<String>,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// A forwarding/bypass path between two of a machine's resources, with the
/// producer→consumer latency it grants. Mirrors [`tir::backend::sched::Forward`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Forward {
    pub from: String,
    pub to: String,
    pub latency: i64,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// A `machine` block: one device implementation. Holds the resource menu, buffer
/// sizes (defaults; the Rust simulator may override), and per-unit cost
/// bindings for a set of ISAs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Machine {
    pub name: String,
    /// Optional friendly name used to select this machine (e.g. `in-order`),
    /// declared as `machine Name ("alias") for [...]`. Keeps tool-facing names
    /// single-sourced in TMDL alongside the machine itself.
    pub alias: Option<String>,
    pub for_isas: Vec<String>,
    pub issue_width: Option<i64>,
    /// Structural buffer sizes by name (e.g. `rob`, `lsq`, `iq`).
    pub buffers: Vec<(String, i64)>,
    /// Ordered pipeline stages; empty when no `pipeline` block is declared.
    pub pipeline: Vec<PipelinePhase>,
    pub resources: Vec<MachineUnit>,
    /// Physical register-file sizes for renaming, keyed by physical-file name (the
    /// root of a register class's inheritance chain; see
    /// [`RegisterClass::register_file`]). A file absent here defaults to the
    /// architectural register count of that file.
    pub reg_files: Vec<(String, i64)>,
    pub binds: Vec<UnitBind>,
    /// Per-instruction cost overrides (take precedence over `binds`).
    pub overrides: Vec<MachineOverride>,
    /// Forwarding/bypass paths between resources.
    pub forwards: Vec<Forward>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EncodingArm {
    pub start: u16,
    pub end: Option<u16>,
    pub value: Expr,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Item {
    Isa(Isa),
    RegisterClass(RegisterClass),
    Template(Template),
    Instruction(Instruction),
    Unit(SchedClassDecl),
    Machine(Machine),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum Lit {
    Str(LitStr),
    Int(LitInt),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct LitStr {
    value: String,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct LitInt {
    value: String,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Field {
    pub base: Box<Expr>,
    pub member: String,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct If {
    pub cond: Box<Expr>,
    pub then: Box<Expr>,
    pub else_: Option<Box<Expr>>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Block {
    pub stmts: Vec<Expr>,
    pub last_expr_return: bool,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Ident {
    pub name: String,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// Exception kinds an `except` clause can catch. Each kind is raised by a
/// specific builtin when the enclosing `try` names a handler for it; without a
/// handler the operation keeps its total (no-trap) semantics, which is how
/// ISAs that do not trap express themselves.
pub const EXCEPTION_KINDS: &[&str] = &["misaligned_load", "misaligned_store"];

/// The five `Ordering::<member>` names, in code order 0..4 (`bits<3>`).
pub const ORDERING_NAMES: &[&str] = &["relaxed", "acquire", "release", "acq_rel", "seq_cst"];

/// The `bits<3>` code of an `Ordering::<member>` constant, or `None` if unknown.
pub fn ordering_code(member: &str) -> Option<u8> {
    ORDERING_NAMES
        .iter()
        .position(|n| *n == member)
        .map(|c| c as u8)
}

/// The closed set of `atomic_rmw` op selectors, in code order 0..8.
pub const ATOMIC_RMW_OPS: &[&str] = &[
    "add", "swap", "xor", "and", "or", "min", "max", "minu", "maxu",
];

/// The op code of an `atomic_rmw` selector identifier, or `None` if unknown.
pub fn atomic_rmw_op_code(name: &str) -> Option<u8> {
    ATOMIC_RMW_OPS
        .iter()
        .position(|n| *n == name)
        .map(|c| c as u8)
}

/// One `except kind(binding) { ... }` clause. The binding receives the
/// exception payload (the faulting address for misaligned accesses).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct ExceptClause {
    pub kind: String,
    pub binding: Option<String>,
    pub body: Expr,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// `try { ... } except ...`: precise-trap semantics. If an operation in the
/// body raises a caught exception, none of the body's effects commit and the
/// matching clause executes against the state at try entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct TryExcept {
    pub body: Box<Expr>,
    pub handlers: Vec<ExceptClause>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Assign {
    pub dest: Box<Expr>,
    pub value: Box<Expr>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Path {
    pub base: String,
    pub remainder: Vec<String>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    UnsignedDiv,
    Equal,
    NotEqual,
    LessThan,
    GreaterThan,
    LessThenEqual,
    GreaterThanEqual,
    UnsignedLessThan,
    UnsignedGreaterThan,
    UnsignedLessThenEqual,
    UnsignedGreaterThanEqual,
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
    ShiftLeftLogical,
    ShiftRightLogical,
    ShiftRightArithmetic,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Binary {
    pub lhs: Box<Expr>,
    pub rhs: Box<Expr>,
    pub op: BinOp,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum UnOp {
    BitwiseNot,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Unary {
    pub x: Box<Expr>,
    pub op: UnOp,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum BuiltinFunction {
    Clamp,
    Extract,
    Log2Ceil,
    /// `regnum(op)`: the encoding index (architectural register number) of a
    /// register operand, as `bits<ENCODING_LEN>` of the operand's class. Reads
    /// the operand's identity, not the value it holds, so `regnum(rs1) != 0`
    /// distinguishes `x0` from any other register regardless of its contents.
    Regnum,
    SExt,
    ZExt,
    Load,
    Store,
    /// `load_reserved(addr, bytes, ordering)`: read memory and register a
    /// reservation covering the access; value is the loaded word.
    LoadReserved,
    /// `store_conditional(addr, bytes, value, ordering)`: write iff a valid
    /// reservation covers the access; value is `bits<1>`, 1 = success.
    StoreConditional,
    /// `atomic_rmw(op, addr, bytes, value, ordering)`: single read-modify-write;
    /// `op` is a bare identifier from the closed set add/swap/xor/and/or/min/max/
    /// minu/maxu. Value is the old memory word.
    AtomicRmw,
    /// `fence(pred, succ)`: data-memory ordering fence with target-defined bit
    /// sets. An effect statement, like `trap`.
    Fence,
    /// `fence_i()`: instruction-stream fence. An effect statement.
    FenceI,
    /// `trap(cause)`: raise a synchronous exception (e.g. ecall/ebreak). An
    /// effect-only builtin handled directly by codegen; it produces no value.
    Trap,
    /// `split(bits, n)`: cut a bit value into `n` equal-width lanes (an iterator),
    /// lane 0 from the low bits.
    Split,
    /// `concat(iter)`: join an iterator's lanes into one bit value, lane 0 in the
    /// low bits. The inverse of `split`.
    Concat,
    /// `map(iter, |x| ...)`: apply a lambda to each lane of an iterator.
    Map,
    /// `reduce(iter, |acc, x| ...)`: left-fold a binary lambda over an iterator's
    /// lanes (e.g. a horizontal add).
    Reduce,
    /// `zip(a, b)`: pair two iterators lane-wise so a `map` lambda can read both
    /// sides as separate parameters.
    Zip,
    /// IEEE 754 binary floating-point arithmetic over register bits; the format
    /// is the binary32/binary64 interchange format of the operand width.
    FAdd,
    FSub,
    FMul,
    FDiv,
    /// `todo()`: the instruction's semantics are not modeled. It suppresses
    /// instruction-selection rule generation (the op still exists, prints, and
    /// parses) and its `execute()` traps. For behaviors the TMDL expression
    /// language cannot yet express (barriers, atomics, special-register reads).
    Todo,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Call {
    pub callee: Box<Expr>,
    pub arguments: Vec<Expr>,
    #[serde(skip_serializing)]
    pub span: Span,
}

/// A Rust-style anonymous function `|params| body`. Only valid as an argument to
/// the `map`/`reduce` builtins; the lowering inlines its body, binding each
/// parameter to the corresponding lambda argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Lambda {
    pub params: Vec<String>,
    pub body: Box<Expr>,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Slice {
    pub base: Box<Expr>,
    pub start: u16,
    pub end: u16,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct IndexAccess {
    pub base: Box<Expr>,
    pub index: u16,
    #[serde(skip_serializing)]
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub enum Expr {
    Assign(Assign),
    Binary(Binary),
    Unary(Unary),
    Block(Block),
    Call(Call),
    Field(Field),
    Ident(Ident),
    If(If),
    IndexAccess(IndexAccess),
    Path(Path),
    Lit(Lit),
    Slice(Slice),
    Try(TryExcept),
    BuiltinFunction(BuiltinFunction),
    Lambda(Lambda),
    Invalid,
}

pub struct SemaLowering {
    pub root: tir::graph::NodeId,
    pub variable_symbols: HashMap<String, u32>,
    pub register_symbols: HashMap<(String, u32), u32>,
    /// Operand name -> symbol id for `regnum(op)`: a symbol bound to the
    /// operand's encoding index rather than its value. Kept apart from
    /// `variable_symbols` so the same operand can appear both by value and by
    /// index within one behavior.
    pub regnum_symbols: HashMap<String, u32>,
}

struct SemaExprLoweringCtx<
    'a,
    G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
> {
    graph: &'a mut G,
    params: &'a HashMap<String, i64>,
    /// Maps `(class, register-name)` to the register's canonical encoding index, so
    /// register paths like `PSTATE::z` that carry no numeric index in their name can
    /// still be lowered to a stable `(class, index)` slot. When absent, only PC and
    /// numbered registers (whose index is in the name, e.g. `x5`) can be resolved.
    register_indices: Option<&'a HashMap<(String, String), u32>>,
    /// ISA parameter values (e.g. `VLEN`, `SEW`), used only to const-evaluate a
    /// vector map's lane count. They are deliberately not consulted for general
    /// `self.PARAM` lowering, which keeps target-dependent params like `XLEN`
    /// symbolic in patterns.
    isa_consts: HashMap<String, i64>,
    next_symbol_id: u32,
    register_symbols: HashMap<(String, u32), u32>,
    variable_symbols: HashMap<String, u32>,
    regnum_symbols: HashMap<String, u32>,
    had_error: bool,
    /// Stack of `map`/`reduce` lambda parameter names, innermost last. An `Ident`
    /// matching a parameter of the innermost lambda lowers to an `Arg` node whose
    /// index is the parameter's position.
    lambda_params: Vec<Vec<String>>,
}

impl<'a, G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>>
    SemaExprLoweringCtx<'a, G>
{
    fn new(graph: &'a mut G, params: &'a HashMap<String, i64>) -> Self {
        Self {
            graph,
            params,
            register_indices: None,
            isa_consts: HashMap::new(),
            next_symbol_id: 0,
            register_symbols: HashMap::new(),
            variable_symbols: HashMap::new(),
            regnum_symbols: HashMap::new(),
            had_error: false,
            lambda_params: Vec::new(),
        }
    }

    fn new_with_registers(
        graph: &'a mut G,
        params: &'a HashMap<String, i64>,
        register_indices: &'a HashMap<(String, String), u32>,
    ) -> Self {
        Self {
            graph,
            params,
            register_indices: Some(register_indices),
            isa_consts: HashMap::new(),
            next_symbol_id: 0,
            register_symbols: HashMap::new(),
            variable_symbols: HashMap::new(),
            regnum_symbols: HashMap::new(),
            had_error: false,
            lambda_params: Vec::new(),
        }
    }

    fn add_node(
        &mut self,
        kind: tir::sem::SymKind,
        children: &[tir::graph::NodeId],
    ) -> tir::graph::NodeId {
        let node = self.graph.add_node(kind);
        for &child in children {
            self.graph.add_edge(node, child);
        }
        node
    }

    fn add_leaf(
        &mut self,
        kind: tir::sem::SymKind,
        data: tir::sem::SymPayload<tir::ValueId>,
    ) -> tir::graph::NodeId {
        let node = self.graph.add_node(kind);
        self.graph.set_leaf_data(node, data);
        node
    }

    fn add_int_const(&mut self, value: tir_adt::APInt) -> tir::graph::NodeId {
        self.add_leaf(
            tir::sem::SymKind::Constant,
            tir::sem::SymPayload::Int(value),
        )
    }

    fn add_bool_const(&mut self, value: bool) -> tir::graph::NodeId {
        self.add_int_const(tir_adt::APInt::new(1, value as u64))
    }

    fn alloc_variable_symbol(&mut self) -> u32 {
        let id = self.next_symbol_id;
        self.next_symbol_id += 1;
        id
    }

    fn get_or_create_variable_symbol(&mut self, name: String) -> u32 {
        if let Some(&id) = self.variable_symbols.get(&name) {
            return id;
        }
        let id = self.alloc_variable_symbol();
        self.variable_symbols.insert(name, id);
        id
    }

    fn get_or_create_register_symbol(&mut self, class: String, number: u32) -> u32 {
        if let Some(&id) = self.register_symbols.get(&(class.clone(), number)) {
            return id;
        }

        let id = self.alloc_variable_symbol();
        self.register_symbols.insert((class, number), id);
        id
    }

    fn get_or_create_regnum_symbol(&mut self, name: String) -> u32 {
        if let Some(&id) = self.regnum_symbols.get(&name) {
            return id;
        }
        let id = self.alloc_variable_symbol();
        self.regnum_symbols.insert(name, id);
        id
    }

    /// Lower a `map`/`reduce` lambda's body, binding its parameters so that
    /// references to them become `Arg` nodes. Non-lambda arguments are an error
    /// (caught by the type checker); lowering them directly keeps the graph valid.
    fn lower_lambda_body(&mut self, arg: &Expr) -> tir::graph::NodeId {
        let Expr::Lambda(lambda) = arg else {
            self.had_error = true;
            return arg.lower_with_ctx(self);
        };
        self.lambda_params.push(lambda.params.clone());
        let body = lambda.body.lower_with_ctx(self);
        self.lambda_params.pop();
        body
    }

    fn build_extract(
        &mut self,
        input_node: tir::graph::NodeId,
        high_node: tir::graph::NodeId,
        low_node: tir::graph::NodeId,
    ) -> tir::graph::NodeId {
        // A single canonical `Extract` node rather than a shift/and/mask tree, so
        // instruction selection can match truncation/bit-slicing structurally
        // (e.g. addw = sext(extract(rs1+rs2, 31, 0), XLEN)) instead of pattern-
        // matching a fragile arithmetic expansion.
        self.add_node(
            tir::sem::SymKind::Extract,
            &[input_node, high_node, low_node],
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct File {
    pub items: Vec<Item>,
    pub file_name: String,
}

impl LitInt {
    pub fn new(value: String, span: Span) -> Self {
        Self { value, span }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    fn parse_u64(&self) -> u64 {
        if self.value.starts_with("0x") || self.value.starts_with("0X") {
            u64::from_str_radix(&self.value[2..], 16).expect("invalid hex literal")
        } else if self.value.starts_with("0b") || self.value.starts_with("0B") {
            u64::from_str_radix(&self.value[2..], 2).expect("invalid binary literal")
        } else {
            self.value.parse::<u64>().expect("invalid integer literal")
        }
    }
}

impl LitStr {
    pub fn new(value: String, span: Span) -> Self {
        Self { value, span }
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

impl From<LitInt> for Expr {
    fn from(val: LitInt) -> Self {
        Expr::Lit(Lit::Int(val))
    }
}

impl From<LitStr> for Expr {
    fn from(val: LitStr) -> Self {
        Expr::Lit(Lit::Str(val))
    }
}

impl Ident {
    pub fn new(name: String, span: Span) -> Ident {
        Ident { name, span }
    }
}

impl From<Ident> for Expr {
    fn from(val: Ident) -> Self {
        Expr::Ident(val)
    }
}

impl From<Block> for Expr {
    fn from(val: Block) -> Self {
        Expr::Block(val)
    }
}

impl From<If> for Expr {
    fn from(val: If) -> Self {
        Expr::If(val)
    }
}

impl Expr {
    fn lower_with_ctx<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        // Inside a `map`/`reduce` lambda, a reference to one of its parameters
        // lowers to an `Arg` leaf carrying the parameter's position.
        if let Expr::Ident(id) = self
            && let Some(params) = ctx.lambda_params.last()
            && let Some(idx) = params.iter().position(|p| p == &id.name)
        {
            return ctx.add_leaf(
                tir::sem::SymKind::Arg,
                tir::sem::SymPayload::Int(tir_adt::APInt::new(32, idx as u64)),
            );
        }
        match self {
            Expr::Assign(x) => x.as_sema_expr(ctx),
            Expr::Binary(x) => x.as_sema_expr(ctx),
            Expr::Unary(x) => x.as_sema_expr(ctx),
            Expr::Block(x) => x.as_sema_expr(ctx),
            Expr::Call(x) => x.as_sema_expr(ctx),
            Expr::Field(x) => x.as_sema_expr(ctx),
            Expr::Ident(x) => x.as_sema_expr(ctx),
            Expr::If(x) => x.as_sema_expr(ctx),
            Expr::IndexAccess(x) => x.as_sema_expr(ctx),
            Expr::Path(x) => x.as_sema_expr(ctx),
            Expr::Lit(x) => x.as_sema_expr(ctx),
            Expr::Slice(x) => x.as_sema_expr(ctx),
            // Semantic expressions model the no-trap path; only the SMT
            // backend gives the handlers meaning.
            Expr::Try(x) => x.body.lower_with_ctx(ctx),
            Expr::BuiltinFunction(_) => panic!("builtin functions must be called"),
            // Lambdas are lowered by the `map`/`reduce` builtins that consume them,
            // which push their parameters and lower the body directly.
            Expr::Lambda(_) => panic!("lambda only valid as a map/reduce argument"),
            Expr::Invalid => panic!("cannot convert invalid expression"),
        }
    }

    pub fn as_sema_expr(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
    ) -> tir::graph::NodeId {
        self.as_sema_expr_with_params(g, &HashMap::new())
    }

    pub(crate) fn as_sema_expr_with_params(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
        params: &HashMap<String, i64>,
    ) -> tir::graph::NodeId {
        let mut ctx = SemaExprLoweringCtx::new(g, params);
        self.lower_with_ctx(&mut ctx)
    }

    /// Lower this expression into a semantic expression graph, returning the
    /// symbol table alongside the root node. Returns `None` if the expression
    /// contains operations that cannot be represented.
    pub fn lower_to_sema(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
        params: &HashMap<String, i64>,
    ) -> Option<SemaLowering> {
        let mut ctx = SemaExprLoweringCtx::new(g, params);
        let root = self.lower_with_ctx(&mut ctx);
        if ctx.had_error {
            return None;
        }
        Some(SemaLowering {
            root,
            variable_symbols: ctx.variable_symbols,
            register_symbols: ctx.register_symbols,
            regnum_symbols: ctx.regnum_symbols,
        })
    }

    /// Like [`Expr::lower_to_sema`], but supplies ISA parameter values used to
    /// const-evaluate a vector map's lane count, so the lowered pattern carries a
    /// concrete width that instruction selection can match.
    pub fn lower_to_sema_with_isa(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
        params: &HashMap<String, i64>,
        isa_consts: &HashMap<String, i64>,
        register_indices: &HashMap<(String, String), u32>,
    ) -> Option<SemaLowering> {
        let mut ctx = SemaExprLoweringCtx::new_with_registers(g, params, register_indices);
        ctx.isa_consts = isa_consts.clone();
        let root = self.lower_with_ctx(&mut ctx);
        if ctx.had_error {
            return None;
        }
        Some(SemaLowering {
            root,
            variable_symbols: ctx.variable_symbols,
            register_symbols: ctx.register_symbols,
            regnum_symbols: ctx.regnum_symbols,
        })
    }

    /// Lower several expressions into one graph through a single shared symbol
    /// table, so an operand referenced by more than one expression binds the
    /// same symbol id in each (a flag definer's per-flag semantics all read the
    /// same `rn`/`rm`). Returns each expression's root in order.
    pub fn lower_all_to_sema_with_isa(
        exprs: &[&Expr],
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
        params: &HashMap<String, i64>,
        isa_consts: &HashMap<String, i64>,
        register_indices: &HashMap<(String, String), u32>,
    ) -> Option<(Vec<tir::graph::NodeId>, SemaLowering)> {
        let mut ctx = SemaExprLoweringCtx::new_with_registers(g, params, register_indices);
        ctx.isa_consts = isa_consts.clone();
        let roots: Vec<_> = exprs
            .iter()
            .map(|expr| expr.lower_with_ctx(&mut ctx))
            .collect();
        if ctx.had_error {
            return None;
        }
        let root = *roots.last()?;
        Some((
            roots,
            SemaLowering {
                root,
                variable_symbols: ctx.variable_symbols,
                register_symbols: ctx.register_symbols,
                regnum_symbols: ctx.regnum_symbols,
            },
        ))
    }

    /// Like [`Expr::lower_to_sema`], but resolves index-less register paths (e.g.
    /// status flags such as `PSTATE::z`) through `register_indices`, a
    /// `(class, register-name) -> index` table derived from the register-class
    /// definitions. Used by simulator codegen so flag reads and writes resolve to a
    /// stable register slot instead of failing to lower.
    pub fn lower_to_sema_with_registers(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem::SymKind,
            Leaf = tir::sem::SymPayload<tir::ValueId>,
        >,
        params: &HashMap<String, i64>,
        register_indices: &HashMap<(String, String), u32>,
    ) -> Option<SemaLowering> {
        let mut ctx = SemaExprLoweringCtx::new_with_registers(g, params, register_indices);
        let root = self.lower_with_ctx(&mut ctx);
        if ctx.had_error {
            return None;
        }
        Some(SemaLowering {
            root,
            variable_symbols: ctx.variable_symbols,
            register_symbols: ctx.register_symbols,
            regnum_symbols: ctx.regnum_symbols,
        })
    }
}

impl Assign {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        self.value.lower_with_ctx(ctx)
    }
}

impl Lit {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        match self {
            Lit::Int(lit_int) => {
                let value = lit_int.parse_u64();

                let width = if value == 0 {
                    1
                } else {
                    64 - value.leading_zeros()
                };

                ctx.add_int_const(tir_adt::APInt::new(width, value))
            }
            Lit::Str(_) => panic!("string literals are not supported in semantic expressions"),
        }
    }
}

impl Ident {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        if let Some(&value) = ctx.params.get(&self.name) {
            let (width, abs_value) = if value < 0 {
                let abs = value.unsigned_abs();
                let width = if abs == 0 {
                    1
                } else {
                    64 - abs.leading_zeros() + 1
                };
                (width, abs)
            } else {
                let v = value as u64;
                let width = if v == 0 { 1 } else { 64 - v.leading_zeros() };
                (width, v)
            };

            if value < 0 {
                ctx.add_int_const(tir_adt::APInt::new_signed(width, value))
            } else {
                ctx.add_int_const(tir_adt::APInt::new(width, abs_value))
            }
        } else {
            let id = ctx.get_or_create_variable_symbol(self.name.clone());
            ctx.add_leaf(
                tir::sem::SymKind::Symbol,
                tir::sem::SymPayload::SymbolId(id),
            )
        }
    }
}

impl Path {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        if self.remainder.len() != 1 {
            ctx.had_error = true;
            return ctx.add_int_const(tir_adt::APInt::new(64, 0));
        }

        // `Ordering::<member>` is a `bits<3>` memory-ordering constant, resolved
        // before register-class handling since `Ordering` is not a class.
        if self.base == "Ordering" {
            let code = ordering_code(&self.remainder[0]).unwrap_or_else(|| {
                ctx.had_error = true;
                0
            });
            return ctx.add_int_const(tir_adt::APInt::new(3, code as u64));
        }

        let reg_name = &self.remainder[0];
        // Resolve the register's encoding index: PC is special; otherwise prefer the
        // `(class, name)` table (which gives index-less registers like status flags a
        // stable slot), falling back to a trailing numeric index in the name. A path
        // that resolves to neither is unrepresentable, so mark the lowering failed
        // rather than panicking — callers turn that into a skipped/None lowering.
        let number = if self.base == "PC" && reg_name == "pc" {
            Some(0)
        } else if let Some(indices) = ctx.register_indices {
            indices.get(&(self.base.clone(), reg_name.clone())).copied()
        } else {
            reg_name
                .find(|c: char| c.is_ascii_digit())
                .and_then(|start| reg_name[start..].parse::<u32>().ok())
        };

        let Some(number) = number else {
            ctx.had_error = true;
            return ctx.add_int_const(tir_adt::APInt::new(64, 0));
        };

        let symbol_id = ctx.get_or_create_register_symbol(self.base.clone(), number);
        ctx.add_leaf(
            tir::sem::SymKind::Symbol,
            tir::sem::SymPayload::SymbolId(symbol_id),
        )
    }
}

impl Field {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        if let Expr::Ident(base_ident) = &*self.base {
            if base_ident.name == "self" {
                return Ident::new(self.member.clone(), self.span).as_sema_expr(ctx);
            }

            let register_number = if let Some(num_str) = self.member.strip_prefix('x') {
                num_str
                    .parse::<u32>()
                    .expect("invalid register number in field access")
            } else {
                self.member
                    .parse::<u32>()
                    .expect("invalid register number in field access")
            };

            let symbol_id =
                ctx.get_or_create_register_symbol(base_ident.name.clone(), register_number);
            ctx.add_leaf(
                tir::sem::SymKind::Symbol,
                tir::sem::SymPayload::SymbolId(symbol_id),
            )
        } else {
            panic!("register field access requires base to be an identifier")
        }
    }
}

impl Binary {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let lhs = self.lhs.lower_with_ctx(ctx);
        let rhs = self.rhs.lower_with_ctx(ctx);

        use tir::sem::SymKind as K;

        match self.op {
            BinOp::Add => ctx.add_node(K::Add, &[lhs, rhs]),
            BinOp::Sub => ctx.add_node(K::Sub, &[lhs, rhs]),
            BinOp::Mul => ctx.add_node(K::Mul, &[lhs, rhs]),
            BinOp::Div => ctx.add_node(K::Div, &[lhs, rhs]),
            BinOp::UnsignedDiv => ctx.add_node(K::UDiv, &[lhs, rhs]),
            BinOp::Equal => ctx.add_node(K::Eq, &[lhs, rhs]),
            BinOp::NotEqual => ctx.add_node(K::Ne, &[lhs, rhs]),
            BinOp::LessThan => ctx.add_node(K::Lt, &[lhs, rhs]),
            BinOp::GreaterThan => ctx.add_node(K::Gt, &[lhs, rhs]),
            BinOp::LessThenEqual => ctx.add_node(K::Ge, &[rhs, lhs]),
            BinOp::GreaterThanEqual => ctx.add_node(K::Ge, &[lhs, rhs]),
            BinOp::UnsignedLessThan => ctx.add_node(K::ULt, &[lhs, rhs]),
            BinOp::UnsignedGreaterThan => ctx.add_node(K::UGt, &[lhs, rhs]),
            BinOp::UnsignedLessThenEqual => ctx.add_node(K::UGe, &[rhs, lhs]),
            BinOp::UnsignedGreaterThanEqual => ctx.add_node(K::UGe, &[lhs, rhs]),
            BinOp::BitwiseAnd => ctx.add_node(K::And, &[lhs, rhs]),
            BinOp::BitwiseOr => ctx.add_node(K::Or, &[lhs, rhs]),
            BinOp::BitwiseXor => ctx.add_node(K::Xor, &[lhs, rhs]),
            BinOp::ShiftLeftLogical => ctx.add_node(K::ShiftLeft, &[lhs, rhs]),
            BinOp::ShiftRightLogical => ctx.add_node(K::ShiftRightLogic, &[lhs, rhs]),
            BinOp::ShiftRightArithmetic => ctx.add_node(K::ShiftRightArithmetic, &[lhs, rhs]),
        }
    }
}

impl Unary {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        use tir::sem::SymKind as K;

        match self.op {
            UnOp::BitwiseNot => {
                // `~literal` must take its width from the surrounding
                // expression, but a literal lowers at its own minimal width,
                // where the inversion would lose the high bits (`~1` at width
                // 1 is 0). Fold it to a signed constant instead: `~v` is
                // `-v - 1`, and sign-extension during width coercion then
                // yields the right bit pattern at any width (`~1` -> -2 ->
                // 0b11..10).
                if let Expr::Lit(Lit::Int(lit)) = &*self.x {
                    let value = !(lit.parse_u64()) as i64;
                    return if value < 0 {
                        let width = 64 - value.unsigned_abs().leading_zeros() + 1;
                        ctx.add_int_const(tir_adt::APInt::new_signed(width, value))
                    } else {
                        let v = value as u64;
                        let width = if v == 0 { 1 } else { 64 - v.leading_zeros() };
                        ctx.add_int_const(tir_adt::APInt::new(width, v))
                    };
                }

                let x = self.x.lower_with_ctx(ctx);
                ctx.add_node(K::Not, &[x])
            }
        }
    }
}

impl If {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let cond = self.cond.lower_with_ctx(ctx);
        let then_ = self.then.lower_with_ctx(ctx);
        let else_ = if let Some(else_expr) = &self.else_ {
            else_expr.lower_with_ctx(ctx)
        } else {
            ctx.add_bool_const(false)
        };

        ctx.add_node(tir::sem::SymKind::If, &[cond, then_, else_])
    }
}

impl Block {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        if self.stmts.is_empty() {
            ctx.add_bool_const(false)
        } else {
            self.stmts
                .last()
                .expect("non-empty block must have last expr")
                .lower_with_ctx(ctx)
        }
    }
}

impl Slice {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let input = self.base.lower_with_ctx(ctx);
        let high = Lit::Int(LitInt::new(self.end.to_string(), self.span)).as_sema_expr(ctx);
        let low = Lit::Int(LitInt::new(self.start.to_string(), self.span)).as_sema_expr(ctx);
        ctx.build_extract(input, high, low)
    }
}

impl IndexAccess {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let input = self.base.lower_with_ctx(ctx);
        let idx = Lit::Int(LitInt::new(self.index.to_string(), self.span)).as_sema_expr(ctx);
        ctx.build_extract(input, idx, idx)
    }
}

/// A constant `Ordering::<member>` or integer literal, or `None` for anything
/// else (e.g. a decoded aq/rl operand feeding a future dynamic ordering).
fn const_eval_u64(expr: &Expr) -> Option<u64> {
    match expr {
        Expr::Lit(Lit::Int(li)) => Some(li.parse_u64()),
        Expr::Path(p) if p.base == "Ordering" && p.remainder.len() == 1 => {
            ordering_code(&p.remainder[0]).map(u64::from)
        }
        _ => None,
    }
}

/// Pack a `load`/`store` inert metadata operand: bit 0 keeps its old meaning
/// (load signedness hint / store address space), bits 3:1 carry the ordering
/// code. `base_meta` is the load's metadata arg (`None` for store, whose bit 0
/// is always 0); `ordering` is the optional trailing ordering arg. A missing
/// ordering reproduces the pre-atomics IR exactly (relaxed = 0).
fn pack_ordering_meta<G>(
    ctx: &mut SemaExprLoweringCtx<'_, G>,
    base_meta: Option<&Expr>,
    ordering: Option<&Expr>,
) -> tir::graph::NodeId
where
    G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
{
    let Some(ordering) = ordering else {
        return match base_meta {
            Some(m) => m.lower_with_ctx(ctx),
            None => ctx.add_int_const(tir_adt::APInt::new(1, 0)),
        };
    };

    let meta_bit0 = match base_meta {
        Some(m) => const_eval_u64(m),
        None => Some(0),
    };
    if let (Some(code), Some(bit0)) = (const_eval_u64(ordering), meta_bit0) {
        let packed = (code << 1) | (bit0 & 1);
        return ctx.add_int_const(tir_adt::APInt::new(4, packed));
    }

    // Dynamic ordering: concat(ordering[2:0], bit0) — ordering in the high bits.
    let ord_node = ordering.lower_with_ctx(ctx);
    let bit0_node = match base_meta {
        Some(m) => {
            let m = m.lower_with_ctx(ctx);
            let zero = ctx.add_int_const(tir_adt::APInt::new(1, 0));
            ctx.build_extract(m, zero, zero)
        }
        None => ctx.add_int_const(tir_adt::APInt::new(1, 0)),
    };
    ctx.add_node(tir::sem::SymKind::Concat, &[ord_node, bit0_node])
}

impl Call {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let Expr::BuiltinFunction(builtin) = &*self.callee else {
            panic!("only builtin functions are supported");
        };

        match builtin {
            BuiltinFunction::Clamp => {
                assert!(self.arguments.len() == 3, "clamp requires 3 arguments");
                let input = self.arguments[0].lower_with_ctx(ctx);
                let min = self.arguments[1].lower_with_ctx(ctx);
                let max = self.arguments[2].lower_with_ctx(ctx);
                ctx.add_node(tir::sem::SymKind::Clamp, &[input, min, max])
            }
            BuiltinFunction::Extract => {
                assert!(self.arguments.len() == 3, "extract requires 3 arguments");
                let input = self.arguments[0].lower_with_ctx(ctx);
                let high = self.arguments[1].lower_with_ctx(ctx);
                let low = self.arguments[2].lower_with_ctx(ctx);
                ctx.build_extract(input, high, low)
            }
            BuiltinFunction::Log2Ceil => {
                assert!(self.arguments.len() == 1, "log2Ceil requires 1 argument");
                let input = self.arguments[0].lower_with_ctx(ctx);
                ctx.add_node(tir::sem::SymKind::Log2Ceil, &[input])
            }
            BuiltinFunction::Regnum => {
                // The argument names a register operand; the result is a symbol
                // bound to that operand's encoding index, resolved per backend
                // (the interpreter reads the decoded attribute, the SMT encoding
                // uses the operand's index parameter).
                let Some(Expr::Ident(id)) = self.arguments.first() else {
                    ctx.had_error = true;
                    return ctx.add_int_const(tir_adt::APInt::new(64, 0));
                };
                let sym = ctx.get_or_create_regnum_symbol(id.name.clone());
                ctx.add_leaf(
                    tir::sem::SymKind::Symbol,
                    tir::sem::SymPayload::SymbolId(sym),
                )
            }
            BuiltinFunction::SExt => {
                assert!(self.arguments.len() == 2, "sext requires 2 arguments");
                let input = self.arguments[0].lower_with_ctx(ctx);
                let width = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem::SymKind::SExt, &[input, width])
            }
            BuiltinFunction::ZExt => {
                assert!(self.arguments.len() == 2, "zext requires 2 arguments");
                let input = self.arguments[0].lower_with_ctx(ctx);
                let width = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem::SymKind::ZExt, &[input, width])
            }
            BuiltinFunction::Load => {
                assert!(
                    matches!(self.arguments.len(), 3 | 4),
                    "load requires 3 or 4 arguments"
                );
                let address = self.arguments[0].lower_with_ctx(ctx);
                let bytes = self.arguments[1].lower_with_ctx(ctx);
                let metadata =
                    pack_ordering_meta(ctx, Some(&self.arguments[2]), self.arguments.get(3));
                ctx.add_node(tir::sem::SymKind::LoadMemory, &[address, bytes, metadata])
            }
            BuiltinFunction::Store => {
                assert!(
                    matches!(self.arguments.len(), 3 | 4),
                    "store requires 3 or 4 arguments"
                );
                let address = self.arguments[0].lower_with_ctx(ctx);
                let bytes = self.arguments[1].lower_with_ctx(ctx);
                let value = self.arguments[2].lower_with_ctx(ctx);
                let address_space = pack_ordering_meta(ctx, None, self.arguments.get(3));
                ctx.add_node(
                    tir::sem::SymKind::StoreMemory,
                    &[address, bytes, value, address_space],
                )
            }
            BuiltinFunction::LoadReserved => {
                assert!(
                    self.arguments.len() == 3,
                    "load_reserved requires 3 arguments"
                );
                let address = self.arguments[0].lower_with_ctx(ctx);
                let bytes = self.arguments[1].lower_with_ctx(ctx);
                let ordering = self.arguments[2].lower_with_ctx(ctx);
                ctx.add_node(tir::sem::SymKind::LoadReserved, &[address, bytes, ordering])
            }
            BuiltinFunction::StoreConditional => {
                assert!(
                    self.arguments.len() == 4,
                    "store_conditional requires 4 arguments"
                );
                let address = self.arguments[0].lower_with_ctx(ctx);
                let bytes = self.arguments[1].lower_with_ctx(ctx);
                let value = self.arguments[2].lower_with_ctx(ctx);
                let ordering = self.arguments[3].lower_with_ctx(ctx);
                ctx.add_node(
                    tir::sem::SymKind::StoreConditional,
                    &[address, bytes, value, ordering],
                )
            }
            BuiltinFunction::AtomicRmw => {
                assert!(self.arguments.len() == 5, "atomic_rmw requires 5 arguments");
                // Arg 0 is the op selector, a bare identifier from the closed set;
                // sema has already validated it, so an unknown name is a lowering
                // failure rather than a panic.
                let op_code = match &self.arguments[0] {
                    Expr::Ident(id) => atomic_rmw_op_code(&id.name),
                    _ => None,
                };
                let Some(op_code) = op_code else {
                    ctx.had_error = true;
                    return ctx.add_int_const(tir_adt::APInt::new(64, 0));
                };
                let op = ctx.add_int_const(tir_adt::APInt::new(4, op_code as u64));
                let address = self.arguments[1].lower_with_ctx(ctx);
                let bytes = self.arguments[2].lower_with_ctx(ctx);
                let value = self.arguments[3].lower_with_ctx(ctx);
                let ordering = self.arguments[4].lower_with_ctx(ctx);
                ctx.add_node(
                    tir::sem::SymKind::AtomicRmw,
                    &[op, address, bytes, value, ordering],
                )
            }
            BuiltinFunction::Fence => {
                assert!(self.arguments.len() == 2, "fence requires 2 arguments");
                let pred = self.arguments[0].lower_with_ctx(ctx);
                let succ = self.arguments[1].lower_with_ctx(ctx);
                let kind = ctx.add_int_const(tir_adt::APInt::new(1, 0));
                ctx.add_node(tir::sem::SymKind::Fence, &[pred, succ, kind])
            }
            BuiltinFunction::FenceI => {
                assert!(self.arguments.is_empty(), "fence_i requires 0 arguments");
                let zero = ctx.add_int_const(tir_adt::APInt::new(1, 0));
                let kind = ctx.add_int_const(tir_adt::APInt::new(1, 1));
                ctx.add_node(tir::sem::SymKind::Fence, &[zero, zero, kind])
            }
            // trap has no semantic-expression form; codegen intercepts trap
            // calls before lowering, so reaching here means the behavior used
            // it in a value position.
            BuiltinFunction::Trap => {
                ctx.had_error = true;
                ctx.add_int_const(tir_adt::APInt::new(64, 0))
            }
            BuiltinFunction::Split => {
                // `split(x, n)` cuts x into n equal lanes; `split(x, n, w)`
                // takes n lanes of w bits from the low end (the RVV shape,
                // where `vl`/SEW bound the active elements independent of the
                // register's total width).
                assert!(
                    matches!(self.arguments.len(), 2 | 3),
                    "split requires 2 or 3 arguments"
                );
                let children: Vec<_> = self
                    .arguments
                    .iter()
                    .map(|arg| arg.lower_with_ctx(ctx))
                    .collect();
                ctx.add_node(tir::sem::SymKind::Split, &children)
            }
            BuiltinFunction::Concat => {
                assert!(self.arguments.len() == 1, "concat requires 1 argument");
                let iter = self.arguments[0].lower_with_ctx(ctx);
                ctx.add_node(tir::sem::SymKind::IterConcat, &[iter])
            }
            BuiltinFunction::Zip => {
                assert!(self.arguments.len() == 2, "zip requires 2 arguments");
                let lhs = self.arguments[0].lower_with_ctx(ctx);
                let rhs = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem::SymKind::Zip, &[lhs, rhs])
            }
            BuiltinFunction::Map => {
                assert!(self.arguments.len() == 2, "map requires 2 arguments");
                let iter = self.arguments[0].lower_with_ctx(ctx);
                let body = ctx.lower_lambda_body(&self.arguments[1]);
                ctx.add_node(tir::sem::SymKind::Map, &[iter, body])
            }
            BuiltinFunction::Reduce => {
                assert!(self.arguments.len() == 2, "reduce requires 2 arguments");
                let iter = self.arguments[0].lower_with_ctx(ctx);
                let body = ctx.lower_lambda_body(&self.arguments[1]);
                ctx.add_node(tir::sem::SymKind::Reduce, &[iter, body])
            }
            BuiltinFunction::FAdd
            | BuiltinFunction::FSub
            | BuiltinFunction::FMul
            | BuiltinFunction::FDiv => {
                let kind = match builtin {
                    BuiltinFunction::FAdd => tir::sem::SymKind::FAdd,
                    BuiltinFunction::FSub => tir::sem::SymKind::FSub,
                    BuiltinFunction::FMul => tir::sem::SymKind::FMul,
                    _ => tir::sem::SymKind::FDiv,
                };
                assert!(
                    self.arguments.len() == 2,
                    "float arithmetic requires 2 arguments"
                );
                let lhs = self.arguments[0].lower_with_ctx(ctx);
                let rhs = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(kind, &[lhs, rhs])
            }
            // `todo()` marks unmodeled semantics; rustgen suppresses selection-rule
            // and `execute()` lowering for such behaviors, so this is never reached.
            BuiltinFunction::Todo => {
                unreachable!("todo() has no semantics to lower")
            }
        }
    }
}

impl Item {
    pub fn name(&self) -> &str {
        match self {
            Item::Isa(isa) => &isa.name,
            Item::Instruction(inst) => &inst.name,
            Item::RegisterClass(rc) => &rc.name,
            Item::Template(tmpl) => &tmpl.name,
            Item::Unit(su) => &su.name,
            Item::Machine(m) => &m.name,
        }
    }

    pub fn as_register_class(&self) -> Option<&RegisterClass> {
        match self {
            Item::RegisterClass(rc) => Some(rc),
            _ => None,
        }
    }

    pub fn as_instruction(&self) -> Option<&Instruction> {
        match self {
            Item::Instruction(i) => Some(i),
            _ => None,
        }
    }

    pub fn as_unit(&self) -> Option<&SchedClassDecl> {
        match self {
            Item::Unit(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_machine(&self) -> Option<&Machine> {
        match self {
            Item::Machine(m) => Some(m),
            _ => None,
        }
    }
}

impl Serialize for Type {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("Type", 2)?;
        match self {
            Type::String => {
                state.serialize_field("name", "String")?;
            }
            Type::Integer => {
                state.serialize_field("name", "Integer")?;
            }
            Type::Bits(width) => {
                state.serialize_field("name", "Bits")?;
                state.serialize_field("width", width)?;
            }
            Type::BitsExpr(expr) => {
                state.serialize_field("name", "BitsExpr")?;
                state.serialize_field("width", expr)?;
            }
            Type::Struct(name) => {
                state.serialize_field("name", "Struct")?;
                state.serialize_field("struct", name)?;
            }
            _ => unreachable!("Other types should not be part of AST"),
        }
        state.end()
    }
}

impl RegisterClass {
    pub fn register_name_tables(&self) -> RegisterNameTables {
        let mut entries = self
            .resolve_registers()
            .map(|reg| {
                (
                    reg.encoding_index().unwrap_or(u16::MAX),
                    reg.name,
                    reg.alias,
                )
            })
            .collect::<Vec<_>>();
        entries.sort_by_key(|(idx, _, _)| *idx);

        let mut next_alias_index = HashMap::new();
        entries.into_iter().fold(
            RegisterNameTables {
                parse_names: Vec::new(),
                isa_names: Vec::new(),
                abi_names: Vec::new(),
            },
            |mut out, (idx, isa_name, alias)| {
                if idx != u16::MAX {
                    out.parse_names.push((isa_name.clone(), idx));
                    out.isa_names.push((idx, isa_name));
                }

                if let Some(alias_name) = alias {
                    let full_alias = if alias_name.contains("{}") {
                        let stem = alias_name.replace("{}", "");
                        let counter = next_alias_index.entry(stem.clone()).or_insert(0);
                        let alias = format!("{}{}", stem, *counter);
                        *counter += 1;
                        alias
                    } else {
                        alias_name
                    };
                    out.parse_names.push((full_alias.clone(), idx));
                    out.abi_names.push((idx, full_alias));
                }

                out
            },
        )
    }

    /// Whether this class holds the program counter.
    pub fn has_program_counter(&self) -> bool {
        self.resolve_registers()
            .any(|reg| reg.traits.contains(&RegisterTrait::ProgramCounter))
    }

    /// Whether this class holds condition-code bits (`status_flag` registers).
    pub fn has_status_flags(&self) -> bool {
        self.resolve_registers()
            .any(|reg| reg.traits.contains(&RegisterTrait::StatusFlag))
    }

    /// Whether this class holds floating-point values (`float` registers).
    pub fn has_float_registers(&self) -> bool {
        self.resolve_registers()
            .any(|reg| reg.traits.contains(&RegisterTrait::Float))
    }

    pub fn hardwired_zero_register_index(&self) -> Option<u16> {
        self.resolve_registers().find_map(|reg| {
            reg.traits
                .iter()
                .any(|t| matches!(t, RegisterTrait::HardwiredZero))
                .then(|| reg.encoding_index().unwrap_or(u16::MAX))
        })
    }

    /// Maps each register's name — and its ABI alias, when fixed — to its canonical
    /// encoding index. The index is the trailing number in the name when present
    /// (`x5` -> 5), otherwise the register's ordinal position in declaration order
    /// (status flags `n`, `z`, `c`, `v` -> 0, 1, 2, 3). This gives index-less
    /// registers a stable slot the simulator can address, while leaving numbered
    /// registers at the index their operand encoding already uses.
    pub fn register_indices(&self) -> Vec<(String, u16)> {
        let mut out = Vec::new();
        for (position, reg) in self.resolve_registers().enumerate() {
            let index = reg.encoding_index().unwrap_or(position as u16);
            out.push((reg.name.clone(), index));
            if let Some(alias) = &reg.alias
                && !alias.contains("{}")
                && alias != &reg.name
            {
                out.push((alias.clone(), index));
            }
        }
        out
    }

    /// Every register that carries a concrete encoding index, paired with its
    /// traits, sorted by index. Registers without a trailing index (e.g. `pc`) are
    /// skipped — they have no encodable slot and are never allocated.
    pub fn indexed_registers(&self) -> Vec<(u16, Vec<RegisterTrait>)> {
        let mut regs = self
            .resolve_registers()
            .filter_map(|reg| reg.encoding_index().map(|idx| (idx, reg.traits)))
            .collect::<Vec<_>>();
        regs.sort_by_key(|(idx, _)| *idx);
        regs
    }

    /// Register allocation metadata derived from per-register traits: which
    /// registers are allocatable (and in what preferred order), the caller/callee
    /// saved partitions, the ordered argument registers, and the return-value
    /// registers. Indices that are reserved by the ABI (zero, return address,
    /// stack/global/thread pointer, program counter) never appear in the
    /// allocation order.
    pub fn allocation_metadata(&self) -> RegisterAllocationMetadata {
        let regs = self.indexed_registers();
        let is_reserved = |traits: &[RegisterTrait]| {
            traits.iter().any(|t| {
                matches!(
                    t,
                    RegisterTrait::HardwiredZero
                        | RegisterTrait::ReturnAddress
                        | RegisterTrait::StackPointer
                        | RegisterTrait::ProgramCounter
                        | RegisterTrait::GlobalPointer
                        | RegisterTrait::ThreadPointer
                        | RegisterTrait::Reserved
                )
            })
        };
        let has = |traits: &[RegisterTrait], want: &RegisterTrait| traits.contains(want);

        let mut caller_saved = Vec::new();
        let mut callee_saved = Vec::new();
        let mut arguments = Vec::new();
        let mut return_values = Vec::new();
        let mut reserved = Vec::new();
        for (idx, traits) in &regs {
            if is_reserved(traits) {
                reserved.push(*idx);
            }
            if has(traits, &RegisterTrait::CallerSaved) && !is_reserved(traits) {
                caller_saved.push(*idx);
            }
            if has(traits, &RegisterTrait::CalleeSaved) && !is_reserved(traits) {
                callee_saved.push(*idx);
            }
            if has(traits, &RegisterTrait::Argument) {
                arguments.push(*idx);
            }
            if has(traits, &RegisterTrait::ReturnValue) {
                return_values.push(*idx);
            }
        }

        // Order argument registers by explicit `arg = N` positions when a class
        // declares them (the x86-64 System V order differs from index order); fall
        // back to index order otherwise.
        let arg_orders: HashMap<u16, u16> = self
            .resolve_registers()
            .filter_map(|reg| match (reg.encoding_index(), reg.arg_order) {
                (Some(idx), Some(order)) => Some((idx, order)),
                _ => None,
            })
            .collect();
        if !arg_orders.is_empty() {
            arguments.sort_by_key(|idx| arg_orders.get(idx).copied().unwrap_or(u16::MAX));
        }

        // Allocate caller-saved (scratch) registers first so short-lived values
        // avoid forcing a callee-saved register's save/restore.
        let allocation_order = caller_saved
            .iter()
            .chain(callee_saved.iter())
            .copied()
            .collect();

        RegisterAllocationMetadata {
            allocation_order,
            caller_saved,
            callee_saved,
            arguments,
            return_values,
            reserved,
        }
    }

    /// The name of the physical register file this class draws from: the root of
    /// its inheritance chain. Classes that share a file (e.g. AArch64 `GPR` and
    /// `GPRsp`) name the same physical register at a given encoding index, so the
    /// register allocator must treat those indices as aliases. A standalone class
    /// is its own file. `classes` maps every class name to its definition.
    pub fn register_file<'a>(&'a self, classes: &'a HashMap<String, &'a RegisterClass>) -> &'a str {
        if let Some(file) = &self.file {
            return file;
        }
        let mut current = self;
        let mut seen = std::collections::HashSet::new();
        while let Some(base_name) = &current.base {
            if !seen.insert(current.name.clone()) {
                break; // defensive: inheritance cycle
            }
            match classes.get(base_name) {
                Some(base) => current = base,
                None => break,
            }
        }
        &current.name
    }

    pub fn resolve_registers(&self) -> impl Iterator<Item = Register> {
        let mut registers = Vec::new();

        for def in &self.registers {
            match def {
                RegisterDef::Single(register) => registers.push(register.clone()),
                RegisterDef::Range(range) => {
                    let (Some(start_idx), Some(end_idx)) = (
                        parse_trailing_index(&range.start),
                        parse_trailing_index(&range.end),
                    ) else {
                        continue;
                    };

                    let prefix = strip_trailing_digits(&range.start);
                    for idx in start_idx..=end_idx {
                        registers.push(Register {
                            name: format!("{prefix}{idx}"),
                            alias: range.alias_pattern.clone(),
                            index: None,
                            arg_order: None,
                            traits: range.traits.clone(),
                            subregisters: Vec::new(),
                            span: range.span,
                        });
                    }
                }
            }
        }

        registers.into_iter()
    }
}

/// Flatten `register_class` inheritance in place: every class with a `base`
/// absorbs the base's parameters and (encoding-expanded) registers, then applies
/// its own declarations as overrides — parameters by name, registers by trailing
/// encoding index (or by name for index-less registers like `pc`). After this runs
/// every class carries its complete register set, so all downstream analysis
/// (typeck, sema, codegen) can treat classes as self-contained. `base` itself is
/// left intact so codegen can still recover the shared register file (see
/// [`RegisterClass::register_file`]).
pub fn resolve_register_class_inheritance(files: &mut [File]) {
    let raw: HashMap<String, RegisterClass> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .map(|rc| (rc.name.clone(), rc.clone()))
        .collect();

    fn merge(
        name: &str,
        raw: &HashMap<String, RegisterClass>,
        cache: &mut HashMap<String, RegisterClass>,
    ) -> RegisterClass {
        if let Some(done) = cache.get(name) {
            return done.clone();
        }
        let mut rc = raw
            .get(name)
            .cloned()
            .expect("merge called with a known class name");

        if let Some(base_name) = rc.base.clone() {
            // A dangling base is reported by sema; treat it as no inheritance here.
            if raw.contains_key(&base_name) && base_name != name {
                let base = merge(&base_name, raw, cache);

                let mut parameters = base.parameters.clone();
                for (key, value) in rc.parameters.iter() {
                    parameters.insert(key.clone(), value.clone());
                }

                let mut registers: Vec<Register> = base.resolve_registers().collect();
                for own in rc.resolve_registers() {
                    let key = own.encoding_index();
                    let existing = registers.iter().position(|r| match key {
                        Some(idx) => r.encoding_index() == Some(idx),
                        None => r.name == own.name,
                    });
                    match existing {
                        Some(pos) => registers[pos] = own,
                        None => registers.push(own),
                    }
                }

                rc.parameters = parameters;
                rc.registers = registers.into_iter().map(RegisterDef::Single).collect();
            }
        }

        cache.insert(name.to_string(), rc.clone());
        rc
    }

    let mut cache: HashMap<String, RegisterClass> = HashMap::new();
    for name in raw.keys() {
        merge(name, &raw, &mut cache);
    }

    for file in files.iter_mut() {
        for item in file.items.iter_mut() {
            if let Item::RegisterClass(rc) = item
                && let Some(merged) = cache.get(&rc.name)
            {
                rc.parameters = merged.parameters.clone();
                rc.registers = merged.registers.clone();
            }
        }
    }
}

fn parse_trailing_index(s: &str) -> Option<u16> {
    let mut i = s.len();
    while i > 0 && s.as_bytes()[i - 1].is_ascii_digit() {
        i -= 1;
    }
    if i < s.len() {
        s[i..].parse::<u16>().ok()
    } else {
        None
    }
}

fn strip_trailing_digits(s: &str) -> &str {
    let mut i = s.len();
    while i > 0 && s.as_bytes()[i - 1].is_ascii_digit() {
        i -= 1;
    }
    &s[..i]
}

impl File {
    pub fn isas(&self) -> impl Iterator<Item = &Isa> {
        self.items.iter().filter_map(|f| match f {
            Item::Isa(isa) => Some(isa),
            _ => None,
        })
    }

    pub fn templates(&self) -> impl Iterator<Item = &Template> {
        self.items.iter().filter_map(|f| match f {
            Item::Template(t) => Some(t),
            _ => None,
        })
    }

    pub fn instructions(&self) -> impl Iterator<Item = &Instruction> {
        self.items.iter().filter_map(|f| match f {
            Item::Instruction(i) => Some(i),
            _ => None,
        })
    }

    pub fn register_classes(&self) -> impl Iterator<Item = &RegisterClass> {
        self.items.iter().filter_map(|f| match f {
            Item::RegisterClass(rc) => Some(rc),
            _ => None,
        })
    }

    pub fn count(&self) -> impl Iterator<Item = &SchedClassDecl> {
        self.items.iter().filter_map(|f| match f {
            Item::Unit(s) => Some(s),
            _ => None,
        })
    }

    pub fn machines(&self) -> impl Iterator<Item = &Machine> {
        self.items.iter().filter_map(|f| match f {
            Item::Machine(m) => Some(m),
            _ => None,
        })
    }
}

#[derive(Serialize)]
struct ParamRef<'a> {
    name: &'a str,
    #[serde(rename = "type")]
    ty: &'a Type,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<&'a Expr>,
}

fn serialize_params<S>(
    params: &HashMap<String, (Type, Option<Expr>)>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut mapped: Vec<ParamRef<'_>> = params
        .iter()
        .map(|(name, (ty, val))| ParamRef {
            name,
            ty,
            value: val.as_ref(),
        })
        .collect();
    mapped.sort_by_key(|x| x.name);

    mapped.serialize(serializer)
}
