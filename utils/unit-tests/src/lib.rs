//! Unit tests for heavy workspace crates.
//!
//! Heavy crates (core, backends, simulators, fcc, tmdl, …) compile with
//! `test = false`; their unit tests live here instead, one `#[cfg(test)]`
//! module per crate, so the workspace links a single test binary. Tests in
//! this crate exercise public APIs only. FileCheck-style tests belong in a
//! crate's `checks/` directory, not here.

#[cfg(test)]
mod fcc {
    mod cir;
    mod codegen;
    mod determinism;
    mod diagnostics;
    mod lang_options;
    mod link_driver;
    mod link_host_compare;
    mod link_mixed_objects;
    mod link_support;
    mod sema;
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
    mod egraph;
    mod egraph_pattern;
    mod exec;
    mod infer;
    mod ops;
    mod rewrite_runner_extract;
    mod sat;
    mod sexpr;
    mod smtlib;
    mod support;
    mod test_lang;
}

#[cfg(test)]
mod arm64;

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
mod fuzz;

#[cfg(test)]
mod llvm;

#[cfg(test)]
mod riscv;

#[cfg(test)]
mod x86_64;

#[cfg(test)]
mod simcore {
    mod executor;
    mod memsys;
    mod predictor;
    mod prefetch;
    mod scoreboard;
    mod timing;
}

#[cfg(test)]
mod verify {
    mod wide_opcode;
}

#[cfg(test)]
mod tools {
    mod model_check;
}

#[cfg(test)]
mod core {
    mod affine;
    mod alias_facts;
    mod analysis;
    mod arith;
    mod backend;
    mod binary;
    mod context;
    mod dependence;
    mod dialects;
    mod dominance;
    mod encodings;
    mod fixtures;
    mod float;
    mod isel;
    mod isel_rules;
    mod layout;
    mod liveness;
    mod machine_ir;
    mod pass;
    mod regalloc;
    mod sem;
}
