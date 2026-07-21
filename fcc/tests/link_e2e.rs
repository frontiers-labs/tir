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
