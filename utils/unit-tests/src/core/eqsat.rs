use tir::{
    builtin::{ops, ModuleOp},
    func::FuncOp,
    passes::{eqsat::add_zero_rewrites, EqSatPass},
    AnalysisManager, Context, OpId, Operation, OperationRef, Pass, PassError, PassManager, Symbol,
};

use super::fixtures::{module_ops, parse_in};

#[allow(dead_code, unused_imports)]
mod test_ops {
    include!(concat!(env!("OUT_DIR"), "/eqsat_ops.rs"));
}

#[allow(dead_code)]
mod rules {
    include!(concat!(env!("OUT_DIR"), "/eqsat_rules.rs"));
}

use test_ops::{DeferredAddOp, DeferredXOrOp, KeepLeftOp};

tir::dialect! {
    RewriteTestDialect {
        name: "test",
        operations: [KeepLeftOp, DeferredAddOp, DeferredXOrOp],
        types: [],
    }
}

struct LowerTestArithmetic;

impl Pass for LowerTestArithmetic {
    fn name(&self) -> &'static str {
        "lower-test-arithmetic"
    }

    fn run(&mut self, root: OpId, context: &Context, _: &AnalysisManager) -> Result<(), PassError> {
        let function = context.get_op(root).as_op::<FuncOp>().unwrap();
        for id in function.body_region().op_ids() {
            let op = context.get_op(id);
            let is_add = op.is::<DeferredAddOp>();
            if !is_add && !op.is::<DeferredXOrOp>() {
                continue;
            }
            let operands = op.operands();
            let ty = context.get_value(op.results()[0]).ty();
            let replacement: Box<dyn Operation> = if is_add {
                Box::new(ops::addi(context, operands[0], operands[1], ty).build())
            } else {
                Box::new(ops::xori(context, operands[0], operands[1], ty).build())
            };
            context.replace_op(&OperationRef::new(op), replacement.as_ref())?;
        }
        Ok(())
    }
}

fn fixture() -> (Context, ModuleOp) {
    let context = Context::with_default_dialects();
    context.register_dialect::<RewriteTestDialect>();
    let module = parse_in(
        &context,
        r#"module {
            func.func @early(%arg: !i8) -> !i8 {
                %x = freeze %arg : !i8
                %from_extra = test.keep_left %x, %x : !i8
                %from_builtin = xori %x, %x : !i8
                %result = addi %from_extra, %from_builtin : !i8
                func.return %result
            }
            func.func @late(%arg: !i8) -> !i8 {
                %x = freeze %arg : !i8
                %zero = constant {value = 0} : !i8
                %from_exclusive = test.deferred_add %x, %zero : !i8
                %from_builtin = test.deferred_xor %x, %x : !i8
                %result = addi %from_exclusive, %from_builtin : !i8
                func.return %result
            }
            module_end
        }"#,
    );
    (context, module)
}

fn count<T: Operation>(context: &Context, module: &ModuleOp, function: &str) -> usize {
    let func = module_ops(context, module.id())
        .into_iter()
        .filter_map(|id| context.get_op(id).as_op::<FuncOp>())
        .find(|op| op.symbol_name() == function)
        .unwrap();
    func.body_region()
        .op_ids()
        .into_iter()
        .filter(|&id| context.get_op(id).is::<T>())
        .count()
}

#[test]
fn lowering_separates_additive_and_exclusive_rewrite_collections() {
    let (context, module) = fixture();
    let mut baseline = PassManager::new();
    baseline
        .nest::<FuncOp>()
        .add_pass(EqSatPass::new())
        .add_pass(LowerTestArithmetic);
    baseline.run(&context, module.id()).unwrap();
    assert_eq!(count::<KeepLeftOp>(&context, &module, "early"), 1);
    assert_eq!(count::<ops::AddIOp>(&context, &module, "late"), 2);
    assert_eq!(count::<ops::XOrIOp>(&context, &module, "late"), 1);

    let (context, module) = fixture();
    let mut pipeline = PassManager::new();
    pipeline
        .nest::<FuncOp>()
        .add_pass(EqSatPass::with_extra_rules([rules::keep_left_rewrites()]))
        .add_pass(LowerTestArithmetic)
        .add_pass(EqSatPass::with_ruleset(add_zero_rewrites()));
    pipeline.run(&context, module.id()).unwrap();
    tir::check_types(&context).unwrap();
    assert_eq!(count::<KeepLeftOp>(&context, &module, "early"), 0);
    assert_eq!(count::<ops::XOrIOp>(&context, &module, "early"), 0);
    assert_eq!(count::<ops::AddIOp>(&context, &module, "late"), 1);
    assert_eq!(count::<ops::XOrIOp>(&context, &module, "late"), 1);

    let mut defaults = PassManager::new();
    defaults.nest::<FuncOp>().add_pass(EqSatPass::new());
    defaults.run(&context, module.id()).unwrap();
    assert_eq!(count::<ops::XOrIOp>(&context, &module, "late"), 0);
}

fn assert_scoped_scalars(db: &mut tir_egglog::Database) {
    let table_count = db.table_count();
    assert_eq!(rules::scoped_value(db, 7).unwrap(), 8);
    assert_eq!(rules::scoped_value(db, 2).unwrap(), 4);
    assert_eq!(rules::scoped_value(db, 7).unwrap(), 8);
    assert_eq!(rules::scoped_pair(db, 7).unwrap(), (8, 2));
    assert_eq!(rules::scoped_projection(db, 7, true).unwrap(), 2);
    assert_eq!(rules::scoped_projection(db, 7, false).unwrap(), 4);
    assert_eq!(db.table_count(), table_count);
    let offset = rules::scoped_offset(db, 7).unwrap();
    assert!(offset.contains(db, 4, true).unwrap());
    assert!(!offset.contains(db, 4, false).unwrap());
    assert!(offset.contains(db, 7, false).unwrap());
    assert!(!offset.contains(db, 7, true).unwrap());
    assert!(!offset.contains(db, i64::MAX, false).unwrap());
}

#[test]
fn scoped_values_and_queries_use_their_captured_inputs() {
    use tir_egglog::Color;

    let context = Context::new();
    let mut engine = context.egraph();
    let db = engine.database_mut();
    rules::install_schema(db).unwrap();
    assert_scoped_scalars(db);

    rules::bind_eqsat_attr_int(db, |_, _| None).unwrap();
    for term in ["ProbeTerm::Original", "ProbeTerm::Input"] {
        let constructor = db.table(term).unwrap();
        db.entry(constructor, &[]).unwrap();
    }
    let mut program = rules::Program::load(db).unwrap();
    let original = program.lookup_probe_term_original().unwrap().unwrap();
    let input = program.lookup_probe_term_input().unwrap().unwrap();
    program.insert_probe_seen((original,)).unwrap();
    program.insert_probe_alternate((input,)).unwrap();
    let color = program.color(&[Color::BASE]);

    let held = rules::scoped_query_in(db, color).unwrap();
    assert!(held.contains(db, original, true).unwrap());
    assert!(!held.contains(db, original, false).unwrap());
    assert!(!held.contains(db, input, true).unwrap());
    assert!(held.contains(db, input, false).unwrap());
    let mut program = rules::Program::load(db).unwrap();
    program.union_in(color, original, input).unwrap();

    let fresh = rules::scoped_query_in(db, color).unwrap();
    assert!(fresh.contains(db, input, true).unwrap());
    assert!(fresh.contains(db, original, false).unwrap());
    assert!(!held.contains(db, input, true).unwrap());
    assert!(!held.contains(db, original, false).unwrap());
    assert!(held.contains(db, original, true).unwrap());
    assert!(held.contains(db, input, false).unwrap());
    let (retained,) = held.rows(db).unwrap().next().unwrap().unwrap();
    assert!(matches!(retained, rules::ProbeTerm::Original));
}
