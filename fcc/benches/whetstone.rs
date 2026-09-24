use tir_bench::program::Program;

pub fn definition(root: &std::path::Path) -> Program {
    Program {
        name: "whetstone",
        resources: root.join("fcc/extbench/whetstone"),
        definition: root.join("fcc/benches/whetstone.rs"),
        sources: vec!["whetstone.c"],
        link_flags: &["-lm"],
        args: &["1000000"],
        ..Program::default()
    }
}

tir_bench::program_main!(definition);
