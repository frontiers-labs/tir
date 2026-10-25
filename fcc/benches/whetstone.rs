#[path = "support/whetstone.rs"]
mod validation;

use std::fs;
use std::io;
use std::path::Path;

use tir_bench::Experiment;

fn bench() -> io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let sources = root.join("utils/third-party/Whetstone");
    let executable = std::env::current_exe()?;
    let target = executable.parent().unwrap().parent().unwrap();
    let fcc = target.join("fcc");
    for (compiler, program, prefix) in [
        ("fcc", fcc.as_path(), &["cc"][..]),
        ("gcc", Path::new("gcc"), &[][..]),
        ("clang", Path::new("clang"), &[][..]),
    ] {
        if compiler == "fcc" {
            Experiment::new("fcc/whetstone", program)
                .skip("FCC cannot select fp.constant instructions")?;
            continue;
        }
        for level in ["-O0", "-O2"] {
            let output = target.join("bench/whetstone").join(compiler).join(level);
            fs::create_dir_all(&output)?;
            let object = output.join("whetstone.o");
            Experiment::new(
                format!("{compiler}/whetstone/{level}/compile/whetstone.c"),
                program,
            )
            .args(prefix)
            .args(["-std=gnu17", level, "-DPRINTOUT", "-c"])
            .args([sources.join("whetstone.c")])
            .args(["-o"])
            .args([&object])
            .run()?;
            let executable = output.join("whetstone");
            Experiment::new(format!("{compiler}/whetstone/{level}/link"), program)
                .args(prefix)
                .args([&object])
                .args(["-lm"])
                .args(["-o"])
                .args([&executable])
                .run()?;
            let log = Experiment::new(format!("{compiler}/whetstone/{level}/run"), &executable)
                .args(["1000000"])
                .run_validated(validation::validate)?;
            fs::write(output.join("stdout.log"), &log)?;
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    bench()
}
