//! Target abstraction shared by the codegen and simulation tools.
//!
//! A [`TargetMachine`] bundles everything a backend exposes behind a single
//! object: dialect registration, the instruction-selection and register
//! allocation passes, an assembly parser and the cycle-approximate machine
//! models. Each backend crate implements this trait and registers itself with
//! the [`TARGETS`] registry via `register_target!`; the `tir-targets` crate
//! links the backends and re-exports the registry's lookup helpers.

use linkme::distributed_slice;
use tir::Context;

use crate::binary::{BinaryWriter, ObjectFormatInfo};
use crate::isel::{InstructionSelectPass, OpLowering};
use crate::regalloc::{RegisterAllocationPass, RegisterInfo};
use crate::sched::MachineModel;
use crate::{AsmParser, AsmPrinter};

/// A selectable code-generation / simulation target.
///
/// Tools obtain one of these from a registry keyed by `--march`/`--mcpu` and
/// drive it uniformly, so adding a backend is a matter of implementing this
/// trait and registering it.
pub trait TargetMachine {
    /// Canonical target name (e.g. `riscv64`, `arm64`).
    fn name(&self) -> &'static str;

    /// Register the dialects this target needs into `context`.
    ///
    /// Implementations must register the shared `asm` dialect alongside their
    /// own machine dialect so parsing and lowering have everything they need.
    fn register_dialects(&self, context: &Context);

    /// The instruction-selection pass, nested under each function.
    fn isel_pass(&self, context: &Context) -> InstructionSelectPass;

    /// The register-allocation pass, run module-wide after instruction
    /// selection.
    fn regalloc_pass(&self) -> RegisterAllocationPass;

    /// The target's register file description. Beyond register allocation this
    /// also tells the simulator which register classes share a physical file
    /// (e.g. AArch64 `GPR`/`GPRsp`), so a value written through one class is
    /// visible through the other.
    fn register_info(&self) -> RegisterInfo;

    /// An assembly parser for this target's textual `.s`/`.S` syntax.
    fn asm_parser(&self, context: &Context) -> AsmParser;

    /// An assembly printer for this target's textual `.s`/`.S` syntax.
    fn asm_printer(&self, context: &Context) -> AsmPrinter;

    /// A cycle-approximate machine model by name, or `None` if this target has no
    /// model under that name compatible with the selected features. Names are
    /// globally unique (e.g. `rv64-ooo`).
    fn machine_model(&self, name: &str) -> Option<MachineModel>;

    /// The selectable machine names compatible with the selected features (for
    /// help text / diagnostics).
    fn machines(&self) -> Vec<&'static str>;

    /// The machine implied by `--mcpu`, used as the default model when a tool
    /// needs one and no explicit machine was selected.
    fn default_machine(&self) -> Option<&str> {
        None
    }

    /// TMDL ISA parameter values (e.g. RISC-V `XLEN`) resolved from the selected
    /// features. Simulators install these so instruction behaviors referencing
    /// `self.PARAM` execute with the selected ISA's value.
    fn isa_params(&self) -> Vec<(&'static str, i64)>;

    /// Architectural width in bits of each register class under the selected
    /// features (e.g. RISC-V `GPR` is 32 bits wide on rv32, 64 on rv64).
    fn register_widths(&self) -> Vec<(&'static str, u32)>;

    /// The ISA (or ABI, when `prefer_abi`) name of a register given its class and
    /// encoding index — the inverse of the asm parser, for printing `x1`/`ra`
    /// instead of the raw `(class, index)`. `None` if the class/index is unknown.
    fn register_name(&self, class: &str, index: u16, prefer_abi: bool) -> Option<String>;

    /// Registers backed by hardware performance counters under the selected
    /// features, as `(class, index, counter)`. Simulators route reads of these
    /// registers to their counters instead of the register file.
    fn counter_registers(&self) -> Vec<(&'static str, u16, crate::PerfCounter)> {
        vec![]
    }

    /// Lowerings that run before instruction selection (e.g. splitting a
    /// two-way `cond_br` into a fall-through `asm.condbr` plus a trailing `br`
    /// so the branch condition is covered by the e-graph). A target opts in only
    /// once it has selection rules for `asm.condbr`.
    fn pre_isel_lowerings(&self) -> Vec<OpLowering> {
        Vec::new()
    }

    /// Lowerings that must run between instruction selection and register
    /// allocation (e.g. expanding `vcond_br` into a conditional branch whose
    /// SSA condition the allocator still has to color).
    fn pre_ra_lowerings(&self) -> Vec<OpLowering> {
        Vec::new()
    }

    /// Lowerings that finalize virtual ops after register allocation
    /// (e.g. `vret` into the target's return instruction).
    fn finalize_lowerings(&self) -> Vec<OpLowering> {
        Vec::new()
    }

    /// Object-format parameters (ELF machine/class/relocations), or `None`
    /// if this target cannot emit object files yet.
    fn object_format(&self) -> Option<ObjectFormatInfo> {
        None
    }

    /// The instruction encoder registry driving object emission, or `None`
    /// if this target cannot emit object files yet.
    fn binary_writer(&self, context: &Context) -> Option<BinaryWriter> {
        let _ = context;
        None
    }
}

/// A target made selectable by `--march`/`--mcpu`.
///
/// Backends contribute entries with [`register_target!`]; tools resolve targets
/// purely through this registry, so adding a backend never requires touching
/// `tir-mc`, `isasim` or the `tir-targets` aggregator.
pub struct TargetInfo {
    /// Canonical names this backend answers to, for help text and diagnostics.
    pub canonical_names: &'static [&'static str],
    /// Parse a `--march`/`--mcpu`/`--mattr` triple, returning a target if this
    /// backend owns it.
    pub select: SelectFn,
}

/// Parses a `--march`/`--mcpu`/`--mattr` triple into a target. `Ok(None)` means
/// the march string belongs to another backend; `Err` means this backend owns
/// the march but the combination is invalid (unknown extension, incompatible
/// CPU, ...).
pub type SelectFn = fn(
    march: &str,
    mcpu: Option<&str>,
    mattr: Option<&str>,
) -> Result<Option<Box<dyn TargetMachine>>, String>;

/// Link-time registry of every target reachable in the final binary.
#[distributed_slice]
pub static TARGETS: [TargetInfo];

/// Resolve a `--march`/`--mcpu`/`--mattr` triple to a target.
pub fn select_target(
    march: &str,
    mcpu: Option<&str>,
    mattr: Option<&str>,
) -> Result<Box<dyn TargetMachine>, String> {
    for t in TARGETS.iter() {
        if let Some(target) = (t.select)(march, mcpu, mattr)? {
            return Ok(target);
        }
    }
    Err(format!(
        "unknown target '{march}' (supported: {})",
        supported_targets().join(", ")
    ))
}

/// Canonical names accepted by [`select_target`], sorted and de-duplicated.
pub fn supported_targets() -> Vec<&'static str> {
    let mut names: Vec<_> = TARGETS
        .iter()
        .flat_map(|t| t.canonical_names.iter().copied())
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// Register a target backend so the tools can select it.
///
/// `select` is a [`SelectFn`]; `names` lists the canonical spellings shown in
/// help and error text.
#[macro_export]
macro_rules! register_target {
    ($select:path, [$($name:expr),+ $(,)?]) => {
        const _: () = {
            #[$crate::linkme::distributed_slice($crate::TARGETS)]
            #[linkme(crate = $crate::linkme)]
            static REGISTRATION: $crate::TargetInfo = $crate::TargetInfo {
                canonical_names: &[$($name),+],
                select: $select,
            };
        };
    };
}
