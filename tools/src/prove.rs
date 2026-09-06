use std::{error::Error, ffi::OsString};

use clap::Args;

use crate::common::read_input;

#[derive(Args)]
pub struct ToolArgs {
    /// The width every width name of a rule takes for the proof.
    #[arg(long, default_value_t = 8)]
    width: u64,

    /// Input PDL file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

/// Prove the `proof smt` rules of a PDL file, one line per rule, failing when
/// any rule is refuted.
pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let source = read_input(args.input.as_ref())?;
    let results = tir::sem::prove_rules(&source, args.width)?;
    let mut refuted = false;
    for (name, proven) in results {
        println!("{name}: {}", if proven { "proven" } else { "refuted" });
        refuted |= !proven;
    }
    if refuted {
        return Err("a rule was refuted".into());
    }
    Ok(())
}
