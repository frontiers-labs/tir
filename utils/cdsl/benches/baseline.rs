use std::{hint::black_box, path::Path};
use tir_bench::{BatchSize, Criterion, TMAMetrics, criterion_group, criterion_main};
use tir_cdsl::{
    compile_file,
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
                    source[token.range].to_owned(),
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
    let before = parse(TokenStream::from_source(source).unwrap()).unwrap();
    let after = parse(TokenStream::from_source(&formatted).unwrap()).unwrap();
    assert_eq!(
        format!("{:?}", before.declarations),
        format!("{:?}", after.declarations)
    );
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
        assert!(parse(TokenStream::from_source(&edited).unwrap()).is_ok());
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

fn benchmarks(c: &mut Criterion, tma: &TMAMetrics) {
    let assembled = std::env::temp_dir().join(format!("tir-cdsl-baseline-{}", std::process::id()));
    std::fs::create_dir_all(&assembled).unwrap();
    let list = include_str!("../stdlib/std/list.cdsl");
    let equality = include_str!("../stdlib/std/equality.cdsl");
    std::fs::write(
        assembled.join("assembled.cdsl"),
        format!("{list}\n{equality}"),
    )
    .unwrap();
    std::fs::write(
        assembled.join("builtin.cdsl"),
        include_str!("../../../core/defs/dialects/builtin.cdsl"),
    )
    .unwrap();
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
    let generic = format!(
        "let x = some_name<{}>;",
        (0..8)
            .map(|index| format!("long_generic_argument_{index}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    formatting_preflight(&generic);
    let formatted = format_source(&generic, FormatOptions::default()).unwrap();
    assert!(
        formatted
            .lines()
            .filter(|line| line.contains("long_generic_argument_"))
            .all(|line| line.starts_with("    "))
    );
    let binary = format!(
        "let x = {};",
        (0..8)
            .map(|index| format!("long_operand_{index}"))
            .collect::<Vec<_>>()
            .join(" + ")
    );
    formatting_preflight(&binary);
    assert!(
        format_source(&binary, FormatOptions::default())
            .unwrap()
            .lines()
            .skip(1)
            .all(|line| line.starts_with("    "))
    );
    let function = "let f = fn(a: T, b: T) { let c = a + b; c };";
    formatting_preflight(function);
    assert!(
        format_source(function, FormatOptions::default())
            .unwrap()
            .contains("let c = a + b;")
    );
    formatting_preflight(&format!("let {} = 1;", "x".repeat(1000)));
    assert!(
        format_source(
            "let x = [// first\n// second\n1];",
            FormatOptions::default()
        )
        .unwrap()
        .contains("// first\n    // second")
    );
    assert!(
        format_source("let x = a + // right\nb;", FormatOptions::default())
            .unwrap()
            .contains("// right\n    b")
    );
    assert!(
        !format_source("let x = [1, // tail\n];", FormatOptions::default())
            .unwrap()
            .contains("// tail\n\n")
    );
    assert!(format_source("/* unterminated", FormatOptions::default()).is_err());
    for invalid in [
        "let = ;",
        "use a::* as b;",
        "let x = (1,;",
        "relation r(a)",
        "let x = if true { 1 };",
    ] {
        let compiler_errors = parse(TokenStream::from_source(invalid).unwrap()).unwrap_err();
        let syntax_errors = match parse_source(invalid, &mut SyntaxCache::new()) {
            Err(tir_cdsl::syntax::SyntaxError::Parse(errors)) => errors,
            Err(error) => panic!("unexpected error: {error}"),
            Ok(_) => panic!("invalid source was accepted"),
        };
        assert_eq!(format!("{compiler_errors:?}"), format!("{syntax_errors:?}"));
    }
    for (name, relative) in [
        ("local", "checks/Inputs/local.cdsl"),
        ("list", "stdlib/std/list.cdsl"),
        ("assembled", "assembled.cdsl"),
        ("builtin", "builtin.cdsl"),
    ] {
        let path = if name == "assembled" || name == "builtin" {
            assembled.join(relative)
        } else {
            Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
        };
        let source = std::fs::read_to_string(&path).unwrap();
        formatting_preflight(&source);
        let tokens = TokenStream::from_source(&source).unwrap();
        assert!(!tokens.tokens.is_empty());
        assert!(parse(tokens.clone()).is_ok());
        if name != "list" {
            let original = compile_file(&path).unwrap();
            assert!(!original.render().is_empty());
            let formatted_path = assembled.join(format!("formatted-{name}.cdsl"));
            std::fs::write(
                &formatted_path,
                format_source(&source, FormatOptions::default()).unwrap(),
            )
            .unwrap();
            let formatted = compile_file(&formatted_path).unwrap();
            let semantics = |program: tir_cdsl::Program| {
                program
                    .modules
                    .into_iter()
                    .map(|module| (module.path, format!("{:?}", module.ast.declarations)))
                    .collect::<Vec<_>>()
            };
            assert_eq!(semantics(original), semantics(formatted));
        }
        let mut group = c.benchmark_group(format!("cdsl/{name}"));
        group.sample_size(50);
        group.warm_up_time(std::time::Duration::from_secs(1));
        group.measurement_time(std::time::Duration::from_secs(2));
        group.bench_function("lex_drop", |b| {
            b.iter(|| {
                drop(black_box(
                    TokenStream::from_source(black_box(&source)).unwrap(),
                ))
            })
        });
        group.bench_function("parse_prelexed", |b| {
            b.iter_batched(
                || tokens.clone(),
                |input| black_box(parse(black_box(input)).unwrap()),
                BatchSize::SmallInput,
            )
        });
        group.bench_function("lex_parse_drop", |b| {
            b.iter(|| {
                drop(black_box(
                    parse(TokenStream::from_source(black_box(&source)).unwrap()).unwrap(),
                ))
            })
        });
        if name != "list" {
            group.bench_function("compile_file_drop", |b| {
                b.iter(|| drop(black_box(compile_file(black_box(&path)).unwrap())))
            });
        }
        let options = FormatOptions::default();
        group.bench_function("format_source_cold_drop", |b| {
            b.iter(|| {
                drop(black_box(
                    format_source(black_box(&source), options).unwrap(),
                ))
            })
        });
        let mut cache = SyntaxCache::new();
        let original = parse_source(&source, &mut cache).unwrap();
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
                    parse_source(black_box(&source), &mut cache).unwrap(),
                ))
            })
        });
        group.finish();
        tma.report(&format!("cdsl/{name}"));
    }
    std::fs::remove_dir_all(assembled).unwrap();
}
criterion_group!(benches, benchmarks);
criterion_main!(benches);
