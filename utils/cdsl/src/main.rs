use tir_cdsl::{SourceMap, compile};

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}
fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let command = args
        .next()
        .ok_or("usage: cdslc <check|eval> FILE [BINDING]")?;
    if !matches!(command.as_str(), "check" | "eval") {
        return Err("expected `check` or `eval`".into());
    }
    let file = args.next().ok_or("missing CDSL source file")?;
    let binding = args.next().unwrap_or_else(|| "main".into());
    if args.next().is_some() {
        return Err("too many arguments".into());
    }
    let (sources, _) = SourceMap::from_file(&file).map_err(diagnostics)?;
    let checked = compile(&sources).map_err(diagnostics)?;
    if command == "eval" {
        println!("{}", checked.evaluate(&binding).map_err(|e| e.to_string())?);
    } else {
        println!("checked");
    }
    Ok(())
}
fn diagnostics(errors: Vec<tir_cdsl::diagnostic::Diagnostic>) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}
