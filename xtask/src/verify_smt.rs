//! SMT equivalence checking of TMDL instruction semantics against the Sail
//! model of the target architecture (the architecture's golden model).
//!
//! For every supported TMDL instruction and a set of concrete operand
//! assignments:
//!   1. the instruction word is computed and decoded from TMDL's structured
//!      encoding fields without a solver round-trip, so encoding bugs remain
//!      covered and lossy immediates are checked at representable values;
//!   2. the pinned `isla-lib` loads the Sail snapshot once and symbolically
//!      executes instruction-word batches over a fully symbolic register
//!      state, returning structured events for every path;
//!   3. for each path, Bitwuzla (with z3 fallback and `sat` cross-checking) is
//!      asked for a register state where TMDL and Sail disagree on the final
//!      GPRs or PC. `unsat` proves agreement for ALL
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
//! External inputs: Bitwuzla, z3, a Sail snapshot, and an Isla config per ISA.
//! The Isla library revision is pinned in `utils/verify/Cargo.toml` and snapshots are downloaded on
//! demand. Override locations with `TIR_ISLA_SNAPSHOT`, `TIR_ISLA_CONFIG`,
//! `TIR_BITWUZLA`, and `TIR_Z3`; `TIR_ISLA_SNAPSHOTS_REF` overrides the snapshot pin.
//! `TIR_VERIFY_SMT_FILTER=add,sub` restricts the instruction set.
//!
//! The x86 snapshot is translated from the ACL2-derived
//! `sail-x86-from-acl2` model (see that repo's `model/Makefile` `x86.ir`
//! target, spliced with `test-generation-patches/isla_footprint.sail`) and is
//! downloaded from the published isla-snapshots mirror. Additional x86 assumptions
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
use std::time::Instant;

use crate::utils::{download_file, project_root};
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
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
    /// Snapshot repository on GitHub.
    snapshot_repo: &'static str,
    /// Default repository ref, overridden by `TIR_ISLA_SNAPSHOTS_REF`.
    snapshot_ref: &'static str,
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
    initial_registers: &'static [&'static str],
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
    /// Simplify Isla traces. The x86 traces must stay
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
    /// Optional local snapshot path, bypassing the download.
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
        snapshot_repo: "rems-project/isla-snapshots",
        snapshot_ref: ISLA_SNAPSHOTS_PIN,
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
        initial_registers: &["cur_privilege=Machine"],
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
        snapshot_repo: "rems-project/isla-snapshots",
        snapshot_ref: ISLA_SNAPSHOTS_PIN,
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
        initial_registers: &["cur_privilege=Machine"],
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
        snapshot_repo: "rems-project/isla-snapshots",
        snapshot_ref: ISLA_SNAPSHOTS_PIN,
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
        initial_registers: &[],
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
        snapshot: "x86.ir",
        snapshot_repo: "frontiers-labs/isla-snapshots",
        snapshot_ref: "master",
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
        initial_registers: &[],
        reg_names: X86_REG_NAMES,
        // rflags bit layout: cf=0, zf=6, sf=7, of=11 (Intel SDM). TMDL EFLAGS
        // slots cf=0, zf=1, sf=2, of=3 (declaration order).
        flag_reg: Some(("rflags", "eflags", &[(0, 0), (1, 6), (2, 7), (3, 11)])),
        simplify: false,
        align_pc: false,
        canonical_addrs: true,
        requires_pc_write: true,
        local_snapshot: None,
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

pub fn verify_smt(sh: &Shell, isa: &str, args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let spec = ISA_SPECS.iter().find(|s| s.name == isa).ok_or_else(|| {
        anyhow!("unsupported ISA {isa}; available: riscv64, riscv32, armv8, x86_64")
    })?;
    let shard = parse_shard(args)?;
    let tools = Tools::ensure(sh, spec)?;
    let root = project_root();
    let out_dir = root.join("target/verify/smt").join(spec.name);
    std::fs::create_dir_all(out_dir.join("cache"))?;
    std::fs::create_dir_all(out_dir.join("queries"))?;

    let smt_path = out_dir.join(format!("{}.smt2", spec.name));
    generate_tmdl_smt(sh, spec, &root, &smt_path)?;
    let metadata_path = smt_path.with_extension("metadata.json");
    let inventory = parse_inventory(&std::fs::read_to_string(metadata_path)?)?;
    anyhow::ensure!(
        inventory.isa == spec.tmdl_isa && inventory.dialect == spec.dialect,
        "SMT metadata target mismatch: expected {}/{}, got {}/{}",
        spec.tmdl_isa,
        spec.dialect,
        inventory.isa,
        inventory.dialect
    );
    let instructions = inventory.instructions;
    let filter: Option<Vec<String>> = std::env::var("TIR_VERIFY_SMT_FILTER")
        .ok()
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect());

    let mut report = Report::new(spec.name, shard);
    let started = Instant::now();
    let mut selected = Vec::new();

    for instr in &instructions {
        if filter.as_ref().is_some_and(|f| !f.contains(&instr.name)) {
            continue;
        }
        if shard.is_some_and(|shard| !shard.contains(&instr.name)) {
            continue;
        }
        if spec.name.starts_with("riscv") && instr.name == "vsetvli" {
            report
                .unsupported
                .push("vsetvli (RVV disabled in Sail configuration)".to_string());
            continue;
        }
        if spec.name.starts_with("riscv") && instr.width_bits == 16 {
            report.unsupported.push(format!(
                "{} (compressed extension disabled in Sail configuration)",
                instr.name
            ));
            continue;
        }
        if spec.name.starts_with("riscv")
            && matches!(instr.name.as_str(), "envcall" | "envbreak" | "cenvbreak")
        {
            report.unsupported.push(format!(
                "{} (terminating Sail trace omits architectural trap state)",
                instr.name
            ));
            continue;
        }
        if !instr.supported {
            report.unsupported.push(instr.name.clone());
            continue;
        }
        // Atomics (A extension) reference the reservation state, whose mapping
        // onto Sail's reservation register is follow-up work (see module docs);
        // skip them until that is enabled.
        if instr.uses_reservation {
            report.unsupported.push(format!(
                "{} (atomic; Sail reservation mapping is follow-up)",
                instr.name
            ));
            continue;
        }
        if instr.flat_execute.is_none() {
            report
                .unsupported
                .push(format!("{} (no flat SMT behavior)", instr.name));
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
        selected.push(instr);
    }

    for instr in selected {
        let (instruction_report, timing, line) =
            verify_instruction(&tools, spec, &out_dir, &inventory.flat, instr)?;
        println!("{line}");
        report.merge(instruction_report);
        report.instructions.push(timing);
    }
    report.wall_ms = started.elapsed().as_millis();

    report.print();
    let report_path = out_dir.join("report.json");
    std::fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("JSON report:      {}", report_path.display());
    if report.failed > 0 {
        anyhow::bail!(
            "SMT equivalence check found {} divergence(s)",
            report.failed
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Serialize)]
struct Shard {
    index: u64,
    count: u64,
}

impl Shard {
    fn contains(self, name: &str) -> bool {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        hasher.finish() % self.count == self.index
    }
}

fn parse_shard(mut args: impl Iterator<Item = String>) -> anyhow::Result<Option<Shard>> {
    let Some(flag) = args.next() else {
        return Ok(None);
    };
    anyhow::ensure!(flag == "--shard", "unknown verify option {flag}");
    let value = args.next().ok_or_else(|| anyhow!("--shard requires k/N"))?;
    anyhow::ensure!(args.next().is_none(), "unexpected verify arguments");
    let (index, count) = value
        .split_once('/')
        .ok_or_else(|| anyhow!("invalid shard {value}; expected k/N"))?;
    let shard = Shard {
        index: index.parse()?,
        count: count.parse()?,
    };
    anyhow::ensure!(
        shard.count > 0 && shard.index < shard.count,
        "invalid shard {value}"
    );
    Ok(Some(shard))
}

struct Tools {
    snapshot: PathBuf,
    isla_config: PathBuf,
    bitwuzla: Option<PathBuf>,
    z3: PathBuf,
    verifier: tir_verify::Verifier,
}

impl Tools {
    /// Resolve the external tools, fetching anything that is not overridden
    /// by an environment variable.
    fn ensure(sh: &Shell, spec: &IsaSpec) -> anyhow::Result<Self> {
        let snapshot = match (std::env::var("TIR_ISLA_SNAPSHOT"), spec.local_snapshot) {
            (Ok(path), _) => path.into(),
            // Allow local snapshots for development and CI overrides.
            (Err(_), Some(local)) => PathBuf::from(local),
            (Err(_), None) => ensure_snapshot(sh, spec)?,
        };
        let isla_config = std::env::var("TIR_ISLA_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| project_root().join("xtask").join(spec.config));
        let threads = std::env::var("TIR_VERIFY_SMT_ISLA_JOBS")
            .ok()
            .and_then(|jobs| jobs.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1)
            });
        let initial_registers = spec
            .initial_registers
            .iter()
            .map(|assignment| assignment.to_string())
            .collect::<Vec<_>>();
        let verifier = tir_verify::Verifier::load(
            &snapshot,
            &isla_config,
            &initial_registers,
            "isla_footprint_no_init",
            threads,
            60,
            spec.simplify,
        )?;
        let bitwuzla = std::env::var("TIR_BITWUZLA")
            .map(PathBuf::from)
            .ok()
            .or_else(|| {
                Command::new("bitwuzla")
                    .arg("--version")
                    .output()
                    .ok()
                    .map(|_| PathBuf::from("bitwuzla"))
            });
        Ok(Tools {
            snapshot,
            isla_config,
            bitwuzla,
            z3: std::env::var("TIR_Z3")
                .unwrap_or_else(|_| "z3".to_string())
                .into(),
            verifier,
        })
    }
}

/// Pinned isla-snapshots commit: the models the specs and configs were
/// validated against. A floating ref breaks silently when upstream swaps
/// model generations (rv32d.ir became a new-interface build on 2026-06-02).
const ISLA_SNAPSHOTS_PIN: &str = "d8b31014643035a3b11071e56ef30001de3f52ab";

fn ensure_snapshot(sh: &Shell, spec: &IsaSpec) -> anyhow::Result<PathBuf> {
    let file = spec.snapshot;
    let snap_ref =
        std::env::var("TIR_ISLA_SNAPSHOTS_REF").unwrap_or_else(|_| spec.snapshot_ref.to_string());
    let dest = project_root()
        .join("target/verify/snapshots")
        .join(snap_ref.replace('/', "-"))
        .join(file);
    let url = format!(
        "https://github.com/{}/raw/{snap_ref}/{file}",
        spec.snapshot_repo
    );
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
// Instruction inventory (from the generated JSON sidecar)
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
    write_classes: Vec<String>,
    uses_reservation: bool,
    pc_source_operands: Vec<usize>,
    memory_accesses: Vec<MemoryAccessMetadata>,
    encoding: Vec<EncodingField>,
    flat_execute: Option<HashMap<String, String>>,
}

#[derive(Clone, Debug, Deserialize)]
struct MemoryAccessMetadata {
    flat_address: String,
}

#[derive(Clone, Debug)]
struct EncodingField {
    word_low: u32,
    word_high: u32,
    operand_index: Option<usize>,
    operand_low: u32,
    value: u128,
}

#[derive(Deserialize)]
struct MetadataFile {
    version: u32,
    isa: String,
    dialect: String,
    flat_state: Vec<FlatStateField>,
    register_classes: Vec<RegisterClassMetadata>,
    instructions: Vec<RawInstruction>,
}

#[derive(Deserialize)]
struct RawInstruction {
    name: String,
    writes_pc: bool,
    width_bits: u32,
    operands: Vec<RawOperand>,
    supported: bool,
    write_classes: Vec<String>,
    uses_reservation: bool,
    pc_source_operands: Vec<usize>,
    memory_accesses: Vec<MemoryAccessMetadata>,
    trap_kinds: Vec<String>,
    encoding: Vec<RawEncodingField>,
    flat_execute: Option<HashMap<String, String>>,
}

#[derive(Clone, Deserialize)]
struct FlatStateField {
    name: String,
    sort: String,
}

#[derive(Clone, Deserialize)]
struct RegisterClassMetadata {
    name: String,
    storage: String,
    index_width: u32,
    value_width: u32,
    storage_width: u32,
    zero_index: Option<u64>,
    bit_offset: u32,
}

#[derive(Deserialize)]
struct RawOperand {
    name: String,
    kind: String,
    class: Option<String>,
    width: u32,
}

#[derive(Deserialize)]
struct RawEncodingField {
    word_low: u32,
    word_high: u32,
    operand: Option<String>,
    operand_low: u32,
    value: String,
}

impl Instruction {
    fn width_bytes(&self) -> u32 {
        self.width_bits / 8
    }
}

struct Inventory {
    isa: String,
    dialect: String,
    flat: FlatModel,
    instructions: Vec<Instruction>,
}

#[derive(Clone)]
struct FlatModel {
    fields: Vec<FlatStateField>,
    classes: HashMap<String, RegisterClassMetadata>,
}

fn parse_inventory(json: &str) -> anyhow::Result<Inventory> {
    let metadata: MetadataFile = serde_json::from_str(json)?;
    anyhow::ensure!(metadata.version == 1, "unsupported SMT metadata version");
    let instructions = metadata
        .instructions
        .into_iter()
        .map(|raw| {
            anyhow::ensure!(
                raw.trap_kinds.iter().all(|kind| !kind.is_empty()),
                "instruction {} has an empty trap kind",
                raw.name
            );
            let operands = raw
                .operands
                .into_iter()
                .map(|operand| {
                    let kind = match operand.kind.as_str() {
                        "register" => OperandKind::Reg {
                            class: operand
                                .class
                                .ok_or_else(|| anyhow!("register operand without class"))?,
                            idx_width: operand.width,
                        },
                        "bits" => OperandKind::Bits(operand.width),
                        "int" => OperandKind::Int,
                        kind => anyhow::bail!("unknown operand kind {kind}"),
                    };
                    Ok((operand.name, kind))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let encoding = raw
                .encoding
                .into_iter()
                .map(|field| {
                    let operand_index = field
                        .operand
                        .map(|name| {
                            operands
                                .iter()
                                .position(|(operand, _)| operand == &name)
                                .ok_or_else(|| {
                                    anyhow!("encoding references unknown operand {name}")
                                })
                        })
                        .transpose()?;
                    Ok(EncodingField {
                        word_low: field.word_low,
                        word_high: field.word_high,
                        operand_index,
                        operand_low: field.operand_low,
                        value: field.value.parse()?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Instruction {
                name: raw.name,
                writes_pc: raw.writes_pc,
                width_bits: raw.width_bits,
                operands,
                supported: raw.supported,
                write_classes: raw.write_classes,
                uses_reservation: raw.uses_reservation,
                pc_source_operands: raw.pc_source_operands,
                memory_accesses: raw.memory_accesses,
                encoding,
                flat_execute: raw.flat_execute,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Inventory {
        isa: metadata.isa,
        dialect: metadata.dialect,
        flat: FlatModel {
            fields: metadata.flat_state,
            classes: metadata
                .register_classes
                .into_iter()
                .map(|class| (class.name.clone(), class))
                .collect(),
        },
        instructions,
    })
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

fn operand_case_is_valid(spec: &IsaSpec, instr: &Instruction, case: &[u64]) -> bool {
    let value = |index: usize| case[index];
    match (spec.name, instr.name.as_str()) {
        ("armv8", "loaddoublewordpreindex" | "loaddoublewordpostindex") => value(0) != value(1),
        ("armv8", "storedoublewordpreindex") => value(0) != value(1),
        ("armv8", "loadpair") => value(0) != value(1),
        ("armv8", "loadpairpreindex" | "loadpairpostindex") => {
            value(0) != value(1) && value(0) != value(2) && value(1) != value(2)
        }
        ("armv8", "storepairpreindex") => value(0) != value(2) && value(1) != value(2),
        (name, "cmove" | "cadd") if name.starts_with("riscv") => value(0) != 0 && value(1) != 0,
        (name, "cjumpreg" | "cjumpandlinkreg") if name.starts_with("riscv") => value(0) != 0,
        (name, "caddimm" | "cloadimm") if name.starts_with("riscv") => value(0) != 0,
        (name, "cloadupperimm") if name.starts_with("riscv") => {
            value(0) != 0 && value(0) != 2 && value(1) != 0
        }
        (name, "caddimm16sp") if name.starts_with("riscv") => value(0) != 0,
        ("riscv32", "cshiftleftlogicalimm") => value(0) != 0 && value(1) < 32,
        (name, "cshiftleftlogicalimm") if name.starts_with("riscv") => value(0) != 0,
        (name, "cloadwordsp" | "cloaddoublesp") if name.starts_with("riscv") => value(0) != 0,
        (
            "x86_64",
            "movload" | "mov32load" | "movsxdload" | "movsx8load" | "movsx16load" | "movzx8load"
            | "movzx16load",
        ) => !matches!(value(1) & 7, 4 | 5),
        ("x86_64", "movstore" | "mov32store" | "mov16store" | "mov8store") => {
            !matches!(value(0) & 7, 4 | 5)
        }
        ("x86_64", "movstoredisp") => value(0) & 7 != 4,
        _ => true,
    }
}

fn operand_smt_literal(spec: &IsaSpec, kind: &OperandKind, value: u64) -> String {
    match kind {
        OperandKind::Reg { idx_width, .. } => format!("(_ bv{} {})", value, idx_width),
        _ => format!("(_ bv{} {})", value, spec.xlen),
    }
}

fn mem_addr_exprs(instr: &Instruction, case: &[u64], spec: &IsaSpec) -> Vec<String> {
    let bindings = instr
        .operands
        .iter()
        .zip(case)
        .map(|((name, kind), value)| {
            format!("({name} {})", operand_smt_literal(spec, kind, *value))
        })
        .collect::<Vec<_>>();
    instr
        .memory_accesses
        .iter()
        .map(|access| {
            if bindings.is_empty() {
                access.flat_address.clone()
            } else {
                format!("(let ({}) {})", bindings.join(" "), access.flat_address)
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Native concrete encoding/decoding from the structured TMDL bit-field map
// ---------------------------------------------------------------------------

fn bit_mask(width: u32) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

fn encode_words(instr: &Instruction, cases: &[Vec<u64>]) -> Vec<u128> {
    cases
        .iter()
        .map(|case| {
            instr.encoding.iter().fold(0u128, |word, field| {
                let width = field.word_high - field.word_low + 1;
                let source = field
                    .operand_index
                    .map_or(field.value, |index| u128::from(case[index]));
                let piece = (source >> field.operand_low) & bit_mask(width);
                word | (piece << field.word_low)
            })
        })
        .collect()
}

/// Operand values recovered by decoding the instruction words back, so the
/// equivalence check uses what the encoding can express: lossy immediate
/// fields drop bits (branch immediates force bit 0, ARM unsigned-offset
/// loads/stores store the byte offset scaled down by the access size).
fn decode_operands(instr: &Instruction, words: &[u128]) -> Vec<Vec<u64>> {
    words
        .iter()
        .map(|word| {
            let mut operands = vec![0u64; instr.operands.len()];
            for field in &instr.encoding {
                let Some(index) = field.operand_index else {
                    continue;
                };
                let width = field.word_high - field.word_low + 1;
                let piece = (word >> field.word_low) & bit_mask(width);
                operands[index] |= (piece << field.operand_low) as u64;
            }
            operands
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Isla library execution (cached per instruction word)
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

fn sail_traces(
    tools: &Tools,
    out_dir: &Path,
    instr: &Instruction,
    words: &[u128],
) -> anyhow::Result<HashMap<u128, Option<Vec<Vec<tir_verify::TraceEvent>>>>> {
    let fingerprint = cache_fingerprint(tools);
    let cache_path = |word| {
        out_dir
            .join("cache")
            .join(format!("{word:020x}-{fingerprint:016x}.json"))
    };
    let mut result = HashMap::new();
    let mut missing = Vec::new();
    for &word in words {
        if result.contains_key(&word) {
            continue;
        }
        match std::fs::read(cache_path(word)) {
            Ok(json) => {
                result.insert(word, Some(serde_json::from_slice(&json)?));
            }
            Err(_) => missing.push(word),
        }
    }
    if !missing.is_empty() {
        let widths = vec![instr.width_bits; missing.len()];
        let mut executed = tools.verifier.execute(&missing, &widths)?;
        for word in missing {
            let traces = executed.remove(&word);
            if let Some(traces) = &traces {
                std::fs::write(cache_path(word), serde_json::to_vec(traces)?)?;
            }
            result.insert(word, traces);
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Structured Isla trace analysis
// ---------------------------------------------------------------------------

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

/// Whether a memory event's kind is a plain data access. Old-interface
/// models (RISC-V) use enum atoms (`|Read_plain|`); new-interface models
/// (ARM) embed the whole request struct, whose `access_kind` must be an
/// explicit access of plain variety and normal (non-acquire/release)
/// strength.
fn is_plain_access(kind: &tir_verify::TraceValue) -> bool {
    kind.smt.trim_matches('|').ends_with("_plain")
        || (kind.smt.contains("|AV_plain|")
            && (!kind.smt.contains("|strength|") || kind.smt.contains("|AS_normal|")))
}

/// Whether the value mentions any isla symbolic variable (`vN`). Struct
/// register reads (RISC-V `mip`) wrap their symbolic fields in a
/// `(_ struct ...)` literal, so a bare-atom check is not enough.
/// The payload of a single-field `(_ struct (|bits| value))` literal, as
/// written for Sail bitfield registers; anything else is returned as is.
fn unwrap_bits_struct(value: &tir_verify::TraceValue) -> &tir_verify::TraceValue {
    match value.fields.get("bits") {
        Some(bits) if value.fields.len() == 1 => bits,
        _ => value,
    }
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

fn analyze_trace(spec: &IsaSpec, events: &[tir_verify::TraceEvent]) -> TraceInfo {
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
        match event {
            tir_verify::TraceEvent::ReadRegister {
                name,
                fields,
                value,
            } => {
                if spec.ignore_regs.contains(&name.as_str()) {
                    continue;
                }
                // A bitfield flag register (x86 `rflags`) read: each mapped bit
                // of the symbolic initial value relates to a flag slot.
                if let Some((flag_name, _, bit_map)) = spec.flag_reg {
                    if name == flag_name {
                        let value = unwrap_bits_struct(value);
                        if value.smt.starts_with('v') && !defined_vars.contains(&value.smt) {
                            for (slot, bit) in bit_map {
                                info.flag_reads.push((*slot, value.smt.clone(), *bit));
                            }
                        }
                        continue;
                    }
                }
                if spec.mmio_regs.contains(&name.as_str()) {
                    exclude(
                        &mut info,
                        format!("reads MMIO-backed register {name} (platform memory map)"),
                    );
                    continue;
                }
                if spec.struct_reg == Some(name) {
                    // Mapped fields (NZCV) relate to TMDL state; other fields
                    // (EL, nRW, ...) are pinned by each path's assertions.
                    if let Some(field) = fields.first() {
                        let full = format!("{name}.{field}");
                        if let Some(i) = spec.extra_regs.iter().position(|(n, _, _, _)| *n == full)
                        {
                            let reg = MappedReg::Slot(i);
                            if let Some(field_value) = value.fields.get(field) {
                                if field_value.smt.starts_with('v')
                                    && !info.writes.contains_key(&reg)
                                    && !defined_vars.contains(&field_value.smt)
                                {
                                    info.reads.push((reg, field_value.smt.clone()));
                                }
                            }
                        }
                    }
                    continue;
                }
                // Bitfield registers (RISC-V mstatus, mtvec, mcause) carry
                // their value in a single-field struct literal.
                let value = unwrap_bits_struct(value);
                match map_register(spec, name) {
                    Some(reg) => {
                        let concrete = value.smt.starts_with('#');
                        if !info.writes.contains_key(&reg)
                            && (concrete || !defined_vars.contains(&value.smt))
                        {
                            info.reads.push((reg, value.smt.clone()));
                        }
                    }
                    None if value.symbolic => {
                        exclude(&mut info, format!("reads unmapped register {}", name));
                    }
                    None => {}
                }
            }
            tir_verify::TraceEvent::WriteRegister {
                name,
                fields,
                value,
            } => {
                if spec.ignore_regs.contains(&name.as_str()) {
                    continue;
                }
                // A write of the bitfield flag register: record each mapped bit
                // as that flag slot's final value (last write wins).
                if let Some((flag_name, _, bit_map)) = spec.flag_reg {
                    if name == flag_name {
                        let value = unwrap_bits_struct(value);
                        for (slot, bit) in bit_map {
                            info.flag_writes
                                .insert(*slot, format!("((_ extract {bit} {bit}) {})", value.smt));
                        }
                        continue;
                    }
                }
                if spec.struct_reg == Some(name) {
                    let mapped = fields.first().and_then(|field| {
                        let full = format!("{name}.{field}");
                        let i = spec.extra_regs.iter().position(|(n, _, _, _)| *n == full)?;
                        Some((i, value.fields.get(field)?))
                    });
                    match mapped {
                        Some((i, field_value)) => {
                            info.writes
                                .insert(MappedReg::Slot(i), field_value.smt.clone());
                        }
                        None => exclude(
                            &mut info,
                            format!("writes unmapped {name} field (trap/system path)"),
                        ),
                    }
                    continue;
                }
                match map_register(spec, name) {
                    Some(MappedReg::X(n)) if Some(n) == spec.zero_reg => {}
                    Some(reg) => {
                        info.writes
                            .insert(reg, unwrap_bits_struct(value).smt.clone());
                    }
                    None => exclude(
                        &mut info,
                        format!("writes unmapped register {} (trap/system path)", name),
                    ),
                }
            }
            tir_verify::TraceEvent::Declare { declaration } => {
                if declaration.contains("(_ BitVec ") || declaration.ends_with(" Bool)") {
                    info.declares.push(declaration.clone());
                } else {
                    exclude(
                        &mut info,
                        format!("symbolic non-bitvector state: {declaration}"),
                    );
                }
            }
            tir_verify::TraceEvent::Define {
                variable,
                expression,
            } => {
                defined_vars.insert(variable.clone());
                info.defines.push((variable.clone(), expression.clone()));
            }
            tir_verify::TraceEvent::Assume { expression } => {
                info.asserts.push(expression.clone());
            }
            // `(read-mem value kind address bytes [tag])`
            // `(write-mem success kind address data bytes [tag])`
            // Only plain accesses relate to the TMDL flat memory; reads are
            // constrained against the initial array, so a read after a write
            // (no such instruction yet) would be unsound and is excluded.
            tir_verify::TraceEvent::ReadMemory {
                kind,
                address,
                value,
                bytes,
            }
            | tir_verify::TraceEvent::WriteMemory {
                kind,
                address,
                value,
                bytes,
            } => {
                if !is_plain_access(kind) {
                    exclude(&mut info, format!("non-plain memory access {}", kind.smt));
                    continue;
                }
                if !matches!(bytes, 1 | 2 | 4 | 8) {
                    exclude(&mut info, format!("unsupported access width {}", bytes));
                    continue;
                }
                let access = MemAccess {
                    value: value.smt.clone(),
                    address: address.smt.clone(),
                    bytes: *bytes,
                };
                if matches!(event, tir_verify::TraceEvent::ReadMemory { .. }) {
                    if !info.mem_writes.is_empty() {
                        exclude(&mut info, "memory read after write".to_string());
                        continue;
                    }
                    info.mem_reads.push(access);
                } else {
                    info.mem_writes.push(access);
                }
            }
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

fn flat_read_register(model: &FlatModel, class: &str, state: &str, index: &str) -> String {
    let info = &model.classes[class];
    let selected = format!("(select {state}_{} {index})", info.storage);
    let value = if info.value_width < info.storage_width || info.bit_offset > 0 {
        format!(
            "((_ extract {} {}) {selected})",
            info.bit_offset + info.value_width - 1,
            info.bit_offset
        )
    } else {
        selected
    };
    match info.zero_index {
        Some(zero) => format!(
            "(ite (= {index} (_ bv{zero} {})) (_ bv0 {}) {value})",
            info.index_width, info.value_width
        ),
        None => value,
    }
}

fn flat_read_memory(xlen: u32, bytes: u32, state: &str, address: &str) -> String {
    (0..bytes)
        .rev()
        .map(|offset| {
            let slot = if offset == 0 {
                address.to_string()
            } else {
                format!("(bvadd {address} (_ bv{offset} {xlen}))")
            };
            format!("(select {state}_mem {slot})")
        })
        .reduce(|high, low| format!("(concat {high} {low})"))
        .expect("memory access has at least one byte")
}

fn build_query(
    spec: &IsaSpec,
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
    trace: &TraceInfo,
    modeled_cause: Option<(&str, &[u64])>,
) -> String {
    let xlen = spec.xlen;
    let mut q = String::from("(set-logic QF_AUFBV)\n(set-option :produce-models true)\n");
    for field in &model.fields {
        let _ = writeln!(q, "(declare-const st0_{} {})", field.name, field.sort);
    }
    q.push_str("(assert (not st0_resv))\n");
    let bindings = instr
        .operands
        .iter()
        .zip(case)
        .map(|((name, kind), value)| {
            format!("({name} {})", operand_smt_literal(spec, kind, *value))
        })
        .collect::<Vec<_>>();
    let execute = instr
        .flat_execute
        .as_ref()
        .expect("supported instruction has flat execute metadata");
    for field in &model.fields {
        let expression = &execute[&field.name];
        if bindings.is_empty() {
            let _ = writeln!(
                q,
                "(define-fun st1_{} () {} {expression})",
                field.name, field.sort
            );
        } else {
            let _ = writeln!(
                q,
                "(define-fun st1_{} () {} (let ({}) {expression}))",
                field.name,
                field.sort,
                bindings.join(" ")
            );
        }
    }

    // Fixed-width ISAs align the PC to the concrete instruction width. x86
    // pins the PC to a concrete aligned value in its config instead.
    if spec.align_pc {
        let alignment_bits = instr.width_bytes().trailing_zeros();
        if alignment_bits > 0 {
            let _ = writeln!(
                q,
                "(assert (= ((_ extract {} 0) st0_pc) (_ bv0 {})))",
                alignment_bits - 1,
                alignment_bits
            );
        }
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
            let _ = writeln!(q, "(assert {})", canonical("st1_pc"));
        }
        // Each memory-access effective address is assumed to sit below 2^46 (a
        // stricter canonical form): the model's 48-bit sign-masking then leaves
        // it unchanged, and a multi-byte access cannot straddle the 2^47
        // canonical boundary (where the model sign-extends but TMDL's flat
        // 64-bit memory does not).
        for addr in mem_addr_exprs(instr, case, spec) {
            let _ = writeln!(q, "(assert (= ((_ extract 63 46) {addr}) (_ bv0 18)))");
        }
    } else {
        // Registers feeding an indirect jump obey the target instruction
        // alignment, so misaligned-fetch trap paths are vacuous.
        let alignment_bits = instr.width_bytes().trailing_zeros();
        for &i in &instr.pc_source_operands {
            if let OperandKind::Reg { class, idx_width } = &instr.operands[i].1 {
                let reg = flat_read_register(
                    model,
                    class,
                    "st0",
                    &format!("(_ bv{} {})", case[i], idx_width),
                );
                if alignment_bits > 0 {
                    let _ = writeln!(
                        q,
                        "(assert (= ((_ extract {} 0) {reg}) (_ bv0 {})))",
                        alignment_bits - 1,
                        alignment_bits
                    );
                }
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
        flat_read_register(model, class, state, &format!("(_ bv{} {})", slot, w))
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
                "(assert (= ((_ extract {bit} {bit}) {var}) {}))",
                flat_read_register(model, class, "st0", &format!("(_ bv{slot} {idx_w})")),
            );
        }
    }
    for (reg, var) in &trace.reads {
        let init = match reg {
            MappedReg::X(n) => {
                flat_read_register(model, "gpr", "st0", &format!("(_ bv{} {})", n, gw))
            }
            MappedReg::Pc => "st0_pc".to_string(),
            MappedReg::NextPc => format!("(bvadd st0_pc (_ bv{width_bytes} {xlen}))"),
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
                .unwrap_or_else(|| {
                    flat_read_register(model, "gpr", "st0", &format!("(_ bv{} {})", n, gw))
                });
            format!(
                "(= {} {})",
                flat_read_register(model, "gpr", "st1", &format!("(_ bv{} {})", n, gw)),
                sail
            )
        })
        .collect();
    // Flag equivalence, only where the TMDL behavior models flags (its
    // execute writes the flag class). The ALU ops deliberately leave flags
    // unmodeled, so Sail's flag writes are ignored for them.
    if let Some((_, class, bit_map)) = spec.flag_reg {
        if instr.write_classes.iter().any(|written| written == class) {
            let idx_w = bit_map
                .iter()
                .map(|(s, _)| 64 - s.leading_zeros())
                .max()
                .unwrap_or(1);
            for (slot, _) in bit_map {
                let sail = trace.flag_writes.get(slot).cloned().unwrap_or_else(|| {
                    flat_read_register(model, class, "st0", &format!("(_ bv{slot} {idx_w})"))
                });
                final_eq.push(format!(
                    "(= {} {sail})",
                    flat_read_register(model, class, "st1", &format!("(_ bv{slot} {idx_w})"))
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
            "(= {} {})",
            read.value,
            flat_read_memory(xlen, read.bytes, "st0", &read.address)
        ));
    }
    if trace.mem_writes.is_empty() {
        // Both sides reduce to the untouched initial array; congruence
        // closes this cheaply.
        final_eq.push("(= st1_mem st0_mem)".to_string());
    } else {
        // Whole-array equality of two store chains makes z3 enumerate index
        // aliasing through the (long) address define-chains — minutes per
        // query. Equisatisfiable select formulation instead: equality at
        // every written slot, plus a frame condition at one fresh index
        // (the extensionality witness), each a directed bitvector goal.
        let mut sail_mem = "st0_mem".to_string();
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
                "(= (select st1_mem {}) (select {} {}))",
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
            "(or {} (= (select st1_mem mem_frame_idx) (select st0_mem mem_frame_idx)))",
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
            asserts.push(format!("(distinct {} st0_pc)", target));
            final_eq.push(format!(
                "(= (ite (= st1_pc st0_pc) (bvadd st0_pc (_ bv{width_bytes} {xlen})) st1_pc) {})",
                target
            ));
        }
        None => final_eq.push("(= st1_pc st0_pc)".to_string()),
    }

    let path = if asserts.is_empty() {
        "true".to_string()
    } else {
        asserts.join(" ")
    };
    let with_defines = |mut body: String| {
        for (var, expr) in trace.defines.iter().rev() {
            body = format!("(let (({} {}))\n{})", var, expr, body);
        }
        body
    };
    let modeled = modeled_cause.map(|(cause, causes)| {
        format!(
            "(or {})",
            causes
                .iter()
                .map(|c| format!("(= {} (_ bv{} {}))", cause, c, xlen))
                .collect::<Vec<_>>()
                .join(" ")
        )
    });
    if let Some(modeled) = &modeled {
        q.push_str("(push)\n");
        let probe = with_defines(format!("(and {path} (not {modeled}))"));
        let _ = writeln!(q, "(assert {probe})");
        q.push_str("(check-sat)\n(pop)\n");
    }
    let cause_constraint = modeled.as_deref().unwrap_or("true");
    let body = with_defines(format!(
        "(and {path} {cause_constraint} (not (and {})))",
        final_eq.join("\n  ")
    ));
    let _ = writeln!(q, "(assert {})", body);
    q.push_str("(check-sat)\n");

    // Counterexample probes, only evaluated on `sat`.
    let mut probes: Vec<String> = vec!["st0_pc".into(), "st1_pc".into()];
    for n in (0..spec.reg_count).filter(|n| Some(*n) != spec.zero_reg) {
        probes.push(flat_read_register(
            model,
            "gpr",
            "st0",
            &format!("(_ bv{} {})", n, gw),
        ));
    }
    let _ = writeln!(q, "(get-value ({}))", probes.join(" "));
    q
}

// ---------------------------------------------------------------------------
// Per-instruction driver and reporting
// ---------------------------------------------------------------------------

#[derive(Default, Serialize)]
struct Report {
    isa: String,
    shard: Option<Shard>,
    wall_ms: u128,
    verified: usize,
    failed: usize,
    unknown: usize,
    excluded_paths: usize,
    excluded_reasons: HashMap<String, usize>,
    unsupported: Vec<String>,
    failures: Vec<String>,
    instructions: Vec<InstructionTiming>,
}

impl Report {
    fn new(isa: &str, shard: Option<Shard>) -> Self {
        Self {
            isa: isa.to_string(),
            shard,
            ..Self::default()
        }
    }

    fn merge(&mut self, mut other: Self) {
        self.verified += other.verified;
        self.failed += other.failed;
        self.unknown += other.unknown;
        self.excluded_paths += other.excluded_paths;
        for (reason, count) in other.excluded_reasons {
            *self.excluded_reasons.entry(reason).or_default() += count;
        }
        self.unsupported.append(&mut other.unsupported);
        self.failures.append(&mut other.failures);
    }

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

#[derive(Default, Serialize)]
struct InstructionTiming {
    instruction: String,
    cases: usize,
    paths: usize,
    encode_ms: u128,
    decode_ms: u128,
    isla_ms: u128,
    solver_ms: u128,
    total_ms: u128,
}

fn verify_instruction(
    tools: &Tools,
    spec: &IsaSpec,
    out_dir: &Path,
    model: &FlatModel,
    instr: &Instruction,
) -> anyhow::Result<(Report, InstructionTiming, String)> {
    let total_started = Instant::now();
    let mut report = Report::default();
    let mut timing = InstructionTiming {
        instruction: instr.name.clone(),
        ..InstructionTiming::default()
    };
    let cases = operand_cases(spec, instr)
        .into_iter()
        .filter(|case| operand_case_is_valid(spec, instr, case))
        .collect::<Vec<_>>();
    timing.cases = cases.len();
    let started = Instant::now();
    let words = encode_words(instr, &cases);
    timing.encode_ms = started.elapsed().as_millis();
    let decode_started = Instant::now();
    let cases = decode_operands(instr, &words);
    timing.decode_ms = decode_started.elapsed().as_millis();
    let mut line = String::new();
    let isla_started = Instant::now();
    let traces_by_word = sail_traces(tools, out_dir, instr, &words)?;
    timing.isla_ms = isla_started.elapsed().as_millis();

    for (case, word) in cases.iter().zip(&words) {
        let Some(traces) = traces_by_word.get(word).and_then(Option::as_ref) else {
            report.excluded_paths += 1;
            *report
                .excluded_reasons
                .entry(format!(
                    "{}: Isla failed or timed out ({:#014x})",
                    instr.name, word
                ))
                .or_default() += 1;
            line.push('I');
            continue;
        };
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
            timing.paths += 1;
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
            let query = build_query(
                spec,
                model,
                instr,
                case,
                &info,
                written_cause.map(|(cause, causes)| (cause.as_str(), causes)),
            );
            let query_path = out_dir
                .join("queries")
                .join(format!("{}_{:08x}_p{}.smt2", instr.name, word, path_idx));
            std::fs::write(&query_path, &query)?;
            let solver_started = Instant::now();
            let output = run_solver(tools, &query_path)?;
            timing.solver_ms += solver_started.elapsed().as_millis();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let statuses: Vec<&str> = stdout
                .lines()
                .filter(|line| matches!(*line, "sat" | "unsat" | "unknown"))
                .collect();
            let (unmodeled_status, equivalence_status) = if written_cause.is_some() {
                (statuses.first().copied(), statuses.get(1).copied())
            } else {
                (None, statuses.first().copied())
            };
            if unmodeled_status == Some("sat") {
                report.excluded_paths += 1;
                *report
                    .excluded_reasons
                    .entry(format!(
                        "{}: trap cause outside the modeled set (access fault path)",
                        instr.name
                    ))
                    .or_default() += 1;
                line.push('-');
            } else if equivalence_status == Some("unsat") {
                report.verified += 1;
                line.push('.');
            } else if equivalence_status == Some("sat") {
                report.failed += 1;
                line.push('X');
                let model = stdout
                    .lines()
                    .skip(if written_cause.is_some() { 2 } else { 1 })
                    .collect::<Vec<_>>()
                    .join("\n");
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
    timing.total_ms = total_started.elapsed().as_millis();
    Ok((report, timing, format!("{:24}{}", instr.name, line)))
}

fn run_z3(tools: &Tools, path: &Path) -> anyhow::Result<std::process::Output> {
    let first = Command::new(&tools.z3)
        .args(["-smt2", "-T:30", "smt.random_seed=0"])
        .arg(path)
        .output()?;
    let stdout = String::from_utf8_lossy(&first.stdout);
    let final_status = stdout
        .lines()
        .rfind(|line| matches!(*line, "sat" | "unsat" | "unknown"));
    if matches!(final_status, Some("sat" | "unsat")) {
        return Ok(first);
    }
    Ok(Command::new(&tools.z3)
        .args(["-smt2", "-T:30", "smt.random_seed=1"])
        .arg(path)
        .output()?)
}

fn solver_statuses(output: &std::process::Output) -> Vec<String> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| matches!(*line, "sat" | "unsat" | "unknown"))
        .map(str::to_string)
        .collect()
}

fn run_solver(tools: &Tools, path: &Path) -> anyhow::Result<std::process::Output> {
    let Some(bitwuzla) = &tools.bitwuzla else {
        return run_z3(tools, path);
    };
    let output = Command::new(bitwuzla)
        .args(["--time-limit", "5000"])
        .arg(path)
        .output();
    let Ok(output) = output else {
        return run_z3(tools, path);
    };
    let statuses = solver_statuses(&output);
    if !output.status.success() || statuses.is_empty() || statuses.iter().any(|s| s == "unknown") {
        return run_z3(tools, path);
    }
    if statuses.iter().any(|s| s == "sat") {
        let z3 = run_z3(tools, path)?;
        anyhow::ensure!(
            statuses == solver_statuses(&z3),
            "bitwuzla/z3 disagreement for {}: {:?} vs {:?}",
            path.display(),
            statuses,
            solver_statuses(&z3)
        );
        return Ok(z3);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_partition_is_complete_and_disjoint() {
        for name in ["add", "sub", "branch_eq", "load64", "store32"] {
            let memberships = (0..4)
                .filter(|index| {
                    Shard {
                        index: *index,
                        count: 4,
                    }
                    .contains(name)
                })
                .count();
            assert_eq!(memberships, 1, "{name}");
        }
    }

    #[test]
    fn parses_shard_option() {
        let shard = parse_shard(["--shard".into(), "2/4".into()].into_iter())
            .unwrap()
            .unwrap();
        assert_eq!((shard.index, shard.count), (2, 4));
        assert!(parse_shard(["--shard".into(), "4/4".into()].into_iter()).is_err());
    }

    #[test]
    fn parses_structured_instruction_metadata() {
        let json = r#"{
          "version": 1,
          "isa": "TestIsa",
          "dialect": "test",
          "smt_prelude": "(set-logic ALL)",
          "flat_state": [
            {"name": "gpr", "sort": "(Array (_ BitVec 5) (_ BitVec 64))"},
            {"name": "mem", "sort": "(Array (_ BitVec 64) (_ BitVec 8))"},
            {"name": "resv", "sort": "Bool"},
            {"name": "resa", "sort": "(_ BitVec 64)"},
            {"name": "pc", "sort": "(_ BitVec 64)"}
          ],
          "register_classes": [{
            "name": "gpr", "storage": "gpr", "index_width": 5,
            "value_width": 64, "storage_width": 64, "zero_index": 0,
            "bit_offset": 0
          }],
          "instructions": [{
            "name": "load", "writes_pc": false, "width_bits": 32,
            "operands": [
              {"name": "rd", "kind": "register", "class": "gpr", "width": 5},
              {"name": "imm", "kind": "bits", "class": null, "width": 12}
            ],
            "supported": true, "write_classes": ["gpr"],
            "uses_reservation": false, "pc_source_operands": [],
            "memory_accesses": [{"kind": "load", "bytes": 4, "address": "(read_gpr st rd)", "flat_address": "(select st0_gpr rd)"}],
            "trap_kinds": ["misaligned_load"],
            "encoding": [
              {"word_low": 7, "word_high": 11, "operand": "rd", "operand_low": 0, "value": "0"},
              {"word_low": 0, "word_high": 6, "operand": null, "operand_low": 0, "value": "3"}
            ],
            "execute": "(write_gpr st rd (_ bv0 64))",
            "flat_execute": {"gpr": "st0_gpr", "mem": "st0_mem", "resv": "st0_resv", "resa": "st0_resa", "pc": "st0_pc"}
          }]
        }"#;
        let inventory = parse_inventory(json).unwrap();
        let instruction = &inventory.instructions[0];
        assert_eq!(instruction.name, "load");
        assert_eq!(inventory.isa, "TestIsa");
        assert_eq!(inventory.dialect, "test");
        assert_eq!(instruction.write_classes, ["gpr"]);
        assert_eq!(
            instruction.memory_accesses[0].flat_address,
            "(select st0_gpr rd)"
        );
        let words = encode_words(instruction, &[vec![5, 0]]);
        assert_eq!(words, [5 << 7 | 3]);
        assert_eq!(decode_operands(instruction, &words)[0][0], 5);
    }
}
