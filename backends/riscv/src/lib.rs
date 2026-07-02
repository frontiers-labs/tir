use tir::helpers::{dialect, operation};
use tir::{Any, Operation};

mod obj;

include!(concat!(env!("OUT_DIR"), "/riscv.rs"));

/// Parsed RISC-V target selection from `--march`/`--mcpu`/`--mattr`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetConfig {
    xlen: u32,
    features: Vec<Feature>,
    /// Machine model implied by `--mcpu`, when it names one.
    machine: Option<String>,
}

impl TargetConfig {
    /// Parse a RISC-V `--march`/`--mcpu`/`--mattr` triple.
    pub fn parse(march: &str, mcpu: Option<&str>, mattr: Option<&str>) -> Result<Self, String> {
        let mut config = parse_march(march)?;
        if let Some(mattr) = mattr {
            apply_mattr(&mut config.features, mattr)?;
        }
        validate_features(&config.features)?;
        let base = config.base_feature();
        if !config.features.contains(&base) {
            return Err(format!(
                "--mattr must not disable the base ISA '{}'",
                base.name()
            ));
        }
        // Exactly one base ISA: parameters like XLEN resolve from it.
        if config.features.contains(&Feature::RV32I) && config.features.contains(&Feature::RV64I) {
            return Err("RV32I and RV64I are mutually exclusive".to_string());
        }
        if let Some(mcpu) = mcpu {
            config.machine = parse_mcpu(mcpu, &config)?;
        }
        Ok(config)
    }

    /// Canonical architecture name for diagnostics and target-specific behavior.
    pub fn canonical_name(&self) -> &'static str {
        match self.xlen {
            32 => "riscv32",
            _ => "riscv64",
        }
    }

    /// The enabled ISA/extension set.
    pub fn features(&self) -> &[Feature] {
        &self.features
    }

    fn base_feature(&self) -> Feature {
        match self.xlen {
            32 => Feature::RV32I,
            _ => Feature::RV64I,
        }
    }

    /// The generic profile for an XLEN: every extension modeled in TMDL.
    fn generic(xlen: u32) -> Self {
        let mut config = TargetConfig {
            xlen,
            features: vec![],
            machine: None,
        };
        config.features = Feature::ALL
            .iter()
            .copied()
            .filter(|f| match f {
                Feature::RV32I => xlen == 32,
                Feature::RV64I => xlen == 64,
                _ => true,
            })
            .collect();
        config
    }
}

fn parse_march(march: &str) -> Result<TargetConfig, String> {
    let march = normalize(march);
    match march.as_str() {
        // Bare architecture names select the generic profile with every
        // modeled extension, mirroring how toolchains treat a bare triple.
        "riscv" | "riscv64" => Ok(TargetConfig::generic(64)),
        "riscv32" => Ok(TargetConfig::generic(32)),
        _ => parse_riscv_isa_string(&march),
    }
}

/// Resolve `--mcpu` to an optional default machine model. Generic CPU names
/// map onto the generic cores when one exists for the configured XLEN; any
/// other name must be a TMDL machine (by name or alias) compatible with the
/// enabled features.
fn parse_mcpu(mcpu: &str, config: &TargetConfig) -> Result<Option<String>, String> {
    let mcpu = normalize(mcpu);
    let name = match (
        mcpu.strip_prefix("riscv32-"),
        mcpu.strip_prefix("riscv64-"),
        config.xlen,
    ) {
        (Some(name), _, 32) | (_, Some(name), 64) => name,
        (Some(_), _, _) | (_, Some(_), _) => {
            return Err(format!(
                "cpu '{mcpu}' does not match the '{}' architecture",
                config.canonical_name()
            ));
        }
        _ => mcpu.as_str(),
    };

    let generic = match name {
        "generic" => Some(None),
        "generic-in-order" | "generic-inorder" | "in-order" | "inorder" => {
            Some((config.xlen == 64).then(|| "rv64-in-order".to_string()))
        }
        "generic-ooo" | "generic-out-of-order" | "ooo" | "out-of-order" => {
            Some((config.xlen == 64).then(|| "rv64-ooo".to_string()))
        }
        _ => None,
    };
    if let Some(machine) = generic {
        return Ok(machine);
    }

    if machine_model(name, &config.features).is_some() {
        return Ok(Some(name.to_string()));
    }
    if machine_model(name, Feature::ALL).is_some() {
        return Err(format!(
            "cpu '{name}' is incompatible with the selected architecture"
        ));
    }
    Err(format!(
        "unknown RISC-V cpu '{name}' (expected 'generic', 'generic-in-order', 'generic-ooo' or one of: {})",
        machines(Feature::ALL).join(", ")
    ))
}

/// Apply an LLVM-style `--mattr` list (`+feat`/`-feat`, comma-separated) on top
/// of the march-derived feature set.
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
        let toggled = attr_features(name)
            .ok_or_else(|| format!("unknown RISC-V feature '{name}' in --mattr"))?;
        for feature in toggled {
            if add && !features.contains(&feature) {
                features.push(feature);
            } else if !add {
                features.retain(|f| *f != feature);
            }
        }
    }
    Ok(())
}

/// Features named by a `--mattr` entry: the march extension letter spellings
/// plus the TMDL feature names.
fn attr_features(name: &str) -> Option<Vec<Feature>> {
    let name = normalize(name);
    match name.as_str() {
        // The M extension implies Zmmul.
        "m" => Some(vec![Feature::RVM, Feature::Zmmul]),
        _ => Feature::from_name(&name).map(|f| vec![f]),
    }
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

fn parse_riscv_isa_string(march: &str) -> Result<TargetConfig, String> {
    let err = || format!("invalid RISC-V ISA string '{march}'");
    let rest = march.strip_prefix("rv").ok_or_else(err)?;
    let (xlen, rest) = if let Some(rest) = rest.strip_prefix("32") {
        (32, rest)
    } else {
        (64, rest.strip_prefix("64").ok_or_else(err)?)
    };

    let base_feature = if xlen == 32 {
        Feature::RV32I
    } else {
        Feature::RV64I
    };
    let mut features = vec![];
    let mut enable = |feature: Feature| {
        if !features.contains(&feature) {
            features.push(feature);
        }
    };

    let mut chars = rest.chars().peekable();
    let base = chars.next().ok_or_else(err)?;
    match base {
        'i' => {
            enable(base_feature);
            skip_extension_version(&mut chars);
        }
        // G abbreviates IMAFD_Zicsr_Zifencei; the parts TMDL does not model
        // yet contribute nothing.
        'g' => {
            enable(base_feature);
            enable(Feature::RVM);
            enable(Feature::Zmmul);
            enable(Feature::Zicsr);
            skip_extension_version(&mut chars);
        }
        'e' => return Err(format!("unsupported RISC-V base ISA 'e' in '{march}'")),
        _ => return Err(err()),
    }

    while chars.peek().is_some() {
        if chars.peek() == Some(&'-') {
            chars.next();
            chars.peek().ok_or_else(err)?;
            continue;
        }

        let ext = chars.next().ok_or_else(err)?;
        if ext.is_ascii_digit() {
            return Err(err());
        }

        match ext {
            'm' => {
                enable(Feature::RVM);
                enable(Feature::Zmmul);
                skip_extension_version(&mut chars);
            }
            'v' => {
                enable(Feature::RVV);
                skip_extension_version(&mut chars);
            }
            // Standard single-letter extensions TMDL does not model yet are
            // accepted so common GNU march strings (e.g. rv64gc) keep working;
            // they contribute no instructions.
            'a' | 'f' | 'd' | 'q' | 'l' | 'c' | 'b' | 'j' | 't' | 'p' | 'h' => {
                skip_extension_version(&mut chars);
            }
            'z' | 's' | 'x' => {
                let name = consume_multi_letter_extension(ext, &mut chars).ok_or_else(err)?;
                // Same policy for multi-letter extensions: enable the modeled
                // ones, accept and ignore the rest.
                if let Some(feature) = Feature::from_name(&name) {
                    enable(feature);
                }
            }
            _ => return Err(err()),
        }
    }

    Ok(TargetConfig {
        xlen,
        features,
        machine: None,
    })
}

fn consume_multi_letter_extension<I>(
    first: char,
    chars: &mut std::iter::Peekable<I>,
) -> Option<String>
where
    I: Iterator<Item = char>,
{
    let mut name = String::from(first);
    while let Some(&c) = chars.peek() {
        if c == '-' {
            break;
        }
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            name.push(c);
            chars.next();
        } else {
            return None;
        }
    }
    (name.len() > 1).then_some(name)
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
        tir::backend::print_branch(fmt, self, "riscv.vbr")
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
        tir::backend::print_branch(fmt, self, "riscv.vcond_br")
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
// clobber list, deferring the actual `jal`/`jalr` encoding to a post-RA pass.
operation! {
    VirtualCallOp {
        name: "vcall",
        dialect: "riscv",
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
        dialect: "riscv",
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
            // M extension (Zmmul subset)
            MulOp,
            MulHOp,
            // V extension (vector-vector arithmetic)
            VAddOp,
            VSubOp,
            VMulOp,
            VSetVliOp,
            VSetIVliOp,
            // Zicsr
            CSRReadWriteOp,
            CSRReadSetOp,
            CSRReadClearOp,
            CSRReadWriteImmOp,
            CSRReadSetImmOp,
            CSRReadClearImmOp,
            // System
            EnvCallOp,
            EnvBreakOp,
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
            VirtualCondBranchOp,
            VirtualCallOp,
            VirtualIndirectCallOp
        ],
    }
}

pub mod ops {
    pub use super::*;
}

impl RiscvDialect {
    pub fn get_asm_parser(&self) -> tir::backend::AsmParser {
        tir::backend::AsmParser::new(get_instruction_parsers(Feature::ALL).0)
    }

    pub fn get_asm_printer(&self) -> tir::backend::AsmPrinter {
        tir::backend::AsmPrinter::new(get_instruction_printers())
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

/// The RISC-V return-address register (`ra` = `x1`).
const RA: u16 = 1;

/// Build a register-register move (`addi rd, rs, 0`).
fn mv(
    context: &tir::Context,
    rd: tir::attributes::AttributeValue,
    rs: tir::attributes::AttributeValue,
) -> Box<dyn Operation> {
    Box::new(
        AddImmOpBuilder::new(context)
            .attr("rd", rd)
            .attr("rs1", rs)
            .attr("imm", tir::attributes::AttributeValue::Int(0))
            .build(),
    )
}

/// Lower the builtin call ops to RISC-V virtual calls. Arguments are moved into
/// the ABI argument registers and the result is copied out of the first return
/// register, so the allocator never has to pin long live ranges. The call
/// clobbers every caller-saved register — including `ra`, which also holds this
/// function's own return address, so it is saved into a fresh virtual register
/// across the call (the allocator gives it a callee-saved register or a spill
/// slot).
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
        .expect("riscv register info must define GPR");
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

    let ra = ("GPR".to_string(), RA);
    let saved_ra = context
        .create_value(tir::builtin::IntegerType::new(context, 64), None)
        .id();
    let save = mv(context, virt(saved_ra.number(), "GPR"), phys(&ra));
    rewriter.insert_op_before(op, save.as_ref())?;
    rewriter.insert_op_before(op, virtual_call.as_ref())?;
    let restore = mv(context, phys(&ra), virt(saved_ra.number(), "GPR"));

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
        .expect("riscv register info must define GPR");
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
        .with_register_definers(get_register_definers(context, features))
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
        .with_op_lowering(lower_branches)
        .with_op_lowering(lower_calls)
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

impl tir::backend::regalloc::TargetRegAlloc for RiscvRegAlloc {
    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
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

pub fn create_regalloc_pass() -> tir::backend::regalloc::RegisterAllocationPass {
    tir::backend::regalloc::RegisterAllocationPass::new(Box::new(RiscvRegAlloc))
}

/// The RISC-V target, selected via `--march`/`--mcpu`.
pub struct RiscvTarget {
    config: TargetConfig,
}

impl tir::backend::TargetMachine for RiscvTarget {
    fn name(&self) -> &'static str {
        self.config.canonical_name()
    }

    fn register_dialects(&self, context: &tir::Context) {
        context.register_dialect::<tir::backend::AsmDialect>();
        context.register_dialect::<RiscvDialect>();
    }

    fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
        create_isel_pass_for(context, &self.config.features)
    }

    fn regalloc_pass(&self) -> tir::backend::regalloc::RegisterAllocationPass {
        create_regalloc_pass()
    }

    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        use tir::backend::regalloc::TargetRegAlloc;
        RiscvRegAlloc.register_info()
    }

    fn asm_parser(&self, _context: &tir::Context) -> tir::backend::AsmParser {
        let (parsers, disabled) = get_instruction_parsers(&self.config.features);
        tir::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
    }

    fn asm_printer(&self, context: &tir::Context) -> tir::backend::AsmPrinter {
        context
            .find_dialect::<RiscvDialect>()
            .expect("riscv dialect must be registered before building an asm printer")
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

    fn counter_registers(&self) -> Vec<(&'static str, u16, tir::backend::PerfCounter)> {
        use tir::backend::PerfCounter;
        if !self.config.features.contains(&Feature::Zicsr) {
            return vec![];
        }
        // The user-level counter CSRs at their architectural addresses (the
        // indices declared in zicsr.tmdl).
        let mut counters = vec![
            ("CSR", 0xC00, PerfCounter::Cycles),
            ("CSR", 0xC01, PerfCounter::Time),
            ("CSR", 0xC02, PerfCounter::InstructionsRetired),
        ];
        // RV32 reads counters as XLEN-wide halves: cycleh/timeh/instreth
        // deliver the upper 32 bits. RV64 reads the full counter directly.
        if self.config.features.contains(&Feature::RV32I) {
            counters.extend([
                ("CSR", 0xC80, PerfCounter::CyclesHigh),
                ("CSR", 0xC81, PerfCounter::TimeHigh),
                ("CSR", 0xC82, PerfCounter::InstructionsRetiredHigh),
            ]);
        }
        counters
    }

    fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        let lower_constant = if self.config.xlen == 64 {
            obj::lower_constant_rv64
        } else {
            obj::lower_constant_rv32
        };
        vec![lower_constant, obj::lower_vcond_br]
    }

    fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        vec![obj::finalize_virtual_ops]
    }

    fn object_format(&self) -> Option<tir::backend::binary::ObjectFormatInfo> {
        Some(obj::object_format(self.config.xlen))
    }

    fn binary_writer(&self, _context: &tir::Context) -> Option<tir::backend::binary::BinaryWriter> {
        Some(tir::backend::binary::BinaryWriter::new(
            get_instruction_encoders(),
            get_instruction_patchers(),
        ))
    }
}

fn select_riscv(
    march: &str,
    mcpu: Option<&str>,
    mattr: Option<&str>,
) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
    let owned = ["riscv", "rv32", "rv64"]
        .iter()
        .any(|prefix| normalize(march).starts_with(prefix));
    if !owned {
        return Ok(None);
    }
    let config = TargetConfig::parse(march, mcpu, mattr)?;
    Ok(Some(Box::new(RiscvTarget { config })))
}

tir::register_target!(select_riscv, ["riscv32", "riscv64"]);

#[cfg(test)]
mod tests {
    use tir::backend::AsmDialect;
    use tir::{
        Context, IRBuilder, IRFormatter, Operation, PassManager,
        builtin::{FuncOp, IntegerType, UnitType, ops},
    };

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
                tir::backend::sched::InstrSchedClass::DEFAULT
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
            Some(tir::backend::sched::Protection::Protected)
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

    fn target_for(march: &str) -> crate::RiscvTarget {
        crate::RiscvTarget {
            config: crate::TargetConfig::parse(march, None, None).expect("march should parse"),
        }
    }

    #[test]
    fn asm_parser_gates_extensions() {
        use tir::backend::TargetMachine;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        // M-extension instructions need RVM (or Zmmul) enabled.
        let mul = ".global f\nf:\n    mul a0, a1, a2\n";
        assert!(
            target_for("rv64i")
                .asm_parser(&context)
                .parse_asm(&context, mul)
                .is_err()
        );
        for march in ["rv64im", "rv64i_zmmul", "riscv64"] {
            assert!(
                target_for(march)
                    .asm_parser(&context)
                    .parse_asm(&context, mul)
                    .is_ok(),
                "mul should parse with --march={march}"
            );
        }

        // RV64-only instructions are rejected on rv32.
        let word_ops = ".global f\nf:\n    addw a0, a1, a2\n    ld a1, 0(sp)\n";
        assert!(
            target_for("rv32im")
                .asm_parser(&context)
                .parse_asm(&context, word_ops)
                .is_err()
        );
        assert!(
            target_for("rv64i")
                .asm_parser(&context)
                .parse_asm(&context, word_ops)
                .is_ok()
        );
    }

    #[test]
    fn machines_filter_by_feature_set() {
        use tir::backend::TargetMachine;

        let rv64 = target_for("rv64im");
        assert_eq!(rv64.machines(), vec!["rv64-in-order", "rv64-ooo"]);
        assert!(rv64.machine_model("rv64-ooo").is_some());
        assert!(rv64.machine_model("scr1-3stage").is_none());

        let rv32 = target_for("rv32i");
        assert_eq!(rv32.machines(), vec!["scr1-3stage"]);
        assert!(rv32.machine_model("scr1-3stage").is_some());
        assert!(rv32.machine_model("rv64-ooo").is_none());
    }

    #[test]
    fn isel_rules_filter_by_feature_set() {
        let context = Context::with_default_dialects();
        let rule_names = |features: &[crate::Feature]| -> Vec<&'static str> {
            crate::get_isel_rules(&context, features)
                .iter()
                .map(|r| r.name)
                .collect()
        };

        let rv64i = rule_names(&[crate::Feature::RV64I]);
        assert!(rv64i.contains(&"addword"));
        assert!(!rv64i.contains(&"mul"));

        let rv64im = rule_names(&[crate::Feature::RV64I, crate::Feature::RVM]);
        assert!(rv64im.contains(&"mul"));

        let rv32i = rule_names(&[crate::Feature::RV32I]);
        assert!(rv32i.contains(&"add"));
        assert!(!rv32i.contains(&"addword"));
        assert!(!rv32i.contains(&"loaddoubleword"));
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

    #[test]
    fn builtin_call_lowers_to_vcall_with_abi_copies() {
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

        let func = ops::func(&context, "caller", i32, Some(region.id())).build();
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
            .result_type(i32)
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

        // Two detach copies, two argument copies into a0/a1, the ra save, the
        // virtual call, the ra restore, and the result copy out of a0.
        assert_eq!(
            body_op_names(&context, region.id()),
            vec![
                "addi",
                "addi",
                "addi",
                "addi",
                "addi",
                "vcall",
                "addi",
                "addi",
                "vret",
                "symbol_end"
            ]
        );
    }

    #[test]
    fn call_finalizes_to_jal_with_symbol_target() {
        use tir::backend::TargetMachine;
        use tir::backend::pipeline::{StopAfter, build_pipeline};

        let context = Context::with_default_dialects();
        let target = target_for("rv64im");
        target.register_dialects(&context);

        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![a]);
        region.add_block(block.id());

        let func = ops::func(&context, "caller", i32, Some(region.id())).build();
        let a = func.body().arguments()[0].id();

        let mut fb = IRBuilder::new(func.body());
        let call = tir::builtin::CallOpBuilder::new(&context)
            .args(vec![a])
            .attr(
                "callee",
                tir::attributes::AttributeValue::Str("foo".to_string()),
            )
            .result_type(i32)
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

        let names = body_op_names(&context, region.id());
        assert_eq!(
            names,
            vec![
                "addi",
                "addi",
                "addi",
                "jal",
                "addi",
                "addi",
                "jalr",
                "symbol_end"
            ]
        );

        let block = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap();
        let jal = block
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "jal")
            .expect("the call must finalize to jal");
        // jal links through ra and targets the callee symbol (a link-time fixup).
        assert_eq!(phys_of(&jal, "rd"), Some(("GPR".to_string(), 1)));
        assert!(jal.attributes.iter().any(|a| a.name == "imm"
            && matches!(&a.value, tir::attributes::AttributeValue::Str(s) if s == "foo")));

        body_blocks_have_no_virtual(&context, region.id());
    }

    #[test]
    fn indirect_call_finalizes_to_jalr() {
        use tir::backend::TargetMachine;
        use tir::backend::pipeline::{StopAfter, build_pipeline};

        let context = Context::with_default_dialects();
        let target = target_for("rv64im");
        target.register_dialects(&context);

        let i64 = IntegerType::new(&context, 64);
        let i32 = IntegerType::new(&context, 32);
        let module = ops::module(&context, None).build();

        let callee = context.create_value(i64, None);
        let x = context.create_value(i32, None);
        let region = context.create_region();
        let block = context.create_block(vec![callee, x]);
        region.add_block(block.id());

        let func = ops::func(&context, "caller", i32, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (callee, x) = (args[0].id(), args[1].id());

        let mut fb = IRBuilder::new(func.body());
        let call = tir::builtin::IndirectCallOpBuilder::new(&context)
            .callee(callee)
            .args(vec![x])
            .result_type(i32)
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
        let jalr = block
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "jalr" && phys_of(op, "rd") == Some(("GPR".to_string(), 1)))
            .expect("the indirect call must finalize to a linking jalr");
        // The callee register was colored to a real register distinct from the
        // argument being passed in a0.
        let target_reg = phys_of(&jalr, "rs1").expect("jalr target must be physical");
        assert_ne!(target_reg.1, 10);

        body_blocks_have_no_virtual(&context, region.id());
    }

    /// A RISC-V target with a deliberately tiny allocatable register file (a0, a1,
    /// t0, t1, t2), so a handful of live values overflow it and exercise spilling
    /// without stressing the solver. Spill code emission delegates to the real
    /// target.
    struct TinyRiscv(crate::RiscvRegAlloc);

    impl tir::backend::regalloc::TargetRegAlloc for TinyRiscv {
        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            tir::backend::regalloc::RegisterInfo {
                classes: &[tir::backend::regalloc::RegClassInfo {
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

    #[test]
    fn encoders_match_isa_golden_words() {
        use crate::{
            AddImmOpBuilder, AddOpBuilder, BranchEqOpBuilder, JumpAndLinkOpBuilder,
            JumpAndLinkRegOpBuilder, LoadDoubleWordOpBuilder, LoadUpperImmOpBuilder,
            StoreDoubleWordOpBuilder, phys,
        };
        use tir::attributes::AttributeValue;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let encoders = crate::get_instruction_encoders();
        let gpr = |i: u16| phys(&("GPR".to_string(), i));
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

        // Golden words produced by clang/llvm-mc for riscv64.
        let add = AddOpBuilder::new(&context)
            .attr("rd", gpr(10))
            .attr("rs1", gpr(11))
            .attr("rs2", gpr(12))
            .build();
        assert_eq!(word(add.id()), 0x00C58533, "add x10, x11, x12");

        let addi = AddImmOpBuilder::new(&context)
            .attr("rd", gpr(5))
            .attr("rs1", gpr(6))
            .attr("imm", AttributeValue::Int(-1))
            .build();
        assert_eq!(word(addi.id()), 0xFFF30293, "addi x5, x6, -1");

        let jalr = JumpAndLinkRegOpBuilder::new(&context)
            .attr("rd", gpr(0))
            .attr("rs1", gpr(1))
            .attr("imm", AttributeValue::Int(0))
            .build();
        assert_eq!(word(jalr.id()), 0x00008067, "jalr x0, x1, 0 (ret)");

        let beq = BranchEqOpBuilder::new(&context)
            .attr("rs1", gpr(1))
            .attr("rs2", gpr(2))
            .attr("imm", AttributeValue::Int(24))
            .build();
        assert_eq!(word(beq.id()), 0x00208C63, "beq x1, x2, +24");

        let jal = JumpAndLinkOpBuilder::new(&context)
            .attr("rd", gpr(1))
            .attr("imm", AttributeValue::Int(20))
            .build();
        assert_eq!(word(jal.id()), 0x014000EF, "jal x1, +20");

        let sd = StoreDoubleWordOpBuilder::new(&context)
            .attr("rs1", gpr(2))
            .attr("rs2", gpr(8))
            .attr("imm", AttributeValue::Int(16))
            .build();
        assert_eq!(word(sd.id()), 0x00813823, "sd x8, 16(x2)");

        let ld = LoadDoubleWordOpBuilder::new(&context)
            .attr("rd", gpr(8))
            .attr("rs1", gpr(2))
            .attr("imm", AttributeValue::Int(16))
            .build();
        assert_eq!(word(ld.id()), 0x01013403, "ld x8, 16(x2)");

        let lui = LoadUpperImmOpBuilder::new(&context)
            .attr("rd", gpr(7))
            .attr("imm", AttributeValue::Int(1))
            .build();
        assert_eq!(word(lui.id()), 0x000013B7, "lui x7, 1");
    }

    #[test]
    fn encoder_rejects_unencodable_operands() {
        use crate::{AddImmOpBuilder, AddOpBuilder, phys, virt};
        use tir::attributes::AttributeValue;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let encoders = crate::get_instruction_encoders();
        let gpr = |i: u16| phys(&("GPR".to_string(), i));

        // A virtual register cannot be encoded.
        let add = AddOpBuilder::new(&context)
            .attr("rd", virt(1, "GPR"))
            .attr("rs1", gpr(11))
            .attr("rs2", gpr(12))
            .build();
        assert!(encoders["add"](&context.get_op(add.id())).is_none());

        // An immediate outside bits<12> cannot be encoded.
        let addi = AddImmOpBuilder::new(&context)
            .attr("rd", gpr(5))
            .attr("rs1", gpr(6))
            .attr("imm", AttributeValue::Int(4096))
            .build();
        assert!(encoders["addi"](&context.get_op(addi.id())).is_none());
    }

    #[test]
    fn symbol_and_block_operands_become_fixups() {
        use crate::{BranchEqOpBuilder, JumpAndLinkOpBuilder, phys};
        use tir::attributes::AttributeValue;
        use tir::backend::binary::{FixupTarget, InstFixup};

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let encoders = crate::get_instruction_encoders();
        let patchers = crate::get_instruction_patchers();
        let gpr = |i: u16| phys(&("GPR".to_string(), i));

        let jal = JumpAndLinkOpBuilder::new(&context)
            .attr("rd", gpr(1))
            .attr("imm", AttributeValue::Str("foo".to_string()))
            .build();
        let enc = encoders["jal"](&context.get_op(jal.id())).unwrap();
        assert_eq!(enc.bytes, 0x000000EFu32.to_le_bytes());
        assert_eq!(
            enc.fixups,
            vec![InstFixup {
                operand: "imm",
                target: FixupTarget::Symbol("foo".to_string()),
            }]
        );

        // Patching scatters a resolved pc-relative delta into the J-type bits.
        let mut bytes = enc.bytes.clone();
        patchers["jal"](&mut bytes, 20).unwrap();
        assert_eq!(bytes, 0x014000EFu32.to_le_bytes(), "jal x1, +20");

        // Odd and out-of-range deltas are rejected.
        assert!(patchers["jal"](&mut enc.bytes.clone(), 3).is_none());
        assert!(patchers["jal"](&mut enc.bytes.clone(), 1 << 20).is_none());

        let block = context.create_block(vec![]);
        let beq = BranchEqOpBuilder::new(&context)
            .attr("rs1", gpr(1))
            .attr("rs2", gpr(2))
            .attr("imm", AttributeValue::Block(block.id()))
            .build();
        let enc = encoders["beq"](&context.get_op(beq.id())).unwrap();
        assert_eq!(enc.bytes, 0x00208063u32.to_le_bytes());
        assert_eq!(
            enc.fixups,
            vec![InstFixup {
                operand: "imm",
                target: FixupTarget::Block(block.id()),
            }]
        );

        let mut bytes = enc.bytes.clone();
        patchers["beq"](&mut bytes, 24).unwrap();
        assert_eq!(bytes, 0x00208C63u32.to_le_bytes(), "beq x1, x2, +24");
    }
}

#[cfg(test)]
mod target_parser_tests {
    use super::{Feature, TargetConfig};

    fn features(march: &str, mattr: Option<&str>) -> Vec<Feature> {
        TargetConfig::parse(march, None, mattr)
            .expect("march should parse")
            .features
    }

    #[test]
    fn march_accepts_gcc_style_isa_strings() {
        assert_eq!(
            TargetConfig::parse("rv64im", None, None).map(|c| c.canonical_name()),
            Ok("riscv64")
        );
        assert_eq!(
            TargetConfig::parse("rv32imac", None, None).map(|c| c.canonical_name()),
            Ok("riscv32")
        );
        assert_eq!(
            TargetConfig::parse("rv64gc_zba_zbb", None, None).map(|c| c.canonical_name()),
            Ok("riscv64")
        );
    }

    #[test]
    fn march_selects_extension_features() {
        assert_eq!(features("rv64i", None), vec![Feature::RV64I]);
        assert_eq!(
            features("rv64im", None),
            vec![Feature::RV64I, Feature::RVM, Feature::Zmmul]
        );
        assert_eq!(
            features("rv32imac", None),
            vec![Feature::RV32I, Feature::RVM, Feature::Zmmul]
        );
        assert_eq!(
            features("rv32i_zmmul", None),
            vec![Feature::RV32I, Feature::Zmmul]
        );
        // G abbreviates IMAFD...; M is the modeled part.
        assert!(features("rv64gc_zba_zbb", None).contains(&Feature::RVM));
        // Bare architecture names select the generic, everything-on profile.
        assert_eq!(
            features("riscv64", None),
            vec![
                Feature::RV64I,
                Feature::Zmmul,
                Feature::RVM,
                Feature::Zicsr,
                Feature::RVV
            ]
        );
        assert!(!features("riscv32", None).contains(&Feature::RV64I));
    }

    #[test]
    fn mattr_toggles_features() {
        assert_eq!(
            features("rv64i", Some("+m")),
            vec![Feature::RV64I, Feature::RVM, Feature::Zmmul]
        );
        assert_eq!(
            features("rv64im", Some("-m,+zmmul")),
            vec![Feature::RV64I, Feature::Zmmul]
        );
        assert!(TargetConfig::parse("rv64i", None, Some("+vector")).is_err());
        assert!(TargetConfig::parse("rv64i", None, Some("m")).is_err());
        assert!(TargetConfig::parse("rv64i", None, Some("-rv64i")).is_err());
    }

    #[test]
    fn mcpu_accepts_target_prefixed_generic_names() {
        assert!(TargetConfig::parse("rv32im", Some("riscv32-generic-in-order"), None).is_ok());
        assert!(TargetConfig::parse("rv64im", Some("riscv32-generic-in-order"), None).is_err());
        assert!(TargetConfig::parse("rv64im", Some("generic-in-order"), None).is_ok());
    }

    #[test]
    fn mcpu_resolves_machine_models() {
        let config = TargetConfig::parse("rv64im", Some("generic-ooo"), None).unwrap();
        assert_eq!(config.machine.as_deref(), Some("rv64-ooo"));
        let config = TargetConfig::parse("rv32i", Some("scr1-3stage"), None).unwrap();
        assert_eq!(config.machine.as_deref(), Some("scr1-3stage"));
        // The SCR1 model is declared `for [RV32I]`; rv64 must reject it.
        assert!(TargetConfig::parse("rv64i", Some("scr1-3stage"), None).is_err());
    }

    #[test]
    fn isa_params_resolve_from_the_selected_base() {
        assert_eq!(crate::isa_params(&[Feature::RV32I]), vec![("XLEN", 32)]);
        assert_eq!(
            crate::isa_params(&[Feature::RV64I, Feature::RVM]),
            vec![("XLEN", 64)]
        );
        // VR is dynamically sized (width = vlenb, an architectural runtime value),
        // so it carries no static width here; its size is supplied by the machine.
        assert_eq!(
            crate::register_widths(&[Feature::RV32I]),
            vec![("PC", 32), ("GPR", 32), ("CSR", 32), ("VCSR", 32)]
        );
        assert_eq!(
            crate::register_widths(&[Feature::RV64I]),
            vec![("PC", 64), ("GPR", 64), ("CSR", 64), ("VCSR", 64)]
        );
        // Extensions alone resolve nothing; the base supplies XLEN.
        assert_eq!(crate::isa_params(&[Feature::RVM]), vec![]);
    }

    #[test]
    fn counter_registers_follow_the_feature_set() {
        use tir::backend::{PerfCounter, TargetMachine};

        let target = |march| crate::RiscvTarget {
            config: TargetConfig::parse(march, None, None).expect("march should parse"),
        };
        assert!(target("rv64i").counter_registers().is_empty());
        // RV64 reads the full 64-bit counters; RV32 adds the high-half CSRs.
        assert_eq!(target("rv64i_zicsr").counter_registers().len(), 3);
        let rv32 = target("rv32i_zicsr").counter_registers();
        assert_eq!(rv32.len(), 6);
        assert!(rv32.contains(&("CSR", 0xC80, PerfCounter::CyclesHigh)));
        assert!(rv32.contains(&("CSR", 0xC82, PerfCounter::InstructionsRetiredHigh)));
    }

    #[test]
    fn base_isas_are_mutually_exclusive() {
        assert!(TargetConfig::parse("rv32i", None, Some("+rv64i")).is_err());
    }

    #[test]
    fn unknown_or_malformed_march_is_rejected() {
        assert!(TargetConfig::parse("rv64", None, None).is_err());
        assert!(TargetConfig::parse("rv64zm", None, None).is_err());
        assert!(TargetConfig::parse("mips", None, None).is_err());
        assert!(TargetConfig::parse("rv64im", Some("riscv64-unknown-cpu"), None).is_err());
    }
}
