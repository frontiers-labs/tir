//! The bridge between the fuzzer and the issue tracker.
//!
//! Rendering lives here rather than in the reporting script so that the format
//! written into an issue and the format parsed back out of one cannot drift
//! apart. Replay is what lets an issue close itself: a defect that no longer
//! reproduces is fixed, whoever fixed it and whether or not they said so.

use std::path::{Path, PathBuf};

use super::harness::{FccVariant, Variant};
use super::report::Failure;
use super::triage;

/// Print a recorded failure as the issue it becomes: title on the first line,
/// body on the rest.
pub fn render(file: &Path) -> anyhow::Result<()> {
    let failure: Failure = serde_json::from_str(&std::fs::read_to_string(file)?)?;
    println!("{}", failure.title());
    print!("{}", failure.body());
    Ok(())
}

/// Recover the defect records embedded in open issues, read as the JSON array
/// `gh issue list --json body` produces, and write one file per record into
/// `dir`. Issues without a record — anything a human filed — are skipped.
pub fn extract(dir: &Path) -> anyhow::Result<()> {
    #[derive(serde::Deserialize)]
    struct Issue {
        body: String,
    }

    std::fs::create_dir_all(dir)?;
    let issues: Vec<Issue> = serde_json::from_reader(std::io::stdin().lock())?;
    let mut found = 0;
    for issue in &issues {
        let Some(failure) = Failure::from_body(&issue.body) else {
            continue;
        };
        found += 1;
        std::fs::write(
            dir.join(format!("{}.json", failure.signature())),
            serde_json::to_string(&failure)?,
        )?;
    }
    println!("fcc-fuzz: {found} tracked defects to replay");
    Ok(())
}

/// Re-run every recorded defect in `dir` and list the signatures that no longer
/// reproduce, one per line, in `fixed`.
pub fn replay(
    fcc: &Path,
    dir: &Path,
    fixed: &Path,
    references: &[Variant],
    work_dir: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(work_dir)?;
    let mut repaired = Vec::new();
    for file in records(dir)? {
        let failure: Failure = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
        // Only a C program can be replayed by compiling it. A raw fuzzer input
        // is replayed by the job that knows how to feed it back in, and is
        // left alone here rather than being declared fixed by default.
        if failure.language != "c" {
            continue;
        }
        let still = triage::diverges(
            fcc,
            &failure.artifact,
            &variant_of(&failure),
            references,
            work_dir,
        );
        let signature = failure.signature();
        if still {
            println!("STILL {signature}: {}", failure.summary);
        } else {
            println!("FIXED {signature}: {}", failure.summary);
            repaired.push(signature);
        }
    }
    std::fs::write(fixed, repaired.join("\n"))?;
    Ok(())
}

/// The variant a record was filed against, recovered from its identity, whose
/// first line is the variant's tag or `fcc-default`.
fn variant_of(failure: &Failure) -> FccVariant {
    match failure.identity.lines().next() {
        Some(first) if first != "fcc-default" => FccVariant::from_tag(first),
        _ => FccVariant::default(),
    }
}

fn records(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    Ok(files)
}
