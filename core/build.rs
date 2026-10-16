use std::error::Error;
use std::fs;
use std::io;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    // The semantic rule set is parsed here and written out as the Rust that
    // builds it: a typo in it, or a pattern the e-graph can never match, is a
    // build error with a span, and no process parses it again.
    let axioms = check("defs/isel.pdl")?;
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    fs::write(out.join("isel_axioms.rs"), tir_pdl::file_to_rust(&axioms))?;

    let mut source = String::new();
    let mut inputs = Vec::new();
    for input in [
        "src/passes/instcombine/rules/integer.pdl",
        "src/passes/instcombine/rules/bitwise.pdl",
        "src/passes/instcombine/rules/control.pdl",
        "src/passes/instcombine/rules/fp.pdl",
    ] {
        println!("cargo:rerun-if-changed={input}");
        let contents = fs::read_to_string(input)?;
        let start = source.len();
        source.push_str(&contents);
        source.push('\n');
        inputs.push((input, start, contents));
    }
    let rust = match tir_pdl::compile_to_rust(&source) {
        Ok(rust) => rust,
        Err(diagnostics) => {
            let mut stderr = io::stderr().lock();
            for mut diagnostic in diagnostics {
                let (input, start, contents) = inputs
                    .iter()
                    .rev()
                    .find(|(_, start, _)| *start <= diagnostic.span.start)
                    .expect("diagnostic belongs to an input");
                diagnostic.span = ((diagnostic.span.start - start).min(contents.len())
                    ..(diagnostic.span.end - start).min(contents.len()))
                    .into();
                diagnostic.write(input, contents, &mut stderr)?;
            }
            return Err("failed to compile instcombine PDL rules".into());
        }
    };
    fs::write(out.join("instcombine_rules.rs"), rust)?;

    let input = "defs/layout.pdl";
    println!("cargo:rerun-if-changed={input}");
    let contents = fs::read_to_string(input)?;
    let rust = tir_pdl::compile_to_rust(&contents).or_else(|diagnostics| {
        let mut stderr = io::stderr().lock();
        for diagnostic in diagnostics {
            diagnostic.write(input, &contents, &mut stderr)?;
        }
        Err::<String, Box<dyn Error>>("failed to compile layout PDL rules".into())
    })?;
    fs::write(out.join("layout_rules.rs"), rust)?;
    Ok(())
}

fn check(input: &str) -> Result<tir_pdl::File, Box<dyn Error>> {
    println!("cargo:rerun-if-changed={input}");
    let source = fs::read_to_string(input)?;
    tir_pdl::compile_patterns(&source).or_else(|diagnostics| {
        let mut stderr = io::stderr().lock();
        for diagnostic in diagnostics {
            diagnostic.write(input, &source, &mut stderr)?;
        }
        Err(format!("failed to compile {input}").into())
    })
}
