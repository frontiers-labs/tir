use tir::Operation;
use tir::backend::RegSlot;
use tir::helpers::{dialect, operation};

const MODEL_CHECK_SOURCES: &[(&str, &str)] = &[
    ("main.tmdl", include_str!("../defs/main.tmdl")),
    ("versions.tmdl", include_str!("../defs/versions.tmdl")),
    ("float.tmdl", include_str!("../defs/float.tmdl")),
    (
        "data_processing.tmdl",
        include_str!("../defs/data_processing.tmdl"),
    ),
    (
        "loads_stores.tmdl",
        include_str!("../defs/loads_stores.tmdl"),
    ),
    ("atomics.tmdl", include_str!("../defs/atomics.tmdl")),
    ("branches.tmdl", include_str!("../defs/branches.tmdl")),
    ("perf.tmdl", include_str!("../defs/perf.tmdl")),
];

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
        let mut config = TargetConfig {
            features: parse_march(march)?,
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

fn parse_march(march: &str) -> Result<Vec<Feature>, String> {
    let march = normalize(march);
    let (major, minor) = match march.as_str() {
        "arm64" | "aarch64" | "armv8" | "armv8a" | "armv8-a" => (8, 0),
        "armv9" | "armv9a" | "armv9-a" => (9, 0),
        _ => parse_arch_version(&march)
            .ok_or_else(|| format!("unknown AArch64 architecture '{march}'"))?,
    };

    match (major, minor) {
        (8, 0..=9) => Ok(armv8_features(minor)),
        (9, 0..=6) => Ok(armv9_features(minor)),
        _ => Err(format!("unknown AArch64 architecture '{march}'")),
    }
}

fn parse_arch_version(march: &str) -> Option<(u8, usize)> {
    let version = march.strip_prefix("armv")?;
    let version = version
        .strip_suffix("-a")
        .or_else(|| version.strip_suffix('a'))
        .unwrap_or(version);
    let (major, minor) = version.split_once('.')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

fn armv8_features(revision: usize) -> Vec<Feature> {
    const REVISIONS: &[Feature] = &[
        Feature::ARMv8A64,
        Feature::ARMv8_1A64,
        Feature::ARMv8_2A64,
        Feature::ARMv8_3A64,
        Feature::ARMv8_4A64,
        Feature::ARMv8_5A64,
        Feature::ARMv8_6A64,
        Feature::ARMv8_7A64,
        Feature::ARMv8_8A64,
        Feature::ARMv8_9A64,
    ];
    let mut features = REVISIONS[..=revision].to_vec();
    features.extend([Feature::FP, Feature::AdvSIMD]);
    if revision >= 1 {
        features.push(Feature::LSE);
    }
    features
}

fn armv9_features(revision: usize) -> Vec<Feature> {
    const REVISIONS: &[Feature] = &[
        Feature::ARMv9A64,
        Feature::ARMv9_1A64,
        Feature::ARMv9_2A64,
        Feature::ARMv9_3A64,
        Feature::ARMv9_4A64,
        Feature::ARMv9_5A64,
        Feature::ARMv9_6A64,
    ];
    let mut features = armv8_features((5 + revision).min(9));
    features.extend_from_slice(&REVISIONS[..=revision]);
    features
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

dialect! {
    Arm64Dialect {
        name: "arm64",
        operation_file: concat!(env!("OUT_DIR"), "/arm64_ops.rs"),
        type_parsers: reg_class_type_parsers(),
    }
}

fn lower_func_and_return_to_asm_symbol(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    tir::backend::lower::lower_function_and_return(context, op, rewriter, |ty| {
        let data = context.get_type_data(ty);
        let data = data.as_ref() as &dyn std::any::Any;
        if data
            .downcast_ref::<tir::builtin::FloatType>()
            .is_some_and(|float| float.bit_width() == 64)
        {
            Ok(RegClass::FPR64.id())
        } else {
            Ok(RegClass::GPR.id())
        }
    })
}

impl Arm64Dialect {
    pub fn get_asm_parser(&self) -> tir::backend::AsmParser {
        tir::backend::AsmParser::new(get_instruction_parsers(Feature::ALL).0)
    }
}

/// Emit the deferred unconditional branch (`vbr`, finalized to `b` after
/// register allocation), forwarding any block arguments.
/// Emit the branch-if-nonzero fallback for a condition no branch rule fused:
/// `and bit, cond, #1` + `cbnz bit, dest`. The condition is a width-1 value,
/// so only bit 0 of its register is defined — comparing the whole register
/// would branch on undefined bits. `immr = imms = 0` encodes the mask `#1`.
fn emit_branch_nonzero(
    context: &tir::Context,
    condition: tir::ValueId,
    dest: tir::BlockId,
) -> Vec<Box<dyn Operation>> {
    let bit = context.create_value(gpr_ty(context), None).id();
    vec![
        Box::new(
            AndImmediateOpBuilder::new(context)
                .result_values(vec![bit])
                .rn(condition)
                .attr("immr", tir::attributes::AttributeValue::Int(0))
                .attr("imms", tir::attributes::AttributeValue::Int(0))
                .build(),
        ),
        Box::new(
            CompareBranchNonZeroOpBuilder::new(context)
                .rt(bit)
                .attr("imm", tir::attributes::AttributeValue::Block(dest))
                .build(),
        ),
    ]
}

/// The AArch64 zero register (`xzr` = slot 31).
const XZR: u16 = 31;

/// Build a register-register move (`orr rd, xzr, rm`).
fn mv(context: &tir::Context, rd: RegSlot, rm: RegSlot) -> Box<dyn Operation> {
    let builder = OrOpBuilder::new(context).attr("rn", phys(&(RegClass::GPR.id(), XZR)));
    let builder = tir::reg_use!(builder, rm, rm);
    Box::new(tir::reg_def!(builder, rd, rd).build())
}

/// The type of a value living in a `GPR`.
fn gpr_ty(context: &tir::Context) -> tir::TypeId {
    tir::backend::RegClassType::new(context, RegClass::GPR.id())
}

pub fn create_isel_pass(context: &tir::Context) -> tir::backend::isel::InstructionSelectPass {
    create_isel_pass_for(context, Feature::ALL, arm64_default_abi())
}

fn create_isel_pass_for(
    context: &tir::Context,
    features: &[Feature],
    abi: &'static tir::backend::abi::AbiInfo,
) -> tir::backend::isel::InstructionSelectPass {
    tir::backend::isel::InstructionSelectPass::new(get_isel_rules(context, features))
        .with_rules(include_str!("isel-materialize.pdl"))
        .with_branch_emitters(tir::backend::isel::BranchEmitters {
            uncond: tir::backend::emit_uncond_branch,
            cond_nonzero: emit_branch_nonzero,
        })
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
        .with_call_lowering(abi, Box::new(Arm64CallEmitter))
}

struct Arm64CallEmitter;

impl tir::backend::call_lowering::CallEmitter for Arm64CallEmitter {
    fn copy(&self, context: &tir::Context, dst: RegSlot, src: RegSlot) -> Box<dyn Operation> {
        let class = tir::backend::slot_class(context, dst);
        if class == Some(RegClass::FPR64.id()) {
            let builder = FMoveRegisterDoubleOpBuilder::new(context);
            let builder = tir::reg_use!(builder, fa, src);
            Box::new(tir::reg_def!(builder, fd, dst).build())
        } else {
            mv(context, dst, src)
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
                StoreFloatDoubleOpBuilder::new(context)
                    .ft(value)
                    .attr("rn", phys(&abi.sp))
                    .attr("imm", offset)
                    .build(),
            ))
        } else {
            Ok(Box::new(
                StoreDoublewordOpBuilder::new(context)
                    .rt(value)
                    .attr("rn", phys(&abi.sp))
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

/// AArch64 register allocation target: the generated register file plus `str`/`ldr`
/// spill code and a `sub sp, sp, #frame` / `add sp, sp, #frame` prologue/epilogue.
pub struct Arm64RegAlloc;

impl tir::backend::regalloc::TargetRegAlloc for Arm64RegAlloc {
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
        if class == RegClass::FPR64.id() {
            return Box::new(
                StoreFloatDoubleOpBuilder::new(context)
                    .ft(value)
                    .attr("rn", phys(frame))
                    .attr("imm", offset)
                    .build(),
            );
        }
        Box::new(
            StoreDoublewordOpBuilder::new(context)
                .rt(value)
                .attr("rn", phys(frame))
                .attr("imm", offset)
                .build(),
        )
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
        if class == RegClass::FPR64.id() {
            return Box::new(
                LoadFloatDoubleOpBuilder::new(context)
                    .result_values(vec![value])
                    .attr("rn", phys(frame))
                    .attr("imm", offset)
                    .build(),
            );
        }
        Box::new(
            LoadDoublewordOpBuilder::new(context)
                .result_values(vec![value])
                .attr("rn", phys(frame))
                .attr("imm", offset)
                .build(),
        )
    }

    fn emit_copy(
        &self,
        context: &tir::Context,
        class: tir::backend::regalloc::RegClassId,
        dst: RegSlot,
        src: RegSlot,
    ) -> Box<dyn Operation> {
        match class.name() {
            // The 32-bit classes alias GPR's physical file (same register numbering),
            // and any value only ever written through its 32-bit view already has
            // zeroed upper bits, so a plain 64-bit copy carries it correctly.
            "GPR" | "GPRsp" | "GPR32" | "GPR32sp" => mv(context, dst, src),
            "FPR64" => {
                let builder = FMoveRegisterDoubleOpBuilder::new(context);
                let builder = tir::reg_use!(builder, fa, src);
                Box::new(tir::reg_def!(builder, fd, dst).build())
            }
            other => unimplemented!("arm64 register copy for class {other} is not implemented"),
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
        abi: &tir::backend::abi::AbiInfo,
        size: u32,
        saves: &[(tir::backend::liveness::PhysReg, i64)],
    ) -> Vec<Box<dyn Operation>> {
        let sp = abi.sp;
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

    fn emit_frame_address(
        &self,
        context: &tir::Context,
        dst: tir::ValueId,
        class: tir::backend::regalloc::RegClassId,
        frame: &tir::backend::liveness::PhysReg,
        offset: i64,
    ) -> Result<Vec<Box<dyn Operation>>, tir::PassError> {
        if !matches!(class.name(), "GPR" | "GPRsp") {
            return Err(tir::PassError::InvalidRuleSet(format!(
                "arm64 stack allocation addresses for register class {} are not supported",
                class.name()
            )));
        }
        Ok(vec![Box::new(
            AddImmediateOpBuilder::new(context)
                .result_values(vec![dst])
                .attr("rn", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )])
    }
}

pub fn create_regalloc_stage() -> Vec<Box<dyn tir::Pass>> {
    tir::backend::prealloc::regalloc_stage_for(|| Box::new(Arm64RegAlloc) as _, arm64_default_abi())
}

/// The AArch64 application-profile target, selected via `--march`/`--mcpu`.
pub struct Arm64Target {
    config: TargetConfig,
    selected_abi: &'static tir::backend::abi::AbiInfo,
}

impl tir::backend::TargetMachine for Arm64Target {
    fn name(&self) -> &'static str {
        self.config.canonical_name()
    }

    fn model_check_target(&self) -> Option<tir::backend::ModelCheckTarget> {
        Some(tir::backend::ModelCheckTarget {
            isa: "ARMv8A64",
            features: self.config.features.iter().map(Feature::name).collect(),
            sources: MODEL_CHECK_SOURCES,
        })
    }

    fn register_dialects(&self, context: &tir::Context) {
        context.register_dialect::<tir::backend::AsmDialect>();
        context.register_dialect::<Arm64Dialect>();
        context.register_reg_classes(register_info().classes);
    }

    fn data_layout(&self) -> Option<tir::attributes::AttributeValue> {
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
                ("p", 64, 64),
            ],
        ))
    }

    fn target_env(&self) -> Option<tir::attributes::AttributeValue> {
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
        Box::new(Arm64RegAlloc)
    }

    fn register_info(&self) -> tir::backend::regalloc::RegisterInfo {
        use tir::backend::regalloc::TargetRegAlloc;
        Arm64RegAlloc.register_info()
    }

    fn abis(&self) -> &'static [tir::backend::abi::AbiInfo] {
        arm64_abis()
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

    fn pre_ra_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        vec![Box::new(obj::lower_sym_addr)]
    }

    fn finalize_lowerings(&self) -> Vec<tir::backend::isel::OpLowering> {
        vec![Box::new(obj::finalize_virtual_ops)]
    }

    fn object_format(&self) -> Option<tir::backend::binary::ObjectFormatInfo> {
        Some(obj::object_format())
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
    mabi: Option<&str>,
) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
    let owned = ["arm", "aarch64"]
        .iter()
        .any(|prefix| normalize(march).starts_with(prefix));
    if !owned {
        return Ok(None);
    }
    let config = TargetConfig::parse(march, mcpu, mattr)?;
    let selected_abi = match mabi {
        Some(name) => arm64_abi_by_name(name).ok_or_else(|| {
            format!(
                "unknown ABI '{name}' for arm64 (available: {})",
                arm64_abis()
                    .iter()
                    .map(|abi| abi.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?,
        None => arm64_default_abi(),
    };
    Ok(Some(Box::new(Arm64Target {
        config,
        selected_abi,
    })))
}

tir::register_target!(select_arm64, ["arm64"]);

fn arm64_abis() -> &'static [tir::backend::abi::AbiInfo] {
    static ABIS: std::sync::OnceLock<Vec<tir::backend::abi::AbiInfo>> = std::sync::OnceLock::new();
    ABIS.get_or_init(|| {
        abis()
            .iter()
            .map(|abi| tir::backend::abi::AbiInfo {
                indirect_result: Some((RegClass::GPR.id(), 8)),
                argument_group_alignment: Some(tir::backend::abi::ArgumentGroupAlignment {
                    kind: tir::backend::abi::ValueKind::Int,
                    minimum_source_alignment: 16,
                    register_multiple: 2,
                }),
                ..*abi
            })
            .collect()
    })
}

fn arm64_default_abi() -> &'static tir::backend::abi::AbiInfo {
    &arm64_abis()[0]
}

fn arm64_abi_by_name(name: &str) -> Option<&'static tir::backend::abi::AbiInfo> {
    arm64_abis().iter().find(|abi| abi.name == name)
}
