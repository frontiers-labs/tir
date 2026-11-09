use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let source = manifest.join("../../core/defs/types/libs_symbolic_src_term.cdsl");
    let (sources, dependencies) = tir_cdsl::SourceMap::from_file(&source).map_err(|errors| {
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
    let mut hosts = BTreeMap::new();
    hosts.insert("base_INT_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert("base_INT_carrier".to_owned(), syn::parse_quote!(first_arg));
    hosts.insert("base_FLOAT_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert(
        "base_FLOAT_carrier".to_owned(),
        syn::parse_quote!(float_carrier),
    );
    hosts.insert("base_UNIT_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert(
        "base_UNIT_carrier".to_owned(),
        syn::parse_quote!(no_carrier),
    );
    hosts.insert("base_TUPLE_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert(
        "base_TUPLE_carrier".to_owned(),
        syn::parse_quote!(no_carrier),
    );
    hosts.insert("base_STATE_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert(
        "base_STATE_carrier".to_owned(),
        syn::parse_quote!(no_carrier),
    );
    hosts.insert("base_NAT_ADD_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert(
        "base_NAT_ADD_carrier".to_owned(),
        syn::parse_quote!(no_carrier),
    );
    hosts.insert("base_RAW_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert("base_RAW_carrier".to_owned(), syn::parse_quote!(first_arg));
    hosts.insert("base_ITER_validate".to_owned(), syn::parse_quote!(valid));
    hosts.insert(
        "base_ITER_carrier".to_owned(),
        syn::parse_quote!(no_carrier),
    );
    hosts.insert(
        "base_SEM_STATE_validate".to_owned(),
        syn::parse_quote!(valid),
    );
    hosts.insert(
        "base_SEM_STATE_carrier".to_owned(),
        syn::parse_quote!(no_carrier),
    );
    hosts.insert(
        "base_NAMED_validate".to_owned(),
        syn::parse_quote!(__TIR_TYPE_VALIDATE),
    );
    hosts.insert(
        "base_NAMED_carrier".to_owned(),
        syn::parse_quote!(no_carrier),
    );
    let runtime = syn::parse_quote!(super);
    let output = PathBuf::from(std::env::var("OUT_DIR")?);
    for (binding, _) in module
        .bindings()
        .filter(|(name, _)| name.starts_with("constructors::base_"))
    {
        let expression =
            tir_cdsl::output::type_definitions::emit_type_ctor(&module, binding, &hosts, &runtime)?;
        let name = binding
            .strip_prefix("constructors::")
            .ok_or("type constructor namespace missing")?;
        std::fs::write(
            output.join(format!("cdsl-type-{name}.rs")),
            expression.to_string(),
        )?;
    }
    Ok(())
}
