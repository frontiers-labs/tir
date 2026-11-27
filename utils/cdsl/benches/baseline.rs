use std::hint::black_box;
use tir_bench::{BatchSize, Criterion, TMAMetrics, Throughput, criterion_group, criterion_main};
use tir_cdsl::{
    CheckedProgram,
    format::{FormatOptions, format_range, format_source, format_tree},
    lexer::{Token, TokenStream, lex_source},
    parser::parse,
    syntax::{SyntaxCache, parse_source},
};

fn preserved(source: &str) -> Vec<(u8, String)> {
    lex_source(source)
        .unwrap()
        .into_iter()
        .filter_map(|token| {
            if token.token == Token::Whitespace {
                None
            } else {
                Some((
                    match token.token {
                        Token::LineComment => 1,
                        Token::BlockComment => 2,
                        _ => 0,
                    },
                    source[token.range()].to_owned(),
                ))
            }
        })
        .collect()
}

fn formatting_preflight(source: &str) {
    let options = FormatOptions::default();
    let mut cache = SyntaxCache::new();
    let tree = parse_source(source, &mut cache).unwrap();
    assert_eq!(tree.root().green().text(), source);
    let formatted = format_tree(&tree, options);
    assert_eq!(preserved(source), preserved(&formatted));
    assert_eq!(format_source(&formatted, options).unwrap(), formatted);
    let before = parse(&TokenStream::from_source(source).unwrap()).unwrap();
    let after = parse(&TokenStream::from_source(&formatted).unwrap()).unwrap();
    assert_eq!(before.decls.len(), after.decls.len());
    for position in [0, source.len() / 2, source.len()] {
        if !source.is_char_boundary(position) {
            continue;
        }
        let edit = format_range(&tree, position..position, options).unwrap();
        let mut edited = source.to_owned();
        edited.replace_range(edit.range.clone(), &edit.replacement);
        assert!(edited.starts_with(&source[..edit.range.start]));
        assert!(edited.ends_with(&source[edit.range.end..]));
        assert_eq!(preserved(source), preserved(&edited));
        assert!(parse(&TokenStream::from_source(&edited).unwrap()).is_ok());
    }
    let whole = format_range(&tree, 0..source.len(), options).unwrap();
    if whole.range == (0..source.len()) {
        assert_eq!(whole.replacement, formatted);
    }
    let formatted_tree = parse_source(&formatted, &mut SyntaxCache::new()).unwrap();
    if let (Some(original), Some(replacement)) = (
        tree.root().children().next(),
        formatted_tree.root().children().next(),
    ) {
        let range = original.text_range();
        let replacement = replacement.green().clone();
        let mut expected = source.to_owned();
        expected.replace_range(
            range.start().get() as usize..range.end().get() as usize,
            &replacement.text(),
        );
        let edited = tir_cst::NodeOrToken::Node(original)
            .replace_with(tir_cst::NodeOrToken::Node(replacement), &mut cache)
            .unwrap();
        assert_eq!(edited.green().text(), expected);
        assert_eq!(tree.root().green().text(), source);
        drop(edited);
    }
    let second = parse_source(source, &mut cache).unwrap();
    assert!(tree.root().green().ptr_eq(second.root().green()));
}

fn generate_v3_corpus(repeats: usize) -> String {
    let mut out = String::with_capacity(repeats * 450 + 512);
    out.push_str(
        "pub enum Option<A> {\n    None,\n    Some(A),\n}\n\n\
         pub enum Pair {\n    Mk(Int, Int),\n}\n\n\
         pub enum Eq: Prop {\n    Refl,\n}\n\n\
         extract rust Option => \"::core::option::Option\" [\n    None => \"::core::option::Option::None\",\n    Some => \"::core::option::Option::Some\",\n];\n\n",
    );
    for i in 0..repeats {
        use std::fmt::Write;
        writeln!(
            out,
            "pub let helper_{i}: Int -> Int = |x: Int| -> Int {{\n\
                 let a: Int = x + 1;\n\
                 let b: Int = a * 2 - 3;\n\
                 if b == 0 {{\n\
                     a\n\
                 }} else {{\n\
                     b + x\n\
                 }}\n\
             }};\n\
             pub let fold_{i}: Pair -> Int = |p: Pair| -> Int {{\n\
                 match p {{\n\
                     Pair::Mk(x, y) => helper_{i}(x) + y,\n\
                 }}\n\
             }};\n\
             pub let proof_{i}: Eq = Eq::Refl;\n",
        )
        .unwrap();
    }
    out
}

fn benchmarks(c: &mut Criterion, tma: &TMAMetrics) {
    for source in [
        "",
        "// lead\nlet x = (1); // tail\n",
        "// trailing spaces   ",
        "/* first\r second */\r// standalone\rlet x = 1;",
        "/* first\n   second */\n// standalone   \nlet x = 1; // trailing   \n",
        "/* outer /* nested */ end */ let x = [1, 2,];",
        "let x = {a: 1, /* field */ b: 2};",
        "let r#type = (1_000, \"A\\n\\u{03B2}\", true,); let β = r#type;",
        "let x = [// first\n// second\n1];",
        "let x = a // left\n+ b;",
        "let x = a + // right\nb;",
        "let x = [1, // tail\n];",
    ] {
        formatting_preflight(source);
    }
    let function = "let f = |a: Int, b: Int| -> Int { let c: Int = a + b; c };";
    formatting_preflight(function);
    assert!(
        format_source(function, FormatOptions::default())
            .unwrap()
            .contains("let c: Int = a + b;")
    );
    formatting_preflight(&format!("let {} = 1;", "x".repeat(1000)));
    assert!(format_source("/* unterminated", FormatOptions::default()).is_err());

    let small_src = generate_v3_corpus(10);
    let large_src = generate_v3_corpus(600); // ~10,000 lines

    for (name, source) in [("small", &small_src), ("large_10k", &large_src)] {
        let lines = source.lines().count() as u64;
        if name == "small" {
            formatting_preflight(source);
        }
        let tokens = TokenStream::from_source(source).unwrap();
        assert!(!tokens.tokens.is_empty());
        assert!(parse(&tokens).is_ok());
        {
            let mut prog = CheckedProgram::from_source(source).unwrap();
            let rust = prog.emit_rust().unwrap();
            assert!(!rust.is_empty());
        }

        let mut group = c.benchmark_group(format!("cdsl/{name}"));
        group.throughput(Throughput::Elements(lines));
        group.sample_size(40);
        group.warm_up_time(std::time::Duration::from_secs(1));
        group.measurement_time(std::time::Duration::from_secs(2));
        group.bench_function("lex_drop", |b| {
            b.iter(|| {
                drop(black_box(
                    TokenStream::from_source(black_box(source)).unwrap(),
                ))
            })
        });
        group.bench_function("parse_prelexed", |b| {
            b.iter_batched(
                || tokens.clone(),
                |input| black_box(parse(black_box(&input)).unwrap()),
                BatchSize::SmallInput,
            )
        });
        group.bench_function("lex_parse_drop", |b| {
            b.iter(|| {
                let ts = TokenStream::from_source(black_box(source)).unwrap();
                drop(black_box(parse(&ts).unwrap()));
            })
        });
        group.bench_function("emit_rust_drop", |b| {
            b.iter(|| {
                let mut prog = CheckedProgram::from_source(black_box(source)).unwrap();
                drop(black_box(prog.emit_rust().unwrap()));
            })
        });
        if name == "small" {
            let options = FormatOptions::default();
            group.bench_function("format_source_cold_drop", |b| {
                b.iter(|| {
                    drop(black_box(
                        format_source(black_box(source), options).unwrap(),
                    ))
                })
            });
            let mut cache = SyntaxCache::new();
            let original = parse_source(source, &mut cache).unwrap();
            group.bench_function("format_retained_root_drop", |b| {
                b.iter(|| drop(black_box(format_tree(black_box(&original), options))))
            });
            let mut middle = source.len() / 2;
            while !source.is_char_boundary(middle) {
                middle -= 1;
            }
            group.bench_function("format_range_retained_root_drop", |b| {
                b.iter(|| {
                    drop(black_box(
                        format_range(black_box(&original), middle..middle, options).unwrap(),
                    ))
                })
            });
            group.bench_function("parse_retained_cache_drop", |b| {
                b.iter(|| {
                    drop(black_box(
                        parse_source(black_box(source), &mut cache).unwrap(),
                    ))
                })
            });
        }
        group.finish();
        tma.report(&format!("cdsl/{name}"));
    }
}
criterion_group!(benches, benchmarks);
criterion_main!(benches);
