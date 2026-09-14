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
    let source = read_input(args.input.as_ref())?;
    let results = tir::sem::prove_rules(&source, args.width)?;
    let mut failed = false;
    for (name, result) in results {
        match result {
            tir::sem::RuleProofResult::Proven => println!("{name}: proven"),
            tir::sem::RuleProofResult::Admitted => {
                println!("{name}: admitted");
                if !args.allow_admitted {
                    failed = true;
                }
            }
            tir::sem::RuleProofResult::Disproven { counterexample } => {
                print!("{name}: disproven");
                if let Some(counterexample) = counterexample {
                    for binding in counterexample.bindings {
                        print!(" {}={}", binding.name, binding.bits);
                    }
                    if let (Some(lhs), Some(rhs)) =
                        (counterexample.lhs_bits, counterexample.rhs_bits)
                    {
                        print!(" lhs={lhs} rhs={rhs}");
                    }
                }
                println!();
                failed = true;
            }
            tir::sem::RuleProofResult::Unsupported { reason } => {
                let reason = match reason {
                    tir::sem::UnsupportedReason::MissingTheory(reason)
                    | tir::sem::UnsupportedReason::InvalidTypes(reason)
                    | tir::sem::UnsupportedReason::UnsupportedObligation(reason) => reason,
                    tir::sem::UnsupportedReason::Timeout => "solver timeout".into(),
                };
                println!("{name}: unsupported: {reason}");
                failed = true;
            }
        }
    }
    if failed {
        return Err("not every rule was proven".into());
    }
    Ok(())
}
