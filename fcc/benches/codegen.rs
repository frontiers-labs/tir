//! End-to-end codegen benchmark for the `fcc` C frontend: it lowers a
//! synthetic translation unit (many functions, each a chain of local
//! declarations over deep arithmetic) down to TIR. `codegen` measures the
//! AST → IR step in isolation; `pipeline` includes tokenizing and parsing.

use std::fmt::Write;
use std::hint::black_box;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use logos::Logos;

use fcc::ast::Ast;
use fcc::codegen::codegen;
use fcc::diagnostics::{Span, intern_file};
use fcc::lexer::Token;
use fcc::parser::parse;
use tir::{Context, Operation};

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

fn parse_src(src: &str) -> Ast {
    let file = intern_file("<bench>", src);
    let tokens: Vec<_> = Token::lexer(src)
        .spanned()
        .map(|(r, span)| (r.unwrap(), Span::new(file, span.start)))
        .collect();
    parse(&tokens).expect("parse")
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

/// Run mem2reg over the decl-heavy unit. fcc lowers locals to alloca/load/store,
/// so promotion is replace-uses heavy; `iter_batched` rebuilds fresh IR per run so
/// only the pass is timed.
fn bench_mem2reg(c: &mut Criterion) {
    let src = gen_source(50, 40);
    let ast = parse_src(&src);

    let mut group = c.benchmark_group("fcc/mem2reg");
    group.bench_function("promote", |b| {
        b.iter_batched(
            || {
                let ctx = Context::with_default_dialects();
                let module = codegen(&ctx, &ast).unwrap();
                (ctx, module)
            },
            |(ctx, module)| {
                let mut pm = tir::parse_pipeline("builtin.func(mem2reg)").unwrap();
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

criterion_group!(
    benches,
    bench_codegen,
    bench_codegen_expr_heavy,
    bench_mem2reg,
    bench_pipeline
);
criterion_main!(benches);
