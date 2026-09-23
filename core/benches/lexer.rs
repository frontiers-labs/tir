use tir_bench::Suite;

use tir::backend::lex;

const LARGE_INPUT: &str = include_str!("./Inputs/large.s");

fn main() -> tir_bench::Result<()> {
    let mut suite = Suite::from_args(concat!(env!("CARGO_PKG_NAME"), "/lexer"))?;
    if suite.options().list {
        suite.list_function("large_asm/lex")?;
        return suite.finish();
    }
    suite.set_throughput(LARGE_INPUT.len() as u64);
    suite.function("large_asm/lex", |b| {
        b.iter(|| {
            let result = lex(LARGE_INPUT);
            assert!(result.is_ok());
        })
    })?;
    suite.finish()
}
