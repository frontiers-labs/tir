use tir_bench::{Criterion, Throughput, criterion_group, criterion_main};
use tmdl::lex;

const INPUT: &str = include_str!("./Inputs/large_instr_template.tmdl");

fn large_instr_template(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_instr_template");
    group.throughput(Throughput::Bytes(INPUT.len() as u64));
    group.bench_function("lex", |b| {
        b.iter(|| {
            let (_, errors) = lex(INPUT);
            assert!(errors.is_empty());
        });
    });
    group.finish();
}

criterion_group!(benches, large_instr_template);
criterion_main!(benches);
