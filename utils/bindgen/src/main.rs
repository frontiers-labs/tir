//! Generate typed language bindings from the TIR operation schema
//! (`tir::schema_json`). One op-constructor is emitted per registered op, so
//! adding ops never requires touching this tool or hand-writing wrappers.
//!
//! Usage: `tir-bindgen [--lang python] [--output PATH]` (defaults: python, stdout).

mod python;

use serde::Deserialize;

#[derive(Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub variadic: bool,
}

#[derive(Deserialize)]
pub struct OpDesc {
    pub dialect: String,
    pub name: String,
    pub operands: Vec<Field>,
    pub results: Vec<Field>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut lang = "python".to_string();
    let mut output: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--lang" if i + 1 < args.len() => {
                lang = args[i + 1].clone();
                i += 2;
            }
            "--output" | "-o" if i + 1 < args.len() => {
                output = Some(args[i + 1].clone());
                i += 2;
            }
            other => {
                eprintln!("usage: tir-bindgen [--lang python] [--output PATH]; bad arg: {other}");
                std::process::exit(2);
            }
        }
    }

    let ops: Vec<OpDesc> =
        serde_json::from_str(&tir::schema_json()).expect("schema JSON must deserialize");

    let code = match lang.as_str() {
        "python" => python::emit(&ops),
        other => {
            eprintln!("unsupported language: {other}");
            std::process::exit(2);
        }
    };

    match output {
        Some(path) => std::fs::write(&path, code).expect("write output"),
        None => print!("{code}"),
    }
}
