//! Generic object writer: walks lowered machine IR the same way the assembly
//! printer does, but encodes instructions to bytes, lays out sections, and
//! resolves fixups — block targets by patching, symbol targets by emitting
//! relocations.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};

use tir::attributes::AttributeValue;
use tir::builtin::GlobalOp;
use tir::builtin::{ModuleEndOp, ModuleOp};
use tir::func::DeclareOp;
use tir::{BlockId, Context, Operation};

use super::format::ObjectFormatInfo;
use super::{
    FixupTarget, ObjReloc, ObjSection, ObjSymbol, ObjectFile, SectionKind, SymBinding, SymKind,
};
use crate::backend::{
    BlockEndOp, DataRelocOp, InstrInfo, LiteralOp, MachineInstruction, SectionEndOp, SectionOp,
    SymbolEndOp, SymbolOp,
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
    info: &'static InstrInfo,
    target: FixupTarget,
}

/// Lays out and encodes machine IR into an object file. Stateless: an
/// instruction's encoder and patcher are fields of its [`InstrInfo`].
#[derive(Default)]
pub struct BinaryWriter;

/// Object emission in progress. A driver that emits the module symbol by
/// symbol threads one of these through [`BinaryWriter::write_op`] and closes it
/// with [`BinaryWriter::finish`]; cross-symbol references are relocations, so
/// only [`BinaryWriter::finish`] needs the whole module to have been walked.
#[derive(Default)]
pub struct ObjectEmission {
    obj: ObjectFile,
    current_section: Option<usize>,
    block_starts: HashMap<BlockId, u64>,
    fixups: Vec<PendingFixup>,
}

impl BinaryWriter {
    pub fn new() -> Self {
        BinaryWriter
    }

    pub fn write_module(
        &self,
        context: &Context,
        module: &ModuleOp,
        fmt: &ObjectFormatInfo,
    ) -> Result<ObjectFile, BinaryEmitError> {
        let mut state = ObjectEmission::default();
        self.walk_block(context, module.body(), &mut state, fmt)?;
        self.finish(state, fmt)
    }

    /// Resolve the fixups left by [`BinaryWriter::write_op`] and yield the object.
    pub fn finish(
        &self,
        mut state: ObjectEmission,
        fmt: &ObjectFormatInfo,
    ) -> Result<ObjectFile, BinaryEmitError> {
        self.resolve_fixups(&mut state, fmt)?;
        Ok(state.obj)
    }

    fn walk_block(
        &self,
        context: &Context,
        block: tir::BlockHandle,
        state: &mut ObjectEmission,
        fmt: &ObjectFormatInfo,
    ) -> Result<(), BinaryEmitError> {
        for op_id in block.op_ids() {
            self.write_op(context, &context.get_op(op_id), state, fmt)?;
        }
        Ok(())
    }

    /// Encode one operation of a module body into `state`. A driver emitting the
    /// module symbol by symbol calls this directly.
    pub fn write_op(
        &self,
        context: &Context,
        op: &tir::OpHandle,
        state: &mut ObjectEmission,
        fmt: &ObjectFormatInfo,
    ) -> Result<(), BinaryEmitError> {
        if op.is::<ModuleEndOp>()
            || op.is::<SectionEndOp>()
            || op.is::<SymbolEndOp>()
            || op.is::<BlockEndOp>()
            // External declarations contribute nothing to the object; their
            // symbols materialize as undefined entries via relocations.
            || op.is::<DeclareOp>()
            || op.clone().as_op::<GlobalOp>().is_some_and(|global| global.is_external())
        {
            return Ok(());
        }

        if let Some(section) = op.clone().as_op::<SectionOp>() {
            let name = string_attr(op, "name").unwrap_or_else(|| ".text".to_string());
            let enclosing = state.current_section;
            state.current_section = Some(ensure_section(&mut state.obj, &name));
            self.walk_block(context, section.body(), state, fmt)?;
            state.current_section = enclosing;
            return Ok(());
        }

        if op.clone().as_op::<SymbolOp>().is_some() {
            self.walk_symbol(context, op, state, fmt)?;
            return Ok(());
        }

        if op.clone().as_op::<LiteralOp>().is_some() {
            emit_literal(op, state)?;
            return Ok(());
        }

        if op.clone().as_op::<DataRelocOp>().is_some() {
            emit_data_reloc(op, state, fmt)?;
            return Ok(());
        }

        self.encode_op(op, state)
    }

    fn walk_symbol(
        &self,
        context: &Context,
        op: &tir::OpHandle,
        state: &mut ObjectEmission,
        fmt: &ObjectFormatInfo,
    ) -> Result<(), BinaryEmitError> {
        let name = string_attr(op, "name").ok_or(BinaryEmitError::MissingSymbolName)?;
        let section = state
            .current_section
            .unwrap_or_else(|| ensure_section(&mut state.obj, ".text"));
        state.current_section = Some(section);

        let align = int_attr(op, "align")
            .and_then(|align| u64::try_from(align).ok())
            .unwrap_or(1)
            .max(1);
        let aligned = (state.obj.sections[section].data.len() as u64).div_ceil(align) * align;
        state.obj.sections[section].data.resize(aligned as usize, 0);
        state.obj.sections[section].align = state.obj.sections[section].align.max(align);
        let start = state.obj.sections[section].data.len() as u64;
        let region = context.get_region(op.regions()[0]);
        for block in region.iter(context.clone()) {
            let offset = state.obj.sections[section].data.len() as u64;
            state.block_starts.insert(block.id(), offset);
            self.walk_block(context, block, state, fmt)?;
        }
        let end = state.obj.sections[section].data.len() as u64;

        state.obj.symbols.push(ObjSymbol {
            name,
            section: Some(section),
            value: start,
            size: end - start,
            binding: if string_attr(op, "binding").as_deref() == Some("local") {
                SymBinding::Local
            } else {
                SymBinding::Global
            },
            kind: if string_attr(op, "kind").as_deref() == Some("object") {
                SymKind::Object
            } else {
                SymKind::Func
            },
        });
        Ok(())
    }

    fn encode_op(
        &self,
        op: &tir::OpHandle,
        state: &mut ObjectEmission,
    ) -> Result<(), BinaryEmitError> {
        let Some(mi) = op.clone().as_interface::<dyn MachineInstruction>() else {
            return Err(BinaryEmitError::UnsupportedOp {
                op: op.name().to_string(),
            });
        };
        let info = mi.info();
        let Some(spec) = info.encode else {
            return Err(BinaryEmitError::MissingEncoder {
                op: op.name().to_string(),
            });
        };
        let encoded =
            super::encode_with(op, spec).ok_or_else(|| BinaryEmitError::CannotEncode {
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
                info,
                target: fixup.target,
            });
        }
        Ok(())
    }

    fn resolve_fixups(
        &self,
        state: &mut ObjectEmission,
        fmt: &ObjectFormatInfo,
    ) -> Result<(), BinaryEmitError> {
        for fixup in &state.fixups {
            match &fixup.target {
                FixupTarget::Block(block) => {
                    let name = fixup.info.name;
                    let target = *state.block_starts.get(block).ok_or_else(|| {
                        BinaryEmitError::UnknownBlockTarget {
                            op: name.to_string(),
                        }
                    })?;
                    let base = if (fmt.pc_rel_from_end)(name) {
                        fixup.offset + u64::from(fixup.len)
                    } else {
                        fixup.offset
                    };
                    let delta = target as i64 - base as i64;
                    let scale = (fmt.pc_rel_scale)(name);
                    if delta & ((1 << scale) - 1) != 0 {
                        return Err(BinaryEmitError::MisalignedTarget {
                            op: name.to_string(),
                            delta,
                        });
                    }
                    let value = delta >> scale;
                    let spec = fixup
                        .info
                        .patch
                        .ok_or_else(|| BinaryEmitError::MissingPatcher {
                            op: name.to_string(),
                        })?;
                    let data = &mut state.obj.sections[fixup.section].data;
                    let range = fixup.offset as usize..(fixup.offset + fixup.len as u64) as usize;
                    super::patch_with(&mut data[range], value, spec).ok_or(
                        BinaryEmitError::FixupOutOfRange {
                            op: name.to_string(),
                            value,
                        },
                    )?;
                }
                FixupTarget::Symbol(symbol) => {
                    let kind = (fmt.reloc_for)(fixup.info.name).ok_or_else(|| {
                        BinaryEmitError::SymbolOperandUnsupported {
                            op: fixup.info.name.to_string(),
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
fn emit_literal(op: &tir::OpHandle, state: &mut ObjectEmission) -> Result<(), BinaryEmitError> {
    let unsupported = || BinaryEmitError::UnsupportedOp {
        op: LiteralOp::name().to_string(),
    };
    let kind = string_attr(op, "kind").ok_or_else(unsupported)?;
    let bytes = match kind.as_str() {
        "asciz" | "string" | "ascii" => {
            let value = string_attr(op, "value").ok_or_else(unsupported)?;
            let mut bytes = value.as_bytes().to_vec();
            if kind != "ascii" {
                bytes.push(0);
            }
            bytes
        }
        "byte" | "half" | "word" | "dword" | "space" => {
            let value = int_attr(op, "value").ok_or_else(unsupported)?;
            match kind.as_str() {
                "space" => vec![0u8; usize::try_from(value).map_err(|_| unsupported())?],
                "dword" => value.to_le_bytes().to_vec(),
                _ => {
                    let width = match kind.as_str() {
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

fn emit_data_reloc(
    op: &tir::OpHandle,
    state: &mut ObjectEmission,
    fmt: &ObjectFormatInfo,
) -> Result<(), BinaryEmitError> {
    let unsupported = || BinaryEmitError::UnsupportedOp {
        op: DataRelocOp::name().to_string(),
    };
    let symbol = string_attr(op, "symbol").ok_or_else(unsupported)?;
    let width = int_attr(op, "width")
        .and_then(|width| u8::try_from(width).ok())
        .ok_or_else(unsupported)?;
    let r_type = (fmt.absolute_reloc)(width).ok_or_else(unsupported)?;
    let addend = int_attr(op, "addend").ok_or_else(unsupported)?;
    let section = state
        .current_section
        .unwrap_or_else(|| ensure_section(&mut state.obj, ".data"));
    state.current_section = Some(section);
    let offset = state.obj.sections[section].data.len() as u64;
    state.obj.sections[section]
        .data
        .resize((offset + u64::from(width)) as usize, 0);
    state.obj.sections[section].relocs.push(ObjReloc {
        offset,
        symbol,
        r_type,
        addend,
    });
    Ok(())
}

fn ensure_section(obj: &mut ObjectFile, name: &str) -> usize {
    if let Some(idx) = obj.sections.iter().position(|s| s.name == name) {
        return idx;
    }
    // Sections start byte-aligned until a directive says otherwise.
    let kind = if name == ".text" || name.starts_with(".text.") {
        SectionKind::Text
    } else if name == ".bss" || name.starts_with(".bss.") {
        SectionKind::UninitializedData
    } else if name == ".rodata" || name.starts_with(".rodata.") {
        SectionKind::ReadOnlyData
    } else {
        SectionKind::Data
    };
    obj.sections.push(ObjSection {
        name: name.to_string(),
        kind,
        align: if kind == SectionKind::Text { 4 } else { 1 },
        data: Vec::new(),
        relocs: Vec::new(),
        insn_spans: Vec::new(),
    });
    obj.sections.len() - 1
}

fn int_attr(op: &tir::OpHandle, name: &str) -> Option<i64> {
    op.attr(name).as_ref().and_then(AttributeValue::as_int)
}

fn string_attr(op: &tir::OpHandle, name: &str) -> Option<String> {
    match op.attr(name)? {
        AttributeValue::Str(value) => Some(value.into_string()),
        _ => None,
    }
}
