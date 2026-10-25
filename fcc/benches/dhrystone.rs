#[path = "support/dhrystone.rs"]
mod validation;

use std::fs;
use std::io;
use std::path::Path;

use tir_bench::Experiment;

fn bench() -> io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let sources = root.join("utils/third-party/Dhrystone");
    let executable = std::env::current_exe()?;
    let target = executable.parent().unwrap().parent().unwrap();
    let fcc = target.join("fcc");
    for (compiler, program, prefix) in [
        ("fcc", fcc.as_path(), &["cc"][..]),
        ("gcc", Path::new("gcc"), &[][..]),
        ("clang", Path::new("clang"), &[][..]),
    ] {
        for level in ["-O0", "-O2"] {
            let output = target.join("bench/dhrystone").join(compiler).join(level);
            fs::create_dir_all(&output)?;
            let mut objects = Vec::new();
            for source in ["dhry_1.c", "dhry_2.c"] {
                let object = output.join(source.replace('/', "_")).with_extension("o");
                Experiment::new(
                    format!("{compiler}/dhrystone/{level}/compile/{source}"),
                    program,
                )
                .args(prefix)
                .args(["-std=gnu17", level, "-DTIME", "-c"])
                .args(["-I"])
                .args([&sources])
                .args([sources.join(source)])
                .args(["-o"])
                .args([&object])
                .run()?;
                objects.push(object);
            }
            let executable = output.join("dhrystone");
            Experiment::new(format!("{compiler}/dhrystone/{level}/link"), program)
                .args(prefix)
                .args(&objects)
                .args(["-o"])
                .args([&executable])
                .run()?;
            let log = Experiment::new(format!("{compiler}/dhrystone/{level}/run"), &executable)
                .args(["100000000"])
                .run_validated(validation::validate)?;
            fs::write(output.join("stdout.log"), &log)?;
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    bench()
}
