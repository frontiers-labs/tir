use tir_pdl::{Item, compile, compile_to_rust};

#[test]
fn compiles_instcombine_rules() {
    let source = include_str!("../../../core/src/passes/instcombine/rules.pdl");
    let file = compile(source).expect("instcombine rules should compile");
    let names: Vec<_> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Rule(rule) => Some(rule.name.as_str()),
            Item::Group(_) => None,
        })
        .collect();

    assert_eq!(
        names,
        [
            "add-zero",
            "mul-one",
            "mul-zero",
            "mul-pow2-to-shl",
            "sub-self",
            "gamma-true",
            "gamma-false",
            "gamma-same",
        ]
    );
}

#[test]
fn emits_executable_instcombine_rules() {
    let source = include_str!("../../../core/src/passes/instcombine/rules.pdl");
    let rust = compile_to_rust(source).expect("instcombine rules should lower to Rust");

    assert!(rust.contains("fn generated_ruleset"));
    assert!(rust.contains("Node::pattern::<crate::builtin::AddIOp>"));
    assert!(rust.contains("Node::introduced::<"));
    assert!(rust.contains("crate::builtin::ShlIOp"));
    assert!(!rust.contains("commutative_pattern"));
    assert!(!rust.contains("pattern_named"));
    assert!(!rust.contains("introduced_named"));
}

#[test]
fn diagnoses_unsupported_codegen_types() {
    let source = "group AnyInt = int<_>; rule bad: builtin.addi(x: AnyInt, 0) => x;";
    let diagnostics = compile_to_rust(source).expect_err("type groups are not lowered yet");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("type group"))
    );
}

#[test]
fn generated_bindings_do_not_shadow_rewrite_locals() {
    let source = "rule shadow-root: builtin.subi(root: int<W>, root) => const<W>(0);";
    let rust = compile_to_rust(source).expect("rule should lower to Rust");

    assert!(!rust.contains("let root = operand"));
    assert!(rust.contains("let binding_0 = operand"));
}

#[test]
fn distinct_rule_names_generate_distinct_functions() {
    let source = "rule a-b: builtin.addi(x: int<W>, 0) => x;\
                  rule a_b: builtin.addi(x: int<W>, 0) => x;";
    let rust = compile_to_rust(source).expect("rules should lower to Rust");

    assert!(rust.contains("fn pdl_rule_0"));
    assert!(rust.contains("fn pdl_rule_1"));
}

#[test]
fn generated_constant_constraints_check_the_width() {
    let source = "rule width: builtin.muli(x: int<W>, c: const<8>) => x;";
    let rust = compile_to_rust(source).expect("rule should lower to Rust");

    assert!(rust.contains("binding_1_value.width() != 8u32"));
}

#[test]
fn diagnoses_a_type_on_a_repeated_binder() {
    let source = "rule bad: builtin.subi(x: int<8>, x: int<16>) => x;";
    let diagnostics = compile_to_rust(source).expect_err("repeated binder type is ambiguous");

    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("repeated binder 'x'"))
    );
}

#[test]
fn generated_bit_counts_preserve_constant_width() {
    let source = "rule count: builtin.muli(x: int<W>, c: const) => x where clz(c) == 7;";
    let rust = compile_to_rust(source).expect("rule should lower to Rust");

    assert!(rust.contains("binding_1_value.count_leading_zeros()"));
}

#[test]
fn diagnoses_bit_counts_without_a_constant_operand() {
    let source = "rule bad: builtin.muli(x: int<W>, c: const) => x where clz(c + 1) == 6;";
    let diagnostics = compile_to_rust(source).expect_err("bit count width would be ambiguous");

    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("bit-count function requires a constant binder")
    }));
}

#[test]
fn diagnoses_integer_overflow() {
    let source = "rule large: builtin.addi(x: int<64>, 18446744073709551615) => x;";
    let diagnostics = compile(source).expect_err("out-of-range integer should be rejected");

    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("integer literal is out of range")
    }));
}

#[test]
fn diagnoses_invalid_constant_width() {
    let source = "rule bad: builtin.muli(x: int<W>, c: const<65>) => x;";
    let diagnostics = compile_to_rust(source).expect_err("APInt width should be rejected");

    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("constant width must be between 1 and 64")
    }));
}

#[test]
fn generated_constants_guard_dynamic_widths() {
    let source = "rule zero: builtin.subi(x: int<W>, x) => const<W>(0);";
    let rust = compile_to_rust(source).expect("rule should lower to Rust");

    assert!(rust.contains("if !(1..=64).contains(&replacement_width)"));
}

#[test]
fn diagnoses_boolean_expression_in_a_constant() {
    let source = "rule bad: builtin.muli(x: int<W>, c: const) => const<W>(!c);";
    let diagnostics =
        compile_to_rust(source).expect_err("boolean constant expression should be rejected");

    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("boolean expression cannot be used as a number")
    }));
}

#[test]
fn diagnoses_integer_type_width_overflow() {
    let source = "rule bad: builtin.addi(x: int<4294967296>, 0) => x;";
    let diagnostics = compile(source).expect_err("out-of-range type width should be rejected");

    assert!(diagnostics.iter().any(|diagnostic| {
        diagnostic
            .message
            .contains("integer type width is out of range")
    }));
}
