//! Emits global data sections from frontend definitions.

use tir::builtin::ModuleOp;
use tir::builtin::ops as b;
use tir::{Context, Operation};

/// Lower frontend data definitions immediately ahead of the machine backend.
/// String uses become addresses into `.rodata`; scalar globals become symbols
/// in `.data`.
pub fn lower_data(context: &Context, module: &ModuleOp) -> Result<(), tir::PassError> {
    use tir::attributes::AttributeValue;
    use tir::backend::{
        LiteralOpBuilder, SectionEndOpBuilder, SectionOpBuilder, SymbolEndOpBuilder,
        SymbolOpBuilder,
    };

    let mut globals = Vec::new();
    let mut read_only = Vec::new();
    let mut zero_globals = Vec::new();

    let module_body = module.body();
    for op_id in module_body.op_ids() {
        let op = context.get_op(op_id);
        let Some(global) = op.clone().as_op::<b::GlobalOp>() else {
            continue;
        };
        if global.is_external() {
            continue;
        }
        let align = global
            .align()
            .expect("a global definition carries an alignment");
        let binding = match tir::symbol_table::visibility_of(&global) {
            tir::Visibility::Private => "local",
            tir::Visibility::Public => "global",
        };
        match global.bytes() {
            Some(bytes) => {
                let entry = (
                    global.sym_name(),
                    bytes,
                    global.relocations(),
                    align,
                    binding,
                );
                if global.section().as_deref() == Some(".rodata") {
                    read_only.push(entry);
                } else {
                    globals.push(entry);
                }
            }
            None => zero_globals.push((
                global.sym_name(),
                global.size().expect("a zero-filled global carries a size"),
                align,
                binding,
            )),
        }
        context.erase_op(&tir::OperationRef::new(op))?;
    }

    emit_data_section(context, &module_body, ".data", globals);
    emit_data_section(context, &module_body, ".rodata", read_only);

    if !zero_globals.is_empty() {
        let section = SectionOpBuilder::new(context)
            .attr("name", AttributeValue::Str(".bss".to_string().into()))
            .build();
        for (name, size, align, binding) in zero_globals {
            let symbol = SymbolOpBuilder::new(context)
                .attr("name", AttributeValue::Str(name.into()))
                .attr("binding", AttributeValue::Str(binding.to_string().into()))
                .attr("kind", AttributeValue::Str("object".to_string().into()))
                .attr("align", AttributeValue::UInt(align))
                .build();
            symbol.body().append_op(
                LiteralOpBuilder::new(context)
                    .kind("space")
                    .attr("value", AttributeValue::Int(size as i64))
                    .build(),
            );
            symbol
                .body()
                .append_op(SymbolEndOpBuilder::new(context).build());
            section.body().append_op(symbol);
        }
        section
            .body()
            .append_op(SectionEndOpBuilder::new(context).build());
        let end = context.get_block(module_body.id()).len().saturating_sub(1);
        module_body.insert(end, section.id());
    }
    Ok(())
}

/// One object per δ definition, its initializer spelled out byte by byte with a
/// relocation wherever it holds an address.
fn emit_data_section(
    context: &Context,
    module_body: &tir::BlockHandle,
    name: &str,
    definitions: Vec<DataSymbol>,
) {
    use tir::attributes::AttributeValue;
    use tir::backend::{
        DataRelocOpBuilder, LiteralOpBuilder, SectionEndOpBuilder, SectionOpBuilder,
        SymbolEndOpBuilder, SymbolOpBuilder,
    };

    if definitions.is_empty() {
        return;
    }
    let byte = |value: u8| {
        LiteralOpBuilder::new(context)
            .kind("byte")
            .attr("value", AttributeValue::Int(i64::from(value)))
            .build()
    };
    let section = SectionOpBuilder::new(context)
        .attr("name", AttributeValue::Str(name.to_string().into()))
        .build();
    for (name, bytes, mut relocations, align, binding) in definitions {
        let symbol = SymbolOpBuilder::new(context)
            .attr("name", AttributeValue::Str(name.into()))
            .attr("binding", AttributeValue::Str(binding.to_string().into()))
            .attr("kind", AttributeValue::Str("object".to_string().into()))
            .attr("align", AttributeValue::UInt(align))
            .build();
        if let (true, Some(text)) = (relocations.is_empty(), c_string(&bytes)) {
            symbol.body().append_op(
                LiteralOpBuilder::new(context)
                    .kind("asciz")
                    .attr("value", AttributeValue::Str(text.into()))
                    .build(),
            );
            symbol
                .body()
                .append_op(SymbolEndOpBuilder::new(context).build());
            section.body().append_op(symbol);
            continue;
        }
        relocations.sort_by_key(|relocation| relocation.0);
        let mut cursor = 0;
        for (offset, target, addend, width) in relocations {
            for &value in &bytes[cursor..offset as usize] {
                symbol.body().append_op(byte(value));
            }
            symbol.body().append_op(
                DataRelocOpBuilder::new(context)
                    .symbol(target)
                    .width(width)
                    .addend(addend)
                    .build(),
            );
            cursor = (offset + width) as usize;
        }
        for &value in &bytes[cursor..] {
            symbol.body().append_op(byte(value));
        }
        symbol
            .body()
            .append_op(SymbolEndOpBuilder::new(context).build());
        section.body().append_op(symbol);
    }
    section
        .body()
        .append_op(SectionEndOpBuilder::new(context).build());
    let end = context.get_block(module_body.id()).len().saturating_sub(1);
    module_body.insert(end, section.id());
}

/// The text of an initializer that is exactly one NUL-terminated string, which
/// the assembler spells `.asciz` rather than byte by byte.
fn c_string(bytes: &[u8]) -> Option<String> {
    let (&0, text) = bytes.split_last()? else {
        return None;
    };
    if text.contains(&0) {
        return None;
    }
    String::from_utf8(text.to_vec()).ok()
}

/// A data object as the machine layer takes it: name, initializer image, the
/// addresses patched into it, alignment and ELF binding.
type DataSymbol = (
    String,
    Vec<u8>,
    Vec<(u64, String, i64, u64)>,
    u64,
    &'static str,
);
