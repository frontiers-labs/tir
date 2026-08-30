use tir::Operation;
use tir::backend::RegSlot;
use tir::helpers::{dialect, operation};

const MODEL_CHECK_SOURCES: &[(&str, &str)] = &[
    ("main.tmdl", include_str!("../defs/main.tmdl")),
    ("base.tmdl", include_str!("../defs/base.tmdl")),
    (
        "multiplication.tmdl",
        include_str!("../defs/multiplication.tmdl"),
    ),
    ("float.tmdl", include_str!("../defs/float.tmdl")),
    ("compressed.tmdl", include_str!("../defs/compressed.tmdl")),
    ("atomics.tmdl", include_str!("../defs/atomics.tmdl")),
    ("zifencei.tmdl", include_str!("../defs/zifencei.tmdl")),
    ("zicsr.tmdl", include_str!("../defs/zicsr.tmdl")),
    ("perf.tmdl", include_str!("../defs/perf.tmdl")),
    ("vector.tmdl", include_str!("../defs/vector.tmdl")),
    ("vector_int.tmdl", include_str!("../defs/vector_int.tmdl")),
    ("vector_mask.tmdl", include_str!("../defs/vector_mask.tmdl")),
    ("vector_red.tmdl", include_str!("../defs/vector_red.tmdl")),
    ("vector_perm.tmdl", include_str!("../defs/vector_perm.tmdl")),
    (
        "vector_widen.tmdl",
        include_str!("../defs/vector_widen.tmdl"),
    ),
    (
        "vector_fixed.tmdl",
        include_str!("../defs/vector_fixed.tmdl"),
    ),
    ("vector_mem.tmdl", include_str!("../defs/vector_mem.tmdl")),
    (
        "vector_float.tmdl",
        include_str!("../defs/vector_float.tmdl"),
    ),
];

mod compress;
mod obj;
mod vsetvli;

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
        // D64 is the internal D∧RV64 conjunction (rv64-only D instructions
        // like fmv.d.x); it follows the D/XLEN selection automatically.
        if config.xlen == 64
            && config.features.contains(&Feature::D)
            && !config.features.contains(&Feature::D64)
        {
            config.features.push(Feature::D64);
        }
        // VF is the internal V∧D conjunction gating the vector floating-point
        // instructions, which need both the vector unit and the f register file.
        if config.features.contains(&Feature::RVV)
            && config.features.contains(&Feature::D)
            && !config.features.contains(&Feature::VF)
        {
            config.features.push(Feature::VF);
        }
        // The M *W forms follow the same pattern: Zmmul64/RVM64 gate the
        // rv64-only word multiply/divide instructions; A64 gates the rv64-only
        // doubleword atomics.
        for (conj, base) in [
            (Feature::Zmmul64, Feature::Zmmul),
            (Feature::RVM64, Feature::RVM),
            (Feature::A64, Feature::A),
        ] {
            if config.xlen == 64
                && config.features.contains(&base)
                && !config.features.contains(&conj)
            {
                config.features.push(conj);
            }
        }
        // The C conjunctions follow the same pattern: C32/C64 gate the
        // XLEN-specific compressed forms, Zcd/Zcf the float compressed
        // loads/stores.
        if config.features.contains(&Feature::C) {
            let derived = [
                (Feature::C32, config.xlen == 32),
                (Feature::C64, config.xlen == 64),
                (Feature::Zcd, config.features.contains(&Feature::D)),
                (
                    Feature::Zcf,
                    config.xlen == 32 && config.features.contains(&Feature::F),
                ),
            ];
            for (feature, enabled) in derived {
                if enabled && !config.features.contains(&feature) {
                    config.features.push(feature);
                }
            }
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
                Feature::RV32I | Feature::C32 | Feature::Zcf => xlen == 32,
                Feature::RV64I
                | Feature::D64
                | Feature::C64
                | Feature::Zmmul64
                | Feature::RVM64
                | Feature::A64 => xlen == 64,
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
        // The D extension implies F.
        "d" => Some(vec![Feature::D, Feature::F]),
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
        // G abbreviates IMAFD_Zicsr_Zifencei.
        'g' => {
            enable(base_feature);
            enable(Feature::RVM);
            enable(Feature::Zmmul);
            enable(Feature::A);
            enable(Feature::F);
            enable(Feature::D);
            enable(Feature::Zicsr);
            enable(Feature::Zifencei);
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
            'f' => {
                enable(Feature::F);
                skip_extension_version(&mut chars);
            }
            // D implies F.
            'd' => {
                enable(Feature::F);
                enable(Feature::D);
                skip_extension_version(&mut chars);
            }
            'c' => {
                enable(Feature::C);
                skip_extension_version(&mut chars);
            }
            'a' => {
                enable(Feature::A);
                skip_extension_version(&mut chars);
            }
            // Standard single-letter extensions TMDL does not model yet are
            // accepted so common GNU march strings (e.g. rv64gc) keep working;
            // they contribute no instructions.
            'q' | 'l' | 'b' | 'j' | 't' | 'p' | 'h' => {
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

dialect! {
    RiscvDialect {
        name: "riscv",
        operation_file: concat!(env!("OUT_DIR"), "/riscv_ops.rs"),
        type_parsers: reg_class_type_parsers(),
    }
}

pub mod ops {
    pub use super::*;
}

impl RiscvDialect {
    pub fn get_asm_parser(&self) -> tir::backend::AsmParser {
        tir::backend::AsmParser::new(get_instruction_parsers(Feature::ALL).0)
    }
}

fn lower_func_and_return_to_asm_symbol(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    tir::backend::lower::lower_function_and_return(context, op, rewriter, |ty| {
        argument_register_class(context, ty)
    })
}

fn argument_register_class(
    context: &tir::Context,
    ty: tir::TypeId,
) -> Result<tir::backend::regalloc::RegClassId, tir::PassError> {
    let ty = context.get_type_data(ty);
    let ty = ty.as_ref() as &dyn std::any::Any;
    if let Some(vector) = ty.downcast_ref::<tir::vector::VectorType>() {
        let element = context.get_type_data(vector.element(context));
        let bits = (element.as_ref() as &dyn std::any::Any)
            .downcast_ref::<tir::builtin::IntegerType>()
            .map_or(0, tir::builtin::IntegerType::width) as i64;
        return vector.length().map_or(Ok(RegClass::VR.id()), |lanes| {
            vsetvli::vr_class_for_bits(lanes as i64 * bits)
        });
    }
    if let Some(float) = ty.downcast_ref::<tir::builtin::FloatType>() {
        return match float.bit_width() {
            32 => Ok(RegClass::FPR32.id()),
            64 => Ok(RegClass::FPR64.id()),
            width => Err(tir::PassError::InvalidRuleSet(format!(
                "{width}-bit float arguments are not supported (only f32/f64)"
            ))),
        };
    }
    Ok(RegClass::GPR.id())
}

/// Lower `vector.vector_len` to `vsetvli rd, avl`: the one instruction that both
/// produces a value (the granted element count) and configures the vector unit.
/// The vsetvli-insertion pass recognizes it as establishing the configuration,
/// so ops demanding the granted count need no further vset{i}vli.
fn lower_vector_len(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::attributes::AttributeValue;

    if op.as_op::<tir::vector::VectorLenOp>().is_none() {
        return Ok(false);
    }
    let inner = op.op();
    let (Some(&result), Some(&avl)) = (inner.results().first(), inner.operands().first()) else {
        return Err(tir::PassError::RewriteFailed(inner.id));
    };
    // The grant is element-width-specific (VLMAX depends on SEW), so the op
    // names the width it configures for.
    let Some(AttributeValue::Int(sew)) = inner.attr("sew") else {
        return Err(tir::PassError::InvalidRuleSet(
            "vector.vector_len requires a `sew` attribute".to_string(),
        ));
    };
    // Selection is done, so the granted length and the requested one are
    // registers now, not integers.
    context.retype_value(result, gpr_ty(context));
    let lowered = VSetVliOpBuilder::new(context)
        .result_values(vec![result])
        .avl(avl)
        .attr("vtypei", AttributeValue::Int(vsetvli::vtypei_for(sew, 1)?))
        .build();
    rewriter.replace_op(op, &lowered)?;
    Ok(true)
}

/// Emit the branch-if-nonzero fallback for a condition no branch rule fused:
/// `andi tmp, cond, 1` + `bne tmp, x0, dest`. The condition is a width-1
/// value, so only bit 0 of its register is defined — branching on the whole
/// register would read undefined bits.
fn emit_branch_nonzero(
    context: &tir::Context,
    condition: tir::ValueId,
    dest: tir::BlockId,
) -> Vec<Box<dyn Operation>> {
    let bit = context.create_value(gpr_ty(context), None).id();
    vec![
        Box::new(
            AndImmOpBuilder::new(context)
                .result_values(vec![bit])
                .rs1(condition)
                .attr("imm", tir::attributes::AttributeValue::Int(1))
                .build(),
        ),
        Box::new(
            BranchNotEqOpBuilder::new(context)
                .rs1(bit)
                .attr("rs2", phys(&(RegClass::GPR.id(), 0)))
                .attr("imm", tir::attributes::AttributeValue::Block(dest))
                .build(),
        ),
    ]
}

/// Build a register-register move (`addi rd, rs, 0`).
fn mv(context: &tir::Context, rd: RegSlot, rs: RegSlot) -> Box<dyn Operation> {
    let builder =
        AddImmOpBuilder::new(context).attr("imm", tir::attributes::AttributeValue::Int(0));
    let builder = tir::reg_use!(builder, rs1, rs);
    Box::new(tir::reg_def!(builder, rd, rd).build())
}

/// The type of a value living in a `GPR`.
fn gpr_ty(context: &tir::Context) -> tir::TypeId {
    tir::backend::RegClassType::new(context, RegClass::GPR.id())
}

pub fn create_isel_pass(context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
    create_isel_pass_for(
        context,
        Feature::ALL,
        riscv_abi_by_name("lp64d").expect("RISC-V must define lp64d"),
    )
}

/// The C extension features. Compressed instructions never take part in
/// instruction selection: they are strictly narrower forms of base
/// instructions (tied operands, 3-bit register fields), so selecting them
/// directly would constrain register allocation for no gain. The
/// finalize-stage compression pass rewrites base instructions into compressed
/// forms after registers and immediates are known.
const COMPRESSED_FEATURES: &[Feature] = &[
    Feature::C,
    Feature::C32,
    Feature::C64,
    Feature::Zcd,
    Feature::Zcf,
];

fn create_isel_pass_for(
    context: &tir::Context,
    features: &[Feature],
    abi: &'static tir::backend::abi::AbiInfo,
) -> tir::backend::isel::InstructionSelectPass {
    let features: Vec<Feature> = features
        .iter()
        .copied()
        .filter(|f| !COMPRESSED_FEATURES.contains(f))
        .collect();
    tir::backend::isel::InstructionSelectPass::new(get_isel_rules(context, &features))
        .with_axioms(include_str!("isel-materialize.axioms"))
        .with_branch_emitters(tir::backend::isel::BranchEmitters {
            uncond: tir::backend::emit_uncond_branch,
            cond_nonzero: emit_branch_nonzero,
        })
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
        .with_call_lowering(abi, Box::new(RiscvCallEmitter))
        .with_op_lowering(lower_vector_len)
}

struct RiscvCallEmitter;

impl tir::backend::call_lowering::CallEmitter for RiscvCallEmitter {
    fn copy(&self, context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
        let class = tir::backend::slot_class(context, dst);
        match class {
            Some(class) if class == RegClass::FPR32.id() => {
                let builder = FMoveSOpBuilder::new(context);
                let builder = tir::reg_use!(builder, fs, src);
                Box::new(tir::reg_def!(builder, fd, dst).build())
            }
            Some(class) if class == RegClass::FPR64.id() => {
                let builder = FMoveDOpBuilder::new(context);
                let builder = tir::reg_use!(builder, fs, src);
                Box::new(tir::reg_def!(builder, fd, dst).build())
            }
            _ => mv(context, dst, src),
        }
    }

    fn stack_arg_store(
        &self,
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        value: tir::ValueId,
        class: tir::backend::regalloc::RegClassId,
        offset: i64,
    ) -> Result<Box<dyn Operation>, tir::PassError> {
        let offset = tir::attributes::AttributeValue::Int(offset);
        if class == RegClass::FPR64.id() {
            Ok(Box::new(
                FStoreDoubleOpBuilder::new(context)
                    .attr("rs1", phys(&abi.sp))
                    .fs2(value)
                    .attr("imm", offset)
                    .build(),
            ))
        } else {
            Ok(Box::new(
                StoreDoubleWordOpBuilder::new(context)
                    .attr("rs1", phys(&abi.sp))
                    .rs2(value)
                    .attr("imm", offset)
                    .build(),
            ))
        }
    }

    fn call_prefix(
        &self,
        _context: &tir::Context,
        _abi: &tir::backend::abi::AbiInfo,
        _outgoing_size: u32,
        _vector_register_args: u8,
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }

    fn call_suffix(
        &self,
        _context: &tir::Context,
        _abi: &tir::backend::abi::AbiInfo,
        _outgoing_size: u32,
    ) -> Vec<Box<dyn Operation>> {
        Vec::new()
    }
}

fn phys(reg: &tir::backend::liveness::PhysReg) -> tir::attributes::AttributeValue {
    tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Physical {
        class: reg.0,
        index: reg.1,
    })
}

/// Store a physical register to `[frame + offset]`, dispatching on its file
/// (`fsw`/`fsd` for the float files, `sd` otherwise). Used to preserve
/// callee-saved registers in the prologue.
fn reg_store(
    context: &tir::Context,
    reg: &tir::backend::liveness::PhysReg,
    frame: &tir::backend::liveness::PhysReg,
    offset: i64,
) -> Box<dyn Operation> {
    let offset = tir::attributes::AttributeValue::Int(offset);
    match reg.0.name() {
        "FPR32" => Box::new(
            FStoreWordOpBuilder::new(context)
                .attr("rs1", phys(frame))
                .attr("fs2", phys(reg))
                .attr("imm", offset)
                .build(),
        ),
        "FPR64" => Box::new(
            FStoreDoubleOpBuilder::new(context)
                .attr("rs1", phys(frame))
                .attr("fs2", phys(reg))
                .attr("imm", offset)
                .build(),
        ),
        _ => Box::new(
            StoreDoubleWordOpBuilder::new(context)
                .attr("rs1", phys(frame))
                .attr("rs2", phys(reg))
                .attr("imm", offset)
                .build(),
        ),
    }
}

/// Reload a physical register from `[frame + offset]`, the inverse of
/// [`reg_store`].
fn reg_reload(
    context: &tir::Context,
    reg: &tir::backend::liveness::PhysReg,
    frame: &tir::backend::liveness::PhysReg,
    offset: i64,
) -> Box<dyn Operation> {
    let offset = tir::attributes::AttributeValue::Int(offset);
    match reg.0.name() {
        "FPR32" => Box::new(
            FLoadWordOpBuilder::new(context)
                .attr("fd", phys(reg))
                .attr("rs1", phys(frame))
                .attr("imm", offset)
                .build(),
        ),
        "FPR64" => Box::new(
            FLoadDoubleOpBuilder::new(context)
                .attr("fd", phys(reg))
                .attr("rs1", phys(frame))
                .attr("imm", offset)
                .build(),
        ),
        _ => Box::new(
            LoadDoubleWordOpBuilder::new(context)
                .attr("rd", phys(reg))
                .attr("rs1", phys(frame))
                .attr("imm", offset)
                .build(),
        ),
    }
}

/// RISC-V register allocation target: the generated register file plus `sd`/`ld`
/// spill code and an `addi sp, sp, ±frame` prologue/epilogue.
pub struct RiscvRegAlloc;

impl tir::backend::regalloc::TargetRegAlloc for RiscvRegAlloc {
    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        register_info()
    }

    fn emit_spill_store(
        &self,
        context: &tir::Context,
        value: tir::ValueId,
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Box<dyn Operation> {
        let offset = tir::attributes::AttributeValue::Int(offset);
        match class.name() {
            "FPR32" => Box::new(
                FStoreWordOpBuilder::new(context)
                    .attr("rs1", phys(frame))
                    .fs2(value)
                    .attr("imm", offset)
                    .build(),
            ),
            "FPR64" => Box::new(
                FStoreDoubleOpBuilder::new(context)
                    .attr("rs1", phys(frame))
                    .fs2(value)
                    .attr("imm", offset)
                    .build(),
            ),
            _ => Box::new(
                StoreDoubleWordOpBuilder::new(context)
                    .attr("rs1", phys(frame))
                    .rs2(value)
                    .attr("imm", offset)
                    .build(),
            ),
        }
    }

    fn emit_spill_reload(
        &self,
        context: &tir::Context,
        value: tir::ValueId,
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Box<dyn Operation> {
        let offset = tir::attributes::AttributeValue::Int(offset);
        let results = vec![value];
        match class.name() {
            "FPR32" => Box::new(
                FLoadWordOpBuilder::new(context)
                    .result_values(results)
                    .attr("rs1", phys(frame))
                    .attr("imm", offset)
                    .build(),
            ),
            "FPR64" => Box::new(
                FLoadDoubleOpBuilder::new(context)
                    .result_values(results)
                    .attr("rs1", phys(frame))
                    .attr("imm", offset)
                    .build(),
            ),
            _ => Box::new(
                LoadDoubleWordOpBuilder::new(context)
                    .result_values(results)
                    .attr("rs1", phys(frame))
                    .attr("imm", offset)
                    .build(),
            ),
        }
    }

    fn emit_copy(
        &self,
        context: &tir::Context,
        class: tir::backend::regalloc::RegClassId,
        dst: RegSlot,
        src: RegSlot,
    ) -> Box<dyn Operation> {
        macro_rules! move_op {
            ($builder:ident, $use_port:ident, $def_port:ident) => {{
                let builder = $builder::new(context);
                let builder = tir::reg_use!(builder, $use_port, src);
                Box::new(tir::reg_def!(builder, $def_port, dst).build())
            }};
        }
        match class.name() {
            "GPR" => mv(context, dst, src),
            "FPR32" => move_op!(FMoveSOpBuilder, fs, fd),
            "FPR64" => move_op!(FMoveDOpBuilder, fs, fd),
            "VR" => move_op!(VMove1ROpBuilder, vs, vd),
            "VRM2" => move_op!(VMove2ROpBuilder, vs, vd),
            "VRM4" => move_op!(VMove4ROpBuilder, vs, vd),
            "VRM8" => move_op!(VMove8ROpBuilder, vs, vd),
            other => unimplemented!("riscv register copy for class {other} is not implemented"),
        }
    }

    fn emit_prologue(
        &self,
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        size: u32,
        saves: &[(tir::backend::liveness::PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        let sp = abi.sp;
        let mut ops: Vec<Box<dyn Operation>> = vec![Box::new(
            AddImmOpBuilder::new(context)
                .attr("rd", phys(&sp))
                .attr("rs1", phys(&sp))
                .attr("imm", tir::attributes::AttributeValue::Int(-(size as i64)))
                .build(),
        )];
        for (reg, offset) in saves {
            ops.push(reg_store(context, reg, &sp, *offset));
        }
        ops
    }

    fn emit_epilogue(
        &self,
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        size: u32,
        saves: &[(tir::backend::liveness::PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        let sp = abi.sp;
        let mut ops: Vec<Box<dyn Operation>> = Vec::new();
        for (reg, offset) in saves {
            ops.push(reg_reload(context, reg, &sp, *offset));
        }
        ops.push(Box::new(
            AddImmOpBuilder::new(context)
                .attr("rd", phys(&sp))
                .attr("rs1", phys(&sp))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        ));
        ops
    }

    fn emit_frame_address(
        &self,
        context: &tir::Context,
        dst: tir::ValueId,
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Result<Vec<Box<dyn Operation>>, tir::PassError> {
        if class.name() != "GPR" {
            return Err(tir::PassError::InvalidRuleSet(format!(
                "riscv stack allocation addresses for register class {} are not supported",
                class.name()
            )));
        }
        Ok(vec![Box::new(
            AddImmOpBuilder::new(context)
                .result_values(vec![dst])
                .attr("rs1", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )])
    }
}

pub fn create_regalloc_stage() -> Vec<Box<dyn tir::Pass>> {
    tir::backend::prealloc::regalloc_stage_for(
        || Box::new(RiscvRegAlloc),
        riscv_abi_by_name("lp64d").expect("RISC-V must define lp64d"),
    )
}

/// The RISC-V target, selected via `--march`/`--mcpu`.
pub struct RiscvTarget {
    config: TargetConfig,
    selected_abi: &'static tir::backend::abi::AbiInfo,
}

impl tir::backend::TargetMachine for RiscvTarget {
    fn name(&self) -> &'static str {
        self.config.canonical_name()
    }

    fn model_check_target(&self) -> Option<tir::backend::ModelCheckTarget> {
        Some(tir::backend::ModelCheckTarget {
            isa: if self.config.xlen == 32 {
                "RV32I"
            } else {
                "RV64I"
            },
            features: self.config.features.iter().map(Feature::name).collect(),
            sources: MODEL_CHECK_SOURCES,
        })
    }

    fn register_dialects(&self, context: &tir::Context) {
        context.register_dialect::<tir::backend::AsmDialect>();
        context.register_dialect::<RiscvDialect>();
        context.register_reg_classes(register_info().classes);
    }

    fn data_layout(&self) -> Option<tir::attributes::AttributeValue> {
        let pointer = self.config.xlen;
        Some(tir::data_layout_spec(
            tir::Endianness::Little,
            self.abi().stack.align * 8,
            &[
                ("i1", 8, 8),
                ("i8", 8, 8),
                ("i16", 16, 16),
                ("i32", 32, 32),
                ("i64", 64, 64),
                ("f32", 32, 32),
                ("f64", 64, 64),
                ("p", pointer, pointer),
            ],
        ))
    }

    fn target_env(&self) -> Option<tir::attributes::AttributeValue> {
        // Lowercased TMDL feature names, which `--mattr` accepts alongside the
        // march extension letters.
        let features: Vec<String> = self
            .config
            .features
            .iter()
            .map(|feature| feature.name().to_ascii_lowercase())
            .collect();
        Some(tir::target_env_spec(
            self.config.canonical_name(),
            &features,
        ))
    }

    fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
        create_isel_pass_for(context, &self.config.features, self.abi())
            .with_data_layout(self.data_layout())
    }

    fn regalloc_target(&self) -> Box<dyn tir::backend::regalloc::TargetRegAlloc> {
        Box::new(RiscvRegAlloc)
    }

    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        use tir::backend::regalloc::TargetRegAlloc;
        RiscvRegAlloc.register_info()
    }

    fn abis(&self) -> &'static [tir::backend::abi::AbiInfo] {
        riscv_abis()
    }

    fn abi(&self) -> &'static tir::backend::abi::AbiInfo {
        self.selected_abi
    }

    fn asm_parser(&self, _context: &tir::Context) -> tir::backend::AsmParser {
        let (parsers, disabled) = get_instruction_parsers(&self.config.features);
        tir::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled)
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

    fn machine_passes(&self) -> Vec<Box<dyn tir::Pass>> {
        if self.config.features.contains(&Feature::RVV) {
            vec![Box::new(vsetvli::InsertVsetvliPass)]
        } else {
            Vec::new()
        }
    }

    fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        if self.config.xlen == 64 {
            vec![obj::lower_constant_rv64, obj::lower_sym_addr]
        } else {
            vec![obj::lower_constant_rv32, obj::lower_sym_addr]
        }
    }

    fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        // Compression must precede virtual-op finalization: a lowered op is
        // not revisited within the pass, and `vret` compresses directly to
        // `c.jr ra`.
        if self.config.features.contains(&Feature::C) {
            let compress = if self.config.xlen == 64 {
                compress::compress_rv64
            } else {
                compress::compress_rv32
            };
            vec![compress, obj::finalize_virtual_ops]
        } else {
            vec![obj::finalize_virtual_ops]
        }
    }

    fn object_format(&self) -> Option<tir::backend::binary::ObjectFormatInfo> {
        Some(obj::object_format(self.config.xlen, &self.config.features))
    }
}

fn select_riscv(
    march: &str,
    mcpu: Option<&str>,
    mattr: Option<&str>,
    mabi: Option<&str>,
) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
    let owned = ["riscv", "rv32", "rv64"]
        .iter()
        .any(|prefix| normalize(march).starts_with(prefix));
    if !owned {
        return Ok(None);
    }
    let config = TargetConfig::parse(march, mcpu, mattr)?;
    let selected_abi = match mabi {
        Some(name) => riscv_abi_by_name(name).ok_or_else(|| {
            format!(
                "unknown ABI '{name}' for riscv (available: {})",
                riscv_abis()
                    .iter()
                    .map(|abi| abi.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?,
        None if config.features.contains(&Feature::D) => {
            riscv_abi_by_name("lp64d").expect("RISC-V must define lp64d")
        }
        None if config.features.contains(&Feature::F) => {
            riscv_abi_by_name("lp64f").expect("RISC-V must define lp64f")
        }
        None => riscv_abi_by_name("lp64").expect("RISC-V must define lp64"),
    };
    Ok(Some(Box::new(RiscvTarget {
        config,
        selected_abi,
    })))
}

tir::register_target!(select_riscv, ["riscv32", "riscv64"]);

fn riscv_abis() -> &'static [tir::backend::abi::AbiInfo] {
    static ABIS: std::sync::OnceLock<Vec<tir::backend::abi::AbiInfo>> = std::sync::OnceLock::new();
    ABIS.get_or_init(|| {
        abis()
            .iter()
            .map(|abi| tir::backend::abi::AbiInfo {
                indirect_result: Some((RegClass::GPR.id(), 10)),
                ..*abi
            })
            .collect()
    })
}

fn riscv_abi_by_name(name: &str) -> Option<&'static tir::backend::abi::AbiInfo> {
    riscv_abis()
        .iter()
        .find(|abi| abi.name.eq_ignore_ascii_case(name))
}
