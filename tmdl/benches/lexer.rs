#[macro_use]
#[path = "../../benchmarks/functions.rs"]
pub mod functions;

use tmdl::lex;

const INPUT: &str = include_str!("./Inputs/large_instr_template.tmdl");

benchmarks! {
    compiler = "tir";
    large_instr_template_lex("large_instr_template/lex", functions::Settings {
        bytes: Some(INPUT.len() as u64),
        ..Default::default()
    }) |b| {
        b.iter(|| {
            let (_, errors) = lex(INPUT);
            assert!(errors.is_empty());
        });
    }
}
