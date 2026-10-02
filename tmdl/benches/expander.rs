use tir_bench::{Criterion, Throughput, criterion_group, criterion_main};
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

fn x86_defs(c: &mut Criterion) {
    let input = DEFS.join("\n");
    let mut group = c.benchmark_group("x86_defs");
    group.throughput(Throughput::Bytes(input.len() as u64));
    group.bench_function("collect_and_expand", |b| {
        let (tokens, errors) = lex(&input);
        assert!(errors.is_empty());
        let arena = StringArena::new();
        b.iter(|| {
            let mut table = MacroTable::new();
            let mut diagnostics = Vec::new();
            let tokens = collect_macros("<bench>", tokens.clone(), &mut table, &mut diagnostics);
            assert!(diagnostics.is_empty());
            let (_output, diagnostics) = expand("<bench>", tokens, &table, &arena);
            assert!(diagnostics.is_empty());
        });
    });
    group.finish();
}

criterion_group!(benches, x86_defs);
criterion_main!(benches);
