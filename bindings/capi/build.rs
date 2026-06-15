use std::env;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=cbindgen.toml");

    let crate_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let config = cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml"))
        .expect("failed to read cbindgen.toml");

    let header = PathBuf::from(&crate_dir).join("include/tir.h");
    match cbindgen::Builder::new()
        .with_crate(&crate_dir)
        .with_config(config)
        .generate()
    {
        // The header is a checked-in artifact; a CI step asserts it is current.
        Ok(bindings) => {
            bindings.write_to_file(&header);
        }
        Err(e) => println!("cargo:warning=cbindgen header generation skipped: {e}"),
    }
}
