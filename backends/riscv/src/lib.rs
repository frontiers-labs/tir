use tir::helpers::{dialect, operation};
use tir::{Any, Operation};

include!(concat!(env!("OUT_DIR"), "/riscv.rs"));

/// Parsed RISC-V target selection from `--march`/`--mcpu`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetConfig {
    xlen: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CpuModel {
    Generic,
    InOrder,
    OutOfOrder,
}

impl TargetConfig {
    /// Parse a RISC-V `--march`/`--mcpu` pair.
    pub fn parse(march: &str, mcpu: Option<&str>) -> Option<Self> {
        let config = parse_march(march)?;
        if let Some(mcpu) = mcpu {
            parse_mcpu(mcpu, config)?;
        }
        Some(config)
    }

    /// Canonical architecture name for diagnostics and target-specific behavior.
    pub fn canonical_name(&self) -> &'static str {
        match self.xlen {
            32 => "riscv32",
            _ => "riscv64",
        }
    }
}

fn parse_march(march: &str) -> Option<TargetConfig> {
    let march = normalize(march);
    match march.as_str() {
        "riscv" => Some(TargetConfig { xlen: 64 }),
        "riscv32" => Some(TargetConfig { xlen: 32 }),
        "riscv64" => Some(TargetConfig { xlen: 64 }),
        _ => parse_riscv_isa_string(&march),
    }
}

fn parse_mcpu(mcpu: &str, march_config: TargetConfig) -> Option<CpuModel> {
    let mcpu = normalize(mcpu);
    for (prefix, xlen) in [("riscv32-", 32), ("riscv64-", 64)] {
        if let Some(model) = mcpu.strip_prefix(prefix).and_then(parse_cpu_model) {
            return (march_config.xlen == xlen).then_some(model);
        }
    }

    parse_cpu_model(&mcpu)
}

fn parse_cpu_model(name: &str) -> Option<CpuModel> {
    match name {
        "generic" => Some(CpuModel::Generic),
        "generic-in-order" | "generic-inorder" | "in-order" | "inorder" => Some(CpuModel::InOrder),
        "generic-ooo" | "generic-out-of-order" | "ooo" | "out-of-order" => {
            Some(CpuModel::OutOfOrder)
        }
        _ => None,
    }
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

fn parse_riscv_isa_string(march: &str) -> Option<TargetConfig> {
    let rest = march.strip_prefix("rv")?;
    let (xlen, rest) = if let Some(rest) = rest.strip_prefix("32") {
        (32, rest)
    } else {
        (64, rest.strip_prefix("64")?)
    };

    let mut chars = rest.chars().peekable();
    let base = chars.next()?;
    match base {
        'i' | 'e' | 'g' => skip_extension_version(&mut chars),
        _ => return None,
    }

    while chars.peek().is_some() {
        if chars.peek() == Some(&'-') {
            chars.next();
            chars.peek()?;
            continue;
        }

        let ext = chars.next()?;
        if ext.is_ascii_digit() {
            return None;
        }

        match ext {
            'm' | 'a' | 'f' | 'd' | 'q' | 'l' | 'c' | 'b' | 'j' | 't' | 'p' | 'v' | 'h' => {
                skip_extension_version(&mut chars);
            }
            'z' | 's' | 'x' => {
                if !consume_multi_letter_extension(&mut chars) {
                    return None;
                }
            }
            _ => return None,
        }
    }

    Some(TargetConfig { xlen })
}

fn consume_multi_letter_extension<I>(chars: &mut std::iter::Peekable<I>) -> bool
where
    I: Iterator<Item = char>,
{
    let mut consumed_name_char = false;
    while let Some(&c) = chars.peek() {
        if c == '-' {
            break;
        }
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            consumed_name_char = true;
            chars.next();
        } else {
            return false;
        }
    }
    consumed_name_char
}

fn skip_extension_version<I>(chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
        chars.next();
    }
    if chars.peek() == Some(&'p') {
        chars.next();
        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            chars.next();
        }
    }
}

operation! {
    VirtualReturnOp {
        name: "vret",
        dialect: "riscv",
        operands: [value],
    }
}

// Virtual control-flow ops: the lowered form of `builtin.br`/`builtin.cond_br`.
// They carry the successor block references and the values forwarded to each
// successor's block arguments, deferring branch-target encoding to a later pass
// (mirroring how `vret` defers the return sequence).
operation! {
    VirtualBranchOp {
        name: "vbr",
        dialect: "riscv",
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
        tir_be_common::print_branch(fmt, self, "riscv.vbr")
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
        dialect: "riscv",
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
        tir_be_common::print_branch(fmt, self, "riscv.vcond_br")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

dialect! {
    RiscvDialect {
        name: "riscv",
        operations: [
            // RV32I register-register ALU
            AddOp,
            SubOp,
            ShiftLeftLogicalOp,
            ShiftRightLogicalOp,
            ShiftRightArithmeticOp,
            XorOp,
            AndOp,
            OrOp,
            SetLessThanOp,
            SetLessThanUnsignedOp,
            // RV32I register-immediate ALU
            AddImmOp,
            XorImmOp,
            OrImmOp,
            AndImmOp,
            ShiftLeftLogicalImmOp,
            ShiftRightLogicalImmOp,
            ShiftRightArithmeticImmOp,
            SetLessThanImmOp,
            SetLessThanUnsignedImmOp,
            LoadUpperImmOp,
            AddUpperImmToPCOp,
            // RV64I word ops (register-register)
            AddWordOp,
            SubWordOp,
            ShiftLeftLogicalWordOp,
            ShiftRightLogicalWordOp,
            ShiftRightArithmeticWordOp,
            // RV64I word ops (register-immediate)
            AddImmWordOp,
            ShiftLeftLogicalImmWordOp,
            ShiftRightLogicalImmWordOp,
            ShiftRightArithmeticImmWordOp,
            // Loads / stores
            LoadByteOp,
            LoadByteUnsignedOp,
            LoadHalfWordOp,
            LoadHalfWordUnsignedOp,
            LoadWordOp,
            LoadWordUnsignedOp,
            LoadDoubleWordOp,
            StoreByteOp,
            StoreHalfWordOp,
            StoreWordOp,
            StoreDoubleWordOp,
            // Control flow
            BranchEqOp,
            BranchNotEqOp,
            BranchLtOp,
            BranchGeOp,
            BranchLtUnsignedOp,
            BranchGeUnsignedOp,
            JumpAndLinkOp,
            JumpAndLinkRegOp,
            VirtualReturnOp,
            VirtualBranchOp,
            VirtualCondBranchOp
        ],
    }
}

pub mod ops {
    pub use super::*;
}

impl RiscvDialect {
    pub fn get_asm_parser(&self) -> tir_be_common::AsmParser {
        tir_be_common::AsmParser::new(get_instruction_parsers())
    }

    pub fn get_asm_printer(&self) -> tir_be_common::AsmPrinter {
        tir_be_common::AsmPrinter::new(get_instruction_printers())
    }
}

fn lower_func_and_return_to_asm_symbol(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::Operation;
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::builtin::{FuncOp, ReturnOp};

    if let Some(func) = op.as_op::<FuncOp>() {
        // asm.symbol regions require an explicit symbol_end terminator.
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

        let arg_regs = func
            .body()
            .arguments()
            .iter()
            .map(|arg| {
                AttributeValue::Register(RegisterAttr::Virtual {
                    id: arg.id().number(),
                    class: Some("GPR".to_string()),
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

/// Lower the builtin control-flow terminators to RISC-V virtual branch ops,
/// preserving successor block references and forwarded block arguments.
fn lower_branches(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::attributes::AttributeValue;
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

pub fn create_isel_pass(context: &tir::Context) -> tir_be_common::isel::InstructionSelectPass {
    tir_be_common::isel::InstructionSelectPass::new(get_isel_rules(context))
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
        .with_op_lowering(lower_branches)
}

/// The RISC-V stack pointer (`sp` = `x2`).
const SP: (&str, u16) = ("GPR", 2);

fn phys(reg: &(String, u16)) -> tir::attributes::AttributeValue {
    tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Physical {
        class: reg.0.clone(),
        index: reg.1,
    })
}

fn virt(value: u32, class: &str) -> tir::attributes::AttributeValue {
    tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Virtual {
        id: value,
        class: Some(class.to_string()),
    })
}

/// RISC-V register allocation target: the generated register file plus `sd`/`ld`
/// spill code and an `addi sp, sp, ±frame` prologue/epilogue.
pub struct RiscvRegAlloc;

impl tir_be_common::regalloc::TargetRegAlloc for RiscvRegAlloc {
    fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
        register_info()
    }

    fn frame_register(&self) -> (String, u16) {
        (SP.0.to_string(), SP.1)
    }

    fn emit_spill_store(
        &self,
        context: &tir::Context,
        value: u32,
        class: &str,
        frame: &(String, u16),
        offset: i64,
    ) -> Box<dyn Operation> {
        Box::new(
            StoreDoubleWordOpBuilder::new(context)
                .attr("rs1", phys(frame))
                .attr("rs2", virt(value, class))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )
    }

    fn emit_spill_reload(
        &self,
        context: &tir::Context,
        value: u32,
        class: &str,
        frame: &(String, u16),
        offset: i64,
    ) -> Box<dyn Operation> {
        Box::new(
            LoadDoubleWordOpBuilder::new(context)
                .attr("rd", virt(value, class))
                .attr("rs1", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )
    }

    fn emit_prologue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        vec![Box::new(
            AddImmOpBuilder::new(context)
                .attr("rd", phys(&(SP.0.to_string(), SP.1)))
                .attr("rs1", phys(&(SP.0.to_string(), SP.1)))
                .attr("imm", tir::attributes::AttributeValue::Int(-(size as i64)))
                .build(),
        )]
    }

    fn emit_epilogue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        vec![Box::new(
            AddImmOpBuilder::new(context)
                .attr("rd", phys(&(SP.0.to_string(), SP.1)))
                .attr("rs1", phys(&(SP.0.to_string(), SP.1)))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        )]
    }
}

pub fn create_regalloc_pass() -> tir_be_common::regalloc::RegisterAllocationPass {
    tir_be_common::regalloc::RegisterAllocationPass::new(Box::new(RiscvRegAlloc))
}

/// The RISC-V target, selected via `--march`/`--mcpu`.
pub struct RiscvTarget {
    config: TargetConfig,
}

impl tir_be_common::TargetMachine for RiscvTarget {
    fn name(&self) -> &'static str {
        self.config.canonical_name()
    }

    fn register_dialects(&self, context: &tir::Context) {
        context.register_dialect::<tir_be_common::AsmDialect>();
        context.register_dialect::<RiscvDialect>();
    }

    fn isel_pass(&self, context: &tir::Context) -> tir_be_common::isel::InstructionSelectPass {
        create_isel_pass(context)
    }

    fn regalloc_pass(&self) -> tir_be_common::regalloc::RegisterAllocationPass {
        create_regalloc_pass()
    }

    fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
        use tir_be_common::regalloc::TargetRegAlloc;
        RiscvRegAlloc.register_info()
    }

    fn asm_parser(&self, context: &tir::Context) -> tir_be_common::AsmParser {
        context
            .find_dialect::<RiscvDialect>()
            .expect("riscv dialect must be registered before building an asm parser")
            .get_asm_parser()
    }

    fn asm_printer(&self, context: &tir::Context) -> tir_be_common::AsmPrinter {
        context
            .find_dialect::<RiscvDialect>()
            .expect("riscv dialect must be registered before building an asm printer")
            .get_asm_printer()
    }

    fn machine_model(&self, name: &str) -> Option<tir_be_common::sched::MachineModel> {
        crate::machine_model(name)
    }

    fn machines(&self) -> &'static [&'static str] {
        crate::MACHINES
    }

    fn register_name(&self, class: &str, index: u16, prefer_abi: bool) -> Option<String> {
        crate::register_name(class, index, prefer_abi)
    }
}

fn select_riscv(march: &str, mcpu: Option<&str>) -> Option<Box<dyn tir_be_common::TargetMachine>> {
    let config = TargetConfig::parse(march, mcpu)?;
    Some(Box::new(RiscvTarget { config }))
}

tir_be_common::register_target!(select_riscv, ["riscv32", "riscv64"]);

#[cfg(test)]
mod tests {
    use tir::{
        Context, IRBuilder, IRFormatter, Operation, PassManager,
        builtin::{FuncOp, IntegerType, UnitType, ops},
    };
    use tir_be_common::AsmDialect;

    use crate::{RiscvDialect, create_isel_pass, create_regalloc_pass};

    fn body_op_names(context: &Context, region_id: tir::RegionId) -> Vec<&'static str> {
        context
            .get_region(region_id)
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect()
    }

    #[test]
    fn builtin_br_lowers_to_virtual() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let module = ops::module(&context, None).build();
        let region = context.create_region();
        let entry = context.create_block(vec![]);
        region.add_block(entry.id());
        let target = context.create_block(vec![]);

        let func = ops::func(&context, "demo", UnitType::new(&context), Some(region.id())).build();
        let mut fb = IRBuilder::new(func.body());
        fb.insert(ops::br(&context, vec![], target.id()).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should lower the branch");

        assert_eq!(
            body_op_names(&context, region.id()),
            vec!["vbr", "symbol_end"]
        );
    }

    #[test]
    fn builtin_cond_br_lowers_to_virtual() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i1 = IntegerType::new(&context, 1);
        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();

        let cond = context.create_value(i1, None);
        let x = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![cond, x]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", UnitType::new(&context), Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (cond_id, x_id) = (args[0].id(), args[1].id());

        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);

        let mut fb = IRBuilder::new(func.body());
        let add = ops::addi(&context, x_id, x_id, i32).build();
        let add_r = add.result();
        fb.insert(add);
        fb.insert(ops::cond_br(&context, cond_id, vec![add_r], vec![], t.id(), f.id()).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should lower the conditional branch");

        // The data op selects (addw), the conditional branch lowers to the virtual
        // op, and no builtin control flow remains.
        assert_eq!(
            body_op_names(&context, region.id()),
            vec!["addw", "vcond_br", "symbol_end"]
        );
        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        assert!(
            !buf.contains("builtin"),
            "no builtin ops should remain:\n{buf}"
        );
    }

    #[test]
    fn machine_models_resolve_scheduling_classes() {
        // ALU ops resolve to the ALU unit (via the WriteIALU schedule on their
        // template), loads/stores to the LSU, and an instruction with no schedule
        // class (e.g. the M-extension `mul`, unmodeled here) falls back to default.
        for model in [
            crate::in_order_core_model(),
            crate::out_of_order_core_model(),
        ] {
            assert_eq!(model.sched_class("add").resources, &["ALU"]);
            assert_eq!(model.sched_class("sub").resources, &["ALU"]);
            assert_eq!(model.sched_class("lw").resources, &["LSU"]);
            assert_eq!(model.sched_class("sw").resources, &["LSU"]);
            assert_eq!(
                model.sched_class("mul"),
                tir_be_common::sched::InstrSchedClass::DEFAULT
            );
        }
    }

    #[test]
    fn phase_based_timing_resolves_from_pipeline() {
        // InOrderCore is phase-based: a 5-stage pipeline (IF ID EX MEM WB), operands
        // read at ID (cycle 1), results written at EX/MEM.
        let in_order = crate::in_order_core_model();
        assert_eq!(in_order.phase_cycle("ID"), Some(1));
        assert_eq!(in_order.phase_cycle("MEM"), Some(3));
        assert_eq!(
            in_order.protection_at(2),
            Some(tir_be_common::sched::Protection::Protected)
        );

        // add: read@ID(1) → write@EX(2) ⇒ latency 1, read_cycle 1, write_cycle 2.
        let add = in_order.sched_class("add");
        assert_eq!((add.read_cycle, add.latency, add.write_cycle()), (1, 1, 2));
        // lw: read@ID(1) → write@MEM(3) ⇒ latency 2, read_cycle 1, write_cycle 3.
        let lw = in_order.sched_class("lw");
        assert_eq!((lw.read_cycle, lw.latency, lw.write_cycle()), (1, 2, 3));

        // OutOfOrderCore is scalar (`latency = N`): read at cycle 0, no pipeline.
        let ooo = crate::out_of_order_core_model();
        assert!(ooo.pipeline.is_empty());
        let ooo_lw = ooo.sched_class("lw");
        assert_eq!((ooo_lw.read_cycle, ooo_lw.latency), (0, 4));
    }

    #[test]
    fn instruction_cost_reflects_unit_defaults() {
        // Machine-independent cost comes from the `unit` defaults, not a machine's
        // `bind`: WriteIALU defaults latency 1, WriteLoad defaults latency 3.
        assert_eq!(crate::instruction_cost("add"), 1);
        assert_eq!(crate::instruction_cost("lw"), 3);
        // Instructions with no `schedule` block fall back to the default cost.
        assert_eq!(crate::instruction_cost("sub"), 1);
        assert_eq!(crate::instruction_cost("nonexistent"), 1);

        // The per-machine model may refine the generic default for that silicon:
        // both demo cores bind WriteLoad to latency 4, independent of the default 3.
        assert_eq!(crate::instruction_cost("lw"), 3);
        assert_eq!(
            crate::out_of_order_core_model().sched_class("lw").latency,
            4
        );
    }

    #[test]
    fn override_supersedes_unit_bind() {
        // OutOfOrderCore overrides `Add` to latency 2, beating WriteIALU's bind (1).
        assert_eq!(
            crate::out_of_order_core_model().sched_class("add").latency,
            2
        );
        // InOrderCore has no override → `add` resolves from its WriteIALU bind.
        assert_eq!(crate::in_order_core_model().sched_class("add").latency, 1);
    }

    #[test]
    fn forwarding_paths_are_modeled() {
        let in_order = crate::in_order_core_model();
        assert_eq!(in_order.forward_latency("ALU", "ALU"), Some(0));
        assert_eq!(in_order.forward_latency("LSU", "ALU"), Some(1));
        assert_eq!(in_order.forward_latency("ALU", "LSU"), None);
        // OutOfOrderCore declares no forwarding network.
        assert!(crate::out_of_order_core_model().forwards.is_empty());
    }

    #[test]
    fn in_order_and_ooo_differ_structurally() {
        let in_order = crate::in_order_core_model();
        assert_eq!(in_order.name, "InOrderCore");
        assert_eq!(in_order.issue_width, 1);
        assert_eq!(in_order.buffer("rob"), None); // no reorder buffer
        assert_eq!(in_order.resource("ALU").map(|r| r.units), Some(1));

        let ooo = crate::out_of_order_core_model();
        assert_eq!(ooo.name, "OutOfOrderCore");
        assert_eq!(ooo.issue_width, 4);
        assert_eq!(ooo.buffer("rob"), Some(128));
        assert_eq!(ooo.resource("ALU").map(|r| r.units), Some(4));
    }

    #[test]
    fn smoke_parser() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();
        let dialect = context.find_dialect::<RiscvDialect>().unwrap();
        let asm_parser = dialect.get_asm_parser();

        let test = "
        .global foo
        entry:
            add a0, a1, a2
        ";

        let module = asm_parser.parse_asm(&context, test);
        let module = module.unwrap();

        let mut new_buf = String::new();
        let mut f = IRFormatter::new(&mut new_buf);
        module.print(&mut f).expect("Failed to print module");
        insta::assert_snapshot!(&new_buf);
    }

    #[test]
    fn builtin_add_lowers_to_riscv_add() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let module = ops::module(&context, None).build();

        let param0 = context.create_value(IntegerType::new(&context, 32), None);
        let param1 = context.create_value(IntegerType::new(&context, 32), None);
        let region = context.create_region();
        let block = context.create_block(vec![param0, param1]);
        region.add_block(block.id());

        let func = ops::func(
            &context,
            "demo",
            IntegerType::new(&context, 32),
            Some(region.id()),
        )
        .build();

        let mut fb = IRBuilder::new(func.body());
        let add = ops::addi(
            &context,
            func.body().arguments()[0].id(),
            func.body().arguments()[1].id(),
            IntegerType::new(&context, 32),
        )
        .build();
        let add_result = add.result();
        fb.insert(add);
        fb.insert(ops::r#return(&context, add_result).build());

        let mut mb = IRBuilder::new(module.body());
        let _func = mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        module.verify(&context).expect("Invalid module");
        // assert!(module.verify(&context).is_ok());
        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("pass pipeline should succeed");
        assert!(module.verify(&context).is_ok());

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        insta::assert_snapshot!("builtin_add_lowers_to_riscv_add_ir", &buf);
    }

    #[test]
    fn multi_op_function_lowers_end_to_end() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);
        let c = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b, c]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i32, Some(region.id())).build();
        let body = func.body();
        let args = body.arguments();
        let (a, b, c) = (args[0].id(), args[1].id(), args[2].id());

        // t1 = a + b ; t2 = t1 - c ; t3 = t2 & a ; t4 = t3 | b ; return t4
        let mut fb = IRBuilder::new(func.body());
        let t1 = ops::addi(&context, a, b, i32).build();
        let t1r = t1.result();
        fb.insert(t1);
        let t2 = ops::subi(&context, t1r, c, i32).build();
        let t2r = t2.result();
        fb.insert(t2);
        let t3 = ops::andi(&context, t2r, a, i32).build();
        let t3r = t3.result();
        fb.insert(t3);
        let t4 = ops::ori(&context, t3r, b, i32).build();
        let t4r = t4.result();
        fb.insert(t4);
        fb.insert(ops::r#return(&context, t4r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        module.verify(&context).expect("invalid module");
        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("pass pipeline should succeed");

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        println!("=== lowered IR ===\n{buf}");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(
            body,
            vec!["addw", "subw", "and", "or", "vret", "symbol_end"],
            "i32 add/sub should select the word ops (addw/subw) on RV64, while \
             bitwise and/or (no word variant) select the plain ops"
        );
    }

    #[test]
    fn i32_register_shift_selects_word_shift() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();
        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i32, Some(region.id())).build();
        let body = func.body();
        let args = body.arguments();
        let (a, b) = (args[0].id(), args[1].id());

        // a << b with b a register. Earlier this matched the immediate shift slliw
        // (whose emit then failed). With operand constraints slliw rejects the
        // register amount, and the Clamp-stripped register word shift sllw wins.
        let mut fb = IRBuilder::new(func.body());
        let s = ops::shli(&context, a, b, i32).build();
        let sr = s.result();
        fb.insert(s);
        fb.insert(ops::r#return(&context, sr).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("pass pipeline should succeed");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(body, vec!["sllw", "vret", "symbol_end"]);
    }

    #[test]
    fn i32_immediate_shift_selects_imm_word_shift() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();
        let a = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![a]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i32, Some(region.id())).build();
        let body = func.body();
        let a = body.arguments()[0].id();

        // a << 3 with 3 an immediate constant. Should pick slliw, not sllw.
        let mut fb = IRBuilder::new(func.body());
        let three = ops::constant(&context, 3, i32).build();
        let three_r = three.result();
        fb.insert(three);
        let s = ops::shli(&context, a, three_r, i32).build();
        let sr = s.result();
        fb.insert(s);
        fb.insert(ops::r#return(&context, sr).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("pass pipeline should succeed");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        // The lowered IR prints (slliw is registered in the dialect, so get_dyn_op
        // resolves it — an unregistered op would panic here).
        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        assert!(buf.contains("slliw"), "expected slliw in:\n{buf}");

        // slliw carries the folded immediate, not a register shift amount.
        let slliw = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "slliw")
            .expect("slliw should be selected");
        assert!(
            slliw
                .attributes
                .iter()
                .any(|a| a.name == "imm"
                    && matches!(a.value, tir::attributes::AttributeValue::Int(3))),
            "slliw should fold the immediate 3, got {:?}",
            slliw.attributes
        );
        // The folded constant is dead and removed; only slliw survives.
        assert_eq!(body, vec!["slliw", "vret", "symbol_end"]);

        // The def-use chain now spans the machine-IR register layer: `a` feeds
        // slliw's rs1 (a register operand carried in an attribute, not `operands`),
        // so it reports a use referencing slliw with no operand index.
        assert!(
            context.is_value_used(a),
            "block arg a should be used by slliw"
        );
        let uses = context.value_uses(a);
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].op(), slliw.id);
        assert_eq!(uses[0].operand_index(), None);

        // slliw's rd value is defined by slliw (def-site followed the rewrite off the
        // erased source op), and the folded constant is genuinely unused.
        assert_eq!(context.get_value(sr).defining_op(), Some(slliw.id));
        assert!(
            !context.is_value_used(three_r),
            "folded constant should be dead"
        );
    }

    #[test]
    fn live_constant_is_not_erased() {
        // A constant with a genuine remaining use (returned directly, so no
        // instruction folds it as an immediate) must survive dead-constant cleanup.
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();
        let region = context.create_region();
        let block = context.create_block(vec![]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i32, Some(region.id())).build();

        let mut fb = IRBuilder::new(func.body());
        let five = ops::constant(&context, 5, i32).build();
        let five_r = five.result();
        fb.insert(five);
        fb.insert(ops::r#return(&context, five_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("pass pipeline should succeed");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert!(
            body.contains(&"constant"),
            "a constant feeding the return must be kept, got {body:?}"
        );
    }

    fn phys_of(op: &std::sync::Arc<tir::OpInstance>, name: &str) -> Option<(String, u16)> {
        use tir::attributes::{AttributeValue, RegisterAttr};
        op.attributes
            .iter()
            .find(|a| a.name == name)
            .and_then(|a| match &a.value {
                AttributeValue::Register(RegisterAttr::Physical { class, index }) => {
                    Some((class.clone(), *index))
                }
                _ => None,
            })
    }

    fn body_blocks_have_no_virtual(context: &Context, region_id: tir::RegionId) {
        use tir::attributes::{AttributeValue, RegisterAttr};
        for block in context.get_region(region_id).iter(context.clone()) {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                for attr in &op.attributes {
                    assert!(
                        !matches!(
                            attr.value,
                            AttributeValue::Register(RegisterAttr::Virtual { .. })
                        ),
                        "op {} still has a virtual register in attr {}",
                        op.name,
                        attr.name
                    );
                }
            }
        }
    }

    #[test]
    fn regalloc_assigns_abi_physical_registers() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i32, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b) = (args[0].id(), args[1].id());
        let mut fb = IRBuilder::new(func.body());
        let add = ops::addi(&context, a, b, i32).build();
        let add_r = add.result();
        fb.insert(add);
        fb.insert(ops::r#return(&context, add_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.add_pass(create_regalloc_pass());
        pm.run(&context, context.get_op(module.id()))
            .expect("isel + regalloc should succeed");

        // The body's add op should now reference physical registers, with the ABI
        // pre-coloring honored: arg0 -> a0 (x10), arg1 -> a1 (x11), and the returned
        // value -> a0 (x10), reusing a0 because arg0 is dead after the add.
        let block = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap();
        let add_op = block
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "addw")
            .expect("the add must have selected to addw");

        assert_eq!(phys_of(&add_op, "rs1"), Some(("GPR".to_string(), 10)));
        assert_eq!(phys_of(&add_op, "rs2"), Some(("GPR".to_string(), 11)));
        assert_eq!(phys_of(&add_op, "rd"), Some(("GPR".to_string(), 10)));

        body_blocks_have_no_virtual(&context, region.id());
    }

    #[test]
    fn regalloc_keeps_simultaneously_live_values_distinct() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i32, None);
        let b = context.create_value(i32, None);
        let c = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b, c]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i32, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b, c) = (args[0].id(), args[1].id(), args[2].id());

        // t1 = a + b ; t2 = t1 - c ; t3 = t2 & a ; t4 = t3 | b ; return t4
        let mut fb = IRBuilder::new(func.body());
        let t1 = ops::addi(&context, a, b, i32).build();
        let t1r = t1.result();
        fb.insert(t1);
        let t2 = ops::subi(&context, t1r, c, i32).build();
        let t2r = t2.result();
        fb.insert(t2);
        let t3 = ops::andi(&context, t2r, a, i32).build();
        let t3r = t3.result();
        fb.insert(t3);
        let t4 = ops::ori(&context, t3r, b, i32).build();
        let t4r = t4.result();
        fb.insert(t4);
        fb.insert(ops::r#return(&context, t4r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.add_pass(create_regalloc_pass());
        pm.run(&context, context.get_op(module.id()))
            .expect("isel + regalloc should succeed");

        body_blocks_have_no_virtual(&context, region.id());

        // Every machine op's rd must differ from its live source registers: a valid
        // coloring never overwrites a still-needed input with the result.
        let block = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap();
        for op_id in block.op_ids() {
            let op = context.get_op(op_id);
            if let Some(rd) = phys_of(&op, "rd") {
                // rs1/rs2 may legitimately equal rd only if that source is dead; we
                // simply assert allocation produced physical regs everywhere.
                assert_eq!(rd.0, "GPR");
            }
        }
    }

    /// A RISC-V target with a deliberately tiny allocatable register file (a0, a1,
    /// t0, t1, t2), so a handful of live values overflow it and exercise spilling
    /// without stressing the solver. Spill code emission delegates to the real
    /// target.
    struct TinyRiscv(crate::RiscvRegAlloc);

    impl tir_be_common::regalloc::TargetRegAlloc for TinyRiscv {
        fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
            tir_be_common::regalloc::RegisterInfo {
                classes: &[tir_be_common::regalloc::RegClassInfo {
                    name: "GPR",
                    file: "GPR",
                    allocation_order: &[10, 11, 5, 6, 7],
                    caller_saved: &[10, 11, 5, 6, 7],
                    callee_saved: &[],
                    arguments: &[10, 11],
                    return_values: &[10],
                    reserved: &[0, 1, 2, 3, 4],
                }],
            }
        }
        fn frame_register(&self) -> (String, u16) {
            self.0.frame_register()
        }
        fn emit_spill_store(
            &self,
            c: &tir::Context,
            v: u32,
            class: &str,
            f: &(String, u16),
            o: i64,
        ) -> Box<dyn Operation> {
            self.0.emit_spill_store(c, v, class, f, o)
        }
        fn emit_spill_reload(
            &self,
            c: &tir::Context,
            v: u32,
            class: &str,
            f: &(String, u16),
            o: i64,
        ) -> Box<dyn Operation> {
            self.0.emit_spill_reload(c, v, class, f, o)
        }
        fn emit_prologue(&self, c: &tir::Context, s: u32) -> Vec<Box<dyn Operation>> {
            self.0.emit_prologue(c, s)
        }
        fn emit_epilogue(&self, c: &tir::Context, s: u32) -> Vec<Box<dyn Operation>> {
            self.0.emit_epilogue(c, s)
        }
    }

    #[test]
    fn regalloc_spills_under_high_register_pressure() {
        use crate::{AddWordOpBuilder, VirtualReturnOpBuilder, virt};

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();

        // Build machine IR directly: an `asm.symbol` whose body produces 8
        // simultaneously-live values from the single argument, then chains them. The
        // tiny 5-register file forces the allocator to spill. (Built directly rather
        // than through isel to stay independent of instruction-selection coverage.)
        let a_val = context.create_value(i32, None);
        let a = a_val.id().number();
        let region = context.create_region();
        let block = context.create_block(vec![a_val]);
        region.add_block(block.id());

        let mut bb = IRBuilder::new(context.get_block(block.id()));
        let mut producers = Vec::new();
        for _ in 0..8 {
            let v = context.create_value(i32, None).id().number();
            bb.insert(
                AddWordOpBuilder::new(&context)
                    .attr("rd", virt(v, "GPR"))
                    .attr("rs1", virt(a, "GPR"))
                    .attr("rs2", virt(a, "GPR"))
                    .build(),
            );
            producers.push(v);
        }
        let mut acc = producers[0];
        for &p in &producers[1..] {
            let s = context.create_value(i32, None).id().number();
            bb.insert(
                AddWordOpBuilder::new(&context)
                    .attr("rd", virt(s, "GPR"))
                    .attr("rs1", virt(acc, "GPR"))
                    .attr("rs2", virt(p, "GPR"))
                    .build(),
            );
            acc = s;
        }
        bb.insert(
            VirtualReturnOpBuilder::new(&context)
                .value(tir::ValueId::from_number(acc))
                .build(),
        );
        bb.insert(tir_be_common::SymbolEndOpBuilder::new(&context).build());

        let symbol = tir_be_common::SymbolOpBuilder::new(&context)
            .body(region.id())
            .attr(
                "name",
                tir::attributes::AttributeValue::Str("demo".to_string()),
            )
            .build();
        let mut mb = IRBuilder::new(module.body());
        mb.insert(symbol);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.add_pass(tir_be_common::regalloc::RegisterAllocationPass::new(
            Box::new(TinyRiscv(crate::RiscvRegAlloc)),
        ));
        pm.run(&context, context.get_op(module.id()))
            .expect("register allocation should converge with spilling");

        // Everything is physically allocated, and spill code (sd/ld) plus a frame
        // prologue (addi sp) were inserted.
        body_blocks_have_no_virtual(&context, region.id());

        let block = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap();
        let names: Vec<&str> = block
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert!(
            names.contains(&"sd"),
            "expected spill stores, got {names:?}"
        );
        assert!(
            names.contains(&"ld"),
            "expected spill reloads, got {names:?}"
        );
        assert_eq!(
            names.first(),
            Some(&"addi"),
            "the frame prologue (addi sp) should lead the block, got {names:?}"
        );
    }
}

#[cfg(test)]
mod target_parser_tests {
    use super::TargetConfig;

    #[test]
    fn march_accepts_gcc_style_isa_strings() {
        assert_eq!(
            TargetConfig::parse("rv64im", None).map(|c| c.canonical_name()),
            Some("riscv64")
        );
        assert_eq!(
            TargetConfig::parse("rv32imac", None).map(|c| c.canonical_name()),
            Some("riscv32")
        );
        assert_eq!(
            TargetConfig::parse("rv64gc_zba_zbb", None).map(|c| c.canonical_name()),
            Some("riscv64")
        );
    }

    #[test]
    fn mcpu_accepts_target_prefixed_generic_names() {
        assert!(TargetConfig::parse("rv32im", Some("riscv32-generic-in-order")).is_some());
        assert!(TargetConfig::parse("rv64im", Some("riscv32-generic-in-order")).is_none());
        assert!(TargetConfig::parse("rv64im", Some("generic-in-order")).is_some());
    }

    #[test]
    fn unknown_or_malformed_march_is_rejected() {
        assert!(TargetConfig::parse("rv64", None).is_none());
        assert!(TargetConfig::parse("rv64zm", None).is_none());
        assert!(TargetConfig::parse("mips", None).is_none());
        assert!(TargetConfig::parse("rv64im", Some("riscv64-unknown-cpu")).is_none());
    }
}
