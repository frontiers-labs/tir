//! A standalone, LLVM-FileCheck-compatible pattern matcher.
//!
//! This crate provides a small reimplementation of the subset of LLVM's
//! [FileCheck](https://llvm.org/docs/CommandGuide/FileCheck.html) tool used by
//! the TIR test suite. It is built on [`chumsky`] (for parsing check patterns)
//! and [`ariadne`] (for rendering rich diagnostics), so the project does not
//! depend on external FileCheck implementations.
//!
//! The directive set supported is `CHECK`, `CHECK-NEXT`, `CHECK-SAME`,
//! `CHECK-NOT`, `CHECK-EMPTY`, `CHECK-DAG`, `CHECK-LABEL` and `CHECK-COUNT-<n>`,
//! together with `{{regex}}` blocks and `[[VAR:regex]]` / `[[VAR]]` variables.
//!
//! Use [`verify`] to run a check file against some input; on failure it returns
//! a fully rendered, human-readable diagnostic.

pub mod config;
pub mod directive;
pub mod matcher;
pub mod pattern;

pub use config::Config;

use ariadne::{sources, Color, Config as AriadneConfig, IndexType, Label, Report, ReportKind};
use matcher::{Failure, FailureKind};

/// An optional label pointing into the input file: a byte range and a message.
type InputLabel = Option<(std::ops::Range<usize>, String)>;

/// A named piece of text (the check file or the input).
#[derive(Debug, Clone)]
pub struct Source {
    pub name: String,
    pub text: String,
}

impl Source {
    pub fn new(name: impl Into<String>, text: impl Into<String>) -> Self {
        Source {
            name: name.into(),
            text: text.into(),
        }
    }
}

/// Verify `input` against the directives found in `check`.
///
/// On success returns `Ok(())`. On failure returns a rendered, multi-line
/// diagnostic suitable for printing to a terminal or embedding in a test
/// failure message.
pub fn verify(check: &Source, input: &Source, config: &Config) -> Result<(), String> {
    let check_prefixes = config.effective_check_prefixes();
    let comment_prefixes = config.effective_comment_prefixes();

    let directives = match directive::scan(&check.text, &check_prefixes, &comment_prefixes) {
        Ok(d) => d,
        Err(e) => return Err(render_directive_error(check, &e)),
    };

    if directives.is_empty() {
        return Err(format!(
            "filecheck: no check directives found in '{}' (prefixes: {})",
            check.name,
            check_prefixes.join(", ")
        ));
    }

    if input.text.trim().is_empty() && !config.allow_empty {
        return Err(format!(
            "filecheck: input '{}' is empty (use --allow-empty to permit this)",
            input.name
        ));
    }

    match matcher::run(&input.text, &directives, config) {
        Ok(()) => Ok(()),
        Err(failure) => Err(render_failure(check, input, &failure)),
    }
}

fn render_directive_error(check: &Source, err: &directive::DirectiveError) -> String {
    let id = check.name.clone();
    let mut buf = Vec::new();
    Report::build(ReportKind::Error, (id.clone(), err.span.clone()))
        .with_config(diag_config())
        .with_message("invalid check directive")
        .with_label(
            Label::new((id.clone(), err.span.clone()))
                .with_message(err.message.clone())
                .with_color(Color::Red),
        )
        .finish()
        .write(sources([(id, check.text.clone())]), &mut buf)
        .ok();
    String::from_utf8_lossy(&buf).into_owned()
}

fn render_failure(check: &Source, input: &Source, failure: &Failure) -> String {
    let cid = check.name.clone();
    let iid = input.name.clone();
    let d = &failure.directive;

    let directive_name = directive_label(d);
    let check_span = (cid.clone(), d.pattern_span.clone());

    // The primary span and the human-readable summary depend on the failure
    // kind. The check-file label is always shown; an input-file label is added
    // where it makes sense.
    let (summary, check_msg, input_label): (String, String, InputLabel) = match &failure.kind {
        FailureKind::NotFound { search_from } => (
            format!("{directive_name}: pattern not found"),
            "this pattern was not found in the input".to_string(),
            Some((
                point(input, *search_from),
                "searched from here to end of input".to_string(),
            )),
        ),
        FailureKind::LineMismatch { region } => (
            format!("{directive_name}: no match on the expected line"),
            "this pattern did not match".to_string(),
            Some((region.clone(), "expected a match on this line".to_string())),
        ),
        FailureKind::NoNextLine { at } => (
            format!("{directive_name}: expected another line, but input ended"),
            "this directive requires a following line".to_string(),
            Some((point(input, *at), "input ends here".to_string())),
        ),
        FailureKind::NotMatched { at } => (
            format!("{directive_name}: excluded pattern matched"),
            "this pattern must not appear here".to_string(),
            Some((at.clone(), "but it matched here".to_string())),
        ),
        FailureKind::ExpectedEmpty { at } => (
            format!("{directive_name}: expected an empty line"),
            "this directive requires the next line to be empty".to_string(),
            Some((at.clone(), "this line is not empty".to_string())),
        ),
        FailureKind::CountMismatch { found, want, at } => (
            format!("{directive_name}: expected {want} matches, found {found}"),
            format!("expected this pattern {want} times"),
            Some((point(input, *at), "ran out of matches here".to_string())),
        ),
        FailureKind::Compile(err) => (format!("{directive_name}: {err}"), err.to_string(), None),
    };

    let mut report = Report::build(ReportKind::Error, check_span.clone())
        .with_config(diag_config())
        .with_message(summary)
        .with_label(
            Label::new(check_span)
                .with_message(check_msg)
                .with_color(Color::Red),
        );

    if let Some((range, msg)) = input_label {
        report = report.with_label(
            Label::new((iid.clone(), range))
                .with_message(msg)
                .with_color(Color::Yellow),
        );
    }

    let mut buf = Vec::new();
    report
        .finish()
        .write(
            sources([(cid, check.text.clone()), (iid, input.text.clone())]),
            &mut buf,
        )
        .ok();
    String::from_utf8_lossy(&buf).into_owned()
}

fn directive_label(d: &directive::Directive) -> String {
    use directive::DirectiveKind::*;
    let suffix = match &d.kind {
        Plain => String::new(),
        Next => "-NEXT".to_string(),
        Same => "-SAME".to_string(),
        Not => "-NOT".to_string(),
        Empty => "-EMPTY".to_string(),
        Dag => "-DAG".to_string(),
        Label => "-LABEL".to_string(),
        Count(n) => format!("-COUNT-{n}"),
    };
    format!("{}{}", d.prefix, suffix)
}

fn point(input: &Source, offset: usize) -> std::ops::Range<usize> {
    let len = input.text.len();
    let start = offset.min(len);
    let end = (start + 1).min(len);
    if start == end && start > 0 {
        (start - 1)..start
    } else {
        start..end
    }
}

fn diag_config() -> AriadneConfig {
    AriadneConfig::new()
        .with_index_type(IndexType::Byte)
        .with_color(false)
}
