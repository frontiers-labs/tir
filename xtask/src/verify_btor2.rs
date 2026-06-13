//! `cargo xtask verify-btor2 <isa>` — model-check a Chisel/RTL implementation
//! against the TMDL golden model.
//!
//! The flow has three parts:
//!   1. `tmdlc --emit-btor2` builds the per-instruction reference checker (see
//!      `tmdl/src/btor2gen.rs`).
//!   2. The implementation is lowered to BTOR2 out-of-band (firtool + Yosys);
//!      its formal top must expose the retirement signals as outputs named
//!      exactly as in [`RVFI_SIGNALS`].
//!   3. [`stitch`] composes the two into one miter: the checker's retirement
//!      inputs are rewired to the implementation's like-named outputs, and the
//!      checker's `bad` becomes the miter's property. A BMC engine (btormc /
//!      Bitwuzla) then searches for a divergence.
//!
//! Stitching is kept pure and unit-tested here; the external tool invocations
//! are documented in `docs/tmdl/btor2_model_checking.md`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use xshell::{cmd, Shell};

/// Resolve the ISA to its TMDL name and definitions, then drive the flow.
/// `impl_btor2` is the implementation lowered to BTOR2 out-of-band (firtool +
/// Yosys); see `docs/tmdl/btor2_model_checking.md`.
pub fn run(sh: &Shell, isa: &str, impl_btor2: &Path) -> Result<()> {
    let (tmdl_isa, defs_dir) = match isa {
        "riscv32" => ("RV32I", "backends/riscv/defs"),
        "riscv64" => ("RV64I", "backends/riscv/defs"),
        _ => bail!("unsupported isa `{isa}`; use riscv32 or riscv64"),
    };
    let mut defs: Vec<PathBuf> = std::fs::read_dir(defs_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "tmdl"))
        .collect();
    defs.sort();
    let defs_ref: Vec<&Path> = defs.iter().map(PathBuf::as_path).collect();
    verify_btor2(sh, isa, tmdl_isa, &defs_ref, impl_btor2)
}

/// Retirement interface the implementation's BTOR2 must expose as outputs and
/// the checker consumes as inputs. Order is irrelevant; names must match.
pub const RVFI_SIGNALS: [&str; 9] = [
    "insn", "pc", "rs1_val", "rs2_val", "rd_addr", "rd_we", "rd_val", "next_pc", "valid",
];

/// Node-reference token positions for the BTOR2 opcodes the checker emits.
/// Position 0 is the node id (always renumbered); a `Some(())` sort flag marks
/// opcodes carrying a sort id at position 2. Remaining entries are operand
/// node positions. Literal fields (slice bounds, extend amounts, constants,
/// names) are left untouched.
fn ref_positions(op: &str) -> (bool, &'static [usize]) {
    match op {
        "sort" => (false, &[]),
        "input" | "constd" | "const" | "one" | "zero" | "ones" => (true, &[]),
        "bad" => (false, &[2]),
        "not" | "sext" | "uext" | "slice" => (true, &[3]),
        "ite" => (true, &[3, 4, 5]),
        // binops, comparisons, concat
        _ => (true, &[3, 4]),
    }
}

/// Merge an implementation BTOR2 and the TMDL checker BTOR2 into one miter.
///
/// The implementation lines are emitted verbatim; the checker is appended with
/// every node id shifted past the implementation's, except its retirement
/// inputs, whose references are redirected to the implementation outputs of the
/// same name.
pub fn stitch(implementation: &str, checker: &str, signals: &[&str]) -> Result<String> {
    let mut max_id = 0u32;
    let mut name_to_node: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    for line in non_blank(implementation) {
        let t: Vec<&str> = line.split_whitespace().collect();
        let id: u32 = t[0]
            .parse()
            .with_context(|| format!("implementation: bad node id in `{line}`"))?;
        max_id = max_id.max(id);
        // `<id> output <node> <name>`: record the driver node for each name.
        if t.get(1) == Some(&"output") && t.len() >= 4 {
            let node: u32 = t[2].parse()?;
            name_to_node.insert(t[3], node);
        }
    }

    // Resolve each retirement signal to its implementation node up front so a
    // missing one is a clear contract error, not a dangling reference.
    let offset = max_id;
    let mut wired: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut checker_inputs: std::collections::HashMap<u32, &str> = std::collections::HashMap::new();
    for line in non_blank(checker) {
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.get(1) == Some(&"input") && t.len() >= 4 {
            checker_inputs.insert(t[0].parse()?, t[3]);
        }
    }
    for &sig in signals {
        let input_id = checker_inputs
            .iter()
            .find(|(_, n)| **n == sig)
            .map(|(id, _)| *id)
            .ok_or_else(|| anyhow!("checker has no retirement input `{sig}`"))?;
        let impl_node = *name_to_node
            .get(sig)
            .ok_or_else(|| anyhow!("implementation exposes no output named `{sig}`"))?;
        wired.insert(input_id, impl_node);
    }
    let wired_names: std::collections::HashSet<&str> = signals.iter().copied().collect();

    let remap = |orig: u32| -> u32 { *wired.get(&orig).unwrap_or(&(orig + offset)) };

    let mut out = String::new();
    out.push_str(implementation.trim_end());
    out.push_str("\n; --- TMDL checker (stitched) ---\n");
    for line in non_blank(checker) {
        let mut t: Vec<String> = line.split_whitespace().map(String::from).collect();
        // Drop the rewired retirement inputs; their uses point at the
        // implementation instead.
        if t.get(1).map(String::as_str) == Some("input")
            && t.len() >= 4
            && wired_names.contains(t[3].as_str())
        {
            continue;
        }
        let (has_sort, positions) = ref_positions(&t[1]);
        t[0] = remap(t[0].parse()?).to_string();
        if has_sort {
            t[2] = remap(t[2].parse()?).to_string();
        }
        for &p in positions {
            if p < t.len() {
                t[p] = remap(t[p].parse()?).to_string();
            }
        }
        out.push_str(&t.join(" "));
        out.push('\n');
    }
    Ok(out)
}

fn non_blank(s: &str) -> impl Iterator<Item = &str> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(';'))
}

/// Driver: emit the checker, stitch against a pre-built implementation BTOR2,
/// and run btormc when available.
pub fn verify_btor2(
    sh: &Shell,
    isa: &str,
    tmdl_isa: &str,
    defs: &[&Path],
    impl_btor2: &Path,
) -> Result<()> {
    let out_dir = Path::new("target/verify/btor2");
    sh.create_dir(out_dir)?;
    let checker_path = out_dir.join("checker.btor2");
    let checker_str = checker_path.to_string_lossy().to_string();
    let defs: Vec<String> = defs
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    cmd!(
        sh,
        "cargo run -q -p tmdl --bin tmdlc -- --action=emit-btor2 --isa={tmdl_isa} --output={checker_str} {defs...}"
    )
    .run()
    .context("emitting TMDL checker btor2")?;

    let implementation = sh.read_file(impl_btor2)?;
    let checker = sh.read_file(&checker_path)?;
    let miter = stitch(&implementation, &checker, &RVFI_SIGNALS)?;
    let miter_path = out_dir.join("miter.btor2");
    sh.write_file(&miter_path, &miter)?;
    println!("wrote miter: {} ({isa})", miter_path.display());

    if which("btormc") {
        let miter_str = miter_path.to_string_lossy().to_string();
        let output = cmd!(sh, "btormc {miter_str}").read()?;
        if output.lines().next().map(str::trim) == Some("sat") {
            println!("SAT: implementation diverges from the TMDL model\n{output}");
            bail!("model checking found a counterexample");
        }
        println!("UNSAT up to bound: no divergence found\n{output}");
    } else {
        println!("btormc not found; run it on the miter to search for bugs");
    }
    Ok(())
}

fn which(bin: &str) -> bool {
    std::process::Command::new(bin)
        .arg("--help")
        .output()
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Implementation exposing two retirement signals as outputs.
    const IMPL: &str = "\
1 sort bitvec 8
2 input 1 clk
3 state 1 reg
4 output 3 x
5 input 1 raw
6 output 5 y
";

    // Checker reading x and y, asserting they differ.
    const CHECKER: &str = "\
1 sort bitvec 8
2 input 1 x
3 input 1 y
4 neq 1 2 3
5 bad 4
";

    #[test]
    fn wires_signals_by_name_and_shifts_ids() {
        let m = stitch(IMPL, CHECKER, &["x", "y"]).unwrap();
        // Implementation kept verbatim.
        assert!(m.contains("3 state 1 reg"));
        // Checker offset by max impl id (6): its sort 1 -> 7, neq 4 -> 10, bad -> 11.
        // The neq operands are rewired to impl nodes 3 (x) and 5 (y).
        assert!(m.contains("10 neq 7 3 5"), "neq not rewired: {m}");
        assert!(m.contains("11 bad 10"), "bad not shifted: {m}");
        // The checker's own input lines are dropped.
        assert!(!m.contains("input 7 x"));
    }

    #[test]
    fn errors_when_signal_missing_from_impl() {
        let err = stitch(IMPL, CHECKER, &["x", "z"]).unwrap_err();
        assert!(err.to_string().contains("`z`"), "unexpected error: {err}");
    }

    #[test]
    fn merged_graph_is_valid() {
        let m = stitch(IMPL, CHECKER, &["x", "y"]).unwrap();
        let mut defined = std::collections::HashSet::new();
        for line in super::non_blank(&m) {
            let t: Vec<&str> = line.split_whitespace().collect();
            let id: u32 = t[0].parse().unwrap();
            let (has_sort, refs) = match t[1] {
                "output" => (false, &[2usize][..]),
                "next" => (true, &[3, 4][..]),
                "bad" => (false, &[2][..]),
                "state" | "input" | "sort" => (false, &[][..]),
                "neq" => (true, &[2, 3][..]),
                _ => (false, &[][..]),
            };
            let _ = has_sort;
            for &p in refs {
                let r: u32 = t[p].parse().unwrap();
                assert!(defined.contains(&r), "dangling ref {r} in `{line}`");
            }
            defined.insert(id);
        }
    }
}
