use tir_bench::program::Program;

pub fn definition(root: &std::path::Path) -> Program {
    Program {
        name: "libquantum",
        resources: root.join("fcc/extbench/libquantum"),
        definition: root.join("fcc/benches/libquantum.rs"),
        sources: vec![
            "src/classic.c",
            "src/complex.c",
            "src/decoherence.c",
            "src/expn.c",
            "src/gates.c",
            "src/matrix.c",
            "src/measure.c",
            "src/oaddn.c",
            "src/objcode.c",
            "src/omuln.c",
            "src/qec.c",
            "src/qft.c",
            "src/qureg.c",
            "src/shor.c",
            "src/specrand.c",
            "src/version.c",
        ],
        flags: &["-DSPEC_CPU", "-DSPEC_CPU_NEED_COMPLEX_H"],
        link_flags: &["-lm"],
        args: &["143", "5"],
        validator: Some("verify.py"),
        llvm: true,
        ..Program::default()
    }
}

tir_bench::program_main!(definition);
