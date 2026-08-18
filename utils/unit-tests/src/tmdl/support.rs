//! Shared scaffolding for decoding the sem blob a TMDL-generated backend
//! embeds beside its Rust: these tests assert over the decoded blob rather
//! than over the emitted source.

use std::{fs, process};

use tir_symbolic::lang::{SymKind, SCALAR_OPS};
use tir_symbolic::sem::{decode_sem_ops, SemOp, SemPayloadDesc};
use tmdl::{Action, Compiler, OutputKind};

/// An absolute path to a fixture under `tmdl/`.
pub fn fixture(rel: &str) -> String {
    format!("{}/../../tmdl/{rel}", env!("CARGO_MANIFEST_DIR"))
}

/// The generated Rust, the kind table it passes to the decoder, and every
/// program of its sem blob.
pub struct Generated {
    pub rust: String,
    pub kinds: Vec<SymKind>,
    pub blob: Vec<u8>,
    pub programs: Vec<String>,
}

impl Generated {
    /// The blob offset of the first `extend_sem_bytes` call after `marker`.
    pub fn offset_after(&self, marker: &str) -> u32 {
        let (_, rest) = self.rust.split_once(marker).unwrap();
        let (_, rest) = rest.split_once("SEM_BLOB, ").unwrap();
        let (offset, _) = rest.split_once(')').unwrap();
        offset.trim().parse().unwrap()
    }

    /// The program the first `extend_sem_bytes` call after `marker` replays.
    pub fn program_after(&self, marker: &str) -> String {
        render(&decode_sem_ops(
            &self.blob,
            self.offset_after(marker),
            &self.kinds,
        ))
    }

    /// The pattern program of the rule spec at `marker` (e.g. a
    /// `static RULE_*` name).
    pub fn rule_program(&self, marker: &str) -> String {
        let (_, rest) = self.rust.split_once(marker).unwrap();
        let (_, rest) = rest.split_once("offset: ").unwrap();
        let (offset, _) = rest.split_once(',').unwrap();
        render(&decode_sem_ops(
            &self.blob,
            offset.trim().parse().unwrap(),
            &self.kinds,
        ))
    }
}

pub fn generate(input: &str, dialect: &str) -> Generated {
    generate_with(input, None, dialect, false)
}

/// Compile an in-memory, objectless (text-only) source.
pub fn generate_source(name: &str, source: &str, dialect: &str) -> Generated {
    generate_with(name, Some(source), dialect, true)
}

fn generate_with(input: &str, source: Option<&str>, dialect: &str, text_only: bool) -> Generated {
    let stem = std::path::Path::new(input).file_stem().unwrap();
    let dir = std::env::temp_dir().join(format!(
        "tmdl-sem-blob-{}-{}",
        stem.to_str().unwrap(),
        process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let builder = Compiler::builder()
        .action(Action::EmitRust)
        .dialect(Some(dialect.to_string()))
        .text_only(text_only)
        .output(OutputKind::File(
            dir.join("generated.rs").to_str().unwrap().to_string(),
        ));
    let builder = match source {
        Some(source) => builder.add_source(input, source),
        None => builder.add_input(input),
    };
    builder.build().compile().unwrap();

    let rust = fs::read_to_string(dir.join("generated.rs")).unwrap();
    let kinds = kind_table(&rust);
    let blob = fs::read(dir.join("tmdl_sem.bin")).unwrap();
    let programs = programs(&blob, &kinds);
    Generated {
        rust,
        kinds,
        blob,
        programs,
    }
}

/// The kind table the generated Rust passes to the decoder.
fn kind_table(rust: &str) -> Vec<SymKind> {
    let (_, rest) = rust
        .split_once("static SEM_KINDS: &[tir::sem::SymKind] = &[")
        .unwrap();
    let (table, _) = rest.split_once("];").unwrap();
    table
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| named_kind(entry.trim_start_matches("tir::sem::SymKind::")))
        .collect()
}

/// Codegen names scalar-op kinds by their Rust variant and the rest by their
/// debug name; this inverts both.
fn named_kind(name: &str) -> SymKind {
    if let Some(op) = SCALAR_OPS.iter().find(|op| op.rust == name) {
        return op.kind;
    }
    match name {
        "Symbol" => SymKind::Symbol,
        "Constant" => SymKind::Constant,
        "If" => SymKind::If,
        "ZExt" => SymKind::ZExt,
        "SExt" => SymKind::SExt,
        "Extract" => SymKind::Extract,
        "Concat" => SymKind::Concat,
        "AtomicRmw" => SymKind::AtomicRmw,
        "Fence" => SymKind::Fence,
        "LoadMemory" => SymKind::LoadMemory,
        "StoreMemory" => SymKind::StoreMemory,
        "LoadReserved" => SymKind::LoadReserved,
        "StoreConditional" => SymKind::StoreConditional,
        "StateAssign" => SymKind::StateAssign,
        "StateBlock" => SymKind::StateBlock,
        "StateIf" => SymKind::StateIf,
        "StateStore" => SymKind::StateStore,
        "Map" => SymKind::Map,
        "Zip" => SymKind::Zip,
        "IterConcat" => SymKind::IterConcat,
        "Split" => SymKind::Split,
        "Reduce" => SymKind::Reduce,
        "Arg" => SymKind::Arg,
        "Iota" => SymKind::Iota,
        other => panic!("sem kind '{other}' is not known to this test"),
    }
}

/// Every program in the blob, rendered one line each: node kinds in post order
/// with their payloads, then each node's operand indices.
fn programs(blob: &[u8], kinds: &[SymKind]) -> Vec<String> {
    let mut offset = 5;
    let mut rendered = Vec::new();
    while offset < blob.len() {
        let len = u32::from_le_bytes(blob[offset..offset + 4].try_into().unwrap()) as usize;
        rendered.push(render(&decode_sem_ops(blob, offset as u32, kinds)));
        offset += 4 + len;
    }
    rendered
}

pub fn render(ops: &[SemOp]) -> String {
    let mut out = String::new();
    for op in ops {
        let step = match op {
            SemOp::Node(kind) => format!("{kind:?}"),
            SemOp::Payload(SemPayloadDesc::SymbolId(id)) => format!("#{id}"),
            SemOp::Payload(SemPayloadDesc::Value(value)) => format!("%{value}"),
            SemOp::Payload(SemPayloadDesc::Int { width, value, .. }) => {
                format!("{value}:{width}")
            }
            SemOp::Payload(SemPayloadDesc::Float(value)) => format!("{value}f"),
            SemOp::Typed(width) => format!("<{width}>"),
            SemOp::Edge(parent, child) => format!("{parent}<-{child}"),
        };
        if !out.is_empty() && !matches!(op, SemOp::Payload(_) | SemOp::Typed(_)) {
            out.push(' ');
        }
        out.push_str(&step);
    }
    out
}
