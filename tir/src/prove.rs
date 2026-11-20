use std::{error::Error, ffi::OsString};

use clap::Args;

use crate::common::read_input;

#[derive(Args)]
pub struct ToolArgs {
    /// The width every width name of a rule takes for the proof.
    #[arg(long, default_value_t = 8)]
    width: u64,

    /// Exit successfully when every non-SMT obligation has checked admission.
    #[arg(long)]
    allow_admitted: bool,

    /// Input PDL file, or `-`/omitted for stdin.
    input: Option<OsString>,
}

/// Report one proof outcome per PDL rule, failing unless every rule is proven.
pub fn run(args: ToolArgs) -> Result<(), Box<dyn Error>> {
    let _source = read_input(args.input.as_ref())?;
    Err("PDL rule proofs are unavailable during the IR migration".into())
}
