use tir::helpers::{dialect, operation};
use tir::{Any, Operation};

mod obj;

include!(concat!(env!("OUT_DIR"), "/arm64.rs"));

/// Parsed AArch64 target selection from `--march`/`--mcpu`/`--mattr`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetConfig {
    features: Vec<Feature>,
    /// Machine model implied by `--mcpu`, when it names one.
    machine: Option<String>,
}

impl TargetConfig {
    /// Parse an AArch64 `--march`/`--mcpu`/`--mattr` triple.
    pub fn parse(march: &str, mcpu: Option<&str>, mattr: Option<&str>) -> Result<Self, String> {
        parse_march(march)?;
        let mut config = TargetConfig {
            features: vec![Feature::ARMv8A64],
            machine: None,
        };
        if let Some(mattr) = mattr {
            apply_mattr(&mut config.features, mattr)?;
        }
        validate_features(&config.features)?;
        if !config.features.contains(&Feature::ARMv8A64) {
            return Err("--mattr must not disable the base ISA 'ARMv8A64'".to_string());
        }
        if let Some(mcpu) = mcpu {
            config.machine = parse_mcpu(mcpu, &config)?;
        }
        Ok(config)
    }

    /// Canonical architecture name for diagnostics and target-specific behavior.
    pub fn canonical_name(&self) -> &'static str {
        "arm64"
    }

    /// The enabled ISA/extension set.
    pub fn features(&self) -> &[Feature] {
        &self.features
    }
}

fn parse_march(march: &str) -> Result<(), String> {
    match normalize(march).as_str() {
        "arm64" | "aarch64" | "armv8" | "armv8a" | "armv8-a" => Ok(()),
        other => Err(format!("unknown AArch64 architecture '{other}'")),
    }
}

/// Resolve `--mcpu` to an optional default machine model. Generic CPU names map
/// onto the generic cores; any other name must be a TMDL machine (by name or
/// alias) compatible with the enabled features.
fn parse_mcpu(mcpu: &str, config: &TargetConfig) -> Result<Option<String>, String> {
    let name = normalize(mcpu);
    let generic = match name.as_str() {
        "generic" | "generic-arm64" | "generic-aarch64" => Some(None),
        "generic-in-order" | "generic-inorder" | "in-order" | "inorder" => {
            Some(Some("arm64-in-order".to_string()))
        }
        "generic-ooo" | "generic-out-of-order" | "ooo" | "out-of-order" => {
            Some(Some("arm64-ooo".to_string()))
        }
        _ => None,
    };
    if let Some(machine) = generic {
        return Ok(machine);
    }

    if machine_model(&name, &config.features).is_some() {
        return Ok(Some(name));
    }
    if machine_model(&name, Feature::ALL).is_some() {
        return Err(format!(
            "cpu '{name}' is incompatible with the selected architecture"
        ));
    }
    Err(format!(
        "unknown AArch64 cpu '{name}' (expected 'generic', 'generic-in-order', 'generic-ooo' or one of: {})",
        machines(Feature::ALL).join(", ")
    ))
}

/// Apply an LLVM-style `--mattr` list (`+feat`/`-feat`, comma-separated).
fn apply_mattr(features: &mut Vec<Feature>, mattr: &str) -> Result<(), String> {
    for item in mattr.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (add, name) = if let Some(name) = item.strip_prefix('+') {
            (true, name)
        } else if let Some(name) = item.strip_prefix('-') {
            (false, name)
        } else {
            return Err(format!(
                "invalid --mattr entry '{item}' (expected '+feature' or '-feature')"
            ));
        };
        let feature = Feature::from_name(&normalize(name))
            .ok_or_else(|| format!("unknown AArch64 feature '{name}' in --mattr"))?;
        if add && !features.contains(&feature) {
            features.push(feature);
        } else if !add {
            features.retain(|f| *f != feature);
        }
    }
    Ok(())
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

operation! {
    VirtualReturnOp {
        name: "vret",
        dialect: "arm64",
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
        dialect: "arm64",
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
        tir::backend::print_branch(fmt, self, "arm64.vbr")
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
        dialect: "arm64",
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
        tir::backend::print_branch(fmt, self, "arm64.vcond_br")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

// Virtual call ops: the lowered form of `builtin.call`/`builtin.indirect_call`.
// Arguments and results travel through the ABI registers via copies emitted by
// `lower_calls`; the ops only carry the callee (a symbol whose address is
// resolved at link time, or an already-colored register) plus the caller-saved
// clobber list, deferring the actual `bl`/`blr` encoding to a post-RA pass.
operation! {
    VirtualCallOp {
        name: "vcall",
        dialect: "arm64",
        attributes: A {
            callee: "Str",
        },
        roles: R {
            clobbers: Clobber,
        },
    }
}

operation! {
    VirtualIndirectCallOp {
        name: "vcall_indirect",
        dialect: "arm64",
        attributes: A {
            callee_reg: "Register",
        },
        roles: R {
            callee_reg: Use,
            clobbers: Clobber,
        },
    }
}

dialect! {
    Arm64Dialect {
        name: "arm64",
        operations: [
            VirtualReturnOp,
            VirtualBranchOp,
            VirtualCondBranchOp,
            VirtualCallOp,
            VirtualIndirectCallOp,
            AddOp,
            SubOp,
            AddImmediateOp,
            SubImmediateOp,
            AndOp,
            OrOp,
            XorOp,
            LogicalShiftLeftVariableOp,
            LogicalShiftRightVariableOp,
            ArithmeticShiftRightVariableOp,
            CompareOp,
            MoveWideZeroOp,
            LoadByteUnsignedOp,
            LoadHalfwordUnsignedOp,
            LoadWordUnsignedOp,
            LoadDoublewordOp,
            LoadByteSignedOp,
            LoadHalfwordSignedOp,
            LoadWordSignedOp,
            StoreByteOp,
            StoreHalfwordOp,
            StoreWordOp,
            StoreDoublewordOp,
            BranchImmediateOp,
            BranchLinkOp,
            BranchRegisterOp,
            BranchLinkRegOp,
            ReturnOp,
            BranchEqOp,
            BranchNotEqOp,
            BranchLessThanOp,
            BranchGreaterEqOp,
            BranchLowerUnsignedOp,
            BranchHigherOrSameUnsignedOp,
        ],
    }
}

fn lower_func_and_return_to_asm_symbol(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::builtin::{FuncOp, ReturnOp};

    if let Some(func) = op.as_op::<FuncOp>() {
        // asm.symbol regions require an explicit symbol_end terminator.
        let body = func.body();
        let has_symbol_end = body
            .op_ids()
            .last()
            .map(|id| context.get_op(*id).name == tir::backend::SymbolEndOp::name())
            .unwrap_or(false);
        if !has_symbol_end {
            let mut b = tir::IRBuilder::new(body);
            b.insert(tir::backend::SymbolEndOpBuilder::new(context).build());
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

        let lowered = tir::backend::SymbolOpBuilder::new(context)
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

impl Arm64Dialect {
    pub fn get_asm_parser(&self) -> tir::backend::AsmParser {
        tir::backend::AsmParser::new(get_instruction_parsers(Feature::ALL).0)
    }

    pub fn get_asm_printer(&self) -> tir::backend::AsmPrinter {
        tir::backend::AsmPrinter::new(get_instruction_printers())
    }
}

/// Lower the builtin control-flow terminators to AArch64 virtual branch ops,
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

/// The AArch64 link register (`lr` = `x30`) and zero register (`xzr` = slot 31).
const LR: u16 = 30;
const XZR: u16 = 31;

/// Build a register-register move (`orr rd, xzr, rm`).
fn mv(
    context: &tir::Context,
    rd: tir::attributes::AttributeValue,
    rm: tir::attributes::AttributeValue,
) -> Box<dyn Operation> {
    Box::new(
        OrOpBuilder::new(context)
            .attr("rd", rd)
            .attr("rn", phys(&("GPR".to_string(), XZR)))
            .attr("rm", rm)
            .build(),
    )
}

/// Lower the builtin call ops to AArch64 virtual calls. Arguments are moved
/// into the ABI argument registers and the result is copied out of the first
/// return register, so the allocator never has to pin long live ranges. The
/// call clobbers every caller-saved register — including `lr`, which also holds
/// this function's own return address, so it is saved into a fresh virtual
/// register across the call.
fn lower_calls(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::attributes::AttributeValue;
    use tir::builtin::{CallOp, IndirectCallOp, UnitType};

    let (callee_value, args, result) = if let Some(call) = op.as_op::<CallOp>() {
        (None, call.args(), call.result())
    } else if let Some(call) = op.as_op::<IndirectCallOp>() {
        (Some(call.callee()), call.args(), call.result())
    } else {
        return Ok(false);
    };

    let info = register_info();
    let class = info
        .class("GPR")
        .expect("arm64 register info must define GPR");
    if args.len() > class.arguments.len() {
        return Err(tir::PassError::InvalidRuleSet(
            "stack-passed call arguments are not supported by codegen yet".to_string(),
        ));
    }

    // Detach the callee and every argument into fresh virtual registers before
    // any argument register is written: an operand may itself live in an
    // argument register (e.g. this function's own incoming arguments), so it
    // must be read before the moves below clobber them, whatever the argument
    // permutation.
    let detach = |rewriter: &mut tir::Rewriter, value: tir::ValueId| {
        let ty = context.get_value(value).ty();
        let fresh = context.create_value(ty, None).id().number();
        let copy = mv(context, virt(fresh, "GPR"), virt(value.number(), "GPR"));
        rewriter.insert_op_before(op, copy.as_ref()).map(|()| fresh)
    };
    let fresh_callee = callee_value
        .map(|value| detach(rewriter, value))
        .transpose()?;
    let mut fresh_args = Vec::with_capacity(args.len());
    for arg in &args {
        fresh_args.push(detach(rewriter, *arg)?);
    }
    for (&fresh, &reg) in fresh_args.iter().zip(class.arguments.iter()) {
        let copy = mv(context, phys(&("GPR".to_string(), reg)), virt(fresh, "GPR"));
        rewriter.insert_op_before(op, copy.as_ref())?;
    }

    let virtual_call: Box<dyn Operation> = match fresh_callee {
        None => {
            let name = op.as_op::<CallOp>().expect("matched above").callee();
            Box::new(
                VirtualCallOpBuilder::new(context)
                    .attr("callee", AttributeValue::Str(name))
                    .attr("clobbers", caller_saved_clobbers())
                    .build(),
            )
        }
        Some(fresh) => Box::new(
            VirtualIndirectCallOpBuilder::new(context)
                .attr("callee_reg", virt(fresh, "GPR"))
                .attr("clobbers", caller_saved_clobbers())
                .build(),
        ),
    };

    let lr = ("GPR".to_string(), LR);
    let saved_lr = context
        .create_value(tir::builtin::IntegerType::new(context, 64), None)
        .id();
    let save = mv(context, virt(saved_lr.number(), "GPR"), phys(&lr));
    rewriter.insert_op_before(op, save.as_ref())?;
    rewriter.insert_op_before(op, virtual_call.as_ref())?;
    let restore = mv(context, phys(&lr), virt(saved_lr.number(), "GPR"));

    let ret_reg = class.return_values[0];
    if context.get_value(result).ty() == UnitType::new(context) {
        rewriter.replace_op(op, restore.as_ref())?;
    } else {
        rewriter.insert_op_before(op, restore.as_ref())?;
        let copy = mv(
            context,
            virt(result.number(), "GPR"),
            phys(&("GPR".to_string(), ret_reg)),
        );
        rewriter.replace_op(op, copy.as_ref())?;
    }
    Ok(true)
}

/// The caller-saved registers a call clobbers, as a register-array attribute.
fn caller_saved_clobbers() -> tir::attributes::AttributeValue {
    let info = register_info();
    let class = info
        .class("GPR")
        .expect("arm64 register info must define GPR");
    tir::attributes::AttributeValue::Array(
        class
            .caller_saved
            .iter()
            .map(|&index| phys(&("GPR".to_string(), index)))
            .collect(),
    )
}

pub fn create_isel_pass(context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
    create_isel_pass_for(context, Feature::ALL)
}

fn create_isel_pass_for(
    context: &tir::Context,
    features: &[Feature],
) -> tir::backend::isel::InstructionSelectPass {
    tir::backend::isel::InstructionSelectPass::new(get_isel_rules(context, features))
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
        .with_op_lowering(lower_branches)
        .with_op_lowering(lower_calls)
}

/// The AArch64 frame base register (`x29`/`fp`), reserved from allocation and used
/// as the base for spill slots.
const FP: (&str, u16) = ("GPR", 29);

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

/// AArch64 register allocation target: the generated register file plus `str`/`ldr`
/// spill code and a `sub fp, fp, #frame` / `add fp, fp, #frame` prologue/epilogue.
pub struct Arm64RegAlloc;

impl tir::backend::regalloc::TargetRegAlloc for Arm64RegAlloc {
    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        register_info()
    }

    fn frame_register(&self) -> (String, u16) {
        (FP.0.to_string(), FP.1)
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
            StoreDoublewordOpBuilder::new(context)
                .attr("rt", virt(value, class))
                .attr("rn", phys(frame))
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
            LoadDoublewordOpBuilder::new(context)
                .attr("rt", virt(value, class))
                .attr("rn", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )
    }

    fn emit_prologue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        vec![Box::new(
            SubImmediateOpBuilder::new(context)
                .attr("rd", phys(&(FP.0.to_string(), FP.1)))
                .attr("rn", phys(&(FP.0.to_string(), FP.1)))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        )]
    }

    fn emit_epilogue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        vec![Box::new(
            AddImmediateOpBuilder::new(context)
                .attr("rd", phys(&(FP.0.to_string(), FP.1)))
                .attr("rn", phys(&(FP.0.to_string(), FP.1)))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        )]
    }
}

pub fn create_regalloc_pass() -> tir::backend::regalloc::RegisterAllocationPass {
    tir::backend::regalloc::RegisterAllocationPass::new(Box::new(Arm64RegAlloc))
}

/// The AArch64 (ARMv8-A) target, selected via `--march`/`--mcpu`.
pub struct Arm64Target {
    config: TargetConfig,
}

impl tir::backend::TargetMachine for Arm64Target {
    fn name(&self) -> &'static str {
        self.config.canonical_name()
    }

    fn register_dialects(&self, context: &tir::Context) {
        context.register_dialect::<tir::backend::AsmDialect>();
        context.register_dialect::<Arm64Dialect>();
    }

    fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
        create_isel_pass_for(context, &self.config.features)
    }

    fn regalloc_pass(&self) -> tir::backend::regalloc::RegisterAllocationPass {
        create_regalloc_pass()
    }

    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        use tir::backend::regalloc::TargetRegAlloc;
        Arm64RegAlloc.register_info()
    }

    fn asm_parser(&self, _context: &tir::Context) -> tir::backend::AsmParser {
        let (parsers, disabled) = get_instruction_parsers(&self.config.features);
        tir::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
    }

    fn asm_printer(&self, context: &tir::Context) -> tir::backend::AsmPrinter {
        context
            .find_dialect::<Arm64Dialect>()
            .expect("arm64 dialect must be registered before building an asm printer")
            .get_asm_printer()
    }

    fn machine_model(&self, name: &str) -> Option<tir::backend::sched::MachineModel> {
        crate::machine_model(name, &self.config.features)
    }

    fn machines(&self) -> Vec<&'static str> {
        crate::machines(&self.config.features)
    }

    fn default_machine(&self) -> Option<&str> {
        self.config.machine.as_deref()
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

    fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        vec![obj::lower_constant, obj::lower_vcond_br]
    }

    fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        vec![obj::finalize_virtual_ops]
    }

    fn object_format(&self) -> Option<tir::backend::binary::ObjectFormatInfo> {
        Some(obj::object_format())
    }

    fn binary_writer(&self, _context: &tir::Context) -> Option<tir::backend::binary::BinaryWriter> {
        Some(tir::backend::binary::BinaryWriter::new(
            get_instruction_encoders(),
            get_instruction_patchers(),
        ))
    }
}

fn select_arm64(
    march: &str,
    mcpu: Option<&str>,
    mattr: Option<&str>,
) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
    let owned = ["arm", "aarch64"]
        .iter()
        .any(|prefix| normalize(march).starts_with(prefix));
    if !owned {
        return Ok(None);
    }
    let config = TargetConfig::parse(march, mcpu, mattr)?;
    Ok(Some(Box::new(Arm64Target { config })))
}

tir::register_target!(select_arm64, ["arm64"]);

#[cfg(test)]
mod tests {
    use tir::backend::AsmDialect;
    use tir::{
        Context, IRBuilder, IRFormatter, Operation, PassManager,
        builtin::{FuncOp, IntegerType, UnitType, ops},
    };

    use crate::{Arm64Dialect, create_isel_pass, create_regalloc_pass};

    #[test]
    fn arm64_builtin_cond_br_lowers_to_virtual() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i1 = IntegerType::new(&context, 1);
        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let cond = context.create_value(i1, None);
        let x = context.create_value(i64, None);
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
        let add = ops::addi(&context, x_id, x_id, i64).build();
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

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(body, vec!["add", "vcond_br", "symbol_end"]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        assert!(
            !buf.contains("builtin"),
            "no builtin ops should remain:\n{buf}"
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
    fn arm64_assembly_parser_rejects_fuzzer_crash_without_panicking() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();
        let arm64 = context.find_dialect::<Arm64Dialect>().unwrap();
        let parser = arm64.get_asm_parser();

        assert!(parser.parse_asm(&context, ".0").is_err());
    }

    #[test]
    fn arm64_add_lowers_to_arm64_add() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b) = (args[0].id(), args[1].id());
        let mut fb = IRBuilder::new(func.body());
        let add = ops::addi(&context, a, b, i64).build();
        let add_r = add.result();
        fb.insert(add);
        fb.insert(ops::r#return(&context, add_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        module.verify(&context).expect("invalid module");
        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should succeed");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(body, vec!["add", "vret", "symbol_end"]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        assert!(
            !buf.contains("builtin"),
            "no builtin ops should remain:\n{buf}"
        );
    }

    #[test]
    fn arm64_multi_op_function_lowers_end_to_end() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let c = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b, c]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b, c) = (args[0].id(), args[1].id(), args[2].id());

        // t1 = a + b ; t2 = t1 - c ; t3 = t2 & a ; t4 = t3 | b ; return t4
        let mut fb = IRBuilder::new(func.body());
        let t1 = ops::addi(&context, a, b, i64).build();
        let t1r = t1.result();
        fb.insert(t1);
        let t2 = ops::subi(&context, t1r, c, i64).build();
        let t2r = t2.result();
        fb.insert(t2);
        let t3 = ops::andi(&context, t2r, a, i64).build();
        let t3r = t3.result();
        fb.insert(t3);
        let t4 = ops::ori(&context, t3r, b, i64).build();
        let t4r = t4.result();
        fb.insert(t4);
        fb.insert(ops::r#return(&context, t4r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should succeed");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(body, vec!["add", "sub", "and", "orr", "vret", "symbol_end"]);
    }

    #[test]
    fn arm64_regalloc_assigns_abi_physical_registers() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b) = (args[0].id(), args[1].id());
        let mut fb = IRBuilder::new(func.body());
        let add = ops::addi(&context, a, b, i64).build();
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

        // AAPCS64 pre-coloring: arg0 -> x0, arg1 -> x1, return value -> x0 (reusing x0
        // because arg0 is dead after the add).
        let add_op = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "add")
            .expect("the add must survive selection");

        assert_eq!(phys_of(&add_op, "rn"), Some(("GPR".to_string(), 0)));
        assert_eq!(phys_of(&add_op, "rm"), Some(("GPR".to_string(), 1)));
        assert_eq!(phys_of(&add_op, "rd"), Some(("GPR".to_string(), 0)));

        body_blocks_have_no_virtual(&context, region.id());
    }

    /// An AArch64 target with a tiny allocatable register file (x0..x4) so a handful
    /// of live values overflow it and exercise spilling; spill emission delegates to
    /// the real target.
    struct TinyArm64(crate::Arm64RegAlloc);

    impl tir::backend::regalloc::TargetRegAlloc for TinyArm64 {
        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            tir::backend::regalloc::RegisterInfo {
                classes: &[tir::backend::regalloc::RegClassInfo {
                    name: "GPR",
                    file: "GPR",
                    allocation_order: &[0, 1, 2, 3, 4],
                    caller_saved: &[0, 1, 2, 3, 4],
                    callee_saved: &[],
                    arguments: &[0, 1],
                    return_values: &[0],
                    reserved: &[29, 30, 31],
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
    fn arm64_regalloc_spills_under_high_register_pressure() {
        use crate::{AddOpBuilder, VirtualReturnOpBuilder, virt};

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        // Build machine IR directly: an `asm.symbol` whose body produces 8
        // simultaneously-live values from the single argument, then chains them. The
        // tiny 5-register file forces spilling.
        let a_val = context.create_value(i64, None);
        let a = a_val.id().number();
        let region = context.create_region();
        let block = context.create_block(vec![a_val]);
        region.add_block(block.id());

        let mut bb = IRBuilder::new(context.get_block(block.id()));
        let mut producers = Vec::new();
        for _ in 0..8 {
            let v = context.create_value(i64, None).id().number();
            bb.insert(
                AddOpBuilder::new(&context)
                    .attr("rd", virt(v, "GPR"))
                    .attr("rn", virt(a, "GPR"))
                    .attr("rm", virt(a, "GPR"))
                    .build(),
            );
            producers.push(v);
        }
        let mut acc = producers[0];
        for &p in &producers[1..] {
            let s = context.create_value(i64, None).id().number();
            bb.insert(
                AddOpBuilder::new(&context)
                    .attr("rd", virt(s, "GPR"))
                    .attr("rn", virt(acc, "GPR"))
                    .attr("rm", virt(p, "GPR"))
                    .build(),
            );
            acc = s;
        }
        bb.insert(
            VirtualReturnOpBuilder::new(&context)
                .value(tir::ValueId::from_number(acc))
                .build(),
        );
        bb.insert(tir::backend::SymbolEndOpBuilder::new(&context).build());

        let symbol = tir::backend::SymbolOpBuilder::new(&context)
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
        pm.add_pass(tir::backend::regalloc::RegisterAllocationPass::new(
            Box::new(TinyArm64(crate::Arm64RegAlloc)),
        ));
        pm.run(&context, context.get_op(module.id()))
            .expect("register allocation should converge with spilling");

        body_blocks_have_no_virtual(&context, region.id());

        let names: Vec<&str> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert!(
            names.contains(&"store_doubleword"),
            "expected spill stores, got {names:?}"
        );
        assert!(
            names.contains(&"load_doubleword"),
            "expected spill reloads, got {names:?}"
        );
        assert_eq!(
            names.first(),
            Some(&"sub_imm"),
            "the frame prologue (sub fp) should lead the block, got {names:?}"
        );
    }

    #[test]
    fn encoders_match_isa_golden_words() {
        use crate::{
            AddOpBuilder, BranchEqOpBuilder, BranchImmediateOpBuilder, BranchLinkOpBuilder,
            CompareOpBuilder, LoadDoublewordOpBuilder, LogicalShiftLeftVariableOpBuilder,
            ReturnOpBuilder, StoreDoublewordOpBuilder, phys,
        };
        use tir::attributes::AttributeValue;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let encoders = crate::get_instruction_encoders();
        let gpr = |i: u16| phys(&("GPR".to_string(), i));
        let gprsp = |i: u16| phys(&("GPRsp".to_string(), i));
        let word = |id: tir::OpId| -> u32 {
            let inst = context.get_op(id);
            let enc = encoders[inst.name](&inst)
                .unwrap_or_else(|| panic!("'{}' failed to encode", inst.name));
            assert!(
                enc.fixups.is_empty(),
                "unexpected fixups for '{}'",
                inst.name
            );
            u32::from_le_bytes(enc.bytes.try_into().unwrap())
        };

        // Golden words produced by clang/llvm-mc for aarch64.
        let add = AddOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(add.id()), 0x8B020020, "add x0, x1, x2");

        let lslv = LogicalShiftLeftVariableOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(lslv.id()), 0x9AC22020, "lslv x0, x1, x2");

        let cmp = CompareOpBuilder::new(&context)
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(cmp.id()), 0xEB02003F, "cmp x1, x2");

        let ldr = LoadDoublewordOpBuilder::new(&context)
            .attr("rt", gpr(0))
            .attr("rn", gprsp(1))
            .attr("imm", AttributeValue::Int(0))
            .build();
        assert_eq!(word(ldr.id()), 0xF9400020, "ldr x0, [x1]");

        let str_ = StoreDoublewordOpBuilder::new(&context)
            .attr("rt", gpr(2))
            .attr("rn", gprsp(3))
            .attr("imm", AttributeValue::Int(0))
            .build();
        assert_eq!(word(str_.id()), 0xF9000062, "str x2, [x3]");

        // Branch immediates hold word offsets (the pc-relative byte delta >> 2).
        let beq = BranchEqOpBuilder::new(&context)
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(beq.id()), 0x54000080, "b.eq +16");

        let b = BranchImmediateOpBuilder::new(&context)
            .attr("imm", AttributeValue::Int(3))
            .build();
        assert_eq!(word(b.id()), 0x14000003, "b +12");

        let bl = BranchLinkOpBuilder::new(&context)
            .attr("imm", AttributeValue::Int(2))
            .build();
        assert_eq!(word(bl.id()), 0x94000002, "bl +8");

        let ret = ReturnOpBuilder::new(&context).attr("rn", gpr(30)).build();
        assert_eq!(word(ret.id()), 0xD65F03C0, "ret");

        let movz = crate::MoveWideZeroOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("imm", AttributeValue::Int(42))
            .build();
        assert_eq!(word(movz.id()), 0xD2800540, "movz x0, #42");
    }

    #[test]
    fn symbol_operands_become_fixups() {
        use crate::BranchLinkOpBuilder;
        use tir::attributes::AttributeValue;
        use tir::backend::binary::{FixupTarget, InstFixup};

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let encoders = crate::get_instruction_encoders();
        let patchers = crate::get_instruction_patchers();

        let bl = BranchLinkOpBuilder::new(&context)
            .attr("imm", AttributeValue::Str("foo".to_string()))
            .build();
        let enc = encoders["bl"](&context.get_op(bl.id())).unwrap();
        assert_eq!(enc.bytes, 0x94000000u32.to_le_bytes());
        assert_eq!(
            enc.fixups,
            vec![InstFixup {
                operand: "imm",
                target: FixupTarget::Symbol("foo".to_string()),
            }]
        );

        // The patch value is the word offset; the byte-delta scaling happens
        // in the object writer.
        let mut bytes = enc.bytes.clone();
        patchers["bl"](&mut bytes, 2).unwrap();
        assert_eq!(bytes, 0x94000002u32.to_le_bytes(), "bl +8");

        assert!(patchers["bl"](&mut enc.bytes.clone(), 1 << 25).is_none());
    }

    #[test]
    fn builtin_call_lowers_to_vcall_with_abi_copies() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "caller", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b) = (args[0].id(), args[1].id());

        let mut fb = IRBuilder::new(func.body());
        let call = tir::builtin::CallOpBuilder::new(&context)
            .args(vec![a, b])
            .attr(
                "callee",
                tir::attributes::AttributeValue::Str("foo".to_string()),
            )
            .result_type(i64)
            .build();
        let call_r = call.result();
        fb.insert(call);
        fb.insert(ops::r#return(&context, call_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should lower the call");

        // Two detach copies, two argument copies into x0/x1, the lr save, the
        // virtual call, the lr restore, and the result copy out of x0.
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
            vec![
                "orr",
                "orr",
                "orr",
                "orr",
                "orr",
                "vcall",
                "orr",
                "orr",
                "vret",
                "symbol_end"
            ]
        );
    }

    #[test]
    fn call_finalizes_to_bl_with_symbol_target() {
        use tir::backend::TargetMachine;
        use tir::backend::pipeline::{StopAfter, build_pipeline};

        let context = Context::with_default_dialects();
        let target = crate::Arm64Target {
            config: crate::TargetConfig::parse("arm64", None, None).expect("march should parse"),
        };
        target.register_dialects(&context);

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a]);
        region.add_block(block.id());

        let func = ops::func(&context, "caller", i64, Some(region.id())).build();
        let a = func.body().arguments()[0].id();

        let mut fb = IRBuilder::new(func.body());
        let call = tir::builtin::CallOpBuilder::new(&context)
            .args(vec![a])
            .attr(
                "callee",
                tir::attributes::AttributeValue::Str("foo".to_string()),
            )
            .result_type(i64)
            .build();
        let call_r = call.result();
        fb.insert(call);
        fb.insert(ops::r#return(&context, call_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = build_pipeline(&target, &context, StopAfter::Finalize);
        pm.run(&context, context.get_op(module.id()))
            .expect("pipeline should lower the call");

        let block = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap();
        let names: Vec<_> = block
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        // The lr save has no callee-saved register to live in (the modeled
        // register file marks x0..x28 and x30 caller-saved), so it spills: a
        // frame is opened, lr is stored before the call and reloaded after.
        assert_eq!(
            names,
            vec![
                "sub_imm",
                "orr",
                "orr",
                "orr",
                "store_doubleword",
                "bl",
                "load_doubleword",
                "orr",
                "orr",
                "ret",
                "add_imm",
                "symbol_end"
            ]
        );

        let bl = block
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "bl")
            .expect("the call must finalize to bl");
        // bl targets the callee symbol (a link-time fixup).
        assert!(bl.attributes.iter().any(|a| a.name == "imm"
            && matches!(&a.value, tir::attributes::AttributeValue::Str(s) if s == "foo")));

        body_blocks_have_no_virtual(&context, region.id());
    }

    #[test]
    fn indirect_call_finalizes_to_blr() {
        use tir::backend::TargetMachine;
        use tir::backend::pipeline::{StopAfter, build_pipeline};

        let context = Context::with_default_dialects();
        let target = crate::Arm64Target {
            config: crate::TargetConfig::parse("arm64", None, None).expect("march should parse"),
        };
        target.register_dialects(&context);

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let callee = context.create_value(i64, None);
        let x = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![callee, x]);
        region.add_block(block.id());

        let func = ops::func(&context, "caller", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (callee, x) = (args[0].id(), args[1].id());

        let mut fb = IRBuilder::new(func.body());
        let call = tir::builtin::IndirectCallOpBuilder::new(&context)
            .callee(callee)
            .args(vec![x])
            .result_type(i64)
            .build();
        let call_r = call.result();
        fb.insert(call);
        fb.insert(ops::r#return(&context, call_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = build_pipeline(&target, &context, StopAfter::Finalize);
        pm.run(&context, context.get_op(module.id()))
            .expect("pipeline should lower the indirect call");

        let block = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap();
        let blr = block
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "blr")
            .expect("the indirect call must finalize to blr");
        // The callee register was colored to a real register distinct from the
        // argument being passed in x0.
        let target_reg = phys_of(&blr, "rn").expect("blr target must be physical");
        assert_ne!(target_reg.1, 0);

        body_blocks_have_no_virtual(&context, region.id());
    }
}

#[cfg(test)]
mod target_parser_tests {
    use super::{Feature, TargetConfig};

    #[test]
    fn accepts_arm64_aliases_and_generic_cpus() {
        assert_eq!(
            TargetConfig::parse("aarch64", Some("generic-in-order"), None)
                .map(|c| c.canonical_name()),
            Ok("arm64")
        );
        assert!(TargetConfig::parse("armv8-a", Some("generic-aarch64"), None).is_ok());
    }

    #[test]
    fn generic_cpu_names_resolve_machine_models() {
        let config = TargetConfig::parse("arm64", Some("generic-ooo"), None).unwrap();
        assert_eq!(config.machine.as_deref(), Some("arm64-ooo"));
        let config = TargetConfig::parse("arm64", Some("arm64-in-order"), None).unwrap();
        assert_eq!(config.machine.as_deref(), Some("arm64-in-order"));
    }

    #[test]
    fn march_enables_the_base_isa() {
        let config = TargetConfig::parse("arm64", None, None).unwrap();
        assert_eq!(config.features(), &[Feature::ARMv8A64]);
        assert!(TargetConfig::parse("arm64", None, Some("-armv8a64")).is_err());
    }

    #[test]
    fn rejects_unknown_march_or_cpu() {
        assert!(TargetConfig::parse("rv64im", None, None).is_err());
        assert!(TargetConfig::parse("arm64", Some("cortex-a710"), None).is_err());
    }
}
