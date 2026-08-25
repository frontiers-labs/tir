//! Whether a program is a legitimate subject for differential testing at all.
//!
//! Two compilers are only obliged to agree on a program whose behavior the
//! standard defines. On one that reads an indeterminate value or overflows a
//! signed multiply they may each do something different and both be right, so
//! a divergence there is noise: it wastes the reader's night and, worse, the
//! reducer will happily shrink a real failure towards it.
//!
//! No single oracle covers the ground, so this one asks two questions. The
//! sanitizers answer for what they are built to catch — overflow,
//! out-of-bounds, bad shifts. Building the program twice with opposite
//! automatic-variable initialization answers for the reads of indeterminate
//! values they do not: a program whose output depends on what happened to be
//! on the stack prints one thing zero-filled and another pattern-filled.
//!
//! Both answers need gcc to cooperate. Where it will not — no sanitizer
//! runtime, a compile that fails, a run that hangs — the verdict is
//! `well_defined`, because suppressing a finding is worse than filing one.

use std::path::Path;
use std::process::Command;

use super::harness::{timed_output, COMPILE_TIMEOUT, RUN_TIMEOUT};

/// Markers a sanitizer prints when it catches something. `-fno-sanitize-recover`
/// also makes the program die, but a program is free to exit however it likes,
/// so the diagnostic is what distinguishes the two.
const DIAGNOSTICS: [&str; 2] = ["runtime error:", "Sanitizer"];

/// Does `source` have defined behavior? Only a program that does can hold a
/// compiler to account for its output.
pub fn well_defined(source: &Path, work_dir: &Path) -> bool {
    !sanitizers_complain(source, work_dir) && !depends_on_uninitialized(source, work_dir)
}

fn sanitizers_complain(source: &Path, work_dir: &Path) -> bool {
    let Some(output) = build_and_run(
        source,
        &work_dir.join("ub-sanitized.bin"),
        &["-fsanitize=undefined,address", "-fno-sanitize-recover=all"],
    ) else {
        return false;
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    DIAGNOSTICS.iter().any(|marker| stderr.contains(marker))
}

fn depends_on_uninitialized(source: &Path, work_dir: &Path) -> bool {
    let mut observed = Vec::new();
    for fill in ["zero", "pattern"] {
        let Some(output) = build_and_run(
            source,
            &work_dir.join(format!("ub-init-{fill}.bin")),
            &[&format!("-ftrivial-auto-var-init={fill}")],
        ) else {
            return false;
        };
        observed.push((output.status.code(), output.stdout));
    }
    observed[0] != observed[1]
}

/// Build `source` with gcc plus `flags` and run what comes out, or `None` where
/// gcc or the program itself did not get that far. Unoptimized on purpose:
/// what folds away at compile time is what the sanitizers never get to see.
fn build_and_run(source: &Path, binary: &Path, flags: &[&str]) -> Option<std::process::Output> {
    let mut compile = Command::new("gcc");
    compile
        .arg("-O0")
        .args(flags)
        .arg("-o")
        .arg(binary)
        .arg(source);
    let built = timed_output(&mut compile, COMPILE_TIMEOUT).ok()??;
    if !built.status.success() {
        return None;
    }
    timed_output(&mut Command::new(binary), RUN_TIMEOUT).ok()?
}

#[cfg(test)]
mod tests {
    use super::well_defined;

    fn check(name: &str, program: &str) -> bool {
        let dir = std::env::temp_dir().join(format!("fcc-fuzz-ub-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("case.c");
        std::fs::write(&source, program).unwrap();
        let verdict = well_defined(&source, &dir);
        std::fs::remove_dir_all(&dir).ok();
        verdict
    }

    #[test]
    fn a_program_that_only_computes_is_well_defined() {
        assert!(check(
            "clean",
            "#include <stdio.h>\n\
             int main(void) { printf(\"%d\\n\", 6 * 7); return 0; }\n",
        ));
    }

    #[test]
    fn signed_overflow_is_not_well_defined() {
        assert!(!check(
            "overflow",
            "#include <stdio.h>\n\
             int main(void) { int a = 83096; printf(\"%d\\n\", a * a); return 0; }\n",
        ));
    }

    #[test]
    fn reading_an_uninitialized_variable_is_not_well_defined() {
        assert!(!check(
            "uninitialized",
            "#include <stdio.h>\n\
             int main(void) { int late; printf(\"%d\\n\", late); return 0; }\n",
        ));
    }
}
