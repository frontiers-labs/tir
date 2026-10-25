//! End-to-end codegen benchmark for the `fcc` C frontend: it lowers a
//! synthetic translation unit (many functions, each a chain of local
//! declarations over deep arithmetic) down to TIR. `codegen` measures the
//! AST → IR step in isolation; `pipeline` includes tokenizing and parsing.

use std::fmt::Write;
use std::hint::black_box;

use logos::Logos;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use fcc::codegen::codegen;
use fcc::diagnostics::{Span, intern_file};
use fcc::lexer::Token;
use fcc::parser::parse;
use fcc::sema::{TypedAst, analyze};
use tir::builtin::ModuleOp;
use tir::{Context, Operation};

// The analyzed unit and the context its types are interned in, which the unit
// is generated into.
type AstInput = (Context, TypedAst);
type IrInput = (Context, ModuleOp);

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

fn gen_expr_heavy(funcs: usize, depth: usize) -> String {
    let mut src = String::new();
    for f in 0..funcs {
        let mut n = f;
        let expr = build_expr(depth, &mut n);
        writeln!(src, "int g{f}(int a, int b, int c) {{ return {expr}; }}").unwrap();
    }
    src
}

fn parse_src(src: &str) -> AstInput {
    let file = intern_file("<bench>", src);
    let tokens: Vec<_> = Token::lexer(src)
        .spanned()
        .map(|(r, span)| (r.unwrap(), Span::new(file, span.start)))
        .collect();
    let options = Default::default();
    let ast = parse(&tokens, options).expect("parse");
    let context = Context::with_default_dialects();
    let ast = analyze(&context, ast, options).expect("sema");
    (context, ast)
}

fn codegen_ir((ctx, ast): AstInput) -> IrInput {
    let module = codegen(&ctx, &ast).unwrap();
    (ctx, module)
}

fn run_codegen(input: AstInput) {
    let (_ctx, module) = codegen_ir(input);
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
    run_codegen(parse_src(src));
}

fn synthetic(c: &mut Criterion) {
    // A unit is generated into the context its analysis interned its types
    // in, so each run analyzes it again, untimed.
    c.bench_function("fcc/codegen/ast_to_ir", |b| {
        let src = gen_source(50, 40);
        b.iter_batched(|| parse_src(&src), run_codegen, BatchSize::SmallInput);
    });
    c.bench_function("fcc/codegen_expr_heavy/ast_to_ir", |b| {
        let src = gen_expr_heavy(20, 12);
        b.iter_batched(|| parse_src(&src), run_codegen, BatchSize::SmallInput);
    });
    c.bench_function("fcc/promote/promote", |b| {
        let src = gen_source(50, 40);
        // Rebuild fresh IR because promotion replaces the locals' uses.
        b.iter_batched(
            || codegen_ir(parse_src(&src)),
            run_promote,
            BatchSize::SmallInput,
        );
    });
    c.bench_function("fcc/pipeline/source_to_ir", |b| {
        let src = gen_source(50, 40);
        b.iter(|| run_pipeline(&src));
    });
}

criterion_group!(benches, synthetic);
criterion_main!(benches);
