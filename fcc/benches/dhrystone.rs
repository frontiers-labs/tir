use tir_bench::program::Program;

pub fn definition(root: &std::path::Path) -> Program {
    Program {
        name: "dhrystone",
        resources: root.join("fcc/extbench/dhrystone"),
        definition: root.join("fcc/benches/dhrystone.rs"),
        sources: vec!["dhry_1.c", "dhry_2.c"],
        flags: &["-DTIME"],
        args: &["100000000"],
        validator: Some("verify.py"),
        llvm: true,
        ..Program::default()
    }
}

tir_bench::program_main!(definition);
