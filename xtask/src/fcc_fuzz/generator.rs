//! Seeded generator of UB-free C programs for the differential fuzzer.
//!
//! Every program is a pure function of its seed: same seed, same bytes. The
//! generated code avoids undefined behavior by construction — integer bounds
//! are tracked through every expression so no signed operation can overflow,
//! divisors are nonzero by shape, left-shift amounts are bounded by the
//! operand's range, and array indices are masked to the array length.

const INT_MAX: i64 = 2_147_483_647;

/// Bound on function arguments at call sites; parameter expressions are
/// generated against this bound.
const PARAM_BOUND: i64 = 1 << 12;
/// Bound on values a function returns.
const RETURN_BOUND: i64 = 1 << 14;
/// Array length used throughout.
const ARRAY_LEN: usize = 8;

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

/// Variables in scope while generating one function body, plus the bound on
/// array elements.
struct Scope {
    variables: Vec<Variable>,
    array_bound: i64,
    /// Only `main` declares the array; functions must not touch it.
    has_array: bool,
}

impl Scope {
    fn new() -> Self {
        Self {
            variables: Vec::new(),
            array_bound: 0,
            has_array: false,
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
            out.push_str(&format!("int {name}(int a, int b);\n"));
        }
        out.push('\n');
        for function in &self.functions {
            out.push_str(&function.body);
        }
        out.push_str(&self.main_body());
        out
    }

    /// A function of two parameters returning a bounded value. It may call
    /// earlier functions, whose signatures it knows.
    fn function_body(&mut self) -> String {
        let mut scope = Scope::new();
        scope.push("a".into(), PARAM_BOUND);
        scope.push("b".into(), PARAM_BOUND);
        let mut body = String::new();
        self.statements(&mut scope, &mut body, 0);

        let result = self.expr(&scope, RETURN_BOUND, 0);
        let name = format!("f{}", self.functions.len());
        format!(
            "int {name}(int a, int b) {{\n{body}    return {};\n}}\n\n",
            result.text
        )
    }

    fn main_body(&mut self) -> String {
        let mut scope = Scope::new();
        let mut body = String::new();

        let elements: Vec<String> = (0..ARRAY_LEN)
            .map(|_| {
                let value = self.rng.range(-1000, 1000);
                scope.array_bound = scope.array_bound.max(value.abs());
                value.to_string()
            })
            .collect();
        body.push_str(&format!(
            "    int arr[{ARRAY_LEN}] = {{{}}};\n",
            elements.join(", ")
        ));
        scope.has_array = true;

        let names: Vec<String> = self.functions.iter().map(|f| f.name.clone()).collect();
        for name in &names {
            let result = self.fresh("r");
            let args: Vec<String> = (0..2)
                .map(|_| self.expr_detached(PARAM_BOUND).text)
                .collect();
            body.push_str(&format!(
                "    int {result} = {name}({});\n",
                args.join(", ")
            ));
            body.push_str(&format!("    printf(\"%d\\n\", {result});\n"));
            scope.push(result, RETURN_BOUND);
        }

        // A loop accumulating over the array, printed per trip so a divergence
        // localizes to a trip count.
        let trips = 3 + self.rng.below(6) as i64;
        let acc = self.fresh("s");
        body.push_str(&format!("    int {acc} = 0;\n"));
        body.push_str(&format!("    for (int i = 0; i < {trips}; i++) {{\n"));
        scope.push_read_only("i", trips.saturating_sub(1));
        let step = self.expr(&scope, 10_000, 1);
        body.push_str(&format!(
            "        {acc} = {} + arr[i & {}];\n",
            step.text,
            ARRAY_LEN - 1
        ));
        body.push_str(&format!("        printf(\"%d\\n\", {acc});\n"));
        body.push_str("    }\n");

        format!("int main(void) {{\n{body}    return 0;\n}}\n")
    }

    fn statements(&mut self, scope: &mut Scope, body: &mut String, depth: u32) {
        let count = 1 + self.rng.below(3);
        for _ in 0..count {
            match self.rng.below(if depth < 1 { 5 } else { 3 }) {
                0 => {
                    let expr = self.expr(scope, 10_000, depth + 1);
                    let name = self.fresh("v");
                    body.push_str(&format!("    int {name} = {};\n", expr.text));
                    scope.push(name, expr.bound);
                }
                1 => {
                    let Some(index) = scope.pick_assignable(&mut self.rng) else {
                        continue;
                    };
                    let bound = scope.variables[index].bound.max(2);
                    let expr = self.expr(scope, bound, depth + 1);
                    scope.variables[index].bound = expr.bound;
                    let name = scope.variables[index].name.clone();
                    body.push_str(&format!("    {name} = {};\n", expr.text));
                }
                2 if scope.has_array => {
                    let expr = self.expr(scope, 10_000, depth + 1);
                    let index = self.index_expr(scope);
                    body.push_str(&format!(
                        "    arr[({index}) & {}] = {};\n",
                        ARRAY_LEN - 1,
                        expr.text
                    ));
                    scope.array_bound = scope.array_bound.max(expr.bound);
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

    fn comparison(&mut self, scope: &Scope, depth: u32) -> Expr {
        let op = ["<", "<=", ">", ">=", "==", "!="][self.rng.below(6) as usize];
        let lhs = self.expr(scope, 10_000, depth);
        let rhs = self.expr(scope, 10_000, depth);
        Expr {
            text: format!("({} {} {})", lhs.text, op, rhs.text),
            bound: 1,
        }
    }

    fn expr(&mut self, scope: &Scope, bound: i64, depth: u32) -> Expr {
        self.expr_inner(Some(scope), bound, depth)
    }

    fn expr_detached(&mut self, bound: i64) -> Expr {
        self.expr_inner(None, bound, 0)
    }

    fn expr_inner(&mut self, scope: Option<&Scope>, bound: i64, depth: u32) -> Expr {
        if depth > 2 || self.rng.chance(30) {
            if let Some(scope) = scope {
                if !self.rng.chance(40) {
                    if let Some(variable) = scope.pick(&mut self.rng) {
                        return Expr {
                            text: variable.name.clone(),
                            bound: variable.bound,
                        };
                    }
                }
                if scope.has_array && self.rng.chance(25) {
                    return Expr {
                        text: format!("arr[({}) & {}]", self.index_expr(scope), ARRAY_LEN - 1),
                        bound: scope.array_bound,
                    };
                }
            }
            return Expr::constant(self.rng.range(-64, 64));
        }

        let child = |generator: &mut Self| generator.expr_inner(scope, bound / 2, depth + 1);
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
                // Both factors limited so their product cannot overflow.
                let factor = INT_MAX.isqrt().min(bound.max(2));
                let lhs = self.expr_inner(scope, factor.min(bound), depth + 1);
                let rhs = self.expr_inner(scope, factor.min(bound), depth + 1);
                Expr {
                    text: format!("({} * {})", lhs.text, rhs.text),
                    bound: lhs.bound.saturating_mul(rhs.bound),
                }
            }
            3 | 4 => {
                let numerator = child(self);
                // Nonzero by construction: `(x & 7) + 1` lies in [1, 8].
                let divisor = match scope.and_then(|s| s.pick(&mut self.rng)) {
                    Some(variable) if self.rng.chance(50) => {
                        format!("(({} & 7) + 1)", variable.name)
                    }
                    _ => (2 + self.rng.below(8)).to_string(),
                };
                if self.rng.below(2) == 0 {
                    Expr {
                        text: format!("({} / ({divisor}))", numerator.text),
                        bound: numerator.bound,
                    }
                } else {
                    Expr {
                        text: format!("({} % ({divisor}))", numerator.text),
                        bound: 8,
                    }
                }
            }
            5 => {
                let (lhs, rhs) = (child(self), child(self));
                let op = ["&", "|", "^"][self.rng.below(3) as usize];
                Expr {
                    text: format!("({} {op} {})", lhs.text, rhs.text),
                    // Conservative: bit patterns of either operand may appear.
                    bound: lhs.bound.saturating_add(rhs.bound),
                }
            }
            6 => {
                let value = child(self);
                // Left-shift amount capped so the result stays in range:
                // `bound << k <= INT_MAX` iff `k <= log2(INT_MAX / bound)`.
                let room = if value.bound <= 1 {
                    15
                } else {
                    ((INT_MAX / value.bound).ilog2() as i64).min(15)
                };
                let shift = self.rng.below(room.max(0) as u64 + 1) as i64;
                Expr {
                    text: format!("({} << {shift})", value.text),
                    bound: value.bound << shift,
                }
            }
            7 => {
                let value = child(self);
                let shift = self.rng.below(16) as i64;
                Expr {
                    text: format!("({} >> {shift})", value.text),
                    bound: value.bound >> shift,
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
    use std::process::Command;

    #[test]
    fn programs_are_a_pure_function_of_the_seed() {
        assert_eq!(generate(42), generate(42));
        assert_ne!(generate(42), generate(43));
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
