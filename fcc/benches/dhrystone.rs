use tir_bench::{Criterion, Program, criterion_group, criterion_main};

fn bench(c: &mut Criterion) {
    c.bench_program(
        &Program::new("dhrystone", "fcc/extbench/dhrystone")
            .sources(["dhry_1.c", "dhry_2.c"])
            .flags(["-DTIME"])
            .args(["100000000"])
            .verify("verify.py")
            .llvm(),
    );
}

criterion_group!(benches, bench);
criterion_main!(benches);
