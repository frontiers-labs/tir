use super::Program;

pub fn definition() -> Program {
    Program {
        name: "dhrystone",
        sources: vec!["dhry_1.c", "dhry_2.c"],
        flags: &["-DTIME"],
        args: &["100000000"],
        validator: Some("verify.py"),
        llvm: true,
        ..Program::default()
    }
}
