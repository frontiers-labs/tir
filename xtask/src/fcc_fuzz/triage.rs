//! Turning a raw divergence into something worth filing: shrink the pipeline to
//! the passes that still miscompile, then shrink the program to the statements
//! that still expose it. What comes out is stable across seeds, which is what
//! lets one issue track one defect.

use std::path::Path;

use super::harness::{self, Behavior, FccVariant, Outcome, Variant};
use super::reduce::{self, STRUCTURAL_PASSES};
use super::report::Failure;
use super::ub;

/// Ceiling on harness runs spent shrinking one failure. Reduction is worth
/// minutes, not hours: past the budget the predicate reports failure, deletions
/// stop, and what has been shrunk so far is still filed.
const BUDGET: usize = 400;

pub struct Reduced {
    /// What the issue shows a human: the minimal program, or — for a curated
    /// corpus case, which is not shrunk — the file's source.
    pub artifact: String,
    /// What makes this the same defect when it turns up again. The minimal
    /// program for a generated case, the path for a corpus one. Never a seed.
    pub subject: String,
    /// The smallest variant that still miscompiles: the shortest pipeline, and
    /// whichever oracles were on. The default when fcc's own output disagrees
    /// with a reference compiler, where there is nothing to blame.
    pub variant: FccVariant,
    /// How many lines the case had before shrinking, where it was shrunk.
    pub shrunk_from: Option<usize>,
}

impl Reduced {
    pub fn identity(&self) -> String {
        format!(
            "{}\n{}",
            self.variant.tag().as_deref().unwrap_or("fcc-default"),
            self.subject
        )
    }

    /// What is left after bisection: the passes to go and look at, and the
    /// oracle that exposed them.
    pub fn culprit(&self) -> String {
        let mut named: Vec<&str> = self
            .variant
            .pipeline
            .as_deref()
            .and_then(|pipeline| pipeline.split_once('('))
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .map(|inner| {
                inner
                    .split(',')
                    .filter(|pass| !STRUCTURAL_PASSES.contains(pass))
                    .collect()
            })
            .unwrap_or_default();
        if self.variant.shuffle_machine_order {
            named.push("machine-order shuffling");
        }
        if !named.is_empty() {
            return named.join(" + ");
        }
        match self.variant.pipeline {
            Some(_) => "pass scheduling alone".to_string(),
            None => "fcc's default pipeline".to_string(),
        }
    }
}

/// Shrink `source` and `pipeline` to the smallest pair that still diverges.
/// `work_dir` must be private to this call; the harness names its artifacts
/// after the source file and would otherwise collide with a concurrent triage.
pub fn triage(
    fcc: &Path,
    source: &str,
    variant: &FccVariant,
    references: &[Variant],
    work_dir: &Path,
) -> Reduced {
    let original_lines = source.lines().count();
    let mut spent = 0;
    let mut still_fails = |source: &str, variant: &FccVariant| {
        if spent >= BUDGET {
            return false;
        }
        spent += 1;
        diverges(fcc, source, variant, references, work_dir)
    };

    // Shrink the pipeline first: it costs a handful of runs, and every pass it
    // drops makes each of the many reduction runs cheaper. An oracle is one
    // switch and has nothing to shrink.
    let variant = bisect(variant, &mut |candidate| still_fails(source, candidate));
    let source = reduce::reduce(source, &mut |candidate| still_fails(candidate, &variant));

    Reduced {
        shrunk_from: Some(original_lines),
        subject: source.clone(),
        artifact: source,
        variant,
    }
}

/// Shrink the pipeline alone, for cases whose program is curated and must not
/// be rewritten.
pub fn bisect(
    variant: &FccVariant,
    still_diverges: &mut dyn FnMut(&FccVariant) -> bool,
) -> FccVariant {
    let Some(pipeline) = variant.pipeline.as_deref() else {
        return variant.clone();
    };
    let mut candidate = variant.clone();
    let shortest = reduce::bisect_pipeline(pipeline, &mut |pipeline| {
        candidate.pipeline = Some(pipeline.to_string());
        still_diverges(&candidate)
    });
    FccVariant {
        pipeline: Some(shortest),
        ..variant.clone()
    }
}

/// Does this candidate still expose the defect? A divergence only counts on a
/// program the standard pins down, so one is put to `ub::well_defined` before
/// it is believed — and to the cheaper check that the reference compilers
/// agree with each other, which the oracles cannot all replace.
pub fn diverges(
    fcc: &Path,
    source: &str,
    variant: &FccVariant,
    references: &[Variant],
    work_dir: &Path,
) -> bool {
    let path = work_dir.join("candidate.c");
    if std::fs::write(&path, source).is_err() {
        return false;
    }

    let mut variants = vec![Variant::fcc(FccVariant::default())];
    if variant.tag().is_some() {
        variants.push(Variant::fcc(variant.clone()));
    }
    variants.extend(references.iter().cloned());

    let outcomes = harness::run_variants(fcc, &path, &variants, work_dir);
    if references_disagree(&outcomes) {
        return false;
    }
    let diverged = outcomes
        .iter()
        .any(|(_, outcome)| matches!(outcome, Outcome::Diverged { .. }));
    // Asked last: it costs three more builds, and most candidates the reducer
    // offers do not diverge at all.
    diverged && ub::well_defined(&path, work_dir)
}

/// Whether two reference compilers produced different behavior. Both are
/// compared against fcc's default, so agreeing with it — or diverging from it
/// identically — means they agree with each other.
fn references_disagree(outcomes: &[(String, Outcome)]) -> bool {
    let observed: Vec<Option<&Behavior>> = outcomes
        .iter()
        .filter(|(name, _)| name == "gcc" || name == "clang")
        .filter_map(|(_, outcome)| match outcome {
            Outcome::Agree => Some(None),
            Outcome::Diverged { actual, .. } => Some(Some(actual)),
            // A reference that would not build says nothing either way.
            Outcome::Errored { .. } => None,
        })
        .collect();
    observed.windows(2).any(|pair| pair[0] != pair[1])
}

/// Build the record to file for a divergence that has already been shrunk.
pub fn failure(
    job: &str,
    summary: String,
    reproduce: String,
    reduced: &Reduced,
    variant: &str,
    expected: &Behavior,
    actual: &Behavior,
) -> Failure {
    let difference =
        harness::first_difference(expected, actual).unwrap_or_else(|| "outputs differ".to_string());
    let details = format!(
        "`{variant}` disagrees with `fcc-default` on the case below.\n\
         \n\
         - First difference: {difference}\n\
         - Expected: `{}`\n\
         - Got: `{}`\n\
         - Narrowed to: **{}**{}",
        expected.describe(),
        actual.describe(),
        reduced.culprit(),
        match reduced.shrunk_from {
            Some(before) => format!(
                "\n- Shrunk from {before} lines to {}",
                reduced.artifact.lines().count()
            ),
            None => String::new(),
        },
    );
    Failure {
        job: job.to_string(),
        summary,
        identity: reduced.identity(),
        reproduce,
        details,
        artifact: reduced.artifact.clone(),
        language: "c".to_string(),
    }
}
