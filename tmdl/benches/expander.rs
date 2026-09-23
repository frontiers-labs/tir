use tir_bench::Suite;

use tmdl::{MacroTable, StringArena, collect_macros, expand, lex};

// The real macro-heavy x86_64 defs, concatenated in build.rs order so every
// macro definition precedes its invocations (collect_macros is order-free, but
// this mirrors the shipped input). Included at compile time to stay in sync.
const DEFS: &[&str] = &[
    include_str!("../../backends/x86_64/defs/main.tmdl"),
    include_str!("../../backends/x86_64/defs/base.tmdl"),
    include_str!("../../backends/x86_64/defs/arith_ext.tmdl"),
    include_str!("../../backends/x86_64/defs/conditional.tmdl"),
    include_str!("../../backends/x86_64/defs/memory_ext.tmdl"),
    include_str!("../../backends/x86_64/defs/float.tmdl"),
];

fn main() -> tir_bench::Result<()> {
    let mut suite = Suite::from_args(concat!(env!("CARGO_PKG_NAME"), "/expander"))?;
    if suite.options().list {
        suite.list_function("x86_defs/collect_and_expand")?;
        return suite.finish();
    }
    if !suite.matches("x86_defs/collect_and_expand") {
        return suite.finish();
    }
    let input = DEFS.join("\n");
    let (tokens, errs) = lex(&input);
    assert!(errs.is_empty());

    // Synthesized-string arena lifetime is unified with the token lifetime, so
    // it must outlive the loop; it grows across iterations but the run is short.
    let arena = StringArena::new();
    suite.set_throughput(input.len() as u64);
    suite.function("x86_defs/collect_and_expand", |b| {
        b.iter(|| {
            let mut table = MacroTable::new();
            let mut diags = Vec::new();
            let toks = collect_macros("<bench>", tokens.clone(), &mut table, &mut diags);
            assert!(diags.is_empty());
            let (_out, diags) = expand("<bench>", toks, &table, &arena);
            assert!(diags.is_empty());
        })
    })?;
    suite.finish()
}
