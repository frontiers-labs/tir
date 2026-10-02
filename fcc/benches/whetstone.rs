use tir_bench::{Criterion, Program, criterion_group, criterion_main};

fn bench(c: &mut Criterion) {
    c.bench_program(
        &Program::new("whetstone", "fcc/extbench/whetstone")
            .sources(["whetstone.c"])
            .flags(["-DPRINTOUT"])
            .link_flags(["-lm"])
            .args(["1000000"])
            .verify("verify.py")
            .llvm(),
    );
}

criterion_group!(benches, bench);
criterion_main!(benches);
