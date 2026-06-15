//! End-to-end codegen benchmark for the `fcc` C frontend: it lowers a
//! synthetic translation unit (many functions, each a chain of local
//! declarations over deep arithmetic) down to TIR. `codegen` measures the
//! AST → IR step in isolation; `pipeline` includes tokenizing and parsing.

use std::fmt::Write;
use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use logos::Logos;

use fcc::ast::Ast;
use fcc::codegen::codegen;
use fcc::lexer::Token;
use fcc::parser::parse;
use tir::Context;

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
        write!(src, "return t{} + t0 * a; }}\n", stmts - 1).unwrap();
    }
    src
}

fn parse_src(src: &str) -> Ast {
    let tokens: Vec<Token> = Token::lexer(src).map(|r| r.unwrap()).collect();
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

criterion_group!(benches, bench_codegen, bench_pipeline);
criterion_main!(benches);
