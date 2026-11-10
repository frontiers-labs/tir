//! Unit tests for heavy workspace crates.
//!
//! Heavy crates (core, backends, simulators, fcc, tmdl, …) compile with
//! `test = false`; their unit tests live here instead, one `#[cfg(test)]`
//! module per crate, so the workspace links a single test binary. Tests in
//! this crate exercise public APIs only. FileCheck-style tests belong in a
//! crate's `checks/` directory, not here.

#[cfg(test)]
mod fcc {
    mod benchmark_output;
    mod cir;
    mod codegen;
    mod determinism;
    mod diagnostics;
    mod lang_options;
    mod link_driver;
    mod link_host_compare;
    mod link_mixed_objects;
    mod link_support;
    // Migration skip: this suite exercises removed selector or allocator infrastructure.
    #[cfg(any())]
    mod pbqp_dump;
    mod sema;
    // Migration skip: the slab storage census API was removed.
    #[cfg(any())]
    mod slab;
    mod support;
}

#[cfg(test)]
mod tmdl {
    mod compiler;
    mod json;
    mod markdown;
    mod parser;
    mod rustgen;
    mod sem_blob;
    mod sem_expr_state;
    mod support;
}

#[cfg(test)]
mod symbolic {
    mod bitblast;
    mod btor2;
    mod discover;
    mod exec;
    mod infer;
    mod ops;
    mod rounded;
    mod sat;
    mod sexpr;
    mod smtlib;
    mod support;
}

// Migration skip: this suite targets the removed tir-egraph engine.
#[cfg(any())]
mod egraph {
    mod egraph_pattern;
    mod engine;
    mod rewrite_runner_extract;
    mod test_lang;
}

#[cfg(test)]
mod backends {
    mod arm64;
    mod riscv;
    mod support;
    mod x86_64;
}

#[cfg(test)]
mod capi {
    mod ffi;
    mod inspect;
    mod mutate;
    mod schema;
    mod support;
    mod target;
    mod types;
}

#[cfg(test)]
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
mod jit;

#[cfg(test)]
mod llvm;

#[cfg(test)]
mod simcore {
    mod conditional_latency;
    mod executor;
    mod memory;
    mod memory_service;
    mod memsys;
    mod predictor;
    mod prefetch;
    mod scoreboard;
    mod support;
    mod timing;
}

#[cfg(test)]
mod verify {
    mod wide_opcode;
}

#[cfg(test)]
mod tools {
    mod model_check;
    mod sched;
}

#[cfg(test)]
mod core {
    // Migration skip: this suite targets the removed affine analysis and strip-mining implementation.
    #[cfg(any())]
    mod affine;
    mod analysis;
    mod arith;
    mod backend;
    mod binary;
    mod binding;
    mod block_layout;
    mod context;
    mod dependence;
    // Migration skip: this suite targets the removed control-recovery implementation.
    #[cfg(any())]
    mod destructure;
    mod dialects;
    mod edits;
    mod encodings;
    // Migration skip: the storage commit and revision APIs were removed.
    #[cfg(any())]
    mod epochs;
    mod eqsat;
    mod fixtures;
    mod float;
    mod memory_proof;
    // Migration skip: this suite targets the removed guarded-relaxation implementation.
    #[cfg(any())]
    mod float_refinement;
    mod idmap;
    mod interfaces;
    // Migration skip: this suite exercises removed selector or allocator infrastructure.
    #[cfg(any())]
    mod isel;
    // Migration skip: this suite targets the removed relational e-graph implementation.
    #[cfg(any())]
    mod isel_automaton;
    // Migration skip: this suite exercises removed selector or allocator infrastructure.
    #[cfg(any())]
    mod isel_rules;
    mod layout;
    mod machine_ir;
    // Migration skip: this suite targets the removed object analysis implementation.
    #[cfg(any())]
    mod objects;
    mod pass;
    mod region;
    // Migration skip: this suite targets the removed relational e-graph implementation.
    #[cfg(any())]
    mod sem;
    // Migration skip: candidate forks and adoption were removed.
    #[cfg(any())]
    mod variants;
}

#[cfg(test)]
fn fixture_root() -> std::path::PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .expect("Cargo test manifest directory")
        .into()
}

#[cfg(test)]
mod egglog {
    mod api;
    mod colors;
    mod containers;
    mod database;
    mod differential;
    mod engine;
    mod extract;
    mod groups;
    mod parser;
    mod passes;
    mod program;
    mod reference;
}
