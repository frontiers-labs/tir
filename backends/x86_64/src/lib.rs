//! x86-64 backend prototype, generated from the TMDL descriptions in `defs/`.

include!(concat!(env!("OUT_DIR"), "/model_check_sources.rs"));

pub use isa::{Feature, instruction_infos, register_info, register_views, register_widths};
pub use isa::{TargetConfig, X86_64Dialect};

mod isa {
    // Generated code: not everything is used by this asm-focused prototype.
    #![allow(dead_code, unused_variables, unused_mut, clippy::all)]

    use tir::helpers::{dialect, operation};

    include!(concat!(env!("OUT_DIR"), "/x86_64.rs"));

    dialect! {
        X86_64Dialect {
            name: "x86_64",
            operation_file: concat!(env!("OUT_DIR"), "/x86_64_ops.rs"),
            type_parsers: reg_class_type_parsers(),
        }
    }

    fn branch_mnemonic(op: &str) -> &str {
        op.strip_suffix("_legacy").unwrap_or(op)
    }

    fn object_format() -> tir::backend::binary::ObjectFormatInfo {
        const R_X86_64_PC32: u32 = 2;
        const R_X86_64_PLT32: u32 = 4;
        const R_X86_64_64: u32 = 1;
        const R_X86_64_PC8: u32 = 15;
        use tir::backend::binary::{EM_X86_64, ElfClass, ObjectFormatInfo, RelocKind};
        ObjectFormatInfo {
            elf_machine: EM_X86_64,
            elf_class: ElfClass::Elf64,
            elf_flags: 0,
            absolute_reloc: |width| (width == 8).then_some(R_X86_64_64),
            reloc_for: |op| match branch_mnemonic(op) {
                // `call rel32`: the disp32 follows the 1-byte opcode.
                "call" => Some(RelocKind {
                    r_type: R_X86_64_PLT32,
                    addend: -4,
                    field_offset: 1,
                }),
                // `lea r64, [rip + disp32]`: the disp32 follows REX, opcode, ModR/M.
                "lea" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 3,
                }),
                // `jmp rel32` (E9 + disp32): the disp32 follows the 1-byte opcode.
                "jmp" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 1,
                }),
                // `jcc rel32` (0F 8x + disp32): the disp32 follows the 2-byte opcode.
                "je" | "jne" | "jl" | "jge" | "jb" | "jae" | "jle" | "jg" | "jbe" | "ja" | "js"
                | "jns" | "jo" | "jno" | "jp" | "jnp" => Some(RelocKind {
                    r_type: R_X86_64_PC32,
                    addend: -4,
                    field_offset: 2,
                }),
                // Counted transfers take rel8 (`loop [e]xx`, `jrcxz rel8` at 0xE0..0xE3).
                "loop" | "loope" | "loopne" | "jrcxz" => Some(RelocKind {
                    r_type: R_X86_64_PC8,
                    addend: -1,
                    field_offset: 1,
                }),
                _ => None,
            },
            pc_rel_scale: |_| 0,
            // rel32/rel8 displacements are measured from the end of the
            // instruction (RIP points past the branch when the displacement
            // applies).
            pc_rel_from_end: |op| {
                matches!(
                    branch_mnemonic(op),
                    "jmp"
                        | "je"
                        | "jne"
                        | "jl"
                        | "jge"
                        | "jb"
                        | "jae"
                        | "jle"
                        | "jg"
                        | "jbe"
                        | "ja"
                        | "js"
                        | "jns"
                        | "jo"
                        | "jno"
                        | "jp"
                        | "jnp"
                        | "loop"
                        | "loope"
                        | "loopne"
                        | "jrcxz"
                        | "call"
                )
            },
        }
    }

    /// Parsed x86-64 target selection.
    #[derive(Clone, Debug, Eq, PartialEq)]
    pub struct TargetConfig {
        features: Vec<Feature>,
        machine: Option<String>,
    }

    impl TargetConfig {
        /// Parse an x86-64 architecture name.
        pub fn parse(march: &str, mcpu: Option<&str>, mattr: Option<&str>) -> Result<Self, String> {
            match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "x86_64" | "amd64" | "x64" => {}
                other => return Err(format!("unknown x86-64 architecture '{other}'")),
            }
            let mut features = vec![Feature::X86, Feature::X86_64, Feature::SSE, Feature::SSE2];
            if let Some(mattr) = mattr {
                tir::backend::apply_mattr(&mut features, mattr, "x86-64", |name| {
                    Feature::from_name(&name.to_ascii_lowercase().replace('-', "_"))
                        .map(|feature| vec![feature])
                })?;
            }
            validate_features(&features)?;
            if !features.contains(&Feature::X86_64) {
                return Err("--mattr must not disable the base ISA 'X86_64'".to_string());
            }
            let machine = match mcpu {
                None => None,
                Some(mcpu) => resolve_machine(mcpu, &features)?,
            };
            Ok(Self { features, machine })
        }

        /// The enabled ISA set.
        pub fn features(&self) -> &[Feature] {
            &self.features
        }

        /// The target name this configuration selects.
        pub fn canonical_name(&self) -> &'static str {
            "x86_64"
        }
    }

    fn resolve_machine(mcpu: &str, features: &[Feature]) -> Result<Option<String>, String> {
        let name = mcpu.trim().to_ascii_lowercase();
        if name == "generic" {
            return Ok(None);
        }
        if machine_model(&name, features).is_some() {
            return Ok(Some(name));
        }
        Err(format!(
            "unknown x86-64 cpu '{mcpu}' (expected 'generic' or one of: {})",
            machines(Feature::ALL).join(", ")
        ))
    }

    struct X86Target {
        config: TargetConfig,
        selected_abi: &'static tir::backend::abi::AbiInfo,
    }

    tir::impl_target_machine! {
        X86Target,
        dialect: X86_64Dialect,
        isa: |_| "X86_64",
        pointer_bits: |_| 64,
        abis: x86_64_abis,
        sources: super::MODEL_CHECK_SOURCES,

        fn unaligned_scalar_bytes(&self) -> &'static [u32] {
            &[1, 2, 4, 8]
        }

        fn unaligned_copy_bytes(&self) -> &'static [u32] {
            if self.config.features().contains(&Feature::SSE) {
                &[16]
            } else {
                &[]
            }
        }

        fn register_views(&self) -> Vec<(&'static str, tir::backend::regalloc::RegisterView)> {
            register_views(self.config.features())
        }

        fn object_format(&self) -> Option<tir::backend::binary::ObjectFormatInfo> {
            Some(object_format())
        }
    }

    fn select_x86_64(
        march: &str,
        mcpu: Option<&str>,
        mattr: Option<&str>,
        mabi: Option<&str>,
    ) -> Result<Option<Box<dyn tir::backend::TargetMachine>>, String> {
        match march.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "x86_64" | "amd64" | "x64" => {
                let config = TargetConfig::parse(march, mcpu, mattr)?;
                let selected_abi = match mabi {
                    Some(name) => x86_64_abi_by_name(name).ok_or_else(|| {
                        format!(
                            "unknown ABI '{name}' for x86_64 (available: {})",
                            x86_64_abis()
                                .iter()
                                .map(|abi| abi.name)
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })?,
                    None => x86_64_default_abi(),
                };
                Ok(Some(Box::new(X86Target {
                    config,
                    selected_abi,
                })))
            }
            _ => Ok(None),
        }
    }

    tir::register_target!(select_x86_64, ["x86_64"]);

    tir::target_abis!(x86_64_abis, x86_64_abi_by_name, |abi| {
        tir::backend::abi::AbiInfo {
            indirect_result: Some((RegClass::GPR.id(), 7)),
            argument_group_policy: Some(tir::backend::abi::ArgumentGroupPolicy {
                register_limit: Some(2),
                rollback: tir::backend::abi::GroupRollback::Preserve,
            }),
            ..*abi
        }
    });

    fn x86_64_default_abi() -> &'static tir::backend::abi::AbiInfo {
        &x86_64_abis()[0]
    }
}
