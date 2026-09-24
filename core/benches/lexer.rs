#[macro_use]
#[path = "../../benchmarks/functions.rs"]
pub mod functions;

use tir::backend::lex;

const LARGE_INPUT: &str = include_str!("./Inputs/large.s");

benchmarks! {
    compiler = "tir";
    large_asm_lex("large_asm/lex", functions::Settings {
        bytes: Some(LARGE_INPUT.len() as u64),
        ..Default::default()
    }) |b| {
        b.iter(|| assert!(lex(LARGE_INPUT).is_ok()));
    }
}
