//! Discovery of check directives within a check file.

use crate::pattern::Pattern;
use std::ops::Range;

/// The kind of a check directive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectiveKind {
    /// `CHECK:` -- match somewhere after the previous match.
    Plain,
    /// `CHECK-NEXT:` -- match on the line immediately after the previous match.
    Next,
    /// `CHECK-SAME:` -- match on the same line as the previous match.
    Same,
    /// `CHECK-NOT:` -- must not match before the next positive directive.
    Not,
    /// `CHECK-EMPTY:` -- the next line must be empty.
    Empty,
    /// `CHECK-DAG:` -- match in any order within a region.
    Dag,
    /// `CHECK-LABEL:` -- a match that also acts as a region boundary.
    Label,
    /// `CHECK-COUNT-<n>:` -- match the pattern exactly `n` times.
    Count(usize),
}

impl DirectiveKind {
    fn from_suffix(suffix: &str) -> Option<DirectiveKind> {
        match suffix {
            "" => Some(DirectiveKind::Plain),
            "-NEXT" => Some(DirectiveKind::Next),
            "-SAME" => Some(DirectiveKind::Same),
            "-NOT" => Some(DirectiveKind::Not),
            "-EMPTY" => Some(DirectiveKind::Empty),
            "-DAG" => Some(DirectiveKind::Dag),
            "-LABEL" => Some(DirectiveKind::Label),
            other => other
                .strip_prefix("-COUNT-")
                .and_then(|n| n.parse::<usize>().ok())
                .map(DirectiveKind::Count),
        }
    }
}

/// A single directive parsed from the check file.
#[derive(Debug, Clone)]
pub struct Directive {
    pub kind: DirectiveKind,
    pub prefix: String,
    pub pattern: Pattern,
    /// 1-based line number of the directive in the check file.
    pub line: usize,
    /// Byte range of the pattern text within the check file (for diagnostics).
    pub pattern_span: Range<usize>,
}

/// Errors that can occur while scanning a check file.
#[derive(Debug, Clone)]
pub struct DirectiveError {
    pub message: String,
    pub line: usize,
    pub span: Range<usize>,
}

/// Scan `text` for check directives using the given prefixes.
///
/// `comment_prefixes` mark lines that are ignored entirely (e.g. `RUN:`).
pub fn scan(
    text: &str,
    check_prefixes: &[String],
    comment_prefixes: &[String],
) -> Result<Vec<Directive>, DirectiveError> {
    let mut directives = Vec::new();
    let mut offset = 0usize;

    for (idx, line) in text.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let trimmed_line = line.trim_end_matches(['\n', '\r']);

        // Skip comment directives (RUN:, COM: ...).
        if comment_prefixes
            .iter()
            .any(|p| find_directive(trimmed_line, p).is_some())
        {
            offset += line.len();
            continue;
        }

        for prefix in check_prefixes {
            if let Some((suffix_start, suffix, colon_end)) = find_directive(trimmed_line, prefix) {
                let suffix_kind = DirectiveKind::from_suffix(suffix);
                let Some(kind) = suffix_kind else {
                    continue;
                };

                let raw_pattern = &trimmed_line[colon_end..];
                // FileCheck strips leading/trailing whitespace from the pattern.
                let pattern_text = raw_pattern.trim();
                let lead = raw_pattern.len() - raw_pattern.trim_start().len();
                let pattern_start = offset + colon_end + lead;
                let pattern_span = pattern_start..(pattern_start + pattern_text.len());

                let pattern = Pattern::parse(pattern_text).map_err(|e| DirectiveError {
                    message: e.to_string(),
                    line: line_no,
                    span: pattern_span.clone(),
                })?;

                let _ = suffix_start;
                directives.push(Directive {
                    kind,
                    prefix: prefix.clone(),
                    pattern,
                    line: line_no,
                    pattern_span,
                });
                break;
            }
        }

        offset += line.len();
    }

    Ok(directives)
}

fn find_directive<'a>(line: &'a str, prefix: &str) -> Option<(usize, &'a str, usize)> {
    let mut search_from = 0;
    while let Some(rel) = line[search_from..].find(prefix) {
        let start = search_from + rel;
        let after = start + prefix.len();
        // The character before the prefix must not be alphanumeric, so we do
        // not match e.g. `XCHECK`.
        let preceded_ok = line[..start]
            .chars()
            .next_back()
            .map(|c| !c.is_alphanumeric() && c != '_')
            .unwrap_or(true);

        if preceded_ok {
            // Find the colon that terminates the directive, with only a valid
            // suffix in between.
            if let Some(colon_rel) = line[after..].find(':') {
                let suffix = &line[after..after + colon_rel];
                if is_valid_suffix(suffix) {
                    return Some((start, suffix, after + colon_rel + 1));
                }
            }
        }
        search_from = after;
    }
    None
}

fn is_valid_suffix(suffix: &str) -> bool {
    suffix.is_empty()
        || matches!(
            suffix,
            "-NEXT" | "-SAME" | "-NOT" | "-EMPTY" | "-DAG" | "-LABEL"
        )
        || suffix
            .strip_prefix("-COUNT-")
            .map(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefixes() -> Vec<String> {
        vec!["CHECK".to_string()]
    }

    fn comments() -> Vec<String> {
        vec!["COM".to_string(), "RUN".to_string()]
    }

    #[test]
    fn scans_basic_directives() {
        let text = "// RUN: foo\n// CHECK: hello\n// CHECK-NEXT: world\n";
        let dirs = scan(text, &prefixes(), &comments()).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].kind, DirectiveKind::Plain);
        assert_eq!(dirs[0].pattern.raw, "hello");
        assert_eq!(dirs[1].kind, DirectiveKind::Next);
        assert_eq!(dirs[1].line, 3);
    }

    #[test]
    fn ignores_run_lines() {
        let text = "// RUN: tmdlc --action=emit-tokens %s | filecheck %s\n// CHECK: x\n";
        let dirs = scan(text, &prefixes(), &comments()).unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].pattern.raw, "x");
    }

    #[test]
    fn parses_count_suffix() {
        let text = "// CHECK-COUNT-3: foo\n";
        let dirs = scan(text, &prefixes(), &comments()).unwrap();
        assert_eq!(dirs[0].kind, DirectiveKind::Count(3));
    }

    #[test]
    fn span_points_at_pattern() {
        let text = "// CHECK: hello\n";
        let dirs = scan(text, &prefixes(), &comments()).unwrap();
        let span = dirs[0].pattern_span.clone();
        assert_eq!(&text[span], "hello");
    }
}
