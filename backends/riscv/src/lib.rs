use tir::backend::normalize_name;
use tir::helpers::dialect;

include!(concat!(env!("OUT_DIR"), "/model_check_sources.rs"));

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
            tir::backend::apply_mattr(&mut config.features, mattr, "RISC-V", attr_features)?;
        }
        if config.features.contains(&Feature::F) && !config.features.contains(&Feature::Zicsr) {
            config.features.push(Feature::Zicsr);
        }
        if config.xlen == 64
            && config.features.contains(&Feature::F)
            && !config.features.contains(&Feature::F64)
        {
            config.features.push(Feature::F64);
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
        for (feature, base, xlen) in [
            (Feature::Zba64, Feature::Zba, 64),
            (Feature::Zbb32, Feature::Zbb, 32),
            (Feature::Zbb64, Feature::Zbb, 64),
        ] {
            config.features.retain(|enabled| *enabled != feature);
            if config.xlen == xlen && config.features.contains(&base) {
                config.features.push(feature);
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
                Feature::RV32I | Feature::C32 | Feature::Zcf | Feature::Zbb32 => xlen == 32,
                Feature::RV64I
                | Feature::F64
                | Feature::D64
                | Feature::C64
                | Feature::Zmmul64
                | Feature::RVM64
                | Feature::A64
                | Feature::Zba64
                | Feature::Zbb64 => xlen == 64,
                _ => true,
            })
            .collect();
        config
    }
}

fn parse_march(march: &str) -> Result<TargetConfig, String> {
    let march = normalize_name(march);
    match march.as_str() {
        // Bare architecture names select the generic profile with every
        // modeled extension, mirroring how toolchains treat a bare triple.
        "riscv" | "riscv64" => Ok(TargetConfig::generic(64)),
        "riscv32" => Ok(TargetConfig::generic(32)),
        _ => parse_riscv_isa_string(&march),
    }
}

fn parse_mcpu(mcpu: &str, config: &TargetConfig) -> Result<Option<String>, String> {
    let mcpu = normalize_name(mcpu);
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

fn attr_features(name: &str) -> Option<Vec<Feature>> {
    let name = normalize_name(name);
    match name.as_str() {
        // The M extension implies Zmmul.
        "m" => Some(vec![Feature::RVM, Feature::Zmmul]),
        // The D extension implies F.
        "d" => Some(vec![Feature::D, Feature::F]),
        _ => Feature::from_name(&name).map(|f| vec![f]),
    }
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

/// The RISC-V target, selected via `--march`/`--mcpu`.
pub struct RiscvTarget {
    config: TargetConfig,
    selected_abi: &'static tir::backend::abi::AbiInfo,
}

tir::impl_target_machine! {
    RiscvTarget,
    dialect: RiscvDialect,
    isa: |config: &TargetConfig| if config.xlen == 32 { "RV32I" } else { "RV64I" },
    pointer_bits: |config: &TargetConfig| config.xlen,
    abis: riscv_abis,
    sources: MODEL_CHECK_SOURCES,

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
        .any(|prefix| normalize_name(march).starts_with(prefix));
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

tir::target_abis!(riscv_abis, riscv_abi_by_name, |abi| {
    tir::backend::abi::AbiInfo {
        indirect_result: Some((RegClass::GPR.id(), 10)),
        ..*abi
    }
});
