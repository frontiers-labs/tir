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
        interfaces: [tir::Terminator],
    }
}

impl tir::Terminator for VirtualReturnOp {}

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
        interfaces: [tir::Terminator],
    }
}

impl tir::Terminator for VirtualBranchOp {
    fn successors(&self) -> Vec<tir::BlockId> {
        tir::backend::branch_successors(self)
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
            VirtualCallOp,
            VirtualIndirectCallOp,
            AddOp,
            SubOp,
            AddImmediateOp,
            SubImmediateOp,
            AndOp,
            OrOp,
            XorOp,
            BicOp,
            OrnOp,
            EonOp,
            LogicalShiftLeftVariableOp,
            LogicalShiftRightVariableOp,
            ArithmeticShiftRightVariableOp,
            RotateRightVariableOp,
            LogicalShiftRightImmOp,
            ArithmeticShiftRightImmOp,
            SignExtendByteOp,
            SignExtendHalfwordOp,
            SignExtendWordOp,
            UnsignedBitfieldMoveOp,
            MoveWideZero32Op,
            AndImmediateOp,
            AndImmediate32Op,
            FAddDoubleOp,
            FSubDoubleOp,
            FMulDoubleOp,
            FDivDoubleOp,
            FMovImmediateDoubleOp,
            FMovDoubleToGeneralOp,
            DupGeneral2DOp,
            AddVector2DOp,
            AddVector4SOp,
            FAddVector2DOp,
            FMulVector2DOp,
            MoveImmediateVector4SOp,
            FMoveImmediateVector2DOp,
            CompareOp,
            CompareNegativeOp,
            TestOp,
            CompareImmediateOp,
            CompareNegImmediateOp,
            SubsImmediateOp,
            AddsImmediateOp,
            MoveWideZeroOp,
            MoveWideNotOp,
            MoveWideKeepOp,
            MoveWideKeepShiftedOp,
            XorShiftedLslOp,
            XorShiftedLsrOp,
            AddressPCRelOp,
            AddressPCRelPageOp,
            MultiplyAddOp,
            MultiplySubOp,
            MultiplyOp,
            MultiplyNegOp,
            SignedMultiplyHighOp,
            SignedDivideOp,
            UnsignedDivideOp,
            ConditionalSetEqOp,
            ConditionalSetNeOp,
            ConditionalSetLtOp,
            ConditionalSetGeOp,
            ConditionalSetGtOp,
            ConditionalSetLeOp,
            ConditionalSetLoOp,
            ConditionalSetHsOp,
            ConditionalSetHiOp,
            ConditionalSetLsOp,
            ConditionalSelectEqOp,
            ConditionalSelectNeOp,
            ConditionalSelectLtOp,
            ConditionalSelectGeOp,
            ConditionalSelectGtOp,
            ConditionalSelectLeOp,
            ConditionalSelectLoOp,
            ConditionalSelectHsOp,
            ConditionalSelectHiOp,
            ConditionalSelectLsOp,
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
            LoadByteUnsignedUnscaledOp,
            LoadHalfwordUnsignedUnscaledOp,
            LoadWordUnsignedUnscaledOp,
            LoadDoublewordUnscaledOp,
            LoadByteSignedUnscaledOp,
            LoadHalfwordSignedUnscaledOp,
            LoadWordSignedUnscaledOp,
            StoreByteUnscaledOp,
            StoreHalfwordUnscaledOp,
            StoreWordUnscaledOp,
            StoreDoublewordUnscaledOp,
            LoadDoublewordRegOffsetOp,
            LoadDoublewordRegOffsetScaledOp,
            StoreDoublewordRegOffsetOp,
            StoreDoublewordRegOffsetScaledOp,
            LoadDoublewordPostIndexOp,
            LoadDoublewordPreIndexOp,
            StoreDoublewordPostIndexOp,
            StoreDoublewordPreIndexOp,
            LoadVector128Op,
            StoreVector128Op,
            LoadPairOp,
            StorePairOp,
            LoadPairPostIndexOp,
            LoadPairPreIndexOp,
            StorePairPostIndexOp,
            StorePairPreIndexOp,
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
            BranchGreaterThanOp,
            BranchLessEqOp,
            BranchHigherUnsignedOp,
            BranchLowerOrSameUnsignedOp,
            BranchMinusOp,
            BranchPlusOp,
            BranchOverflowSetOp,
            BranchOverflowClearOp,
            CompareBranchZeroOp,
            CompareBranchNonZeroOp,
            TestBitBranchZeroOp,
            TestBitBranchNonZeroOp,
            SupervisorOp,
            NoOperationOp,
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
                    class: Some(RegClass::GPR.id()),
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

/// Emit the deferred unconditional branch (`vbr`, finalized to `b` after
/// register allocation), forwarding any block arguments.
fn emit_uncond_branch(
    context: &tir::Context,
    dest: tir::BlockId,
    args: &[tir::ValueId],
) -> Box<dyn Operation> {
    Box::new(
        VirtualBranchOpBuilder::new(context)
            .dest_args(args.to_vec())
            .attr("dest", tir::attributes::AttributeValue::Block(dest))
            .build(),
    )
}

/// Emit the branch-if-nonzero fallback for a condition no branch rule fused:
/// `cmp cond, xzr` + `b.ne dest`.
fn emit_branch_nonzero(
    context: &tir::Context,
    condition: tir::ValueId,
    dest: tir::BlockId,
) -> Vec<Box<dyn Operation>> {
    vec![
        Box::new(
            CompareOpBuilder::new(context)
                .attr("rn", virt(condition.number(), RegClass::GPR.id()))
                .attr("rm", phys(&(RegClass::GPR.id(), XZR)))
                .build(),
        ),
        Box::new(
            BranchNotEqOpBuilder::new(context)
                .attr("imm", tir::attributes::AttributeValue::Block(dest))
                .build(),
        ),
    ]
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
            .attr("rn", phys(&(RegClass::GPR.id(), XZR)))
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
        let copy = mv(
            context,
            virt(fresh, RegClass::GPR.id()),
            virt(value.number(), RegClass::GPR.id()),
        );
        rewriter.insert_op_before(op, copy.as_ref()).map(|()| fresh)
    };
    let fresh_callee = callee_value
        .map(|value| detach(rewriter, value))
        .transpose()?;
    let mut fresh_args = Vec::with_capacity(args.len());
    for arg in &args {
        fresh_args.push(detach(rewriter, *arg)?);
    }

    let lr = (RegClass::GPR.id(), LR);
    let saved_lr = context
        .create_value(tir::builtin::IntegerType::new(context, 64), None)
        .id();
    let save = mv(
        context,
        virt(saved_lr.number(), RegClass::GPR.id()),
        phys(&lr),
    );
    rewriter.insert_op_before(op, save.as_ref())?;

    for (&fresh, &reg) in fresh_args.iter().zip(class.arguments.iter()) {
        let copy = mv(
            context,
            phys(&(RegClass::GPR.id(), reg)),
            virt(fresh, RegClass::GPR.id()),
        );
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
                .attr("callee_reg", virt(fresh, RegClass::GPR.id()))
                .attr("clobbers", caller_saved_clobbers())
                .build(),
        ),
    };

    rewriter.insert_op_before(op, virtual_call.as_ref())?;
    let restore = mv(
        context,
        phys(&lr),
        virt(saved_lr.number(), RegClass::GPR.id()),
    );

    let ret_reg = class.return_values[0];
    if context.get_value(result).ty() == UnitType::new(context) {
        rewriter.replace_op(op, restore.as_ref())?;
    } else {
        rewriter.insert_op_before(op, restore.as_ref())?;
        let copy = mv(
            context,
            virt(result.number(), RegClass::GPR.id()),
            phys(&(RegClass::GPR.id(), ret_reg)),
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
            .map(|&index| phys(&(RegClass::GPR.id(), index)))
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
        .with_axioms(include_str!("isel.axioms"))
        .with_branch_emitters(tir::backend::isel::BranchEmitters {
            uncond: emit_uncond_branch,
            cond_nonzero: emit_branch_nonzero,
        })
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
        .with_op_lowering(lower_calls)
}

/// The AArch64 stack pointer, reserved from allocation and used as the base for
/// spill slots after the frame has been reserved.
fn sp() -> tir::backend::liveness::PhysReg {
    (RegClass::GPRsp.id(), 31)
}

fn phys(reg: &tir::backend::liveness::PhysReg) -> tir::attributes::AttributeValue {
    tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Physical {
        class: reg.0,
        index: reg.1,
    })
}

fn virt(value: u32, class: tir::backend::regalloc::RegClassId) -> tir::attributes::AttributeValue {
    tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Virtual {
        id: value,
        class: Some(class),
    })
}

/// AArch64 register allocation target: the generated register file plus `str`/`ldr`
/// spill code and a `sub sp, sp, #frame` / `add sp, sp, #frame` prologue/epilogue.
pub struct Arm64RegAlloc;

impl tir::backend::regalloc::TargetRegAlloc for Arm64RegAlloc {
    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        register_info()
    }

    fn frame_align(&self) -> u32 {
        16
    }

    fn frame_register(&self) -> tir::backend::liveness::PhysReg {
        sp()
    }

    fn emit_spill_store(
        &self,
        context: &tir::Context,
        value: u32,
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
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
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
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

    fn emit_copy(
        &self,
        context: &tir::Context,
        class: tir::backend::regalloc::RegClassId,
        dst: u32,
        src: u32,
    ) -> Box<dyn Operation> {
        match class.name() {
            "GPR" => mv(
                context,
                virt(dst, RegClass::GPR.id()),
                virt(src, RegClass::GPR.id()),
            ),
            other => unimplemented!(
                "arm64 register copy for class {other} is not implemented (no float/vector move op)"
            ),
        }
    }

    fn emit_prologue(
        &self,
        context: &tir::Context,
        size: u32,
        saves: &[(tir::backend::liveness::PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        let sp = sp();
        let mut ops: Vec<Box<dyn Operation>> = vec![Box::new(
            SubImmediateOpBuilder::new(context)
                .attr("rd", phys(&sp))
                .attr("rn", phys(&sp))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        )];
        for (reg, offset) in saves {
            ops.push(Box::new(
                StoreDoublewordOpBuilder::new(context)
                    .attr("rt", phys(reg))
                    .attr("rn", phys(&sp))
                    .attr("imm", tir::attributes::AttributeValue::Int(*offset))
                    .build(),
            ));
        }
        ops
    }

    fn emit_epilogue(
        &self,
        context: &tir::Context,
        size: u32,
        saves: &[(tir::backend::liveness::PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        let sp = sp();
        let mut ops: Vec<Box<dyn Operation>> = Vec::new();
        for (reg, offset) in saves {
            ops.push(Box::new(
                LoadDoublewordOpBuilder::new(context)
                    .attr("rt", phys(reg))
                    .attr("rn", phys(&sp))
                    .attr("imm", tir::attributes::AttributeValue::Int(*offset))
                    .build(),
            ));
        }
        ops.push(Box::new(
            AddImmediateOpBuilder::new(context)
                .attr("rd", phys(&sp))
                .attr("rn", phys(&sp))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        ));
        ops
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
        context.register_reg_classes(register_info().classes);
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
        vec![obj::lower_constant, obj::lower_addr_of]
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

    fn instruction_decoder(&self) -> Option<tir::backend::InstructionDecoder> {
        Some(decode_instruction)
    }

    fn hardwired_zero_registers(&self) -> &'static [(&'static str, u16)] {
        hardwired_zero_registers()
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

    use crate::{Arm64Dialect, RegClass, create_isel_pass, create_regalloc_pass};

    #[test]
    #[ignore = "run with `cargo xtask axioms`"]
    fn committed_isel_axioms_are_fresh() {
        let context = Context::with_default_dialects();
        let discovered = tir::backend::isel::discover_axioms(&crate::get_isel_rules(
            &context,
            crate::Feature::ALL,
        ));
        assert_eq!(
            include_str!("isel.axioms"),
            tir::backend::isel::render_axioms_file(&discovered),
            "isel.axioms is stale; run `cargo run -p tir-tools --bin tir -- axioms --write`"
        );
    }

    #[test]
    fn arm64_builtin_cond_br_lowers_through_branch_emitters() {
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
        fb.insert(add);
        // A bare i1 condition (a block argument): no branch rule can fuse it, so
        // selection falls back to `cmp cond, xzr` + `b.ne t` plus the deferred
        // `vbr f`.
        fb.insert(ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build());

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
        assert_eq!(body, vec!["add", "cmp", "b.ne", "vbr", "symbol_end"]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        assert!(
            !buf.contains("builtin"),
            "no builtin ops should remain:\n{buf}"
        );
    }

    #[test]
    fn arm64_cmpi_cond_br_fuses_into_cmp_and_bcond() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let i1 = IntegerType::new(&context, 1);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", UnitType::new(&context), Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a_id, b_id) = (args[0].id(), args[1].id());

        let t = context.create_block(vec![]);
        let f = context.create_block(vec![]);

        let mut fb = IRBuilder::new(func.body());
        let cmp = tir::builtin::CmpIOpBuilder::new(&context)
            .lhs(a_id)
            .rhs(b_id)
            .predicate("slt")
            .result_type(i1)
            .build();
        let cmp_r = cmp.result();
        fb.insert(cmp);
        fb.insert(ops::cond_br(&context, cmp_r, vec![], vec![], t.id(), f.id()).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should select the flag-mediated branch pair");

        // The signed compare-and-branch selects through the TMDL-derived
        // `cmp+b.lt` rule: the definer sets PSTATE, `b.lt` reads it, and the
        // `cmpi` op is consumed.
        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(body, vec!["cmp", "b.lt", "vbr", "symbol_end"]);
    }

    fn phys_of(
        op: &std::sync::Arc<tir::OpInstance>,
        name: &str,
    ) -> Option<tir::backend::liveness::PhysReg> {
        use tir::attributes::{AttributeValue, RegisterAttr};
        op.attributes
            .iter()
            .find(|a| a.name == name)
            .and_then(|a| match &a.value {
                AttributeValue::Register(RegisterAttr::Physical { class, index }) => {
                    Some((*class, *index))
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

        assert_eq!(phys_of(&add_op, "rn"), Some((RegClass::GPR.id(), 0)));
        assert_eq!(phys_of(&add_op, "rm"), Some((RegClass::GPR.id(), 1)));
        assert_eq!(phys_of(&add_op, "rd"), Some((RegClass::GPR.id(), 0)));

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
                    group_width: 1,
                }],
            }
        }
        fn frame_register(&self) -> tir::backend::liveness::PhysReg {
            self.0.frame_register()
        }
        fn emit_spill_store(
            &self,
            c: &tir::Context,
            v: u32,
            class: tir::backend::regalloc::RegClassId,
            f: &tir::backend::liveness::PhysReg,
            o: i64,
        ) -> Box<dyn Operation> {
            self.0.emit_spill_store(c, v, class, f, o)
        }
        fn emit_spill_reload(
            &self,
            c: &tir::Context,
            v: u32,
            class: tir::backend::regalloc::RegClassId,
            f: &tir::backend::liveness::PhysReg,
            o: i64,
        ) -> Box<dyn Operation> {
            self.0.emit_spill_reload(c, v, class, f, o)
        }
        fn emit_prologue(
            &self,
            c: &tir::Context,
            s: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            self.0.emit_prologue(c, s, saves)
        }
        fn emit_epilogue(
            &self,
            c: &tir::Context,
            s: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            self.0.emit_epilogue(c, s, saves)
        }
    }

    #[test]
    fn arm64_regalloc_spills_under_high_register_pressure() {
        use crate::{AddOpBuilder, VirtualReturnOpBuilder, virt};
        use tir::backend::regalloc::TargetRegAlloc;

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

        // Tag every vreg with the tiny target's own `GPR` class, whose 5-register
        // allocation order is what forces spilling. A `RegClassId` is an absolute
        // handle into a specific register table, so this must be the same class the
        // allocator reads from `TinyArm64::register_info`, not the full arm64 `GPR`.
        let gpr = TinyArm64(crate::Arm64RegAlloc)
            .register_info()
            .class("GPR")
            .unwrap();
        let mut bb = IRBuilder::new(context.get_block(block.id()));
        let mut producers = Vec::new();
        for _ in 0..8 {
            let v = context.create_value(i64, None).id().number();
            bb.insert(
                AddOpBuilder::new(&context)
                    .attr("rd", virt(v, gpr))
                    .attr("rn", virt(a, gpr))
                    .attr("rm", virt(a, gpr))
                    .build(),
            );
            producers.push(v);
        }
        let mut acc = producers[0];
        for &p in &producers[1..] {
            let s = context.create_value(i64, None).id().number();
            bb.insert(
                AddOpBuilder::new(&context)
                    .attr("rd", virt(s, gpr))
                    .attr("rn", virt(acc, gpr))
                    .attr("rm", virt(p, gpr))
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
            "the frame prologue (sub sp) should lead the block, got {names:?}"
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
        let gpr = |i: u16| phys(&(RegClass::GPR.id(), i));
        let gprsp = |i: u16| phys(&(RegClass::GPRsp.id(), i));
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
    fn decoder_round_trips_golden_words() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        // The encoder is golden-verified against llvm-mc (see the test above), so
        // `decode(word) -> op` is correct iff re-encoding that op reproduces the
        // original word. This exercises operand extraction (registers, immediates,
        // split fields) and fixed-opcode matching across the instruction classes
        // the benchmark ELFs execute, plus the newly-added `svc`.
        let encoders = crate::get_instruction_encoders();
        let reencode = |id: tir::OpId| -> u32 {
            let inst = context.get_op(id);
            let enc = encoders[inst.name](&inst)
                .unwrap_or_else(|| panic!("'{}' failed to re-encode", inst.name));
            u32::from_le_bytes(enc.bytes.try_into().unwrap())
        };

        let cases: &[(u32, &str)] = &[
            (0x8B020020, "add x0, x1, x2"),
            (0x9AC22020, "lslv x0, x1, x2"),
            (0xEB02003F, "cmp x1, x2"),
            (0xF9400020, "ldr x0, [x1]"),
            (0xF9000062, "str x2, [x3]"),
            (0x54000080, "b.eq +16"),
            (0x14000003, "b +12"),
            (0x94000002, "bl +8"),
            (0xD65F03C0, "ret"),
            (0xD2800540, "movz x0, #42"),
            (0xD4000001, "svc #0"),
            (0xF1000400, "subs x0, x0, #1"),
            (0xB100094A, "adds x10, x10, #2"),
            (0xF8616801, "ldr x1, [x0, x1]"),
            (0xF860790D, "ldr x13, [x8, x0, lsl #3]"),
            (0xF82D696E, "str x14, [x11, x13]"),
            (0xF82B790D, "str x13, [x8, x11, lsl #3]"),
            (0xF81F0FFE, "str x30, [sp, #-16]!"),
            (0xF84107FE, "ldr x30, [sp], #16"),
            (0xF802050A, "str x10, [x8], #32"),
            (0xF8408C20, "ldr x0, [x1, #8]!"),
            (0xA9BF7BFD, "stp x29, x30, [sp, #-16]!"),
            (0xA8C17BFD, "ldp x29, x30, [sp], #16"),
            (0xD503201F, "nop"),
            (0xF2BBD5A9, "movk x9, #0xdead, lsl #16"),
            (0xF2C24689, "movk x9, #0x1234, lsl #32"),
            (0xF2F4B4A9, "movk x9, #0xa5a5, lsl #48"),
            (0xCA493129, "eor x9, x9, x9, lsr #12"),
            (0xCA096529, "eor x9, x9, x9, lsl #25"),
            (0xD37AE5AD, "lsl x13, x13, #6"),
            (0x52807D02, "movz w2, #1000"),
            (0x12001C00, "and w0, w0, #0xff"),
            (0x92401C41, "and x1, x2, #0xff"),
            (0x92402C83, "and x3, x4, #0xfff"),
            (0x1E612802, "fadd d2, d0, d1"),
            (0x1E600843, "fmul d3, d2, d0"),
            (0x1E601000, "fmov d0, #2.0"),
            (0x9E660064, "fmov x4, d3"),
            (0x4E080C00, "dup v0.2d, x0"),
            (0x4EE18402, "add v2.2d, v0.2d, v1.2d"),
            (0x4EA18403, "add v3.4s, v0.4s, v1.4s"),
            (0x4E61D404, "fadd v4.2d, v0.2d, v1.2d"),
            (0x6E61DC05, "fmul v5.2d, v0.2d, v1.2d"),
            (0x3DC00008, "ldr q8, [x0]"),
            (0x3D800009, "str q9, [x0]"),
            (0x4F000426, "movi v6.4s, #1"),
            (0x6F00F407, "fmov v7.2d, #2.0"),
        ];
        for &(w, asm) in cases {
            let id = crate::decode_instruction(&context, w)
                .unwrap_or_else(|| panic!("failed to decode {asm} ({w:#010x})"));
            assert_eq!(reencode(id), w, "round-trip mismatch for {asm}");
        }
    }

    #[test]
    fn extended_encoders_match_isa_golden_words() {
        use crate::phys;
        use tir::attributes::AttributeValue;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let encoders = crate::get_instruction_encoders();
        let gpr = |i: u16| phys(&(RegClass::GPR.id(), i));
        let gprsp = |i: u16| phys(&(RegClass::GPRsp.id(), i));
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
        let bic = crate::BicOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(bic.id()), 0x8A220020, "bic x0, x1, x2");

        let orn = crate::OrnOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(orn.id()), 0xAA220020, "orn x0, x1, x2");

        let eon = crate::EonOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(eon.id()), 0xCA220020, "eon x0, x1, x2");

        let rorv = crate::RotateRightVariableOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(rorv.id()), 0x9AC22C20, "rorv x0, x1, x2");

        let sdiv = crate::SignedDivideOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(sdiv.id()), 0x9AC20C20, "sdiv x0, x1, x2");

        let udiv = crate::UnsignedDivideOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(udiv.id()), 0x9AC20820, "udiv x0, x1, x2");

        let cmn = crate::CompareNegativeOpBuilder::new(&context)
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(cmn.id()), 0xAB02003F, "cmn x1, x2");

        let tst = crate::TestOpBuilder::new(&context)
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(tst.id()), 0xEA02003F, "tst x1, x2");

        let cmp_imm = crate::CompareImmediateOpBuilder::new(&context)
            .attr("rn", gprsp(1))
            .attr("imm", AttributeValue::Int(5))
            .build();
        assert_eq!(word(cmp_imm.id()), 0xF100143F, "cmp x1, #5");

        let cmn_imm = crate::CompareNegImmediateOpBuilder::new(&context)
            .attr("rn", gprsp(1))
            .attr("imm", AttributeValue::Int(5))
            .build();
        assert_eq!(word(cmn_imm.id()), 0xB100143F, "cmn x1, #5");

        let movn = crate::MoveWideNotOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("imm", AttributeValue::Int(42))
            .build();
        assert_eq!(word(movn.id()), 0x92800540, "movn x0, #42");

        let movk = crate::MoveWideKeepOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("imm", AttributeValue::Int(42))
            .build();
        assert_eq!(word(movk.id()), 0xF2800540, "movk x0, #42");

        let madd = crate::MultiplyAddOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .attr("ra", gpr(3))
            .build();
        assert_eq!(word(madd.id()), 0x9B020C20, "madd x0, x1, x2, x3");

        let msub = crate::MultiplySubOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .attr("ra", gpr(3))
            .build();
        assert_eq!(word(msub.id()), 0x9B028C20, "msub x0, x1, x2, x3");

        let mul = crate::MultiplyOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(mul.id()), 0x9B027C20, "mul x0, x1, x2");

        let mneg = crate::MultiplyNegOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(mneg.id()), 0x9B02FC20, "mneg x0, x1, x2");

        let smulh = crate::SignedMultiplyHighOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(smulh.id()), 0x9B427C20, "smulh x0, x1, x2");

        let lsr = crate::LogicalShiftRightImmOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(lsr.id()), 0xD344FC20, "lsr x0, x1, #4");

        let asr = crate::ArithmeticShiftRightImmOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(asr.id()), 0x9344FC20, "asr x0, x1, #4");

        let sxtb = crate::SignExtendByteOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .build();
        assert_eq!(word(sxtb.id()), 0x93401C20, "sxtb x0, w1");

        let sxth = crate::SignExtendHalfwordOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .build();
        assert_eq!(word(sxth.id()), 0x93403C20, "sxth x0, w1");

        let sxtw = crate::SignExtendWordOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .build();
        assert_eq!(word(sxtw.id()), 0x93407C20, "sxtw x0, w1");

        let adr = crate::AddressPCRelOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("imm", AttributeValue::Int(16))
            .build();
        assert_eq!(word(adr.id()), 0x10000080, "adr x0, #16");

        let adrp = crate::AddressPCRelPageOpBuilder::new(&context)
            .attr("rd", gpr(1))
            .attr("imm", AttributeValue::Int(5))
            .build();
        assert_eq!(word(adrp.id()), 0xB0000021, "adrp x1, #0x5000");

        let cset = crate::ConditionalSetEqOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .build();
        assert_eq!(word(cset.id()), 0x9A9F17E0, "cset x0, eq");

        let csel = crate::ConditionalSelectEqOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rn", gpr(1))
            .attr("rm", gpr(2))
            .build();
        assert_eq!(word(csel.id()), 0x9A820020, "csel x0, x1, x2, eq");

        // Branch immediates hold word offsets (the pc-relative byte delta >> 2).
        let bgt = crate::BranchGreaterThanOpBuilder::new(&context)
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(bgt.id()), 0x5400008C, "b.gt +16");

        let cbz = crate::CompareBranchZeroOpBuilder::new(&context)
            .attr("rt", gpr(1))
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(cbz.id()), 0xB4000081, "cbz x1, +16");

        let cbnz = crate::CompareBranchNonZeroOpBuilder::new(&context)
            .attr("rt", gpr(1))
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(cbnz.id()), 0xB5000081, "cbnz x1, +16");

        let tbz = crate::TestBitBranchZeroOpBuilder::new(&context)
            .attr("rt", gpr(1))
            .attr("bit", AttributeValue::Int(3))
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(tbz.id()), 0x36180081, "tbz x1, #3, +16");

        // The bit number's high bit lands in word bit 31 (b5).
        let tbz_hi = crate::TestBitBranchZeroOpBuilder::new(&context)
            .attr("rt", gpr(1))
            .attr("bit", AttributeValue::Int(35))
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(tbz_hi.id()), 0xB6180081, "tbz x1, #35, +16");

        let tbnz = crate::TestBitBranchNonZeroOpBuilder::new(&context)
            .attr("rt", gpr(1))
            .attr("bit", AttributeValue::Int(3))
            .attr("imm", AttributeValue::Int(4))
            .build();
        assert_eq!(word(tbnz.id()), 0x37180081, "tbnz x1, #3, +16");

        let ldur = crate::LoadDoublewordUnscaledOpBuilder::new(&context)
            .attr("rt", gpr(0))
            .attr("rn", gprsp(1))
            .attr("imm", AttributeValue::Int(-8))
            .build();
        assert_eq!(word(ldur.id()), 0xF85F8020, "ldur x0, [x1, #-8]");

        let stur = crate::StoreDoublewordUnscaledOpBuilder::new(&context)
            .attr("rt", gpr(2))
            .attr("rn", gprsp(3))
            .attr("imm", AttributeValue::Int(-8))
            .build();
        assert_eq!(word(stur.id()), 0xF81F8062, "stur x2, [x3, #-8]");

        let ldursw = crate::LoadWordSignedUnscaledOpBuilder::new(&context)
            .attr("rt", gpr(0))
            .attr("rn", gprsp(1))
            .attr("imm", AttributeValue::Int(-4))
            .build();
        assert_eq!(word(ldursw.id()), 0xB89FC020, "ldursw x0, [x1, #-4]");

        let ldp = crate::LoadPairOpBuilder::new(&context)
            .attr("rt", gpr(0))
            .attr("rt2", gpr(1))
            .attr("rn", gprsp(2))
            .attr("imm", AttributeValue::Int(16))
            .build();
        assert_eq!(word(ldp.id()), 0xA9410440, "ldp x0, x1, [x2, #16]");

        let stp = crate::StorePairOpBuilder::new(&context)
            .attr("rt", gpr(0))
            .attr("rt2", gpr(1))
            .attr("rn", gprsp(2))
            .attr("imm", AttributeValue::Int(16))
            .build();
        assert_eq!(word(stp.id()), 0xA9010440, "stp x0, x1, [x2, #16]");
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
        // The lr save lives in a callee-saved register (x19..x28 per AAPCS64),
        // preserved by the prologue right after the frame is reserved and
        // restored by the epilogue before the frame is released.
        assert_eq!(
            names,
            vec![
                "sub_imm",
                "store_doubleword",
                "orr",
                "orr",
                "orr",
                "bl",
                "orr",
                "orr",
                "load_doubleword",
                "add_imm",
                "ret",
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
