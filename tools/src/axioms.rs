//! Regenerate a backend's discovered isel bridge axioms. Run after adding or
//! removing instructions; a per-backend freshness test fails when the
//! committed file no longer matches what discovery finds.

use std::error::Error;
use std::path::PathBuf;

use clap::Parser;
use tir::Context;
use tir::backend::isel::{Rule, discover_axioms, render_axioms_file};

const TARGETS: &[&str] = &["riscv", "arm64", "x86_64"];

#[derive(Parser)]
pub struct ToolArgs {
    /// Target to discover axioms for; all targets when omitted.
    #[arg(long)]
    target: Option<String>,

    /// Write `backends/<target>/src/isel.axioms` instead of printing.
    #[arg(long)]
    write: bool,
}

fn rules_for(target: &str, context: &Context) -> Result<Vec<Rule>, Box<dyn Error>> {
    Ok(match target {
        "riscv" => tir_riscv::get_isel_rules(context, tir_riscv::Feature::ALL),
        "arm64" => tir_arm64::get_isel_rules(context, tir_arm64::Feature::ALL),
        "x86_64" => tir_x86_64::get_isel_rules(context, tir_x86_64::Feature::ALL),
        other => return Err(format!("unknown target `{other}`; expected {TARGETS:?}").into()),
    })
}

pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let targets: Vec<&str> = match &args.target {
        Some(t) => vec![t.as_str()],
        None => TARGETS.to_vec(),
    };

    for target in targets {
        let context = Context::with_default_dialects();
        let rules = rules_for(target, &context)?;
        let axioms = discover_axioms(&rules);
        let file = render_axioms_file(&axioms);

        if args.write {
            // The utility is always run from the workspace it was built in.
            let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../backends")
                .join(target)
                .join("src");
            let path = src.join("isel.axioms");
            std::fs::write(&path, &file)?;
            println!("wrote {}", path.display());
        } else {
            println!("; --- {target} ---");
            print!("{file}");
        }
    }
    Ok(())
}
