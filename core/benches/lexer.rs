use tir::backend::lex;
use tir_bench::{Criterion, Throughput, criterion_group, criterion_main};

const LARGE_INPUT: &str = include_str!("./Inputs/large.s");

fn large_asm(c: &mut Criterion, tma: &tir_bench::TMAMetrics) {
    let mut group = c.benchmark_group("large_asm");
    group.throughput(Throughput::Bytes(LARGE_INPUT.len() as u64));
    group.bench_function("lex", |b| b.iter(|| assert!(lex(LARGE_INPUT).is_ok())));
    tma.report("large_asm/lex");
    group.finish();
}

criterion_group!(benches, large_asm);
criterion_main!(benches);
