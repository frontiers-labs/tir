//! Generic object writer: walks lowered machine IR the same way the assembly
//! printer does, but encodes instructions to bytes, lays out sections, and
//! resolves fixups — block targets by patching, symbol targets by emitting
//! relocations.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::sync::Arc;

use tir::attributes::AttributeValue;
use tir::builtin::{DeclareOp, ModuleEndOp, ModuleOp};
use tir::{BlockId, Context, Operation};

use super::format::ObjectFormatInfo;
use super::{
    FixupTarget, InstructionEncoder, InstructionPatcher, ObjReloc, ObjSection, ObjSymbol,
    ObjectFile, SectionKind, SymBinding, SymKind,
};
use crate::backend::{
    BlockEndOp, LiteralOp, MachineInstruction, SectionEndOp, SectionOp, SymbolEndOp, SymbolOp,
    int_attr,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryEmitError {
    MissingSymbolName,
    MissingEncoder { op: String },
    CannotEncode { op: String },
    UnsupportedOp { op: String },
    UnknownBlockTarget { op: String },
    MisalignedTarget { op: String, delta: i64 },
    FixupOutOfRange { op: String, value: i64 },
    MissingPatcher { op: String },
    SymbolOperandUnsupported { op: String },
}

impl Display for BinaryEmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryEmitError::MissingSymbolName => write!(f, "asm symbol is missing name"),
            BinaryEmitError::MissingEncoder { op } => {
                write!(f, "no instruction encoder registered for '{op}'")
            }
            BinaryEmitError::CannotEncode { op } => {
                write!(f, "instruction encoder rejected '{op}'")
            }
            BinaryEmitError::UnsupportedOp { op } => {
                write!(f, "cannot encode '{op}' into an object file")
            }
            BinaryEmitError::UnknownBlockTarget { op } => {
                write!(f, "'{op}' targets a block outside the emitted symbol")
            }
            BinaryEmitError::MisalignedTarget { op, delta } => {
                write!(f, "branch target of '{op}' is misaligned (delta {delta})")
            }
            BinaryEmitError::FixupOutOfRange { op, value } => {
                write!(f, "branch target of '{op}' is out of range (value {value})")
            }
            BinaryEmitError::MissingPatcher { op } => {
                write!(f, "no fixup patcher registered for '{op}'")
            }
            BinaryEmitError::SymbolOperandUnsupported { op } => {
                write!(f, "instruction '{op}' cannot take a symbol operand")
            }
        }
    }
}

impl Error for BinaryEmitError {}

/// A fixup recorded during layout, pending resolution.
struct PendingFixup {
    section: usize,
    offset: u64,
    len: u8,
    op: String,
    target: FixupTarget,
}

pub struct BinaryWriter {
    encoders: HashMap<String, InstructionEncoder>,
    patchers: HashMap<String, InstructionPatcher>,
}

struct WalkState {
    obj: ObjectFile,
    current_section: Option<usize>,
    block_starts: HashMap<BlockId, u64>,
    fixups: Vec<PendingFixup>,
}

impl BinaryWriter {
    pub fn new(
        encoders: HashMap<String, InstructionEncoder>,
        patchers: HashMap<String, InstructionPatcher>,
    ) -> Self {
        Self { encoders, patchers }
    }

    pub fn write_module(
        &self,
        context: &Context,
        module: &ModuleOp,
        fmt: &ObjectFormatInfo,
    ) -> Result<ObjectFile, BinaryEmitError> {
        let mut state = WalkState {
            obj: ObjectFile::default(),
            current_section: None,
            block_starts: HashMap::new(),
            fixups: Vec::new(),
        };

        self.walk_block(context, module.body(), &mut state)?;
        self.resolve_fixups(&mut state, fmt)?;
        Ok(state.obj)
    }

    fn walk_block(
        &self,
        context: &Context,
        block: Arc<tir::Block>,
        state: &mut WalkState,
    ) -> Result<(), BinaryEmitError> {
        for op_id in block.op_ids() {
            let op = context.get_op(op_id);
            if op.is::<ModuleEndOp>()
                || op.is::<SectionEndOp>()
                || op.is::<SymbolEndOp>()
                || op.is::<BlockEndOp>()
                // External declarations contribute nothing to the object; their
                // symbols materialize as undefined entries via relocations.
                || op.is::<DeclareOp>()
            {
                continue;
            }

            if let Some(section) = op.clone().as_op::<SectionOp>() {
                let name = string_attr(&op, "name").unwrap_or(".text");
                let enclosing = state.current_section;
                state.current_section = Some(ensure_section(&mut state.obj, name));
                self.walk_block(context, section.body(), state)?;
                state.current_section = enclosing;
                continue;
            }

            if op.clone().as_op::<SymbolOp>().is_some() {
                self.walk_symbol(context, &op, state)?;
                continue;
            }

            if op.clone().as_op::<LiteralOp>().is_some() {
                emit_literal(&op, state)?;
                continue;
            }

            self.encode_op(&op, state)?;
        }
        Ok(())
    }

    fn walk_symbol(
        &self,
        context: &Context,
        op: &Arc<tir::OpInstance>,
        state: &mut WalkState,
    ) -> Result<(), BinaryEmitError> {
        let name = string_attr(op, "name")
            .ok_or(BinaryEmitError::MissingSymbolName)?
            .to_string();
        let section = state
            .current_section
            .unwrap_or_else(|| ensure_section(&mut state.obj, ".text"));
        state.current_section = Some(section);

        let start = state.obj.sections[section].data.len() as u64;
        let region = context.get_region(op.regions[0]);
        for block in region.iter(context.clone()) {
            let offset = state.obj.sections[section].data.len() as u64;
            state.block_starts.insert(block.id(), offset);
            self.walk_block(context, block, state)?;
        }
        let end = state.obj.sections[section].data.len() as u64;

        state.obj.symbols.push(ObjSymbol {
            name,
            section: Some(section),
            value: start,
            size: end - start,
            binding: if string_attr(op, "binding") == Some("local") {
                SymBinding::Local
            } else {
                SymBinding::Global
            },
            kind: if string_attr(op, "kind") == Some("object") {
                SymKind::Object
            } else {
                SymKind::Func
            },
        });
        Ok(())
    }

    fn encode_op(
        &self,
        op: &Arc<tir::OpInstance>,
        state: &mut WalkState,
    ) -> Result<(), BinaryEmitError> {
        let Some(encoder) = self.encoders.get(op.name().as_str()) else {
            if op
                .clone()
                .as_interface::<dyn MachineInstruction>()
                .is_some()
            {
                return Err(BinaryEmitError::MissingEncoder {
                    op: op.name().to_string(),
                });
            }
            return Err(BinaryEmitError::UnsupportedOp {
                op: op.name().to_string(),
            });
        };
        let encoded = encoder(op).ok_or_else(|| BinaryEmitError::CannotEncode {
            op: op.name().to_string(),
        })?;

        let section = state
            .current_section
            .unwrap_or_else(|| ensure_section(&mut state.obj, ".text"));
        state.current_section = Some(section);
        let data = &mut state.obj.sections[section].data;
        let offset = data.len() as u64;
        let len = encoded.bytes.len() as u8;
        data.extend_from_slice(&encoded.bytes);
        state.obj.sections[section].insn_spans.push((offset, len));

        for fixup in encoded.fixups {
            state.fixups.push(PendingFixup {
                section,
                offset,
                len,
                op: op.name().to_string(),
                target: fixup.target,
            });
        }
        Ok(())
    }

    fn resolve_fixups(
        &self,
        state: &mut WalkState,
        fmt: &ObjectFormatInfo,
    ) -> Result<(), BinaryEmitError> {
        for fixup in &state.fixups {
            match &fixup.target {
                FixupTarget::Block(block) => {
                    let target = *state.block_starts.get(block).ok_or_else(|| {
                        BinaryEmitError::UnknownBlockTarget {
                            op: fixup.op.clone(),
                        }
                    })?;
                    let base = if (fmt.pc_rel_from_end)(&fixup.op) {
                        fixup.offset + u64::from(fixup.len)
                    } else {
                        fixup.offset
                    };
                    let delta = target as i64 - base as i64;
                    let scale = (fmt.pc_rel_scale)(&fixup.op);
                    if delta & ((1 << scale) - 1) != 0 {
                        return Err(BinaryEmitError::MisalignedTarget {
                            op: fixup.op.clone(),
                            delta,
                        });
                    }
                    let value = delta >> scale;
                    let patcher = self.patchers.get(&fixup.op).ok_or_else(|| {
                        BinaryEmitError::MissingPatcher {
                            op: fixup.op.clone(),
                        }
                    })?;
                    let data = &mut state.obj.sections[fixup.section].data;
                    let range = fixup.offset as usize..(fixup.offset + fixup.len as u64) as usize;
                    patcher(&mut data[range], value).ok_or(BinaryEmitError::FixupOutOfRange {
                        op: fixup.op.clone(),
                        value,
                    })?;
                }
                FixupTarget::Symbol(symbol) => {
                    let kind = (fmt.reloc_for)(&fixup.op).ok_or_else(|| {
                        BinaryEmitError::SymbolOperandUnsupported {
                            op: fixup.op.clone(),
                        }
                    })?;
                    state.obj.sections[fixup.section].relocs.push(ObjReloc {
                        offset: fixup.offset + kind.field_offset,
                        symbol: symbol.clone(),
                        r_type: kind.r_type,
                        addend: kind.addend,
                    });
                }
            }
        }
        Ok(())
    }
}

/// Append a data directive's bytes to the current section. String directives
/// emit their raw bytes; numeric directives emit little-endian values.
fn emit_literal(op: &Arc<tir::OpInstance>, state: &mut WalkState) -> Result<(), BinaryEmitError> {
    let unsupported = || BinaryEmitError::UnsupportedOp {
        op: LiteralOp::name().to_string(),
    };
    let kind = string_attr(op, "kind").ok_or_else(unsupported)?;
    let bytes = match kind {
        "asciz" | "string" | "ascii" => {
            let value = string_attr(op, "value").ok_or_else(unsupported)?;
            let mut bytes = value.as_bytes().to_vec();
            if kind != "ascii" {
                bytes.push(0);
            }
            bytes
        }
        "byte" | "half" | "word" | "dword" | "space" => {
            let value = int_attr(&op.attributes, "value").ok_or_else(unsupported)?;
            match kind {
                "space" => vec![0u8; usize::try_from(value).map_err(|_| unsupported())?],
                "dword" => value.to_le_bytes().to_vec(),
                _ => {
                    let width = match kind {
                        "byte" => 1,
                        "half" => 2,
                        _ => 4,
                    };
                    // Accept the full signed and unsigned range of the width.
                    let min = -(1i64 << (width * 8 - 1));
                    let max = (1i64 << (width * 8)) - 1;
                    if value < min || value > max {
                        return Err(unsupported());
                    }
                    value.to_le_bytes()[..width].to_vec()
                }
            }
        }
        _ => return Err(unsupported()),
    };

    let section = state
        .current_section
        .unwrap_or_else(|| ensure_section(&mut state.obj, ".text"));
    state.current_section = Some(section);
    state.obj.sections[section].data.extend_from_slice(&bytes);
    Ok(())
}

fn ensure_section(obj: &mut ObjectFile, name: &str) -> usize {
    if let Some(idx) = obj.sections.iter().position(|s| s.name == name) {
        return idx;
    }
    // Only .text holds code; everything else (.rodata, .data) is data with
    // byte alignment until a directive says otherwise.
    let is_text = name == ".text" || name.starts_with(".text.");
    obj.sections.push(ObjSection {
        name: name.to_string(),
        kind: if is_text {
            SectionKind::Text
        } else {
            SectionKind::Data
        },
        align: if is_text { 4 } else { 1 },
        data: Vec::new(),
        relocs: Vec::new(),
        insn_spans: Vec::new(),
    });
    obj.sections.len() - 1
}

fn string_attr<'a>(op: &'a tir::OpInstance, name: &str) -> Option<&'a str> {
    op.attributes.iter().find_map(|attr| {
        if attr.name != name {
            return None;
        }
        match &attr.value {
            AttributeValue::Str(value) => Some(value.as_str()),
            _ => None,
        }
    })
}
