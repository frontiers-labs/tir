//! SMT equivalence checking of TMDL instruction semantics against the Sail
//! model of the target architecture (the architecture's golden model).
//!
//! For every supported TMDL instruction and a set of concrete operand
//! assignments:
//!   1. the instruction word is computed from the TMDL `encode_*` function
//!      and the operands are decoded back from it via `decode_*` (both
//!      evaluated by z3), so the TMDL encoding is part of what is checked and
//!      lossy immediate fields are verified at representable values;
//!   2. `isla-footprint` symbolically executes that word in the Sail model
//!      over a fully symbolic register state, producing one SMT trace per
//!      execution path;
//!   3. for each path, z3 is asked for a register state where TMDL and Sail
//!      disagree on the final GPRs or PC. `unsat` proves agreement for ALL
//!      2^XLEN values of every register; `sat` yields a counterexample.
//!
//! Modeling assumptions, reported with the results:
//!   - machine mode, no traps: paths that touch unmapped architectural state
//!     (CSRs, mcause, ...) are excluded and counted;
//!   - the initial PC is 4-byte aligned and `nextPC = PC + 4` (the fetch
//!     invariant for non-compressed instructions);
//!   - registers feeding an indirect jump (`jalr` base) hold 4-byte-aligned
//!     values, so misaligned-fetch trap paths are vacuous (temporary until the
//!     C extension is modeled);
//!   - TMDL leaves PC untouched for fall-through instructions, so a Sail path
//!     that does not write the (next) PC requires TMDL's final PC to equal the
//!     initial one, and a path that writes it requires equality with it;
//!   - memory is the TMDL flat little-endian byte array: Sail's plain read
//!     values are constrained against the initial array and its writes are
//!     folded into the expected final array. Paths through the platform
//!     memory map (CLINT) are excluded via their backing-register reads.
//!
//! External tools: `isla-footprint` and `z3`, plus a Sail model snapshot and
//! an isla config per ISA. isla is cloned and built and the snapshot is
//! downloaded automatically; override locations with `TIR_ISLA_FOOTPRINT`,
//! `TIR_ISLA_SNAPSHOT`, `TIR_ISLA_CONFIG`, `TIR_Z3`. `TIR_ISLA_REF` overrides
//! the isla checkout (default master) and `TIR_ISLA_SNAPSHOTS_REF` the
//! snapshot ref (default: a pinned commit; see `ISLA_SNAPSHOTS_PIN`).
//! `TIR_VERIFY_SMT_FILTER=add,sub` restricts the instruction set.
//!
//! x86 modeling notes. There is no published Sail snapshot for x86, so it is
//! built locally from the ACL2-derived `sail-x86-from-acl2` model (see that
//! repo's `model/Makefile` `x86.ir` target, spliced with
//! `test-generation-patches/isla_footprint.sail`) and read from the hard-coded
//! `IsaSpec::local_snapshot`. TO PUBLISH IT AND USE A URL: upload the built
//! `x86.ir` to the isla-snapshots repository under `snapshot = "x86.ir"`, then
//! set `local_snapshot: None` in the x86_64 `IsaSpec` — the download path
//! (base URL in `ensure_snapshot`, ref via `TIR_ISLA_SNAPSHOTS_REF`) then
//! serves it exactly like the RISC-V/ARM snapshots. Additional x86 assumptions
//! beyond machine-mode/no-trap:
//!   - the PC (`rip`) is pinned to a concrete canonical address by the config,
//!     and the fetch/decode is served from a concrete instruction-byte register
//!     (the spliced `rb`) so isla does not fork the byte-at-a-time decoder;
//!   - data-access and branch-target addresses are assumed canonical (the
//!     model masks linear addresses to 48 bits, TMDL's flat memory is 64-bit),
//!     the analogue of the RISC-V aligned-address assumption;
//!   - flags (`rflags` cf/zf/sf/of) are compared only for instructions whose
//!     TMDL behavior writes them; the ALU ops deliberately leave flags
//!     unmodeled, so their flag writes are ignored.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::utils::{download_file, git_checkout, project_root};
use anyhow::anyhow;
use xshell::{cmd, Shell};

/// A Sail bitfield flag register mapped to TMDL flag slots:
/// `(sail register, tmdl class, [(slot, bit index)])`.
type FlagReg = (&'static str, &'static str, &'static [(u64, u32)]);

pub struct IsaSpec {
    name: &'static str,
    tmdl_isa: &'static str,
    dialect: &'static str,
    defs_dir: &'static str,
    /// Snapshot file name in the isla-snapshots repository.
    snapshot: &'static str,
    /// isla config file name under `xtask/`.
    config: &'static str,
    xlen: u32,
    /// Sail GPR names are `{reg_prefix}{n}` for `n < reg_count`.
    reg_prefix: &'static str,
    reg_count: u32,
    /// Encoding index that names the hardwired-zero register, if it is not a
    /// real Sail register (RISC-V `x0`; AArch64 has no `R31`).
    zero_reg: Option<u32>,
    pc: &'static str,
    /// Sail's delayed PC register written by branches, when the model has one
    /// (RISC-V `nextPC`); the ARM model writes the PC directly.
    next_pc: Option<&'static str>,
    /// Model bookkeeping registers with no architectural meaning; reads and
    /// writes of them never exclude a path.
    ignore_regs: &'static [&'static str],
    /// Registers backing memory-mapped devices (RISC-V CLINT): a read means
    /// the access resolved into the platform memory map, which TMDL's flat
    /// memory does not model, so the path is excluded even when the value is
    /// concrete (`mtimecmp` is pinned by the config).
    mmio_regs: &'static [&'static str],
    /// Named Sail registers mapped onto single slots of TMDL register-file
    /// classes beyond the GPR file: `(sail name, tmdl class, slot, index
    /// width)`. Fields of `struct_reg` are named `<reg>.<field>`. Several
    /// Sail names may alias one slot (AArch64 `SP_ELx`).
    extra_regs: &'static [(&'static str, &'static str, u64, u32)],
    /// Sail struct register accessed via `(_ field |F|)` accessors (AArch64
    /// `PSTATE`); its fields map through `extra_regs`.
    struct_reg: Option<&'static str>,
    /// Concrete operand values for register classes whose encoding space is
    /// mostly unimplemented (CSR addresses), instead of the GPR patterns.
    fixed_reg_values: &'static [(&'static str, &'static [u64])],
    /// Sail trap-cause register and the causes TMDL behaviors model. A path
    /// writing a cause outside this set (access faults: the TMDL model treats
    /// all of memory as RAM) is excluded, established by a z3 probe since the
    /// written value is a path expression.
    trap_cause: Option<(&'static str, &'static [u64])>,
    isla_args: &'static [&'static str],
    /// Sail GPR register name -> encoding index, for ISAs whose registers are
    /// named individually rather than `{prefix}{n}` (x86 `rax`..`r15`). Empty
    /// for `{prefix}{n}` ISAs.
    reg_names: &'static [(&'static str, u32)],
    /// A Sail bitfield register whose bits carry TMDL flag slots:
    /// `(sail register, tmdl class, [(slot, bit index)])`. The whole-register
    /// read/write is decomposed into per-flag bits (x86 `rflags`), unlike a
    /// field-accessor struct register. Flags are only compared for
    /// instructions whose TMDL behavior writes them (`x86` ALU ops deliberately
    /// leave flags unmodeled).
    flag_reg: Option<FlagReg>,
    /// Pass `-s` (simplify) to isla-footprint. The x86 traces must stay
    /// unsimplified: the simplifier mishandles the model's wide struct values.
    simplify: bool,
    /// Assert the initial PC is 4-byte aligned (fixed-width fetch). x86 pins the
    /// PC to a concrete aligned value via the config instead.
    align_pc: bool,
    /// Assume data-access and indirect-jump-target addresses are canonical
    /// (bits 63..47 sign-extended), so the model's non-canonical `#GP` paths are
    /// vacuous. x86 only; the analogue of the RISC-V aligned-address assumption.
    canonical_addrs: bool,
    /// Every completing instruction advances the PC (x86 always writes `rip`
    /// via the fetch-decode-execute epilogue), so a path that does not write it
    /// faulted or decoded to something else and is excluded.
    requires_pc_write: bool,
    /// Local snapshot path, bypassing the download (x86 has no published
    /// snapshot). See the module docs for swapping in a URL.
    local_snapshot: Option<&'static str>,
    /// Sub-register view classes that alias the GPR file (x86
    /// `gpr8`/`gpr16`/`gpr32`/`gpr8h`), treated as mapped like `gpr`.
    gpr_view_classes: &'static [&'static str],
}

impl IsaSpec {
    /// TMDL register classes the driver can relate to Sail state. The x86
    /// sub-register views (`gpr8`/`gpr16`/`gpr32`/`gpr8h`) alias the GPR file
    /// and are named by the same Sail registers, so they map like `gpr`.
    fn class_is_mapped(&self, class: &str) -> bool {
        class == "gpr"
            || self.gpr_view_classes.contains(&class)
            || self.extra_regs.iter().any(|(_, c, _, _)| *c == class)
    }

    /// Bit width of the GPR file's encoding index (`read_gpr` parameter): the
    /// bits needed to name `reg_count` registers (RISC-V/AArch64: 5, x86: 4).
    fn gpr_idx_width(&self) -> u32 {
        32 - (self.reg_count - 1).leading_zeros()
    }
}

/// Whether the TMDL behavior of `instr` writes the flag register's class (x86
/// `cmp`/`test` do; the ALU ops deliberately do not), found by scanning the
/// emitted `execute_*` body for a `write_<class>` call.
fn tmdl_writes_flags(smt: &str, instr: &Instruction, class: &str) -> bool {
    let Some(start) = smt.find(&format!("(define-fun execute_{} ", instr.name)) else {
        return false;
    };
    sexpr_at(&smt[start..]).contains(&format!("(write_{} ", class))
}

/// Whether the TMDL behavior of `instr` touches the reservation state (LR/SC/
/// AMO), found by scanning its emitted `execute_*` body for the reservation
/// accessors and constructors.
fn instr_uses_reservation(smt: &str, instr: &Instruction) -> bool {
    let Some(start) = smt.find(&format!("(define-fun execute_{} ", instr.name)) else {
        return false;
    };
    let body = sexpr_at(&smt[start..]);
    ["set_res", "clear_res", "(resv ", "(resa "]
        .iter()
        .any(|p| body.contains(p))
}

/// Plain read-write CSR storage (`mscratch`) plus the machine-mode trap setup
/// state written by exception handlers in TMDL behaviors. The counter CSRs
/// stay unmapped: they are read-only and their Sail accesses trap or go
/// through mcounteren, which is outside the no-trap assumptions.
const RISCV_EXTRA_REGS: &[(&str, &str, u64, u32)] = &[
    ("mscratch", "csr", 0x340, 12),
    ("mstatus", "csr", 0x300, 12),
    ("mtvec", "csr", 0x305, 12),
    ("mepc", "csr", 0x341, 12),
    ("mcause", "csr", 0x342, 12),
    ("mtval", "csr", 0x343, 12),
];

/// CLINT-backed state: loads/stores whose address falls into the CLINT MMIO
/// window read or write these instead of memory.
const RISCV_MMIO_REGS: &[&str] = &["mtime", "mtimecmp", "mip"];

/// The TMDL `pstate` file holds the NZCV flags at their declaration-order
/// indices; the stack pointer is slot 31 of the shared GPR file, reachable
/// through the `gprsp` accessors (no hardwired-zero special case). Whichever
/// `SP_ELx` a path touches plays the role of TMDL's single SP.
const ARMV8_EXTRA_REGS: &[(&str, &str, u64, u32)] = &[
    ("PSTATE.N", "pstate", 0, 2),
    ("PSTATE.Z", "pstate", 1, 2),
    ("PSTATE.C", "pstate", 2, 2),
    ("PSTATE.V", "pstate", 3, 2),
    ("SP_EL0", "gprsp", 31, 5),
    ("SP_EL1", "gprsp", 31, 5),
    ("SP_EL2", "gprsp", 31, 5),
    ("SP_EL3", "gprsp", 31, 5),
];

const ISA_SPECS: &[IsaSpec] = &[
    IsaSpec {
        name: "riscv64",
        tmdl_isa: "RV64I",
        dialect: "riscv",
        defs_dir: "backends/riscv/defs",
        snapshot: "riscv64.ir",
        config: "verify-smt-riscv64.toml",
        xlen: 64,
        reg_prefix: "x",
        reg_count: 32,
        zero_reg: Some(0),
        pc: "PC",
        next_pc: Some("nextPC"),
        // cur_privilege is pinned to Machine and machine-mode traps stay in
        // Machine, so its (enum-valued) reads and re-writes carry no state
        // the TMDL model could diverge on.
        ignore_regs: &["cur_privilege"],
        mmio_regs: RISCV_MMIO_REGS,
        extra_regs: RISCV_EXTRA_REGS,
        struct_reg: None,
        fixed_reg_values: &[("csr", &[0x340])],
        // 3: breakpoint, 4: load address misaligned, 6: store/AMO address
        // misaligned, 11: environment call from M-mode.
        trap_cause: Some(("mcause", &[3, 4, 6, 11])),
        isla_args: &["-I", "cur_privilege=Machine"],
        reg_names: &[],
        flag_reg: None,
        simplify: true,
        align_pc: true,
        canonical_addrs: false,
        requires_pc_write: false,
        local_snapshot: None,
        gpr_view_classes: &[],
    },
    IsaSpec {
        name: "riscv32",
        tmdl_isa: "RV32I",
        dialect: "riscv",
        defs_dir: "backends/riscv/defs",
        snapshot: "rv32d.ir",
        config: "verify-smt-riscv32.toml",
        xlen: 32,
        reg_prefix: "x",
        reg_count: 32,
        zero_reg: Some(0),
        pc: "PC",
        next_pc: Some("nextPC"),
        ignore_regs: &["cur_privilege"],
        mmio_regs: RISCV_MMIO_REGS,
        extra_regs: RISCV_EXTRA_REGS,
        struct_reg: None,
        fixed_reg_values: &[("csr", &[0x340])],
        // 3: breakpoint, 4: load address misaligned, 6: store/AMO address
        // misaligned, 11: environment call from M-mode.
        trap_cause: Some(("mcause", &[3, 4, 6, 11])),
        isla_args: &["-I", "cur_privilege=Machine"],
        reg_names: &[],
        flag_reg: None,
        simplify: true,
        align_pc: true,
        canonical_addrs: false,
        requires_pc_write: false,
        local_snapshot: None,
        gpr_view_classes: &[],
    },
    IsaSpec {
        name: "armv8",
        tmdl_isa: "ARMv8A64",
        dialect: "arm64",
        defs_dir: "backends/arm64/defs",
        snapshot: "armv8p5.ir",
        config: "verify-smt-armv8.toml",
        xlen: 64,
        reg_prefix: "R",
        reg_count: 31,
        zero_reg: None,
        pc: "_PC",
        next_pc: None,
        ignore_regs: &[
            "SEE",
            "__unconditional",
            "__PC_changed",
            "__currentInstrLength",
            "BTypeNext",
            "BTypeCompatible",
            // Load/store instruction syndrome, model bookkeeping for fault
            // reporting; written on every memory access path.
            "__LSISyndrome",
        ],
        mmio_regs: &[],
        extra_regs: ARMV8_EXTRA_REGS,
        struct_reg: Some("PSTATE"),
        fixed_reg_values: &[],
        trap_cause: None,
        isla_args: &[],
        reg_names: &[],
        flag_reg: None,
        simplify: true,
        align_pc: true,
        canonical_addrs: false,
        requires_pc_write: false,
        local_snapshot: None,
        gpr_view_classes: &[],
    },
    IsaSpec {
        name: "x86_64",
        tmdl_isa: "X86_64",
        dialect: "x86_64",
        defs_dir: "backends/x86_64/defs",
        // No published snapshot: built locally from the sail-x86-from-acl2 model
        // (see module docs and local_snapshot below).
        snapshot: "x86.ir",
        config: "verify-smt-x86_64.toml",
        xlen: 64,
        // x86 GPRs are named individually; see reg_names.
        reg_prefix: "",
        reg_count: 16,
        zero_reg: None,
        pc: "rip",
        next_pc: None,
        // Footprint-setup and model-bookkeeping registers: their reads/writes
        // (application view, 64-bit mode, the fetch buffer) carry no
        // architectural meaning the TMDL model tracks.
        ignore_regs: &[
            "app_view",
            "marking_view",
            "ms_reg",
            "fault_reg",
            "msrs",
            "seg_hidden_attrs",
            "seg_hidden_bases",
            "seg_hidden_limits",
            "seg_visibles",
            "isla_ifetch_buf",
            "log_register_writes",
            // Control registers (CR0/CR3/CR4): read during memory access checks
            // even in the application view, where paging is bypassed.
            "ctrs",
            "os",
        ],
        mmio_regs: &[],
        extra_regs: &[],
        struct_reg: None,
        fixed_reg_values: &[],
        trap_cause: None,
        isla_args: &[],
        reg_names: X86_REG_NAMES,
        // rflags bit layout: cf=0, zf=6, sf=7, of=11 (Intel SDM). TMDL EFLAGS
        // slots cf=0, zf=1, sf=2, of=3 (declaration order).
        flag_reg: Some(("rflags", "eflags", &[(0, 0), (1, 6), (2, 7), (3, 11)])),
        simplify: false,
        align_pc: false,
        canonical_addrs: true,
        requires_pc_write: true,
        local_snapshot: Some("/home/alex/Projects/frontiers/sail_workspace/isla-snapshots/x86.ir"),
        gpr_view_classes: &["gpr8", "gpr16", "gpr32", "gpr8h"],
    },
];

/// x86 Sail GPR names in TMDL encoding-index order (`rax`=0 .. `r15`=15).
const X86_REG_NAMES: &[(&str, u32)] = &[
    ("rax", 0),
    ("rcx", 1),
    ("rdx", 2),
    ("rbx", 3),
    ("rsp", 4),
    ("rbp", 5),
    ("rsi", 6),
    ("rdi", 7),
    ("r8", 8),
    ("r9", 9),
    ("r10", 10),
    ("r11", 11),
    ("r12", 12),
    ("r13", 13),
    ("r14", 14),
    ("r15", 15),
];

pub fn verify_smt(sh: &Shell, isa: &str) -> anyhow::Result<()> {
    let spec = ISA_SPECS.iter().find(|s| s.name == isa).ok_or_else(|| {
        anyhow!("unsupported ISA {isa}; available: riscv64, riscv32, armv8, x86_64")
    })?;
    let tools = Tools::ensure(sh, spec)?;
    let root = project_root();
    let out_dir = root.join("target/verify/smt").join(spec.name);
    std::fs::create_dir_all(out_dir.join("cache"))?;
    std::fs::create_dir_all(out_dir.join("queries"))?;

    let smt_path = out_dir.join(format!("{}.smt2", spec.name));
    generate_tmdl_smt(sh, spec, &root, &smt_path)?;
    let smt = std::fs::read_to_string(&smt_path)?;

    let instructions = parse_inventory(&smt);
    let filter: Option<Vec<String>> = std::env::var("TIR_VERIFY_SMT_FILTER")
        .ok()
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect());

    let mut report = Report::default();

    for instr in &instructions {
        if filter.as_ref().is_some_and(|f| !f.contains(&instr.name)) {
            continue;
        }
        if !instr.supported {
            report.unsupported.push(instr.name.clone());
            continue;
        }
        // Atomics (A extension) reference the reservation state, whose mapping
        // onto Sail's reservation register is follow-up work (see module docs);
        // skip them until that is enabled.
        if instr_uses_reservation(&smt, instr) {
            report.unsupported.push(format!(
                "{} (atomic; Sail reservation mapping is follow-up)",
                instr.name
            ));
            continue;
        }
        // Skip instructions with operands in register classes that have no
        // correspondence to Sail state (e.g. the TMDL `pc` operand class).
        if let Some(class) = instr.operands.iter().find_map(|(_, k)| match k {
            OperandKind::Reg { class, .. } if !spec.class_is_mapped(class) => Some(class),
            _ => None,
        }) {
            report.unsupported.push(format!(
                "{} (unmapped register class {})",
                instr.name, class
            ));
            continue;
        }
        verify_instruction(&tools, spec, &out_dir, &smt, instr, &mut report)?;
    }

    report.print();
    if report.failed > 0 {
        anyhow::bail!(
            "SMT equivalence check found {} divergence(s)",
            report.failed
        );
    }
    Ok(())
}

struct Tools {
    isla_footprint: PathBuf,
    snapshot: PathBuf,
    isla_config: PathBuf,
    z3: PathBuf,
}

impl Tools {
    /// Resolve the external tools, fetching anything that is not overridden
    /// by an environment variable.
    fn ensure(sh: &Shell, spec: &IsaSpec) -> anyhow::Result<Self> {
        let isla_footprint = match std::env::var("TIR_ISLA_FOOTPRINT") {
            Ok(path) => path.into(),
            Err(_) => ensure_isla_footprint(sh)?,
        };
        let snapshot = match (std::env::var("TIR_ISLA_SNAPSHOT"), spec.local_snapshot) {
            (Ok(path), _) => path.into(),
            // No published snapshot: use the locally built model. To publish it,
            // upload the `.ir` and clear `local_snapshot` so the download path
            // (governed by `TIR_ISLA_SNAPSHOTS_REF`) is used instead.
            (Err(_), Some(local)) => PathBuf::from(local),
            (Err(_), None) => ensure_snapshot(sh, spec.snapshot)?,
        };
        Ok(Tools {
            isla_footprint,
            snapshot,
            isla_config: std::env::var("TIR_ISLA_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|_| project_root().join("xtask").join(spec.config)),
            z3: std::env::var("TIR_Z3")
                .unwrap_or_else(|_| "z3".to_string())
                .into(),
        })
    }
}

fn ensure_isla_footprint(sh: &Shell) -> anyhow::Result<PathBuf> {
    let isla_dir = project_root().join("target/isla");
    let bin = isla_dir.join("target/release/isla-footprint");
    if bin.exists() {
        return Ok(bin);
    }
    let isla_ref = std::env::var("TIR_ISLA_REF").unwrap_or_else(|_| "master".to_string());
    git_checkout(
        sh,
        "https://github.com/rems-project/isla",
        &isla_ref,
        "isla",
    )?;
    let manifest = isla_dir.join("Cargo.toml");
    cmd!(
        sh,
        "cargo build --release --manifest-path {manifest} --bin isla-footprint"
    )
    .run()?;
    Ok(bin)
}

/// Pinned isla-snapshots commit: the models the specs and configs were
/// validated against. A floating ref breaks silently when upstream swaps
/// model generations (rv32d.ir became a new-interface build on 2026-06-02).
const ISLA_SNAPSHOTS_PIN: &str = "d8b31014643035a3b11071e56ef30001de3f52ab";

fn ensure_snapshot(sh: &Shell, file: &str) -> anyhow::Result<PathBuf> {
    let snap_ref =
        std::env::var("TIR_ISLA_SNAPSHOTS_REF").unwrap_or_else(|_| ISLA_SNAPSHOTS_PIN.to_string());
    let dest = project_root()
        .join("target/verify/snapshots")
        .join(snap_ref.replace('/', "-"))
        .join(file);
    let url = format!("https://github.com/rems-project/isla-snapshots/raw/{snap_ref}/{file}");
    download_file(sh, &url, &dest)?;
    Ok(dest)
}

fn generate_tmdl_smt(sh: &Shell, spec: &IsaSpec, root: &Path, out: &Path) -> anyhow::Result<()> {
    let defs: Vec<PathBuf> = std::fs::read_dir(root.join(spec.defs_dir))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "tmdl"))
        .collect();
    let out_str = out.to_string_lossy().to_string();
    let dialect = spec.dialect;
    let tmdl_isa = spec.tmdl_isa;
    cmd!(
        sh,
        "cargo run -p tmdl --bin tmdlc -- --action emit-smtlib --dialect {dialect} --isa {tmdl_isa} --output {out_str} {defs...}"
    )
    .run()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Instruction inventory (from `; INSTRUCTION:` metadata in the generated SMT)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum OperandKind {
    Reg { class: String, idx_width: u32 },
    Bits(u32),
    Int,
}

#[derive(Clone, Debug)]
struct Instruction {
    name: String,
    writes_pc: bool,
    /// Instruction encoding width in bits (a byte multiple). Fixed-width ISAs
    /// are always 32; x86 varies per instruction.
    width_bits: u32,
    operands: Vec<(String, OperandKind)>,
    supported: bool,
}

impl Instruction {
    fn width_bytes(&self) -> u32 {
        self.width_bits / 8
    }
}

fn parse_inventory(smt: &str) -> Vec<Instruction> {
    let unsupported: Vec<&str> = smt
        .lines()
        .filter_map(|l| l.strip_prefix("; UNSUPPORTED-BEHAVIOR: "))
        .collect();

    smt.lines()
        .filter_map(|l| l.strip_prefix("; INSTRUCTION: "))
        .filter_map(|l| {
            let mut parts = l.split_whitespace().peekable();
            let name = parts.next()?.to_string();
            let writes_pc = parts.next()? == "writes-pc=true";
            // `width=<bits>` sits between writes-pc and the operands (older
            // snapshots without it default to 32-bit fixed-width).
            let width_bits = match parts.peek().and_then(|p| p.strip_prefix("width=")) {
                Some(w) => {
                    let w = w.parse().ok()?;
                    parts.next();
                    w
                }
                None => 32,
            };
            let operands = parts
                .map(|op| {
                    let (op_name, kind) = op.split_once(':')?;
                    let kind = match kind.split_once(':') {
                        Some(("reg", rest)) => {
                            let (class, w) = rest.split_once(':')?;
                            OperandKind::Reg {
                                class: class.to_string(),
                                idx_width: w.parse().ok()?,
                            }
                        }
                        Some(("bits", w)) => OperandKind::Bits(w.parse().ok()?),
                        _ if kind == "int" => OperandKind::Int,
                        _ => return None,
                    };
                    Some((op_name.to_string(), kind))
                })
                .collect::<Option<Vec<_>>>()?;
            let supported = !unsupported.contains(&name.as_str());
            Some(Instruction {
                name,
                writes_pc,
                width_bits,
                operands,
                supported,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Operand assignments
// ---------------------------------------------------------------------------

/// Concrete operand tuples for one instruction. Registers cover zero-register
/// corner cases and aliasing; immediates cover boundary patterns. PC-writing
/// instructions get 4-byte aligned immediates so that, together with the
/// aligned-PC assumption, Sail's misaligned-fetch trap paths are vacuous.
fn operand_cases(spec: &IsaSpec, instr: &Instruction) -> Vec<Vec<u64>> {
    let fixed_values = |class: &str| {
        spec.fixed_reg_values
            .iter()
            .find(|(c, _)| *c == class)
            .map(|(_, vals)| *vals)
    };
    // Operands with a fixed value list (CSR addresses) sit outside the GPR
    // patterns; they get their fixed values appended below.
    let fixed_positions: Vec<(usize, &[u64])> = instr
        .operands
        .iter()
        .enumerate()
        .filter_map(|(i, (_, k))| match k {
            OperandKind::Reg { class, .. } => fixed_values(class).map(|vals| (i, vals)),
            _ => None,
        })
        .collect();
    let reg_positions: Vec<usize> = instr
        .operands
        .iter()
        .enumerate()
        .filter(|(i, (_, k))| {
            matches!(k, OperandKind::Reg { .. }) && !fixed_positions.iter().any(|(fi, _)| fi == i)
        })
        .map(|(i, _)| i)
        .collect();
    let reg_patterns: Vec<Vec<u64>> = match reg_positions.len() {
        0 => vec![vec![]],
        1 => vec![vec![1], vec![0], vec![31]],
        2 => vec![vec![1, 2], vec![0, 3], vec![4, 0], vec![5, 5], vec![31, 30]],
        _ => vec![
            vec![1, 2, 3],
            vec![0, 5, 6],
            vec![7, 0, 8],
            vec![9, 10, 0],
            vec![4, 4, 4],
            vec![31, 30, 29],
            vec![11, 12, 12],
        ],
    };

    let imm_position: Option<(usize, u32)> =
        instr
            .operands
            .iter()
            .enumerate()
            .find_map(|(i, (_, k))| match k {
                OperandKind::Bits(w) => Some((i, *w)),
                OperandKind::Int => Some((i, 64)),
                OperandKind::Reg { .. } => None,
            });
    let imm_values: Vec<u64> = match imm_position {
        None => vec![0],
        Some((_, w)) => {
            let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            if instr.writes_pc {
                vec![4, 8, mask & !3, 1u64 << (w - 1), (1u64 << (w - 1)) - 4]
            } else {
                vec![
                    0,
                    1,
                    mask,
                    1u64 << (w - 1),
                    (1u64 << (w - 1)) - 1,
                    0xAAAA & mask,
                ]
            }
        }
    };

    let mut cases = vec![];
    for regs in &reg_patterns {
        for imm in &imm_values {
            let mut case = vec![0u64; instr.operands.len()];
            for (slot, value) in reg_positions.iter().zip(regs) {
                case[*slot] = *value;
            }
            if let Some((slot, _)) = imm_position {
                case[slot] = *imm;
            }
            cases.push(case);
            if imm_position.is_none() {
                break;
            }
        }
        if reg_positions.is_empty() {
            break;
        }
    }
    for (i, case) in cases.iter_mut().enumerate() {
        for (slot, vals) in &fixed_positions {
            case[*slot] = vals[i % vals.len()];
        }
    }
    cases
}

/// First balanced s-expression (or bare token) at the start of `s`.
fn sexpr_at(s: &str) -> &str {
    if s.starts_with('(') {
        let mut depth = 0usize;
        for (i, c) in s.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return &s[..=i];
                    }
                }
                _ => {}
            }
        }
        s
    } else {
        let end = s
            .find(|c: char| c.is_whitespace() || c == ')')
            .unwrap_or(s.len());
        &s[..end]
    }
}

/// Indices of register operands whose value feeds a PC write (`jalr`-style
/// indirect jumps), found by scanning the emitted `execute_*` body for
/// `read_*` of the operand inside a `write_pc` value expression. Branch
/// targets are PC-relative and never match.
fn pc_source_reg_operands(smt: &str, instr: &Instruction) -> Vec<usize> {
    let Some(start) = smt.find(&format!("(define-fun execute_{} ", instr.name)) else {
        return vec![];
    };
    let body = sexpr_at(&smt[start..]);
    let mut sources = vec![];
    let mut rest = body;
    while let Some(pos) = rest.find("(write_pc ") {
        rest = &rest[pos + "(write_pc ".len()..];
        let state = sexpr_at(rest);
        let value = sexpr_at(rest[state.len()..].trim_start());
        for (i, (name, kind)) in instr.operands.iter().enumerate() {
            if let OperandKind::Reg { class, .. } = kind {
                if value.contains(&format!("(read_{} st {})", class, name)) && !sources.contains(&i)
                {
                    sources.push(i);
                }
            }
        }
    }
    sources
}

/// Replace whole-word occurrences of `word` in `s` (identifier boundaries).
fn replace_word(s: &str, word: &str, repl: &str) -> String {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find(word) {
        let before_ok = rest[..pos].chars().last().is_none_or(|c| !is_ident(c));
        let after = &rest[pos + word.len()..];
        let after_ok = after.chars().next().is_none_or(|c| !is_ident(c));
        out.push_str(&rest[..pos]);
        if before_ok && after_ok {
            out.push_str(repl);
        } else {
            out.push_str(word);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// TMDL memory-access address expressions (rewritten to `st0` and concrete
/// operand values), found by scanning the `execute_*` body for `read_mem_*` /
/// `write_mem_*`. On x86 these are constrained canonical: the model masks
/// linear addresses to 48 bits while TMDL's flat memory is 64-bit-addressed, so
/// the two agree only on canonical addresses.
fn mem_addr_exprs(smt: &str, instr: &Instruction, case: &[u64], spec: &IsaSpec) -> Vec<String> {
    let Some(start) = smt.find(&format!("(define-fun execute_{} ", instr.name)) else {
        return vec![];
    };
    let body = sexpr_at(&smt[start..]);
    let mut exprs = vec![];
    for prefix in ["(read_mem_", "(write_mem_"] {
        let mut rest = body;
        while let Some(pos) = rest.find(prefix) {
            rest = &rest[pos + prefix.len()..];
            // "<width> st <address> ..." — skip the width and state parameter.
            let after = rest
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start();
            let after = after.strip_prefix("st").unwrap_or(after).trim_start();
            let addr = sexpr_at(after);
            let mut expr = addr.to_string();
            for ((name, kind), value) in instr.operands.iter().zip(case) {
                let lit = match kind {
                    OperandKind::Reg { idx_width, .. } => format!("(_ bv{} {})", value, idx_width),
                    _ => format!("(_ bv{} {})", value, spec.xlen),
                };
                expr = replace_word(&expr, name, &lit);
            }
            exprs.push(replace_word(&expr, "st", "st0"));
        }
    }
    exprs
}

fn operand_smt_args(spec: &IsaSpec, instr: &Instruction, case: &[u64]) -> String {
    instr
        .operands
        .iter()
        .zip(case)
        .map(|((_, kind), value)| match kind {
            OperandKind::Reg { idx_width, .. } => format!("(_ bv{} {})", value, idx_width),
            _ => format!("(_ bv{} {})", value, spec.xlen),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Encoding via z3 (evaluate the TMDL `encode_*` functions)
// ---------------------------------------------------------------------------

fn encode_words(
    tools: &Tools,
    spec: &IsaSpec,
    out_dir: &Path,
    smt: &str,
    instr: &Instruction,
    cases: &[Vec<u64>],
) -> anyhow::Result<Vec<u128>> {
    let mut query = String::from(smt);
    query.push_str("\n(check-sat)\n");
    for case in cases {
        let args = operand_smt_args(spec, instr, case);
        let call = if args.is_empty() {
            format!("encode_{}", instr.name)
        } else {
            format!("(encode_{} {})", instr.name, args)
        };
        writeln!(query, "(get-value ({}))", call)?;
    }
    let path = out_dir
        .join("queries")
        .join(format!("encode_{}.smt2", instr.name));
    std::fs::write(&path, query)?;
    let output = Command::new(&tools.z3).arg("-smt2").arg(&path).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // z3 prints a width-W bitvector as `#x` + W/4 hex digits (all encoding
    // widths are byte multiples, so divisible by 4).
    let hex_digits = (instr.width_bits / 4) as usize;
    let words: Vec<u128> = stdout
        .split("#x")
        .skip(1)
        .filter_map(|chunk| u128::from_str_radix(chunk.get(..hex_digits)?, 16).ok())
        .collect();
    anyhow::ensure!(
        words.len() == cases.len(),
        "z3 evaluated {} of {} encodings for {}: {}",
        words.len(),
        cases.len(),
        instr.name,
        stdout
    );
    Ok(words)
}

/// Operand values recovered by decoding the instruction words back, so the
/// equivalence check uses what the encoding can express: lossy immediate
/// fields drop bits (branch immediates force bit 0, ARM unsigned-offset
/// loads/stores store the byte offset scaled down by the access size).
fn decode_operands(
    tools: &Tools,
    spec: &IsaSpec,
    out_dir: &Path,
    smt: &str,
    instr: &Instruction,
    words: &[u128],
) -> anyhow::Result<Vec<Vec<u64>>> {
    if instr.operands.is_empty() {
        return Ok(words.iter().map(|_| vec![]).collect());
    }
    let mut query = String::from(smt);
    query.push_str("\n(check-sat)\n");
    for word in words {
        let probes = instr
            .operands
            .iter()
            .map(|(op, _)| {
                format!(
                    "({}_{} (decode_{} (_ bv{} 32)))",
                    instr.name, op, spec.dialect, word
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        writeln!(query, "(get-value ({}))", probes)?;
    }
    let path = out_dir
        .join("queries")
        .join(format!("decode_{}.smt2", instr.name));
    std::fs::write(&path, query)?;
    let output = Command::new(&tools.z3).arg("-smt2").arg(&path).output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // z3 prints bitvector values as #x… (multiples of 4 bits) or #b…; the
    // echoed probe expressions only contain decimal (_ bvN 32) literals, so
    // every #-literal is an operand value.
    let mut values = Vec::new();
    let mut rest: &str = &stdout;
    while let Some(pos) = rest.find('#') {
        rest = &rest[pos + 1..];
        let (radix, digits): (u32, &str) = match rest.as_bytes().first() {
            Some(b'x') => (16, &rest[1..]),
            Some(b'b') => (2, &rest[1..]),
            _ => continue,
        };
        let end = digits
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(digits.len());
        if let Ok(v) = u64::from_str_radix(&digits[..end], radix) {
            values.push(v);
        }
        rest = &digits[end..];
    }
    anyhow::ensure!(
        values.len() == words.len() * instr.operands.len(),
        "z3 decoded {} of {} operand values for {}",
        values.len(),
        words.len() * instr.operands.len(),
        instr.name
    );
    Ok(values
        .chunks(instr.operands.len())
        .map(|chunk| chunk.to_vec())
        .collect())
}

// ---------------------------------------------------------------------------
// isla-footprint invocation (cached per instruction word)
// ---------------------------------------------------------------------------

/// Traces depend on the Sail snapshot and isla config; fingerprint both so a
/// swap invalidates the cache.
fn cache_fingerprint(tools: &Tools) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::fs::read(&tools.isla_config)
        .unwrap_or_default()
        .hash(&mut hasher);
    tools.snapshot.hash(&mut hasher);
    std::fs::metadata(&tools.snapshot)
        .map(|m| m.len())
        .unwrap_or(0)
        .hash(&mut hasher);
    hasher.finish()
}

/// `Ok(None)` means isla failed or had to be killed for this word (a few
/// encodings blow up its symbolic executor); the caller records and moves on.
fn sail_traces(
    tools: &Tools,
    spec: &IsaSpec,
    out_dir: &Path,
    instr: &Instruction,
    word: u128,
) -> anyhow::Result<Option<String>> {
    let cache = out_dir.join("cache").join(format!(
        "{:020x}-{:016x}.trace",
        word,
        cache_fingerprint(tools)
    ));
    if let Ok(cached) = std::fs::read_to_string(&cache) {
        return Ok(Some(cached));
    }
    let bits = format!("{:0width$b}", word, width = instr.width_bits as usize);
    let mut cmd = Command::new(&tools.isla_footprint);
    cmd.args(["-A"])
        .arg(&tools.snapshot)
        .arg("-C")
        .arg(&tools.isla_config)
        .args([
            "-T",
            "1",
            "--function",
            "isla_footprint_no_init",
            "--timeout",
            "90",
            "--partial",
            "-i",
            &bits,
        ]);
    if spec.simplify {
        cmd.arg("-s");
    }
    let mut child = cmd
        .args(spec.isla_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    // Drain stdout on a thread so a chatty child can't dead-lock on a full
    // pipe while we poll for exit.
    let mut pipe = child.stdout.take().expect("stdout piped");
    let reader = std::thread::spawn(move || {
        use std::io::Read as _;
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    let stdout = reader.join().expect("reader thread");
    if !status.is_some_and(|s| s.success()) {
        return Ok(None);
    }
    std::fs::write(&cache, &stdout)?;
    Ok(Some(stdout))
}

// ---------------------------------------------------------------------------
// Trace parsing
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Sexp {
    Atom(String),
    List(Vec<Sexp>),
}

impl std::fmt::Display for Sexp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Sexp::Atom(a) => write!(f, "{}", a),
            Sexp::List(items) => {
                write!(f, "(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, ")")
            }
        }
    }
}

fn parse_sexps(input: &str) -> Vec<Sexp> {
    let mut stack: Vec<Vec<Sexp>> = vec![vec![]];
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // isla appends structural parens after end-of-line location
            // comments; the comments themselves never contain parens.
            ';' => {
                while let Some(&next) = chars.peek() {
                    if next == '\n' || next == '(' || next == ')' {
                        break;
                    }
                    chars.next();
                }
            }
            '(' => stack.push(vec![]),
            ')' => {
                let done = stack.pop().unwrap_or_default();
                if let Some(top) = stack.last_mut() {
                    top.push(Sexp::List(done));
                } else {
                    stack.push(vec![Sexp::List(done)]);
                }
            }
            '"' | '|' => {
                let quote = c;
                let mut atom = String::new();
                atom.push(quote);
                for c in chars.by_ref() {
                    atom.push(c);
                    if c == quote {
                        break;
                    }
                }
                if let Some(top) = stack.last_mut() {
                    top.push(Sexp::Atom(atom));
                }
            }
            c if c.is_whitespace() => {}
            c => {
                let mut atom = String::new();
                atom.push(c);
                while let Some(&next) = chars.peek() {
                    if next.is_whitespace() || next == '(' || next == ')' || next == ';' {
                        break;
                    }
                    atom.push(next);
                    chars.next();
                }
                if let Some(top) = stack.last_mut() {
                    top.push(Sexp::Atom(atom));
                }
            }
        }
    }
    stack.pop().unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum MappedReg {
    X(u32),
    Pc,
    NextPc,
    /// Index into `spec.extra_regs`.
    Slot(usize),
}

fn map_register(spec: &IsaSpec, name: &str) -> Option<MappedReg> {
    let name = name.trim_matches('|');
    if name == spec.pc {
        return Some(MappedReg::Pc);
    }
    if spec.next_pc == Some(name) {
        return Some(MappedReg::NextPc);
    }
    if let Some(i) = spec.extra_regs.iter().position(|(n, _, _, _)| *n == name) {
        return Some(MappedReg::Slot(i));
    }
    if !spec.reg_names.is_empty() {
        return spec
            .reg_names
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, idx)| MappedReg::X(*idx));
    }
    name.strip_prefix(spec.reg_prefix)
        .and_then(|n| n.parse::<u32>().ok())
        .filter(|n| *n < spec.reg_count)
        .map(MappedReg::X)
}

/// Field name from a `((_ field |F|))` register accessor.
fn accessor_field(items: &[Sexp]) -> Option<&str> {
    let Some(Sexp::List(accessors)) = items.get(2) else {
        return None;
    };
    let Some(Sexp::List(accessor)) = accessors.first() else {
        return None;
    };
    match accessor.as_slice() {
        [Sexp::Atom(u), Sexp::Atom(kw), Sexp::Atom(field)] if u == "_" && kw == "field" => {
            Some(field.trim_matches('|'))
        }
        _ => None,
    }
}

/// Whether a memory event's kind is a plain data access. Old-interface
/// models (RISC-V) use enum atoms (`|Read_plain|`); new-interface models
/// (ARM) embed the whole request struct, whose `access_kind` must be an
/// explicit access of plain variety and normal (non-acquire/release)
/// strength.
fn is_plain_access(kind: &Sexp) -> bool {
    match kind {
        Sexp::Atom(k) => k.trim_matches('|').ends_with("_plain"),
        Sexp::List(_) => {
            let rendered = kind.to_string();
            rendered.contains("|AV_plain|")
                && (!rendered.contains("|strength|") || rendered.contains("|AS_normal|"))
        }
    }
}

/// Whether the value mentions any isla symbolic variable (`vN`). Struct
/// register reads (RISC-V `mip`) wrap their symbolic fields in a
/// `(_ struct ...)` literal, so a bare-atom check is not enough.
fn contains_symbolic(value: &Sexp) -> bool {
    match value {
        Sexp::Atom(a) => a
            .strip_prefix('v')
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())),
        Sexp::List(items) => items.iter().any(contains_symbolic),
    }
}

/// The payload of a single-field `(_ struct (|bits| value))` literal, as
/// written for Sail bitfield registers; anything else is returned as is.
fn unwrap_bits_struct(value: &Sexp) -> &Sexp {
    match struct_field(value, "bits") {
        Some(bits) if matches!(value, Sexp::List(items) if items.len() == 3) => bits,
        _ => value,
    }
}

/// Component of a `(_ struct (|F| value) ...)` literal.
fn struct_field<'a>(value: &'a Sexp, field: &str) -> Option<&'a Sexp> {
    let Sexp::List(items) = value else {
        return None;
    };
    if items.first() != Some(&Sexp::Atom("_".into()))
        || items.get(1) != Some(&Sexp::Atom("struct".into()))
    {
        return None;
    }
    items.iter().skip(2).find_map(|item| match item {
        Sexp::List(pair) => match pair.as_slice() {
            [Sexp::Atom(name), value] if name.trim_matches('|') == field => Some(value),
            _ => None,
        },
        _ => None,
    })
}

/// One memory access event: `value` is the read result variable or the
/// written data expression.
struct MemAccess {
    value: String,
    address: String,
    bytes: u32,
}

#[derive(Default)]
struct TraceInfo {
    /// `(register, symbolic-variable)` for symbolic initial-state reads.
    reads: Vec<(MappedReg, String)>,
    /// Final value per written register (last write wins).
    writes: HashMap<MappedReg, String>,
    /// Plain memory reads, related to the initial TMDL memory array.
    mem_reads: Vec<MemAccess>,
    /// Plain memory writes in order, folded into the expected final array.
    mem_writes: Vec<MemAccess>,
    /// Verbatim `(declare-const v Sort)` lines.
    declares: Vec<String>,
    /// Ordered `define-const` bindings, replayed as a `let` chain.
    defines: Vec<(String, String)>,
    asserts: Vec<String>,
    /// `(flag slot, whole-register value, bit index)` for symbolic reads of a
    /// bitfield flag register (x86 `rflags`): each bit relates to a flag slot's
    /// initial state.
    flag_reads: Vec<(u64, String, u32)>,
    /// `flag slot -> bit expression` from a write of the flag register (last
    /// write wins). Only compared when the TMDL behavior also writes flags.
    flag_writes: HashMap<u64, String>,
    /// Why this path cannot be checked against the TMDL state (trap paths,
    /// CSR accesses, ...), if so.
    excluded: Option<String>,
}

fn analyze_trace(spec: &IsaSpec, events: &[Sexp]) -> TraceInfo {
    let mut info = TraceInfo::default();
    // Variables bound by `define-const`: reads returning them are read-backs
    // of values the model computed (e.g. Sail writes `nextPC = PC + 4` and
    // reads it back later), not symbolic initial state.
    let mut defined_vars = std::collections::HashSet::new();
    let exclude = |info: &mut TraceInfo, reason: String| {
        if info.excluded.is_none() {
            info.excluded = Some(reason);
        }
    };

    for event in events {
        let Sexp::List(items) = event else { continue };
        let Some(Sexp::Atom(head)) = items.first() else {
            continue;
        };
        match head.as_str() {
            "read-reg" => {
                let (Some(Sexp::Atom(name)), Some(value)) = (items.get(1), items.last()) else {
                    continue;
                };
                let trimmed = name.trim_matches('|');
                if spec.ignore_regs.contains(&trimmed) {
                    continue;
                }
                // A bitfield flag register (x86 `rflags`) read: each mapped bit
                // of the symbolic initial value relates to a flag slot.
                if let Some((flag_name, _, bit_map)) = spec.flag_reg {
                    if trimmed == flag_name {
                        if let Sexp::Atom(var) = unwrap_bits_struct(value) {
                            if var.starts_with('v') && !defined_vars.contains(var) {
                                for (slot, bit) in bit_map {
                                    info.flag_reads.push((*slot, var.clone(), *bit));
                                }
                            }
                        }
                        continue;
                    }
                }
                if spec.mmio_regs.contains(&trimmed) {
                    exclude(
                        &mut info,
                        format!(
                            "reads MMIO-backed register {} (platform memory map)",
                            trimmed
                        ),
                    );
                    continue;
                }
                if spec.struct_reg == Some(trimmed) {
                    // Mapped fields (NZCV) relate to TMDL state; other fields
                    // (EL, nRW, ...) are pinned by each path's assertions.
                    if let Some(field) = accessor_field(items) {
                        let full = format!("{}.{}", trimmed, field);
                        if let Some(i) = spec.extra_regs.iter().position(|(n, _, _, _)| *n == full)
                        {
                            let reg = MappedReg::Slot(i);
                            if let Some(Sexp::Atom(var)) = struct_field(value, field) {
                                if var.starts_with('v')
                                    && !info.writes.contains_key(&reg)
                                    && !defined_vars.contains(var)
                                {
                                    info.reads.push((reg, var.clone()));
                                }
                            }
                        }
                    }
                    continue;
                }
                // Bitfield registers (RISC-V mstatus, mtvec, mcause) carry
                // their value in a single-field struct literal.
                let value = unwrap_bits_struct(value);
                let symbolic = contains_symbolic(value);
                match map_register(spec, name) {
                    Some(reg) => {
                        if let Sexp::Atom(var) = value {
                            // A concrete read of a mapped register pins the
                            // TMDL slot to the config's value; a symbolic one
                            // names the slot's initial state.
                            let concrete = var.starts_with('#');
                            if !info.writes.contains_key(&reg)
                                && (concrete || !defined_vars.contains(var))
                            {
                                info.reads.push((reg, var.clone()));
                            }
                        }
                    }
                    None if symbolic => {
                        exclude(&mut info, format!("reads unmapped register {}", name));
                    }
                    None => {}
                }
            }
            "write-reg" => {
                let (Some(Sexp::Atom(name)), Some(value)) = (items.get(1), items.last()) else {
                    continue;
                };
                let trimmed = name.trim_matches('|');
                if spec.ignore_regs.contains(&trimmed) {
                    continue;
                }
                // A write of the bitfield flag register: record each mapped bit
                // as that flag slot's final value (last write wins).
                if let Some((flag_name, _, bit_map)) = spec.flag_reg {
                    if trimmed == flag_name {
                        let value = unwrap_bits_struct(value);
                        for (slot, bit) in bit_map {
                            info.flag_writes
                                .insert(*slot, format!("((_ extract {bit} {bit}) {value})"));
                        }
                        continue;
                    }
                }
                if spec.struct_reg == Some(trimmed) {
                    let mapped = accessor_field(items).and_then(|field| {
                        let full = format!("{}.{}", trimmed, field);
                        let i = spec.extra_regs.iter().position(|(n, _, _, _)| *n == full)?;
                        Some((i, struct_field(value, field)?))
                    });
                    match mapped {
                        Some((i, field_value)) => {
                            info.writes
                                .insert(MappedReg::Slot(i), field_value.to_string());
                        }
                        None => exclude(
                            &mut info,
                            format!("writes unmapped {} field (trap/system path)", trimmed),
                        ),
                    }
                    continue;
                }
                match map_register(spec, name) {
                    Some(MappedReg::X(n)) if Some(n) == spec.zero_reg => {}
                    Some(reg) => {
                        info.writes
                            .insert(reg, unwrap_bits_struct(value).to_string());
                    }
                    None => exclude(
                        &mut info,
                        format!("writes unmapped register {} (trap/system path)", name),
                    ),
                }
            }
            "declare-const" => {
                let (Some(Sexp::Atom(var)), Some(sort)) = (items.get(1), items.get(2)) else {
                    continue;
                };
                let is_bitvec = matches!(
                    sort,
                    Sexp::List(s) if s.first() == Some(&Sexp::Atom("_".into()))
                ) || sort == &Sexp::Atom("Bool".into());
                if is_bitvec {
                    info.declares
                        .push(format!("(declare-const {} {})", var, sort));
                } else {
                    exclude(&mut info, format!("symbolic non-bitvector state: {}", sort));
                }
            }
            "define-const" => {
                let (Some(Sexp::Atom(var)), Some(expr)) = (items.get(1), items.get(2)) else {
                    continue;
                };
                defined_vars.insert(var.clone());
                info.defines.push((var.clone(), expr.to_string()));
            }
            "assert" => {
                if let Some(expr) = items.get(1) {
                    info.asserts.push(expr.to_string());
                }
            }
            // `(read-mem value kind address bytes [tag])`
            // `(write-mem success kind address data bytes [tag])`
            // Only plain accesses relate to the TMDL flat memory; reads are
            // constrained against the initial array, so a read after a write
            // (no such instruction yet) would be unsound and is excluded.
            "read-mem" | "write-mem" => {
                let (Some(kind), Some(address), Some(payload), Some(Sexp::Atom(bytes))) = (
                    items.get(2),
                    items.get(3),
                    items.get(if head == "read-mem" { 1 } else { 4 }),
                    items.get(if head == "read-mem" { 4 } else { 5 }),
                ) else {
                    exclude(&mut info, format!("malformed {} event", head));
                    continue;
                };
                if !is_plain_access(kind) {
                    exclude(&mut info, format!("non-plain memory access {}", kind));
                    continue;
                }
                let Ok(bytes @ (1 | 2 | 4 | 8)) = bytes.parse::<u32>() else {
                    exclude(&mut info, format!("unsupported access width {}", bytes));
                    continue;
                };
                let access = MemAccess {
                    value: payload.to_string(),
                    address: address.to_string(),
                    bytes,
                };
                if head == "read-mem" {
                    if !info.mem_writes.is_empty() {
                        exclude(&mut info, "memory read after write".to_string());
                        continue;
                    }
                    info.mem_reads.push(access);
                } else {
                    info.mem_writes.push(access);
                }
            }
            _ => {}
        }
    }
    // A completing x86 instruction always advances the PC; a path that never
    // writes it faulted or decoded to a different instruction (an artifact of
    // the model's forking address decode), so it cannot be checked against TMDL.
    if spec.requires_pc_write
        && info.excluded.is_none()
        && !info.writes.contains_key(&MappedReg::Pc)
        && !info.writes.contains_key(&MappedReg::NextPc)
    {
        exclude(&mut info, "incomplete path (no PC write)".to_string());
    }
    info
}

// ---------------------------------------------------------------------------
// Equivalence query construction
// ---------------------------------------------------------------------------

enum QueryGoal<'a> {
    /// Find a state where TMDL and Sail disagree on the final state.
    Equivalence,
    /// Find a state where the path's written trap cause is outside the set
    /// the TMDL behaviors model (`sat` = access-fault path, excluded).
    UnmodeledCause { cause: &'a str, causes: &'a [u64] },
}

fn build_query(
    spec: &IsaSpec,
    smt: &str,
    instr: &Instruction,
    case: &[u64],
    trace: &TraceInfo,
    goal: QueryGoal<'_>,
) -> String {
    let xlen = spec.xlen;
    let mut q = String::from(smt);
    q.push_str("\n(declare-const st0 TMDLState)\n");
    // No reservation is held on entry (a hart starts with no LR outstanding).
    q.push_str("(assert (not (resv st0)))\n");
    let args = operand_smt_args(spec, instr, case);
    let call = if args.is_empty() {
        format!("(execute_{} st0)", instr.name)
    } else {
        format!("(execute_{} st0 {})", instr.name, args)
    };
    let _ = writeln!(q, "(define-fun st1 () TMDLState {})", call);

    // Fetch invariant: PC is 4-byte aligned (no compressed instructions). x86
    // pins the PC to a concrete aligned value in its config instead.
    if spec.align_pc {
        q.push_str("(assert (= ((_ extract 1 0) (pc st0)) #b00))\n");
    }

    // A value is (low-half) canonical when bits 63..47 are all zero: a valid
    // user x86-64 linear address that the model's 52-bit physical masking leaves
    // unchanged. Non-canonical accesses/jumps `#GP`, which TMDL's flat model
    // does not track.
    let canonical = |v: &str| format!("(= ((_ extract 63 47) {v}) (_ bv0 17))");

    if spec.canonical_addrs {
        // Any address the branch target resolves to (an indirect jump register,
        // a `ret`'s loaded return address, a `call` displacement) is assumed
        // canonical, so the model's 48-bit-truncated PC equals TMDL's full one.
        if instr.writes_pc {
            let _ = writeln!(q, "(assert {})", canonical("(pc st1)"));
        }
        // Each memory-access effective address is assumed to sit below 2^46 (a
        // stricter canonical form): the model's 48-bit sign-masking then leaves
        // it unchanged, and a multi-byte access cannot straddle the 2^47
        // canonical boundary (where the model sign-extends but TMDL's flat
        // 64-bit memory does not).
        for addr in mem_addr_exprs(smt, instr, case, spec) {
            let _ = writeln!(q, "(assert (= ((_ extract 63 46) {addr}) (_ bv0 18)))");
        }
    } else {
        // RISC-V (until the C extension is modeled): registers feeding an
        // indirect jump are assumed 4-byte aligned, so misaligned-fetch trap
        // paths are vacuous.
        for i in pc_source_reg_operands(smt, instr) {
            if let OperandKind::Reg { class, idx_width } = &instr.operands[i].1 {
                let reg = format!("(read_{} st0 (_ bv{} {}))", class, case[i], idx_width);
                let _ = writeln!(q, "(assert (= ((_ extract 1 0) {reg}) #b00))");
            }
        }
    }

    for decl in &trace.declares {
        q.push_str(decl);
        q.push('\n');
    }
    let gw = spec.gpr_idx_width();
    let width_bytes = instr.width_bytes();
    let slot_access = |i: usize, state: &str| {
        let (_, class, slot, w) = spec.extra_regs[i];
        format!("(read_{} {} (_ bv{} {}))", class, state, slot, w)
    };
    // A read of a bitfield flag register pins each mapped bit of the symbolic
    // initial value to that flag slot's initial TMDL state.
    if let Some((_, class, bit_map)) = spec.flag_reg {
        let idx_w = bit_map
            .iter()
            .map(|(s, _)| 64 - s.leading_zeros())
            .max()
            .unwrap_or(1);
        for (slot, var, bit) in &trace.flag_reads {
            let _ = writeln!(
                q,
                "(assert (= ((_ extract {bit} {bit}) {var}) (read_{class} st0 (_ bv{slot} {idx_w}))))",
            );
        }
    }
    for (reg, var) in &trace.reads {
        let init = match reg {
            MappedReg::X(n) => format!("(read_gpr st0 (_ bv{} {}))", n, gw),
            MappedReg::Pc => "(pc st0)".to_string(),
            MappedReg::NextPc => format!("(bvadd (pc st0) (_ bv{width_bytes} {xlen}))"),
            MappedReg::Slot(i) => slot_access(*i, "st0"),
        };
        let _ = writeln!(q, "(assert (= {} {}))", var, init);
    }

    let mut final_eq: Vec<String> = (0..spec.reg_count)
        .filter(|n| Some(*n) != spec.zero_reg)
        .map(|n| {
            let sail = trace
                .writes
                .get(&MappedReg::X(n))
                .cloned()
                .unwrap_or_else(|| format!("(read_gpr st0 (_ bv{} {}))", n, gw));
            format!("(= (read_gpr st1 (_ bv{} {})) {})", n, gw, sail)
        })
        .collect();
    // Flag equivalence, only where the TMDL behavior models flags (its
    // execute writes the flag class). The ALU ops deliberately leave flags
    // unmodeled, so Sail's flag writes are ignored for them.
    if let Some((_, class, bit_map)) = spec.flag_reg {
        if tmdl_writes_flags(smt, instr, class) {
            let idx_w = bit_map
                .iter()
                .map(|(s, _)| 64 - s.leading_zeros())
                .max()
                .unwrap_or(1);
            for (slot, _) in bit_map {
                let sail = trace
                    .flag_writes
                    .get(slot)
                    .cloned()
                    .unwrap_or_else(|| format!("(read_{class} st0 (_ bv{slot} {idx_w}))"));
                final_eq.push(format!(
                    "(= (read_{class} st1 (_ bv{slot} {idx_w})) {sail})"
                ));
            }
        }
    }
    // Extra mapped state, deduplicated by underlying TMDL slot since several
    // Sail names may alias one slot (SP_ELx); a write through any alias is
    // the slot's final value.
    let mut seen_slots = std::collections::HashSet::new();
    for (i, (_, class, slot, _)) in spec.extra_regs.iter().enumerate() {
        if !seen_slots.insert((*class, *slot)) {
            continue;
        }
        let sail = spec
            .extra_regs
            .iter()
            .enumerate()
            .filter(|(_, (_, c, s, _))| c == class && s == slot)
            .find_map(|(j, _)| trace.writes.get(&MappedReg::Slot(j)).cloned())
            .unwrap_or_else(|| slot_access(i, "st0"));
        final_eq.push(format!("(= {} {})", slot_access(i, "st1"), sail));
    }
    // Models with a delayed PC (RISC-V `nextPC`) announce taken branches
    // there; the ARM model writes the PC register directly.
    let sail_pc = trace
        .writes
        .get(&MappedReg::NextPc)
        .or_else(|| trace.writes.get(&MappedReg::Pc));
    let mut asserts = trace.asserts.clone();

    // Memory: Sail's read values come from TMDL's initial array, and the
    // final array must equal the initial one with Sail's writes applied
    // little-endian byte by byte (the `write_mem_*` convention). Both the
    // constraints and the equality can mention `define-const` variables, so
    // they live inside the let chain with the path asserts.
    for read in &trace.mem_reads {
        asserts.push(format!(
            "(= {} (read_mem_{} st0 {}))",
            read.value, read.bytes, read.address
        ));
    }
    if trace.mem_writes.is_empty() {
        // Both sides reduce to the untouched initial array; congruence
        // closes this cheaply.
        final_eq.push("(= (mem st1) (mem st0))".to_string());
    } else {
        // Whole-array equality of two store chains makes z3 enumerate index
        // aliasing through the (long) address define-chains — minutes per
        // query. Equisatisfiable select formulation instead: equality at
        // every written slot, plus a frame condition at one fresh index
        // (the extensionality witness), each a directed bitvector goal.
        let mut sail_mem = "(mem st0)".to_string();
        let mut slots = Vec::new();
        for write in &trace.mem_writes {
            for i in 0..write.bytes {
                let slot = if i == 0 {
                    write.address.clone()
                } else {
                    format!("(bvadd {} (_ bv{} {}))", write.address, i, xlen)
                };
                sail_mem = format!(
                    "(store {} {} ((_ extract {} {}) {}))",
                    sail_mem,
                    slot,
                    i * 8 + 7,
                    i * 8,
                    write.value
                );
                slots.push(slot);
            }
        }
        for slot in &slots {
            final_eq.push(format!(
                "(= (select (mem st1) {}) (select {} {}))",
                slot, sail_mem, slot
            ));
        }
        let _ = writeln!(q, "(declare-const mem_frame_idx (_ BitVec {xlen}))");
        let written = slots
            .iter()
            .map(|slot| format!("(= mem_frame_idx {})", slot))
            .collect::<Vec<_>>()
            .join(" ");
        final_eq.push(format!(
            "(or {} (= (select (mem st1) mem_frame_idx) (select (mem st0) mem_frame_idx)))",
            written
        ));
    }

    match sail_pc {
        Some(target) => {
            // TMDL encodes fall-through as "PC untouched", while current Sail
            // models write the next PC unconditionally, so compare against
            // TMDL's effective next PC. A self-jump (target == initial PC) is
            // indistinguishable from fall-through under this convention;
            // assume it away rather than reporting a fake divergence.
            asserts.push(format!("(distinct {} (pc st0))", target));
            final_eq.push(format!(
                "(= (ite (= (pc st1) (pc st0)) (bvadd (pc st0) (_ bv{width_bytes} {xlen})) (pc st1)) {})",
                target
            ));
        }
        None => final_eq.push("(= (pc st1) (pc st0))".to_string()),
    }

    let negated_goal = match goal {
        QueryGoal::Equivalence => format!("(not (and {}))", final_eq.join("\n  ")),
        QueryGoal::UnmodeledCause { cause, causes } => format!(
            "(not (or {}))",
            causes
                .iter()
                .map(|c| format!("(= {} (_ bv{} {}))", cause, c, xlen))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    };
    let mut body = format!(
        "(and {} {})",
        if asserts.is_empty() {
            "true".to_string()
        } else {
            asserts.join(" ")
        },
        negated_goal
    );
    for (var, expr) in trace.defines.iter().rev() {
        body = format!("(let (({} {}))\n{})", var, expr, body);
    }
    let _ = writeln!(q, "(assert {})", body);
    q.push_str("(check-sat)\n");

    if matches!(goal, QueryGoal::Equivalence) {
        // Counterexample probes, only evaluated on `sat`.
        let mut probes: Vec<String> = vec!["(pc st0)".into(), "(pc st1)".into()];
        for n in (0..spec.reg_count).filter(|n| Some(*n) != spec.zero_reg) {
            probes.push(format!("(read_gpr st0 (_ bv{} {}))", n, gw));
        }
        let _ = writeln!(q, "(get-value ({}))", probes.join(" "));
    }
    q
}

// ---------------------------------------------------------------------------
// Per-instruction driver and reporting
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Report {
    verified: usize,
    failed: usize,
    unknown: usize,
    excluded_paths: usize,
    excluded_reasons: HashMap<String, usize>,
    unsupported: Vec<String>,
    failures: Vec<String>,
}

impl Report {
    fn print(&self) {
        println!("\n=== TMDL vs Sail SMT equivalence ===");
        println!("verified paths:  {}", self.verified);
        println!("divergences:     {}", self.failed);
        println!("solver unknown:  {}", self.unknown);
        println!(
            "excluded paths:  {} (outside the machine-mode/no-trap assumptions)",
            self.excluded_paths
        );
        let mut reasons: Vec<_> = self.excluded_reasons.iter().collect();
        reasons.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (reason, n) in reasons {
            println!("  {:5}x {}", n, reason);
        }
        if !self.unsupported.is_empty() {
            println!(
                "not modeled in SMT (skipped): {}",
                self.unsupported.join(", ")
            );
        }
        for failure in &self.failures {
            println!("\n{}", failure);
        }
    }
}

fn verify_instruction(
    tools: &Tools,
    spec: &IsaSpec,
    out_dir: &Path,
    smt: &str,
    instr: &Instruction,
    report: &mut Report,
) -> anyhow::Result<()> {
    let cases = operand_cases(spec, instr);
    let words = encode_words(tools, spec, out_dir, smt, instr, &cases)?;
    // x86 encodings are lossless (register indices and immediates are stored
    // verbatim), so the decoded operands equal the inputs; the wide, variable
    // `decode_*` round-trip is only needed for the lossy fixed-width ISAs.
    let cases = if spec.reg_names.is_empty() {
        decode_operands(tools, spec, out_dir, smt, instr, &words)?
    } else {
        cases
    };
    print!("{:24}", instr.name);
    let mut line = String::new();

    for (case, word) in cases.iter().zip(&words) {
        let Some(raw) = sail_traces(tools, spec, out_dir, instr, *word)? else {
            report.excluded_paths += 1;
            *report
                .excluded_reasons
                .entry(format!(
                    "{}: isla-footprint failed or timed out ({:#014x})",
                    instr.name, word
                ))
                .or_default() += 1;
            line.push('I');
            continue;
        };
        let traces: Vec<Vec<Sexp>> = parse_sexps(&raw)
            .into_iter()
            .filter_map(|s| match s {
                Sexp::List(items) if items.first() == Some(&Sexp::Atom("trace".into())) => {
                    Some(items[1..].to_vec())
                }
                _ => None,
            })
            .collect();
        if traces.is_empty() {
            report.failed += 1;
            report.failures.push(format!(
                "{} {:?} ({:#010x}): Sail produced no execution path (illegal instruction?)",
                instr.name, case, word
            ));
            line.push('E');
            continue;
        }

        for (path_idx, events) in traces.iter().enumerate() {
            let info = analyze_trace(spec, events);
            if let Some(reason) = &info.excluded {
                report.excluded_paths += 1;
                *report
                    .excluded_reasons
                    .entry(format!("{}: {}", instr.name, reason))
                    .or_default() += 1;
                line.push('-');
                continue;
            }
            // A path writing a trap cause TMDL does not model (access fault)
            // lies outside the all-of-memory-is-RAM assumption.
            let written_cause = spec.trap_cause.and_then(|(cause_reg, causes)| {
                let slot = spec
                    .extra_regs
                    .iter()
                    .position(|(n, _, _, _)| *n == cause_reg)?;
                Some((info.writes.get(&MappedReg::Slot(slot))?, causes))
            });
            if let Some((cause, causes)) = written_cause {
                let probe = build_query(
                    spec,
                    smt,
                    instr,
                    case,
                    &info,
                    QueryGoal::UnmodeledCause { cause, causes },
                );
                let probe_path = out_dir.join("queries").join(format!(
                    "{}_{:08x}_p{}_cause.smt2",
                    instr.name, word, path_idx
                ));
                std::fs::write(&probe_path, &probe)?;
                let output = Command::new(&tools.z3)
                    .arg("-smt2")
                    .arg("-T:240")
                    .arg(&probe_path)
                    .output()?;
                if String::from_utf8_lossy(&output.stdout).starts_with("sat") {
                    report.excluded_paths += 1;
                    *report
                        .excluded_reasons
                        .entry(format!(
                            "{}: trap cause outside the modeled set (access fault path)",
                            instr.name
                        ))
                        .or_default() += 1;
                    line.push('-');
                    continue;
                }
            }
            let query = build_query(spec, smt, instr, case, &info, QueryGoal::Equivalence);
            let query_path = out_dir
                .join("queries")
                .join(format!("{}_{:08x}_p{}.smt2", instr.name, word, path_idx));
            std::fs::write(&query_path, &query)?;
            let output = Command::new(&tools.z3)
                .arg("-smt2")
                .arg("-T:240")
                .arg(&query_path)
                .output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            if stdout.starts_with("unsat") {
                report.verified += 1;
                line.push('.');
            } else if stdout.starts_with("sat") {
                report.failed += 1;
                line.push('X');
                let model = stdout.lines().skip(1).collect::<Vec<_>>().join("\n");
                report.failures.push(format!(
                    "DIVERGENCE {} operands {:?} word {:#010x} path {} (query: {})\n\
                     counterexample (initial pc, final pc, gprs):\n{}",
                    instr.name,
                    case,
                    word,
                    path_idx,
                    query_path.display(),
                    model
                ));
            } else {
                report.unknown += 1;
                line.push('?');
            }
        }
    }
    println!("{}", line);
    Ok(())
}
