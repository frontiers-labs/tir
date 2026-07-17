//! Hardware model checking against TMDL instruction semantics.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args;
use tmdl::{Action, Compiler, OutputKind};

#[derive(Args)]
pub struct ToolArgs {
    /// Target architecture.
    #[arg(long)]
    target: String,
    /// DUT lowered to BTOR2 with the required retirement outputs.
    dut: PathBuf,
}

pub fn run(args: ToolArgs) -> std::result::Result<(), Box<dyn std::error::Error>> {
    run_model_check(args).map_err(Into::into)
}

fn run_model_check(args: ToolArgs) -> Result<()> {
    let target = tir::backend::select_target(&args.target, None, None)
        .map_err(|error| anyhow!("invalid target `{}`: {error}", args.target))?;
    let description = target.model_check_target().ok_or_else(|| {
        anyhow!(
            "target `{}` does not provide encoded retirement semantics",
            args.target
        )
    })?;
    let enabled_isas = description
        .features
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();

    verify_btor2(
        &args.target,
        description.isa,
        &enabled_isas,
        description.sources,
        &args.dut,
    )
}

/// Node-reference token positions for the BTOR2 opcodes the checker emits.
/// Position 0 is the node id (always renumbered); a `Some(())` sort flag marks
/// opcodes carrying a sort id at position 2. Remaining entries are operand
/// node positions. Literal fields (slice bounds, extend amounts, constants,
/// names) are left untouched.
fn ref_positions(op: &str) -> (bool, &'static [usize]) {
    match op {
        "sort" => (false, &[]),
        "input" | "constd" | "const" | "one" | "zero" | "ones" => (true, &[]),
        // `output <node> <name>` carries no sort; the node is at position 2.
        "output" | "bad" => (false, &[2]),
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
    let mut reset_node: Option<u32> = None;
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
        if t.get(1) == Some(&"input") && t.get(3) == Some(&"reset") {
            reset_node = Some(id);
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
    let mut last = offset;
    let mut bads: Vec<(u32, String)> = Vec::new();
    for line in non_blank(checker) {
        let mut t: Vec<String> = line.split_whitespace().map(String::from).collect();
        last = last.max(remap(t[0].parse()?));
        // Drop the rewired retirement inputs; their uses point at the
        // implementation instead.
        if t.get(1).map(String::as_str) == Some("input")
            && t.len() >= 4
            && wired_names.contains(t[3].as_str())
        {
            continue;
        }
        // Hold each property aside so it can be gated on the reset pulse below.
        if t[1] == "bad" {
            let name = t.get(3).cloned().unwrap_or_default();
            bads.push((remap(t[2].parse()?), name));
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

    if bads.is_empty() {
        bail!("checker has no `bad` property");
    }
    emit_property(&mut out, last, &bads, reset_node);
    Ok(out)
}

/// Emit the miter properties. When the implementation has a `reset` input, drive
/// a one-cycle reset pulse and gate each mismatch until reset has deasserted, so
/// uninitialized pipeline state at step 0 cannot raise a spurious counterexample.
fn emit_property(out: &mut String, last: u32, bads: &[(u32, String)], reset_node: Option<u32>) {
    out.push_str("; --- reset-gated properties ---\n");
    let mut nid = last;
    let mut node = |body: String| -> u32 {
        nid += 1;
        out.push_str(&format!("{nid} {body}\n"));
        nid
    };
    let Some(reset) = reset_node else {
        for (bad, name) in bads {
            node(format!("bad {bad} {name}"));
        }
        return;
    };
    let s1 = node("sort bitvec 1".into());
    let one = node(format!("one {s1}"));
    let zero = node(format!("zero {s1}"));
    // `started` is 0 at step 0 and 1 thereafter.
    let started = node(format!("state {s1} started"));
    node(format!("init {s1} {started} {zero}"));
    node(format!("next {s1} {started} {one}"));
    // Force a one-cycle reset pulse: reset high at step 0, low afterwards.
    let not_started = node(format!("not {s1} {started}"));
    let reset_ok = node(format!("eq {s1} {reset} {not_started}"));
    node(format!("constraint {reset_ok}"));
    for (bad, name) in bads {
        let gated = node(format!("and {s1} {bad} {started}"));
        node(format!("bad {gated} {name}"));
    }
}

fn non_blank(s: &str) -> impl Iterator<Item = &str> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(';'))
}

/// Driver: emit the checker, stitch against a pre-built implementation BTOR2,
/// and run btormc when available.
fn verify_btor2(
    target: &str,
    tmdl_isa: &str,
    enabled_isas: &[String],
    sources: &[(&str, &str)],
    impl_btor2: &Path,
) -> Result<()> {
    let out_dir = Path::new("target/model-check").join(target);
    std::fs::create_dir_all(&out_dir)?;
    let checker_path = out_dir.join("checker.btor2");
    let mut compiler = Compiler::builder()
        .action(Action::EmitBtor2)
        .isa(Some(tmdl_isa.to_string()))
        .btor2_isas(enabled_isas.to_vec())
        .output(OutputKind::File(
            checker_path.to_string_lossy().into_owned(),
        ));
    for (name, source) in sources {
        compiler = compiler.add_source(name, source);
    }
    compiler
        .build()
        .compile()
        .context("failed to emit the TMDL checker")?;

    let implementation = std::fs::read_to_string(impl_btor2)
        .with_context(|| format!("failed to read DUT {}", impl_btor2.display()))?;
    let checker = std::fs::read_to_string(&checker_path)?;
    let signals: Vec<&str> = non_blank(&checker)
        .filter_map(|line| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            (tokens.get(1) == Some(&"input"))
                .then(|| tokens.get(3).copied())
                .flatten()
        })
        .collect();
    let miter = stitch(&implementation, &checker, &signals)?;
    let miter_path = out_dir.join("miter.btor2");
    std::fs::write(&miter_path, &miter)?;
    println!("wrote miter: {}", miter_path.display());

    let output = Command::new("btormc")
        .arg(&miter_path)
        .output()
        .context("failed to run required external engine `btormc`")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stdout.lines().next().map(str::trim) == Some("sat") {
        bail!("model checking found a counterexample\n{stdout}{stderr}");
    }
    if !output.status.success() {
        bail!("btormc failed\n{stdout}{stderr}");
    }
    println!("{stdout}");
    Ok(())
}
