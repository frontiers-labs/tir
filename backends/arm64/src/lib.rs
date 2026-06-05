use tir::helpers::{dialect, operation};
use tir::{Any, Operation};

include!(concat!(env!("OUT_DIR"), "/arm64.rs"));

/// Parsed AArch64 target selection from `--march`/`--mcpu`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetConfig;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CpuModel {
    Generic,
    InOrder,
    OutOfOrder,
}

impl TargetConfig {
    /// Parse an AArch64 `--march`/`--mcpu` pair.
    pub fn parse(march: &str, mcpu: Option<&str>) -> Option<Self> {
        parse_march(march)?;
        if let Some(mcpu) = mcpu {
            parse_mcpu(mcpu)?;
        }
        Some(TargetConfig)
    }

    /// Canonical architecture name for diagnostics and target-specific behavior.
    pub fn canonical_name(&self) -> &'static str {
        "arm64"
    }
}

fn parse_march(march: &str) -> Option<()> {
    match normalize(march).as_str() {
        "arm64" | "aarch64" | "armv8" | "armv8a" | "armv8-a" => Some(()),
        _ => None,
    }
}

fn parse_mcpu(mcpu: &str) -> Option<CpuModel> {
    match normalize(mcpu).as_str() {
        "generic" | "generic-arm64" | "generic-aarch64" => Some(CpuModel::Generic),
        "generic-in-order" | "generic-inorder" | "in-order" | "inorder" => Some(CpuModel::InOrder),
        "generic-ooo" | "generic-out-of-order" | "ooo" | "out-of-order" => {
            Some(CpuModel::OutOfOrder)
        }
        _ => None,
    }
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase().replace('_', "-")
}

operation! {
    VirtualReturnOp {
        name: "vret",
        dialect: "arm64",
        operands: [value],
    }
}

dialect! {
    Arm64Dialect {
        name: "arm64",
        operations: [
            VirtualReturnOp,
            AddOp,
            SubOp,
            AddImmediateOp,
            SubImmediateOp,
            AndOp,
            OrOp,
            XorOp,
            LogicalShiftLeftVariableOp,
            LogicalShiftRightVariableOp,
            ArithmeticShiftRightVariableOp,
            CompareOp,
            LoadByteUnsignedOp,
            LoadHalfwordUnsignedOp,
            LoadWordUnsignedOp,
            LoadDoublewordOp,
            LoadByteSignedOp,
            LoadHalfwordSignedOp,
            LoadWordSignedOp,
            StoreByteOp,
            StoreHalfwordOp,
            StoreWordOp,
            StoreDoublewordOp,
            BranchImmediateOp,
            BranchLinkOp,
            BranchRegisterOp,
            BranchLinkRegOp,
            ReturnOp,
            BranchEqOp,
            BranchNotEqOp,
            BranchLessThanOp,
            BranchGreaterEqOp,
            BranchLowerUnsignedOp,
            BranchHigherOrSameUnsignedOp,
        ],
    }
}

fn lower_func_and_return_to_asm_symbol(
    context: &tir::Context,
    op: &tir::OperationRef,
    rewriter: &mut tir::Rewriter,
) -> Result<bool, tir::PassError> {
    use tir::attributes::{AttributeValue, RegisterAttr};
    use tir::builtin::{FuncOp, ReturnOp};

    if let Some(func) = op.as_op::<FuncOp>() {
        // asm.symbol regions require an explicit symbol_end terminator.
        let body = func.body();
        let has_symbol_end = body
            .op_ids()
            .last()
            .map(|id| context.get_op(*id).name == tir_be_common::SymbolEndOp::name())
            .unwrap_or(false);
        if !has_symbol_end {
            let mut b = tir::IRBuilder::new(body);
            b.insert(tir_be_common::SymbolEndOpBuilder::new(context).build());
        }

        let sym_name = func
            .attributes()
            .iter()
            .find(|a| a.name == "sym_name")
            .and_then(|a| match &a.value {
                AttributeValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "unknown".to_string());

        let arg_regs = func
            .body()
            .arguments()
            .iter()
            .map(|arg| {
                AttributeValue::Register(RegisterAttr::Virtual {
                    id: arg.id().number(),
                    class: Some("GPR".to_string()),
                })
            })
            .collect::<Vec<_>>();

        let lowered = tir_be_common::SymbolOpBuilder::new(context)
            .body(op.op().regions[0])
            .attr("name", AttributeValue::Str(sym_name))
            .attr("arg_regs", AttributeValue::Array(arg_regs))
            .build();
        rewriter.replace_op(op, &lowered)?;
        return Ok(true);
    }

    if let Some(ret) = op.as_op::<ReturnOp>() {
        let mut builder = VirtualReturnOpBuilder::new(context);
        if let Some(value) = ret.operands().first().copied() {
            builder = builder.value(value);
        }
        let lowered = builder.build();
        rewriter.replace_op(op, &lowered)?;
        return Ok(true);
    }

    Ok(false)
}

impl Arm64Dialect {
    pub fn get_asm_parser(&self) -> tir_be_common::AsmParser {
        tir_be_common::AsmParser::new(get_instruction_parsers())
    }
}

pub fn create_isel_pass(context: &tir::Context) -> tir_be_common::isel::InstructionSelectPass {
    tir_be_common::isel::InstructionSelectPass::new(get_isel_rules(context))
        .with_op_lowering(lower_func_and_return_to_asm_symbol)
}

/// The AArch64 frame base register (`x29`/`fp`), reserved from allocation and used
/// as the base for spill slots.
const FP: (&str, u16) = ("GPR", 29);

fn phys(reg: &(String, u16)) -> tir::attributes::AttributeValue {
    tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Physical {
        class: reg.0.clone(),
        index: reg.1,
    })
}

fn virt(value: u32, class: &str) -> tir::attributes::AttributeValue {
    tir::attributes::AttributeValue::Register(tir::attributes::RegisterAttr::Virtual {
        id: value,
        class: Some(class.to_string()),
    })
}

/// AArch64 register allocation target: the generated register file plus `str`/`ldr`
/// spill code and a `sub fp, fp, #frame` / `add fp, fp, #frame` prologue/epilogue.
pub struct Arm64RegAlloc;

impl tir_be_common::regalloc::TargetRegAlloc for Arm64RegAlloc {
    fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
        register_info()
    }

    fn frame_register(&self) -> (String, u16) {
        (FP.0.to_string(), FP.1)
    }

    fn emit_spill_store(
        &self,
        context: &tir::Context,
        value: u32,
        class: &str,
        frame: &(String, u16),
        offset: i64,
    ) -> Box<dyn Operation> {
        Box::new(
            StoreDoublewordOpBuilder::new(context)
                .attr("rt", virt(value, class))
                .attr("rn", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )
    }

    fn emit_spill_reload(
        &self,
        context: &tir::Context,
        value: u32,
        class: &str,
        frame: &(String, u16),
        offset: i64,
    ) -> Box<dyn Operation> {
        Box::new(
            LoadDoublewordOpBuilder::new(context)
                .attr("rt", virt(value, class))
                .attr("rn", phys(frame))
                .attr("imm", tir::attributes::AttributeValue::Int(offset))
                .build(),
        )
    }

    fn emit_prologue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        vec![Box::new(
            SubImmediateOpBuilder::new(context)
                .attr("rd", phys(&(FP.0.to_string(), FP.1)))
                .attr("rn", phys(&(FP.0.to_string(), FP.1)))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        )]
    }

    fn emit_epilogue(&self, context: &tir::Context, size: u32) -> Vec<Box<dyn Operation>> {
        vec![Box::new(
            AddImmediateOpBuilder::new(context)
                .attr("rd", phys(&(FP.0.to_string(), FP.1)))
                .attr("rn", phys(&(FP.0.to_string(), FP.1)))
                .attr("imm", tir::attributes::AttributeValue::Int(size as i64))
                .build(),
        )]
    }
}

pub fn create_regalloc_pass() -> tir_be_common::regalloc::RegisterAllocationPass {
    tir_be_common::regalloc::RegisterAllocationPass::new(Box::new(Arm64RegAlloc))
}

#[cfg(test)]
mod tests {
    use tir::{
        Context, IRBuilder, IRFormatter, Operation, PassManager,
        builtin::{FuncOp, IntegerType, ops},
    };
    use tir_be_common::AsmDialect;

    use crate::{Arm64Dialect, create_isel_pass, create_regalloc_pass};

    fn phys_of(op: &std::sync::Arc<tir::OpInstance>, name: &str) -> Option<(String, u16)> {
        use tir::attributes::{AttributeValue, RegisterAttr};
        op.attributes
            .iter()
            .find(|a| a.name == name)
            .and_then(|a| match &a.value {
                AttributeValue::Register(RegisterAttr::Physical { class, index }) => {
                    Some((class.clone(), *index))
                }
                _ => None,
            })
    }

    fn body_blocks_have_no_virtual(context: &Context, region_id: tir::RegionId) {
        use tir::attributes::{AttributeValue, RegisterAttr};
        for block in context.get_region(region_id).iter(context.clone()) {
            for op_id in block.op_ids() {
                let op = context.get_op(op_id);
                for attr in &op.attributes {
                    assert!(
                        !matches!(
                            attr.value,
                            AttributeValue::Register(RegisterAttr::Virtual { .. })
                        ),
                        "op {} still has a virtual register in attr {}",
                        op.name,
                        attr.name
                    );
                }
            }
        }
    }

    #[test]
    fn arm64_assembly_parser_rejects_fuzzer_crash_without_panicking() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();
        let arm64 = context.find_dialect::<Arm64Dialect>().unwrap();
        let parser = arm64.get_asm_parser();

        assert!(parser.parse_asm(&context, ".0").is_err());
    }

    #[test]
    fn arm64_add_lowers_to_arm64_add() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b) = (args[0].id(), args[1].id());
        let mut fb = IRBuilder::new(func.body());
        let add = ops::addi(&context, a, b, i64).build();
        let add_r = add.result();
        fb.insert(add);
        fb.insert(ops::r#return(&context, add_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        module.verify(&context).expect("invalid module");
        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should succeed");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(body, vec!["add", "vret", "symbol_end"]);

        let mut buf = String::new();
        let mut fmt = IRFormatter::new(&mut buf);
        module.print(&mut fmt).expect("print lowered module");
        assert!(
            !buf.contains("builtin"),
            "no builtin ops should remain:\n{buf}"
        );
    }

    #[test]
    fn arm64_multi_op_function_lowers_end_to_end() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let c = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b, c]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b, c) = (args[0].id(), args[1].id(), args[2].id());

        // t1 = a + b ; t2 = t1 - c ; t3 = t2 & a ; t4 = t3 | b ; return t4
        let mut fb = IRBuilder::new(func.body());
        let t1 = ops::addi(&context, a, b, i64).build();
        let t1r = t1.result();
        fb.insert(t1);
        let t2 = ops::subi(&context, t1r, c, i64).build();
        let t2r = t2.result();
        fb.insert(t2);
        let t3 = ops::andi(&context, t2r, a, i64).build();
        let t3r = t3.result();
        fb.insert(t3);
        let t4 = ops::ori(&context, t3r, b, i64).build();
        let t4r = t4.result();
        fb.insert(t4);
        fb.insert(ops::r#return(&context, t4r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.run(&context, context.get_op(module.id()))
            .expect("isel should succeed");

        let body: Vec<_> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert_eq!(body, vec!["add", "sub", "and", "orr", "vret", "symbol_end"]);
    }

    #[test]
    fn arm64_regalloc_assigns_abi_physical_registers() {
        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        let a = context.create_value(i64, None);
        let b = context.create_value(i64, None);
        let region = context.create_region();
        let block = context.create_block(vec![a, b]);
        region.add_block(block.id());

        let func = ops::func(&context, "demo", i64, Some(region.id())).build();
        let fbody = func.body();
        let args = fbody.arguments();
        let (a, b) = (args[0].id(), args[1].id());
        let mut fb = IRBuilder::new(func.body());
        let add = ops::addi(&context, a, b, i64).build();
        let add_r = add.result();
        fb.insert(add);
        fb.insert(ops::r#return(&context, add_r).build());

        let mut mb = IRBuilder::new(module.body());
        mb.insert(func);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.nest(FuncOp::name()).add_pass(create_isel_pass(&context));
        pm.add_pass(create_regalloc_pass());
        pm.run(&context, context.get_op(module.id()))
            .expect("isel + regalloc should succeed");

        // AAPCS64 pre-coloring: arg0 -> x0, arg1 -> x1, return value -> x0 (reusing x0
        // because arg0 is dead after the add).
        let add_op = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id))
            .find(|op| op.name == "add")
            .expect("the add must survive selection");

        assert_eq!(phys_of(&add_op, "rn"), Some(("GPR".to_string(), 0)));
        assert_eq!(phys_of(&add_op, "rm"), Some(("GPR".to_string(), 1)));
        assert_eq!(phys_of(&add_op, "rd"), Some(("GPR".to_string(), 0)));

        body_blocks_have_no_virtual(&context, region.id());
    }

    /// An AArch64 target with a tiny allocatable register file (x0..x4) so a handful
    /// of live values overflow it and exercise spilling; spill emission delegates to
    /// the real target.
    struct TinyArm64(crate::Arm64RegAlloc);

    impl tir_be_common::regalloc::TargetRegAlloc for TinyArm64 {
        fn register_info(&self) -> tir_be_common::regalloc::RegisterInfo {
            tir_be_common::regalloc::RegisterInfo {
                classes: &[tir_be_common::regalloc::RegClassInfo {
                    name: "GPR",
                    file: "GPR",
                    allocation_order: &[0, 1, 2, 3, 4],
                    caller_saved: &[0, 1, 2, 3, 4],
                    callee_saved: &[],
                    arguments: &[0, 1],
                    return_values: &[0],
                    reserved: &[29, 30, 31],
                }],
            }
        }
        fn frame_register(&self) -> (String, u16) {
            self.0.frame_register()
        }
        fn emit_spill_store(
            &self,
            c: &tir::Context,
            v: u32,
            class: &str,
            f: &(String, u16),
            o: i64,
        ) -> Box<dyn Operation> {
            self.0.emit_spill_store(c, v, class, f, o)
        }
        fn emit_spill_reload(
            &self,
            c: &tir::Context,
            v: u32,
            class: &str,
            f: &(String, u16),
            o: i64,
        ) -> Box<dyn Operation> {
            self.0.emit_spill_reload(c, v, class, f, o)
        }
        fn emit_prologue(&self, c: &tir::Context, s: u32) -> Vec<Box<dyn Operation>> {
            self.0.emit_prologue(c, s)
        }
        fn emit_epilogue(&self, c: &tir::Context, s: u32) -> Vec<Box<dyn Operation>> {
            self.0.emit_epilogue(c, s)
        }
    }

    #[test]
    fn arm64_regalloc_spills_under_high_register_pressure() {
        use crate::{AddOpBuilder, VirtualReturnOpBuilder, virt};

        let context = Context::with_default_dialects();
        context.register_dialect::<AsmDialect>();
        context.register_dialect::<Arm64Dialect>();

        let i64 = IntegerType::new(&context, 64);
        let module = ops::module(&context, None).build();

        // Build machine IR directly: an `asm.symbol` whose body produces 8
        // simultaneously-live values from the single argument, then chains them. The
        // tiny 5-register file forces spilling.
        let a_val = context.create_value(i64, None);
        let a = a_val.id().number();
        let region = context.create_region();
        let block = context.create_block(vec![a_val]);
        region.add_block(block.id());

        let mut bb = IRBuilder::new(context.get_block(block.id()));
        let mut producers = Vec::new();
        for _ in 0..8 {
            let v = context.create_value(i64, None).id().number();
            bb.insert(
                AddOpBuilder::new(&context)
                    .attr("rd", virt(v, "GPR"))
                    .attr("rn", virt(a, "GPR"))
                    .attr("rm", virt(a, "GPR"))
                    .build(),
            );
            producers.push(v);
        }
        let mut acc = producers[0];
        for &p in &producers[1..] {
            let s = context.create_value(i64, None).id().number();
            bb.insert(
                AddOpBuilder::new(&context)
                    .attr("rd", virt(s, "GPR"))
                    .attr("rn", virt(acc, "GPR"))
                    .attr("rm", virt(p, "GPR"))
                    .build(),
            );
            acc = s;
        }
        bb.insert(
            VirtualReturnOpBuilder::new(&context)
                .value(tir::ValueId::from_number(acc))
                .build(),
        );
        bb.insert(tir_be_common::SymbolEndOpBuilder::new(&context).build());

        let symbol = tir_be_common::SymbolOpBuilder::new(&context)
            .body(region.id())
            .attr(
                "name",
                tir::attributes::AttributeValue::Str("demo".to_string()),
            )
            .build();
        let mut mb = IRBuilder::new(module.body());
        mb.insert(symbol);
        mb.insert(ops::module_end(&context).build());

        let mut pm = PassManager::new();
        pm.add_pass(tir_be_common::regalloc::RegisterAllocationPass::new(
            Box::new(TinyArm64(crate::Arm64RegAlloc)),
        ));
        pm.run(&context, context.get_op(module.id()))
            .expect("register allocation should converge with spilling");

        body_blocks_have_no_virtual(&context, region.id());

        let names: Vec<&str> = context
            .get_region(region.id())
            .iter(context.clone())
            .next()
            .unwrap()
            .op_ids()
            .into_iter()
            .map(|id| context.get_op(id).name)
            .collect();
        assert!(
            names.contains(&"store_doubleword"),
            "expected spill stores, got {names:?}"
        );
        assert!(
            names.contains(&"load_doubleword"),
            "expected spill reloads, got {names:?}"
        );
        assert_eq!(
            names.first(),
            Some(&"sub_imm"),
            "the frame prologue (sub fp) should lead the block, got {names:?}"
        );
    }
}

#[cfg(test)]
mod target_parser_tests {
    use super::TargetConfig;

    #[test]
    fn accepts_arm64_aliases_and_generic_cpus() {
        assert_eq!(
            TargetConfig::parse("aarch64", Some("generic-in-order")).map(|c| c.canonical_name()),
            Some("arm64")
        );
        assert!(TargetConfig::parse("armv8-a", Some("generic-aarch64")).is_some());
    }

    #[test]
    fn rejects_unknown_march_or_cpu() {
        assert!(TargetConfig::parse("rv64im", None).is_none());
        assert!(TargetConfig::parse("arm64", Some("cortex-a710")).is_none());
    }
}
