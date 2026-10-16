//! Shrinking a fuzz failure to something a human can act on: delete statements
//! from the program while it still fails, and drop passes from the pipeline
//! while it still diverges. What survives both is the report's identity, so the
//! same defect found from two different seeds lands on one issue.

/// Delete statements from `source` for as long as `still_fails` holds of the
/// result. A line opening a block is deleted together with the block it opens,
/// so every candidate stays syntactically whole, and a `return` is never
/// deleted, so a caller never starts reading an indeterminate value.
pub fn reduce(source: &str, still_fails: &mut dyn FnMut(&str) -> bool) -> String {
    let mut lines: Vec<&str> = source.lines().collect();
    // Deleting a statement can make an earlier one deletable in turn — the last
    // use of a variable going away frees its declaration — so sweep until a
    // whole pass finds nothing. Sweeps are linear; restarting after every
    // deletion would make this quadratic in harness runs, which CI cannot pay.
    for _ in 0..MAX_SWEEPS {
        let mut deleted = false;
        let mut start = 0;
        while start < lines.len() {
            if !deletable(lines[start]) {
                start += 1;
                continue;
            }
            let end = chunk_end(&lines, start);
            let mut candidate = lines.clone();
            candidate.drain(start..=end);
            if still_fails(&join(&candidate)) {
                lines = candidate;
                deleted = true;
            } else {
                start += 1;
            }
        }
        if !deleted {
            break;
        }
    }
    join(&lines)
}

/// How many times to sweep the program before settling for what is left.
const MAX_SWEEPS: usize = 4;

fn deletable(line: &str) -> bool {
    let line = line.trim_start();
    !line.starts_with('}') && !line.starts_with("return")
}

fn chunk_end(lines: &[&str], start: usize) -> usize {
    if !lines[start].trim_end().ends_with('{') {
        return start;
    }
    let mut depth = 0i32;
    for (offset, line) in lines[start..].iter().enumerate() {
        depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
        if depth <= 0 {
            return start + offset;
        }
    }
    lines.len() - 1
}

fn join(lines: &[&str]) -> String {
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// Drop passes from `pipeline` for as long as `still_diverges` holds of the
/// result. The checks bracketing every pipeline are structural rather than
/// optimizations, so they are never dropped; what is left is the shortest pass
/// sequence that still miscompiles.
pub fn bisect_pipeline(pipeline: &str, still_diverges: &mut dyn FnMut(&str) -> bool) -> String {
    let mut current = pipeline.to_string();
    let mut kept = 0;
    loop {
        let Some(&(start, end)) = pipeline_items(&current).get(kept) else {
            return current;
        };
        if STRUCTURAL_PASSES.contains(&&current[start..end]) {
            kept += 1;
            continue;
        }
        let candidate = without_item(&current, start, end);
        if still_diverges(&candidate) {
            current = candidate;
        } else {
            kept += 1;
        }
    }
}

/// The check every pipeline runs under: it rewrites nothing, so dropping it
/// makes the reproduction weaker rather than smaller.
pub const STRUCTURAL_PASSES: [&str; 1] = ["verify-deps"];

fn pipeline_items(pipeline: &str) -> Vec<(usize, usize)> {
    let mut items = Vec::new();
    let mut enclosing = Vec::new();
    let mut start = 0;
    let push = |items: &mut Vec<(usize, usize)>, start: usize, end: usize| {
        if start < end {
            items.push((start, end));
        }
    };
    for (index, byte) in pipeline.bytes().enumerate() {
        match byte {
            b'(' => {
                enclosing.push(start);
                start = index + 1;
            }
            b')' => {
                push(&mut items, start, index);
                start = enclosing.pop().unwrap_or(index + 1);
            }
            b',' => {
                push(&mut items, start, index);
                start = index + 1;
            }
            _ => {}
        }
    }
    push(&mut items, start, pipeline.len());
    items
}

fn without_item(pipeline: &str, start: usize, end: usize) -> String {
    let (from, to) = if pipeline[..start].ends_with(',') {
        (start - 1, end)
    } else if pipeline[end..].starts_with(',') {
        (start, end + 1)
    } else {
        (start, end)
    };
    format!("{}{}", &pipeline[..from], &pipeline[to..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_every_statement_the_failure_does_not_need() {
        let source = "int main(void) {\n\
                      \x20   int a = 1;\n\
                      \x20   if (a) {\n\
                      \x20       int b = 2;\n\
                      \x20       MARKER;\n\
                      \x20   }\n\
                      \x20   int c = 3;\n\
                      \x20   return 0;\n\
                      }\n";

        let reduced = reduce(source, &mut |candidate| candidate.contains("MARKER"));

        assert_eq!(
            reduced,
            "int main(void) {\n\
             \x20   if (a) {\n\
             \x20       MARKER;\n\
             \x20   }\n\
             \x20   return 0;\n\
             }\n"
        );
    }

    #[test]
    fn keeps_the_return_a_caller_reads() {
        let source = "int f(void) {\n\
                      \x20   int a = 1;\n\
                      \x20   return a;\n\
                      }\n";

        let reduced = reduce(source, &mut |candidate| candidate.contains("int f(void)"));

        assert_eq!(
            reduced,
            "int f(void) {\n\
             \x20   return a;\n\
             }\n"
        );
    }

    #[test]
    fn drops_every_pass_the_divergence_does_not_need() {
        let pipeline =
            "func.func(verify-deps,instcombine-nodes,promote-nodes,affine,instcombine-nodes)";

        let minimal = bisect_pipeline(pipeline, &mut |candidate| candidate.contains("promote"));

        assert_eq!(minimal, "func.func(verify-deps,promote-nodes)");
    }

    #[test]
    fn drops_passes_nested_under_another_pipeline() {
        let pipeline = "func.func(promote-nodes),fixpoint<3>(func.func(instcombine-nodes,affine))";

        let minimal = bisect_pipeline(pipeline, &mut |candidate| candidate.contains("affine"));

        assert_eq!(minimal, "fixpoint<3>(func.func(affine))");
    }
}
