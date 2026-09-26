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
//! Undefined Sail values are internal choices. The query searches for an
//! architectural input whose TMDL result no choice can reproduce. Register
//! and memory inputs remain outside that quantifier.
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
//! `xtask/verify/<isa>.toml` names them and maps Sail state to TMDL slots.
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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::utils::{download_file, project_root};
use anyhow::{anyhow, Context};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use tmdl::{FlatStateFieldMetadata, MemoryAccessMetadata, RegisterClassMetadata, SmtMetadata};
use xshell::{cmd, Shell};

/// One ISA's verification setup, read from `xtask/verify/<isa>.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IsaSpec {
    #[serde(skip)]
    name: String,
    tmdl_isa: String,
    dialect: String,
    defs_dir: String,
    /// Snapshot file, its GitHub repository and the pinned commit the configs
    /// were validated against (`TIR_ISLA_SNAPSHOTS_REF` overrides it).
    snapshot: String,
    snapshot_repo: String,
    snapshot_ref: String,
    /// Isla config file name under `xtask/`.
    isla_config: String,
    xlen: u32,
    /// Simplify Isla traces.
    simplify: bool,
    #[serde(default)]
    initial_registers: Vec<String>,
    /// The Sail PC, and the delayed PC branches write when the model has one
    /// (RISC-V `nextPC`).
    pc: String,
    next_pc: Option<String>,
    /// Model bookkeeping registers with no architectural meaning; reads and
    /// writes of them never exclude a path.
    #[serde(default)]
    ignore: Vec<String>,
    /// Registers backing memory-mapped devices: a read means the access
    /// resolved into the platform memory map, which TMDL's flat memory does
    /// not model, so the path is excluded even when the value is concrete.
    #[serde(default)]
    mmio: Vec<String>,
    /// A path writing a trap cause outside `causes` (access faults: the TMDL
    /// model treats all of memory as RAM) is excluded, established by a solver
    /// probe since the written value is a path expression.
    trap_cause: Option<TrapCause>,
    /// Concrete operand values for register classes whose encoding space is
    /// mostly unimplemented (CSR addresses), instead of the GPR patterns.
    #[serde(default)]
    operand_values: HashMap<String, Vec<u64>>,
    /// Assume the initial PC is aligned to the instruction width.
    #[serde(default)]
    align_pc: bool,
    /// Assume data-access and branch-target addresses are canonical, so the
    /// model's non-canonical `#GP` paths are vacuous.
    #[serde(default)]
    canonical_addrs: bool,
    /// Every completing instruction writes the PC, so a path that does not
    /// faulted or decoded to something else and is excluded.
    #[serde(default)]
    requires_pc_write: bool,
    #[serde(deserialize_with = "expand_map")]
    map: Vec<MapRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TrapCause {
    register: String,
    causes: Vec<u64>,
}

/// A `map` entry as written. `sail` is `reg`, `reg.field` (struct field
/// accessor) or `reg[element]` (vector element); `{n}` in it expands over the
/// inclusive range `n`, which is then the slot index. A row without `class`
/// holds bits the architecture fixes at zero.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MapEntry {
    sail: String,
    bits: Option<(u32, u32)>,
    class: Option<String>,
    index: Option<u64>,
    n: Option<(u64, u64)>,
    #[serde(default)]
    if_written: bool,
}

/// One Sail location related to one piece of TMDL state.
struct MapRow {
    register: String,
    field: Option<String>,
    element: Option<usize>,
    /// `(high, low)` bits of the Sail value that hold the state.
    bits: Option<(u32, u32)>,
    state: State,
    /// Compared only when the TMDL behavior writes the slot.
    if_written: bool,
}

/// TMDL state a Sail location relates to.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum State {
    Pc,
    /// The fall-through PC, which Sail's delayed PC starts at.
    NextPc,
    Slot {
        class: String,
        index: u64,
    },
    /// Architecturally zero bits of this width.
    Zero(u32),
}

fn expand_map<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Vec<MapRow>, D::Error> {
    let mut rows = Vec::new();
    for entry in Vec::<MapEntry>::deserialize(de)? {
        let slots = match entry.n {
            Some((low, high)) => (low..=high).map(Some).collect(),
            None => vec![entry.index],
        };
        for index in slots {
            let sail = match index {
                Some(n) => entry.sail.replace("{n}", &n.to_string()),
                None => entry.sail.clone(),
            };
            let (register, element) = match sail.strip_suffix(']').and_then(|s| s.split_once('[')) {
                Some((register, element)) => {
                    (register, Some(element.parse().map_err(D::Error::custom)?))
                }
                None => (sail.as_str(), None),
            };
            let (register, field) = match register.split_once('.') {
                Some((register, field)) => (register, Some(field.to_string())),
                None => (register, None),
            };
            let state = match (&entry.class, index, entry.bits) {
                (Some(class), Some(index), _) => State::Slot {
                    class: class.clone(),
                    index,
                },
                (None, None, Some((high, low))) => State::Zero(high - low + 1),
                _ => {
                    return Err(D::Error::custom(format!(
                        "{sail}: need class and index, or bits alone"
                    )))
                }
            };
            rows.push(MapRow {
                register: register.to_string(),
                field,
                element,
                bits: entry.bits,
                state,
                if_written: entry.if_written,
            });
        }
    }
    Ok(rows)
}

impl IsaSpec {
    fn load(isa: &str) -> anyhow::Result<Self> {
        let path = project_root()
            .join("xtask/verify")
            .join(format!("{isa}.toml"));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("unsupported ISA {isa}: cannot read {}", path.display()))?;
        let mut spec: Self =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        spec.name = isa.to_string();
        let pcs = [
            (Some(spec.pc.clone()), State::Pc),
            (spec.next_pc.clone(), State::NextPc),
        ];
        for (register, state) in pcs {
            if let Some(register) = register {
                spec.map.push(MapRow {
                    register,
                    field: None,
                    element: None,
                    bits: None,
                    state,
                    if_written: false,
                });
            }
        }
        Ok(spec)
    }

    /// TMDL register classes the driver can relate to Sail state: those
    /// sharing storage with a mapped class (the x86 `gpr8`/`gpr16`/`gpr32`
    /// views alias the GPR file).
    fn class_is_mapped(&self, model: &FlatModel, class: &str) -> bool {
        let storage = |class: &str| model.classes.get(class).map(|info| &info.storage);
        storage(class).is_some_and(|mapped| {
            self.map.iter().any(|row| {
                matches!(&row.state, State::Slot { class, .. } if storage(class) == Some(mapped))
            })
        })
    }
}

impl MapRow {
    /// This row's part of a register access, before `bits`: `Ok(None)` when
    /// the access names another field.
    fn select(
        &self,
        fields: &[String],
        value: &tir_verify::TraceValue,
    ) -> Result<Option<String>, String> {
        if let Some(field) = &self.field {
            if fields.first() != Some(field) {
                return Ok(None);
            }
            return Ok(value.fields.get(field).map(|value| value.smt.clone()));
        }
        // Bitfield registers carry their value in a single-field struct.
        let value = &unwrap_bits_struct(value).smt;
        let Some(element) = self.element else {
            return Ok(Some(value.clone()));
        };
        value
            .strip_prefix("(_ vec ")
            .and_then(|body| body.strip_suffix(')'))
            .filter(|body| !body.contains('('))
            .and_then(|body| body.split_whitespace().nth(element))
            .map(|element| Some(element.to_string()))
            .ok_or_else(|| format!("unrecognized {} vector value", self.register))
    }

    fn slice(&self, value: String) -> String {
        match self.bits {
            Some((high, low)) => format!("((_ extract {high} {low}) {value})"),
            None => value,
        }
    }
}

// These AdvSIMD operations have been checked across every lane arrangement.
// Keep other vector operations out of the nightly proof set until their Sail
// paths have the same coverage, since many fork once per lane.
const ARM_VECTOR_PROOFS: &[&str] = &[
    "addvector8b",
    "addvector16b",
    "addvector4h",
    "addvector8h",
    "addvector2s",
    "addvector4s",
    "addvector2d",
    "subvector8b",
    "subvector16b",
    "subvector4h",
    "subvector8h",
    "subvector2s",
    "subvector4s",
    "subvector2d",
    "andvector8b",
    "andvector16b",
    "orrvector8b",
    "orrvector16b",
    "eorvector8b",
    "eorvector16b",
];

// The pinned ACL2-derived snapshot has no execution semantics for these
// instructions: every Sail path stops before writing RIP.
const X86_SAIL_UNIMPLEMENTED: &[&str] = &[
    "andn", "andn32", "bextr", "bextr32", "blsi", "blsi32", "blsmsk", "blsmsk32", "blsr", "blsr32",
    "btc", "btr", "bts", "bzhi", "bzhi32", "mulx", "mulx32", "rorx", "rorx32", "sarx", "sarx32",
    "shlx", "shlx32", "shrx", "shrx32",
];

// The pinned Isla evaluator panics when Sail's 64-bit SHLD/SHRD forms convert
// their symbolic 128-bit intermediate to an integer.
const X86_ISLA_128BIT_SHIFTS: &[&str] = &["shldimm", "shrdimm", "shldcl", "shrdcl"];

// These pinned Sail forms do not complete a trace within Isla's execution
// limit, so there is no path on which to compare architectural state.
const X86_ISLA_UNEXECUTABLE: &[&str] = &["pushf", "signeddivide32"];

fn x86_unsupported_reason(name: &str) -> Option<&'static str> {
    if X86_SAIL_UNIMPLEMENTED.contains(&name) {
        Some("not implemented by pinned Sail snapshot")
    } else if X86_ISLA_128BIT_SHIFTS.contains(&name) {
        Some("pinned Isla evaluator cannot execute symbolic 128-bit shift")
    } else if X86_ISLA_UNEXECUTABLE.contains(&name) {
        Some("pinned Isla evaluator cannot complete Sail execution")
    } else if name == "unsigneddivide32" {
        Some("guarded narrow division proof incomplete")
    } else {
        None
    }
}

fn uses_arm_vector(model: &FlatModel, instr: &Instruction) -> bool {
    let is_vector = |class: &str| {
        model
            .classes
            .get(class)
            .is_some_and(|info| info.storage == "vpr")
    };
    instr
        .operands
        .iter()
        .any(|(_, kind)| matches!(kind, OperandKind::Reg { class, .. } if is_vector(class)))
        || instr.write_classes.iter().any(|class| is_vector(class))
}

/// Why `instr` cannot be verified against the model, as the report names it,
/// or `None` when it can.
fn unsupported_reason(spec: &IsaSpec, model: &FlatModel, instr: &Instruction) -> Option<String> {
    let riscv = spec.name.starts_with("riscv");
    if riscv && instr.name == "vsetvli" {
        return Some("vsetvli (RVV disabled in Sail configuration)".to_string());
    }
    if riscv && instr.width_bits == 16 {
        return Some(format!(
            "{} (compressed extension disabled in Sail configuration)",
            instr.name
        ));
    }
    if riscv && matches!(instr.name.as_str(), "envcall" | "envbreak" | "cenvbreak") {
        return Some(format!(
            "{} (terminating Sail trace omits architectural trap state)",
            instr.name
        ));
    }
    if !instr.supported {
        return Some(instr.name.clone());
    }
    if spec.name == "x86_64" {
        if let Some(reason) = x86_unsupported_reason(&instr.name) {
            return Some(format!("{} ({reason})", instr.name));
        }
    }
    // Atomics (A extension) reference the reservation state, whose mapping onto
    // Sail's reservation register is follow-up work (see module docs).
    if instr.uses_reservation {
        return Some(format!(
            "{} (atomic; Sail reservation mapping is follow-up)",
            instr.name
        ));
    }
    if instr.flat_execute.is_none() {
        return Some(format!("{} (no flat SMT behavior)", instr.name));
    }
    if spec.name == "armv8"
        && uses_arm_vector(model, instr)
        && !ARM_VECTOR_PROOFS.contains(&instr.name.as_str())
    {
        return Some(format!(
            "{} (ARM vector equivalence not yet checked)",
            instr.name
        ));
    }
    // Operands in register classes that have no correspondence to Sail state
    // (e.g. the TMDL `pc` operand class).
    instr
        .operands
        .iter()
        .find_map(|(_, kind)| match kind {
            OperandKind::Reg { class, .. } if !spec.class_is_mapped(model, class) => Some(class),
            _ => None,
        })
        .map(|class| format!("{} (unmapped register class {})", instr.name, class))
}

pub fn verify_smt(sh: &Shell, isa: &str, args: impl Iterator<Item = String>) -> anyhow::Result<()> {
    let spec = &IsaSpec::load(isa)?;
    let shard = parse_shard(args)?;
    let tools = Tools::ensure(sh, spec)?;
    let root = project_root();
    let out_dir = root.join("target/verify/smt").join(&spec.name);
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
    for row in &spec.map {
        if let State::Slot { class, .. } = &row.state {
            anyhow::ensure!(
                inventory.flat.classes.contains_key(class),
                "{} maps {} to unknown TMDL class {class}",
                spec.name,
                row.register
            );
        }
    }
    let instructions = inventory.instructions;
    let filter: Option<Vec<String>> = std::env::var("TIR_VERIFY_SMT_FILTER")
        .ok()
        .map(|f| f.split(',').map(|s| s.trim().to_string()).collect());

    let mut report = Report::new(&spec.name, shard);
    let started = Instant::now();
    let mut selected = Vec::new();

    for instr in &instructions {
        if filter.as_ref().is_some_and(|f| !f.contains(&instr.name)) {
            continue;
        }
        if shard.is_some_and(|shard| !shard.contains(&instr.name)) {
            continue;
        }
        if let Some(reason) = unsupported_reason(spec, &inventory.flat, instr) {
            report.unsupported.push(reason);
            continue;
        }
        selected.push(instr);
    }

    for instr in &selected {
        let (instruction_report, timing, line) =
            verify_instruction(&tools, spec, &out_dir, &inventory.flat, instr)?;
        println!("{line}");
        report.merge(instruction_report);
        report.instructions.push(timing);
    }

    // What an encoding owes its decoder, whichever shape it took: the word the
    // instruction encodes to reads back as that instruction.
    for instr in &selected {
        match prove_roundtrip(&tools, spec, &out_dir, &smt_path, instr)? {
            true => report.roundtrip_proved.push(instr.name.clone()),
            false => report.roundtrip_open.push(instr.name.clone()),
        }
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
    if !report.behavior_independent.is_empty() {
        anyhow::bail!(
            "{} instruction(s) verify with their TMDL behavior replaced by a no-op: {}",
            report.behavior_independent.len(),
            report.behavior_independent.join(", ")
        );
    }
    let uncovered = report.uncovered_shapes();
    if !uncovered.is_empty() {
        anyhow::bail!(
            "{} encoding shape(s) came out of no verified case: {}",
            uncovered.len(),
            uncovered.join(", ")
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
    bitwuzla: Option<PathBuf>,
    z3: PathBuf,
    verifier: tir_verify::Verifier,
}

impl Tools {
    /// Resolve the external tools, fetching anything that is not overridden
    /// by an environment variable.
    fn ensure(sh: &Shell, spec: &IsaSpec) -> anyhow::Result<Self> {
        let snapshot = match std::env::var("TIR_ISLA_SNAPSHOT") {
            Ok(path) => path.into(),
            Err(_) => ensure_snapshot(sh, spec)?,
        };
        let isla_config = std::env::var("TIR_ISLA_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| project_root().join("xtask").join(&spec.isla_config));
        let threads = std::env::var("TIR_VERIFY_SMT_ISLA_JOBS")
            .ok()
            .and_then(|jobs| jobs.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(usize::from)
                    .unwrap_or(1)
            });
        let verifier = tir_verify::Verifier::load(
            &snapshot,
            &isla_config,
            &spec.initial_registers,
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
            bitwuzla,
            z3: std::env::var("TIR_Z3")
                .unwrap_or_else(|_| "z3".to_string())
                .into(),
            verifier,
        })
    }
}

fn ensure_snapshot(sh: &Shell, spec: &IsaSpec) -> anyhow::Result<PathBuf> {
    let file = &spec.snapshot;
    let snap_ref =
        std::env::var("TIR_ISLA_SNAPSHOTS_REF").unwrap_or_else(|_| spec.snapshot_ref.clone());
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
    let defs: Vec<PathBuf> = std::fs::read_dir(root.join(&spec.defs_dir))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "tmdl"))
        .collect();
    let out_str = out.to_string_lossy().to_string();
    let dialect = &spec.dialect;
    let tmdl_isa = &spec.tmdl_isa;
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
    Bits(u32, ImmConstraint),
    Int(ImmConstraint),
}

/// The values an immediate operand admits beyond its width: `#[align(N)]` makes
/// it a multiple of `N`, `#[nonzero]` excludes zero.
#[derive(Clone, Copy, Debug)]
struct ImmConstraint {
    align: u64,
    nonzero: bool,
}

impl ImmConstraint {
    /// `value` moved into the admitted set: rounded down to the alignment, and
    /// `None` when nothing is left (zero under `#[nonzero]`).
    fn admitted(&self, value: u64) -> Option<u64> {
        let value = value & !(self.align - 1);
        (!(self.nonzero && value == 0)).then_some(value)
    }
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
    fixed_register_writes: Vec<(String, u32)>,
    uses_reservation: bool,
    pc_source_operands: Vec<usize>,
    memory_accesses: Vec<MemoryAccessMetadata>,
    /// The fixed bit maps this instruction encodes to, each with the guard over
    /// the operands that selects it. Every ISA but x86 has exactly one.
    shapes: Vec<Shape>,
    flat_execute: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Debug)]
struct Shape {
    name: String,
    width_bits: u32,
    guard: tmdl::shapes::Predicate,
    encoding: Vec<EncodingField>,
}

#[derive(Clone, Debug)]
struct EncodingField {
    word_low: u32,
    word_high: u32,
    operand_index: Option<usize>,
    operand_low: u32,
    value: u128,
}

impl Instruction {
    /// The shape an operand tuple encodes to: the first whose guard holds, as
    /// the encoder picks it. The guards partition the operand domain, so
    /// "first" only decides which of two equal answers is taken.
    fn shape_for(&self, case: &[u64]) -> Option<&Shape> {
        let value = |name: &str| {
            self.operands
                .iter()
                .position(|(operand, _)| operand == name)
                .and_then(|index| case.get(index).copied())
                .unwrap_or(0)
        };
        self.shapes
            .iter()
            .find(|shape| shape.guard.holds(&value))
            .or(self.shapes.first())
    }

    /// The bytes one operand tuple encodes to. Shapes differ in width, and the
    /// program counter moves by the one the encoder picked, not by the widest.
    fn width_bytes(&self, case: &[u64]) -> u32 {
        self.shape_for(case)
            .map_or(self.width_bits, |shape| shape.width_bits)
            / 8
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
    fields: Vec<FlatStateFieldMetadata>,
    classes: HashMap<String, RegisterClassMetadata>,
}

fn parse_inventory(json: &str) -> anyhow::Result<Inventory> {
    let metadata: SmtMetadata = serde_json::from_str(json)?;
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
                    let constraint = ImmConstraint {
                        align: u64::from(operand.align),
                        nonzero: operand.nonzero,
                    };
                    let width = u32::from(operand.width);
                    let kind = match operand.kind.as_str() {
                        "register" => OperandKind::Reg {
                            class: operand
                                .class
                                .ok_or_else(|| anyhow!("register operand without class"))?,
                            idx_width: width,
                        },
                        "bits" => OperandKind::Bits(width, constraint),
                        "int" => OperandKind::Int(constraint),
                        kind => anyhow::bail!("unknown operand kind {kind}"),
                    };
                    Ok((operand.name, kind))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let shapes = raw
                .shapes
                .into_iter()
                .map(|shape| {
                    let encoding = shape
                        .fields
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
                                word_low: u32::from(field.word_low),
                                word_high: u32::from(field.word_high),
                                operand_index,
                                operand_low: u32::from(field.operand_low),
                                value: field.value.parse()?,
                            })
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    Ok(Shape {
                        name: shape.name,
                        width_bits: u32::from(shape.width_bits),
                        guard: shape.guard,
                        encoding,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Instruction {
                name: raw.name,
                writes_pc: raw.writes_pc,
                width_bits: u32::from(raw.width_bits),
                operands,
                supported: raw.supported,
                write_classes: raw.write_classes,
                fixed_register_writes: raw.fixed_register_writes,
                uses_reservation: raw.uses_reservation,
                pc_source_operands: raw.pc_source_operands,
                memory_accesses: raw.memory_accesses,
                shapes,
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
    let fixed_values = |class: &str| spec.operand_values.get(class).map(Vec::as_slice);
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

    // Every immediate walks its own boundary list, in step with the others: a
    // second immediate left at zero hides behavior (a zero branch offset turns
    // a taken branch into a self-jump).
    let imm_values: Vec<(usize, Vec<u64>)> = instr
        .operands
        .iter()
        .enumerate()
        .filter_map(|(i, (_, k))| match k {
            OperandKind::Bits(w, c) => Some((i, *w, *c)),
            OperandKind::Int(c) => Some((i, 64, *c)),
            OperandKind::Reg { .. } => None,
        })
        .map(|(i, w, constraint)| {
            let mask = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let values = if instr.writes_pc {
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
            };
            // A case the operand's declared constraints exclude is not a case
            // the instruction has: it encodes nothing.
            let mut admitted: Vec<u64> = vec![];
            for value in values.into_iter().filter_map(|v| constraint.admitted(v)) {
                if !admitted.contains(&value) {
                    admitted.push(value);
                }
            }
            (i, admitted)
        })
        .collect();
    let imm_cases = if imm_values.iter().any(|(_, values)| values.is_empty()) {
        0
    } else {
        imm_values
            .iter()
            .map(|(_, values)| values.len())
            .max()
            .unwrap_or(1)
    };

    let mut cases = vec![];
    for regs in &reg_patterns {
        for k in 0..imm_cases {
            let mut case = vec![0u64; instr.operands.len()];
            for (slot, value) in reg_positions.iter().zip(regs) {
                case[*slot] = *value;
            }
            for (slot, values) in &imm_values {
                case[*slot] = values[k % values.len()];
            }
            cases.push(case);
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

fn operand_case_is_valid(
    spec: &IsaSpec,
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
) -> bool {
    if !instr
        .operands
        .iter()
        .zip(case)
        .all(|((_, kind), value)| match kind {
            OperandKind::Reg { class, .. } => model.classes[class]
                .indices
                .iter()
                .any(|index| u64::from(*index) == *value),
            _ => true,
        })
    {
        return false;
    }
    let value = |index: usize| case[index];
    match (spec.name.as_str(), instr.name.as_str()) {
        ("armv8", "loaddoublewordpreindex" | "loaddoublewordpostindex") => value(0) != value(1),
        ("armv8", "storedoublewordpreindex") => value(0) != value(1),
        ("armv8", "loadpair") => value(0) != value(1),
        ("armv8", "loadpairpreindex" | "loadpairpostindex") => {
            value(0) != value(1) && value(0) != value(2) && value(1) != value(2)
        }
        ("armv8", "storepairpreindex") => value(0) != value(2) && value(1) != value(2),
        ("armv8", "andimmediate") => !reserved_bitmask(true, value(3)),
        ("armv8", "andimmediate32") => !reserved_bitmask(false, value(3)),
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
        _ => true,
    }
}

/// Whether a logical-immediate `N:imms` is the reserved all-ones element
/// (`DecodeBitMasks` is UNDEFINED there).
fn reserved_bitmask(n: bool, imms: u64) -> bool {
    let len = if n {
        6
    } else {
        match (!imms & 0x3f).checked_ilog2() {
            Some(len) => len,
            None => return true,
        }
    };
    let levels = (1 << len) - 1;
    imms & levels == levels
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
            instr
                .shape_for(case)
                .map(|shape| shape.encoding.as_slice())
                .unwrap_or_default()
                .iter()
                .fold(0u128, |word, field| {
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
fn decode_operands(instr: &Instruction, cases: &[Vec<u64>], words: &[u128]) -> Vec<Vec<u64>> {
    cases
        .iter()
        .zip(words)
        .map(|(case, word)| {
            let mut operands = vec![0u64; instr.operands.len()];
            let shape = instr.shape_for(case);
            let fields = shape
                .map(|shape| shape.encoding.as_slice())
                .unwrap_or_default();
            for field in fields {
                let Some(index) = field.operand_index else {
                    continue;
                };
                let width = field.word_high - field.word_low + 1;
                let piece = (word >> field.word_low) & bit_mask(width);
                operands[index] |= (piece << field.operand_low) as u64;
            }
            // Bits the shape leaves out of the word are the ones its guard
            // pins: a base register the SIB byte names by a fixed pattern is
            // one, and the guard says which.
            for (index, (name, _)) in instr.operands.iter().enumerate() {
                let covered: u64 = fields
                    .iter()
                    .filter(|field| field.operand_index == Some(index))
                    .map(|field| {
                        let width = field.word_high - field.word_low + 1;
                        (bit_mask(width) << field.operand_low) as u64
                    })
                    .fold(0, |acc, mask| acc | mask);
                if let Some(shape) = shape {
                    for (lo, hi, value) in slice_constraints(&shape.guard, name) {
                        let mask = (bit_mask(hi - lo + 1) << lo) as u64;
                        operands[index] |= (value as u64) << lo & mask & !covered;
                    }
                }
            }
            // A shape the guard picked because the value fits a narrow field
            // spells only that field. The bits it left out are the extension of
            // the ones it spelled, which is how the hardware reads the word
            // back, so the case the model is checked against carries them.
            for (index, (name, kind)) in instr.operands.iter().enumerate() {
                let OperandKind::Bits(declared, _) = kind else {
                    continue;
                };
                let carried = fields
                    .iter()
                    .filter(|field| field.operand_index == Some(index))
                    .map(|field| field.operand_low + field.word_high - field.word_low + 1)
                    .max()
                    .unwrap_or(0);
                if carried == 0 || carried >= *declared {
                    continue;
                }
                let signed = shape.is_some_and(|shape| signed_fit(&shape.guard, name, carried));
                if signed && operands[index] & (1 << (carried - 1)) != 0 {
                    operands[index] |= (!bit_mask(carried) & bit_mask(*declared)) as u64;
                }
            }
            operands
        })
        .collect()
}

/// The `x[hi..lo] == k` tests the guard makes of `operand`, which fix those
/// bits for every operand tuple the shape covers.
fn slice_constraints(guard: &tmdl::shapes::Predicate, operand: &str) -> Vec<(u32, u32, u128)> {
    use tmdl::shapes::Predicate;
    match guard {
        Predicate::SliceEq { op, lo, hi, value } if op == operand => {
            vec![(u32::from(*lo), u32::from(*hi), *value)]
        }
        // Only a conjunction pins bits: one arm of a disjunction does not.
        Predicate::And(parts) => parts
            .iter()
            .flat_map(|part| slice_constraints(part, operand))
            .collect(),
        _ => Vec::new(),
    }
}

/// An operand tuple the guard of `shape` accepts, built from `base` by making
/// each test the guard makes come out the way that shape needs. Breaking a
/// disjunction means choosing which arm to break, and an arm a sibling test
/// needs back is a dead end, so every combination of those choices is tried.
/// `None` when a test is one this cannot arrange, or when no combination lands
/// in `shape`.
fn case_reaching(instr: &Instruction, shape: &Shape, base: &[u64]) -> Option<Vec<u64>> {
    (0..CHOICE_LIMIT).find_map(|choices| {
        let mut case = base.to_vec();
        let mut choices = Choices(choices);
        satisfy(&shape.guard, instr, &mut case, true, &mut choices)?;
        // The guard has to hold outright: `shape_for` falls back to the first
        // shape when none does. And the encoder takes the first shape whose
        // guard holds, so a tuple an earlier shape also accepts is not a tuple
        // that reaches this one.
        (shape.guard.holds(&reader(instr, &case)) && case_admitted(instr, &case)).then_some(())?;
        instr
            .shape_for(&case)
            .filter(|held| std::ptr::eq(*held, shape))?;
        Some(case)
    })
}

/// Reads an operand out of `case` by name, the way a guard asks for it.
fn reader<'a>(instr: &'a Instruction, case: &'a [u64]) -> impl Fn(&str) -> u64 + 'a {
    move |name: &str| {
        instr
            .operands
            .iter()
            .position(|(operand, _)| operand == name)
            .and_then(|slot| case.get(slot).copied())
            .unwrap_or(0)
    }
}

/// Whether every operand of `case` holds a value its declaration admits. The
/// sampled cases go through the same test, so a synthesized one that skipped it
/// would put the instruction through an operand it does not have.
fn case_admitted(instr: &Instruction, case: &[u64]) -> bool {
    instr
        .operands
        .iter()
        .zip(case)
        .all(|((_, kind), value)| match kind {
            OperandKind::Reg { .. } => true,
            OperandKind::Int(constraint) => constraint.admitted(*value) == Some(*value),
            OperandKind::Bits(width, constraint) => {
                constraint.admitted(*value) == Some(*value)
                    && *value & !(bit_mask(*width) as u64) == 0
            }
        })
}

/// How many combinations of branch choices `case_reaching` tries. Guards on
/// this side of a machine model nest a handful of tests deep, and a walk that
/// needs more than this is one to write a case for by hand.
const CHOICE_LIMIT: u64 = 4096;

/// Which arm to take at each choice point, read off one number a digit at a
/// time, so counting from zero walks every combination.
struct Choices(u64);

impl Choices {
    fn pick(&mut self, arity: usize) -> usize {
        if arity == 0 {
            return 0;
        }
        let arity = arity as u64;
        let taken = self.0 % arity;
        self.0 /= arity;
        taken as usize
    }
}

/// Move `case` to where `predicate` takes the value `want`.
fn satisfy(
    predicate: &tmdl::shapes::Predicate,
    instr: &Instruction,
    case: &mut [u64],
    want: bool,
    choices: &mut Choices,
) -> Option<()> {
    use tmdl::shapes::{CmpOp, Predicate};
    let index = |op: &str| instr.operands.iter().position(|(name, _)| name == op);
    // A test the tuple already answers the wanted way is a test to leave alone:
    // moving an operand for it would undo the work of a neighbouring test.
    if predicate.holds(&reader(instr, case)) == want {
        return Some(());
    }
    match predicate {
        Predicate::Always => want.then_some(()),
        Predicate::Not(inner) => satisfy(inner, instr, case, !want, choices),
        // Every conjunct has to hold; to break the conjunction, breaking any
        // one is enough, and which one is a choice.
        Predicate::And(parts) if want => parts
            .iter()
            .try_for_each(|part| satisfy(part, instr, case, true, choices)),
        Predicate::And(parts) => satisfy(
            parts.get(choices.pick(parts.len()))?,
            instr,
            case,
            false,
            choices,
        ),
        Predicate::Or(parts) if want => satisfy(
            parts.get(choices.pick(parts.len()))?,
            instr,
            case,
            true,
            choices,
        ),
        Predicate::Or(parts) => parts
            .iter()
            .try_for_each(|part| satisfy(part, instr, case, false, choices)),
        Predicate::Bit { op, bit } => {
            let slot = &mut case[index(op)?];
            match want {
                true => *slot |= 1 << bit,
                false => *slot &= !(1 << bit),
            }
            Some(())
        }
        Predicate::SliceEq { op, lo, hi, value } => {
            let slot = &mut case[index(op)?];
            let mask = (bit_mask(u32::from(hi - lo + 1)) << lo) as u64;
            let value = ((*value << lo) as u64) & mask;
            *slot = match want {
                true => (*slot & !mask) | value,
                // Any other pattern of those bits will do.
                false => (*slot & !mask) | ((value ^ mask) & mask),
            };
            Some(())
        }
        Predicate::Cmp {
            op,
            cmp_width,
            cmp,
            value,
            ..
        } => {
            let slot = &mut case[index(op)?];
            let mask = bit_mask(u32::from(*cmp_width)) as u64;
            let value = (*value as u64) & mask;
            let hit = match (cmp, want) {
                (CmpOp::Eq, true) | (CmpOp::Ne, false) => value,
                (CmpOp::Eq, false) | (CmpOp::Ne, true) => value.wrapping_add(1) & mask,
                (CmpOp::Lt | CmpOp::ULt, true) | (CmpOp::Ge | CmpOp::UGe, false) => {
                    value.checked_sub(1)? & mask
                }
                (CmpOp::Le | CmpOp::ULe, true) | (CmpOp::Gt | CmpOp::UGt, false) => value,
                (CmpOp::Gt | CmpOp::UGt, true) | (CmpOp::Le | CmpOp::ULe, false) => {
                    value.wrapping_add(1) & mask
                }
                (CmpOp::Ge | CmpOp::UGe, true) | (CmpOp::Lt | CmpOp::ULt, false) => value,
            };
            *slot = hit;
            Some(())
        }
        Predicate::Fits { op, bits, .. } => {
            let slot = &mut case[index(op)?];
            *slot = match want {
                // The largest value the narrow field holds.
                true => bit_mask(u32::from(bits.checked_sub(1)?)) as u64,
                // One bit past it, which the field cannot hold either way.
                false => 1u64.checked_shl(u32::from(*bits))?,
            };
            Some(())
        }
    }
}

/// Whether the guard asks that `operand` survives `bits` bits as a signed
/// value, which is what makes the bits the shape drops recoverable.
fn signed_fit(guard: &tmdl::shapes::Predicate, operand: &str, bits: u32) -> bool {
    use tmdl::shapes::Predicate;
    match guard {
        Predicate::Fits {
            op,
            bits: n,
            signed,
            ..
        } => *signed && op == operand && u32::from(*n) == bits,
        Predicate::And(parts) | Predicate::Or(parts) => {
            parts.iter().any(|part| signed_fit(part, operand, bits))
        }
        Predicate::Not(inner) => signed_fit(inner, operand, bits),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Isla library execution (cached per instruction word)
// ---------------------------------------------------------------------------

fn sail_traces(
    tools: &Tools,
    out_dir: &Path,
    words: &[(u128, u32)],
) -> anyhow::Result<HashMap<u128, Option<Vec<Vec<tir_verify::TraceEvent>>>>> {
    let fingerprint = tools.verifier.cache_fingerprint();
    let cache_path = |word| {
        out_dir
            .join("cache")
            .join(format!("{word:020x}-{fingerprint:016x}.json"))
    };
    let mut result = HashMap::new();
    let mut missing = Vec::new();
    let mut widths = Vec::new();
    for &(word, width) in words {
        if result.contains_key(&word) {
            continue;
        }
        match std::fs::read(cache_path(word)) {
            Ok(json) => {
                result.insert(word, Some(serde_json::from_slice(&json)?));
            }
            Err(_) => {
                missing.push(word);
                widths.push(width);
            }
        }
    }
    if !missing.is_empty() {
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

/// Whether a memory event's kind is a plain data access. Old-interface
/// models (RISC-V) use enum atoms (`|Read_plain|`); new-interface models
/// (ARM) embed the whole request struct, whose `access_kind` must be an
/// explicit access of plain variety. Acquire/release strength changes ordering,
/// which a single-instruction state comparison does not model.
fn is_plain_access(kind: &tir_verify::TraceValue) -> bool {
    kind.smt.trim_matches('|').ends_with("_plain") || kind.smt.contains("|AV_plain|")
}

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
    /// Initial state the model read, as `(state, Sail value)`.
    reads: Vec<(State, String)>,
    /// Final value per written state (last write wins).
    writes: HashMap<State, String>,
    /// Plain memory reads, related to the initial TMDL memory array.
    mem_reads: Vec<MemAccess>,
    /// Plain memory writes in order, folded into the expected final array.
    mem_writes: Vec<MemAccess>,
    /// Verbatim `(declare-const v Sort)` lines.
    declares: Vec<String>,
    inputs: std::collections::HashSet<String>,
    /// Ordered `define-const` bindings, replayed as a `let` chain.
    defines: Vec<(String, String)>,
    asserts: Vec<String>,
    /// Why this path cannot be checked against the TMDL state (trap paths,
    /// CSR accesses, ...), if so.
    excluded: Option<String>,
}

fn exclude(info: &mut TraceInfo, reason: String) {
    if info.excluded.is_none() {
        info.excluded = Some(reason);
    }
}

/// Relate a register read to TMDL's initial state. A read returning a
/// `define-const` variable, or following a write, reads back a value the
/// model computed rather than initial state.
fn analyze_register_read(
    spec: &IsaSpec,
    info: &mut TraceInfo,
    defined_vars: &std::collections::HashSet<String>,
    name: &str,
    fields: &[String],
    value: &tir_verify::TraceValue,
) {
    let name = name.trim_matches('|');
    if spec.ignore.iter().any(|ignored| ignored == name) {
        return;
    }
    if spec.mmio.iter().any(|mmio| mmio == name) {
        exclude(
            info,
            format!("reads MMIO-backed register {name} (platform memory map)"),
        );
        return;
    }
    let mut mapped = false;
    for row in spec.map.iter().filter(|row| row.register == name) {
        mapped = true;
        match row.select(fields, value) {
            Ok(Some(read))
                if !info.writes.contains_key(&row.state) && !defined_vars.contains(&read) =>
            {
                info.reads.push((row.state.clone(), row.slice(read)));
            }
            Ok(_) => {}
            Err(reason) => exclude(info, reason),
        }
    }
    if !mapped && value.symbolic {
        exclude(info, format!("reads unmapped register {name}"));
    }
}

fn analyze_register_write(
    spec: &IsaSpec,
    info: &mut TraceInfo,
    name: &str,
    fields: &[String],
    value: &tir_verify::TraceValue,
) {
    let name = name.trim_matches('|');
    if spec.ignore.iter().any(|ignored| ignored == name) {
        return;
    }
    let mut mapped = false;
    for row in spec.map.iter().filter(|row| row.register == name) {
        match row.select(fields, value) {
            Ok(Some(written)) => {
                info.writes.insert(row.state.clone(), row.slice(written));
                mapped = true;
            }
            Ok(None) => {}
            Err(reason) => exclude(info, reason),
        }
    }
    if !mapped {
        exclude(info, format!("writes unmapped {name} (trap/system path)"));
    }
}

fn analyze_memory_access(
    spec: &IsaSpec,
    info: &mut TraceInfo,
    is_read: bool,
    kind: &tir_verify::TraceValue,
    address: &tir_verify::TraceValue,
    value: &tir_verify::TraceValue,
    bytes: u32,
) {
    if !is_plain_access(kind) {
        exclude(info, format!("non-plain memory access {}", kind.smt));
        return;
    }
    if !matches!(bytes, 1 | 2 | 4 | 8) {
        exclude(info, format!("unsupported access width {}", bytes));
        return;
    }
    let access = MemAccess {
        value: value.smt.clone(),
        address: if spec.xlen == 32 {
            format!("((_ extract 31 0) {})", address.smt)
        } else {
            address.smt.clone()
        },
        bytes,
    };
    if is_read {
        if !info.mem_writes.is_empty() {
            exclude(info, "memory read after write".to_string());
            return;
        }
        info.mem_reads.push(access);
    } else {
        info.mem_writes.push(access);
    }
}

fn symbolic_variables(expression: &str) -> impl Iterator<Item = &str> {
    expression
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| {
            word.strip_prefix('v').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            })
        })
}

fn analyze_trace(spec: &IsaSpec, events: &[tir_verify::TraceEvent]) -> TraceInfo {
    let mut info = TraceInfo::default();
    // Variables bound by `define-const`: reads returning them are read-backs
    // of values the model computed (e.g. Sail writes `nextPC = PC + 4` and
    // reads it back later), not symbolic initial state.
    let mut defined_vars = std::collections::HashSet::new();

    for event in events {
        let input = match event {
            tir_verify::TraceEvent::ReadRegister { value, .. }
            | tir_verify::TraceEvent::ReadMemory { value, .. } => Some(value),
            _ => None,
        };
        if let Some(input) = input {
            info.inputs
                .extend(symbolic_variables(&input.smt).map(str::to_owned));
        }
        match event {
            tir_verify::TraceEvent::ReadRegister {
                name,
                fields,
                value,
            } => analyze_register_read(spec, &mut info, &defined_vars, name, fields, value),
            tir_verify::TraceEvent::WriteRegister {
                name,
                fields,
                value,
            } => analyze_register_write(spec, &mut info, name, fields, value),
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
            } => analyze_memory_access(spec, &mut info, true, kind, address, value, *bytes),
            tir_verify::TraceEvent::WriteMemory {
                kind,
                address,
                value,
                bytes,
            } => analyze_memory_access(spec, &mut info, false, kind, address, value, *bytes),
        }
    }
    let definitions: HashMap<_, _> = info
        .defines
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect();
    let mut pending: Vec<_> = info.inputs.iter().cloned().collect();
    while let Some(name) = pending.pop() {
        if let Some(expression) = definitions.get(name.as_str()) {
            for dependency in symbolic_variables(expression) {
                if info.inputs.insert(dependency.to_owned()) {
                    pending.push(dependency.to_owned());
                }
            }
        }
    }
    // A completing x86 instruction always advances the PC; a path that never
    // writes it faulted or decoded to a different instruction (an artifact of
    // the model's forking address decode), so it cannot be checked against TMDL.
    if spec.requires_pc_write
        && info.excluded.is_none()
        && !info.writes.contains_key(&State::Pc)
        && !info.writes.contains_key(&State::NextPc)
    {
        exclude(&mut info, "incomplete path (no PC write)".to_string());
    }
    info
}

/// The pinned x86 snapshot subtracts CMPS operands in the opposite order.
/// Build its status flags from its own RSI and RDI bytes in ISA order.
fn normalize_x86_cmps_flags(trace: &mut TraceInfo, bytes: u32) -> anyhow::Result<()> {
    anyhow::ensure!(
        trace.mem_reads.len() == 2 * bytes as usize
            && trace.mem_reads.iter().all(|read| read.bytes == 1)
            && trace.mem_writes.is_empty(),
        "Sail CMPS memory trace changed"
    );
    let operand = |start: usize| {
        trace.mem_reads[start..start + bytes as usize]
            .iter()
            .rev()
            .map(|read| read.value.clone())
            .reduce(|high, low| format!("(concat {high} {low})"))
            .expect("CMPS has at least one byte")
    };
    let lhs = operand(0);
    let rhs = operand(bytes as usize);
    let width = bytes * 8;
    let diff = format!("(bvsub {lhs} {rhs})");
    let parity = (0..8)
        .map(|bit| format!("((_ extract {bit} {bit}) {diff})"))
        .reduce(|a, b| format!("(bvxor {a} {b})"))
        .context("CMPS has at least one parity bit")?;
    let high = width - 1;
    let flags = [
        (0, format!("(ite (bvult {lhs} {rhs}) #b1 #b0)")),
        (1, format!("(bvnot {parity})")),
        (2, format!("(ite (= {lhs} {rhs}) #b1 #b0)")),
        (3, format!("((_ extract {high} {high}) {diff})")),
        (
            4,
            format!("((_ extract {high} {high}) (bvand (bvxor {lhs} {rhs}) (bvxor {lhs} {diff})))"),
        ),
        (
            5,
            format!("((_ extract 4 4) (bvxor (bvxor {lhs} {rhs}) {diff}))"),
        ),
    ];
    trace.writes.extend(flags.map(|(index, value)| {
        let flag = State::Slot {
            class: "eflags".to_string(),
            index,
        };
        (flag, value)
    }));
    Ok(())
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

/// Replay only the Sail definitions reached from this query body.
fn with_trace_defines(trace: &TraceInfo, mut body: String) -> String {
    let mut needed: HashSet<String> = symbolic_variables(&body).map(str::to_owned).collect();
    for (var, expr) in trace.defines.iter().rev() {
        if needed.remove(var) {
            needed.extend(symbolic_variables(expr).map(str::to_owned));
            body = format!("(let (({} {}))\n{})", var, expr, body);
        }
    }
    body
}

fn emit_state_transition(
    spec: &IsaSpec,
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
) -> String {
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
    q
}

/// Assumptions on the addresses this instance may form: PC alignment on a
/// fixed-width ISA, canonical (low-half) addresses on x86-64.
fn emit_address_assumptions(
    spec: &IsaSpec,
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
) -> String {
    let mut q = String::new();
    // A value is (low-half) canonical when bits 63..47 are all zero: a valid
    // user x86-64 linear address that the model's 52-bit physical masking leaves
    // unchanged. Non-canonical accesses/jumps `#GP`, which TMDL's flat model
    // does not track.
    let canonical = |v: &str| format!("(= ((_ extract 63 47) {v}) (_ bv0 17))");

    // Fixed-width ISAs align the PC to the concrete instruction width. x86
    // pins the PC to a concrete aligned value in its config instead.
    if spec.align_pc {
        let alignment_bits = instr.width_bytes(case).trailing_zeros();
        if alignment_bits > 0 {
            let _ = writeln!(
                q,
                "(assert (= ((_ extract {} 0) st0_pc) (_ bv0 {})))",
                alignment_bits - 1,
                alignment_bits
            );
        }
    }

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
        let alignment_bits = instr.width_bytes(case).trailing_zeros();
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
    q
}

fn read_slot(model: &FlatModel, class: &str, index: u64, state: &str) -> String {
    let width = model.classes[class].index_width;
    flat_read_register(model, class, state, &format!("(_ bv{index} {width})"))
}

/// Pin the symbolic initial values the model read back to TMDL's initial state.
fn emit_trace_read_constraints(
    spec: &IsaSpec,
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
    trace: &TraceInfo,
) -> String {
    let mut q = String::new();
    for decl in &trace.declares {
        q.push_str(decl);
        q.push('\n');
    }
    for (state, value) in &trace.reads {
        let initial = match state {
            State::Pc => "st0_pc".to_string(),
            State::NextPc => format!(
                "(bvadd st0_pc (_ bv{} {}))",
                instr.width_bytes(case),
                spec.xlen
            ),
            State::Slot { class, index } => read_slot(model, class, *index, "st0"),
            State::Zero(width) => format!("(_ bv0 {width})"),
        };
        let _ = writeln!(q, "(assert (= {value} {initial}))");
    }
    q
}

/// The reference x86 model gives a concrete CF for oversized narrow shifts.
/// Intel leaves SHL/SHR CF undefined there; SAR's CF remains the old sign bit.
fn x86_narrow_shift_count(
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
) -> Option<(u32, String)> {
    let name = instr.name.as_str();
    if !["shl", "shr", "sal", "sar"]
        .iter()
        .any(|op| name.starts_with(op))
    {
        return None;
    }
    let OperandKind::Reg { class, .. } = &instr.operands.first()?.1 else {
        return None;
    };
    let width = model.classes[class].value_width;
    if !matches!(width, 8 | 16) {
        return None;
    }
    let count = if name.contains("imm") {
        format!("(_ bv{} 64)", case.get(1)? & 31)
    } else if name.contains("cl") {
        format!(
            "(bvand {} (_ bv31 64))",
            flat_read_register(model, "gpr", "st0", "(_ bv1 4)")
        )
    } else {
        return None;
    };
    Some((u32::from(width), count))
}

fn x86_carry_equality(
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
    trace: &TraceInfo,
    tmdl: &str,
    sail: &str,
) -> Option<String> {
    if let Some((width, count)) = x86_narrow_shift_count(model, instr, case) {
        let defined = format!("(bvult {count} (_ bv{width} 64))");
        if instr.name.starts_with("sar") {
            let OperandKind::Reg { class, idx_width } = &instr.operands[0].1 else {
                unreachable!()
            };
            let old = flat_read_register(
                model,
                class,
                "st0",
                &format!("(_ bv{} {idx_width})", case[0]),
            );
            let sign = format!("((_ extract {} {}) {old})", width - 1, width - 1);
            return Some(format!("(= {tmdl} (ite {defined} {sail} {sign}))"));
        }
        return Some(format!("(or (not {defined}) (= {tmdl} {sail}))"));
    }

    // The pinned model uses the result's low bit for ROR carry.
    let (width, mask) = match instr.name.as_str() {
        "rorimm" => (64, 63),
        "rorimm32" => (32, 31),
        _ => return None,
    };
    if *case.get(1)? & mask == 0 {
        return None;
    }
    let result = trace.writes.get(&State::Slot {
        class: "gpr".to_string(),
        index: *case.first()?,
    })?;
    let high = width - 1;
    Some(format!("(= {tmdl} ((_ extract {high} {high}) {result}))"))
}

fn query_prelude(
    spec: &IsaSpec,
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
    trace: &TraceInfo,
) -> String {
    let mut query = emit_state_transition(spec, model, instr, case);
    query.push_str(&emit_address_assumptions(spec, model, instr, case));
    query.push_str(&emit_trace_read_constraints(
        spec, model, instr, case, trace,
    ));
    query
}

/// Prove the Sail and TMDL read addresses agree before replacing Sail's
/// masked addresses. This keeps multiplication out of the address proof.
fn normalize_x86_imul_read_addresses(
    tools: &Tools,
    spec: &IsaSpec,
    model: &FlatModel,
    instr: &Instruction,
    case: &[u64],
    trace: &mut TraceInfo,
    query_path: &Path,
) -> anyhow::Result<()> {
    // Match one TMDL load to Isla's bytewise reads before comparing addresses.
    let [access] = instr.memory_accesses.as_slice() else {
        return Ok(());
    };
    if spec.name != "x86_64"
        || !instr.name.starts_with("imul")
        || access.kind != "load"
        || trace.mem_reads.len() != access.bytes as usize
        || !trace.mem_reads.iter().all(|read| read.bytes == 1)
        || !trace.mem_writes.is_empty()
    {
        return Ok(());
    }

    let base = mem_addr_exprs(instr, case, spec)
        .into_iter()
        .next()
        .expect("single memory access");
    let addresses: Vec<String> = (0..access.bytes)
        .map(|offset| {
            if offset == 0 {
                base.clone()
            } else {
                format!("(bvadd {base} (_ bv{offset} {}))", spec.xlen)
            }
        })
        .collect();
    let equalities = trace
        .mem_reads
        .iter()
        .zip(&addresses)
        .map(|(read, expected)| format!("(= {} {expected})", read.address))
        .collect::<Vec<_>>()
        .join(" ");
    // A satisfiable query would expose a path where the addresses differ.
    let path = trace.asserts.join(" ");
    let mut query = query_prelude(spec, model, instr, case, trace);
    let body = with_trace_defines(trace, format!("(and {path} (not (and {equalities})))"));
    let _ = writeln!(query, "(assert {body})\n(check-sat)");
    let address_path = query_path.with_extension("addr.smt2");
    std::fs::write(&address_path, query)?;
    let output = run_solver(tools, &address_path)?;
    // A counterexample or solver unknown keeps the original Sail addresses.
    if solver_statuses(&output)
        .last()
        .is_some_and(|status| status == "unsat")
    {
        for (read, address) in trace.mem_reads.iter_mut().zip(addresses) {
            read.address = address;
        }
    }
    Ok(())
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
    let mut q = query_prelude(spec, model, instr, case, trace);

    let width_bytes = instr.width_bytes(case);
    // Every mapped slot, once: several Sail names may alias one slot (SP_ELx),
    // and a write through any alias is the slot's final value.
    let mut final_eq = Vec::new();
    let mut compared = HashSet::new();
    for row in &spec.map {
        let State::Slot { class, index } = &row.state else {
            continue;
        };
        let written = || {
            instr
                .fixed_register_writes
                .iter()
                .any(|(written, slot)| written == class && u64::from(*slot) == *index)
        };
        if !compared.insert(&row.state) || (row.if_written && !written()) {
            continue;
        }
        let sail = trace
            .writes
            .get(&row.state)
            .cloned()
            .unwrap_or_else(|| read_slot(model, class, *index, "st0"));
        let tmdl = read_slot(model, class, *index, "st1");
        let carry = (class == "eflags" && *index == 0)
            .then(|| x86_carry_equality(model, instr, case, trace, &tmdl, &sail))
            .flatten();
        final_eq.push(carry.unwrap_or_else(|| format!("(= {tmdl} {sail})")));
    }
    // Models with a delayed PC (RISC-V `nextPC`) announce taken branches
    // there; the ARM model writes the PC register directly.
    let sail_pc = trace
        .writes
        .get(&State::NextPc)
        .or_else(|| trace.writes.get(&State::Pc));
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
    let with_defines = |body| with_trace_defines(trace, body);
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
        q.push_str("(push 1)\n");
        let probe = with_defines(format!("(and {path} (not {modeled}))"));
        let _ = writeln!(q, "(assert {probe})");
        q.push_str("(check-sat)\n(pop 1)\n");
    }
    let cause_constraint = modeled.as_deref().unwrap_or("true");
    let reachable = with_defines(format!("(and {path} {cause_constraint})"));
    // An unsat equivalence query proves nothing when the path is unreachable
    // under the assumptions above, so the path must be shown reachable first.
    let _ = writeln!(q, "(push 1)\n(assert {reachable})\n(check-sat)\n(pop 1)");
    let agrees = with_defines(format!(
        "(and {path} {cause_constraint} {})",
        final_eq.join("\n  ")
    ));
    let compared = with_defines(format!("(and {})", final_eq.join(" ")));
    let used: HashSet<_> = symbolic_variables(&compared).collect();
    let choices = trace
        .declares
        .iter()
        .filter_map(|declaration| {
            let binding = declaration
                .strip_prefix("(declare-const ")?
                .strip_suffix(')')?;
            let (name, _) = binding.split_once(' ')?;
            (used.contains(name) && !trace.inputs.contains(name)).then(|| format!("({binding})"))
        })
        .collect::<Vec<_>>();
    let body = if choices.is_empty() {
        with_defines(format!(
            "(and {path} {cause_constraint} (not (and {})))",
            final_eq.join("\n  ")
        ))
    } else {
        q = q.replacen("(set-logic QF_AUFBV)", "(set-logic AUFBV)", 1);
        format!(
            "(and {reachable} (not (exists ({}) {agrees})))",
            choices.join(" ")
        )
    };
    let _ = writeln!(q, "(assert {})", body);
    q.push_str("(check-sat)\n");

    // Counterexample probes, only evaluated on `sat`.
    let mut probes: Vec<String> = vec!["st0_pc".into(), "st1_pc".into()];
    for row in &spec.map {
        if let State::Slot { class, index } = &row.state {
            if class == "gpr" {
                probes.push(read_slot(model, class, *index, "st0"));
            }
        }
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
    /// Paths unreachable under the checker's assumptions, whose equivalence
    /// query would be unsat without proving anything.
    vacuous: usize,
    excluded_paths: usize,
    excluded_reasons: HashMap<String, usize>,
    unsupported: Vec<String>,
    /// Instructions whose encoding is proved to decode back to itself, and
    /// those where the solver could not show it. Two instructions with the same
    /// bytes land in the second list: the word cannot say which one it was.
    roundtrip_proved: Vec<String>,
    roundtrip_open: Vec<String>,
    /// Instructions whose verified paths also verify with the TMDL behavior
    /// replaced by a no-op: the proofs compare none of the state they write.
    behavior_independent: Vec<String>,
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
        self.vacuous += other.vacuous;
        self.excluded_paths += other.excluded_paths;
        for (reason, count) in other.excluded_reasons {
            *self.excluded_reasons.entry(reason).or_default() += count;
        }
        self.unsupported.append(&mut other.unsupported);
        self.behavior_independent
            .append(&mut other.behavior_independent);
        self.failures.append(&mut other.failures);
    }

    /// The shapes no operand tuple verified. Each is an encoding of a real
    /// instruction that nothing here checked, so the job does not pass with one
    /// outstanding.
    fn uncovered_shapes(&self) -> Vec<&str> {
        self.instructions
            .iter()
            .flat_map(|timing| timing.shape_cases.iter())
            .filter(|(_, cases)| *cases == 0)
            .map(|(shape, _)| shape.as_str())
            .collect()
    }

    fn print(&self) {
        println!("\n=== TMDL vs Sail SMT equivalence ===");
        println!("verified paths:  {}", self.verified);
        println!("divergences:     {}", self.failed);
        println!("solver unknown:  {}", self.unknown);
        println!("vacuous paths:   {}", self.vacuous);
        println!(
            "round trips:     {} proved, {} open",
            self.roundtrip_proved.len(),
            self.roundtrip_open.len()
        );
        if !self.roundtrip_open.is_empty() {
            println!("  open: {}", self.roundtrip_open.join(", "));
        }
        println!(
            "excluded paths:  {} (outside the machine-mode/no-trap assumptions)",
            self.excluded_paths
        );
        let mut reasons: Vec<_> = self.excluded_reasons.iter().collect();
        reasons.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        for (reason, n) in reasons {
            println!("  {:5}x {}", n, reason);
        }
        let shapes: usize = self.instructions.iter().map(|t| t.shape_cases.len()).sum();
        let uncovered = self.uncovered_shapes();
        println!(
            "encoding shapes: {} verified, {} unchecked",
            shapes - uncovered.len(),
            uncovered.len()
        );
        if !uncovered.is_empty() {
            println!("  unchecked: {}", uncovered.join(", "));
        }
        if !self.behavior_independent.is_empty() {
            println!(
                "proofs independent of TMDL behavior: {}",
                self.behavior_independent.join(", ")
            );
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
    /// Cases per encoding shape whose paths the solver agreed with. A zero
    /// here is a shape nothing verified.
    shape_cases: Vec<(String, usize)>,
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
        .filter(|case| operand_case_is_valid(spec, model, instr, case))
        .collect::<Vec<_>>();
    // A shape the sampled cases never reach is a shape nothing verifies, so
    // each one that comes up empty gets a case built to satisfy its guard.
    let mut cases = cases;
    for shape in &instr.shapes {
        let covered = cases.iter().any(|case| {
            instr
                .shape_for(case)
                .is_some_and(|held| std::ptr::eq(held, shape))
        });
        if covered {
            continue;
        }
        let Some(base) = cases.first() else { continue };
        let Some(case) = case_reaching(instr, shape, base) else {
            continue;
        };
        if operand_case_is_valid(spec, model, instr, &case) {
            cases.push(case);
        }
    }
    timing.cases = cases.len();
    // Which shape encodes each case, so the report can say what every shape
    // came out of at the end.
    let shape_of_case: Vec<Option<usize>> = cases
        .iter()
        .map(|case| {
            let held = instr.shape_for(case)?;
            instr
                .shapes
                .iter()
                .position(|shape| std::ptr::eq(shape, held))
        })
        .collect();
    let mut verified_by_case = vec![0usize; cases.len()];
    // The TMDL behavior replaced by a no-op. A proof that still holds for it
    // compares none of the state the instruction writes, so verified paths are
    // re-checked against it until one tells the two apart.
    let mut nop = instr
        .flat_execute
        .as_ref()
        .filter(|execute| {
            execute
                .iter()
                .any(|(field, expr)| *expr != format!("st0_{field}"))
        })
        .map(|execute| Instruction {
            flat_execute: Some(
                execute
                    .keys()
                    .map(|f| (f.clone(), format!("st0_{f}")))
                    .collect(),
            ),
            ..instr.clone()
        });
    let started = Instant::now();
    let words = encode_words(instr, &cases);
    timing.encode_ms = started.elapsed().as_millis();
    let word_widths: Vec<(u128, u32)> = cases
        .iter()
        .zip(&words)
        .map(|(case, word)| (*word, instr.width_bytes(case) * 8))
        .collect();
    let decode_started = Instant::now();
    let cases = decode_operands(instr, &cases, &words);
    timing.decode_ms = decode_started.elapsed().as_millis();
    let mut line = String::new();
    let isla_started = Instant::now();
    let traces_by_word = sail_traces(tools, out_dir, &word_widths)?;
    timing.isla_ms = isla_started.elapsed().as_millis();

    for (index, (case, word)) in cases.iter().zip(&words).enumerate() {
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
            let mut info = analyze_trace(spec, events);
            if let Some(reason) = &info.excluded {
                report.excluded_paths += 1;
                *report
                    .excluded_reasons
                    .entry(format!("{}: {}", instr.name, reason))
                    .or_default() += 1;
                line.push('-');
                continue;
            }
            if spec.name == "x86_64" {
                let bytes = match instr.name.as_str() {
                    "cmpsb" => Some(1),
                    "cmpsw" => Some(2),
                    "cmpsdstring" => Some(4),
                    "cmpsq" => Some(8),
                    _ => None,
                };
                if let Some(bytes) = bytes {
                    normalize_x86_cmps_flags(&mut info, bytes)?;
                }
            }
            let query_path = out_dir
                .join("queries")
                .join(format!("{}_{:08x}_p{}.smt2", instr.name, word, path_idx));
            let solver_started = Instant::now();
            normalize_x86_imul_read_addresses(
                tools,
                spec,
                model,
                instr,
                case,
                &mut info,
                &query_path,
            )?;
            // A path writing a trap cause TMDL does not model (access fault)
            // lies outside the all-of-memory-is-RAM assumption.
            let written_cause = spec.trap_cause.as_ref().and_then(|trap| {
                let row = spec.map.iter().find(|row| row.register == trap.register)?;
                Some((info.writes.get(&row.state)?, trap.causes.as_slice()))
            });
            let cause = written_cause.map(|(cause, causes)| (cause.as_str(), causes));
            let query = build_query(spec, model, instr, case, &info, cause);
            std::fs::write(&query_path, &query)?;
            let output = run_solver(tools, &query_path)?;
            timing.solver_ms += solver_started.elapsed().as_millis();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let is_status = |line: &&str| matches!(*line, "sat" | "unsat" | "unknown");
            let mut statuses = stdout.lines().filter(is_status);
            let unmodeled_status = written_cause.and_then(|_| statuses.next());
            let reachable_status = statuses.next();
            let equivalence_status = statuses.next();
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
            } else if reachable_status == Some("unsat") {
                report.vacuous += 1;
                line.push('V');
            } else if reachable_status == Some("sat") && equivalence_status == Some("unsat") {
                report.verified += 1;
                verified_by_case[index] += 1;
                line.push('.');
                if let Some(mutant) = &nop {
                    let mutant_started = Instant::now();
                    let mutant_path = query_path.with_extension("nop.smt2");
                    std::fs::write(
                        &mutant_path,
                        build_query(spec, model, mutant, case, &info, cause),
                    )?;
                    let statuses = solver_statuses(&run_solver(tools, &mutant_path)?);
                    timing.solver_ms += mutant_started.elapsed().as_millis();
                    // Only a reachable path the no-op still verifies counts
                    // against the proof.
                    let probes = usize::from(cause.is_some());
                    if !statuses.iter().skip(probes).eq(["sat", "unsat"]) {
                        nop = None;
                    }
                }
            } else if equivalence_status == Some("sat") {
                report.failed += 1;
                line.push('X');
                let model = stdout
                    .lines()
                    .filter(|line| !is_status(line))
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
    // Every shape is its own encoding of the instruction, so the report counts
    // the cases each one came out of that the solver agreed with. Reaching a
    // shape is not verifying it: a case whose word Isla could not run leaves
    // its shape as unchecked as no case at all.
    timing.shape_cases = instr
        .shapes
        .iter()
        .enumerate()
        .map(|(index, shape)| {
            let verified = shape_of_case
                .iter()
                .zip(&verified_by_case)
                .filter(|(held, paths)| **held == Some(index) && **paths > 0)
                .count();
            (shape.name.clone(), verified)
        })
        .collect();
    if nop.is_some() && report.verified > 0 {
        report.behavior_independent.push(instr.name.clone());
    }
    timing.total_ms = total_started.elapsed().as_millis();
    Ok((report, timing, format!("{:24}{}", instr.name, line)))
}

fn run_z3(tools: &Tools, path: &Path) -> anyhow::Result<std::process::Output> {
    let first = Command::new(&tools.z3)
        .args(["-smt2", "-T:30", "smt.random_seed=0"])
        .arg(path)
        .output()?;
    let statuses = solver_statuses(&first);
    let final_status = statuses.last().map(String::as_str);
    if matches!(final_status, Some("sat" | "unsat")) {
        return Ok(first);
    }
    let second = Command::new(&tools.z3)
        .args(["-smt2", "-T:30", "smt.random_seed=1"])
        .arg(path)
        .output()?;
    anyhow::ensure!(
        !solver_rejected(&second),
        "z3 rejected {}:\n{}{}",
        path.display(),
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    Ok(second)
}

/// Ask the solver whether `roundtrip_<instr>` holds for every operand tuple.
/// The obligation is emitted by the SMT backend next to the model; a `sat`
/// answer is an operand tuple whose word decodes to something else.
fn prove_roundtrip(
    tools: &Tools,
    spec: &IsaSpec,
    out_dir: &Path,
    model: &Path,
    instr: &Instruction,
) -> anyhow::Result<bool> {
    let mut query = std::fs::read_to_string(model)?;
    let mut args = Vec::new();
    for (name, kind) in &instr.operands {
        let width = match kind {
            OperandKind::Reg { idx_width, .. } => *idx_width,
            _ => spec.xlen,
        };
        let _ = writeln!(query, "(declare-const rt_{name} (_ BitVec {width}))");
        args.push(format!("rt_{name}"));
    }
    let call = match args.is_empty() {
        true => format!("roundtrip_{}", instr.name),
        false => format!("(roundtrip_{} {})", instr.name, args.join(" ")),
    };
    let _ = writeln!(query, "(assert (not {call}))\n(check-sat)");
    let path = out_dir
        .join("queries")
        .join(format!("{}_roundtrip.smt2", instr.name));
    std::fs::write(&path, query)?;
    let output = run_solver(tools, &path)?;
    Ok(solver_statuses(&output)
        .last()
        .is_some_and(|s| s == "unsat"))
}

fn solver_statuses(output: &std::process::Output) -> Vec<String> {
    if solver_rejected(output) {
        return Vec::new();
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter(|line| matches!(*line, "sat" | "unsat" | "unknown"))
        .map(str::to_string)
        .collect()
}

fn solver_rejected(output: &std::process::Output) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_model_error = stdout
        .lines()
        .rfind(|line| matches!(*line, "sat" | "unsat" | "unknown"))
        .is_some_and(|status| status == "unsat");
    let errors = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("(error "))
        .collect::<Vec<_>>();
    errors
        .iter()
        .any(|line| !(expected_model_error && line.contains("model is not available")))
        || (!output.status.success() && errors.is_empty())
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
    // Only the final verdict can be a counterexample; earlier probes such as
    // path reachability are expected to be sat.
    if statuses.last().is_some_and(|s| s == "sat") {
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

    /// Metadata for one 32-bit `load rd, imm`, with the operand declarations
    /// spliced in so a test can vary the constraints they carry.
    fn load_metadata(operands: &str) -> String {
        METADATA_TEMPLATE.replace("OPERANDS", operands)
    }

    const METADATA_TEMPLATE: &str = r#"{
          "version": 1,
          "isa": "TestIsa",
          "dialect": "test",
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
            "bit_offset": 0, "indices": [0, 1, 2, 3, 4, 5]
          }],
          "instructions": [{
            "name": "load", "writes_pc": false, "width_bits": 32,
            "operands": [OPERANDS],
            "supported": true, "write_classes": ["gpr"], "fixed_register_writes": [],
            "uses_reservation": false, "pc_source_operands": [],
            "memory_accesses": [{"kind": "load", "bytes": 4, "address": "(read_gpr st rd)", "flat_address": "(select st0_gpr rd)"}],
            "trap_kinds": ["misaligned_load"],
            "shapes": [{
              "name": "load", "width_bits": 32, "guard": "always",
              "fields": [
                {"word_low": 7, "word_high": 11, "operand": "rd", "operand_low": 0, "value": "0"},
                {"word_low": 0, "word_high": 6, "operand": null, "operand_low": 0, "value": "3"}
              ]
            }],
            "flat_execute": {"gpr": "st0_gpr", "mem": "st0_mem", "resv": "st0_resv", "resa": "st0_resa", "pc": "st0_pc"}
          }]
        }"#;

    #[test]
    fn parses_structured_instruction_metadata() {
        let json = load_metadata(
            r#"{"name": "rd", "kind": "register", "class": "gpr", "width": 5, "align": 1, "nonzero": false},
               {"name": "imm", "kind": "bits", "class": null, "width": 12, "align": 1, "nonzero": false}"#,
        );
        let inventory = parse_inventory(&json).unwrap();
        let instruction = &inventory.instructions[0];
        assert_eq!(instruction.name, "load");
        assert_eq!(inventory.isa, "TestIsa");
        assert_eq!(inventory.dialect, "test");
        assert_eq!(instruction.write_classes, ["gpr"]);
        assert_eq!(
            instruction.memory_accesses[0].flat_address,
            "(select st0_gpr rd)"
        );
        let cases = [vec![5, 0]];
        let words = encode_words(instruction, &cases);
        assert_eq!(words, [5 << 7 | 3]);
        assert_eq!(decode_operands(instruction, &cases, &words)[0][0], 5);
    }

    // A boundary case the operand's `#[align]`/`#[nonzero]` exclude is not a
    // case the instruction has: the concrete pass must not send it to Sail.
    #[test]
    fn operand_cases_honour_immediate_constraints() {
        let json = load_metadata(
            r#"{"name": "rd", "kind": "register", "class": "gpr", "width": 5, "align": 1, "nonzero": false},
               {"name": "imm", "kind": "bits", "class": null, "width": 12, "align": 4, "nonzero": true}"#,
        );
        let inventory = parse_inventory(&json).unwrap();
        let spec = IsaSpec::load("riscv64").unwrap();
        let cases = operand_cases(&spec, &inventory.instructions[0]);
        assert!(!cases.is_empty());
        assert!(cases.iter().all(|case| case[1] != 0 && case[1] % 4 == 0));
    }

    #[test]
    fn x86_flag_queries_use_eflags_storage_slots() {
        let spec = &IsaSpec::load("x86_64").unwrap();
        let model = FlatModel {
            fields: vec![],
            classes: [
                (
                    "eflags".into(),
                    RegisterClassMetadata {
                        name: "eflags".into(),
                        storage: "eflags".into(),
                        index_width: 3,
                        value_width: 1,
                        storage_width: 1,
                        indices: (0..5).collect(),
                        zero_index: None,
                        bit_offset: 0,
                    },
                ),
                (
                    "gpr".into(),
                    RegisterClassMetadata {
                        name: "gpr".into(),
                        storage: "gpr".into(),
                        index_width: 4,
                        value_width: 64,
                        storage_width: 64,
                        indices: (0..16).collect(),
                        zero_index: None,
                        bit_offset: 0,
                    },
                ),
            ]
            .into(),
        };
        let instruction = Instruction {
            name: "cmp".into(),
            writes_pc: false,
            width_bits: 8,
            operands: vec![],
            supported: true,
            write_classes: vec!["eflags".into()],
            fixed_register_writes: vec![("eflags".into(), 0)],
            uses_reservation: false,
            pc_source_operands: vec![],
            memory_accesses: vec![],
            shapes: vec![],
            flat_execute: Some(BTreeMap::new()),
        };
        let trace = analyze_trace(
            spec,
            &[tir_verify::TraceEvent::ReadRegister {
                name: "rflags".into(),
                fields: vec![],
                value: tir_verify::TraceValue {
                    smt: "v0".into(),
                    symbolic: true,
                    fields: HashMap::new(),
                },
            }],
        );

        let query = build_query(spec, &model, &instruction, &[], &trace, None);

        for (slot, bit) in [(0, 0), (2, 6), (3, 7), (4, 11)] {
            assert!(query.contains(&format!(
                "((_ extract {bit} {bit}) v0) (select st0_eflags (_ bv{slot} 3))"
            )));
        }
    }

    #[test]
    fn solver_errors_are_not_statuses() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"(error \"sort mismatch\")\nsat\n".to_vec(),
            stderr: vec![],
        };

        assert!(solver_statuses(&output).is_empty());
    }

    #[test]
    fn solver_timeout_is_not_rejection() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"timeout\n".to_vec(),
            stderr: vec![],
        };

        assert!(!solver_rejected(&output));
        assert!(solver_statuses(&output).is_empty());
    }

    #[test]
    fn unavailable_model_after_unsat_keeps_status() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(256),
            stdout: b"unsat\n(error \"model is not available\")\n".to_vec(),
            stderr: vec![],
        };

        assert_eq!(solver_statuses(&output), ["unsat"]);
    }
}
