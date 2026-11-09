use tir::backend::normalize_name;
use tir::helpers::dialect;

include!(concat!(env!("OUT_DIR"), "/model_check_sources.rs"));

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
            tir::backend::apply_mattr(&mut config.features, mattr, "AArch64", |name| {
                Feature::from_name(&normalize_name(name)).map(|feature| vec![feature])
            })?;
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
    let march = normalize_name(march);
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

fn parse_mcpu(mcpu: &str, config: &TargetConfig) -> Result<Option<String>, String> {
    let name = normalize_name(mcpu);
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

dialect! {
    Arm64Dialect {
        name: "arm64",
        operation_file: concat!(env!("OUT_DIR"), "/arm64_ops.rs"),
        type_parsers: reg_class_type_parsers(),
    }
}

impl Arm64Dialect {
    pub fn get_asm_parser(&self) -> tir::backend::AsmParser {
        tir::backend::AsmParser::new(get_instruction_parsers(Feature::ALL).0)
    }
}

pub struct Arm64Target {
    config: TargetConfig,
    selected_abi: &'static tir::backend::abi::AbiInfo,
}

tir::impl_target_machine! {
    Arm64Target,
    dialect: Arm64Dialect,
    isa: |_| "ARMv8A64",
    pointer_bits: |_| 64,
    abis: arm64_abis,
    sources: MODEL_CHECK_SOURCES,

    fn unaligned_scalar_bytes(&self) -> &'static [u32] {
        &[1, 2, 4, 8]
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
        .any(|prefix| normalize_name(march).starts_with(prefix));
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

tir::target_abis!(arm64_abis, arm64_abi_by_name, |abi| {
    tir::backend::abi::AbiInfo {
        indirect_result: Some((RegClass::GPR.id(), 8)),
        argument_group_alignment: Some(tir::backend::abi::ArgumentGroupAlignment {
            kind: tir::backend::abi::ValueKind::Int,
            minimum_source_alignment: 16,
            register_multiple: 2,
        }),
        ..*abi
    }
});

fn arm64_default_abi() -> &'static tir::backend::abi::AbiInfo {
    &arm64_abis()[0]
}
