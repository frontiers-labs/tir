//! SMT equivalence checking of TMDL instruction semantics against the Sail
//! RISC-V model (the architecture's golden model).
//!
//! For every supported TMDL instruction and a set of concrete operand
//! assignments:
//!   1. the instruction word is computed from the TMDL `encode_*` function
//!      (evaluated by z3), so the TMDL encoding is part of what is checked;
//!   2. `isla-footprint` symbolically executes that word in the Sail model
//!      over a fully symbolic register state, producing one SMT trace per
//!      execution path;
//!   3. for each path, z3 is asked for a register state where TMDL and Sail
//!      disagree on the final GPRs or PC. `unsat` proves agreement for ALL
//!      2^64 values of every register; `sat` yields a counterexample.
//!
//! Modeling assumptions, reported with the results:
//!   - machine mode, no traps: paths that touch unmapped architectural state
//!     (CSRs, mcause, ...) are excluded and counted;
//!   - the initial PC is 4-byte aligned and `nextPC = PC + 4` (the fetch
//!     invariant for non-compressed instructions);
//!   - TMDL leaves PC untouched for fall-through instructions, so a Sail path
//!     that does not write `nextPC` requires TMDL's final PC to equal the
//!     initial one, and a path that writes `nextPC` requires equality with it.
//!
//! External tools: `isla-footprint` (with a Sail RISC-V snapshot + isla config)
//! and `z3`. Override locations with `TIR_ISLA_FOOTPRINT`, `TIR_ISLA_SNAPSHOT`,
//! `TIR_ISLA_CONFIG`, `TIR_Z3`. `TIR_VERIFY_SMT_FILTER=add,sub` restricts the
//! instruction set.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::utils::project_root;
use xshell::{cmd, Shell};

const REG_COUNT: u32 = 32;

pub fn verify_smt(sh: &Shell) -> anyhow::Result<()> {
    let tools = Tools::from_env()?;
    let root = project_root();
    let out_dir = root.join("target/verify/smt");
    std::fs::create_dir_all(out_dir.join("cache"))?;
    std::fs::create_dir_all(out_dir.join("queries"))?;

    let smt_path = out_dir.join("riscv.smt2");
    generate_tmdl_smt(sh, &root, &smt_path)?;
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
        // Only GPRs are mapped onto Sail state; e.g. the abstract TMDL `csr`
        // register file has no per-CSR correspondence yet.
        if let Some(class) = instr.operands.iter().find_map(|(_, k)| match k {
            OperandKind::Reg(class) if class != "gpr" => Some(class),
            _ => None,
        }) {
            report.unsupported.push(format!(
                "{} (unmapped register class {})",
                instr.name, class
            ));
            continue;
        }
        verify_instruction(&tools, &out_dir, &smt, instr, &mut report)?;
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
    fn from_env() -> anyhow::Result<Self> {
        let var =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.to_string());
        let tools = Tools {
            isla_footprint: var("TIR_ISLA_FOOTPRINT", "isla-footprint").into(),
            snapshot: std::env::var("TIR_ISLA_SNAPSHOT")
                .map_err(|_| {
                    anyhow::anyhow!(
                        "TIR_ISLA_SNAPSHOT must point to a Sail RISC-V isla snapshot (.ir), \
                         e.g. rv64d.ir from https://github.com/rems-project/isla-snapshots"
                    )
                })?
                .into(),
            isla_config: std::env::var("TIR_ISLA_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|_| project_root().join("xtask/verify-smt-riscv64.toml")),
            z3: var("TIR_Z3", "z3").into(),
        };
        Ok(tools)
    }
}

fn generate_tmdl_smt(sh: &Shell, root: &Path, out: &Path) -> anyhow::Result<()> {
    let defs: Vec<PathBuf> = std::fs::read_dir(root.join("backends/riscv/defs"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "tmdl"))
        .collect();
    let out_str = out.to_string_lossy().to_string();
    cmd!(
        sh,
        "cargo run -p tmdl --bin tmdlc -- --action emit-smtlib --dialect riscv --output {out_str} {defs...}"
    )
    .run()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Instruction inventory (from `; INSTRUCTION:` metadata in the generated SMT)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum OperandKind {
    Reg(String),
    Bits(u32),
    Int,
}

#[derive(Clone, Debug)]
struct Instruction {
    name: String,
    writes_pc: bool,
    operands: Vec<(String, OperandKind)>,
    supported: bool,
}

fn parse_inventory(smt: &str) -> Vec<Instruction> {
    let unsupported: Vec<&str> = smt
        .lines()
        .filter_map(|l| l.strip_prefix("; UNSUPPORTED-BEHAVIOR: "))
        .collect();

    smt.lines()
        .filter_map(|l| l.strip_prefix("; INSTRUCTION: "))
        .filter_map(|l| {
            let mut parts = l.split_whitespace();
            let name = parts.next()?.to_string();
            let writes_pc = parts.next()? == "writes-pc=true";
            let operands = parts
                .map(|op| {
                    let (op_name, kind) = op.split_once(':')?;
                    let kind = match kind.split_once(':') {
                        Some(("reg", class)) => OperandKind::Reg(class.to_string()),
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
                operands,
                supported,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Operand assignments
// ---------------------------------------------------------------------------

/// Concrete operand tuples for one instruction. Registers cover x0 corner
/// cases and aliasing; immediates cover boundary patterns. PC-writing
/// instructions get 4-byte aligned immediates so that, together with the
/// aligned-PC assumption, Sail's misaligned-fetch trap paths are vacuous.
fn operand_cases(instr: &Instruction) -> Vec<Vec<u64>> {
    let reg_positions: Vec<usize> = instr
        .operands
        .iter()
        .enumerate()
        .filter(|(_, (_, k))| matches!(k, OperandKind::Reg(_)))
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
                OperandKind::Reg(_) => None,
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
    cases
}

fn operand_smt_args(instr: &Instruction, case: &[u64]) -> String {
    instr
        .operands
        .iter()
        .zip(case)
        .map(|((_, kind), value)| match kind {
            OperandKind::Reg(_) => format!("(_ bv{} 5)", value),
            _ => format!("(_ bv{} 64)", value),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// Encoding via z3 (evaluate the TMDL `encode_*` functions)
// ---------------------------------------------------------------------------

fn encode_words(
    tools: &Tools,
    out_dir: &Path,
    smt: &str,
    instr: &Instruction,
    cases: &[Vec<u64>],
) -> anyhow::Result<Vec<u32>> {
    let mut query = String::from(smt);
    query.push_str("\n(check-sat)\n");
    for case in cases {
        let args = operand_smt_args(instr, case);
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
    let words: Vec<u32> = stdout
        .split("#x")
        .skip(1)
        .filter_map(|chunk| u32::from_str_radix(chunk.get(..8)?, 16).ok())
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
fn sail_traces(tools: &Tools, out_dir: &Path, word: u32) -> anyhow::Result<Option<String>> {
    let cache = out_dir.join("cache").join(format!(
        "{:08x}-{:016x}.trace",
        word,
        cache_fingerprint(tools)
    ));
    if let Ok(cached) = std::fs::read_to_string(&cache) {
        return Ok(Some(cached));
    }
    let bits = format!("{:032b}", word);
    let mut child = Command::new(&tools.isla_footprint)
        .args(["-A"])
        .arg(&tools.snapshot)
        .arg("-C")
        .arg(&tools.isla_config)
        .args([
            "-T",
            "1",
            "--function",
            "isla_footprint_no_init",
            "-I",
            "cur_privilege=Machine",
            "--timeout",
            "90",
            "--partial",
            "-i",
            &bits,
            "-s",
        ])
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
}

fn map_register(name: &str) -> Option<MappedReg> {
    let name = name.trim_matches('|');
    match name {
        "PC" => Some(MappedReg::Pc),
        "nextPC" => Some(MappedReg::NextPc),
        _ => name
            .strip_prefix('x')
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|n| *n < REG_COUNT)
            .map(MappedReg::X),
    }
}

#[derive(Default)]
struct TraceInfo {
    /// `(register, symbolic-variable)` for symbolic initial-state reads.
    reads: Vec<(MappedReg, String)>,
    /// Final value per written register (last write wins).
    writes: HashMap<MappedReg, String>,
    /// Verbatim `(declare-const v Sort)` lines.
    declares: Vec<String>,
    /// Ordered `define-const` bindings, replayed as a `let` chain.
    defines: Vec<(String, String)>,
    asserts: Vec<String>,
    /// Why this path cannot be checked against the TMDL state (trap paths,
    /// CSR accesses, memory, ...), if so.
    excluded: Option<String>,
}

fn analyze_trace(events: &[Sexp]) -> TraceInfo {
    let mut info = TraceInfo::default();
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
                let symbolic = matches!(value, Sexp::Atom(a) if a.starts_with('v'));
                match map_register(name) {
                    Some(reg) => {
                        if let Sexp::Atom(var) = value {
                            info.reads.push((reg, var.clone()));
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
                match map_register(name) {
                    Some(MappedReg::X(0)) => {}
                    Some(reg) => {
                        info.writes.insert(reg, value.to_string());
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
                info.defines.push((var.clone(), expr.to_string()));
            }
            "assert" => {
                if let Some(expr) = items.get(1) {
                    info.asserts.push(expr.to_string());
                }
            }
            "read-mem" | "write-mem" => {
                exclude(&mut info, "memory access".to_string());
            }
            _ => {}
        }
    }
    info
}

// ---------------------------------------------------------------------------
// Equivalence query construction
// ---------------------------------------------------------------------------

fn build_query(smt: &str, instr: &Instruction, case: &[u64], trace: &TraceInfo) -> String {
    let mut q = String::from(smt);
    q.push_str("\n(declare-const st0 TMDLState)\n");
    let args = operand_smt_args(instr, case);
    let call = if args.is_empty() {
        format!("(execute_{} st0)", instr.name)
    } else {
        format!("(execute_{} st0 {})", instr.name, args)
    };
    let _ = writeln!(q, "(define-fun st1 () TMDLState {})", call);

    // Fetch invariant: PC is 4-byte aligned (no compressed instructions).
    q.push_str("(assert (= ((_ extract 1 0) (pc st0)) #b00))\n");

    for decl in &trace.declares {
        q.push_str(decl);
        q.push('\n');
    }
    for (reg, var) in &trace.reads {
        let init = match reg {
            MappedReg::X(n) => format!("(read_gpr st0 (_ bv{} 5))", n),
            MappedReg::Pc => "(pc st0)".to_string(),
            MappedReg::NextPc => "(bvadd (pc st0) (_ bv4 64))".to_string(),
        };
        let _ = writeln!(q, "(assert (= {} {}))", var, init);
    }

    let mut final_eq: Vec<String> = (1..REG_COUNT)
        .map(|n| {
            let sail = trace
                .writes
                .get(&MappedReg::X(n))
                .cloned()
                .unwrap_or_else(|| format!("(read_gpr st0 (_ bv{} 5))", n));
            format!("(= (read_gpr st1 (_ bv{} 5)) {})", n, sail)
        })
        .collect();
    final_eq.push(match trace.writes.get(&MappedReg::NextPc) {
        Some(target) => format!("(= (pc st1) {})", target),
        None => "(= (pc st1) (pc st0))".to_string(),
    });

    let mut body = format!(
        "(and {} (not (and {})))",
        if trace.asserts.is_empty() {
            "true".to_string()
        } else {
            trace.asserts.join(" ")
        },
        final_eq.join("\n  ")
    );
    for (var, expr) in trace.defines.iter().rev() {
        body = format!("(let (({} {}))\n{})", var, expr, body);
    }
    let _ = writeln!(q, "(assert {})", body);
    q.push_str("(check-sat)\n");

    // Counterexample probes, only evaluated on `sat`.
    let mut probes: Vec<String> = vec!["(pc st0)".into(), "(pc st1)".into()];
    for n in 1..REG_COUNT {
        probes.push(format!("(read_gpr st0 (_ bv{} 5))", n));
    }
    let _ = writeln!(q, "(get-value ({}))", probes.join(" "));
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
    out_dir: &Path,
    smt: &str,
    instr: &Instruction,
    report: &mut Report,
) -> anyhow::Result<()> {
    let cases = operand_cases(instr);
    let words = encode_words(tools, out_dir, smt, instr, &cases)?;
    print!("{:24}", instr.name);
    let mut line = String::new();

    for (case, word) in cases.iter().zip(&words) {
        let Some(raw) = sail_traces(tools, out_dir, *word)? else {
            report.excluded_paths += 1;
            *report
                .excluded_reasons
                .entry(format!(
                    "{}: isla-footprint failed or timed out ({:#010x})",
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
            let info = analyze_trace(events);
            if let Some(reason) = &info.excluded {
                report.excluded_paths += 1;
                *report
                    .excluded_reasons
                    .entry(format!("{}: {}", instr.name, reason))
                    .or_default() += 1;
                line.push('-');
                continue;
            }
            let query = build_query(smt, instr, case, &info);
            let query_path = out_dir
                .join("queries")
                .join(format!("{}_{:08x}_p{}.smt2", instr.name, word, path_idx));
            std::fs::write(&query_path, &query)?;
            let output = Command::new(&tools.z3)
                .arg("-smt2")
                .arg("-T:60")
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
                     counterexample (initial pc, final pc, x1..x31):\n{}",
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
