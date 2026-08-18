#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

//! Mixed-object ABI tests: compile one translation unit with `fcc`, its
//! counterpart with the host `cc`, link them together and run the result, so
//! every argument-passing and return convention is checked against the host
//! ABI from both sides of the call. Skipped when `cc` is unavailable.

use super::link_support::{assert_fcc_object_executes_with_host, cc_available};

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
fn many_escaping_locals_keep_distinct_addresses_across_a_call() {
    if !cc_available() {
        return;
    }
    assert_fcc_object_executes_with_host(
        "void observe(int *a, int *b, int *c, int *d, int *e, int *f, int *g, int *h)\n\
         { *a += 1; *b += 2; *c += 3; *d += 4; *e += 5; *f += 6; *g += 7; *h += 8; }\n\
         int sum_locals(void) {\n\
             int a = 1, b = 2, c = 3, d = 4, e = 5, f = 6, g = 7, h = 8;\n\
             observe(&a, &b, &c, &d, &e, &f, &g, &h);\n\
             return a + b + c + d + e + f + g + h;\n\
         }\n",
        "int sum_locals(void); int main(void) { return sum_locals() == 72 ? 0 : 1; }\n",
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
