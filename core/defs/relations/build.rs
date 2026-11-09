use std::error::Error;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
mod schema;
use tir_egglog::ast::{Command, Expr, Subdatatypes};
use tir_egglog::{Signature, WriteState};

pub fn compile(core: &Path) -> Result<(), Box<dyn Error>> {
    let definitions = core.join("defs/relations");
    println!(
        "cargo:rerun-if-changed={}",
        definitions.join("build.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        definitions.join("schema.rs").display()
    );
    let (sources, dependencies) = tir_cdsl::SourceMap::from_file(definitions.join("schema.cdsl"))
        .map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    for dependency in dependencies {
        println!("cargo:rerun-if-changed={}", dependency.display());
    }
    let module = tir_cdsl::compile(&sources).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    })?;
    let entries = schema::ENTRIES;
    let schema = tir_cdsl::relations::emit_schema(&module, entries)?;
    let mut engine = tir_egglog::EGraph::default();
    engine.run_program(engine.parse_program("ir-schema.egg", &schema)?)?;
    let output = PathBuf::from(std::env::var("OUT_DIR")?);
    std::fs::write(output.join("ir-schema.egg"), schema)?;
    println!(
        "cargo:rerun-if-changed={}",
        definitions.join("tmdl-schema.egg").display()
    );
    let commands = engine.parse_program(
        "tmdl-schema.egg",
        &std::fs::read_to_string(definitions.join("tmdl-schema.egg"))?,
    )?;
    engine.run_program(commands)?;
    std::fs::write(
        output.join("ir-schema.bin"),
        engine.save_program_with_schema()?,
    )?;
    let ir_tables = engine.database().schema().tables().count();
    // Compiling rules needs signatures; their runtime callbacks are never executed here.
    let db = engine.database_mut();
    let integer = db.sort("i64")?;
    let string = db.sort("String")?;
    let natural = db.sort("BigInt")?;
    for name in ["fresh-ir-id", "fresh-ir-type-id"] {
        db.add_primitive_in(
            name,
            Signature {
                inputs: vec![],
                output: integer,
            },
            WriteState::valid_contexts(),
            |_, _| None,
        );
    }
    for (name, input, output) in [
        ("i64-hex", integer, string),
        ("hex-i64", string, integer),
        ("nat-i64", natural, integer),
        ("hex-big", string, natural),
    ] {
        db.add_primitive(
            name,
            Signature {
                inputs: vec![input],
                output,
            },
            |_, _| None,
        );
    }
    let mut sorts = Vec::new();
    for file in [
        "regalloc.egg",
        "regalloc-control.egg",
        "regalloc-structured.egg",
        "regalloc-color.egg",
        "regalloc-bindings.egg",
        "regalloc-abi.egg",
        "regalloc-publish.egg",
        "regalloc-transfer.egg",
        "regalloc-encoding.egg",
        "regalloc-select.egg",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            definitions.join(file).display()
        );
        let source = std::fs::read_to_string(definitions.join(file))?;
        let commands = engine.parse_program(file, &source)?;
        for command in &commands {
            match command {
                Command::Sort {
                    name,
                    presort_and_args,
                    ..
                } => {
                    sorts.push((name.clone(), presort_and_args.clone()));
                }
                Command::Datatype { name, .. } => sorts.push((name.clone(), None)),
                Command::Datatypes { datatypes, .. } => {
                    for (_, name, datatype) in datatypes {
                        let presort = match datatype {
                            Subdatatypes::Variants(_) => None,
                            Subdatatypes::NewSort(name, args) => Some((name.clone(), args.clone())),
                        };
                        sorts.push((name.clone(), presort));
                    }
                }
                _ => {}
            }
        }
        engine.run_program(commands)?;
    }
    std::fs::write(
        output.join("regalloc.bin"),
        engine.save_program_with_schema()?,
    )?;

    let mut schema = String::from(
        "/// Sort declarations required before loading the compiled allocation program.\n\
         pub const REGALLOC_SORTS: &[(&str, Option<&str>, &[&str])] = &[\n",
    );
    // Containers may refer to Eq sorts whose constructors appear later in the source.
    for (name, presort) in sorts
        .iter()
        .filter(|(_, presort)| presort.is_none())
        .chain(sorts.iter().filter(|(_, presort)| presort.is_some()))
    {
        write!(schema, "({name:?}, ")?;
        if let Some((presort, args)) = presort {
            write!(schema, "Some({presort:?}), &[")?;
            for arg in args {
                let Expr::Var(_, name) = arg else {
                    return Err(format!("sort {name} has a non-name presort argument").into());
                };
                write!(schema, "{name:?},")?;
            }
            write!(schema, "]")?;
        } else {
            write!(schema, "None, &[]")?;
        }
        writeln!(schema, "),")?;
    }
    schema.push_str(
        "];\n\n\
         /// Exact tables owned by the allocation program.\n\
         pub const REGALLOC_TABLES: &[&str] = &[\n",
    );
    for (_, table) in engine.database().schema().tables().skip(ir_tables) {
        writeln!(schema, "{:?},", table.name())?;
    }
    schema.push_str("];\n");
    std::fs::write(output.join("regalloc-schema.rs"), schema)?;
    Ok(())
}
