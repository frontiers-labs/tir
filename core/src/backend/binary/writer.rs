//! Generic object writer: walks lowered machine IR the same way the assembly
//! printer does, but encodes instructions to bytes, lays out sections, and
//! resolves fixups — block targets by patching, symbol targets by emitting
//! relocations.

use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};

use tir::builtin::{GlobalOp, ModuleOp, SymbolDifference};
use tir::{BlockId, Context, Operation};

use super::format::ObjectFormatInfo;
use super::{
    FixupTarget, ObjReloc, ObjSection, ObjSymbol, ObjectFile, SectionKind, SymBinding, SymKind,
};
use crate::backend::{
    AsmItem, DataRelocOp, InstrInfo, LiteralOp, MachineInstruction, as_int_attr, as_string_attr,
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
    SymbolOperandUnsupported { op: String },
    CannotResolveSymbolDifference { symbol: String, base: String },
}

impl Display for BinaryEmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryEmitError::MissingSymbolName => write!(f, "asm symbol is missing name"),
            BinaryEmitError::MissingEncoder { op } => {
                write!(f, "'{op}' has no encoding")
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
            BinaryEmitError::SymbolOperandUnsupported { op } => {
                write!(f, "instruction '{op}' cannot take a symbol operand")
            }
            BinaryEmitError::CannotResolveSymbolDifference { symbol, base } => {
                write!(
                    f,
                    "symbol difference '{symbol} - {base}' requires definitions in the same section"
                )
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
    /// The immediate field of the shape the instruction was encoded as.
    patch: &'static super::PatchField,
}

/// Lays out and encodes machine IR into an object file. Stateless: an
/// instruction's encoder is a field of its [`InstrInfo`], and the patcher for
/// a fixup comes back from the encoding.
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
    /// Where register allocation put the current symbol's values; the encoder
    /// resolves a register slot holding a value through it.
    assignment: crate::backend::RegAssignment,
    block_starts: HashMap<BlockId, u64>,
    fixups: Vec<PendingFixup>,
    symbol_differences: Vec<(usize, SymbolDifference)>,
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
        resolve_symbol_differences(&mut state)?;
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
        if let Some(global) = op.clone().as_op::<GlobalOp>() {
            return emit_global(&global, state, fmt);
        }
        match crate::backend::asm_item(op) {
            AsmItem::Skip => Ok(()),
            AsmItem::Section(section) => {
                let name = as_string_attr(op.attr("name")).unwrap_or_else(|| ".text".to_string());
                let enclosing = state.current_section;
                state.current_section = Some(ensure_section(&mut state.obj, &name));
                self.walk_block(context, section.body(), state, fmt)?;
                state.current_section = enclosing;
                Ok(())
            }
            AsmItem::Symbol => self.walk_symbol(context, op, state, fmt),
            AsmItem::Literal => emit_literal(op, state),
            AsmItem::DataReloc => emit_data_reloc(op, state, fmt),
            AsmItem::Instruction => self.encode_op(op, state),
        }
    }

    fn walk_symbol(
        &self,
        context: &Context,
        op: &tir::OpHandle,
        state: &mut ObjectEmission,
        fmt: &ObjectFormatInfo,
    ) -> Result<(), BinaryEmitError> {
        let name = as_string_attr(op.attr("name")).ok_or(BinaryEmitError::MissingSymbolName)?;
        state.assignment =
            crate::backend::RegAssignment::of_op(op, crate::backend::ASSIGNMENT_ATTR);
        let section = state
            .current_section
            .unwrap_or_else(|| ensure_section(&mut state.obj, ".text"));
        state.current_section = Some(section);

        let align = as_int_attr(op.attr("align"))
            .and_then(|align| u64::try_from(align).ok())
            .unwrap_or(1)
            .max(1);
        let aligned = (state.obj.sections[section].data.len() as u64).div_ceil(align) * align;
        state.obj.sections[section].data.resize(aligned as usize, 0);
        state.obj.sections[section].align = state.obj.sections[section].align.max(align);
        let start = state.obj.sections[section].data.len() as u64;
        let blocks = crate::backend::symbol_body_blocks(context, op);
        for (index, &block_id) in blocks.iter().enumerate() {
            let offset = state.obj.sections[section].data.len() as u64;
            state.block_starts.insert(block_id, offset);
            let omitted = blocks
                .get(index + 1)
                .and_then(|&next| crate::backend::fallthrough_branch(context, block_id, next));
            for op_id in context.get_block(block_id).op_ids() {
                if Some(op_id) != omitted {
                    self.write_op(context, &context.get_op(op_id), state, fmt)?;
                }
            }
        }
        let end = state.obj.sections[section].data.len() as u64;

        state.obj.symbols.push(ObjSymbol {
            name,
            section: Some(section),
            value: start,
            size: end - start,
            binding: if as_string_attr(op.attr("binding")).as_deref() == Some("local") {
                SymBinding::Local
            } else {
                SymBinding::Global
            },
            kind: if as_string_attr(op.attr("kind")).as_deref() == Some("object") {
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
        let encoded = super::encode_with(op, spec, &state.assignment).ok_or_else(|| {
            BinaryEmitError::CannotEncode {
                op: op.name().to_string(),
            }
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
                patch: fixup.patch,
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
                    let data = &mut state.obj.sections[fixup.section].data;
                    let range = fixup.offset as usize..(fixup.offset + fixup.len as u64) as usize;
                    super::patch_with(&mut data[range], value, fixup.patch).ok_or(
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

fn resolve_symbol_differences(state: &mut ObjectEmission) -> Result<(), BinaryEmitError> {
    let symbols: HashMap<_, _> = state
        .obj
        .symbols
        .iter()
        .map(|symbol| (symbol.name.as_str(), (symbol.section, symbol.value)))
        .collect();
    for (section, difference) in &state.symbol_differences {
        let error = || BinaryEmitError::CannotResolveSymbolDifference {
            symbol: difference.symbol.clone(),
            base: difference.base.clone(),
        };
        let (Some(symbol_section), symbol) = symbols
            .get(difference.symbol.as_str())
            .copied()
            .ok_or_else(error)?
        else {
            return Err(error());
        };
        let (Some(base_section), base) = symbols
            .get(difference.base.as_str())
            .copied()
            .ok_or_else(error)?
        else {
            return Err(error());
        };
        if symbol_section != base_section {
            return Err(error());
        }
        let offset = difference.offset as usize;
        let width = difference.width as usize;
        state.obj.sections[*section].data[offset..offset + width]
            .copy_from_slice(&symbol.wrapping_sub(base).to_le_bytes()[..width]);
    }
    Ok(())
}

fn emit_global(
    global: &GlobalOp,
    state: &mut ObjectEmission,
    fmt: &ObjectFormatInfo,
) -> Result<(), BinaryEmitError> {
    if global.is_external() {
        return Ok(());
    }
    let align = global
        .align()
        .expect("global definitions have an alignment");
    let bytes = global.bytes();
    let name = global
        .section()
        .unwrap_or_else(|| if bytes.is_some() { ".data" } else { ".bss" }.to_string());
    let section = ensure_section(&mut state.obj, &name);
    let data = &mut state.obj.sections[section];
    let offset = (data.data.len() as u64).div_ceil(align) * align;
    data.data.resize(offset as usize, 0);
    let size = match bytes {
        Some(bytes) => {
            let size = bytes.len() as u64;
            data.data.extend(bytes);
            size
        }
        None => {
            let size = global.size().expect("zero-filled globals have a size");
            data.data.resize((offset + size) as usize, 0);
            size
        }
    };
    data.align = data.align.max(align);
    for (relative, symbol, addend, width) in global.relocations() {
        let r_type = u8::try_from(width)
            .ok()
            .and_then(fmt.absolute_reloc)
            .ok_or_else(|| BinaryEmitError::UnsupportedOp {
                op: GlobalOp::name().to_string(),
            })?;
        data.relocs.push(ObjReloc {
            offset: offset + relative,
            symbol,
            r_type,
            addend,
        });
    }
    for mut difference in
        global
            .symbol_differences()
            .map_err(|_| BinaryEmitError::CannotEncode {
                op: GlobalOp::name().to_string(),
            })?
    {
        difference.offset += offset;
        state.symbol_differences.push((section, difference));
    }
    state.obj.symbols.push(ObjSymbol {
        name: global.sym_name(),
        section: Some(section),
        value: offset,
        size,
        binding: match tir::symbol_table::visibility_of(global) {
            tir::Visibility::Private => SymBinding::Local,
            tir::Visibility::Public => SymBinding::Global,
        },
        kind: SymKind::Object,
    });
    Ok(())
}

/// Append a data directive's bytes to the current section. String directives
/// emit their raw bytes; numeric directives emit little-endian values.
fn emit_literal(op: &tir::OpHandle, state: &mut ObjectEmission) -> Result<(), BinaryEmitError> {
    let unsupported = || BinaryEmitError::UnsupportedOp {
        op: LiteralOp::name().to_string(),
    };
    let kind = as_string_attr(op.attr("kind")).ok_or_else(unsupported)?;
    let bytes = match kind.as_str() {
        "asciz" | "string" | "ascii" => {
            let value = as_string_attr(op.attr("value")).ok_or_else(unsupported)?;
            let mut bytes = value.as_bytes().to_vec();
            if kind != "ascii" {
                bytes.push(0);
            }
            bytes
        }
        "byte" | "half" | "word" | "dword" | "space" => {
            let value = as_int_attr(op.attr("value")).ok_or_else(unsupported)?;
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
    let symbol = as_string_attr(op.attr("symbol")).ok_or_else(unsupported)?;
    let width = as_int_attr(op.attr("width"))
        .and_then(|width| u8::try_from(width).ok())
        .ok_or_else(unsupported)?;
    let r_type = (fmt.absolute_reloc)(width).ok_or_else(unsupported)?;
    let addend = as_int_attr(op.attr("addend")).ok_or_else(unsupported)?;
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
