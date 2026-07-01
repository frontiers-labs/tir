use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use tir::backend::lex;

const LARGE_INPUT: &str = include_str!("./Inputs/large.s");

fn large_asm(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_asm");
    group.throughput(Throughput::Bytes(LARGE_INPUT.len() as u64));
    group.bench_function("lex", |b| {
        b.iter(|| {
            let result = lex(LARGE_INPUT);
            assert!(result.is_ok());
        })
    });
    group.finish()
}

criterion_group!(benches, large_asm);
criterion_main!(benches);
