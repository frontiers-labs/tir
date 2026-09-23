use super::Program;

pub fn definition() -> Program {
    Program {
        name: "whetstone",
        sources: vec!["whetstone.c"],
        link_flags: &["-lm"],
        args: &["1000000"],
        ..Program::default()
    }
}
