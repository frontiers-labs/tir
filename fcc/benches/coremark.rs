use tir_bench::{Criterion, Program};

pub fn coremark() -> Program {
    Program::new("coremark", "fcc/extbench/coremark")
        .git(
            "https://github.com/eembc/coremark.git",
            "1f483d5b8316753a742cbf5590caf5bd0a4e4777",
        )
        .sources([
            "core_list_join.c",
            "core_main.c",
            "core_matrix.c",
            "core_state.c",
            "core_util.c",
            "posix/core_portme.c",
        ])
        .flags(["-I.", "-Iposix", "-DFLAGS_STR=\"\"", "-DPERFORMANCE_RUN=1"])
        .args(["0", "0", "0", "1000000"])
        .verify("verify.py")
        .llvm()
}

fn bench(c: &mut Criterion) {
    c.bench_program(&coremark());
}

// xtask compiles this file too, under an edition that orders a mixed import
// list of types and macros differently, so the macros are named by path.
tir_bench::criterion_group!(benches, bench);
tir_bench::criterion_main!(benches);
