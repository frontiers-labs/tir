//! Prototype x86 backend: scalar integer codegen for 32-bit (`X86_32`) and
//! 64-bit (`X86_64`), modeled as two separate ISAs, emitting AT&T assembly and
//! ELF relocatable objects.
//!
//! Scope and simplifications:
//! - Only the eight legacy general-purpose registers are modeled, so every ModRM
//!   register field fits in three bits and no REX register-extension is needed.
//!   `rsp`/`rbp` are reserved (stack pointer and frame base), leaving six
//!   allocatable registers per ISA.
//! - Integer ALU ops are two-address; selection emits `mov rd, lhs` then the
//!   in-place op (see `lower_alu`).
//! - The calling convention is a self-consistent register convention, not the
//!   System V psABI: arguments in `rcx, rdx, rsi, rdi`, result in `rax`. Stack
//!   argument passing is not supported.
//! - No floats, no vectors, no variable shifts. Memory addressing is limited to
//!   `disp32(base)`; the IR's integer width must match the target ISA width.
//! - Spilling is emitted via `mov` to/from `rbp`-relative slots, but the tight
//!   six-register file combined with two-address operands can exceed the PBQP
//!   allocator's heuristic on high-pressure functions.

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::helpers::{dialect, operation};
use tir::{Any, Operation};

mod obj;

include!(concat!(env!("OUT_DIR"), "/x86.rs"));

/// Parsed x86 target selection from `--march`/`--mcpu`/`--mattr`. The prototype
/// models 32-bit and 64-bit as the separate ISAs `X86_32` and `X86_64`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetConfig {
    xlen: u32,
    features: Vec<Feature>,
    machine: Option<String>,
}

impl TargetConfig {
    pub fn parse(march: &str, mcpu: Option<&str>, mattr: Option<&str>) -> Result<Self, String> {
        let xlen = parse_march(march)?;
        let _ = (mcpu, mattr);
        let features = vec![if xlen == 32 {
            Feature::X86_32
        } else {
            Feature::X86_64
        }];
        Ok(TargetConfig {
            xlen,
            features,
            machine: None,
        })
    }

    pub fn canonical_name(&self) -> &'static str {
        if self.xlen == 32 { "i386" } else { "x86_64" }
    }

    pub fn features(&self) -> &[Feature] {
        &self.features
    }
}

fn parse_march(march: &str) -> Result<u32, String> {
    // `normalize` has already mapped `_` to `-`, so `x86_64` arrives as `x86-64`.
    match normalize(march).as_str() {
        "i386" | "i486" | "i586" | "i686" | "x86" | "ia32" => Ok(32),
        "x86-64" | "amd64" | "x64" => Ok(64),
        other => Err(format!("unknown x86 architecture '{other}'")),
    }
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

/// The architectural register class for an ISA width.
fn class_name(bits: u32) -> &'static str {
    if bits == 32 { "GPR32" } else { "GPR64" }
}

/// The register file restricted to the active ISA's single GPR class. The
/// generated `register_info()` lists both `GPR32` and `GPR64` (one per ISA), but
/// the allocator's `default_integer_class` picks the first class with argument
/// registers; exposing only the live class keeps argument/return precoloring on
/// the right width.
fn register_info_for(bits: u32) -> tir_be_common::regalloc::RegisterInfo {
    use std::sync::OnceLock;
    use tir_be_common::regalloc::{RegClassInfo, RegisterInfo};
    static INFO32: OnceLock<&'static [RegClassInfo]> = OnceLock::new();
    static INFO64: OnceLock<&'static [RegClassInfo]> = OnceLock::new();
    let cell = if bits == 32 { &INFO32 } else { &INFO64 };
    let classes = cell.get_or_init(|| {
        let want = class_name(bits);
        let only: Vec<RegClassInfo> = register_info()
            .classes
            .iter()
            .filter(|c| c.name == want)
            .cloned()
            .collect();
        &*Box::leak(only.into_boxed_slice())
    });
    RegisterInfo { classes }
}

fn phys(class: &str, index: u16) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Physical {
        class: class.to_string(),
        index,
    })
}

fn virt(value: u32, class: &str) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Virtual {
        id: value,
        class: Some(class.to_string()),
    })
}

/// A register-to-register move for the given ISA width.
fn mv(
    context: &tir::Context,
    bits: u32,
    rd: AttributeValue,
    rs: AttributeValue,
) -> Box<dyn Operation> {
    if bits == 32 {
        Box::new(
            MovRR32OpBuilder::new(context)
                .attr("rd", rd)
                .attr("rs", rs)
                .build(),
        )
    } else {
        Box::new(
            MovRR64OpBuilder::new(context)
                .attr("rd", rd)
                .attr("rs", rs)
                .build(),
        )
    }
}

// --- Virtual ops -----------------------------------------------------------
// Lowered forms of the builtin terminators and calls, finalized to real x86
// instructions after register allocation (mirrors the AArch64 backend).

operation! {
    VirtualReturnOp {
        name: "vret",
        dialect: "x86",
        operands: [value],
    }
}

operation! {
    VirtualBranchOp {
        name: "vbr",
        dialect: "x86",
        format: "custom",
        operands: O {
            dest_args: "*Any",
        },
        attributes: A {
            dest: "Block",
        },
    }
}

impl VirtualBranchOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        tir_be_common::print_branch(fmt, self, "x86.vbr")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

operation! {
    VirtualCondBranchOp {
        name: "vcond_br",
        dialect: "x86",
        format: "custom",
        operands: O {
            condition: "Any",
            true_args: "*Any",
            false_args: "*Any",
        },
        attributes: A {
            true_dest: "Block",
            false_dest: "Block",
        },
    }
}

impl VirtualCondBranchOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        tir_be_common::print_branch(fmt, self, "x86.vcond_br")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

operation! {
    VirtualCallOp {
        name: "vcall",
        dialect: "x86",
        attributes: A {
            callee: "Str",
        },
        roles: R {
            clobbers: Clobber,
        },
    }
}

dialect! {
    X86Dialect {
        name: "x86",
        operations: [
            VirtualReturnOp,
            VirtualBranchOp,
            VirtualCondBranchOp,
            VirtualCallOp,
            Add32Op,
            Sub32Op,
            And32Op,
            Or32Op,
            Xor32Op,
            Add64Op,
            Sub64Op,
            And64Op,
            Or64Op,
            Xor64Op,
            MovRR32Op,
            MovRR64Op,
            MovRI32Op,
            MovRI64Op,
            Load32Op,
            Store32Op,
            Load64Op,
            Store64Op,
            SubFrame32Op,
            AddFrame32Op,
            SubFrame64Op,
            AddFrame64Op,
            Test32Op,
            Test64Op,
            JmpOp,
            JneOp,
            CallRelOp,
            RetOp,
        ],
    }
}

impl X86Dialect {
    pub fn get_asm_parser(&self) -> tir_be_common::AsmParser {
        tir_be_common::AsmParser::new(get_instruction_parsers(Feature::ALL).0)
    }

    pub fn get_asm_printer(&self) -> tir_be_common::AsmPrinter {
        tir_be_common::AsmPrinter::new(get_instruction_printers())
    }
}

/// Lower `builtin.func` to an `asm.symbol` (recording argument registers) and
/// `builtin.return` to the placeholder `vret`.
fn lower_func_impl(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    bits: u32,
) -> Result<bool, tir::PassError> {
    use tir::builtin::{FuncOp, ReturnOp};

    if let Some(func) = op.as_op::<FuncOp>() {
        let body = func.body();
        let has_symbol_end = body
            .op_ids()
            .last()
            .map(|id| context.get_op(*id).name == tir_be_common::SymbolEndOp::name())
            .unwrap_or(false);
        if !has_symbol_end {
            let mut b = tir::IRBuilder::new(body);
            b.insert(tir_be_common::SymbolEndOpBuilder::new(context).build());
        }

        let sym_name = func
            .attributes()
            .iter()
            .find(|a| a.name == "sym_name")
            .and_then(|a| match &a.value {
                AttributeValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_string());

        let class = class_name(bits);
        let arg_regs = func
            .body()
            .arguments()
            .iter()
            .map(|arg| {
                AttributeValue::Register(RegisterAttr::Virtual {
                    id: arg.id().number(),
                    class: Some(class.to_string()),
                })
            })
            .collect::<Vec<_>>();

        let lowered = tir_be_common::SymbolOpBuilder::new(context)
            .body(op.op().regions[0])
            .attr("name", AttributeValue::Str(sym_name))
            .attr("arg_regs", AttributeValue::Array(arg_regs))
            .build();
        rewriter.replace_op(op, &lowered)?;
        return Ok(true);
    }

    if let Some(ret) = op.as_op::<ReturnOp>() {
        let mut builder = VirtualReturnOpBuilder::new(context);
        if let Some(value) = ret.operands().first().copied() {
            builder = builder.value(value);
        }
        let lowered = builder.build();
        rewriter.replace_op(op, &lowered)?;
        return Ok(true);
    }

    Ok(false)
}

fn lower_func_32(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_func_impl(c, op, r, 32)
}

fn lower_func_64(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_func_impl(c, op, r, 64)
}

fn lower_branches(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::builtin::{BranchOp, CondBranchOp};

    if let Some(br) = op.as_op::<BranchOp>() {
        let lowered = VirtualBranchOpBuilder::new(context)
            .dest_args(br.dest_args())
            .attr("dest", AttributeValue::Block(br.dest()))
            .build();
        rewriter.replace_op(op, &lowered)?;
        return Ok(true);
    }

    if let Some(cond_br) = op.as_op::<CondBranchOp>() {
        let lowered = VirtualCondBranchOpBuilder::new(context)
            .condition(cond_br.condition())
            .true_args(cond_br.true_args())
            .false_args(cond_br.false_args())
            .attr("true_dest", AttributeValue::Block(cond_br.true_dest()))
            .attr("false_dest", AttributeValue::Block(cond_br.false_dest()))
            .build();
        rewriter.replace_op(op, &lowered)?;
        return Ok(true);
    }

    Ok(false)
}

/// Lower `builtin.call` to `vcall`, moving arguments into the ABI argument
/// registers and copying the result out of the return register. x86 keeps its
/// return address on the stack, so (unlike AArch64) no link register survives
/// the call and needs saving.
fn lower_calls_impl(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    bits: u32,
) -> Result<bool, tir::PassError> {
    use tir::builtin::{CallOp, UnitType};

    let Some(call) = op.as_op::<CallOp>() else {
        return Ok(false);
    };
    let (args, result) = (call.args(), call.result());
    let class = class_name(bits);

    let info = register_info_for(bits);
    let regs = info
        .class(class)
        .expect("x86 register info must define the GPR class");
    if args.len() > regs.arguments.len() {
        return Err(tir::PassError::InvalidRuleSet(
            "stack-passed call arguments are not supported by codegen yet".to_string(),
        ));
    }

    // Detach arguments into fresh virtual registers before any argument register
    // is written, so an operand already living in an argument register is read
    // before the moves below clobber it.
    let mut fresh_args = Vec::with_capacity(args.len());
    for arg in &args {
        let ty = context.get_value(*arg).ty();
        let fresh = context.create_value(ty, None).id().number();
        let copy = mv(context, bits, virt(fresh, class), virt(arg.number(), class));
        rewriter.insert_op_before(op, copy.as_ref())?;
        fresh_args.push(fresh);
    }
    for (&fresh, &reg) in fresh_args.iter().zip(regs.arguments.iter()) {
        let copy = mv(context, bits, phys(class, reg), virt(fresh, class));
        rewriter.insert_op_before(op, copy.as_ref())?;
    }

    let clobbers = AttributeValue::Array(
        regs.caller_saved
            .iter()
            .map(|&index| phys(class, index))
            .collect(),
    );
    let vcall = VirtualCallOpBuilder::new(context)
        .attr("callee", AttributeValue::Str(call.callee()))
        .attr("clobbers", clobbers)
        .build();

    let ret_reg = regs.return_values[0];
    if context.get_value(result).ty() == UnitType::new(context) {
        rewriter.replace_op(op, &vcall)?;
    } else {
        rewriter.insert_op_before(op, &vcall)?;
        let copy = mv(
            context,
            bits,
            virt(result.number(), class),
            phys(class, ret_reg),
        );
        rewriter.replace_op(op, copy.as_ref())?;
    }
    Ok(true)
}

fn lower_calls_32(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_calls_impl(c, op, r, 32)
}

fn lower_calls_64(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_calls_impl(c, op, r, 64)
}

#[derive(Clone, Copy)]
enum Alu {
    Add,
    Sub,
    And,
    Or,
    Xor,
}

/// Build a two-address ALU op `rd = rd OP rs2` for the given width.
fn build_alu(
    context: &tir::Context,
    bits: u32,
    alu: Alu,
    rd: AttributeValue,
    rs2: AttributeValue,
) -> Box<dyn Operation> {
    macro_rules! b {
        ($builder:ident) => {
            Box::new(
                $builder::new(context)
                    .attr("rd", rd)
                    .attr("rs2", rs2)
                    .build(),
            ) as Box<dyn Operation>
        };
    }
    match (bits, alu) {
        (32, Alu::Add) => b!(Add32OpBuilder),
        (32, Alu::Sub) => b!(Sub32OpBuilder),
        (32, Alu::And) => b!(And32OpBuilder),
        (32, Alu::Or) => b!(Or32OpBuilder),
        (32, Alu::Xor) => b!(Xor32OpBuilder),
        (_, Alu::Add) => b!(Add64OpBuilder),
        (_, Alu::Sub) => b!(Sub64OpBuilder),
        (_, Alu::And) => b!(And64OpBuilder),
        (_, Alu::Or) => b!(Or64OpBuilder),
        (_, Alu::Xor) => b!(Xor64OpBuilder),
    }
}

/// Select a builtin integer ALU op to a two-address x86 op: `mov rd, lhs` then
/// `op rhs, rd`. The destination is read-write, so the allocator keeps `rhs` out
/// of `rd` and the encoding computes `lhs OP rhs`.
fn lower_alu_impl(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
    bits: u32,
) -> Result<bool, tir::PassError> {
    use tir::builtin::{AddIOp, AndIOp, OrIOp, SubIOp, XOrIOp};

    let (lhs, rhs, result, alu) = if let Some(o) = op.as_op::<AddIOp>() {
        (o.operands()[0], o.operands()[1], o.result(), Alu::Add)
    } else if let Some(o) = op.as_op::<SubIOp>() {
        (o.operands()[0], o.operands()[1], o.result(), Alu::Sub)
    } else if let Some(o) = op.as_op::<AndIOp>() {
        (o.operands()[0], o.operands()[1], o.result(), Alu::And)
    } else if let Some(o) = op.as_op::<OrIOp>() {
        (o.operands()[0], o.operands()[1], o.result(), Alu::Or)
    } else if let Some(o) = op.as_op::<XOrIOp>() {
        (o.operands()[0], o.operands()[1], o.result(), Alu::Xor)
    } else {
        return Ok(false);
    };

    let class = class_name(bits);
    let copy = mv(
        context,
        bits,
        virt(result.number(), class),
        virt(lhs.number(), class),
    );
    rewriter.insert_op_before(op, copy.as_ref())?;
    let alu_op = build_alu(
        context,
        bits,
        alu,
        virt(result.number(), class),
        virt(rhs.number(), class),
    );
    rewriter.replace_op(op, alu_op.as_ref())?;
    Ok(true)
}

fn lower_alu_32(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_alu_impl(c, op, r, 32)
}

fn lower_alu_64(
    c: &tir::Context,
    op: &tir::OperationRef,
    r: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    lower_alu_impl(c, op, r, 64)
}

fn create_isel_pass_for(
    context: &tir::Context,
    config: &TargetConfig,
) -> tir_be_common::isel::InstructionSelectPass {
    use tir_be_common::isel::OpLowering;
    let (lower_func, lower_calls, lower_alu): (OpLowering, OpLowering, OpLowering) =
        if config.xlen == 32 {
            (lower_func_32, lower_calls_32, lower_alu_32)
        } else {
            (lower_func_64, lower_calls_64, lower_alu_64)
        };
    tir_be_common::isel::InstructionSelectPass::new(get_isel_rules(context, &config.features))
        .with_op_lowering(lower_func)
        .with_op_lowering(lower_branches)
        .with_op_lowering(lower_calls)
        .with_op_lowering(lower_alu)
}

pub fn create_isel_pass(context: &tir::Context) -> tir_be_common::isel::InstructionSelectPass {
    create_isel_pass_for(context, &TargetConfig::parse("x86-64", None, None).unwrap())
}

// --- register allocation ---------------------------------------------------

/// x86 register-allocation target. The frame base is `rbp`/`ebp` (index 5); the
/// prologue/epilogue grow and shrink the frame with `sub`/`add` on it and spills
/// are addressed relative to it.
pub struct X86RegAlloc {
    bits: u32,
}

const FRAME_INDEX: u16 = 5;

impl tir_be_common::regalloc::TargetRegAlloc for X86RegAlloc {
    fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
        register_info_for(self.bits)
    }

    fn frame_register(&self) -> (String, u16) {
        (class_name(self.bits).to_string(), FRAME_INDEX)
    }

    fn emit_spill_store(
        &self,
        context: &tir::Context,
        value: u32,
        class: &str,
        frame: &(String, u16),
        offset: i64,
    ) -> Box<dyn Operation> {
        if self.bits == 32 {
            Box::new(
                Store32OpBuilder::new(context)
                    .attr("rs", virt(value, class))
                    .attr("base", phys(&frame.0, frame.1))
                    .attr("disp", AttributeValue::Int(offset))
                    .build(),
            )
        } else {
            Box::new(
                Store64OpBuilder::new(context)
                    .attr("rs", virt(value, class))
                    .attr("base", phys(&frame.0, frame.1))
                    .attr("disp", AttributeValue::Int(offset))
                    .build(),
            )
        }
    }

    fn emit_spill_reload(
        &self,
        context: &tir::Context,
        value: u32,
        class: &str,
        frame: &(String, u16),
        offset: i64,
    ) -> Box<dyn Operation> {
        if self.bits == 32 {
            Box::new(
                Load32OpBuilder::new(context)
                    .attr("rd", virt(value, class))
                    .attr("base", phys(&frame.0, frame.1))
                    .attr("disp", AttributeValue::Int(offset))
                    .build(),
            )
        } else {
            Box::new(
                Load64OpBuilder::new(context)
                    .attr("rd", virt(value, class))
                    .attr("base", phys(&frame.0, frame.1))
                    .attr("disp", AttributeValue::Int(offset))
                    .build(),
            )
        }
    }

    fn emit_prologue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        let frame = phys(class_name(self.bits), FRAME_INDEX);
        let op: Box<dyn Operation> = if self.bits == 32 {
            Box::new(
                SubFrame32OpBuilder::new(context)
                    .attr("rd", frame)
                    .attr("imm", AttributeValue::Int(size as i64))
                    .build(),
            )
        } else {
            Box::new(
                SubFrame64OpBuilder::new(context)
                    .attr("rd", frame)
                    .attr("imm", AttributeValue::Int(size as i64))
                    .build(),
            )
        };
        vec![op]
    }

    fn emit_epilogue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        let frame = phys(class_name(self.bits), FRAME_INDEX);
        let op: Box<dyn Operation> = if self.bits == 32 {
            Box::new(
                AddFrame32OpBuilder::new(context)
                    .attr("rd", frame)
                    .attr("imm", AttributeValue::Int(size as i64))
                    .build(),
            )
        } else {
            Box::new(
                AddFrame64OpBuilder::new(context)
                    .attr("rd", frame)
                    .attr("imm", AttributeValue::Int(size as i64))
                    .build(),
            )
        };
        vec![op]
    }
}

pub fn create_regalloc_pass(bits: u32) -> tir_be_common::regalloc::RegisterAllocationPass {
    tir_be_common::regalloc::RegisterAllocationPass::new(Box::new(X86RegAlloc { bits }))
}

// --- target machine --------------------------------------------------------

pub struct X86Target {
    config: TargetConfig,
}

impl tir_be_common::TargetMachine for X86Target {
    fn name(&self) -> &'static str {
        self.config.canonical_name()
    }

    fn register_dialects(&self, context: &tir::Context) {
        context.register_dialect::<tir_be_common::AsmDialect>();
        context.register_dialect::<X86Dialect>();
    }

    fn isel_pass(&self, context: &tir::Context) -> tir_be_common::isel::InstructionSelectPass {
        create_isel_pass_for(context, &self.config)
    }

    fn regalloc_pass(&self) -> tir_be_common::regalloc::RegisterAllocationPass {
        create_regalloc_pass(self.config.xlen)
    }

    fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
        register_info_for(self.config.xlen)
    }

    fn asm_parser(&self, _context: &tir::Context) -> tir_be_common::AsmParser {
        let (parsers, disabled) = get_instruction_parsers(&self.config.features);
        tir_be_common::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
    }

    fn asm_printer(&self, context: &tir::Context) -> tir_be_common::AsmPrinter {
        context
            .find_dialect::<X86Dialect>()
            .expect("x86 dialect must be registered before building an asm printer")
            .get_asm_printer()
    }

    fn machine_model(&self, name: &str) -> Option<tir_be_common::sched::MachineModel> {
        crate::machine_model(name, &self.config.features)
    }

    fn machines(&self) -> Vec<&'static str> {
        crate::machines(&self.config.features)
    }

    fn isa_params(&self) -> Vec<(&'static str, i64)> {
        crate::isa_params(&self.config.features)
    }

    fn register_widths(&self) -> Vec<(&'static str, u32)> {
        crate::register_widths(&self.config.features)
    }

    fn register_name(&self, class: &str, index: u16, prefer_abi: bool) -> Option<String> {
        crate::register_name(class, index, prefer_abi)
    }

    fn pre_ra_lowerings(&self) -> Vec<tir_be_common::isel::OpLowering> {
        if self.config.xlen == 32 {
            vec![obj::lower_constant_32, obj::lower_vcond_br_32]
        } else {
            vec![obj::lower_constant_64, obj::lower_vcond_br_64]
        }
    }

    fn finalize_lowerings(&self) -> Vec<tir_be_common::isel::OpLowering> {
        vec![obj::finalize_virtual_ops]
    }

    fn object_format(&self) -> Option<tir_be_common::binary::ObjectFormatInfo> {
        Some(obj::object_format(self.config.xlen))
    }

    fn binary_writer(
        &self,
        _context: &tir::Context,
    ) -> Option<tir_be_common::binary::BinaryWriter> {
        Some(obj::binary_writer())
    }
}

fn select_x86(
    march: &str,
    mcpu: Option<&str>,
    mattr: Option<&str>,
) -> Result<Option<Box<dyn tir_be_common::TargetMachine>>, String> {
    let n = normalize(march);
    let owned = [
        "i386", "i486", "i586", "i686", "x86", "ia32", "amd64", "x64",
    ]
    .iter()
    .any(|name| n == *name)
        || n.starts_with("x86");
    if !owned {
        return Ok(None);
    }
    let config = TargetConfig::parse(march, mcpu, mattr)?;
    Ok(Some(Box::new(X86Target { config })))
}

tir_be_common::register_target!(select_x86, ["x86_64", "i386"]);

#[cfg(test)]
mod tests {
    use tir::attributes::AttributeValue;
    use tir::{Context, Operation};
    use tir_be_common::AsmDialect;

    use crate::X86Dialect;

    /// Encode an op and return its bytes, asserting it produced no fixups.
    fn bytes(context: &Context, op: &dyn tir::Operation) -> Vec<u8> {
        let encoders = crate::get_instruction_encoders();
        let inst = context.get_op(op.id());
        let enc = encoders[inst.name](&inst).expect("op should encode");
        assert!(enc.fixups.is_empty(), "unexpected fixups for {}", inst.name);
        enc.bytes
    }

    #[test]
    fn golden_encodings_match_x86() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<X86Dialect>();
        let gpr64 = |i: u16| crate::phys("GPR64", i);
        let gpr32 = |i: u16| crate::phys("GPR32", i);

        // add %rdx, %rax  =>  48 01 D0
        let add = crate::Add64OpBuilder::new(&context)
            .attr("rd", gpr64(0))
            .attr("rs2", gpr64(2))
            .build();
        assert_eq!(bytes(&context, &add), [0x48, 0x01, 0xD0]);

        // mov %rcx, %rax  =>  48 89 C8
        let mov = crate::MovRR64OpBuilder::new(&context)
            .attr("rd", gpr64(0))
            .attr("rs", gpr64(1))
            .build();
        assert_eq!(bytes(&context, &mov), [0x48, 0x89, 0xC8]);

        // movq $42, %rax  =>  48 C7 C0 2A 00 00 00
        let movi = crate::MovRI64OpBuilder::new(&context)
            .attr("rd", gpr64(0))
            .attr("imm", AttributeValue::Int(42))
            .build();
        assert_eq!(bytes(&context, &movi), [0x48, 0xC7, 0xC0, 0x2A, 0, 0, 0]);

        // movq 16(%rbp), %rax  =>  48 8B 85 10 00 00 00
        let load = crate::Load64OpBuilder::new(&context)
            .attr("rd", gpr64(0))
            .attr("base", gpr64(5))
            .attr("disp", AttributeValue::Int(16))
            .build();
        assert_eq!(bytes(&context, &load), [0x48, 0x8B, 0x85, 0x10, 0, 0, 0]);

        // ret  =>  C3
        let ret = crate::RetOpBuilder::new(&context).build();
        assert_eq!(bytes(&context, &ret), [0xC3]);

        // 32-bit, no REX: addl %edx, %eax => 01 D0 ; movl $7, %eax => B8 07 00 00 00
        let add32 = crate::Add32OpBuilder::new(&context)
            .attr("rd", gpr32(0))
            .attr("rs2", gpr32(2))
            .build();
        assert_eq!(bytes(&context, &add32), [0x01, 0xD0]);
        let movi32 = crate::MovRI32OpBuilder::new(&context)
            .attr("rd", gpr32(0))
            .attr("imm", AttributeValue::Int(7))
            .build();
        assert_eq!(bytes(&context, &movi32), [0xB8, 0x07, 0, 0, 0]);
    }

    #[test]
    fn call_emits_pcrel_fixup() {
        use tir_be_common::binary::{FixupTarget, InstFixup};
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<X86Dialect>();

        let encoders = crate::get_instruction_encoders();
        let call = crate::CallRelOpBuilder::new(&context)
            .attr("imm", AttributeValue::Str("foo".to_string()))
            .build();
        let enc = encoders["call"](&context.get_op(call.id())).unwrap();
        assert_eq!(enc.bytes, [0xE8, 0, 0, 0, 0]);
        assert_eq!(
            enc.fixups,
            vec![InstFixup {
                operand: "imm",
                target: FixupTarget::Symbol("foo".to_string()),
            }]
        );
    }

    #[test]
    fn march_parsing() {
        use crate::TargetConfig;
        assert_eq!(
            TargetConfig::parse("x86-64", None, None).map(|c| c.canonical_name()),
            Ok("x86_64")
        );
        assert_eq!(
            TargetConfig::parse("i686", None, None).map(|c| c.canonical_name()),
            Ok("i386")
        );
        assert!(TargetConfig::parse("riscv64", None, None).is_err());
    }
}
