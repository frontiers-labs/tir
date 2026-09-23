use tir_bench::Suite;

use tmdl::lex;

const LARGE_INSTR_TEMPLATE_INPUT: &str = include_str!("./Inputs/large_instr_template.tmdl");

fn main() -> tir_bench::Result<()> {
    let mut suite = Suite::from_args(concat!(env!("CARGO_PKG_NAME"), "/lexer"))?;
    if suite.options().list {
        suite.list_function("large_instr_template/lex")?;
        return suite.finish();
    }
    suite.set_throughput(LARGE_INSTR_TEMPLATE_INPUT.len() as u64);
    suite.function("large_instr_template/lex", |b| {
        b.iter(|| {
            let (_, errs) = lex(LARGE_INSTR_TEMPLATE_INPUT);
            assert!(errs.is_empty());
        })
    })?;
    suite.finish()
}
