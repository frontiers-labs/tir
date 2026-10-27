#[path = "support/programs.rs"]
mod programs;
#[path = "support/whetstone.rs"]
mod validation;

use std::fs;
use std::io;

fn main() -> io::Result<()> {
    let sources = programs::sources("Whetstone");
    let unsupported = [("fcc", "FCC cannot select fp.constant instructions")];
    programs::each("whetstone", &unsupported, |build| {
        let object = build.output.join("whetstone.o");
        build
            .compiler("compile/whetstone.c")
            .args(["-std=gnu17", build.level, "-DPRINTOUT", "-c"])
            .args([sources.join("whetstone.c")])
            .args(["-o"])
            .args([&object])
            .run()?;
        let executable = build.output.join("whetstone");
        build
            .compiler("link")
            .args([&object])
            .args(["-lm"])
            .args(["-o"])
            .args([&executable])
            .run()?;
        let log = build
            .step("run", &executable)
            .args(["1000000"])
            .run_validated(validation::validate)?;
        fs::write(build.output.join("stdout.log"), &log)
    })
}
