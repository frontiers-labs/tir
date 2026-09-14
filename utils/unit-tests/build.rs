use tmdl::{Action, Compiler, OutputKind};

fn main() {
    let input = "tests/conditional-latency.tmdl";
    let output = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed={input}");
    for (action, name) in [
        (Action::EmitRust, "latency.rs"),
        (Action::EmitOperationList, "latency_ops.rs"),
    ] {
        Compiler::builder()
            .action(action)
            .dialect(Some("latency".to_string()))
            .output(OutputKind::File(format!("{output}/{name}")))
            .add_input(input)
            .build()
            .compile()
            .unwrap();
    }
}
