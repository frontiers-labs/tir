//! The FileCheck matching engine.
//!
//! Mirrors the semantics of LLVM's FileCheck for the directives the TIR test
//! suite uses. Matching is buffer-oriented: a cursor advances through the input
//! as directives are satisfied.

// A `Failure` carries the directive that failed (for diagnostics), so the
// `Err` variant is intentionally on the larger side; boxing it would only add
// indirection on the cold error path.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::ops::Range;

use crate::config::Config;
use crate::directive::{Directive, DirectiveKind};
use crate::pattern::{Pattern, PatternError};

/// A successful match: its byte range in the input plus any captured variables.
type MatchResult = (Range<usize>, Vec<(String, String)>);

/// A description of why matching failed.
#[derive(Debug)]
pub struct Failure {
    pub directive: Directive,
    pub kind: FailureKind,
}

#[derive(Debug)]
pub enum FailureKind {
    /// The pattern was not found in the remaining input.
    NotFound { search_from: usize },
    /// A `CHECK-NEXT`/`CHECK-SAME` pattern did not match the expected line.
    LineMismatch { region: Range<usize> },
    /// There was no following line for a `CHECK-NEXT`/`CHECK-EMPTY`.
    NoNextLine { at: usize },
    /// A `CHECK-NOT` pattern matched where it must not.
    NotMatched { at: Range<usize> },
    /// A `CHECK-EMPTY` directive found a non-empty line.
    ExpectedEmpty { at: Range<usize> },
    /// A `CHECK-COUNT-n` directive found the wrong number of matches.
    CountMismatch {
        found: usize,
        want: usize,
        at: usize,
    },
    /// The pattern failed to compile (e.g. an undefined variable).
    Compile(PatternError),
}

/// Run the directives against the input buffer.
pub fn run(buf: &str, directives: &[Directive], config: &Config) -> Result<(), Failure> {
    Matcher::new(buf, config).run(directives)
}

struct Matcher<'a> {
    buf: &'a str,
    config: &'a Config,
    vars: HashMap<String, String>,
    /// Byte offset where the next search begins (end of the last match).
    pos: usize,
    /// Accumulated `CHECK-NOT` directives awaiting the next positive match.
    pending_nots: Vec<Directive>,
}

impl<'a> Matcher<'a> {
    fn new(buf: &'a str, config: &'a Config) -> Self {
        Matcher {
            buf,
            config,
            vars: HashMap::new(),
            pos: 0,
            pending_nots: Vec::new(),
        }
    }

    fn run(&mut self, directives: &[Directive]) -> Result<(), Failure> {
        let mut i = 0;
        while i < directives.len() {
            let d = &directives[i];
            match &d.kind {
                DirectiveKind::Not => {
                    self.pending_nots.push(d.clone());
                    i += 1;
                }
                DirectiveKind::Dag => {
                    // Gather the whole consecutive DAG group.
                    let start = i;
                    while i < directives.len() && directives[i].kind == DirectiveKind::Dag {
                        i += 1;
                    }
                    self.match_dag(&directives[start..i])?;
                }
                DirectiveKind::Plain | DirectiveKind::Label => {
                    self.match_plain(d)?;
                    i += 1;
                }
                DirectiveKind::Count(n) => {
                    self.match_count(d, *n)?;
                    i += 1;
                }
                DirectiveKind::Next => {
                    self.match_next(d)?;
                    i += 1;
                }
                DirectiveKind::Same => {
                    self.match_same(d)?;
                    i += 1;
                }
                DirectiveKind::Empty => {
                    self.match_empty(d)?;
                    i += 1;
                }
            }
        }

        // Any remaining CHECK-NOT/implicit-not patterns must not match in the
        // rest of the input.
        self.check_nots(self.pos, self.buf.len())?;
        Ok(())
    }

    fn compile(&self, d: &Directive) -> Result<crate::pattern::CompiledPattern, Failure> {
        d.pattern
            .compile(&self.vars, self.config.strict_whitespace, false)
            .map_err(|e| Failure {
                directive: d.clone(),
                kind: FailureKind::Compile(e),
            })
    }

    fn search(
        &self,
        compiled: &crate::pattern::CompiledPattern,
        from: usize,
        to: usize,
    ) -> Option<MatchResult> {
        let slice = &self.buf[from..to];
        let caps = compiled.regex.captures(slice)?;
        let m = caps.get(0)?;
        let range = (from + m.start())..(from + m.end());
        let vars = compiled.extract_vars(&caps);
        Some((range, vars))
    }

    fn match_plain(&mut self, d: &Directive) -> Result<(), Failure> {
        let compiled = self.compile(d)?;
        match self.search(&compiled, self.pos, self.buf.len()) {
            Some((range, vars)) => {
                self.check_nots(self.pos, range.start)?;
                self.commit(range, vars);
                Ok(())
            }
            None => Err(Failure {
                directive: d.clone(),
                kind: FailureKind::NotFound {
                    search_from: self.pos,
                },
            }),
        }
    }

    fn match_count(&mut self, d: &Directive, want: usize) -> Result<(), Failure> {
        let compiled = self.compile(d)?;
        let mut found = 0;
        let mut first = true;
        while found < want {
            match self.search(&compiled, self.pos, self.buf.len()) {
                Some((range, vars)) => {
                    if first {
                        self.check_nots(self.pos, range.start)?;
                        first = false;
                    }
                    self.commit(range, vars);
                    found += 1;
                }
                None => {
                    return Err(Failure {
                        directive: d.clone(),
                        kind: FailureKind::CountMismatch {
                            found,
                            want,
                            at: self.pos,
                        },
                    });
                }
            }
        }
        Ok(())
    }

    fn match_next(&mut self, d: &Directive) -> Result<(), Failure> {
        let compiled = self.compile(d)?;
        let line_end = self.line_end(self.pos);
        if line_end >= self.buf.len() {
            return Err(Failure {
                directive: d.clone(),
                kind: FailureKind::NoNextLine { at: self.pos },
            });
        }
        let next_start = line_end + 1;
        let next_end = self.line_end(next_start);
        match self.search(&compiled, next_start, next_end) {
            Some((range, vars)) => {
                self.check_nots(self.pos, range.start)?;
                self.commit(range, vars);
                Ok(())
            }
            None => Err(Failure {
                directive: d.clone(),
                kind: FailureKind::LineMismatch {
                    region: next_start..next_end,
                },
            }),
        }
    }

    fn match_same(&mut self, d: &Directive) -> Result<(), Failure> {
        let compiled = self.compile(d)?;
        let line_end = self.line_end(self.pos);
        match self.search(&compiled, self.pos, line_end) {
            Some((range, vars)) => {
                self.check_nots(self.pos, range.start)?;
                self.commit(range, vars);
                Ok(())
            }
            None => Err(Failure {
                directive: d.clone(),
                kind: FailureKind::LineMismatch {
                    region: self.pos..line_end,
                },
            }),
        }
    }

    fn match_empty(&mut self, d: &Directive) -> Result<(), Failure> {
        let line_end = self.line_end(self.pos);
        if line_end >= self.buf.len() {
            return Err(Failure {
                directive: d.clone(),
                kind: FailureKind::NoNextLine { at: self.pos },
            });
        }
        let next_start = line_end + 1;
        let next_end = self.line_end(next_start);
        if next_start == next_end {
            self.check_nots(self.pos, next_start)?;
            self.pos = next_start;
            Ok(())
        } else {
            Err(Failure {
                directive: d.clone(),
                kind: FailureKind::ExpectedEmpty {
                    at: next_start..next_end,
                },
            })
        }
    }

    fn match_dag(&mut self, group: &[Directive]) -> Result<(), Failure> {
        let region_start = self.pos;
        let mut max_end = self.pos;
        let mut used: Vec<Range<usize>> = Vec::new();
        let mut earliest = self.buf.len();

        for d in group {
            let compiled = self.compile(d)?;
            // Find the first match in the region that does not overlap a
            // previously used range.
            let mut from = region_start;
            let found = loop {
                match self.search(&compiled, from, self.buf.len()) {
                    Some((range, vars)) => {
                        if used.iter().any(|u| ranges_overlap(u, &range)) {
                            from = range.start + 1;
                            continue;
                        }
                        break Some((range, vars));
                    }
                    None => break None,
                }
            };
            match found {
                Some((range, vars)) => {
                    earliest = earliest.min(range.start);
                    max_end = max_end.max(range.end);
                    used.push(range);
                    for (k, v) in vars {
                        self.vars.insert(k, v);
                    }
                }
                None => {
                    return Err(Failure {
                        directive: d.clone(),
                        kind: FailureKind::NotFound {
                            search_from: region_start,
                        },
                    });
                }
            }
        }

        self.check_nots(region_start, earliest)?;
        self.pos = max_end;
        Ok(())
    }

    fn check_nots(&mut self, from: usize, to: usize) -> Result<(), Failure> {
        if from > to {
            self.pending_nots.clear();
            return Ok(());
        }

        for d in &self.pending_nots {
            let compiled = d
                .pattern
                .compile(&self.vars, self.config.strict_whitespace, false)
                .map_err(|e| Failure {
                    directive: d.clone(),
                    kind: FailureKind::Compile(e),
                })?;
            if let Some(m) = compiled.regex.find(&self.buf[from..to]) {
                let at = (from + m.start())..(from + m.end());
                return Err(Failure {
                    directive: d.clone(),
                    kind: FailureKind::NotMatched { at },
                });
            }
        }

        for pat in &self.config.implicit_check_not {
            let pattern = Pattern::parse(pat).map_err(|e| Failure {
                directive: synthetic_not(pat),
                kind: FailureKind::Compile(e),
            })?;
            let compiled = pattern
                .compile(&self.vars, self.config.strict_whitespace, false)
                .map_err(|e| Failure {
                    directive: synthetic_not(pat),
                    kind: FailureKind::Compile(e),
                })?;
            if let Some(m) = compiled.regex.find(&self.buf[from..to]) {
                let at = (from + m.start())..(from + m.end());
                return Err(Failure {
                    directive: synthetic_not(pat),
                    kind: FailureKind::NotMatched { at },
                });
            }
        }

        self.pending_nots.clear();
        Ok(())
    }

    fn commit(&mut self, range: Range<usize>, vars: Vec<(String, String)>) {
        self.pos = range.end;
        for (k, v) in vars {
            self.vars.insert(k, v);
        }
    }

    fn line_end(&self, off: usize) -> usize {
        let off = off.min(self.buf.len());
        match self.buf[off..].find('\n') {
            Some(rel) => off + rel,
            None => self.buf.len(),
        }
    }
}

fn ranges_overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

fn synthetic_not(pat: &str) -> Directive {
    Directive {
        kind: DirectiveKind::Not,
        prefix: "implicit".to_string(),
        pattern: Pattern::parse(pat).unwrap_or(Pattern {
            segments: Vec::new(),
            raw: pat.to_string(),
        }),
        line: 0,
        pattern_span: 0..0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directive::scan;

    fn check(input: &str, directives: &str) -> Result<(), Failure> {
        let prefixes = vec!["CHECK".to_string()];
        let comments = vec!["RUN".to_string(), "COM".to_string()];
        let dirs = scan(directives, &prefixes, &comments).unwrap();
        run(input, &dirs, &Config::default())
    }

    #[test]
    fn plain_and_next() {
        let input = "alpha\nbeta\ngamma\n";
        assert!(check(input, "// CHECK: alpha\n// CHECK-NEXT: beta\n").is_ok());
        // gamma is not on the next line after alpha
        assert!(check(input, "// CHECK: alpha\n// CHECK-NEXT: gamma\n").is_err());
    }

    #[test]
    fn plain_searches_forward() {
        let input = "one\ntwo\nthree\n";
        assert!(check(input, "// CHECK: one\n// CHECK: three\n").is_ok());
        // out of order fails
        assert!(check(input, "// CHECK: three\n// CHECK: one\n").is_err());
    }

    #[test]
    fn check_not() {
        let input = "start\nmiddle\nend\n";
        assert!(check(input, "// CHECK: start\n// CHECK-NOT: zzz\n// CHECK: end\n").is_ok());
        assert!(check(
            input,
            "// CHECK: start\n// CHECK-NOT: middle\n// CHECK: end\n"
        )
        .is_err());
    }

    #[test]
    fn check_same() {
        let input = "foo bar baz\n";
        assert!(check(input, "// CHECK: foo\n// CHECK-SAME: baz\n").is_ok());
    }

    #[test]
    fn check_not_applies_before_next_same_and_empty() {
        assert!(check(
            "foo bad bar\n",
            "// CHECK: foo\n// CHECK-NOT: bad\n// CHECK-SAME: bar\n"
        )
        .is_err());

        assert!(check(
            "foo\nbad ok\n",
            "// CHECK: foo\n// CHECK-NOT: bad\n// CHECK-NEXT: ok\n"
        )
        .is_err());

        assert!(check(
            "foo bad\n\n",
            "// CHECK: foo\n// CHECK-NOT: bad\n// CHECK-EMPTY:\n"
        )
        .is_err());
    }

    #[test]
    fn check_empty() {
        let input = "foo\n\nbar\n";
        assert!(check(input, "// CHECK: foo\n// CHECK-EMPTY:\n").is_ok());
        let input2 = "foo\nbar\n";
        assert!(check(input2, "// CHECK: foo\n// CHECK-EMPTY:\n").is_err());
    }

    #[test]
    fn check_count() {
        let input = "x\nx\nx\n";
        assert!(check(input, "// CHECK-COUNT-3: x\n").is_ok());
        assert!(check(input, "// CHECK-COUNT-4: x\n").is_err());
    }

    #[test]
    fn check_dag() {
        let input = "b\na\nc\n";
        assert!(check(input, "// CHECK-DAG: a\n// CHECK-DAG: b\n").is_ok());
    }

    #[test]
    fn regex_and_vars() {
        let input = "result = 42\nuse 42\n";
        assert!(check(
            input,
            "// CHECK: result = [[VAL:[0-9]+]]\n// CHECK-NEXT: use [[VAL]]\n"
        )
        .is_ok());
        let input2 = "result = 42\nuse 7\n";
        assert!(check(
            input2,
            "// CHECK: result = [[VAL:[0-9]+]]\n// CHECK-NEXT: use [[VAL]]\n"
        )
        .is_err());
    }
}
