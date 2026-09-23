use super::{Program, Source};
use tir_bench::sources::GitSource;

pub fn definition() -> Program {
    Program {
        name: "coremark",
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
