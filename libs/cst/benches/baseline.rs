use std::hint::black_box;
use tir_bench::{Criterion, TMAMetrics, criterion_group, criterion_main};
use tir_cst::{GreenNode, GreenNodeBuilder, NodeCache, NodeOrToken, SyntaxNode};

fn build(cache: &mut NodeCache<u8>, rows: usize, mixed: bool) -> GreenNode<u8> {
    let mut b = GreenNodeBuilder::with_cache(cache);
    b.start_node(0).unwrap();
    for i in 0..rows {
        if mixed {
            b.start_node(3).unwrap();
            b.token(2, "fn scope() {\n").unwrap();
        }
        b.start_node(1).unwrap();
        let value = if mixed {
            format!("value_{i}")
        } else {
            String::new()
        };
        let number = if mixed {
            format!("{}", i * 17)
        } else {
            String::new()
        };
        for text in [
            "let",
            " ",
            if mixed { &value } else { "value" },
            " = ",
            if mixed { &number } else { "123" },
            ";",
            "\n",
        ] {
            b.token(2, text).unwrap();
        }
        b.finish_node().unwrap();
        if mixed {
            b.token(2, "}\n").unwrap();
            b.finish_node().unwrap();
        }
    }
    b.finish_node().unwrap();
    b.finish_green().unwrap()
}
fn benchmarks(c: &mut Criterion, tma: &TMAMetrics) {
    for (rows, mixed) in [(16, false), (2048, false), (256, true)] {
        let mut cache = NodeCache::new();
        let retained = build(&mut cache, rows, mixed);
        let root = SyntaxNode::new_root(retained.clone());
        let expected = if mixed {
            (0..rows)
                .map(|i| format!("fn scope() {{\nlet value_{i} = {};\n}}\n", i * 17))
                .collect::<String>()
        } else {
            "let value = 123;\n".repeat(rows)
        };
        assert_eq!(retained.text(), expected);
        assert_eq!(
            root.descendants_with_tokens().count(),
            rows * if mixed { 11 } else { 8 }
        );
        let token = root
            .descendants_with_tokens()
            .find_map(|element| match element {
                NodeOrToken::Token(token) if token.text() == "let" => Some(token),
                _ => None,
            })
            .unwrap();
        let child = root.first_child().unwrap();
        let replacement_node = build(&mut cache, 1, false);
        let replacement = cache.token(2, "other").unwrap();
        let edited = NodeOrToken::Token(token.clone())
            .replace_with(NodeOrToken::Token(replacement.clone()), &mut cache)
            .unwrap();
        assert_eq!(
            edited.green().text(),
            expected.replacen(token.text(), "other", 1)
        );
        drop(edited);
        let child_text = child.green().text();
        let changed = format!(
            "{}{}",
            replacement_node.text(),
            &expected[child_text.len()..]
        );
        assert_eq!(
            child
                .replace_with(replacement_node.clone(), &mut cache)
                .unwrap()
                .green()
                .text(),
            changed
        );
        assert_eq!(
            root.splice_children(
                0..1,
                [NodeOrToken::Node(replacement_node.clone())],
                &mut cache
            )
            .unwrap()
            .green()
            .text(),
            changed
        );
        assert_eq!(root.green().text(), expected);
        assert_eq!(retained.text(), expected);
        let mut group = c.benchmark_group(format!("cst/synthetic_rows_{rows}_mixed_{mixed}"));
        group.sample_size(50);
        group.warm_up_time(std::time::Duration::from_secs(1));
        group.measurement_time(std::time::Duration::from_secs(2));
        group.bench_function("cold_build_drop", |b| {
            b.iter(|| {
                drop(black_box(build(
                    &mut NodeCache::new(),
                    black_box(rows),
                    mixed,
                )))
            })
        });
        group.bench_function("retained_cache_build_drop", |b| {
            b.iter(|| drop(black_box(build(&mut cache, black_box(rows), mixed))))
        });
        group.bench_function("owned_traversal", |b| {
            b.iter(|| black_box(root.descendants_with_tokens().map(black_box).count()))
        });
        group.bench_function("render_drop", |b| {
            b.iter(|| drop(black_box(retained.text())))
        });
        group.bench_function("token_edit_drop", |b| {
            b.iter(|| {
                drop(black_box(
                    NodeOrToken::Token(token.clone())
                        .replace_with(NodeOrToken::Token(replacement.clone()), &mut cache)
                        .unwrap(),
                ))
            })
        });
        group.bench_function("node_edit_drop", |b| {
            b.iter(|| {
                drop(black_box(
                    child
                        .replace_with(replacement_node.clone(), &mut cache)
                        .unwrap(),
                ))
            })
        });
        group.bench_function("splice_drop", |b| {
            b.iter(|| {
                drop(black_box(
                    root.splice_children(
                        0..1,
                        [NodeOrToken::Node(replacement_node.clone())],
                        &mut cache,
                    )
                    .unwrap(),
                ))
            })
        });
        group.finish();
        black_box(&retained);
        tma.report(&format!("cst/synthetic_rows_{rows}_mixed_{mixed}"));
    }
}
criterion_group!(benches, benchmarks);
criterion_main!(benches);
