#[path = "support/coremark.rs"]
mod coremark;
#[path = "support/programs.rs"]
mod programs;

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

/// CoreMark's run rules reject a timed run shorter than this.
const MIN_RUN_SECONDS: f64 = 10.0;

/// CoreMark parses its iteration count as an `ee_s32`.
const MAX_ITERATIONS: u64 = i32::MAX as u64;

fn performance_run(iterations: u64) -> io::Result<[String; 4]> {
    if iterations > MAX_ITERATIONS {
        return Err(io::Error::other(format!(
            "CoreMark needs more than {MAX_ITERATIONS} iterations to run long enough; its clock \
             may not advance"
        )));
    }
    Ok(["0".into(), "0".into(), "0".into(), iterations.to_string()])
}

// Sizes a run a tenth past the minimum from one that lasted `seconds`.
fn sized(iterations: u64, seconds: f64) -> u64 {
    (iterations as f64 * MIN_RUN_SECONDS * 1.1 / seconds).ceil() as u64
}

// Doubles a run until it lasts a second, as CoreMark's own calibration grows
// one, and sizes the timed run from it.
fn estimate(executable: &Path) -> io::Result<u64> {
    let mut iterations = 1;
    loop {
        let output = Command::new(executable)
            .args(performance_run(iterations)?)
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "CoreMark probe {executable:?} failed with {}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let seconds = coremark::reported(
            &String::from_utf8_lossy(&output.stdout),
            "Total time (secs)",
        );
        if seconds >= 1.0 {
            return Ok(sized(iterations, seconds));
        }
        iterations *= 2;
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
        // Short runs are up to 12% slower per iteration than full ones, so the
        // estimate can fall short of the minimum, which CoreMark then refuses
        // to validate. Such a run is sized again from its own time.
        let mut iterations = estimate(&executable)?;
        loop {
            let seconds = Cell::new(None);
            let run = build
                .step("run", &executable)
                .args(performance_run(iterations)?)
                // A run averages its iterations already.
                .min_repetitions(1)
                .run_self_timed(|output| {
                    seconds.set(Some(coremark::reported(output, "Total time (secs)")));
                    coremark::time_per_iteration(output)
                });
            match (run, seconds.get()) {
                (Ok(log), _) => return fs::write(build.output.join("stdout.log"), &log),
                (Err(_), Some(seconds)) if seconds < MIN_RUN_SECONDS => {
                    iterations = sized(iterations, seconds);
                }
                (Err(error), _) => return Err(error),
            }
        }
    })
}
