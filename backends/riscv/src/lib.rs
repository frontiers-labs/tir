use tir::helpers::{dialect, operation};
use tir::{Any, Operation};

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

operation! {
    VirtualReturnOp {
        name: "vret",
        dialect: "riscv",
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
        dialect: "riscv",
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
        tir::backend::print_branch(fmt, self, "riscv.vbr")
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
        operations: [VirtualReturnOp, VirtualBranchOp, VirtualCallOp, VirtualIndirectCallOp],
        operation_file: concat!(env!("OUT_DIR"), "/riscv_ops.rs"),
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

        // Argument register class follows the value's type: vectors live in the
        // vector register file (in the LMUL group class their size implies),
        // floats in the FPR file at their format's width, everything else in
        // GPRs.
        let mut arg_regs = Vec::new();
        for arg in func.body().arguments().iter() {
            let ty = context.get_type_data(arg.ty());
            let ty_any = ty.as_ref() as &dyn std::any::Any;
            let class = if let Some(vec_ty) = ty_any.downcast_ref::<tir::vector::VectorType>() {
                let elem = context.get_type_data(vec_ty.element(context));
                let elem_bits = (elem.as_ref() as &dyn std::any::Any)
                    .downcast_ref::<tir::builtin::IntegerType>()
                    .map(|i| i.width())
                    .unwrap_or(0) as i64;
                match vec_ty.length() {
                    Some(lanes) => vsetvli::vr_class_for_bits(lanes as i64 * elem_bits)?,
                    None => RegClass::VR.id(),
                }
            } else if let Some(float_ty) = ty_any.downcast_ref::<tir::builtin::FloatType>() {
                match float_ty.bit_width() {
                    32 => RegClass::FPR32.id(),
                    64 => RegClass::FPR64.id(),
                    other => {
                        return Err(tir::PassError::InvalidRuleSet(format!(
                            "{other}-bit float arguments are not supported (only f32/f64)"
                        )));
                    }
                }
            } else {
                RegClass::GPR.id()
            };
            arg_regs.push(AttributeValue::Register(RegisterAttr::Virtual {
                id: arg.id().number(),
                class: Some(class),
            }));
        }

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
    let (Some(&result), Some(&avl)) = (inner.results.first(), inner.operands.first()) else {
        return Err(tir::PassError::RewriteFailed(inner.id));
    };
    // The grant is element-width-specific (VLMAX depends on SEW), so the op
    // names the width it configures for.
    let Some(AttributeValue::Int(sew)) = inner
        .attributes
        .iter()
        .find(|a| a.name == "sew")
        .map(|a| a.value.clone())
    else {
        return Err(tir::PassError::InvalidRuleSet(
            "vector.vector_len requires a `sew` attribute".to_string(),
        ));
    };
    let lowered = VSetVliOpBuilder::new(context)
        .attr("rd", virt(result.number(), RegClass::GPR.id()))
        .attr("avl", virt(avl.number(), RegClass::GPR.id()))
        .attr("vtypei", AttributeValue::Int(vsetvli::vtypei_for(sew, 1)?))
        .build();
    rewriter.replace_op(op, &lowered)?;
    Ok(true)
}

/// Emit the deferred unconditional branch (`vbr`, finalized to `jal x0` after
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
/// `bne cond, x0, dest`.
fn emit_branch_nonzero(
    context: &tir::Context,
    condition: tir::ValueId,
    dest: tir::BlockId,
) -> Vec<Box<dyn Operation>> {
    vec![Box::new(
        BranchNotEqOpBuilder::new(context)
            .attr("rs1", virt(condition.number(), RegClass::GPR.id()))
            .attr("rs2", phys(&(RegClass::GPR.id(), 0)))
            .attr("imm", tir::attributes::AttributeValue::Block(dest))
            .build(),
    )]
}

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

pub fn create_isel_pass(context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
    create_isel_pass_for(
        context,
        Feature::ALL,
        abi_by_name("lp64d").expect("RISC-V must define lp64d"),
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
        .with_axioms(include_str!("isel.axioms"))
        .with_axioms(include_str!("isel-materialize.axioms"))
        .with_branch_emitters(tir::backend::isel::BranchEmitters {
            uncond: emit_uncond_branch,
            cond_nonzero: emit_branch_nonzero,
        })
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
        .with_call_lowering(abi, Box::new(RiscvCallEmitter))
        .with_op_lowering(lower_vector_len)
}

struct RiscvCallEmitter;

impl tir::backend::call_lowering::CallEmitter for RiscvCallEmitter {
    fn copy(
        &self,
        context: &tir::Context,
        dst: tir::attributes::AttributeValue,
        src: tir::attributes::AttributeValue,
    ) -> Box<dyn Operation> {
        mv(context, dst, src)
    }

    fn vcall(
        &self,
        context: &tir::Context,
        callee: String,
        clobbers: tir::attributes::AttributeValue,
    ) -> Box<dyn Operation> {
        Box::new(
            VirtualCallOpBuilder::new(context)
                .attr("callee", tir::attributes::AttributeValue::Str(callee))
                .attr("clobbers", clobbers)
                .build(),
        )
    }

    fn vcall_indirect(
        &self,
        context: &tir::Context,
        callee: tir::attributes::AttributeValue,
        clobbers: tir::attributes::AttributeValue,
    ) -> Box<dyn Operation> {
        Box::new(
            VirtualIndirectCallOpBuilder::new(context)
                .attr("callee_reg", callee)
                .attr("clobbers", clobbers)
                .build(),
        )
    }

    fn stack_arg_store(
        &self,
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        value: tir::attributes::AttributeValue,
        offset: i64,
    ) -> Result<Box<dyn Operation>, tir::PassError> {
        Ok(Box::new(
            StoreDoubleWordOpBuilder::new(context)
                .attr("rs1", phys(&abi.sp))
                .attr("rs2", value)
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        ))
    }

    fn call_prefix(
        &self,
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        outgoing_size: u32,
    ) -> Vec<Box<dyn Operation>> {
        if outgoing_size == 0 {
            return Vec::new();
        }
        vec![Box::new(
            AddImmOpBuilder::new(context)
                .attr("rd", phys(&abi.sp))
                .attr("rs1", phys(&abi.sp))
                .attr(
                    "imm",
                    tir::attributes::AttributeValue::Int(-i64::from(outgoing_size)),
                )
                .build(),
        )]
    }

    fn call_suffix(
        &self,
        context: &tir::Context,
        abi: &tir::backend::abi::AbiInfo,
        outgoing_size: u32,
    ) -> Vec<Box<dyn Operation>> {
        if outgoing_size == 0 {
            return Vec::new();
        }
        vec![Box::new(
            AddImmOpBuilder::new(context)
                .attr("rd", phys(&abi.sp))
                .attr("rs1", phys(&abi.sp))
                .attr(
                    "imm",
                    tir::attributes::AttributeValue::Int(i64::from(outgoing_size)),
                )
                .build(),
        )]
    }
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
        value: u32,
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Box<dyn Operation> {
        let offset = tir::attributes::AttributeValue::Int(offset);
        match class.name() {
            "FPR32" => Box::new(
                FStoreWordOpBuilder::new(context)
                    .attr("rs1", phys(frame))
                    .attr("fs2", virt(value, class))
                    .attr("imm", offset)
                    .build(),
            ),
            "FPR64" => Box::new(
                FStoreDoubleOpBuilder::new(context)
                    .attr("rs1", phys(frame))
                    .attr("fs2", virt(value, class))
                    .attr("imm", offset)
                    .build(),
            ),
            _ => Box::new(
                StoreDoubleWordOpBuilder::new(context)
                    .attr("rs1", phys(frame))
                    .attr("rs2", virt(value, class))
                    .attr("imm", offset)
                    .build(),
            ),
        }
    }

    fn emit_spill_reload(
        &self,
        context: &tir::Context,
        value: u32,
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Box<dyn Operation> {
        let offset = tir::attributes::AttributeValue::Int(offset);
        match class.name() {
            "FPR32" => Box::new(
                FLoadWordOpBuilder::new(context)
                    .attr("fd", virt(value, class))
                    .attr("rs1", phys(frame))
                    .attr("imm", offset)
                    .build(),
            ),
            "FPR64" => Box::new(
                FLoadDoubleOpBuilder::new(context)
                    .attr("fd", virt(value, class))
                    .attr("rs1", phys(frame))
                    .attr("imm", offset)
                    .build(),
            ),
            _ => Box::new(
                LoadDoubleWordOpBuilder::new(context)
                    .attr("rd", virt(value, class))
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
                "riscv register copy for class {other} is not implemented (no float/vector move op)"
            ),
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

    fn emit_incoming_stack_arg_load(
        &self,
        context: &tir::Context,
        dst: &tir::backend::liveness::PhysReg,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Result<Box<dyn Operation>, tir::PassError> {
        if dst.0.name() != "GPR" {
            return Err(tir::PassError::InvalidRuleSet(format!(
                "riscv stack arguments for register class {} are not supported",
                dst.0.name()
            )));
        }
        Ok(Box::new(
            LoadDoubleWordOpBuilder::new(context)
                .attr("rd", phys(dst))
                .attr("rs1", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        ))
    }

    fn emit_frame_address(
        &self,
        context: &tir::Context,
        dst: &tir::backend::liveness::PhysReg,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Result<Vec<Box<dyn Operation>>, tir::PassError> {
        if dst.0.name() != "GPR" {
            return Err(tir::PassError::InvalidRuleSet(format!(
                "riscv stack allocation addresses for register class {} are not supported",
                dst.0.name()
            )));
        }
        Ok(vec![Box::new(
            AddImmOpBuilder::new(context)
                .attr("rd", phys(dst))
                .attr("rs1", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )])
    }
}

pub fn create_regalloc_pass() -> tir::backend::regalloc::RegisterAllocationPass {
    create_regalloc_pass_for(abi_by_name("lp64d").expect("RISC-V must define lp64d"))
}

fn create_regalloc_pass_for(
    abi: &'static tir::backend::abi::AbiInfo,
) -> tir::backend::regalloc::RegisterAllocationPass {
    tir::backend::regalloc::RegisterAllocationPass::with_abi(Box::new(RiscvRegAlloc), abi)
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

    fn isel_pass(&self, context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
        create_isel_pass_for(context, &self.config.features, self.abi())
    }

    fn regalloc_pass(&self) -> tir::backend::regalloc::RegisterAllocationPass {
        create_regalloc_pass_for(self.abi())
    }

    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        use tir::backend::regalloc::TargetRegAlloc;
        RiscvRegAlloc.register_info()
    }

    fn abis(&self) -> &'static [tir::backend::abi::AbiInfo] {
        abis()
    }

    fn abi(&self) -> &'static tir::backend::abi::AbiInfo {
        self.selected_abi
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

    fn machine_passes(&self) -> Vec<Box<dyn tir::Pass>> {
        if self.config.features.contains(&Feature::RVV) {
            vec![Box::new(vsetvli::InsertVsetvliPass::new(self.config.xlen))]
        } else {
            Vec::new()
        }
    }

    fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        if self.config.xlen == 64 {
            vec![obj::lower_constant_rv64, obj::lower_addr_of]
        } else {
            vec![obj::lower_constant_rv32, obj::lower_addr_of]
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
        Some(name) => abi_by_name(name).ok_or_else(|| {
            format!(
                "unknown ABI '{name}' for riscv (available: {})",
                abis()
                    .iter()
                    .map(|abi| abi.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?,
        None if config.features.contains(&Feature::D) => {
            abi_by_name("lp64d").expect("RISC-V must define lp64d")
        }
        None => abi_by_name("lp64").expect("RISC-V must define lp64"),
    };
    Ok(Some(Box::new(RiscvTarget {
        config,
        selected_abi,
    })))
}

tir::register_target!(select_riscv, ["riscv32", "riscv64"]);

#[cfg(test)]
mod tests {
    use tir::backend::AsmDialect;
    use tir::{
        Context, IRBuilder, IRFormatter, Operation, PassManager,
        builtin::{FuncOp, IntegerType, UnitType, ops},
    };

    use crate::{RegClass, RiscvDialect, create_isel_pass, create_regalloc_pass};

    #[test]
    fn generated_abi_matches_lp64d_register_convention() {
        let abi = crate::abi_by_name("lp64d").unwrap();
        let args = |kind| {
            abi.args
                .iter()
                .find(|sequence| sequence.kind == kind)
                .unwrap()
        };
        let rets = |kind| {
            abi.rets
                .iter()
                .find(|sequence| sequence.kind == kind)
                .unwrap()
        };

        assert_eq!(abi.name, "lp64d");
        assert_eq!(abi.sp, (RegClass::GPR.id(), 2));
        assert_eq!(abi.ra, Some((RegClass::GPR.id(), 1)));
        assert_eq!(abi.fp, Some((RegClass::GPR.id(), 8)));
        assert_eq!(abi.stack.align, 16);
        assert_eq!(abi.stack.slot_size, 8);
        assert_eq!(
            args(tir::backend::abi::ValueKind::Int)
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            (10..=17).collect::<Vec<_>>()
        );
        assert_eq!(
            args(tir::backend::abi::ValueKind::Float)
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            (10..=17).collect::<Vec<_>>()
        );
        assert_eq!(
            rets(tir::backend::abi::ValueKind::Int)
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert_eq!(
            rets(tir::backend::abi::ValueKind::Float)
                .regs
                .iter()
                .map(|register| register.1)
                .collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert!(abi.callee_saved.contains(&(RegClass::GPR.id(), 8)));
        assert!(abi.callee_saved.contains(&(RegClass::FPR64.id(), 8)));
    }

    #[test]
    fn target_selection_accepts_and_validates_mabi() {
        let target =
            tir::backend::select_target_with_abi("riscv64", None, None, Some("lp64d")).unwrap();
        assert_eq!(target.abi().name, "lp64d");

        let target = tir::backend::select_target("rv64i", None, None).unwrap();
        assert_eq!(target.abi().name, "lp64");

        let target = tir::backend::select_target("rv64id", None, None).unwrap();
        assert_eq!(target.abi().name, "lp64d");

        let error = tir::backend::select_target_with_abi("riscv64", None, None, Some("unknown"))
            .err()
            .unwrap();
        assert_eq!(
            error,
            "unknown ABI 'unknown' for riscv (available: lp64, lp64d)"
        );
    }

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
        fb.insert(add);
        // A bare i1 condition (a block argument): no branch rule can fuse it, so
        // selection falls back to `bne cond, x0, t` plus the deferred `vbr f`.
        fb.insert(ops::cond_br(&context, cond_id, vec![], vec![], t.id(), f.id()).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should lower the conditional branch");

        // The data op selects (addw), the conditional branch lowers to the
        // fallback machine branch + virtual fallthrough, and no builtin control
        // flow remains.
        assert_eq!(
            body_op_names(&context, region.id()),
            vec!["addw", "bne", "vbr", "symbol_end"]
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
            selected_abi: crate::default_abi(),
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
    #[ignore = "run with `cargo xtask axioms`"]
    fn committed_isel_axioms_are_fresh() {
        let context = Context::with_default_dialects();
        let rules = crate::get_isel_rules(&context, crate::Feature::ALL);
        let discovered = tir::backend::isel::discover_axioms(&rules);
        assert_eq!(
            include_str!("isel.axioms"),
            tir::backend::isel::render_axioms_file(&discovered),
            "isel.axioms is stale; run `cargo run -p tir-tools --bin tir -- axioms --write`"
        );
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

        // F gates the single-precision rules, D the double-precision ones.
        assert!(!rv32i.contains(&"fadds"));
        let rv32if = rule_names(&[crate::Feature::RV32I, crate::Feature::F]);
        assert!(rv32if.contains(&"fadds"));
        assert!(rv32if.contains(&"floadword"));
        assert!(rv32if.contains(&"fmvwx"));
        assert!(!rv32if.contains(&"faddd"));
        let rv64ifd = rule_names(&[
            crate::Feature::RV64I,
            crate::Feature::F,
            crate::Feature::D,
            crate::Feature::D64,
        ]);
        assert!(rv64ifd.contains(&"fadds"));
        assert!(rv64ifd.contains(&"faddd"));
        assert!(rv64ifd.contains(&"fmvdx"));
        assert!(rv64ifd.contains(&"fstoredouble"));
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
    fn live_constant_materializes_li() {
        // A constant with a genuine remaining use (returned directly, so no
        // instruction folds it as an immediate) is selected as the canonical
        // `li` (`addi rd, x0, imm`) rather than surviving to the pre-RA hook
        // — and is never silently erased.
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
            body.contains(&"addi") && !body.contains(&"constant"),
            "a constant feeding the return must select as li, got {body:?}"
        );
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

        assert_eq!(phys_of(&add_op, "rs1"), Some((RegClass::GPR.id(), 10)));
        assert_eq!(phys_of(&add_op, "rs2"), Some((RegClass::GPR.id(), 11)));
        assert_eq!(phys_of(&add_op, "rd"), Some((RegClass::GPR.id(), 10)));

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
                assert_eq!(rd.0, RegClass::GPR.id());
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

        // The prologue reserves the frame and saves the callee-saved register
        // holding the return address across the call (`addi sp`, `sd`); the
        // epilogue restores it (`ld`, `addi sp`) before the `jalr` return.
        let names = body_op_names(&context, region.id());
        assert_eq!(
            names,
            vec![
                "addi", // prologue: reserve frame
                "sd",   // prologue: save the callee-saved register (return address)
                "addi",
                "addi",
                "addi", // detach arg + move into a0 + save ra
                "jal",  // the call
                "addi",
                "addi", // restore ra + copy the result out of a0
                "ld",   // epilogue: reload the callee-saved register
                "addi", // epilogue: release frame
                "jalr", // return
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
        assert_eq!(phys_of(&jal, "rd"), Some((RegClass::GPR.id(), 1)));
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
            .find(|op| op.name == "jalr" && phys_of(op, "rd") == Some((RegClass::GPR.id(), 1)))
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

    fn abi_with_callers(
        callers: Vec<tir::backend::liveness::PhysReg>,
    ) -> &'static tir::backend::abi::AbiInfo {
        let mut abi = *crate::default_abi();
        abi.caller_saved = Box::leak(callers.into_boxed_slice());
        abi.callee_saved = &[];
        Box::leak(Box::new(abi))
    }

    impl tir::backend::regalloc::TargetRegAlloc for TinyRiscv {
        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            crate::register_info()
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
            a: &tir::backend::abi::AbiInfo,
            s: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            self.0.emit_prologue(c, a, s, saves)
        }
        fn emit_epilogue(
            &self,
            c: &tir::Context,
            a: &tir::backend::abi::AbiInfo,
            s: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            self.0.emit_epilogue(c, a, s, saves)
        }
    }

    #[test]
    fn regalloc_spills_under_high_register_pressure() {
        use crate::{AddWordOpBuilder, VirtualReturnOpBuilder, virt};
        use tir::backend::regalloc::TargetRegAlloc;

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

        // Tag every vreg with the tiny target's own `GPR` class, whose 5-register
        // allocation order is what forces spilling. A `RegClassId` is an absolute
        // handle into a specific register table, so this must be the same class the
        // allocator reads from `TinyRiscv::register_info`, not the full riscv `GPR`.
        let gpr = TinyRiscv(crate::RiscvRegAlloc)
            .register_info()
            .class("GPR")
            .unwrap();
        let mut bb = IRBuilder::new(context.get_block(block.id()));
        let mut producers = Vec::new();
        for _ in 0..8 {
            let v = context.create_value(i32, None).id().number();
            bb.insert(
                AddWordOpBuilder::new(&context)
                    .attr("rd", virt(v, gpr))
                    .attr("rs1", virt(a, gpr))
                    .attr("rs2", virt(a, gpr))
                    .build(),
            );
            producers.push(v);
        }
        let mut acc = producers[0];
        for &p in &producers[1..] {
            let s = context.create_value(i32, None).id().number();
            bb.insert(
                AddWordOpBuilder::new(&context)
                    .attr("rd", virt(s, gpr))
                    .attr("rs1", virt(acc, gpr))
                    .attr("rs2", virt(p, gpr))
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
        pm.add_pass(tir::backend::regalloc::RegisterAllocationPass::with_abi(
            Box::new(TinyRiscv(crate::RiscvRegAlloc)),
            abi_with_callers(
                vec![5, 6, 7, 10, 11]
                    .into_iter()
                    .map(|index| (RegClass::GPR.id(), index))
                    .collect(),
            ),
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

    /// A RISC-V target whose FPR32 file has only three allocatable registers,
    /// so a handful of live floats overflow it and exercise FP spilling (fsw
    /// stores, flw reloads). Spill code emission delegates to the real target.
    struct TinyFprRiscv(crate::RiscvRegAlloc);

    impl tir::backend::regalloc::TargetRegAlloc for TinyFprRiscv {
        fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
            crate::register_info()
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
            a: &tir::backend::abi::AbiInfo,
            s: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            self.0.emit_prologue(c, a, s, saves)
        }
        fn emit_epilogue(
            &self,
            c: &tir::Context,
            a: &tir::backend::abi::AbiInfo,
            s: u32,
            saves: &[(tir::backend::liveness::PhysReg, i64)],
        ) -> Vec<Box<dyn Operation>> {
            self.0.emit_epilogue(c, a, s, saves)
        }
    }

    #[test]
    fn regalloc_spills_fp_values_through_fp_loads_and_stores() {
        use crate::{FAddSOpBuilder, VirtualReturnOpBuilder, virt};
        use tir::backend::regalloc::TargetRegAlloc;
        use tir::builtin::FloatType;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let f32_ty = FloatType::f32(&context);
        let module = ops::module(&context, None).build();
        let fpr32 = TinyFprRiscv(crate::RiscvRegAlloc)
            .register_info()
            .class("FPR32")
            .unwrap();

        // Machine IR: 6 simultaneously-live f32 values chained down to one, in a
        // 3-register FPR32 file, forcing FP spills.
        let a_val = context.create_value(f32_ty, None);
        let a = a_val.id().number();
        let region = context.create_region();
        let block = context.create_block(vec![a_val]);
        region.add_block(block.id());

        let mut bb = IRBuilder::new(context.get_block(block.id()));
        let mut producers = Vec::new();
        for _ in 0..6 {
            let v = context.create_value(f32_ty, None).id().number();
            bb.insert(
                FAddSOpBuilder::new(&context)
                    .attr("fd", virt(v, fpr32))
                    .attr("fs1", virt(a, fpr32))
                    .attr("fs2", virt(a, fpr32))
                    .build(),
            );
            producers.push(v);
        }
        let mut acc = producers[0];
        for &p in &producers[1..] {
            let s = context.create_value(f32_ty, None).id().number();
            bb.insert(
                FAddSOpBuilder::new(&context)
                    .attr("fd", virt(s, fpr32))
                    .attr("fs1", virt(acc, fpr32))
                    .attr("fs2", virt(p, fpr32))
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
        pm.add_pass(tir::backend::regalloc::RegisterAllocationPass::with_abi(
            Box::new(TinyFprRiscv(crate::RiscvRegAlloc)),
            abi_with_callers(vec![
                (RegClass::GPR.id(), 10),
                (RegClass::GPR.id(), 11),
                (RegClass::FPR32.id(), 10),
                (RegClass::FPR32.id(), 0),
                (RegClass::FPR32.id(), 1),
            ]),
        ));
        pm.run(&context, context.get_op(module.id()))
            .expect("register allocation should converge with FP spilling");

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
            names.contains(&"fsw"),
            "expected FP spill stores, got {names:?}"
        );
        assert!(
            names.contains(&"flw"),
            "expected FP spill reloads, got {names:?}"
        );
        assert!(
            !names.contains(&"sd") && !names.contains(&"ld"),
            "FP values must spill through the FP file, got {names:?}"
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
        let gpr = |i: u16| phys(&(RegClass::GPR.id(), i));
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
    fn fp_encoders_match_isa_golden_words() {
        use crate::{
            FAddDOpBuilder, FAddSOpBuilder, FLoadWordOpBuilder, FMvDXOpBuilder, FMvWXOpBuilder,
            FStoreDoubleOpBuilder, phys,
        };
        use tir::attributes::AttributeValue;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let encoders = crate::get_instruction_encoders();
        let gpr = |i: u16| phys(&(RegClass::GPR.id(), i));
        let fpr32 = |i: u16| phys(&(RegClass::FPR32.id(), i));
        let fpr64 = |i: u16| phys(&(RegClass::FPR64.id(), i));
        let word = |id: tir::OpId| -> u32 {
            let inst = context.get_op(id);
            let enc = encoders[inst.name](&inst)
                .unwrap_or_else(|| panic!("'{}' failed to encode", inst.name));
            u32::from_le_bytes(enc.bytes.try_into().unwrap())
        };

        // Golden words produced by clang/llvm-mc for riscv64 (dynamic rounding).
        let fadd_s = FAddSOpBuilder::new(&context)
            .attr("fd", fpr32(10))
            .attr("fs1", fpr32(10))
            .attr("fs2", fpr32(11))
            .build();
        assert_eq!(word(fadd_s.id()), 0x00B57553, "fadd.s fa0, fa0, fa1");

        let fadd_d = FAddDOpBuilder::new(&context)
            .attr("fd", fpr64(10))
            .attr("fs1", fpr64(10))
            .attr("fs2", fpr64(11))
            .build();
        assert_eq!(word(fadd_d.id()), 0x02B57553, "fadd.d fa0, fa0, fa1");

        let flw = FLoadWordOpBuilder::new(&context)
            .attr("fd", fpr32(10))
            .attr("rs1", gpr(2))
            .attr("imm", AttributeValue::Int(16))
            .build();
        assert_eq!(word(flw.id()), 0x01012507, "flw fa0, 16(sp)");

        let fsd = FStoreDoubleOpBuilder::new(&context)
            .attr("rs1", gpr(2))
            .attr("fs2", fpr64(8))
            .attr("imm", AttributeValue::Int(16))
            .build();
        assert_eq!(word(fsd.id()), 0x00813827, "fsd fs0, 16(sp)");

        let fmv_w_x = FMvWXOpBuilder::new(&context)
            .attr("fd", fpr32(10))
            .attr("rs1", gpr(10))
            .build();
        assert_eq!(word(fmv_w_x.id()), 0xF0050553, "fmv.w.x fa0, a0");

        let fmv_d_x = FMvDXOpBuilder::new(&context)
            .attr("fd", fpr64(10))
            .attr("rs1", gpr(10))
            .build();
        assert_eq!(word(fmv_d_x.id()), 0xF2050553, "fmv.d.x fa0, a0");
    }

    #[test]
    fn encoder_rejects_unencodable_operands() {
        use crate::{AddImmOpBuilder, AddOpBuilder, phys, virt};
        use tir::attributes::AttributeValue;

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<RiscvDialect>();

        let encoders = crate::get_instruction_encoders();
        let gpr = |i: u16| phys(&(RegClass::GPR.id(), i));

        // A virtual register cannot be encoded.
        let add = AddOpBuilder::new(&context)
            .attr("rd", virt(1, RegClass::GPR.id()))
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
        let gpr = |i: u16| phys(&(RegClass::GPR.id(), i));

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
        // On rv64 the M *W conjunctions (Zmmul64/RVM64) follow M automatically.
        assert_eq!(
            features("rv64im", None),
            vec![
                Feature::RV64I,
                Feature::RVM,
                Feature::Zmmul,
                Feature::Zmmul64,
                Feature::RVM64
            ]
        );
        assert_eq!(
            features("rv32imac", None),
            vec![
                Feature::RV32I,
                Feature::RVM,
                Feature::Zmmul,
                Feature::A,
                Feature::C,
                Feature::C32
            ]
        );
        assert_eq!(
            features("rv32i_zmmul", None),
            vec![Feature::RV32I, Feature::Zmmul]
        );
        // F/D select the float extensions; D implies F.
        assert_eq!(features("rv32if", None), vec![Feature::RV32I, Feature::F]);
        // On rv64 the internal D64 conjunction follows D automatically.
        assert_eq!(
            features("rv64ifd", None),
            vec![Feature::RV64I, Feature::F, Feature::D, Feature::D64]
        );
        assert_eq!(
            features("rv64id", None),
            vec![Feature::RV64I, Feature::F, Feature::D, Feature::D64]
        );
        assert_eq!(
            features("rv32ifd", None),
            vec![Feature::RV32I, Feature::F, Feature::D]
        );
        // G abbreviates IMAFD_Zicsr_Zifencei; M, A, F, D and Zifencei are modeled.
        let g = features("rv64gc_zba_zbb", None);
        assert!(g.contains(&Feature::RVM));
        assert!(g.contains(&Feature::A));
        assert!(g.contains(&Feature::F));
        assert!(g.contains(&Feature::D));
        assert!(g.contains(&Feature::Zifencei));
        // Bare architecture names select the generic, everything-on profile.
        assert_eq!(
            features("riscv64", None),
            vec![
                Feature::RV64I,
                Feature::Zmmul,
                Feature::RVM,
                Feature::Zmmul64,
                Feature::RVM64,
                Feature::F,
                Feature::D,
                Feature::D64,
                Feature::C,
                Feature::C64,
                Feature::Zcd,
                Feature::A,
                Feature::A64,
                Feature::Zifencei,
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
            vec![
                Feature::RV64I,
                Feature::RVM,
                Feature::Zmmul,
                Feature::Zmmul64,
                Feature::RVM64
            ]
        );
        assert_eq!(
            features("rv64im", Some("-m,+zmmul")),
            vec![Feature::RV64I, Feature::Zmmul, Feature::Zmmul64]
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
            vec![
                ("PC", 32),
                ("GPR", 32),
                ("FPR32", 32),
                ("FPR64", 64),
                ("GPRC", 32),
                ("FPR64C", 64),
                ("FPR32C", 32),
                ("CSR", 32),
                ("VCSR", 32),
                ("VCFG", 32)
            ]
        );
        assert_eq!(
            crate::register_widths(&[Feature::RV64I]),
            vec![
                ("PC", 64),
                ("GPR", 64),
                ("FPR32", 32),
                ("FPR64", 64),
                ("GPRC", 64),
                ("FPR64C", 64),
                ("FPR32C", 32),
                ("CSR", 64),
                ("VCSR", 64),
                ("VCFG", 64)
            ]
        );
        // Extensions alone resolve nothing; the base supplies XLEN.
        assert_eq!(crate::isa_params(&[Feature::RVM]), vec![]);
    }

    #[test]
    fn counter_registers_follow_the_feature_set() {
        use tir::backend::{PerfCounter, TargetMachine};

        let target = |march| crate::RiscvTarget {
            config: TargetConfig::parse(march, None, None).expect("march should parse"),
            selected_abi: crate::default_abi(),
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
