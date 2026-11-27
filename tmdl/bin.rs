use tmdl::compiler_main;

pub fn main() -> Result<(), Box<dyn std::error::Error>> {
    compiler_main(None, |source, package| {
        Ok(tir_cdsl::CheckedProgram::emit_rust_source(source, package)?)
    })
}
