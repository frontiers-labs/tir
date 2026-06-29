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
    /// Carries an incoming argument under the calling convention. Argument order
    /// follows the register index order within the class (a0 before a1, ...).
    Argument,
    /// Holds an outgoing return value under the calling convention.
    ReturnValue,
    Temporary,
    Saved,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Register {
    pub name: String,
    pub alias: Option<String>,
    /// Explicit encoding index (`index = 0xC00`), for registers whose
    /// architectural number is not derivable from the name (e.g. CSRs).
    pub index: Option<u16>,
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
/// [`tir_be_common::sched::Protection`].
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
/// producer→consumer latency it grants. Mirrors [`tir_be_common::sched::Forward`].
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

/// `for VAR in START..END { body }`: a statement that runs `body` once for each
/// integer in the half-open range `[START, END)`, with `VAR` bound to the current
/// value. Bounds must be compile-time constants; the loop is unrolled before
/// lowering, so it carries no runtime iteration.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct For {
    pub var: String,
    pub start: Box<Expr>,
    pub end: Box<Expr>,
    pub body: Box<Expr>,
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
    SExt,
    ZExt,
    Load,
    Store,
    /// `lane(vector, index)`: read one lane of a vector value. Used inside a
    /// value-producing `for` loop to express elementwise vector behavior.
    Lane,
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
    For(For),
    IndexAccess(IndexAccess),
    Path(Path),
    Lit(Lit),
    Slice(Slice),
    Try(TryExcept),
    BuiltinFunction(BuiltinFunction),
    Lambda(Lambda),
    Invalid,
}

/// Evaluate a compile-time integer expression used as a `for` loop bound.
/// Supports literals, known constants (`consts`: ISA/instruction parameters and
/// enclosing loop variables), `self.PARAM`, and basic integer arithmetic.
pub(crate) fn eval_const_int(expr: &Expr, consts: &HashMap<String, i64>) -> Option<i64> {
    match expr {
        Expr::Lit(Lit::Int(li)) => Some(li.parse_u64() as i64),
        Expr::Ident(id) => consts.get(&id.name).copied(),
        Expr::Field(f) => match &*f.base {
            Expr::Ident(b) if b.name == "self" => consts.get(&f.member).copied(),
            _ => None,
        },
        Expr::Unary(u) => match u.op {
            UnOp::BitwiseNot => Some(!eval_const_int(&u.x, consts)?),
        },
        Expr::Binary(b) => {
            let l = eval_const_int(&b.lhs, consts)?;
            let r = eval_const_int(&b.rhs, consts)?;
            Some(match b.op {
                BinOp::Add => l + r,
                BinOp::Sub => l - r,
                BinOp::Mul => l * r,
                BinOp::Div | BinOp::UnsignedDiv if r != 0 => l / r,
                _ => return None,
            })
        }
        _ => None,
    }
}

/// Replace every `Ident` named `var` with the integer literal `value`.
fn subst_ident(expr: &mut Expr, var: &str, value: i64) {
    match expr {
        Expr::Ident(id) if id.name == var => {
            *expr = Expr::Lit(Lit::Int(LitInt::new(value.to_string(), id.span)));
        }
        Expr::Ident(_)
        | Expr::Lit(_)
        | Expr::Path(_)
        | Expr::BuiltinFunction(_)
        | Expr::Invalid => {}
        Expr::Assign(a) => {
            subst_ident(&mut a.dest, var, value);
            subst_ident(&mut a.value, var, value);
        }
        Expr::Binary(b) => {
            subst_ident(&mut b.lhs, var, value);
            subst_ident(&mut b.rhs, var, value);
        }
        Expr::Unary(u) => subst_ident(&mut u.x, var, value),
        Expr::Block(b) => {
            for stmt in &mut b.stmts {
                subst_ident(stmt, var, value);
            }
        }
        Expr::Call(c) => {
            subst_ident(&mut c.callee, var, value);
            for arg in &mut c.arguments {
                subst_ident(arg, var, value);
            }
        }
        Expr::Field(f) => subst_ident(&mut f.base, var, value),
        Expr::If(i) => {
            subst_ident(&mut i.cond, var, value);
            subst_ident(&mut i.then, var, value);
            if let Some(e) = &mut i.else_ {
                subst_ident(e, var, value);
            }
        }
        Expr::For(f) => {
            subst_ident(&mut f.start, var, value);
            subst_ident(&mut f.end, var, value);
            // A nested loop reusing the same variable name shadows the outer one.
            if f.var != var {
                subst_ident(&mut f.body, var, value);
            }
        }
        Expr::IndexAccess(i) => subst_ident(&mut i.base, var, value),
        Expr::Slice(s) => subst_ident(&mut s.base, var, value),
        Expr::Try(t) => {
            subst_ident(&mut t.body, var, value);
            for h in &mut t.handlers {
                subst_ident(&mut h.body, var, value);
            }
        }
        Expr::Lambda(l) => {
            // A parameter sharing the substituted name shadows it inside the body.
            if !l.params.iter().any(|p| p == var) {
                subst_ident(&mut l.body, var, value);
            }
        }
    }
}

/// Unroll a `for` loop into a `Block` of its body repeated once per iteration,
/// each copy binding the loop variable to its concrete value. Returns `None`
/// when the bounds are not compile-time constants.
pub(crate) fn unroll_for(f: &For, consts: &HashMap<String, i64>) -> Option<Expr> {
    let start = eval_const_int(&f.start, consts)?;
    let end = eval_const_int(&f.end, consts)?;
    let mut stmts = Vec::new();
    for i in start..end {
        let mut body = (*f.body).clone();
        subst_ident(&mut body, &f.var, i);
        stmts.push(body);
    }
    Some(Expr::Block(Block {
        stmts,
        last_expr_return: false,
        span: f.span,
    }))
}

/// Whether two expressions name the same assignment target (register operand,
/// register path, or status-flag field), ignoring spans.
fn same_target(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Ident(x), Expr::Ident(y)) => x.name == y.name,
        (Expr::Path(x), Expr::Path(y)) => x.base == y.base && x.remainder == y.remainder,
        (Expr::Field(x), Expr::Field(y)) => x.member == y.member && same_target(&x.base, &y.base),
        _ => false,
    }
}

impl For {
    /// If the body is a single assignment `dest = step` (optionally wrapped in a
    /// one-statement block), return `(dest, step)`. This accumulator form is what
    /// lowers to a first-class `Loop` node; other shapes fall back to unrolling.
    pub(crate) fn accumulator(&self) -> Option<(&Expr, &Expr)> {
        let body = match &*self.body {
            Expr::Block(b) if b.stmts.len() == 1 => &b.stmts[0],
            other => other,
        };
        match body {
            Expr::Assign(a) => Some((&a.dest, &a.value)),
            _ => None,
        }
    }

    /// If the body is a single value-producing expression (not an assignment),
    /// the loop is a vector map: `elem` is that per-lane expression. This is the
    /// counterpart of `accumulator`, lowering to a first-class `VectorMap` node.
    pub(crate) fn map_elem(&self) -> Option<&Expr> {
        let body = match &*self.body {
            Expr::Block(b) if b.stmts.len() == 1 => &b.stmts[0],
            other => other,
        };
        match body {
            Expr::Assign(_) | Expr::Invalid => None,
            // Effect-only statements (a bare `store`/`trap`) produce no lane value,
            // so they are unrolled as statements rather than mapped.
            Expr::Call(c)
                if matches!(
                    &*c.callee,
                    Expr::BuiltinFunction(BuiltinFunction::Store | BuiltinFunction::Trap)
                ) =>
            {
                None
            }
            elem => Some(elem),
        }
    }

    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let Some((dest, step)) = self.accumulator() else {
            // A value-producing loop is a vector map: `elem` lowered once with the
            // loop variable bound to the induction value, building a `VectorMap`
            // whose lane count is the (zero-based) range length.
            if let Some(elem) = self.map_elem() {
                // The lane count must be a compile-time constant so the lowered
                // pattern carries a concrete vector width that instruction
                // selection can match. The induction value ranges `0..count`, so
                // the loop must start at zero for `IndVar` to equal the loop
                // variable; otherwise fall back to lowering the bound expression.
                let mut consts = ctx.params.clone();
                for (name, value) in &ctx.isa_consts {
                    consts.entry(name.clone()).or_insert(*value);
                }
                let count = match (
                    eval_const_int(&self.start, &consts),
                    eval_const_int(&self.end, &consts),
                ) {
                    (Some(0), Some(end)) if end >= 0 => {
                        ctx.add_int_const(tir::utils::APInt::new(32, end as u64))
                    }
                    _ => self.end.lower_with_ctx(ctx),
                };
                let prev = ctx.loop_ctx.take();
                // A non-matching dest (Invalid) means `Acc` is never produced; only
                // the loop variable maps to the induction value.
                ctx.loop_ctx = Some((self.var.clone(), Expr::Invalid));
                let elem_node = elem.lower_with_ctx(ctx);
                ctx.loop_ctx = prev;
                return ctx.add_node(tir::sem_expr::ExprKind::VectorMap, &[count, elem_node]);
            }
            // Non-accumulator statement loops are not first-class values: unroll
            // constant bounds, otherwise the lowering cannot represent them.
            return match unroll_for(self, ctx.params) {
                Some(block) => block.lower_with_ctx(ctx),
                None => {
                    ctx.had_error = true;
                    ctx.add_int_const(tir::utils::APInt::new(64, 0))
                }
            };
        };

        let start = self.start.lower_with_ctx(ctx);
        let end = self.end.lower_with_ctx(ctx);
        // `init` is the accumulator's value at loop entry, lowered outside the
        // loop context so the destination reads its surrounding value.
        let init = dest.lower_with_ctx(ctx);

        let prev = ctx.loop_ctx.take();
        ctx.loop_ctx = Some((self.var.clone(), dest.clone()));
        let step_node = step.lower_with_ctx(ctx);
        ctx.loop_ctx = prev;

        ctx.add_node(
            tir::sem_expr::ExprKind::Loop,
            &[start, end, init, step_node],
        )
    }
}

impl Expr {
    /// Return a clone of this expression with every `for` loop fully unrolled
    /// (recursively). Loops whose bounds are not constant under `consts` are left
    /// in place.
    pub(crate) fn expand_loops(&self, consts: &HashMap<String, i64>) -> Expr {
        let mut out = self.clone();
        expand_loops_inplace(&mut out, consts);
        out
    }
}

fn expand_loops_inplace(expr: &mut Expr, consts: &HashMap<String, i64>) {
    match expr {
        Expr::For(f) => {
            expand_loops_inplace(&mut f.start, consts);
            expand_loops_inplace(&mut f.end, consts);
            expand_loops_inplace(&mut f.body, consts);
            // Accumulator loops lower to a `Loop` node and value-producing loops to
            // a `VectorMap`, so leave both intact; only statement loops are
            // unrolled here.
            if f.accumulator().is_none()
                && f.map_elem().is_none()
                && let Some(block) = unroll_for(f, consts)
            {
                *expr = block;
            }
        }
        Expr::Ident(_)
        | Expr::Lit(_)
        | Expr::Path(_)
        | Expr::BuiltinFunction(_)
        | Expr::Invalid => {}
        Expr::Assign(a) => {
            expand_loops_inplace(&mut a.dest, consts);
            expand_loops_inplace(&mut a.value, consts);
        }
        Expr::Binary(b) => {
            expand_loops_inplace(&mut b.lhs, consts);
            expand_loops_inplace(&mut b.rhs, consts);
        }
        Expr::Unary(u) => expand_loops_inplace(&mut u.x, consts),
        Expr::Block(b) => {
            for stmt in &mut b.stmts {
                expand_loops_inplace(stmt, consts);
            }
        }
        Expr::Call(c) => {
            expand_loops_inplace(&mut c.callee, consts);
            for arg in &mut c.arguments {
                expand_loops_inplace(arg, consts);
            }
        }
        Expr::Field(f) => expand_loops_inplace(&mut f.base, consts),
        Expr::If(i) => {
            expand_loops_inplace(&mut i.cond, consts);
            expand_loops_inplace(&mut i.then, consts);
            if let Some(e) = &mut i.else_ {
                expand_loops_inplace(e, consts);
            }
        }
        Expr::IndexAccess(i) => expand_loops_inplace(&mut i.base, consts),
        Expr::Slice(s) => expand_loops_inplace(&mut s.base, consts),
        Expr::Try(t) => {
            expand_loops_inplace(&mut t.body, consts);
            for h in &mut t.handlers {
                expand_loops_inplace(&mut h.body, consts);
            }
        }
        Expr::Lambda(l) => expand_loops_inplace(&mut l.body, consts),
    }
}

pub struct SemaLowering {
    pub root: tir::graph::NodeId,
    pub variable_symbols: HashMap<String, u32>,
    pub register_symbols: HashMap<(String, u32), u32>,
}

struct SemaExprLoweringCtx<
    'a,
    G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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
    had_error: bool,
    /// Active while lowering a loop's `step`: `(induction-variable name, dest)`.
    /// References to `dest` lower to the loop accumulator and references to the
    /// induction variable to the loop counter, so the body becomes a `Loop` node.
    loop_ctx: Option<(String, Expr)>,
    /// Stack of `map`/`reduce` lambda parameter names, innermost last. An `Ident`
    /// matching a parameter of the innermost lambda lowers to an `Arg` node whose
    /// index is the parameter's position.
    lambda_params: Vec<Vec<String>>,
}

impl<'a, G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>>
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
            had_error: false,
            loop_ctx: None,
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
            had_error: false,
            loop_ctx: None,
            lambda_params: Vec::new(),
        }
    }

    fn add_node(
        &mut self,
        kind: tir::sem_expr::ExprKind,
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
        kind: tir::sem_expr::ExprKind,
        data: tir::sem_expr::ExprPayload,
    ) -> tir::graph::NodeId {
        let node = self.graph.add_node(kind);
        self.graph.set_leaf_data(node, data);
        node
    }

    fn add_int_const(&mut self, value: tir::utils::APInt) -> tir::graph::NodeId {
        self.add_leaf(
            tir::sem_expr::ExprKind::Constant,
            tir::sem_expr::ExprPayload::Int(value),
        )
    }

    fn add_bool_const(&mut self, value: bool) -> tir::graph::NodeId {
        self.add_int_const(tir::utils::APInt::new(1, value as u64))
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
            tir::sem_expr::ExprKind::Extract,
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
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        // Inside a loop's `step`, references to the accumulator and induction
        // variable become the dedicated leaf nodes the `Loop` reads.
        let loop_leaf = ctx
            .loop_ctx
            .as_ref()
            .map(|(var, dest)| {
                if same_target(self, dest) {
                    1u8
                } else if matches!(self, Expr::Ident(id) if &id.name == var) {
                    2
                } else {
                    0
                }
            })
            .unwrap_or(0);
        match loop_leaf {
            1 => return ctx.graph.add_node(tir::sem_expr::ExprKind::Acc),
            2 => return ctx.graph.add_node(tir::sem_expr::ExprKind::IndVar),
            _ => {}
        }
        // Inside a `map`/`reduce` lambda, a reference to one of its parameters
        // lowers to an `Arg` leaf carrying the parameter's position.
        if let Expr::Ident(id) = self
            && let Some(params) = ctx.lambda_params.last()
            && let Some(idx) = params.iter().position(|p| p == &id.name)
        {
            return ctx.add_leaf(
                tir::sem_expr::ExprKind::Arg,
                tir::sem_expr::ExprPayload::Int(tir::utils::APInt::new(32, idx as u64)),
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
            Expr::For(f) => f.as_sema_expr(ctx),
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
            Node = tir::sem_expr::ExprKind,
            Leaf = tir::sem_expr::ExprPayload,
        >,
    ) -> tir::graph::NodeId {
        self.as_sema_expr_with_params(g, &HashMap::new())
    }

    pub(crate) fn as_sema_expr_with_params(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem_expr::ExprKind,
            Leaf = tir::sem_expr::ExprPayload,
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
            Node = tir::sem_expr::ExprKind,
            Leaf = tir::sem_expr::ExprPayload,
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
        })
    }

    /// Like [`Expr::lower_to_sema`], but supplies ISA parameter values used to
    /// const-evaluate a vector map's lane count, so the lowered pattern carries a
    /// concrete width that instruction selection can match.
    pub fn lower_to_sema_with_isa(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem_expr::ExprKind,
            Leaf = tir::sem_expr::ExprPayload,
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
        })
    }

    /// Like [`Expr::lower_to_sema`], but resolves index-less register paths (e.g.
    /// status flags such as `PSTATE::z`) through `register_indices`, a
    /// `(class, register-name) -> index` table derived from the register-class
    /// definitions. Used by simulator codegen so flag reads and writes resolve to a
    /// stable register slot instead of failing to lower.
    pub fn lower_to_sema_with_registers(
        &self,
        g: &mut impl tir::graph::MutDag<
            Node = tir::sem_expr::ExprKind,
            Leaf = tir::sem_expr::ExprPayload,
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
        })
    }
}

impl Assign {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        self.value.lower_with_ctx(ctx)
    }
}

impl Lit {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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

                ctx.add_int_const(tir::utils::APInt::new(width, value))
            }
            Lit::Str(_) => panic!("string literals are not supported in semantic expressions"),
        }
    }
}

impl Ident {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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
                ctx.add_int_const(tir::utils::APInt::new_signed(width, value))
            } else {
                ctx.add_int_const(tir::utils::APInt::new(width, abs_value))
            }
        } else {
            let id = ctx.get_or_create_variable_symbol(self.name.clone());
            ctx.add_leaf(
                tir::sem_expr::ExprKind::Symbol,
                tir::sem_expr::ExprPayload::SymbolId(id),
            )
        }
    }
}

impl Path {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        if self.remainder.len() != 1 {
            ctx.had_error = true;
            return ctx.add_int_const(tir::utils::APInt::new(64, 0));
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
            return ctx.add_int_const(tir::utils::APInt::new(64, 0));
        };

        let symbol_id = ctx.get_or_create_register_symbol(self.base.clone(), number);
        ctx.add_leaf(
            tir::sem_expr::ExprKind::Symbol,
            tir::sem_expr::ExprPayload::SymbolId(symbol_id),
        )
    }
}

impl Field {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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
                tir::sem_expr::ExprKind::Symbol,
                tir::sem_expr::ExprPayload::SymbolId(symbol_id),
            )
        } else {
            panic!("register field access requires base to be an identifier")
        }
    }
}

impl Binary {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let lhs = self.lhs.lower_with_ctx(ctx);
        let rhs = self.rhs.lower_with_ctx(ctx);

        use tir::sem_expr::ExprKind as K;

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
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        use tir::sem_expr::ExprKind as K;

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
                        ctx.add_int_const(tir::utils::APInt::new_signed(width, value))
                    } else {
                        let v = value as u64;
                        let width = if v == 0 { 1 } else { 64 - v.leading_zeros() };
                        ctx.add_int_const(tir::utils::APInt::new(width, v))
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
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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

        ctx.add_node(tir::sem_expr::ExprKind::If, &[cond, then_, else_])
    }
}

impl Block {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
    >(
        &self,
        ctx: &mut SemaExprLoweringCtx<'_, G>,
    ) -> tir::graph::NodeId {
        let input = self.base.lower_with_ctx(ctx);
        let idx = Lit::Int(LitInt::new(self.index.to_string(), self.span)).as_sema_expr(ctx);
        ctx.build_extract(input, idx, idx)
    }
}

impl Call {
    fn as_sema_expr<
        G: tir::graph::MutDag<Node = tir::sem_expr::ExprKind, Leaf = tir::sem_expr::ExprPayload>,
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
                ctx.add_node(tir::sem_expr::ExprKind::Clamp, &[input, min, max])
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
                ctx.add_node(tir::sem_expr::ExprKind::Log2Ceil, &[input])
            }
            BuiltinFunction::Lane => {
                assert!(self.arguments.len() == 2, "lane requires 2 arguments");
                let vector = self.arguments[0].lower_with_ctx(ctx);
                let index = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem_expr::ExprKind::Lane, &[vector, index])
            }
            BuiltinFunction::SExt => {
                assert!(self.arguments.len() == 2, "sext requires 2 arguments");
                let input = self.arguments[0].lower_with_ctx(ctx);
                let width = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem_expr::ExprKind::SExt, &[input, width])
            }
            BuiltinFunction::ZExt => {
                assert!(self.arguments.len() == 2, "zext requires 2 arguments");
                let input = self.arguments[0].lower_with_ctx(ctx);
                let width = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem_expr::ExprKind::ZExt, &[input, width])
            }
            BuiltinFunction::Load => {
                assert!(self.arguments.len() == 3, "load requires 3 arguments");
                let address = self.arguments[0].lower_with_ctx(ctx);
                let bytes = self.arguments[1].lower_with_ctx(ctx);
                let metadata = self.arguments[2].lower_with_ctx(ctx);
                ctx.add_node(
                    tir::sem_expr::ExprKind::LoadMemory,
                    &[address, bytes, metadata],
                )
            }
            BuiltinFunction::Store => {
                assert!(self.arguments.len() == 3, "store requires 3 arguments");
                let address = self.arguments[0].lower_with_ctx(ctx);
                let bytes = self.arguments[1].lower_with_ctx(ctx);
                let value = self.arguments[2].lower_with_ctx(ctx);
                let address_space = ctx.add_int_const(tir::utils::APInt::new(1, 0));
                ctx.add_node(
                    tir::sem_expr::ExprKind::StoreMemory,
                    &[address, bytes, value, address_space],
                )
            }
            // trap has no semantic-expression form; codegen intercepts trap
            // calls before lowering, so reaching here means the behavior used
            // it in a value position.
            BuiltinFunction::Trap => {
                ctx.had_error = true;
                ctx.add_int_const(tir::utils::APInt::new(64, 0))
            }
            BuiltinFunction::Split => {
                assert!(self.arguments.len() == 2, "split requires 2 arguments");
                let bits = self.arguments[0].lower_with_ctx(ctx);
                let n = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem_expr::ExprKind::Split, &[bits, n])
            }
            BuiltinFunction::Concat => {
                assert!(self.arguments.len() == 1, "concat requires 1 argument");
                let iter = self.arguments[0].lower_with_ctx(ctx);
                ctx.add_node(tir::sem_expr::ExprKind::IterConcat, &[iter])
            }
            BuiltinFunction::Zip => {
                assert!(self.arguments.len() == 2, "zip requires 2 arguments");
                let lhs = self.arguments[0].lower_with_ctx(ctx);
                let rhs = self.arguments[1].lower_with_ctx(ctx);
                ctx.add_node(tir::sem_expr::ExprKind::Zip, &[lhs, rhs])
            }
            BuiltinFunction::Map => {
                assert!(self.arguments.len() == 2, "map requires 2 arguments");
                let iter = self.arguments[0].lower_with_ctx(ctx);
                let body = ctx.lower_lambda_body(&self.arguments[1]);
                ctx.add_node(tir::sem_expr::ExprKind::Map, &[iter, body])
            }
            BuiltinFunction::Reduce => {
                assert!(self.arguments.len() == 2, "reduce requires 2 arguments");
                let iter = self.arguments[0].lower_with_ctx(ctx);
                let body = ctx.lower_lambda_body(&self.arguments[1]);
                ctx.add_node(tir::sem_expr::ExprKind::Reduce, &[iter, body])
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
