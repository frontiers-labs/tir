//! End-to-end linking: compile a libc-free program with `fcc`, link it via the
//! system `cc`, run it, and check its exit status. LIT cannot express this
//! (no `%t`, no way to execute a produced file), so it lives here. Gated to
//! supported host backends and skipped when `cc` is unavailable.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const FCC: &str = env!("CARGO_BIN_EXE_fcc");
const SOURCE: &str = "int main(void) { return 42; }\n";

fn cc_available() -> bool {
    Command::new("cc")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_fcc(dir: &Path, args: &[&str]) {
    let status = Command::new(FCC)
        .args(args)
        .current_dir(dir)
        .status()
        .expect("spawn fcc");
    assert!(status.success(), "fcc {args:?} failed");
}

fn run_program(dir: &Path, program: &str) -> Output {
    Command::new(dir.join(program))
        .output()
        .expect("run linked program")
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("program exited via signal")
}

fn compile_fcc(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("test.c"), source).unwrap();
    run_fcc(dir, &["cc", "test.c", "-o", output]);
}

fn compile_host(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("host.c"), source).unwrap();
    let status = Command::new("cc")
        .args(["host.c", "-o", output])
        .current_dir(dir)
        .status()
        .expect("spawn host cc");
    assert!(status.success(), "host cc failed");
}

fn assert_fcc_matches_host(source: &str) {
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "fcc-program");
    compile_host(dir.path(), source, "host-program");
    let fcc = run_program(dir.path(), "fcc-program");
    let host = run_program(dir.path(), "host-program");
    assert_eq!(exit_code(&fcc), exit_code(&host));
    assert_eq!(fcc.stdout, host.stdout);
    assert_eq!(fcc.stderr, host.stderr);
}

fn compile_host_object(dir: &Path, source: &str, output: &str) {
    fs::write(dir.join("host.c"), source).unwrap();
    let status = Command::new("cc")
        .args(["-c", "host.c", "-o", output])
        .current_dir(dir)
        .status()
        .expect("spawn host cc");
    assert!(status.success(), "host cc failed");
}

fn assert_fcc_object_executes_with_host(source: &str, host: &str) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("fcc.c"), source).unwrap();
    run_fcc(dir.path(), &["cc", "-c", "fcc.c", "-o", "fcc.o"]);
    compile_host_object(dir.path(), host, "host.o");
    run_fcc(dir.path(), &["cc", "fcc.o", "host.o", "-o", "program"]);
    assert_eq!(exit_code(&run_program(dir.path(), "program")), 0);
}

#[test]
fn compile_and_link_in_one_step() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("r.c"), SOURCE).unwrap();
    run_fcc(dir.path(), &["cc", "r.c", "-o", "r"]);
    assert_eq!(exit_code(&run_program(dir.path(), "r")), 42);
}

#[test]
fn system_headers_compile_across_translation_units() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("helper.c"),
        "#include <stdio.h>\nint helper(void) { return printf(\"fcc\"); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("main.c"),
        "#include <stdio.h>\nint helper(void); int main(void) { return helper() != 3; }\n",
    )
    .unwrap();
    run_fcc(
        dir.path(),
        &["cc", "-O2", "helper.c", "main.c", "-o", "program"],
    );
    assert_eq!(exit_code(&run_program(dir.path(), "program")), 0);
}

#[test]
fn separate_compile_then_link() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("r.c"), SOURCE).unwrap();
    run_fcc(dir.path(), &["cc", "-c", "r.c"]);
    assert!(dir.path().join("r.o").exists(), "r.o was not produced");
    run_fcc(dir.path(), &["cc", "r.o", "-o", "r2"]);
    assert_eq!(exit_code(&run_program(dir.path(), "r2")), 42);
}

#[test]
fn captures_program_output() {
    if !cc_available() {
        return;
    }
    let source = r#"int puts(const char *text);
int main(void) { puts("fcc output"); return 0; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "output");
    let output = run_program(dir.path(), "output");
    assert_eq!(exit_code(&output), 0);
    assert_eq!(output.stdout, b"fcc output\n");
}

#[test]
fn variadic_double_argument_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int printf(const char *format, ...); int main(void) { printf(\"%.1f\\n\", 1.5); return 0; }\n",
    );
}

#[test]
fn compares_program_with_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int puts(const char *text);
int main(void) { puts("same output"); return 17; }
"#,
    );
}

#[test]
fn scalar_fibonacci_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("scalar/fibonacci.c"));
}

#[test]
fn scalar_sieve_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("scalar/sieve.c"));
}

#[test]
fn scalar_recursive_descent_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("scalar/recursive_descent.c"));
}

#[test]
fn scalar_mixed_widths_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("scalar/mixed_widths.c"));
}

#[test]
fn scalar_call_chain_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("scalar/call_chain.c"));
}

#[test]
fn scalar_branch_mix_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("scalar/branch_mix.c"));
}

#[test]
fn local_pointer_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("memory/local_pointer.c"));
}

#[test]
fn memory_linked_list_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("memory/linked_list.c"));
}

#[test]
fn memory_hash_table_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(include_str!("memory/hash_table.c"));
}

#[test]
fn global_function_pointer_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int add_five(int value) { return value + 5; } int (*function)(int) = add_five; int main(void) { return function(37) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn function_pointer_call_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int add_five(int value) { return value + 5; } int main(void) { int (*function)(int) = add_five; return function(37) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn function_pointer_parameter_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "typedef int (*Unary)(int); int add_five(int value) { return value + 5; } int apply(Unary function, int value) { return function(value); } int main(void) { return apply(add_five, 37) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn dereferenced_function_pointer_call_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int add_five(int value) { return value + 5; } int main(void) { int (*function)(int) = add_five; return (*function)(37) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn returned_function_pointer_call_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "typedef int (*Unary)(int); int add_five(int value) { return value + 5; } Unary choose(void) { return add_five; } int main(void) { return choose()(37) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn function_pointer_large_record_return_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Triple { long a; long b; long c; }; typedef struct Triple (*Maker)(long); struct Triple make(long value) { struct Triple result = {value, value + 1, value + 2}; return result; } int main(void) { Maker maker = make; struct Triple result = maker(39); return result.a == 39 && result.b == 40 && result.c == 41 ? 0 : 1; }\n",
    );
}

#[test]
fn returned_function_pointer_large_record_return_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Triple { long a; long b; long c; }; typedef struct Triple (*Maker)(long); struct Triple make(long value) { struct Triple result = {value, value + 1, value + 2}; return result; } Maker choose(void) { return make; } int main(void) { struct Triple result = choose()(39); return result.a == 39 && result.b == 40 && result.c == 41 ? 0 : 1; }\n",
    );
}

#[test]
fn function_pointer_large_record_argument_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Triple { long a; long b; long c; }; typedef long (*Reducer)(struct Triple); long sum(struct Triple value) { return value.a + value.b + value.c; } int main(void) { Reducer reducer = sum; struct Triple value = {13, 14, 15}; return reducer(value) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn variadic_function_pointer_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "typedef int (*Variadic)(const char *, ...); int call_first(Variadic call, const char *text) { return call(text, 42); }\n",
        "int call_first(int (*)(const char *, ...), const char *); int first(const char *text, ...) { return text[0]; } int main(void) { return call_first(first, \"x\") == 'x' ? 0 : 1; }\n",
    );
}

#[test]
fn pointer_addition_scales_by_pointee_size() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int third(int *values) { return *(values + 2); }\n",
        "int third(int *); int main(void) { int values[3] = {11, 22, 37}; return third(values) == 37 ? 0 : 1; }\n",
    );
}

#[test]
fn pointer_increment_scales_by_pointee_size() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int *advance(int **value) { return (*value)++; }\n",
        "int *advance(int **); int main(void) { int values[2]; int *value = values; return advance(&value) == values && value == values + 1 ? 0 : 1; }\n",
    );
}

#[test]
fn pointer_subtraction_scales_by_pointee_size() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int previous(int *value) { return *(value - 1); }\n",
        "int previous(int *); int main(void) { int values[3] = {11, 22, 37}; return previous(&values[2]) == 22 ? 0 : 1; }\n",
    );
}

#[test]
fn pointer_difference_counts_elements() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "long distance(int *begin, int *end) { return end - begin; }\n",
        "long distance(int *, int *); int main(void) { int values[4]; return distance(values, values + 3) == 3 && distance(values + 3, values) == -3 ? 0 : 1; }\n",
    );
}

#[test]
fn integer_plus_pointer_scales_by_pointee_size() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int third(int *values) { return *(2 + values); }\n",
        "int third(int *); int main(void) { int values[3] = {11, 22, 37}; return third(values) == 37 ? 0 : 1; }\n",
    );
}

#[test]
fn pointer_subscript_scales_by_pointee_size() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int third(int *values) { return values[2]; }\n",
        "int third(int *); int main(void) { int values[3] = {11, 22, 37}; return third(values) == 37 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_subscript_scales_by_element_size() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "enum Index { SECOND = 1 }; int select(int *values, enum Index index) { return values[index]; }\n",
        "enum Index { SECOND = 1 }; int select(int *, enum Index); int main(void) { int values[2] = {11, 37}; return select(values, SECOND) == 37 ? 0 : 1; }\n",
    );
}

#[test]
fn local_array_storage_is_contiguous() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[3]; values[0] = 11; values[1] = 22; values[2] = 37; return values[0] + values[1] + values[2] - 70; }\n",
    );
}

#[test]
fn local_array_decays_when_passed_to_function() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int sum(int *values) { return values[0] + values[1] + values[2]; } int main(void) { int values[3]; values[0] = 11; values[1] = 22; values[2] = 37; return sum(values) - 70; }\n",
    );
}

#[test]
fn local_array_initializer_zero_fills_remainder() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[3] = {11, 22}; return values[0] + values[1] + values[2] - 33; }\n",
    );
}

#[test]
fn local_array_designated_initializer_selects_elements() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[5] = {[3] = 30, [1] = 12}; return values[0] == 0 && values[1] == 12 && values[2] == 0 && values[3] == 30 && values[4] == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn array_initializer_continues_after_designator() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[4] = {[1] = 12, 30}; return values[0] == 0 && values[1] == 12 && values[2] == 30 && values[3] == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn local_array_initializer_infers_omitted_bound() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[] = {11, 22, 37}; return sizeof(values) == 12 && values[2] == 37 ? 0 : 1; }\n",
    );
}

#[test]
fn local_array_designator_infers_omitted_bound() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[] = {[4] = 42}; return sizeof(values) == 20 && values[4] == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn nested_array_initializer_uses_row_major_storage() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[2][3] = {{11, 22, 33}, {44, 55}}; return sizeof(values) == 24 && values[1][0] == 44 && values[1][2] == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn nested_array_initializer_infers_outer_bound() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { int values[][2] = {{11, 22}, {33, 44}}; return sizeof(values) == 16 && values[1][1] == 44 ? 0 : 1; }\n",
    );
}

#[test]
fn bitwise_and_shifts_match_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"unsigned int bits(unsigned int a, unsigned int b) {
    return ((a & b) | (a ^ b)) << 2 >> 1;
}
int signed_shift(int value) { return value >> 3; }
int main(void) {
    if (bits(10, 12) != 28) return 1;
    if (signed_shift(16) != 2) return 2;
    return 0;
}
"#,
    );
}

#[test]
fn variable_shift_count_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"unsigned int shift(unsigned int value, unsigned int count) {
    return value << count;
}
int main(void) {
    if (shift(3, 4) == 48) return 0;
    return 1;
}
"#,
    );
}

#[test]
fn double_addition_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double add(double lhs, double rhs) { return lhs + rhs; }\n",
        "double add(double, double); int main(void) { return add(1.25, 2.5) == 3.75 ? 0 : 1; }\n",
    );
}

#[test]
fn double_subtraction_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double subtract(double lhs, double rhs) { return lhs - rhs; }\n",
        "double subtract(double, double); int main(void) { return subtract(4.5, 1.25) == 3.25 ? 0 : 1; }\n",
    );
}

#[test]
fn double_multiplication_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double multiply(double lhs, double rhs) { return lhs * rhs; }\n",
        "double multiply(double, double); int main(void) { return multiply(1.5, 2.5) == 3.75 ? 0 : 1; }\n",
    );
}

#[test]
fn double_division_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double divide(double lhs, double rhs) { return lhs / rhs; }\n",
        "double divide(double, double); int main(void) { return divide(7.5, 2.5) == 3.0 ? 0 : 1; }\n",
    );
}

#[test]
fn double_literal_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double literal(void) { return 1.5; }\n",
        "double literal(void); int main(void) { return literal() == 1.5 ? 0 : 1; }\n",
    );
}

#[test]
fn signed_integer_to_double_conversion_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double convert(int value) { return value; }\n",
        "double convert(int); int main(void) { return convert(-17) == -17.0 ? 0 : 1; }\n",
    );
}

#[test]
fn unsigned_integer_to_double_conversion_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double convert(unsigned int value) { return value; }\n",
        "double convert(unsigned int); int main(void) { return convert(4000000000u) == 4000000000.0 ? 0 : 1; }\n",
    );
}

#[test]
fn double_to_signed_integer_conversion_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int convert(double value) { return value; }\n",
        "int convert(double); int main(void) { return convert(-17.75) == -17 ? 0 : 1; }\n",
    );
}

#[test]
fn double_to_unsigned_integer_conversion_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "unsigned int convert(double value) { return value; }\n",
        "unsigned int convert(double); int main(void) { return convert(4000000000.75) == 4000000000u ? 0 : 1; }\n",
    );
}

#[test]
fn double_less_than_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int less(double left, double right) { return left < right; }\n",
        "int less(double, double);\n\
         int main(void) {\n\
           double nan = 0.0 / 0.0;\n\
           return less(-1.25, 2.5) == 1 && less(3.0, 2.0) == 0 &&\n\
                  less(-0.0, 0.0) == 0 && less(nan, 1.0) == 0 ? 0 : 1;\n\
         }\n",
    );
}

#[test]
fn double_less_equal_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int less_equal(double left, double right) { return left <= right; }\n",
        "int less_equal(double, double); int main(void) { double nan = 0.0 / 0.0; return less_equal(-1.25, 2.5) == 1 && less_equal(2.5, 2.5) == 1 && less_equal(3.0, 2.0) == 0 && less_equal(nan, 2.0) == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn double_greater_than_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int greater(double left, double right) { return left > right; }\n",
        "int greater(double, double); int main(void) { double nan = 0.0 / 0.0; return greater(3.0, 2.0) == 1 && greater(2.0, 2.0) == 0 && greater(-1.25, 2.5) == 0 && greater(nan, 2.0) == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn double_greater_equal_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int greater_equal(double left, double right) { return left >= right; }\n",
        "int greater_equal(double, double); int main(void) { double nan = 0.0 / 0.0; return greater_equal(3.0, 2.0) == 1 && greater_equal(2.0, 2.0) == 1 && greater_equal(-1.25, 2.5) == 0 && greater_equal(nan, 2.0) == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn double_equal_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int equal(double left, double right) { return left == right; }\n",
        "int equal(double, double); int main(void) { double nan = 0.0 / 0.0; return equal(2.5, 2.5) == 1 && equal(-1.25, 2.5) == 0 && equal(nan, nan) == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn double_not_equal_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int not_equal(double left, double right) { return left != right; }\n",
        "int not_equal(double, double); int main(void) { double nan = 0.0 / 0.0; return not_equal(2.5, 2.5) == 0 && not_equal(-1.25, 2.5) == 1 && not_equal(nan, nan) == 1 ? 0 : 1; }\n",
    );
}

#[test]
fn signed_integer_division_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int divide(int lhs, int rhs) { return lhs / rhs; }\n",
        "int divide(int, int); int main(void) { return divide(-17, 5) == -3 ? 0 : 1; }\n",
    );
}

#[test]
fn unsigned_integer_division_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "unsigned int divide(unsigned int lhs, unsigned int rhs) { return lhs / rhs; }\n",
        "unsigned int divide(unsigned int, unsigned int); int main(void) { return divide(4294967295U, 2U) == 2147483647U ? 0 : 1; }\n",
    );
}

#[test]
fn signed_integer_remainder_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int mod_signed(int lhs, int rhs) { return lhs % rhs; }\n",
        "int mod_signed(int, int); int main(void) { return mod_signed(-17, 5) == -2 ? 0 : 1; }\n",
    );
}

#[test]
fn signed_integer_remainder_assignment_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "int mod_assign(int lhs, int rhs) { lhs %= rhs; return lhs; }\n",
        "int mod_assign(int, int); int main(void) { return mod_assign(-17, 5) == -2 ? 0 : 1; }\n",
    );
}

#[test]
fn unsigned_integer_remainder_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "unsigned int mod_unsigned(unsigned int lhs, unsigned int rhs) { return lhs % rhs; }\n",
        "unsigned int mod_unsigned(unsigned int, unsigned int); int main(void) { return mod_unsigned(4294967295U, 2U) == 1U ? 0 : 1; }\n",
    );
}

#[test]
fn unsigned_integer_remainder_assignment_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "unsigned int mod_assign(unsigned int lhs, unsigned int rhs) { lhs %= rhs; return lhs; }\n",
        "unsigned int mod_assign(unsigned int, unsigned int); int main(void) { return mod_assign(4294967295U, 2U) == 1U ? 0 : 1; }\n",
    );
}

#[test]
fn double_add_assignment_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double update(double value, double amount) { value += amount; return value; }\n",
        "double update(double, double); int main(void) { return update(1.25, 2.5) == 3.75 ? 0 : 1; }\n",
    );
}

#[test]
fn double_sub_assignment_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double update(double value, double amount) { value -= amount; return value; }\n",
        "double update(double, double); int main(void) { return update(4.5, 1.25) == 3.25 ? 0 : 1; }\n",
    );
}

#[test]
fn double_mul_assignment_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double update(double value, double amount) { value *= amount; return value; }\n",
        "double update(double, double); int main(void) { return update(1.5, 2.5) == 3.75 ? 0 : 1; }\n",
    );
}

#[test]
fn double_div_assignment_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double update(double value, double amount) { value /= amount; return value; }\n",
        "double update(double, double); int main(void) { return update(7.5, 2.5) == 3.0 ? 0 : 1; }\n",
    );
}

#[test]
fn character_constant_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int value(void) { return 'A'; } int main(void) { if (value() != 65) return 1; return 0; }\n",
    );
}

#[test]
fn enum_constants_use_implicit_and_explicit_values() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Color { Red, Green = 5, Blue }; int main(void) { return Red == 0 && Green == 5 && Blue == 6 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_constants_evaluate_integer_constant_expressions() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Value { Base = 3, Scaled = Base * 4 + 2, Negative = -1 }; int main(void) { return Scaled == 14 && Negative == -1 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_constants_evaluate_shift_and_bitwise_expressions() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Flags { Read = 1 << 0, Write = 1 << 1, Both = Read | Write, Masked = (Both ^ Read) & 3, High = 8 >> 1 }; int main(void) { return Both == 3 && Masked == 2 && High == 4 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_constants_evaluate_remainder_and_unary_expressions() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Value { Remainder = 17 % 5, Inverted = ~0, False = !1, True = !0 }; int main(void) { return Remainder == 2 && Inverted == -1 && False == 0 && True == 1 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_constants_evaluate_comparison_logical_and_conditional_expressions() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Decision { Lt = 2 < 3, Le = 3 <= 3, Gt = 4 > 3, Ge = 4 >= 4, Eq = 5 == 5, Ne = 5 != 6, Both = Lt && Eq, Either = 0 || Both, Pick = Either ? 9 : 1 / 0 }; int main(void) { return Lt && Le && Gt && Ge && Eq && Ne && Both && Either && Pick == 9 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_constants_accept_character_constants() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Character { Letter = 'A', Newline = '\\n' }; int main(void) { return Letter == 65 && Newline == 10 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_constants_accept_sizeof_and_integer_casts() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Value { IntSize = sizeof(int), LongSize = sizeof(long), Narrow = (unsigned char)258, Signed = (signed char)255, Truth = (_Bool)42 }; int main(void) { return IntSize == 4 && LongSize == 8 && Narrow == 2 && Signed == -1 && Truth == 1 ? 0 : 1; }\n",
    );
}

#[test]
fn enum_constants_accept_immediately_cast_floating_constants() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Value { Truncated = (int)3.75 }; int main(void) { return Truncated == 3 ? 0 : 1; }\n",
    );
}

#[test]
fn tagged_enum_objects_execute_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "enum Color { Red = 3 }; int main(void) { enum Color value = Red; return value == 3 ? 0 : 1; }\n",
    );
}

#[test]
fn local_enum_declarations_execute_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { enum Local { Base = 2, Value = Base + 3 }; return Value == 5 ? 0 : 1; }\n",
    );
}

#[test]
fn local_enum_definition_can_declare_an_object() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int main(void) { enum Local { Value = 5 } value = Value; return value == 5 ? 0 : 1; }\n",
    );
}

#[test]
fn escaped_character_constant_executes_through_driver() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int value(void) { return '\\n'; } int main(void) { if (value() != 10) return 1; return 0; }\n",
    );
}

#[test]
fn logical_and_short_circuits_rhs() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int logical_and(int lhs) {
    int rhs = 0;
    int result = lhs && ++rhs;
    return result * 10 + rhs;
}
int main(void) {
    if (logical_and(0) != 0) return 1;
    if (logical_and(1) != 11) return 2;
    return 0;
}
"#,
    );
}

#[test]
fn logical_or_short_circuits_rhs() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int logical_or(int lhs) {
    int rhs = 0;
    int result = lhs || ++rhs;
    return result * 10 + rhs;
}
int main(void) {
    if (logical_or(0) != 11) return 1;
    if (logical_or(1) != 10) return 2;
    return 0;
}
"#,
    );
}

#[test]
fn conditional_operator_executes_only_selected_arm() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int conditional(int condition) {
    int lhs = 0;
    int rhs = 0;
    int result = condition ? ++lhs : ++rhs;
    return result * 100 + lhs * 10 + rhs;
}
int main(void) {
    if (conditional(0) != 101) return 1;
    if (conditional(1) != 110) return 2;
    return 0;
}
"#,
    );
}

#[test]
fn switch_dispatch_fallthrough_and_break_match_host() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int classify(int value) {
    int result = 0;
    switch (value) {
    case 0:
        result = 1;
        break;
    case 1:
        result = 2;
    case 2:
        result += 3;
        break;
    default:
        result = 9;
    }
    return result;
}
int main(void) {
    if (classify(0) != 1) return 1;
    if (classify(1) != 5) return 2;
    if (classify(2) != 3) return 3;
    if (classify(3) != 9) return 4;
    return 0;
}
"#,
    );
}

#[test]
fn switch_break_exits_nearest_scope() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int accumulate(void) {
    int result = 0;
    for (int i = 0; i < 3; i = i + 1) {
        switch (i) {
        case 0:
            result += 1;
            break;
        default:
            result += 2;
        }
        result += 4;
    }
    return result;
}
int main(void) { return accumulate() == 17 ? 0 : 1; }
"#,
    );
}

#[test]
fn switch_default_can_fall_through_in_source_order() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int classify(int value) {
    int result = 0;
    switch (value) {
    default:
        result = 4;
    case 2:
        result += 3;
        break;
    case 5:
        result = 9;
    }
    return result;
}
int main(void) {
    if (classify(0) != 7) return 1;
    if (classify(2) != 3) return 2;
    if (classify(5) != 9) return 3;
    return 0;
}
"#,
    );
}

#[test]
fn switch_without_matching_case_preserves_state() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int classify(int value) {
    int result = 4;
    switch (value) {
    case 1:
        result = 9;
    }
    return result;
}
int main(void) { return classify(2) == 4 ? 0 : 1; }
"#,
    );
}

#[test]
fn goto_and_labels_match_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int sum_to(int limit) {
    int sum = 0;
    int value = 0;
again:
    if (value == limit) goto done;
    sum += value;
    value = value + 1;
    goto again;
done:
    return sum;
}
int main(void) { return sum_to(5) == 10 ? 0 : 1; }
"#,
    );
}

#[test]
fn goto_can_enter_a_loop_body() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int count(int enter) {
    int value = 0;
    int total = 0;
    if (enter) goto inside;
    while (value < 2) {
        total += 10;
inside:
        total += 1;
        value = value + 1;
    }
    return total;
}
int main(void) {
    if (count(0) != 22) return 1;
    if (count(1) != 12) return 2;
    return 0;
}
"#,
    );
}

#[test]
fn goto_can_exit_nested_control_flow() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int count(void) {
    int value = 0;
    while (1) {
        if (value == 3) goto done;
        value = value + 1;
    }
done:
    return value;
}
int main(void) { return count() == 3 ? 0 : 1; }
"#,
    );
}

#[test]
fn goto_reaches_a_label_after_return() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int choose(int second) {
    if (second) goto second_result;
    return 1;
second_result:
    return 2;
}
int main(void) {
    if (choose(0) != 1) return 1;
    if (choose(1) != 2) return 2;
    return 0;
}
"#,
    );
}

#[test]
fn goto_reaches_a_nested_label_after_return() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int choose(int second) {
    if (second) goto second_result;
    return 1;
    if (0) {
second_result:
        return 2;
    }
    return 3;
}
int main(void) {
    if (choose(0) != 1) return 1;
    if (choose(1) != 2) return 2;
    return 0;
}
"#,
    );
}

#[test]
fn unary_operators_match_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int negate(int value) { return -value; }
unsigned int complement(unsigned int value) { return ~value; }
int logical_not(int value) { return !value; }
int positive(int value) { return +value; }
int main(void) {
    if (negate(7) + 7 != 0) return 1;
    if (complement(0) + 1 != 0) return 2;
    if (logical_not(0) != 1) return 3;
    if (logical_not(9) != 0) return 4;
    if (positive(9) != 9) return 5;
    return 0;
}
"#,
    );
}

#[test]
fn comma_operator_matches_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int comma_value(void) {
    int value = 0;
    return (value = 3, value + 4);
}
int main(void) {
    if (comma_value() == 7) return 0;
    return 1;
}
"#,
    );
}

#[test]
fn integer_casts_match_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int truncate(int value) { return (unsigned char)value; }
long widen(int value) { return (long)value; }
unsigned long widen_unsigned(unsigned int value) { return (unsigned long)value; }
int main(void) {
    if (truncate(257) != 1) return 1;
    if ((int)(widen(-2) >> 32) != -1) return 2;
    if ((int)(widen_unsigned(7U) >> 32) != 0) return 3;
    return 0;
}
"#,
    );
}

#[test]
fn increment_operators_match_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int increment_values(void) {
    int value = 4;
    int post = value++;
    int pre = ++value;
    int old = value--;
    int now = --value;
    return post + pre + old + now + value;
}
int main(void) {
    if (increment_values() == 24) return 0;
    return 1;
}
"#,
    );
}

#[test]
fn compound_assignments_match_host_compiler() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        r#"int compound_assign(void) {
    int value = 5;
    value += 3;
    value *= 2;
    value -= 4;
    value <<= 1;
    value >>= 2;
    value &= 7;
    value ^= 3;
    value |= 8;
    return value;
}
int main(void) {
    if (compound_assign() == 13) return 0;
    return 1;
}
"#,
    );
}

#[test]
fn loops_execute_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"int loop_break(int n) {
    for (;;) { if (n) break; return 4; }
    return 7;
}
int main(void) {
    if (loop_break(0) != 4) return 1;
    if (loop_break(1) != 7) return 2;
    return 0;
}
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "loops");
    assert_eq!(exit_code(&run_program(dir.path(), "loops")), 0);
}

#[test]
fn struct_fields_execute_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"struct Pair { char tag; int value; };
int read(void) { struct Pair pair; pair.value = 42; return pair.value; }
int main(void) { if (read() == 42) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "struct-fields");
    assert_eq!(exit_code(&run_program(dir.path(), "struct-fields")), 0);
}

#[test]
fn pointer_member_access_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("read.c"),
        "struct Pair { char tag; int value; }; int read(struct Pair *pair) { return pair->value; }\n",
    )
    .unwrap();
    run_fcc(dir.path(), &["cc", "-c", "read.c", "-o", "read.o"]);
    compile_host_object(
        dir.path(),
        "struct Pair { char tag; int value; }; int read(struct Pair *); int main(void) { struct Pair pair = { 1, 73 }; return read(&pair) == 73 ? 0 : 1; }\n",
        "host.o",
    );
    run_fcc(
        dir.path(),
        &["cc", "read.o", "host.o", "-o", "pointer-member"],
    );
    assert_eq!(exit_code(&run_program(dir.path(), "pointer-member")), 0);
}

#[test]
fn one_word_struct_argument_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Box { int value; }; int read(struct Box box) { return box.value; }\n",
        "struct Box { int value; }; int read(struct Box); int main(void) { struct Box box = {42}; return read(box) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn one_word_struct_call_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Box { int value; }; int read(struct Box); int main(void) { struct Box box = {42}; return read(box) == 42 ? 0 : 1; }\n",
        "struct Box { int value; }; int read(struct Box box) { return box.value; }\n",
    );
}

#[test]
fn two_word_struct_argument_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Pair { long left; long right; }; long sum(struct Pair pair) { return pair.left + pair.right; }\n",
        "struct Pair { long left; long right; }; long sum(struct Pair); int main(void) { struct Pair pair = {11, 31}; return sum(pair) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn two_word_struct_call_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Pair { long left; long right; }; long sum(struct Pair); int main(void) { struct Pair pair = {11, 31}; return sum(pair) == 42 ? 0 : 1; }\n",
        "struct Pair { long left; long right; }; long sum(struct Pair pair) { return pair.left + pair.right; }\n",
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn four_member_hfa_argument_and_return_match_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Quad { double first; double second; double third; double fourth; }; long check(double a, double b, double c, double d, double e, double f, struct Quad value) { return a == 1.0 && b == 2.0 && c == 3.0 && d == 4.0 && e == 5.0 && f == 6.0 && value.first == 4.25 && value.second == 5.5 && value.third == 6.75 && value.fourth == 7.125 ? 0 : 1; } struct Quad make(void) { struct Quad result = {4.25, 5.5, 6.75, 7.125}; return result; }\n",
        "struct Quad { double first; double second; double third; double fourth; }; long check(double, double, double, double, double, double, struct Quad); struct Quad make(void); int main(void) { struct Quad value = {4.25, 5.5, 6.75, 7.125}; if (check(1.0, 2.0, 3.0, 4.0, 5.0, 6.0, value)) return 1; struct Quad result = make(); return result.first == 4.25 && result.second == 5.5 && result.third == 6.75 && result.fourth == 7.125 ? 0 : 2; }\n",
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn four_member_hfa_call_and_return_call_match_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Quad { double first; double second; double third; double fourth; }; long check(double, double, double, double, double, double, struct Quad); struct Quad make(void); int main(void) { struct Quad value = {4.25, 5.5, 6.75, 7.125}; if (check(1.0, 2.0, 3.0, 4.0, 5.0, 6.0, value)) return 1; struct Quad result = make(); return result.first == 4.25 && result.second == 5.5 && result.third == 6.75 && result.fourth == 7.125 ? 0 : 2; }\n",
        "struct Quad { double first; double second; double third; double fourth; }; long check(double a, double b, double c, double d, double e, double f, struct Quad value) { return a == 1.0 && b == 2.0 && c == 3.0 && d == 4.0 && e == 5.0 && f == 6.0 && value.first == 4.25 && value.second == 5.5 && value.third == 6.75 && value.fourth == 7.125 ? 0 : 1; } struct Quad make(void) { struct Quad result = {4.25, 5.5, 6.75, 7.125}; return result; }\n",
    );
}

#[test]
fn union_argument_and_return_match_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "union Word { long integer; double fp; }; long check(long i0, long i1, long i2, long i3, long i4, long i5, long i6, double d0, double d1, double d2, double d3, double d4, double d5, double d6, union Word value) { return i0 == 10 && i1 == 11 && i2 == 12 && i3 == 13 && i4 == 14 && i5 == 15 && i6 == 16 && d0 == 1.0 && d1 == 2.0 && d2 == 3.0 && d3 == 4.0 && d4 == 5.0 && d5 == 6.0 && d6 == 7.0 && value.integer == 808 ? 0 : 1; } union Word make(void) { union Word result = {808}; return result; }\n",
        "union Word { long integer; double fp; }; long check(long, long, long, long, long, long, long, double, double, double, double, double, double, double, union Word); union Word make(void); int main(void) { union Word value = {808}; if (check(10, 11, 12, 13, 14, 15, 16, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, value)) return 1; union Word result = make(); return result.integer == 808 ? 0 : 2; }\n",
    );
}

#[test]
fn union_call_and_return_call_match_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "union Word { long integer; double fp; }; long check(long, long, long, long, long, long, long, double, double, double, double, double, double, double, union Word); union Word make(void); int main(void) { union Word value = {808}; if (check(10, 11, 12, 13, 14, 15, 16, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, value)) return 1; union Word result = make(); return result.integer == 808 ? 0 : 2; }\n",
        "union Word { long integer; double fp; }; long check(long i0, long i1, long i2, long i3, long i4, long i5, long i6, double d0, double d1, double d2, double d3, double d4, double d5, double d6, union Word value) { return i0 == 10 && i1 == 11 && i2 == 12 && i3 == 13 && i4 == 14 && i5 == 15 && i6 == 16 && d0 == 1.0 && d1 == 2.0 && d2 == 3.0 && d3 == 4.0 && d4 == 5.0 && d5 == 6.0 && d6 == 7.0 && value.integer == 808 ? 0 : 1; } union Word make(void) { union Word result = {808}; return result; }\n",
    );
}

#[test]
fn mixed_struct_argument_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Mixed { double fp; long integer; }; long read(struct Mixed value) { return value.fp == 10.0 ? value.integer : 0; }\n",
        "struct Mixed { double fp; long integer; }; long read(struct Mixed); int main(void) { struct Mixed value = {10.0, 32}; return read(value) == 32 ? 0 : 1; }\n",
    );
}

#[test]
fn mixed_struct_call_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Mixed { double fp; long integer; }; long read(struct Mixed); int main(void) { struct Mixed value = {10.0, 32}; return read(value) == 32 ? 0 : 1; }\n",
        "struct Mixed { double fp; long integer; }; long read(struct Mixed value) { return value.fp == 10.0 ? value.integer : 0; }\n",
    );
}

#[test]
fn large_struct_argument_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Large { long values[3]; }; long sum(struct Large value, long tail) { return value.values[0] + value.values[1] + value.values[2] + tail; }\n",
        "struct Large { long values[3]; }; long sum(struct Large, long); int main(void) { struct Large value = {{5, 7, 11}}; return sum(value, 19) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn large_struct_call_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Large { long values[3]; }; long sum(struct Large, long); int main(void) { struct Large value = {{5, 7, 11}}; return sum(value, 19) == 42 ? 0 : 1; }\n",
        "struct Large { long values[3]; }; long sum(struct Large value, long tail) { return value.values[0] + value.values[1] + value.values[2] + tail; }\n",
    );
}

#[test]
fn pressured_struct_argument_rolls_back_sysv_registers() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Pair { long left; long right; }; long check(long a, long b, long c, long d, long e, struct Pair pair, long tail) { return a + b + c + d + e + pair.left + pair.right + tail; }\n",
        "struct Pair { long left; long right; }; long check(long, long, long, long, long, struct Pair, long); int main(void) { struct Pair pair = {6, 7}; return check(1, 2, 3, 4, 5, pair, 14) == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn pressured_struct_call_rolls_back_sysv_registers() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Pair { long left; long right; }; long check(long, long, long, long, long, struct Pair, long); int main(void) { struct Pair pair = {6, 7}; return check(1, 2, 3, 4, 5, pair, 14) == 42 ? 0 : 1; }\n",
        "struct Pair { long left; long right; }; long check(long a, long b, long c, long d, long e, struct Pair pair, long tail) { return a + b + c + d + e + pair.left + pair.right + tail; }\n",
    );
}

#[test]
fn float_stack_call_matches_sysv_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double ninth(double, double, double, double, double, double, double, double, double); int main(void) { return ninth(1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0) == 9.0 ? 0 : 1; }\n",
        "double ninth(double a0, double a1, double a2, double a3, double a4, double a5, double a6, double a7, double a8) { return a8; }\n",
    );
}

#[test]
fn float_stack_parameter_matches_sysv_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "double ninth(double a0, double a1, double a2, double a3, double a4, double a5, double a6, double a7, double a8) { return a8; }\n",
        "double ninth(double, double, double, double, double, double, double, double, double); int main(void) { return ninth(1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0) == 9.0 ? 0 : 1; }\n",
    );
}

#[test]
fn integer_stack_call_survives_register_pressure() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "long sum20(long, long, long, long, long, long, long, long, long, long, long, long, long, long, long, long, long, long, long, long); int main(void) { return sum20(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20) == 210 ? 0 : 1; }\n",
        "long sum20(long a1, long a2, long a3, long a4, long a5, long a6, long a7, long a8, long a9, long a10, long a11, long a12, long a13, long a14, long a15, long a16, long a17, long a18, long a19, long a20) { return a1+a2+a3+a4+a5+a6+a7+a8+a9+a10+a11+a12+a13+a14+a15+a16+a17+a18+a19+a20; }\n",
    );
}

#[test]
fn one_word_struct_return_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Box { int value; }; struct Box make(int value) { struct Box box = {value}; return box; }\n",
        "struct Box { int value; }; struct Box make(int); int main(void) { return make(42).value == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn one_word_struct_return_call_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Box { int value; }; struct Box make(int); int main(void) { return make(42).value == 42 ? 0 : 1; }\n",
        "struct Box { int value; }; struct Box make(int value) { struct Box box = {value}; return box; }\n",
    );
}

#[test]
fn two_word_struct_return_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Pair { long left; long right; }; struct Pair make(long left, long right) { struct Pair pair = {left, right}; return pair; }\n",
        "struct Pair { long left; long right; }; struct Pair make(long, long); int main(void) { struct Pair pair = make(11, 31); return pair.left + pair.right == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn two_word_struct_return_call_matches_host_abi() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Pair { long left; long right; }; struct Pair make(long, long); int main(void) { struct Pair pair = make(11, 31); return pair.left + pair.right == 42 ? 0 : 1; }\n",
        "struct Pair { long left; long right; }; struct Pair make(long left, long right) { struct Pair pair = {left, right}; return pair; }\n",
    );
}

#[test]
fn mixed_struct_return_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Mixed { double fp; long integer; }; struct Mixed make(double fp, long integer) { struct Mixed value = {fp, integer}; return value; }\n",
        "struct Mixed { double fp; long integer; }; struct Mixed make(double, long); int main(void) { struct Mixed value = make(10.0, 32); return value.fp == 10.0 && value.integer == 32 ? 0 : 1; }\n",
    );
}

#[test]
fn mixed_struct_return_call_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Mixed { double fp; long integer; }; struct Mixed make(double, long); int main(void) { struct Mixed value = make(10.0, 32); return value.fp == 10.0 && value.integer == 32 ? 0 : 1; }\n",
        "struct Mixed { double fp; long integer; }; struct Mixed make(double fp, long integer) { struct Mixed value = {fp, integer}; return value; }\n",
    );
}

#[test]
fn large_struct_return_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Large { long values[3]; }; struct Large make(long a, long b, long c) { struct Large value = {{a, b, c}}; return value; }\n",
        "struct Large { long values[3]; }; struct Large make(long, long, long); int main(void) { struct Large value = make(5, 7, 30); return value.values[0] + value.values[1] + value.values[2] == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn large_struct_return_call_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Large { long values[3]; }; struct Large make(long, long, long); int main(void) { struct Large value = make(5, 7, 30); return (int)(value.values[0] + value.values[1] + value.values[2] - 42); }\n",
        "struct Large { long values[3]; }; struct Large make(long a, long b, long c) { struct Large value = {{a, b, c}}; return value; }\n",
    );
}

#[test]
fn nested_large_struct_return_matches_sysv_host_abi() {
    if !cc_available() || !cfg!(target_arch = "x86_64") {
        return;
    }
    assert_fcc_object_executes_with_host(
        "struct Large { long values[3]; }; struct Large make(long a, long b, long c) { struct Large value = {{a, b, c}}; return value; } struct Large forward(long a, long b, long c) { return make(a, b, c); }\n",
        "struct Large { long values[3]; }; struct Large forward(long, long, long); int main(void) { struct Large value = forward(5, 7, 30); return value.values[0] + value.values[1] + value.values[2] == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn whole_struct_copy_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"struct Pair { char tag; int value; };
int copy(void) {
    struct Pair source;
    struct Pair destination;
    source.tag = 3;
    source.value = 91;
    destination = source;
    return destination.tag + destination.value;
}
int main(void) { if (copy() == 94) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "struct-copy");
    assert_eq!(exit_code(&run_program(dir.path(), "struct-copy")), 0);
}

#[test]
fn anonymous_struct_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"typedef struct { int value; } Pair;
int read(void) { Pair pair; pair.value = 29; return pair.value; }
int main(void) { if (read() == 29) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "anonymous-struct");
    assert_eq!(exit_code(&run_program(dir.path(), "anonymous-struct")), 0);
}

#[test]
fn sizeof_struct_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("size.c"),
        "struct Pair { char tag; int value; }; int size(void) { return sizeof(struct Pair); }\n",
    )
    .unwrap();
    run_fcc(dir.path(), &["cc", "-c", "size.c", "-o", "size.o"]);
    compile_host_object(
        dir.path(),
        "int size(void); int main(void) { return size() == 8 ? 0 : 1; }\n",
        "host.o",
    );
    run_fcc(
        dir.path(),
        &["cc", "size.o", "host.o", "-o", "sizeof-struct"],
    );
    assert_eq!(exit_code(&run_program(dir.path(), "sizeof-struct")), 0);
}

#[test]
fn nested_struct_member_executes_through_driver() {
    if !cc_available() {
        return;
    }
    let source = r#"struct Inner { int value; };
struct Outer { char tag; struct Inner inner; };
int read(void) { struct Outer outer; outer.inner.value = 61; return outer.inner.value; }
int main(void) { if (read() == 61) return 0; return 1; }
"#;
    let dir = tempfile::tempdir().unwrap();
    compile_fcc(dir.path(), source, "nested-struct");
    assert_eq!(exit_code(&run_program(dir.path(), "nested-struct")), 0);
}

#[test]
fn local_record_initializer_follows_field_order() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { int left; int right; }; int main(void) { struct Pair pair = {11, 22}; return pair.left + pair.right - 33; }\n",
    );
}

#[test]
fn local_record_designated_initializer_selects_fields() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { int left; int right; }; int main(void) { struct Pair pair = {.right = 22, .left = 11}; return pair.left + pair.right - 33; }\n",
    );
}

#[test]
fn record_initializer_continues_after_designator() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { int left; int right; }; int main(void) { struct Pair pair = {.left = 11, 22}; return pair.left + pair.right - 33; }\n",
    );
}

#[test]
fn local_record_designated_initializer_overrides_earlier_value() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { int left; int right; }; int main(void) { struct Pair pair = {.left = 40, .left = 41, .left = 2}; return pair.left - 2; }\n",
    );
}

#[test]
fn local_union_initializer_uses_first_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "union Value { int integer; long wide; }; int main(void) { union Value value = {42}; return value.integer - 42; }\n",
    );
}

#[test]
fn local_union_designator_selects_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "union Value { int integer; long wide; }; int main(void) { union Value value = {.wide = 42}; return value.wide - 42; }\n",
    );
}

#[test]
fn later_union_designator_overrides_earlier_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "union Value { int integer; long wide; }; int main(void) { union Value value = {.integer = 11, .wide = 42}; return value.wide - 42; }\n",
    );
}

#[test]
fn reselected_union_member_discards_overwritten_initializers() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { int left; int right; }; union Value { struct Pair pair; long wide; }; int main(void) { union Value value = {.pair.left = 11, .wide = -1, .pair.right = 42}; return value.pair.left == 0 && value.pair.right == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn nested_record_initializer_zero_fills_fields() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Inner { int left; int right; }; struct Outer { int tag; struct Inner inner; }; int main(void) { struct Outer value = {7, {11}}; return value.tag == 7 && value.inner.left == 11 && value.inner.right == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn nested_anonymous_record_type_is_defined() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Holder { int tag; union { int number; char bytes[4]; } value; };\n\
         int main(void) { struct Holder holder = {0}; holder.value.number = 42;\n\
         return holder.value.number != 42; }\n",
    );
}

#[test]
fn chained_field_designator_selects_nested_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Inner { int left; int right; }; struct Outer { int tag; struct Inner inner; }; int main(void) { struct Outer value = {.inner.right = 42}; return value.tag == 0 && value.inner.left == 0 && value.inner.right == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn chained_field_and_index_designators_select_nested_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Row { int left; int right; }; struct Table { struct Row rows[2]; }; int main(void) { struct Table value = {.rows[1].right = 42}; return value.rows[0].right == 0 && value.rows[1].left == 0 && value.rows[1].right == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn nested_initializer_continues_after_chained_designator() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Inner { int left; int right; }; struct Outer { int tag; struct Inner inner; int tail; }; int main(void) { struct Outer value = {.inner.left = 11, 22}; return value.tag == 0 && value.inner.left == 11 && value.inner.right == 22 && value.tail == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn initialized_scalar_global_is_read_by_main() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host("int answer = 42; int main(void) { return answer - 42; }\n");
}

#[test]
fn tentative_scalar_global_is_zero_initialized() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host("int counter; int main(void) { return counter; }\n");
}

#[test]
fn initialized_global_array_uses_constant_data() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int values[3] = {11, 22, 9}; int main(void) { return values[0] + values[1] + values[2] - 42; }\n",
    );
}

#[test]
fn initialized_global_array_designators_select_elements() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int values[5] = {[3] = 30, [1] = 12}; int main(void) { return values[0] == 0 && values[1] == 12 && values[2] == 0 && values[3] == 30 && values[4] == 0 ? 0 : 1; }\n",
    );
}

#[test]
fn initialized_global_array_designator_infers_omitted_bound() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int values[] = {[4] = 42}; int main(void) { return sizeof(values) == 20 && values[4] == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn initialized_global_struct_uses_field_layout() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { char tag; int value; } pair = {3, 39}; int main(void) { return pair.tag + pair.value - 42; }\n",
    );
}

#[test]
fn initialized_global_struct_designators_select_fields() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { char tag; int value; } pair = {.value = 39, .tag = 3}; int main(void) { return pair.tag + pair.value - 42; }\n",
    );
}

#[test]
fn initialized_global_chained_designators_select_nested_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Row { int left; int right; }; struct Table { struct Row rows[2]; } value = {.rows[1].right = 42}; int main(void) { return value.rows[0].right == 0 && value.rows[1].left == 0 && value.rows[1].right == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn initialized_global_union_uses_first_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "union Value { int integer; long wide; } value = {42}; int main(void) { return value.integer - 42; }\n",
    );
}

#[test]
fn initialized_global_union_designator_selects_member() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "union Value { int integer; long wide; } value = {.wide = 42}; int main(void) { return value.wide - 42; }\n",
    );
}

#[test]
fn global_reselected_union_member_discards_overwritten_initializers() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "struct Pair { int left; int right; }; union Value { struct Pair pair; long wide; } value = {.pair.left = 11, .wide = -1, .pair.right = 42}; int main(void) { return value.pair.left == 0 && value.pair.right == 42 ? 0 : 1; }\n",
    );
}

#[test]
fn global_objects_respect_source_alignment() {
    if !cc_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("globals.c"),
        "char prefix = 1; long value = 42; int main(void) { return prefix + value - 43; }\n",
    )
    .unwrap();
    run_fcc(dir.path(), &["cc", "-c", "globals.c", "-o", "globals.o"]);
    let object = fs::read(dir.path().join("globals.o")).unwrap();
    let elf = tir::backend::binary::parse_elf(&object).unwrap();
    let value = elf
        .symbols
        .iter()
        .find(|symbol| symbol.name == "value")
        .unwrap();
    let data = elf
        .sections
        .iter()
        .find(|section| section.name == ".data")
        .unwrap();

    assert_eq!(value.value % 8, 0);
    assert!(data.addralign >= 8);
}

#[test]
fn global_object_assembly_emits_alignment() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("globals.c"),
        "char prefix = 1; long value = 42;\n",
    )
    .unwrap();
    run_fcc(dir.path(), &["cc", "-S", "globals.c", "-o", "globals.s"]);
    let assembly = fs::read_to_string(dir.path().join("globals.s")).unwrap();

    assert!(assembly.contains("\t.balign 8\nvalue:\n"));
}

#[test]
fn global_pointer_initializer_emits_a_relocation() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "int target = 42; int *pointer = &target; int main(void) { return *pointer - 42; }\n",
    );
}

#[test]
fn global_array_of_string_pointers_emits_relocations() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "char *values[2] = {\"first\", \"second\"};\n\
         int main(void) { return values[0][0] + values[1][0] - 'f' - 's'; }\n",
    );
}

#[test]
fn global_pointer_array_accepts_cast_string_literals() {
    if !cc_available() {
        return;
    }
    assert_fcc_matches_host(
        "unsigned char *values[2] = {(unsigned char *)\"a\", (unsigned char *)\"b\"};\n\
         int main(void) { return values[0][0] + values[1][0] - 'a' - 'b'; }\n",
    );
}

#[test]
fn global_pointer_assembly_uses_a_symbolic_directive() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("globals.c"),
        "int target = 42; int *pointer = &target;\n",
    )
    .unwrap();
    run_fcc(dir.path(), &["cc", "-S", "globals.c", "-o", "globals.s"]);
    let assembly = fs::read_to_string(dir.path().join("globals.s")).unwrap();

    assert!(assembly.contains("pointer:\n\t.quad target\n"));
}
