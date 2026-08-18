//! Data layout, target environment and scoped-attribute resolution.

use tir::{
    attributes::AttributeValue,
    builtin::{ops, FloatType, IntegerType, ModuleOp, UnitType},
    func::ops as func_ops,
    parse::ir::parse_ir,
    ptr::PtrType,
    scoped_dict, Context, DataLayout, Endianness, Operation, TargetEnv,
};

/// The layout of a module declaring `spec`.
fn layout(context: &Context, spec: &str) -> DataLayout {
    let src = format!("module {{data_layout = {spec}}} {{\n  module_end\n}}");
    let module = parse_ir::<ModuleOp>(context, &src).expect("parse module");
    DataLayout::for_op(context, module.id()).expect("layout in scope")
}

#[test]
fn endianness_is_read_from_the_spec() {
    let context = Context::with_default_dialects();

    let little = layout(&context, r#"{endianness = "little"}"#);
    let big = layout(&context, r#"{endianness = "big"}"#);

    assert_eq!(little.endianness(), Some(Endianness::Little));
    assert_eq!(big.endianness(), Some(Endianness::Big));
}

#[test]
fn stack_alignment_is_read_from_the_spec() {
    let context = Context::with_default_dialects();

    let layout = layout(&context, "{stack_alignment = 128}");

    assert_eq!(layout.stack_alignment(), Some(128));
}

#[test]
fn pointer_size_comes_from_the_pointer_entry() {
    let context = Context::with_default_dialects();

    let layout = layout(&context, "{types = {p = {size = 32, abi = 32}}}");

    assert_eq!(layout.pointer_size(), Some(32));
}

#[test]
fn a_class_layout_is_read_without_a_type() {
    let context = Context::with_default_dialects();

    let layout = layout(&context, "{types = {f64 = {size = 64, abi = 32}}}");

    assert_eq!(layout.class_layout("f64"), Some((64, 32)));
    assert_eq!(layout.class_layout("f32"), None);
}

#[test]
fn an_integer_size_defaults_to_its_own_width() {
    let context = Context::with_default_dialects();
    let i32_ty = IntegerType::new(&context, 32);

    let layout = layout(&context, "{types = {i32 = {abi = 32}}}");

    assert_eq!(layout.size_in_bits(&context, i32_ty), Some(32));
}

#[test]
fn a_declared_size_overrides_the_type_width() {
    let context = Context::with_default_dialects();
    let i1 = IntegerType::new(&context, 1);

    let layout = layout(&context, "{types = {i1 = {size = 8, abi = 8}}}");

    assert_eq!(layout.size_in_bits(&context, i1), Some(8));
}

#[test]
fn abi_alignment_is_read_per_type_class() {
    let context = Context::with_default_dialects();
    let i16_ty = IntegerType::new(&context, 16);
    let f64_ty = FloatType::f64(&context);
    let pointer = PtrType::opaque(&context);

    let layout = layout(
        &context,
        "{types = {i16 = {abi = 16}, f64 = {abi = 64}, p = {size = 64, abi = 64}}}",
    );

    assert_eq!(layout.abi_alignment(&context, i16_ty), Some(16));
    assert_eq!(layout.abi_alignment(&context, f64_ty), Some(64));
    assert_eq!(layout.abi_alignment(&context, pointer), Some(64));
}

#[test]
fn preferred_alignment_defaults_to_the_abi_alignment() {
    let context = Context::with_default_dialects();
    let i16_ty = IntegerType::new(&context, 16);

    let layout = layout(&context, "{types = {i16 = {abi = 16}}}");

    assert_eq!(layout.preferred_alignment(&context, i16_ty), Some(16));
}

#[test]
fn preferred_alignment_is_read_when_declared() {
    let context = Context::with_default_dialects();
    let i16_ty = IntegerType::new(&context, 16);

    let layout = layout(&context, "{types = {i16 = {abi = 16, preferred = 32}}}");

    assert_eq!(layout.preferred_alignment(&context, i16_ty), Some(32));
}

#[test]
fn an_undeclared_type_class_has_no_alignment() {
    let context = Context::with_default_dialects();
    let i128_ty = IntegerType::new(&context, 128);

    let layout = layout(&context, "{types = {i64 = {abi = 64}}}");

    assert_eq!(layout.abi_alignment(&context, i128_ty), None);
}

#[test]
fn entries_outside_the_predefined_set_stay_readable() {
    let context = Context::with_default_dialects();

    let layout = layout(&context, "{address_spaces = {global = 1}}");

    assert!(layout.get("address_spaces").is_some());
    assert!(layout.get("endianness").is_none());
}

#[test]
fn ir_entries_override_the_target_default_key_by_key() {
    let context = Context::with_default_dialects();
    let default = tir::data_layout_spec(Endianness::Big, 128, &[("i32", 32, 32), ("p", 64, 64)]);
    let src = r#"module {data_layout = {endianness = "little", types = {p = {size = 32, abi = 32}}}} {
  module_end
}"#;
    let module = parse_ir::<ModuleOp>(&context, src).expect("parse module");

    let layout = DataLayout::for_op_with_default(&context, module.id(), Some(&default))
        .expect("target default applies");

    // The module overrides byte order and the pointer entry; the i32 entry
    // and the stack alignment it never mentions come from the target.
    assert_eq!(layout.endianness(), Some(Endianness::Little));
    assert_eq!(layout.pointer_size(), Some(32));
    assert_eq!(
        layout.abi_alignment(&context, IntegerType::new(&context, 32)),
        Some(32)
    );
    assert_eq!(layout.stack_alignment(), Some(128));
}

#[test]
fn the_target_default_applies_where_the_ir_declares_nothing() {
    let context = Context::with_default_dialects();
    let default = tir::data_layout_spec(Endianness::Little, 64, &[("p", 64, 64)]);
    let module = parse_ir::<ModuleOp>(&context, "module {\n  module_end\n}").expect("parse");

    let layout = DataLayout::for_op_with_default(&context, module.id(), Some(&default))
        .expect("target default applies");

    assert_eq!(layout.pointer_size(), Some(64));
}

#[test]
fn a_target_default_needs_no_enclosing_scope() {
    let spec = AttributeValue::Dict(Box::new(
        [("stack_alignment".to_string(), AttributeValue::UInt(64))]
            .into_iter()
            .collect(),
    ));

    let layout = DataLayout::from_value(&spec).expect("spec is a dict");

    assert_eq!(layout.stack_alignment(), Some(64));
    assert!(DataLayout::from_value(&AttributeValue::UInt(64)).is_none());
}

/// The environment of a module declaring `spec`.
fn target_env(context: &Context, spec: &str) -> TargetEnv {
    let src = format!("module {{target_env = {spec}}} {{\n  module_end\n}}");
    let module = parse_ir::<ModuleOp>(context, &src).expect("parse module");
    TargetEnv::for_op(context, module.id()).expect("environment in scope")
}

#[test]
fn arch_and_cpu_are_read_from_the_spec() {
    let context = Context::with_default_dialects();

    let env = target_env(&context, r#"{arch = "riscv64", cpu = "sifive-u74"}"#);

    assert_eq!(env.arch(), Some("riscv64"));
    assert_eq!(env.cpu(), Some("sifive-u74"));
}

#[test]
fn an_absent_cpu_is_unknown() {
    let context = Context::with_default_dialects();

    let env = target_env(&context, r#"{arch = "arm64"}"#);

    assert_eq!(env.cpu(), None);
}

#[test]
fn enabled_features_are_queryable_by_name() {
    let context = Context::with_default_dialects();

    let env = target_env(&context, r#"{arch = "riscv64", features = ["m", "c"]}"#);

    assert!(env.has_feature("m"));
    assert!(env.has_feature("c"));
    assert!(!env.has_feature("v"));
}

#[test]
fn a_spec_without_features_enables_none() {
    let context = Context::with_default_dialects();

    let env = target_env(&context, r#"{arch = "riscv64"}"#);

    assert!(!env.has_feature("m"));
}

#[test]
fn target_entries_outside_the_predefined_set_stay_readable() {
    let context = Context::with_default_dialects();

    let env = target_env(&context, "{shared_memory = 65536}");

    assert_eq!(env.get("shared_memory"), Some(&AttributeValue::Int(65536)));
}

#[test]
fn a_target_description_needs_no_enclosing_scope() {
    let spec = AttributeValue::Dict(Box::new(
        [(
            "arch".to_string(),
            AttributeValue::Str("arm64".to_string().into()),
        )]
        .into_iter()
        .collect(),
    ));

    let env = TargetEnv::from_value(&spec).expect("spec is a dict");

    assert_eq!(env.arch(), Some("arm64"));
    assert!(TargetEnv::from_value(&AttributeValue::UInt(0)).is_none());
}

fn dict(entries: impl IntoIterator<Item = (&'static str, AttributeValue)>) -> AttributeValue {
    AttributeValue::Dict(Box::new(
        entries
            .into_iter()
            .map(|(name, value)| (name.to_string(), value))
            .collect(),
    ))
}

#[test]
fn nothing_is_in_scope_without_the_attribute() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None).build();

    assert!(scoped_dict(&context, module.id(), "data_layout").is_none());
}

#[test]
fn a_nested_op_reads_the_enclosing_scope() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None)
        .attr("data_layout", dict([("endianness", "little".into())]))
        .build();
    let nested = module.body().append_op(ops::module_end(&context).build());

    let resolved = scoped_dict(&context, nested.id(), "data_layout").expect("module scope");

    assert_eq!(resolved.get("endianness"), Some(&"little".into()));
}

#[test]
fn an_inner_scope_overrides_one_nested_entry() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None)
        .attr(
            "data_layout",
            dict([
                ("endianness", "little".into()),
                (
                    "types",
                    dict([
                        ("i32", dict([("abi", 32.into())])),
                        ("i64", dict([("abi", 64.into())])),
                    ]),
                ),
            ]),
        )
        .build();
    let func = module.body().append_op(
        func_ops::func(&context, "f", UnitType::new(&context), None)
            .attr(
                "data_layout",
                dict([("types", dict([("i32", dict([("abi", 8.into())]))]))]),
            )
            .build(),
    );

    let resolved = scoped_dict(&context, func.id(), "data_layout").expect("module scope");

    let AttributeValue::Dict(types) = &resolved["types"] else {
        panic!("types must stay a dict");
    };
    // The func overrides i32 only: i64 and the sibling endianness survive.
    assert_eq!(types["i32"], dict([("abi", 8.into())]));
    assert_eq!(types["i64"], dict([("abi", 64.into())]));
    assert_eq!(resolved.get("endianness"), Some(&"little".into()));
}

#[test]
fn an_inner_scope_replaces_an_array_entry() {
    let context = Context::with_default_dialects();
    let module = ops::module(&context, None)
        .attr(
            "target_env",
            dict([("features", vec!["m".into(), "a".into()].into())]),
        )
        .build();
    let nested = module.body().append_op(
        ops::module_end(&context)
            .attr("target_env", dict([("features", vec!["c".into()].into())]))
            .build(),
    );

    let resolved = scoped_dict(&context, nested.id(), "target_env").expect("module scope");

    assert_eq!(resolved["features"], vec!["c".into()].into());
}
