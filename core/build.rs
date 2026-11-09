#[path = "defs/relations/build.rs"]
mod relations;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    relations::compile(std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR")?))
}
