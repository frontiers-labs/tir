//! End-to-end codegen benchmark for the `fcc` C frontend: it lowers a
//! synthetic translation unit (many functions, each a chain of local
//! declarations over deep arithmetic) down to TIR. `codegen` measures the
//! AST → IR step in isolation; `pipeline` includes tokenizing and parsing.

#[macro_use]
#[path = "../../benchmarks/functions.rs"]
pub mod functions;

use std::fmt::Write;
use std::hint::black_box;
use std::time::Duration;

use logos::Logos;

use fcc::cir::CirDialect;
use fcc::codegen::codegen;
use fcc::diagnostics::{Span, intern_file};
use fcc::lexer::Token;
use fcc::parser::parse;
use fcc::passes::LowerCirStructsPass;
use fcc::sema::{TypedAst, analyze};
use tir::backend::TargetMachine;
use tir::backend::pipeline::{Oracles, StopAfter, build_pipeline};
use tir::builtin::ModuleOp;
use tir::func::FuncOp;
use tir::passes::{
    InstCombineNodesPass, MaterializeSymbolAddressesPass, PromoteNodesPass, RestructureNodesPass,
    VerifyDepsPass,
};
use tir::{Context, Operation, PassManager};

type IrInput = (Context, ModuleOp);
type BackendInput = (Context, ModuleOp, Box<dyn TargetMachine>);

const GCC_20011219_1: &str = r#"
extern void abort (void);
extern void exit (int);

enum X { A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P, Q };

void bar (const char *x, int y, const char *z)
{
}

long foo (enum X x, const void *y)
{
  long a;

  switch (x)
    {
    case K:
      a = *(long *)y;
      break;
    case L:
      a = *(long *)y;
      break;
    case M:
      a = *(long *)y;
      break;
    case N:
      a = *(long *)y;
      break;
    case O:
      a = *(long *)y;
      break;
    default:
      bar ("foo", 1, "bar");
    }
  return a;
}

int main ()
{
  long i = 24;
  if (foo (N, &i) != 24)
    abort ();
  exit (0);
}
"#;

/// Build a translation unit with `funcs` functions, each declaring `stmts`
/// locals over progressively deeper expressions before returning one.
fn gen_source(funcs: usize, stmts: usize) -> String {
    let mut src = String::new();
    for f in 0..funcs {
        write!(src, "int f{f}(int a, int b, int c) {{ ").unwrap();
        src.push_str("int t0 = a * b + c; ");
        for s in 1..stmts {
            write!(src, "int t{s} = t{} * a - b + t{} * c; ", s - 1, s / 2).unwrap();
        }
        writeln!(src, "return t{} + t0 * a; }}", stmts - 1).unwrap();
    }
    src
}

/// A fully-parenthesized balanced expression of the given depth (`2^depth`
/// leaves), rotating through the parameters and operators.
fn build_expr(depth: usize, n: &mut usize) -> String {
    if depth == 0 {
        let v = ["a", "b", "c"][*n % 3];
        *n += 1;
        return v.to_string();
    }
    let lhs = build_expr(depth - 1, n);
    let op = ["+", "-", "*"][*n % 3];
    let rhs = build_expr(depth - 1, n);
    format!("({lhs} {op} {rhs})")
}

/// Expression-dominated translation unit: a handful of functions, each a single
/// `return` over one huge arithmetic tree, so codegen time is almost entirely
/// expression lowering.
fn gen_expr_heavy(funcs: usize, depth: usize) -> String {
    let mut src = String::new();
    for f in 0..funcs {
        let mut n = f;
        let expr = build_expr(depth, &mut n);
        writeln!(src, "int g{f}(int a, int b, int c) {{ return {expr}; }}").unwrap();
    }
    src
}

fn parse_src(src: &str) -> TypedAst {
    let file = intern_file("<bench>", src);
    let tokens: Vec<_> = Token::lexer(src)
        .spanned()
        .map(|(r, span)| (r.unwrap(), Span::new(file, span.start)))
        .collect();
    let options = Default::default();
    let ast = parse(&tokens, options).expect("parse");
    analyze(ast, options).expect("sema")
}

fn lower_before_instcombine(ast: &TypedAst) -> IrInput {
    let context = Context::with_default_dialects();
    context.register_dialect::<CirDialect>();
    let module = codegen(&context, ast).unwrap();

    let mut pm = PassManager::new();
    pm.add_pass(LowerCirStructsPass::new());
    let function_pipeline = pm.nest::<FuncOp>();
    function_pipeline.add_pass(RestructureNodesPass::new());
    function_pipeline.add_pass(PromoteNodesPass::new());
    function_pipeline.add_pass(VerifyDepsPass::new());
    pm.run(&context, context.get_op(module.id())).unwrap();
    (context, module)
}

fn lower_before_isel(ast: &TypedAst) -> BackendInput {
    let (context, module) = lower_before_instcombine(ast);
    let target =
        tir::backend::select_target_with_abi("x86_64", None, None, None).expect("x86_64 target");
    target.register_dialects(&context);

    let mut pm = PassManager::new();
    let function_pipeline = pm.nest::<FuncOp>();
    function_pipeline.add_pass(InstCombineNodesPass::new());
    pm.add_pass(MaterializeSymbolAddressesPass::new());
    pm.run(&context, context.get_op(module.id())).unwrap();
    fcc::codegen::lower_data(&context, &module).unwrap();
    (context, module, target)
}

fn codegen_ir(ast: &TypedAst) -> IrInput {
    let ctx = Context::with_default_dialects();
    let module = codegen(&ctx, ast).unwrap();
    (ctx, module)
}

fn run_codegen(ast: &TypedAst) {
    let (_ctx, module) = codegen_ir(ast);
    black_box(module);
}

fn run_promote((ctx, module): IrInput) {
    let mut pm = tir::parse_pipeline(
        "func.func(restructure-nodes,promote-nodes),fixpoint<3>(func.func(instcombine-nodes))",
    )
    .unwrap();
    pm.run(&ctx, ctx.get_op(module.id())).unwrap();
}

fn run_pipeline(src: &str) {
    let ast = parse_src(src);
    run_codegen(&ast);
}

fn run_instcombine((context, module): IrInput) {
    let mut pm = PassManager::new();
    pm.nest::<FuncOp>().add_pass(InstCombineNodesPass::new());
    pm.run(&context, context.get_op(module.id())).unwrap();
}

fn run_backend((context, module, target): BackendInput, stop_after: StopAfter) {
    let mut pm = build_pipeline(target.as_ref(), &context, stop_after, Oracles::default());
    pm.run(&context, context.get_op(module.id())).unwrap();
}

fn gcc_settings() -> functions::Settings {
    functions::Settings {
        samples: Some(10),
        warmup: Some(Duration::from_secs(1)),
        measurement: Some(Duration::from_secs(5)),
        ..Default::default()
    }
}

benchmarks! {
    compiler = "fcc";
    bench_codegen("fcc/codegen/ast_to_ir") |b| {
        let ast = parse_src(&gen_source(50, 40));
        b.iter(|| run_codegen(&ast));
    }
    bench_codegen_expr_heavy("fcc/codegen_expr_heavy/ast_to_ir") |b| {
        let ast = parse_src(&gen_expr_heavy(20, 12));
        b.iter(|| run_codegen(&ast));
    }
    bench_promote("fcc/promote/promote") |b| {
        let ast = parse_src(&gen_source(50, 40));
        // Rebuild fresh IR because promotion replaces the locals' uses.
        b.iter_batched(|| codegen_ir(&ast), run_promote);
    }
    bench_pipeline("fcc/pipeline/source_to_ir") |b| {
        let src = gen_source(50, 40);
        b.iter(|| run_pipeline(&src));
    }
    bench_gcc_instcombine("fcc/gcc_20011219_1/instcombine", gcc_settings()) |b| {
        let ast = parse_src(GCC_20011219_1);
        b.iter_batched(|| lower_before_instcombine(&ast), run_instcombine);
    }
    bench_gcc_instruction_selection("fcc/gcc_20011219_1/instruction_selection", gcc_settings()) |b| {
        let ast = parse_src(GCC_20011219_1);
        b.iter_batched(|| lower_before_isel(&ast), |input| run_backend(input, StopAfter::ISel));
    }
    bench_gcc_backend_through_finalize("fcc/gcc_20011219_1/backend_through_finalize", gcc_settings()) |b| {
        let ast = parse_src(GCC_20011219_1);
        b.iter_batched(|| lower_before_isel(&ast), |input| run_backend(input, StopAfter::Finalize));
    }
}
