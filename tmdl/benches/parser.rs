use tir_bench::Suite;

use tmdl::{lex, parse};

const LARGE_INSTR_TEMPLATE_INPUT: &str = include_str!("./Inputs/large_instr_template.tmdl");

fn main() -> tir_bench::Result<()> {
    let mut suite = Suite::from_args(concat!(env!("CARGO_PKG_NAME"), "/parser"))?;
    if suite.options().list {
        suite.list_function("large_instr_template/parse")?;
        return suite.finish();
    }
    suite.set_throughput(LARGE_INSTR_TEMPLATE_INPUT.len() as u64);
    suite.function("large_instr_template/parse", |b| {
        b.iter(|| {
            let (tokens, errs) = lex(LARGE_INSTR_TEMPLATE_INPUT);
            assert!(errs.is_empty());
            let _ = parse(LARGE_INSTR_TEMPLATE_INPUT, &tokens, "<bench>");
        })
    })?;
    suite.finish()
}
