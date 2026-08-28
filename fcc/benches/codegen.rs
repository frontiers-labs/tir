//! End-to-end codegen benchmark for the `fcc` C frontend: it lowers a
//! synthetic translation unit (many functions, each a chain of local
//! declarations over deep arithmetic) down to TIR. `codegen` measures the
//! AST → IR step in isolation; `pipeline` includes tokenizing and parsing.

use std::fmt::Write;
use std::hint::black_box;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use logos::Logos;

use fcc::cir::CirDialect;
use fcc::codegen::codegen;
use fcc::diagnostics::{Span, intern_file};
use fcc::lexer::Token;
use fcc::parser::parse;
use fcc::passes::LowerCirStructsPass;
use fcc::sema::{TypedAst, analyze};
use tir::backend::TargetMachine;
use tir::backend::pipeline::{StopAfter, build_pipeline};
use tir::func::FuncOp;
use tir::passes::{EraseStatePass, InstCombinePass, PromotePass, RestructurePass, ThreadStatePass};
use tir::{Context, Operation, PassManager};

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

fn lower_before_instcombine(ast: &TypedAst) -> (Context, tir::builtin::ModuleOp) {
    let context = Context::with_default_dialects();
    context.register_dialect::<CirDialect>();
    let module = codegen(&context, ast).unwrap();

    let mut pm = PassManager::new();
    pm.add_pass(LowerCirStructsPass::new());
    let function_pipeline = pm.nest::<FuncOp>();
    function_pipeline.add_pass(RestructurePass::new());
    function_pipeline.add_pass(PromotePass::new());
    function_pipeline.add_pass(ThreadStatePass::new());
    pm.run(&context, context.get_op(module.id())).unwrap();
    (context, module)
}

fn lower_before_isel(ast: &TypedAst) -> (Context, tir::builtin::ModuleOp, Box<dyn TargetMachine>) {
    let (context, module) = lower_before_instcombine(ast);
    let target =
        tir::backend::select_target_with_abi("x86_64", None, None, None).expect("x86_64 target");
    target.register_dialects(&context);

    let mut pm = PassManager::new();
    let function_pipeline = pm.nest::<FuncOp>();
    function_pipeline.add_pass(InstCombinePass::new());
    function_pipeline.add_pass(EraseStatePass::new());
    pm.run(&context, context.get_op(module.id())).unwrap();
    fcc::codegen::lower_data(&context, &module).unwrap();
    (context, module, target)
}

fn bench_codegen(c: &mut Criterion) {
    let src = gen_source(50, 40);
    let ast = parse_src(&src);

    let mut group = c.benchmark_group("fcc/codegen");
    group.bench_function("ast_to_ir", |b| {
        b.iter(|| {
            let ctx = Context::with_default_dialects();
            black_box(codegen(&ctx, &ast).unwrap());
        });
    });
    group.finish();
}

fn bench_codegen_expr_heavy(c: &mut Criterion) {
    let src = gen_expr_heavy(20, 12);
    let ast = parse_src(&src);

    let mut group = c.benchmark_group("fcc/codegen_expr_heavy");
    group.bench_function("ast_to_ir", |b| {
        b.iter(|| {
            let ctx = Context::with_default_dialects();
            black_box(codegen(&ctx, &ast).unwrap());
        });
    });
    group.finish();
}

/// Run the promotion path over the decl-heavy unit. fcc lowers locals to
/// alloca/load/store, so promotion is replace-uses heavy; `iter_batched` rebuilds
/// fresh IR per run so only the passes are timed.
fn bench_promote(c: &mut Criterion) {
    let src = gen_source(50, 40);
    let ast = parse_src(&src);

    let mut group = c.benchmark_group("fcc/promote");
    group.bench_function("promote", |b| {
        b.iter_batched(
            || {
                let ctx = Context::with_default_dialects();
                let module = codegen(&ctx, &ast).unwrap();
                (ctx, module)
            },
            |(ctx, module)| {
                let mut pm =
                    tir::parse_pipeline("func.func(promote,thread-state,instcombine,erase-state)")
                        .unwrap();
                pm.run(&ctx, ctx.get_op(module.id())).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn bench_pipeline(c: &mut Criterion) {
    let src = gen_source(50, 40);

    let mut group = c.benchmark_group("fcc/pipeline");
    group.bench_function("source_to_ir", |b| {
        b.iter(|| {
            let ast = parse_src(&src);
            let ctx = Context::with_default_dialects();
            black_box(codegen(&ctx, &ast).unwrap());
        });
    });
    group.finish();
}

fn bench_gcc_20011219_1(c: &mut Criterion) {
    let ast = parse_src(GCC_20011219_1);
    let mut group = c.benchmark_group("fcc/gcc_20011219_1");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(5));

    group.bench_function("instcombine", |b| {
        b.iter_batched(
            || lower_before_instcombine(&ast),
            |(context, module)| {
                let mut pm = PassManager::new();
                pm.nest::<FuncOp>().add_pass(InstCombinePass::new());
                pm.run(&context, context.get_op(module.id())).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("instruction_selection", |b| {
        b.iter_batched(
            || lower_before_isel(&ast),
            |(context, module, target)| {
                let mut pm = build_pipeline(target.as_ref(), &context, StopAfter::ISel);
                pm.run(&context, context.get_op(module.id())).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
    group.bench_function("backend_through_finalize", |b| {
        b.iter_batched(
            || lower_before_isel(&ast),
            |(context, module, target)| {
                let mut pm = build_pipeline(target.as_ref(), &context, StopAfter::Finalize);
                pm.run(&context, context.get_op(module.id())).unwrap();
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_codegen,
    bench_codegen_expr_heavy,
    bench_promote,
    bench_pipeline,
    bench_gcc_20011219_1
);
criterion_main!(benches);
