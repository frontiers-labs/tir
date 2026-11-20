fn main() -> Result<(), Box<dyn std::error::Error>> {
    tmdl::build_backend(
        "arm64",
        &[
            "./defs/main.tmdl",
            "./defs/versions.tmdl",
            "./defs/float.tmdl",
            "./defs/advsimd.tmdl",
            "./defs/data_processing.tmdl",
            "./defs/loads_stores.tmdl",
            "./defs/atomics.tmdl",
            "./defs/branches.tmdl",
            "./defs/perf.tmdl",
            "./defs/cpu/arm/cortex_a720.tmdl",
        ],
    )
}
