#[macro_use]
#[path = "../../benchmarks/functions.rs"]
pub mod functions;

use tmdl::{lex, parse};

const INPUT: &str = include_str!("./Inputs/large_instr_template.tmdl");

benchmarks! {
    compiler = "tir";
    large_instr_template_parse("large_instr_template/parse", functions::Settings {
        bytes: Some(INPUT.len() as u64),
        ..Default::default()
    }) |b| {
        b.iter(|| {
            let (tokens, errors) = lex(INPUT);
            assert!(errors.is_empty());
            let _ = parse(INPUT, &tokens, "<bench>");
        });
    }
}
