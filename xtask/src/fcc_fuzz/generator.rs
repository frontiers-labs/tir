//! Seeded generator of UB-free C programs for the differential fuzzer.
//!
//! Every program is a pure function of its seed: same seed, same bytes. The
//! generated code avoids undefined behavior by construction — integer bounds
//! are tracked through every expression so no signed operation can overflow,
//! divisors are nonzero by shape, left shifts are of a masked non-negative
//! value by an amount its bound allows, and array indices are masked to the
//! array length.
//!
//! The bound is a guarantee, not a wish: `expr(scope, b, _)` never returns an
//! expression that can exceed `b`. That is what a caller may rely on, and it
//! is why a leaf turns down a variable too wide for the request rather than
//! handing it over with its own bound. Weaken it anywhere and the property
//! stops holding everywhere downstream — an argument wider than a parameter's
//! bound breaks the callee's proof, whose return then breaks its caller's.
//!
//! One bound serves everything, because a value only reaches an expression if
//! it fits: staggered bounds would leave whole classes of value with nowhere
//! to go — an array element too wide to be an argument, a call result too wide
//! to be compared — and a fuzzer that cannot connect a load to a call is worth
//! less than the tidiness costs.
//!
//! Memory is exercised through pointers: every function takes two pointers
//! into the caller's arrays and owns an array of its own, reads and writes go
//! through indexing or pointer arithmetic, and a function may hand any of them
//! on to an earlier function. The two arguments are slices of one array as
//! often as not, at any shift the array allows, so a callee sees every
//! relation a no-alias fact has to tell apart: the same range, a partial
//! overlap, and disjoint ranges.

const INT_MAX: i64 = 2_147_483_647;

/// Bound on every value a generated program names: an argument, a return, an
/// array element, a local, and every intermediate on the way to one. A store
/// through a pointer respects it too, so a callee cannot break its caller's.
const VALUE_BOUND: i64 = 10_000;
/// Array length used throughout.
const ARRAY_LEN: usize = 8;
/// How far into an array a pointer passed to a function may start, so that the
/// callee's masked indices stay inside the array. At the far end the range the
/// callee reaches is disjoint from the one at the near end.
const POINTER_SHIFT: i64 = 4;
/// Mask on indices through a passed pointer: `POINTER_SHIFT + 3 < ARRAY_LEN`.
const POINTER_MASK: usize = 3;

/// The widest value any generated program computes is `main`'s accumulator,
/// an array element plus a step, each within `VALUE_BOUND`. Everything else is
/// one bounded value, so no generated arithmetic can overflow.
const _: () = assert!(2 * VALUE_BOUND < INT_MAX);

pub fn generate(seed: u64) -> String {
    let mut generator = Generator::new(seed);
    generator.program()
}

struct Generator {
    rng: Rng,
    functions: Vec<Function>,
    next_id: u32,
}

struct Function {
    name: String,
    body: String,
}

/// A declared variable: its name, the bound on the value it currently holds,
/// and whether assignment to it preserves termination (loop counters are
/// read-only, or a body could reset the counter forever).
#[derive(Clone)]
struct Variable {
    name: String,
    bound: i64,
    assignable: bool,
}

/// A generated expression and the maximum magnitude it can produce. Every
/// operation's output bound is derived from its operands', so a bound of B
/// proves |value| <= B and no overflow occurs.
struct Expr {
    text: String,
    bound: i64,
}

impl Expr {
    fn constant(value: i64) -> Self {
        Self {
            text: value.to_string(),
            bound: value.abs(),
        }
    }
}

/// An array in scope: its name and the mask that keeps an index inside it.
struct Array {
    name: String,
    mask: usize,
}

/// Variables and arrays in scope while generating one function body.
struct Scope {
    variables: Vec<Variable>,
    arrays: Vec<Array>,
}

impl Scope {
    fn new() -> Self {
        Self {
            variables: Vec::new(),
            arrays: Vec::new(),
        }
    }

    fn push(&mut self, name: String, bound: i64) {
        self.variables.push(Variable {
            name,
            bound,
            assignable: true,
        });
    }

    fn push_read_only(&mut self, name: &str, bound: i64) {
        self.variables.push(Variable {
            name: name.to_string(),
            bound,
            assignable: false,
        });
    }

    /// Truncate back to a snapshot taken before entering a nested block.
    fn restore(&mut self, mark: usize) {
        self.variables.truncate(mark);
    }

    fn pick(&self, rng: &mut Rng) -> Option<&Variable> {
        self.variables
            .get(rng.below(self.variables.len() as u64) as usize)
    }

    /// A variable narrow enough to stand in for an expression bounded by
    /// `bound`. Wider ones are not masked down to fit: that would silently
    /// change the value the program computes, and the point of the bound is to
    /// describe what the program does, not to constrain it after the fact.
    fn pick_within(&self, rng: &mut Rng, bound: i64) -> Option<&Variable> {
        let candidates: Vec<&Variable> =
            self.variables.iter().filter(|v| v.bound <= bound).collect();
        candidates
            .get(rng.below(candidates.len() as u64) as usize)
            .copied()
    }

    fn pick_array(&self, rng: &mut Rng) -> Option<&Array> {
        self.arrays
            .get(rng.below(self.arrays.len() as u64) as usize)
    }

    fn pick_assignable(&self, rng: &mut Rng) -> Option<usize> {
        let candidates: Vec<usize> = self
            .variables
            .iter()
            .enumerate()
            .filter(|(_, v)| v.assignable)
            .map(|(index, _)| index)
            .collect();
        if candidates.is_empty() {
            None
        } else {
            Some(candidates[rng.below(candidates.len() as u64) as usize])
        }
    }
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
            functions: Vec::new(),
            next_id: 0,
        }
    }

    fn fresh(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    fn program(&mut self) -> String {
        let count = 1 + self.rng.below(2) as usize;
        for index in 0..count {
            let name = format!("f{index}");
            let body = self.function_body();
            self.functions.push(Function { name, body });
        }

        let mut out = String::from("#include <stdio.h>\n\n");
        for function in &self.functions {
            let name = &function.name;
            out.push_str(&format!("int {name}(int *p, int *q, int a, int b);\n"));
        }
        out.push('\n');
        for function in &self.functions {
            out.push_str(&function.body);
        }
        out.push_str(&self.main_body());
        out
    }

    /// A function of two pointers and two integers returning a bounded value.
    /// It owns an array of its own and may call earlier functions, handing them
    /// any array in scope.
    fn function_body(&mut self) -> String {
        let mut scope = Scope::new();
        scope.push("a".into(), VALUE_BOUND);
        scope.push("b".into(), VALUE_BOUND);
        for name in ["p", "q"] {
            scope.arrays.push(Array {
                name: name.into(),
                mask: POINTER_MASK,
            });
        }
        let mut body = self.array_declaration("loc");
        scope.arrays.push(Array {
            name: "loc".into(),
            mask: ARRAY_LEN - 1,
        });
        self.statements(&mut scope, &mut body, 0);

        let result = self.expr(&scope, VALUE_BOUND, 0);
        let name = format!("f{}", self.functions.len());
        format!(
            "int {name}(int *p, int *q, int a, int b) {{\n{body}    return {};\n}}\n\n",
            result.text
        )
    }

    fn array_declaration(&mut self, name: &str) -> String {
        let elements: Vec<String> = (0..ARRAY_LEN)
            .map(|_| self.rng.range(-1000, 1000).to_string())
            .collect();
        format!(
            "    int {name}[{ARRAY_LEN}] = {{{}}};\n",
            elements.join(", ")
        )
    }

    /// A call to `callee`, passing two of the arrays in scope and two bounded
    /// integers.
    fn call(&mut self, scope: &Scope, callee: &str) -> String {
        let first = self.pointer_argument(scope);
        let second = self.pointer_argument(scope);
        let args: Vec<String> = (0..2)
            .map(|_| self.expr(scope, VALUE_BOUND, 1).text)
            .collect();
        format!("{callee}({first}, {second}, {})", args.join(", "))
    }

    /// One pointer argument: an array in scope, shifted along where it is the
    /// function's own. An array the caller was handed is passed as it stands —
    /// shifting it further would take the callee past the end of the array it
    /// came from.
    fn pointer_argument(&mut self, scope: &Scope) -> String {
        match scope.pick_array(&mut self.rng) {
            Some(array) if array.mask == POINTER_MASK => array.name.clone(),
            Some(array) => format!("{} + {}", array.name, self.rng.range(0, POINTER_SHIFT)),
            None => "arr".to_string(),
        }
    }

    fn main_body(&mut self) -> String {
        let mut scope = Scope::new();
        let mut body = self.array_declaration("arr");
        scope.arrays.push(Array {
            name: "arr".into(),
            mask: ARRAY_LEN - 1,
        });

        let names: Vec<String> = self.functions.iter().map(|f| f.name.clone()).collect();
        for name in &names {
            let result = self.fresh("r");
            let call = self.call(&scope, name);
            body.push_str(&format!("    int {result} = {call};\n"));
            body.push_str(&format!("    printf(\"%d\\n\", {result});\n"));
            scope.push(result, VALUE_BOUND);
        }

        // A loop accumulating over the array, printed per trip so a divergence
        // localizes to a trip count.
        let trips = 3 + self.rng.below(6) as i64;
        let acc = self.fresh("s");
        body.push_str(&format!("    int {acc} = 0;\n"));
        body.push_str(&format!("    for (int i = 0; i < {trips}; i++) {{\n"));
        scope.push_read_only("i", trips.saturating_sub(1));
        let step = self.expr(&scope, VALUE_BOUND, 1);
        body.push_str(&format!(
            "        {acc} = {} + arr[i & {}];\n",
            step.text,
            ARRAY_LEN - 1
        ));
        body.push_str(&format!("        printf(\"%d\\n\", {acc});\n"));
        body.push_str("    }\n");
        // Every element is printed: a store through a pointer that went wrong
        // shows up even where the loop above never read it.
        body.push_str(&format!(
            "    for (int i = 0; i < {ARRAY_LEN}; i++) printf(\"%d\\n\", arr[i]);\n"
        ));

        format!("int main(void) {{\n{body}    return 0;\n}}\n")
    }

    fn statements(&mut self, scope: &mut Scope, body: &mut String, depth: u32) {
        let count = 1 + self.rng.below(3);
        for _ in 0..count {
            match self.rng.below(if depth < 1 { 6 } else { 3 }) {
                0 => {
                    let expr = self.expr(scope, VALUE_BOUND, depth + 1);
                    let name = self.fresh("v");
                    body.push_str(&format!("    int {name} = {};\n", expr.text));
                    scope.push(name, expr.bound);
                }
                5 if !self.functions.is_empty() => {
                    let callee = self.functions
                        [self.rng.below(self.functions.len() as u64) as usize]
                        .name
                        .clone();
                    let call = self.call(scope, &callee);
                    let name = self.fresh("v");
                    body.push_str(&format!("    int {name} = {call};\n"));
                    scope.push(name, VALUE_BOUND);
                }
                1 => {
                    let Some(index) = scope.pick_assignable(&mut self.rng) else {
                        continue;
                    };
                    let previous = scope.variables[index].bound;
                    let expr = self.expr(scope, previous.max(2), depth + 1);
                    // The assignment may sit in a branch that is never taken,
                    // so afterwards the variable holds either value.
                    scope.variables[index].bound = previous.max(expr.bound);
                    let name = scope.variables[index].name.clone();
                    body.push_str(&format!("    {name} = {};\n", expr.text));
                }
                2 => {
                    let expr = self.expr(scope, VALUE_BOUND, depth + 1);
                    let element = self.element(scope);
                    body.push_str(&format!("    {element} = {};\n", expr.text));
                }
                3 => {
                    let cond = self.comparison(scope, depth + 1);
                    body.push_str(&format!("    if ({}) {{\n", cond.text));
                    let mark = scope.variables.len();
                    let mut inner = String::new();
                    self.statements(scope, &mut inner, depth + 1);
                    body.push_str(&indent(&inner));
                    // The else arm must not see declarations from the then arm.
                    scope.restore(mark);
                    if self.rng.chance(50) {
                        body.push_str("    } else {\n");
                        let mut other = String::new();
                        self.statements(scope, &mut other, depth + 1);
                        body.push_str(&indent(&other));
                    }
                    body.push_str("    }\n");
                    scope.restore(mark);
                }
                _ => {
                    let trips = 1 + self.rng.below(6) as i64;
                    body.push_str(&format!("    for (int i = 0; i < {trips}; i++) {{\n"));
                    let mark = scope.variables.len();
                    scope.push_read_only("i", trips.saturating_sub(1));
                    let mut inner = String::new();
                    self.statements(scope, &mut inner, depth + 1);
                    body.push_str(&indent(&inner));
                    body.push_str("    }\n");
                    scope.restore(mark);
                }
            }
        }
    }

    fn index_expr(&mut self, scope: &Scope) -> String {
        match scope.pick(&mut self.rng) {
            Some(variable) => variable.name.clone(),
            None => self.rng.range(0, 64).to_string(),
        }
    }

    /// An element of an array in scope, spelled by indexing or by pointer
    /// arithmetic; every scope has at least one array.
    fn element(&mut self, scope: &Scope) -> String {
        let index = self.index_expr(scope);
        let array = scope.pick_array(&mut self.rng).expect("an array in scope");
        if self.rng.chance(50) {
            format!("{}[({index}) & {}]", array.name, array.mask)
        } else {
            format!("*({} + (({index}) & {}))", array.name, array.mask)
        }
    }

    fn comparison(&mut self, scope: &Scope, depth: u32) -> Expr {
        let op = ["<", "<=", ">", ">=", "==", "!="][self.rng.below(6) as usize];
        let lhs = self.expr(scope, VALUE_BOUND, depth);
        let rhs = self.expr(scope, VALUE_BOUND, depth);
        Expr {
            text: format!("({} {} {})", lhs.text, op, rhs.text),
            bound: 1,
        }
    }

    /// An expression whose value is provably within `bound`.
    fn expr(&mut self, scope: &Scope, bound: i64, depth: u32) -> Expr {
        let expr = self.build(scope, bound, depth);
        debug_assert!(
            expr.bound <= bound,
            "`{}` was asked for {bound} and claims {}",
            expr.text,
            expr.bound
        );
        expr
    }

    fn build(&mut self, scope: &Scope, bound: i64, depth: u32) -> Expr {
        // Below two there is nothing left for an operator to divide between
        // its operands, and every operator below assumes it has room.
        if depth > 2 || bound < 2 || self.rng.chance(30) {
            if !self.rng.chance(40) {
                if let Some(variable) = scope.pick_within(&mut self.rng, bound) {
                    return Expr {
                        text: variable.name.clone(),
                        bound: variable.bound,
                    };
                }
            }
            if bound >= VALUE_BOUND && self.rng.chance(25) {
                return Expr {
                    text: self.element(scope),
                    bound: VALUE_BOUND,
                };
            }
            let magnitude = bound.min(64);
            return Expr::constant(self.rng.range(-magnitude, magnitude));
        }

        let child = |generator: &mut Self| generator.expr(scope, bound / 2, depth + 1);
        match self.rng.below(9) {
            0 => {
                let (lhs, rhs) = (child(self), child(self));
                Expr {
                    text: format!("({} + {})", lhs.text, rhs.text),
                    bound: lhs.bound + rhs.bound,
                }
            }
            1 => {
                let (lhs, rhs) = (child(self), child(self));
                Expr {
                    text: format!("({} - {})", lhs.text, rhs.text),
                    bound: lhs.bound + rhs.bound,
                }
            }
            2 => {
                // Each factor within the square root of the product's bound,
                // so the product stays inside it.
                let factor = bound.isqrt();
                let lhs = self.expr(scope, factor, depth + 1);
                let rhs = self.expr(scope, factor, depth + 1);
                Expr {
                    text: format!("({} * {})", lhs.text, rhs.text),
                    bound: lhs.bound * rhs.bound,
                }
            }
            3 | 4 => {
                let numerator = child(self);
                if self.rng.below(2) == 0 {
                    // Nonzero by construction: `(x & 7) + 1` lies in [1, 8].
                    let divisor = match scope.pick(&mut self.rng) {
                        Some(variable) if self.rng.chance(50) => {
                            format!("(({} & 7) + 1)", variable.name)
                        }
                        _ => (2 + self.rng.below(8)).to_string(),
                    };
                    Expr {
                        text: format!("({} / ({divisor}))", numerator.text),
                        bound: numerator.bound,
                    }
                } else {
                    // A remainder is smaller than its divisor, so the divisor
                    // is what the requested bound has to cap.
                    let divisor = 2 + self.rng.below((bound + 1).min(9) as u64 - 1) as i64;
                    Expr {
                        text: format!("({} % ({divisor}))", numerator.text),
                        bound: divisor - 1,
                    }
                }
            }
            5 => {
                let (lhs, rhs) = (child(self), child(self));
                let op = ["&", "|", "^"][self.rng.below(3) as usize];
                Expr {
                    text: format!("({} {op} {})", lhs.text, rhs.text),
                    // Conservative: bit patterns of either operand may appear.
                    bound: lhs.bound + rhs.bound,
                }
            }
            6 => {
                // Left-shifting a negative value is undefined before C23 and
                // the reference compilers do not agree on which language they
                // are compiling, so the operand is masked non-negative: `x &
                // m` has only bits `m` has, hence lies in `[0, m]`. Room for
                // the amount is made by asking for a narrower operand rather
                // than by capping the amount against a wide one, which would
                // leave almost every shift a shift by nothing.
                let shift = self.rng.below(bound.ilog2() as u64 + 1) as i64;
                let value = self.expr(scope, bound >> shift, depth + 1);
                let mask = value.bound.max(1);
                Expr {
                    text: format!("(({} & {mask}) << {shift})", value.text),
                    bound: mask << shift,
                }
            }
            7 => {
                let value = child(self);
                let shift = self.rng.below(16) as i64;
                Expr {
                    text: format!("({} >> {shift})", value.text),
                    // An arithmetic shift of a negative value floors towards
                    // -1 and never reaches zero, however far it goes.
                    bound: match value.bound {
                        0 => 0,
                        magnitude => (magnitude >> shift).max(1),
                    },
                }
            }
            _ => {
                let value = child(self);
                Expr {
                    text: format!("-({})", value.text),
                    bound: value.bound,
                }
            }
        }
    }
}

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("    {line}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::generate;
    use crate::fcc_fuzz::ub;
    use std::process::Command;

    /// Seeds whose programs the nightly once filed as miscompiles, when in
    /// truth the generator had handed the compilers undefined behavior.
    const FILED_AS_DEFECTS: [u64; 2] = [32_806_581_369, 32_806_581_428];

    #[test]
    fn programs_are_a_pure_function_of_the_seed() {
        assert_eq!(generate(42), generate(42));
        assert_ne!(generate(42), generate(43));
    }

    #[test]
    fn generated_programs_have_defined_behavior() {
        // The whole differential method rests on this: two compilers only owe
        // each other the same answer where the standard says what the answer
        // is. A generated program that overflows, shifts a negative value or
        // reads an indeterminate one turns every divergence it produces into
        // a false report.
        for seed in (0u64..24).chain(FILED_AS_DEFECTS) {
            let dir = std::env::temp_dir().join(format!("fcc-fuzz-ub-seed-{seed}"));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("prog.c");
            std::fs::write(&source, generate(seed)).unwrap();
            let defined = ub::well_defined(&source, &dir);
            std::fs::remove_dir_all(&dir).ok();
            assert!(
                defined,
                "seed {seed}: the generated program has undefined behavior"
            );
        }
    }

    #[test]
    fn generated_programs_compile_and_run_bound() {
        // A sample of seeds must produce C that gcc accepts; the harness
        // compares behavior, which requires every variant to build.
        for seed in [0u64, 1, 7, 123] {
            let dir = std::env::temp_dir().join(format!("fcc-fuzz-gen-{seed}"));
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("prog.c");
            std::fs::write(&source, generate(seed)).unwrap();
            let status = Command::new("gcc")
                .args(["-O1", "-o"])
                .arg(dir.join("prog.bin"))
                .arg(&source)
                .status()
                .unwrap();
            assert!(status.success(), "seed {seed}: gcc rejected the program");
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
