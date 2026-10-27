#[path = "support/programs.rs"]
mod programs;
#[path = "support/dhrystone.rs"]
mod validation;

use std::fs;
use std::io;

fn main() -> io::Result<()> {
    let sources = programs::sources("Dhrystone");
    programs::each("dhrystone", &[], |build| {
        let mut objects = Vec::new();
        for source in ["dhry_1.c", "dhry_2.c"] {
            let object = build.output.join(source).with_extension("o");
            build
                .compiler(&format!("compile/{source}"))
                .args(["-std=gnu17", build.level, "-DTIME", "-c"])
                .args(["-I"])
                .args([&sources])
                .args([sources.join(source)])
                .args(["-o"])
                .args([&object])
                .run()?;
            objects.push(object);
        }
        let executable = build.output.join("dhrystone");
        build
            .compiler("link")
            .args(&objects)
            .args(["-o"])
            .args([&executable])
            .run()?;
        let log = build
            .step("run", &executable)
            .args(["100000000"])
            .run_validated(validation::validate)?;
        fs::write(build.output.join("stdout.log"), &log)
    })
}
