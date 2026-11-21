use std::{collections::BTreeMap, error::Error, path::PathBuf};
use tir_cdsl::{CheckedProgram, Schema};

struct RustOutput {
    filter: &'static str,
    destination: &'static str,
}

const RUST_OUTPUTS: &[RustOutput] = &[
    RustOutput {
        filter: "core::dialects::son::*",
        destination: "son_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::builtin::*",
        destination: "builtin.rs",
    },
    RustOutput {
        filter: "core::dialects::fp::*",
        destination: "fp.rs",
    },
    RustOutput {
        filter: "core::dialects::asm::*",
        destination: "asm_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::cfg::*",
        destination: "cfg_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::func::*",
        destination: "func_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::layout::*",
        destination: "layout_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::ptr::*",
        destination: "ptr_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::scf::*",
        destination: "scf_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::state::*",
        destination: "state_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::type_def::*",
        destination: "type_def_ops.rs",
    },
    RustOutput {
        filter: "core::dialects::vector::*",
        destination: "vector_ops.rs",
    },
];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    println!("cargo:rerun-if-changed={}", manifest.join("defs").display());
    let output = PathBuf::from(std::env::var("OUT_DIR")?);
    let mut schema = Schema::from_catalog(tir_ir_schema::IR_SCHEMA)?;
    let files = [
        "bits", "context", "integer", "memory", "loops", "fp", "layout",
    ];
    let rule_source = files
        .iter()
        .map(|name| std::fs::read_to_string(manifest.join(format!("defs/eqsat/{name}.cdsl"))))
        .collect::<Result<Vec<_>, _>>()?
        .join("\n");
    let eqsat = tir_cdsl::compile_source(&rule_source)?.lower()?;
    schema.bind_names([
        ("equal_id", "bool-="),
        ("less_id", "bool-<"),
        ("eqsat_same_bits", "bool-="),
        ("eqsat_bigint", "bigint"),
        ("eqsat_bit_and", "&"),
        ("eqsat_bit_or", "|"),
        ("eqsat_not_bits", "not-Z"),
        ("eqsat_width", "eqsat-width"),
        ("eqsat_attr_case", "eqsat-attr-case"),
        ("eqsat_shift", "<<"),
        ("eqsat_sub", "-"),
        ("eqsat_int_add", "+"),
        ("eqsat_int_sub", "-"),
        ("eqsat_bit_len", "bits"),
        ("eqsat_shift_amount", "eqsat-amount"),
        ("eqsat_sum", "+"),
        ("eqsat_print_bits", "to-string"),
        ("eqsat_product", "*"),
        ("eqsat_mod", "%"),
        ("eqsat_attr_int", "eqsat-attr-int"),
        ("EqSatRebuild", "forms::EqSatRebuild"),
        ("eqsat_attr_bool", "eqsat-attr-bool"),
        ("eqsat_leq_bits", "bool-<="),
    ]);
    schema.bind_external("eqsat-width", &["i64"], "i64")?;
    schema.bind_external("eqsat-attr-bool", &["i64"], "bool")?;
    schema.bind_external("eqsat-attr-int", &["i64"], "BigInt")?;
    schema.bind_external("eqsat-amount", &["BigInt"], "i64")?;
    schema.bind_external("eqsat-bool-bit", &["bool"], "BigInt")?;
    schema.bind_external("eqsat-attr-case", &["i64"], "String")?;
    schema.bind_external(
        "eqsat-pick-case",
        &["String", "String", "bool", "bool"],
        "bool",
    )?;
    schema.extend(&eqsat)?;
    let builtin = tir_cdsl::compile_file(&manifest.join("defs/dialects/builtin.cdsl"))?.lower()?;
    std::fs::write(
        output.join("eqsat-rules.bin"),
        schema.compile_integer_operations(&eqsat, &builtin, "builtin")?,
    )?;
    compile_cfg_analysis(&manifest, &output)?;
    let package = CheckedProgram::compile_for_rust(&manifest.join("defs/lib.cdsl"))?;
    let mut files = BTreeMap::new();
    for file in RUST_OUTPUTS {
        let std::collections::btree_map::Entry::Vacant(destination) = files.entry(file.destination)
        else {
            return Err(format!("duplicate Rust output {}", file.destination).into());
        };
        destination.insert(package.emit_rust_filter("core", file.filter)?);
    }
    for (destination, rust) in files {
        std::fs::write(output.join(destination), rust)?;
    }
    Ok(())
}

fn compile_cfg_analysis(
    manifest: &std::path::Path,
    output: &std::path::Path,
) -> Result<(), Box<dyn Error>> {
    let source = std::fs::read_to_string(manifest.join("defs/analysis/cfg.cdsl"))?;
    let mut engine = tir_egglog::EGraph::default();
    tir_egglog::Program::from_bytes(tir_ir_schema::IR_SCHEMA, engine.database_mut())?;
    let int = engine.database().sort("i64")?;
    engine.database_mut().add_relation("AnalysisRoot", &[int]);
    let (engine, program, producers) =
        Schema::compile_producers(engine, &source, tir_ir_schema::ANALYSIS_INPUTS)?;
    let db = engine.database();
    let schema_bytes = tir_egglog::Rules::compile_schema(db)?.to_bytes();
    std::fs::write(output.join("cfg-analysis-schema.bin"), &schema_bytes)?;
    std::fs::write(output.join("cfg-analysis-rules.bin"), program.to_bytes())?;
    let mut table_meta = String::from("&[\n");
    for producer in &producers {
        let name = match &producer.schedule {
            tir_egglog::Schedule::Saturate(_, inner) => match inner.as_ref() {
                tir_egglog::Schedule::Run(_, cfg) => cfg.ruleset.as_str(),
                _ => return Err("unexpected producer schedule".into()),
            },
            _ => return Err("unexpected producer schedule".into()),
        };
        let names = |ids: &std::collections::BTreeSet<tir_egglog::TableId>| {
            ids.iter()
                .map(|&id| format!("{:?}", db.schema().table(id).name()))
                .collect::<Vec<_>>()
                .join(", ")
        };
        table_meta.push_str(&format!(
            "    ({:?}, &[{}], &[{}]),\n",
            name,
            names(&producer.reads),
            names(&producer.writes)
        ));
    }
    table_meta.push_str("]\n");
    std::fs::write(output.join("cfg-analysis-producers.rs"), table_meta)?;

    // Compile the restructure-nodes transformation pass against IR + cfg analysis schema.
    let mut pass_schema = Schema::from_catalog(&schema_bytes)?;
    pass_schema.bind_names([
        ("equal_id", "bool-="),
        ("less_id", "bool-<"),
        ("int_add", "+"),
        ("int_sub", "-"),
        ("int_mul", "*"),
        ("eqsat_attr_bool", "eqsat-attr-bool"),
        ("eqsat_attr_int", "eqsat-attr-int"),
        ("eqsat_amount", "eqsat-amount"),
        ("eqsat_width", "eqsat-width"),
        ("OrdNext", "tables::OrdNext"),
        ("RegDepth", "tables::RegDepth"),
        ("SeseDupMin", "tables::SeseDupMin"),
    ]);
    pass_schema.bind_external("eqsat-width", &["i64"], "i64")?;
    pass_schema.bind_external("eqsat-attr-bool", &["i64"], "bool")?;
    pass_schema.bind_external("eqsat-attr-int", &["i64"], "BigInt")?;
    pass_schema.bind_external("eqsat-amount", &["BigInt"], "i64")?;
    let pass_src = std::fs::read_to_string(manifest.join("defs/passes/restructure.cdsl"))?;
    let pass_checked = tir_cdsl::compile_source(&pass_src)?.lower()?;
    pass_schema.extend(&pass_checked)?;
    std::fs::write(
        output.join("restructure-rules.bin"),
        pass_schema.compile_bytes(&pass_checked)?,
    )?;
    Ok(())
}
