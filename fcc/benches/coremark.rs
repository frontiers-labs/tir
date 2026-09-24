use tir_bench::program::{Program, Source};
use tir_bench::sources::GitSource;

pub fn definition(root: &std::path::Path) -> Program {
    Program {
        name: "coremark",
        resources: root.join("fcc/extbench/coremark"),
        definition: root.join("fcc/benches/coremark.rs"),
        source: Source::Git(GitSource {
            repository: "https://github.com/eembc/coremark.git",
            revision: "1f483d5b8316753a742cbf5590caf5bd0a4e4777",
            subdir: "",
        }),
        sources: vec![
            "core_list_join.c",
            "core_main.c",
            "core_matrix.c",
            "core_state.c",
            "core_util.c",
            "posix/core_portme.c",
        ],
        flags: &["-I.", "-Iposix", "-DFLAGS_STR=\"\"", "-DPERFORMANCE_RUN=1"],
        args: &["0", "0", "0", "1000000"],
        validator: Some("verify.py"),
        llvm: true,
        ..Program::default()
    }
}

tir_bench::program_main!(definition);
