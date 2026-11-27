//! InstCombine as a round of the mid-end pipeline: what it extracts from its
//! own output must leave that output alone.

use tir::{func::FuncOp, Operation, PassManager};

use super::fixtures;

// Functions that need the same labels at different types and in different
// orders, so whichever runs first the pass has met labels the next one
// needs, in an order that function would not have met them in.
const SHARED_LABELS: [&str; 4] = [
    r#"  %f0 = func.func @telescope(%x: !i32, %y: !i32, %z: !i32) -> !i32 {
    %a = subi %x, %y : !i32
    %b = subi %y, %z : !i32
    %result = addi %a, %b : !i32
    func.return %result
  }
"#,
    r#"  %f1 = func.func @scaled(%x: !i64, %y: !i64) -> !i64 {
    %one = constant {value = 1} : !i64
    %four = constant {value = 4} : !i64
    %scale = muli %one, %four : !i64
    %wide = muli %x, %scale : !i64
    %sum = addi %wide, %y : !i64
    %back = subi %sum, %y : !i64
    func.return %back
  }
"#,
    r#"  %f2 = func.func @mixed(%x: !i32, %w: !i64) -> !i32 {
    %narrow = trunci %w : !i32
    %again = trunci %w : !i32
    %sum = addi %narrow, %x : !i32
    %swapped = addi %x, %again : !i32
    %both = muli %sum, %swapped : !i32
    %zero = constant {value = 0} : !i32
    %kept = addi %both, %zero : !i32
    func.return %kept
  }
"#,
    r#"  %f3 = func.func @masked(%x: !i8, %y: !i8) -> !i8 {
    %both = andi %x, %y : !i8
    %either = ori %x, %y : !i8
    %diff = xori %both, %either : !i8
    %sum = addi %diff, %both : !i8
    %back = subi %sum, %both : !i8
    func.return %back
  }
"#,
];

// `text` with its values renamed in the order they first appear: a value's
// printed name is its id, which says where in the module it was minted.
fn renamed(text: &str) -> String {
    let mut names: Vec<&str> = Vec::new();
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find('%') {
        let digits = rest[at + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len() - at - 1);
        let name = &rest[at + 1..at + 1 + digits];
        let known = names.iter().position(|seen| *seen == name);
        let index = known.unwrap_or_else(|| {
            names.push(name);
            names.len() - 1
        });
        out.push_str(&rest[..at]);
        out.push_str(&format!("%{index}"));
        rest = &rest[at + 1 + digits..];
    }
    out + rest
}

// `SHARED_LABELS` after instcombine with the module's functions in `order`
// and `workers` at once: the module as printed, and each function by name
// with its values renamed.
fn simplified(order: [usize; 4], workers: usize) -> (String, Vec<String>) {
    let functions: String = order.iter().map(|&at| SHARED_LABELS[at]).collect();
    let (context, module) = fixtures::parse(&format!("module {{\n{functions}  module_end\n}}"));
    let mut pm = PassManager::new();
    pm.set_workers(workers);
    pm.nest::<FuncOp>()
        .add_pass(tir::passes::RestructureNodesPass::new())
        .add_pass(tir::passes::InstCombineNodesPass::new());
    pm.run(&context, module.id())
        .expect("the functions simplify");
    let print = |op: &dyn Fn(&mut tir::IRFormatter<'_>)| {
        let mut out = String::new();
        op(&mut tir::IRFormatter::new(&mut out));
        out
    };
    let mut functions: Vec<String> = fixtures::module_ops(&context, module.id())
        .into_iter()
        .filter_map(|op| context.get_op(op).as_op::<FuncOp>())
        .map(|function| {
            renamed(&print(&|fmt| {
                tir::print_ir(&function, &context, fmt).unwrap()
            }))
        })
        .collect();
    functions.sort();
    let module = print(&|fmt| tir::print_ir(&module, &context, fmt).unwrap());
    (module, functions)
}

#[test]
fn a_function_simplifies_alike_wherever_it_comes_and_however_many_run_at_once() {
    let (_, alone) = simplified([0, 1, 2, 3], 1);
    assert_eq!(alone.len(), 4);
    for order in [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1]] {
        let (module, functions) = simplified(order, 1);
        assert_eq!(functions, alone, "{order:?}");
        for workers in [2, 4] {
            assert_eq!(
                simplified(order, workers),
                (module.clone(), functions.clone())
            );
        }
    }
}
