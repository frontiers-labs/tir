#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

//! End-to-end host-compare tests: compile a program with `fcc` and with the
//! host `cc`, run both, and require identical exit status and output. LIT
//! cannot express this (no way to execute a produced file), so they live here.
//! Skipped when `cc` is unavailable.

use super::link_support::{assert_fcc_matches_host, cc_available, host_tests};

host_tests! {
    assert_fcc_matches_host:
    variadic_double_argument_matches_host_compiler => ("int printf(const char *format, ...); int main(void) { printf(\"%.1f\\n\", 1.5); return 0; }\n",);
    compares_program_with_host_compiler => (r#"int puts(const char *text);
int main(void) { puts("same output"); return 17; }
"#,);
    scalar_fibonacci_matches_host_compiler => (include_str!("corpus/scalar/fibonacci.c"));
    scalar_sieve_matches_host_compiler => (include_str!("corpus/scalar/sieve.c"));
    scalar_recursive_descent_matches_host_compiler => (include_str!("corpus/scalar/recursive_descent.c"));
    scalar_mixed_widths_matches_host_compiler => (include_str!("corpus/scalar/mixed_widths.c"));
    scalar_compound_assign_promotion_matches_host_compiler => (include_str!("corpus/scalar/compound_assign_promotion.c"));
    scalar_crc16_matches_host_compiler => (include_str!("corpus/scalar/crc16.c"));
    scalar_call_chain_matches_host_compiler => (include_str!("corpus/scalar/call_chain.c"));
    scalar_early_return_matches_host_compiler => (include_str!("corpus/scalar/early_return.c"));
    /// A flag merged out of both arms of an `if` inside a loop: promotion leaves an
    /// arm and the code after it holding identical constants, which must not be
    /// unified across the region boundary.
    scalar_loop_flag_merge_matches_host_compiler => (include_str!("corpus/scalar/loop_flag_merge.c"));
    /// `break` and `continue` keep their C meaning inside the `scf.while` a `for`
    /// or a `do` becomes: the step and the trailing condition still run on a
    /// `continue`, and a `break` skips both.
    scalar_loop_control_matches_host_compiler => (include_str!("corpus/scalar/loop_control.c"));
    /// `do` loops become `scf.while` with the condition appended to the body, which
    /// only holds while `break` and `continue` keep their meaning.
    do_while_control_matches_host_compiler => (r#"int printf(const char *format, ...);
int main(void) {
    int i = 0;
    int s = 0;
    int j = 0;
    do {
        i = i + 1;
        if (i == 3) {
            continue;
        }
        if (i == 7) {
            break;
        }
        s = s + i;
    } while (i < 10);
    printf("%d %d\n", i, s);
    do {
        j = j + 2;
    } while (j < 5);
    printf("%d\n", j);
    do {
        s = s + 1;
        if (s > 20) {
            break;
        }
    } while (1);
    printf("%d\n", s);
    return s & 63;
}
"#,);
    /// A returned aggregate is held in a slot until the single `return`, both when
    /// it travels in registers and when the caller supplies the storage.
    aggregate_early_return_matches_host_compiler => (r#"int printf(const char *format, ...);
struct Small { int a; int b; };
struct Big { int v[8]; };
struct Small pick(int n) {
    struct Small s;
    if (n > 0) {
        s.a = 1;
        s.b = n;
        return s;
    }
    s.a = -1;
    s.b = 0;
    return s;
}
struct Big fill(int n) {
    struct Big b;
    int i;
    for (i = 0; i < 8; i = i + 1) {
        b.v[i] = i * n;
        if (i == 5) {
            b.v[7] = 99;
            return b;
        }
    }
    return b;
}
int main(void) {
    struct Small s = pick(4);
    struct Small t = pick(-4);
    struct Big g = fill(2);
    printf("%d %d %d %d %d\n", s.a, s.b, t.a, t.b, g.v[7]);
    return s.b + t.a + g.v[3];
}
"#,);
    scalar_branch_mix_matches_host_compiler => (include_str!("corpus/scalar/branch_mix.c"));
    local_pointer_matches_host_compiler => (include_str!("corpus/memory/local_pointer.c"));
    memory_linked_list_matches_host_compiler => (include_str!("corpus/memory/linked_list.c"));
    memory_hash_table_matches_host_compiler => (include_str!("corpus/memory/hash_table.c"));
    /// A store through the caller's array from inside a branch, found by the
    /// nightly fuzzer: the store was dropped and the array kept its old value.
    memory_pointer_store_in_branch_matches_host_compiler => (include_str!("corpus/memory/pointer_store_in_branch.c"));
    global_function_pointer_matches_host_compiler => ("int add_five(int value) { return value + 5; } int (*function)(int) = add_five; int main(void) { return function(37) == 42 ? 0 : 1; }\n",);
    function_pointer_call_matches_host_compiler => ("int add_five(int value) { return value + 5; } int main(void) { int (*function)(int) = add_five; return function(37) == 42 ? 0 : 1; }\n",);
    function_pointer_parameter_matches_host_compiler => ("typedef int (*Unary)(int); int add_five(int value) { return value + 5; } int apply(Unary function, int value) { return function(value); } int main(void) { return apply(add_five, 37) == 42 ? 0 : 1; }\n",);
    dereferenced_function_pointer_call_matches_host_compiler => ("int add_five(int value) { return value + 5; } int main(void) { int (*function)(int) = add_five; return (*function)(37) == 42 ? 0 : 1; }\n",);
    returned_function_pointer_call_matches_host_compiler => ("typedef int (*Unary)(int); int add_five(int value) { return value + 5; } Unary choose(void) { return add_five; } int main(void) { return choose()(37) == 42 ? 0 : 1; }\n",);
    function_pointer_large_record_return_matches_host_compiler => ("struct Triple { long a; long b; long c; }; typedef struct Triple (*Maker)(long); struct Triple make(long value) { struct Triple result = {value, value + 1, value + 2}; return result; } int main(void) { Maker maker = make; struct Triple result = maker(39); return result.a == 39 && result.b == 40 && result.c == 41 ? 0 : 1; }\n",);
    returned_function_pointer_large_record_return_matches_host_compiler => ("struct Triple { long a; long b; long c; }; typedef struct Triple (*Maker)(long); struct Triple make(long value) { struct Triple result = {value, value + 1, value + 2}; return result; } Maker choose(void) { return make; } int main(void) { struct Triple result = choose()(39); return result.a == 39 && result.b == 40 && result.c == 41 ? 0 : 1; }\n",);
    function_pointer_large_record_argument_matches_host_compiler => ("struct Triple { long a; long b; long c; }; typedef long (*Reducer)(struct Triple); long sum(struct Triple value) { return value.a + value.b + value.c; } int main(void) { Reducer reducer = sum; struct Triple value = {13, 14, 15}; return reducer(value) == 42 ? 0 : 1; }\n",);
    local_array_storage_is_contiguous => ("int main(void) { int values[3]; values[0] = 11; values[1] = 22; values[2] = 37; return values[0] + values[1] + values[2] - 70; }\n",);
    local_array_decays_when_passed_to_function => ("int sum(int *values) { return values[0] + values[1] + values[2]; } int main(void) { int values[3]; values[0] = 11; values[1] = 22; values[2] = 37; return sum(values) - 70; }\n",);
    local_array_initializer_zero_fills_remainder => ("int main(void) { int values[3] = {11, 22}; return values[0] + values[1] + values[2] - 33; }\n",);
    local_array_designated_initializer_selects_elements => ("int main(void) { int values[5] = {[3] = 30, [1] = 12}; return values[0] == 0 && values[1] == 12 && values[2] == 0 && values[3] == 30 && values[4] == 0 ? 0 : 1; }\n",);
    array_initializer_continues_after_designator => ("int main(void) { int values[4] = {[1] = 12, 30}; return values[0] == 0 && values[1] == 12 && values[2] == 30 && values[3] == 0 ? 0 : 1; }\n",);
    local_array_initializer_infers_omitted_bound => ("int main(void) { int values[] = {11, 22, 37}; return sizeof(values) == 12 && values[2] == 37 ? 0 : 1; }\n",);
    local_array_designator_infers_omitted_bound => ("int main(void) { int values[] = {[4] = 42}; return sizeof(values) == 20 && values[4] == 42 ? 0 : 1; }\n",);
    nested_array_initializer_uses_row_major_storage => ("int main(void) { int values[2][3] = {{11, 22, 33}, {44, 55}}; return sizeof(values) == 24 && values[1][0] == 44 && values[1][2] == 0 ? 0 : 1; }\n",);
    nested_array_initializer_infers_outer_bound => ("int main(void) { int values[][2] = {{11, 22}, {33, 44}}; return sizeof(values) == 16 && values[1][1] == 44 ? 0 : 1; }\n",);
    bitwise_and_shifts_match_host_compiler => (r#"unsigned int bits(unsigned int a, unsigned int b) {
    return ((a & b) | (a ^ b)) << 2 >> 1;
}
int signed_shift(int value) { return value >> 3; }
int main(void) {
    if (bits(10, 12) != 28) return 1;
    if (signed_shift(16) != 2) return 2;
    return 0;
}
"#,);
    variable_shift_count_matches_host_compiler => (r#"unsigned int shift(unsigned int value, unsigned int count) {
    return value << count;
}
int main(void) {
    if (shift(3, 4) == 48) return 0;
    return 1;
}
"#,);
    character_constant_executes_through_driver => ("int value(void) { return 'A'; } int main(void) { if (value() != 65) return 1; return 0; }\n",);
    enum_constants_use_implicit_and_explicit_values => ("enum Color { Red, Green = 5, Blue }; int main(void) { return Red == 0 && Green == 5 && Blue == 6 ? 0 : 1; }\n",);
    enum_constants_evaluate_integer_constant_expressions => ("enum Value { Base = 3, Scaled = Base * 4 + 2, Negative = -1 }; int main(void) { return Scaled == 14 && Negative == -1 ? 0 : 1; }\n",);
    enum_constants_evaluate_shift_and_bitwise_expressions => ("enum Flags { Read = 1 << 0, Write = 1 << 1, Both = Read | Write, Masked = (Both ^ Read) & 3, High = 8 >> 1 }; int main(void) { return Both == 3 && Masked == 2 && High == 4 ? 0 : 1; }\n",);
    enum_constants_evaluate_remainder_and_unary_expressions => ("enum Value { Remainder = 17 % 5, Inverted = ~0, False = !1, True = !0 }; int main(void) { return Remainder == 2 && Inverted == -1 && False == 0 && True == 1 ? 0 : 1; }\n",);
    enum_constants_evaluate_comparison_logical_and_conditional_expressions => ("enum Decision { Lt = 2 < 3, Le = 3 <= 3, Gt = 4 > 3, Ge = 4 >= 4, Eq = 5 == 5, Ne = 5 != 6, Both = Lt && Eq, Either = 0 || Both, Pick = Either ? 9 : 1 / 0 }; int main(void) { return Lt && Le && Gt && Ge && Eq && Ne && Both && Either && Pick == 9 ? 0 : 1; }\n",);
    enum_constants_accept_character_constants => ("enum Character { Letter = 'A', Newline = '\\n' }; int main(void) { return Letter == 65 && Newline == 10 ? 0 : 1; }\n",);
    enum_constants_accept_sizeof_and_integer_casts => ("enum Value { IntSize = sizeof(int), LongSize = sizeof(long), Narrow = (unsigned char)258, Signed = (signed char)255, Truth = (_Bool)42 }; int main(void) { return IntSize == 4 && LongSize == 8 && Narrow == 2 && Signed == -1 && Truth == 1 ? 0 : 1; }\n",);
    enum_constants_accept_immediately_cast_floating_constants => ("enum Value { Truncated = (int)3.75 }; int main(void) { return Truncated == 3 ? 0 : 1; }\n",);
    tagged_enum_objects_execute_through_driver => ("enum Color { Red = 3 }; int main(void) { enum Color value = Red; return value == 3 ? 0 : 1; }\n",);
    local_enum_declarations_execute_through_driver => ("int main(void) { enum Local { Base = 2, Value = Base + 3 }; return Value == 5 ? 0 : 1; }\n",);
    local_enum_definition_can_declare_an_object => ("int main(void) { enum Local { Value = 5 } value = Value; return value == 5 ? 0 : 1; }\n",);
    escaped_character_constant_executes_through_driver => ("int value(void) { return '\\n'; } int main(void) { if (value() != 10) return 1; return 0; }\n",);
    logical_and_short_circuits_rhs => (r#"int logical_and(int lhs) {
    int rhs = 0;
    int result = lhs && ++rhs;
    return result * 10 + rhs;
}
int main(void) {
    if (logical_and(0) != 0) return 1;
    if (logical_and(1) != 11) return 2;
    return 0;
}
"#,);
    logical_or_short_circuits_rhs => (r#"int logical_or(int lhs) {
    int rhs = 0;
    int result = lhs || ++rhs;
    return result * 10 + rhs;
}
int main(void) {
    if (logical_or(0) != 11) return 1;
    if (logical_or(1) != 10) return 2;
    return 0;
}
"#,);
    conditional_operator_executes_only_selected_arm => (r#"int conditional(int condition) {
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
"#,);
    switch_dispatch_fallthrough_and_break_match_host => (r#"int classify(int value) {
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
"#,);
    switch_break_exits_nearest_scope => (r#"int accumulate(void) {
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
"#,);
    switch_default_can_fall_through_in_source_order => (r#"int classify(int value) {
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
"#,);
    switch_without_matching_case_preserves_state => (r#"int classify(int value) {
    int result = 4;
    switch (value) {
    case 1:
        result = 9;
    }
    return result;
}
int main(void) { return classify(2) == 4 ? 0 : 1; }
"#,);
    goto_and_labels_match_host_compiler => (r#"int sum_to(int limit) {
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
"#,);
    /// Every `goto` shape fcc must restructure: cleanup exits, two jumps to one
    /// label, exits from nested loops, a loop written out of a backward jump, an
    /// irreducible pair of labels, and jumps into a compound statement, a loop body
    /// and a `switch` body, as well as out of a `switch`.
    scalar_goto_shapes_match_host_compiler => (include_str!("corpus/scalar/goto_shapes.c"));
    goto_can_enter_a_loop_body => (r#"int count(int enter) {
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
"#,);
    goto_can_exit_nested_control_flow => (r#"int count(void) {
    int value = 0;
    while (1) {
        if (value == 3) goto done;
        value = value + 1;
    }
done:
    return value;
}
int main(void) { return count() == 3 ? 0 : 1; }
"#,);
    goto_reaches_a_label_after_return => (r#"int choose(int second) {
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
"#,);
    goto_reaches_a_nested_label_after_return => (r#"int choose(int second) {
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
"#,);
    unary_operators_match_host_compiler => (r#"int negate(int value) { return -value; }
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
"#,);
    comma_operator_matches_host_compiler => (r#"int comma_value(void) {
    int value = 0;
    return (value = 3, value + 4);
}
int main(void) {
    if (comma_value() == 7) return 0;
    return 1;
}
"#,);
    integer_casts_match_host_compiler => (r#"int truncate(int value) { return (unsigned char)value; }
long widen(int value) { return (long)value; }
unsigned long widen_unsigned(unsigned int value) { return (unsigned long)value; }
int main(void) {
    if (truncate(257) != 1) return 1;
    if ((int)(widen(-2) >> 32) != -1) return 2;
    if ((int)(widen_unsigned(7U) >> 32) != 0) return 3;
    return 0;
}
"#,);
    increment_operators_match_host_compiler => (r#"int increment_values(void) {
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
"#,);
    compound_assignments_match_host_compiler => (r#"int compound_assign(void) {
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
"#,);
    local_record_initializer_follows_field_order => ("struct Pair { int left; int right; }; int main(void) { struct Pair pair = {11, 22}; return pair.left + pair.right - 33; }\n",);
    local_record_designated_initializer_selects_fields => ("struct Pair { int left; int right; }; int main(void) { struct Pair pair = {.right = 22, .left = 11}; return pair.left + pair.right - 33; }\n",);
    record_initializer_continues_after_designator => ("struct Pair { int left; int right; }; int main(void) { struct Pair pair = {.left = 11, 22}; return pair.left + pair.right - 33; }\n",);
    local_record_designated_initializer_overrides_earlier_value => ("struct Pair { int left; int right; }; int main(void) { struct Pair pair = {.left = 40, .left = 41, .left = 2}; return pair.left - 2; }\n",);
    local_union_initializer_uses_first_member => ("union Value { int integer; long wide; }; int main(void) { union Value value = {42}; return value.integer - 42; }\n",);
    local_union_designator_selects_member => ("union Value { int integer; long wide; }; int main(void) { union Value value = {.wide = 42}; return value.wide - 42; }\n",);
    later_union_designator_overrides_earlier_member => ("union Value { int integer; long wide; }; int main(void) { union Value value = {.integer = 11, .wide = 42}; return value.wide - 42; }\n",);
    reselected_union_member_discards_overwritten_initializers => ("struct Pair { int left; int right; }; union Value { struct Pair pair; long wide; }; int main(void) { union Value value = {.pair.left = 11, .wide = -1, .pair.right = 42}; return value.pair.left == 0 && value.pair.right == 42 ? 0 : 1; }\n",);
    nested_record_initializer_zero_fills_fields => ("struct Inner { int left; int right; }; struct Outer { int tag; struct Inner inner; }; int main(void) { struct Outer value = {7, {11}}; return value.tag == 7 && value.inner.left == 11 && value.inner.right == 0 ? 0 : 1; }\n",);
    nested_anonymous_record_type_is_defined => ("struct Holder { int tag; union { int number; char bytes[4]; } value; };\n\
         int main(void) { struct Holder holder = {0}; holder.value.number = 42;\n\
         return holder.value.number != 42; }\n",);
    chained_field_designator_selects_nested_member => ("struct Inner { int left; int right; }; struct Outer { int tag; struct Inner inner; }; int main(void) { struct Outer value = {.inner.right = 42}; return value.tag == 0 && value.inner.left == 0 && value.inner.right == 42 ? 0 : 1; }\n",);
    chained_field_and_index_designators_select_nested_member => ("struct Row { int left; int right; }; struct Table { struct Row rows[2]; }; int main(void) { struct Table value = {.rows[1].right = 42}; return value.rows[0].right == 0 && value.rows[1].left == 0 && value.rows[1].right == 42 ? 0 : 1; }\n",);
    nested_initializer_continues_after_chained_designator => ("struct Inner { int left; int right; }; struct Outer { int tag; struct Inner inner; int tail; }; int main(void) { struct Outer value = {.inner.left = 11, 22}; return value.tag == 0 && value.inner.left == 11 && value.inner.right == 22 && value.tail == 0 ? 0 : 1; }\n",);
    initialized_scalar_global_is_read_by_main => ("int answer = 42; int main(void) { return answer - 42; }\n");
    tentative_scalar_global_is_zero_initialized => ("int counter; int main(void) { return counter; }\n");
    initialized_global_array_uses_constant_data => ("int values[3] = {11, 22, 9}; int main(void) { return values[0] + values[1] + values[2] - 42; }\n",);
    initialized_global_array_designators_select_elements => ("int values[5] = {[3] = 30, [1] = 12}; int main(void) { return values[0] == 0 && values[1] == 12 && values[2] == 0 && values[3] == 30 && values[4] == 0 ? 0 : 1; }\n",);
    initialized_global_array_designator_infers_omitted_bound => ("int values[] = {[4] = 42}; int main(void) { return sizeof(values) == 20 && values[4] == 42 ? 0 : 1; }\n",);
    initialized_global_struct_uses_field_layout => ("struct Pair { char tag; int value; } pair = {3, 39}; int main(void) { return pair.tag + pair.value - 42; }\n",);
    initialized_global_struct_designators_select_fields => ("struct Pair { char tag; int value; } pair = {.value = 39, .tag = 3}; int main(void) { return pair.tag + pair.value - 42; }\n",);
    initialized_global_chained_designators_select_nested_member => ("struct Row { int left; int right; }; struct Table { struct Row rows[2]; } value = {.rows[1].right = 42}; int main(void) { return value.rows[0].right == 0 && value.rows[1].left == 0 && value.rows[1].right == 42 ? 0 : 1; }\n",);
    initialized_global_union_uses_first_member => ("union Value { int integer; long wide; } value = {42}; int main(void) { return value.integer - 42; }\n",);
    initialized_global_union_designator_selects_member => ("union Value { int integer; long wide; } value = {.wide = 42}; int main(void) { return value.wide - 42; }\n",);
    global_reselected_union_member_discards_overwritten_initializers => ("struct Pair { int left; int right; }; union Value { struct Pair pair; long wide; } value = {.pair.left = 11, .wide = -1, .pair.right = 42}; int main(void) { return value.pair.left == 0 && value.pair.right == 42 ? 0 : 1; }\n",);
    global_pointer_initializer_emits_a_relocation => ("int target = 42; int *pointer = &target; int main(void) { return *pointer - 42; }\n",);
    global_array_of_string_pointers_emits_relocations => ("char *values[2] = {\"first\", \"second\"};\n\
         int main(void) { return values[0][0] + values[1][0] - 'f' - 's'; }\n",);
    global_pointer_array_accepts_cast_string_literals => ("unsigned char *values[2] = {(unsigned char *)\"a\", (unsigned char *)\"b\"};\n\
         int main(void) { return values[0][0] + values[1][0] - 'a' - 'b'; }\n",);
    /// Two mutually exclusive branches computing the same predicate produce two
    /// equal γ merges. Selection may not bind one block's argument register inside
    /// the other branch: that register holds a value only on the path it merges.
    duplicate_predicate_in_exclusive_branches_matches_host_compiler => (r#"int printf(const char *format, ...);
int run(char *s) {
    int st = 0;
    char *p = s;
    for (; *p && st != 1; p++) {
        char sym = *p;
        if (st == 0) {
            if (sym >= '0' && sym <= '9') st = 4;
            else if (sym == '.') st = 5;
            else st = 1;
        } else if (st == 5) {
            if (!(sym >= '0' && sym <= '9')) st = 1;
        }
    }
    return st * 100 + (int)(p - s);
}
int main(void) {
    printf("%d\n", run((char *)".500"));
    return 0;
}
"#,);
    /// A `switch` whose arms all leave one value: the e-graph folds the gate to that
    /// value, so the class holding it also holds the gate's own result. Selection must
    /// not answer the class with that result where the class is read inside the gate —
    /// an arm would then yield the name the gate publishes, which is its own future.
    /// This is the shape CoreMark's `core_init_state` dispatch has, and it decided the
    /// wrong case.
    switch_fallthrough_dispatch_matches_host_compiler => (r#"void run(unsigned n, short seed, int *out) {
    unsigned next = 0;
    int k = 0;
    while (k < (int)n) {
        out[k++] = (int)next;
        seed++;
        switch (seed & 3) {
            case 0:
            case 1: next = 4; break;
            case 2: next = 8; break;
            case 3: next = 888; break;
        }
    }
}
int main(void) {
    int out[16];
    run(12, 3, out);
    int sum = 0;
    for (int i = 0; i < 12; i++) sum = sum * 3 + out[i];
    return sum & 0x7f;
}
"#,);
}
