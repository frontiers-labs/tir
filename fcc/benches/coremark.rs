#[path = "support/coremark.rs"]
mod coremark;
#[path = "support/programs.rs"]
mod programs;

use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// CoreMark's run rules reject a timed run shorter than this.
const MIN_RUN_SECONDS: f64 = 10.0;

/// CoreMark parses its iteration count as an `ee_s32`.
const MAX_ITERATIONS: u64 = i32::MAX as u64;

fn performance_run(iterations: u64) -> [String; 4] {
    ["0".into(), "0".into(), "0".into(), iterations.to_string()]
}

fn seconds(executable: &Path, iterations: u64) -> io::Result<f64> {
    if iterations > MAX_ITERATIONS {
        return Err(io::Error::other(format!(
            "CoreMark {executable:?} needs more than {MAX_ITERATIONS} iterations to run for a \
             second; its clock may not advance"
        )));
    }
    let output = Command::new(executable)
        .args(performance_run(iterations))
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "CoreMark probe {executable:?} failed with {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let output = String::from_utf8_lossy(&output.stdout);
    Ok(coremark::reported(&output, "Total time (secs)"))
}

// Doubles a run until it lasts a second, as CoreMark's own calibration grows
// one, then sizes it a tenth past the minimum. Short runs are up to 12% slower
// per iteration than full ones, so a full-length run must confirm the size,
// with half that margin left for run-to-run variation.
fn iterations(executable: &Path) -> io::Result<u64> {
    let mut iterations = 1;
    let mut elapsed = seconds(executable, iterations)?;
    while elapsed < 1.0 {
        iterations *= 2;
        elapsed = seconds(executable, iterations)?;
    }
    loop {
        iterations = (iterations as f64 * MIN_RUN_SECONDS * 1.1 / elapsed).ceil() as u64;
        elapsed = seconds(executable, iterations)?;
        if elapsed >= MIN_RUN_SECONDS * 1.05 {
            return Ok(iterations);
        }
    }
}

fn main() -> io::Result<()> {
    let sources = programs::sources("CoreMark");
    programs::each("coremark", &[], |build| {
        let mut objects = Vec::new();
        for source in [
            "core_list_join.c",
            "core_main.c",
            "core_matrix.c",
            "core_state.c",
            "core_util.c",
            "posix/core_portme.c",
        ] {
            let object = build
                .output
                .join(source.replace('/', "_"))
                .with_extension("o");
            build
                .compiler(&format!("compile/{source}"))
                .args([
                    "-std=gnu17",
                    build.level,
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
                .min_repetitions(5)
                .run()?;
            objects.push(object);
        }
        let executable = build.output.join("coremark");
        build
            .compiler("link")
            .args(&objects)
            .args(["-o"])
            .args([&executable])
            .min_duration(Duration::from_millis(500))
            .run()?;
        let iterations = iterations(&executable)?;
        let log = build
            .step("run", &executable)
            .args(performance_run(iterations))
            // A run averages its iterations already.
            .min_repetitions(1)
            .run_self_timed(coremark::time_per_iteration)?;
        fs::write(build.output.join("stdout.log"), &log)
    })
}
