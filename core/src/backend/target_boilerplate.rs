//! The parts of a backend that follow from its generated code.
//!
//! Every backend implements [`TargetMachine`](crate::backend::TargetMachine)
//! the same way: forward to the functions rustgen emits under fixed names, and
//! answer a handful of questions only the target can answer. The macro here
//! writes the forwarding half so a backend states only its deltas, and the
//! functions beside it are the target-selection and lowering steps every
//! backend spells identically.

/// The spelling ISA strings and feature names are compared in: trimmed,
/// lowercased, with `_` folded to `-` so `zicsr`, `Zicsr` and `zi_csr` name one
/// feature.
pub fn normalize_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

/// Apply an LLVM-style `--mattr` list (`+feat`/`-feat`, comma-separated) on top
/// of the march-derived feature set.
///
/// `lookup` resolves one spelling to every feature it toggles — a single name
/// may stand for several (RISC-V `m` implies `Zmmul`) — and `arch` names the
/// target in the diagnostics.
pub fn apply_mattr<F: Copy + PartialEq>(
    features: &mut Vec<F>,
    mattr: &str,
    arch: &str,
    lookup: impl Fn(&str) -> Option<Vec<F>>,
) -> Result<(), String> {
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
        let toggled =
            lookup(name).ok_or_else(|| format!("unknown {arch} feature '{name}' in --mattr"))?;
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

/// Define a backend's ABI table: the rustgen-emitted `abis()` with `customize`
/// applied to each entry, memoized, plus a case-insensitive lookup by name.
///
/// The table is a `static` so the entries can be handed out as `'static`, which
/// a generic function cannot do — its statics are shared across every backend
/// that instantiates it.
#[macro_export]
macro_rules! target_abis {
    ($table:ident, $by_name:ident, |$abi:ident| $customize:expr) => {
        fn $table() -> &'static [$crate::backend::abi::AbiInfo] {
            static ABIS: std::sync::OnceLock<Vec<$crate::backend::abi::AbiInfo>> =
                std::sync::OnceLock::new();
            ABIS.get_or_init(|| abis().iter().map(|$abi| $customize).collect())
        }

        fn $by_name(name: &str) -> Option<&'static $crate::backend::abi::AbiInfo> {
            $table()
                .iter()
                .find(|abi| abi.name.eq_ignore_ascii_case(name))
        }
    };
}

/// Implement [`TargetMachine`](crate::backend::TargetMachine) for a target
/// struct holding a `config` and a `selected_abi` field.
///
/// The forwarded half resolves at the expansion site: the backend must have
/// `Feature`, `register_info`, `get_instruction_parsers`, `machine_model`,
/// `machines`, `isa_params`, `register_widths` and `register_name` from
/// rustgen, plus a `create_isel_pass_for(context, features, abi)` of its own,
/// and its `config` must offer `canonical_name()`, `features()` and a `machine`
/// field. `isa`, `pointer_bits` and `regalloc` are closures because a macro
/// cannot hand the caller's expression the `self` it writes. Methods listed
/// after the parameters are spliced into the same impl.
#[macro_export]
macro_rules! impl_target_machine {
    (
        $target:ty,
        dialect: $dialect:ty,
        isa: $isa:expr,
        pointer_bits: $pointer_bits:expr,
        regalloc: $regalloc:expr,
        abis: $abis:path,
        $(asm_parser: $asm_parser:expr,)?
        sources: $sources:expr,
        $($extra:item)*
    ) => {
        impl $crate::backend::TargetMachine for $target {
            fn name(&self) -> &'static str {
                self.config.canonical_name()
            }

            fn model_check_target(&self) -> Option<$crate::backend::ModelCheckTarget> {
                Some($crate::backend::ModelCheckTarget {
                    isa: ($isa)(&self.config),
                    features: self.config.features().iter().map(Feature::name).collect(),
                    sources: $sources,
                })
            }

            fn register_dialects(&self, context: &$crate::Context) {
                context.register_dialect::<$crate::backend::AsmDialect>();
                context.register_dialect::<$dialect>();
                context.register_reg_classes(register_info().classes);
            }

            fn data_layout(&self) -> Option<$crate::attributes::AttributeValue> {
                let pointer = ($pointer_bits)(&self.config);
                Some($crate::data_layout_spec(
                    $crate::Endianness::Little,
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

            fn target_env(&self) -> Option<$crate::attributes::AttributeValue> {
                let features: Vec<String> = self
                    .config
                    .features()
                    .iter()
                    .map(|feature| feature.name().to_ascii_lowercase())
                    .collect();
                Some($crate::target_env_spec(self.name(), &features))
            }

            fn isel_pass(
                &self,
                context: &$crate::Context,
            ) -> $crate::backend::isel::InstructionSelectPass {
                create_isel_pass_for(context, self.config.features(), self.abi())
                    .with_data_layout(self.data_layout())
            }

            fn regalloc_target(&self) -> Box<dyn $crate::backend::regalloc::TargetRegAlloc> {
                Box::new(($regalloc)(self.config.features()))
            }

            fn register_info(&self) -> $crate::backend::regalloc::RegisterInfo {
                use $crate::backend::regalloc::TargetRegAlloc;
                ($regalloc)(self.config.features()).register_info()
            }

            fn abis(&self) -> &'static [$crate::backend::abi::AbiInfo] {
                $abis()
            }

            fn abi(&self) -> &'static $crate::backend::abi::AbiInfo {
                self.selected_abi
            }

            fn asm_parser(&self, _context: &$crate::Context) -> $crate::backend::AsmParser {
                let (parsers, disabled) = get_instruction_parsers(self.config.features());
                let parser = $crate::backend::AsmParser::new(parsers).with_disabled_mnemonics(disabled);
                $(let parser = ($asm_parser)(parser);)?
                parser
            }

            fn machine_model(&self, name: &str) -> Option<$crate::backend::sched::MachineModel> {
                machine_model(name, self.config.features())
            }

            fn machines(&self) -> Vec<&'static str> {
                machines(self.config.features())
            }

            fn default_machine(&self) -> Option<&str> {
                self.config.machine.as_deref()
            }

            fn isa_params(&self) -> Vec<(&'static str, i64)> {
                isa_params(self.config.features())
            }

            fn register_widths(&self) -> Vec<(&'static str, u32)> {
                register_widths(self.config.features())
            }

            fn register_name(&self, class: &str, index: u16, prefer_abi: bool) -> Option<String> {
                register_name(class, index, prefer_abi)
            }

            $($extra)*
        }
    };
}
