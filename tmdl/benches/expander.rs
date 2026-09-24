#[macro_use]
#[path = "../../benchmarks/functions.rs"]
pub mod functions;

use tmdl::{MacroTable, StringArena, collect_macros, expand, lex};

// The real macro-heavy x86_64 defs, concatenated in build.rs order so every
// macro definition precedes its invocations (collect_macros is order-free, but
// this mirrors the shipped input). Included at compile time to stay in sync.
const DEFS: &[&str] = &[
    include_str!("../../backends/x86_64/defs/main.tmdl"),
    include_str!("../../backends/x86_64/defs/base.tmdl"),
    include_str!("../../backends/x86_64/defs/arith_ext.tmdl"),
    include_str!("../../backends/x86_64/defs/conditional.tmdl"),
    include_str!("../../backends/x86_64/defs/memory_ext.tmdl"),
    include_str!("../../backends/x86_64/defs/float.tmdl"),
];

benchmarks! {
    compiler = "tir", inputs = DEFS;
    x86_defs_collect_and_expand("x86_defs/collect_and_expand", functions::Settings {
        bytes: Some(DEFS.join("\n").len() as u64),
        ..Default::default()
    }) |b| {
        let input = DEFS.join("\n");
        let (tokens, errors) = lex(&input);
        assert!(errors.is_empty());
        let arena = StringArena::new();
        b.iter(|| {
            let mut table = MacroTable::new();
            let mut diagnostics = Vec::new();
            let tokens = collect_macros("<bench>", tokens.clone(), &mut table, &mut diagnostics);
            assert!(diagnostics.is_empty());
            let (_output, diagnostics) = expand("<bench>", tokens, &table, &arena);
            assert!(diagnostics.is_empty());
        });
    }
}
