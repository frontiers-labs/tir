#[path = "support/coremark.rs"]
mod validation;

use std::fs;
use std::io;
use std::path::Path;
use std::time::Duration;

use tir_bench::Experiment;

fn bench() -> io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let sources = root.join("utils/third-party/CoreMark");
    let executable = std::env::current_exe()?;
    let target = executable.parent().unwrap().parent().unwrap();
    let fcc = target.join("fcc");
    for (compiler, program, prefix) in [
        ("fcc", fcc.as_path(), &["cc"][..]),
        ("gcc", Path::new("gcc"), &[][..]),
        ("clang", Path::new("clang"), &[][..]),
    ] {
        for level in ["-O0", "-O2"] {
            let output = target.join("bench/coremark").join(compiler).join(level);
            fs::create_dir_all(&output)?;
            let mut objects = Vec::new();
            for source in [
                "core_list_join.c",
                "core_main.c",
                "core_matrix.c",
                "core_state.c",
                "core_util.c",
                "posix/core_portme.c",
            ] {
                let object = output.join(source.replace('/', "_")).with_extension("o");
                Experiment::new(
                    format!("{compiler}/coremark/{level}/compile/{source}"),
                    program,
                )
                .args(prefix)
                .args([
                    "-std=gnu17",
                    level,
                    "-DFLAGS_STR=\"\"",
                    "-DPERFORMANCE_RUN=1",
                    "-c",
                ])
                .args(["-I"])
                .args([&sources])
                .args(["-I"])
                .args([sources.join("posix")])
                .args([sources.join(source)])
                .args(["-o"])
                .args([&object])
                .min_duration(Duration::from_secs(1))
                .min_repetitions(5)
                .run()?;
                objects.push(object);
            }
            let executable = output.join("coremark");
            Experiment::new(format!("{compiler}/coremark/{level}/link"), program)
                .args(prefix)
                .args(&objects)
                .args(["-o"])
                .args([&executable])
                .min_duration(Duration::from_millis(500))
                .run()?;
            let log = Experiment::new(format!("{compiler}/coremark/{level}/run"), &executable)
                .args(["0", "0", "0", "600000"])
                .run_validated(validation::validate)?;
            fs::write(output.join("stdout.log"), &log)?;
        }
    }
    Ok(())
}

fn main() -> io::Result<()> {
    bench()
}
