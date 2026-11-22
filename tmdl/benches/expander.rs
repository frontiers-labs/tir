use tir_bench::{Criterion, Throughput, criterion_group, criterion_main};
use tmdl::{MacroTable, StringArena, collect_macros, expand, lex};

// These x86-64 sources contain cross-file macro definitions and invocations.
const DEFS: &[&str] = &[
    include_str!("../../backends/x86_64/isa_defs/main.tmdl"),
    include_str!("../../backends/x86_64/isa_defs/base.tmdl"),
    include_str!("../../backends/x86_64/isa_defs/arith_ext.tmdl"),
    include_str!("../../backends/x86_64/isa_defs/conditional.tmdl"),
    include_str!("../../backends/x86_64/isa_defs/memory_ext.tmdl"),
    include_str!("../../backends/x86_64/isa_defs/float.tmdl"),
];

fn x86_defs(c: &mut Criterion, tma: &tir_bench::TMAMetrics) {
    let defs = DEFS.join("\n");
    // Keep module loading outside the flat macro workload.
    let input: String = defs
        .split_inclusive('\n')
        .filter(|line| !line.starts_with("use "))
        .collect();
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
    tma.report("x86_defs/collect_and_expand");
    group.finish();
}

criterion_group!(benches, x86_defs);
criterion_main!(benches);
