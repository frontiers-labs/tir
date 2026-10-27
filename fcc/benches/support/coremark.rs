use std::io;
use std::time::Duration;

fn field<'a>(output: &'a str, name: &str) -> Option<&'a str> {
    output.lines().find_map(|line| {
        let (key, value) = line.trim().trim_start_matches("[0]").split_once(':')?;
        (key.trim() == name).then_some(value.trim())
    })
}

/// A number that CoreMark reports, such as `Total time (secs)`.
pub fn reported(output: &str, name: &str) -> f64 {
    field(output, name)
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("missing CoreMark {name}:\n{output}"))
}

/// Checks that one run is a valid performance run and returns CoreMark's
/// time per iteration.
pub fn time_per_iteration(output: &str) -> io::Result<Duration> {
    // CoreMark validates only runs of at least 10 seconds whose CRCs match
    // those known for their seeds.
    if !output.contains("Correct operation validated.") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("CoreMark did not validate the run:\n{output}"),
        ));
    }
    if field(output, "seedcrc") != Some("0xe9f5") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("not a CoreMark performance run:\n{output}"),
        ));
    }
    Ok(Duration::from_secs_f64(
        reported(output, "Total time (secs)") / reported(output, "Iterations"),
    ))
}
