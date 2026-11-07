//! Emits global data sections from frontend definitions.

use tir::Context;
use tir::builtin::ModuleOp;
use tir::builtin::ops as b;

/// Lower frontend data definitions immediately ahead of the machine backend.
/// String uses become addresses into `.rodata`; scalar globals become symbols
/// in `.data`.
pub fn lower_data(context: &Context, module: &ModuleOp) -> Result<(), tir::PassError> {
    use tir::attributes::AttributeValue;
    use tir::backend::{LiteralOp, SectionEndOp, SectionOp, SymbolEndOp, SymbolOp};

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
        let section = SectionOp::new(
            context,
            None,
            tir::NewOpParts {
                attributes: vec![
                    (context)
                        .named_attribute("name", AttributeValue::Str(".bss".to_string().into())),
                ],
            },
            tir::InsertionPoint::Block {
                block: module_body.id(),
                position: module_body.len().saturating_sub(1),
            },
        );
        for (name, size, align, binding) in zero_globals {
            let symbol = SymbolOp::new(
                context,
                None,
                tir::NewOpParts {
                    attributes: vec![
                        context.named_attribute("name", AttributeValue::Str(name.into())),
                        context.named_attribute(
                            "binding",
                            AttributeValue::Str(binding.to_string().into()),
                        ),
                        context.named_attribute(
                            "kind",
                            AttributeValue::Str("object".to_string().into()),
                        ),
                        context.named_attribute("align", AttributeValue::UInt(align)),
                    ],
                },
                section.body().end(),
            );
            LiteralOp::new(
                context,
                tir::attributes::AttributeValue::Str(("space").into()),
                tir::NewOpParts {
                    attributes: vec![
                        context.named_attribute("value", AttributeValue::Int(size as i64)),
                    ],
                },
                symbol.body().end(),
            );
            SymbolEndOp::new(context, Default::default(), symbol.body().end());
        }
        SectionEndOp::new(context, Default::default(), section.body().end());
    }
    Ok(())
}

fn emit_data_section(
    context: &Context,
    module_body: &tir::BlockHandle,
    name: &str,
    definitions: Vec<DataSymbol>,
) {
    use tir::attributes::AttributeValue;
    use tir::backend::{DataRelocOp, LiteralOp, SectionEndOp, SectionOp, SymbolEndOp, SymbolOp};

    if definitions.is_empty() {
        return;
    }
    let byte = |insertion, value: u8| {
        LiteralOp::new(
            context,
            tir::attributes::AttributeValue::Str(("byte").into()),
            tir::NewOpParts {
                attributes: vec![
                    context.named_attribute("value", AttributeValue::Int(i64::from(value))),
                ],
            },
            insertion,
        )
    };
    let section = SectionOp::new(
        context,
        None,
        tir::NewOpParts {
            attributes: vec![
                context.named_attribute("name", AttributeValue::Str(name.to_string().into())),
            ],
        },
        tir::InsertionPoint::Block {
            block: module_body.id(),
            position: module_body.len().saturating_sub(1),
        },
    );
    for (name, bytes, mut relocations, align, binding) in definitions {
        let symbol = SymbolOp::new(
            context,
            None,
            tir::NewOpParts {
                attributes: vec![
                    context.named_attribute("name", AttributeValue::Str(name.into())),
                    context.named_attribute(
                        "binding",
                        AttributeValue::Str(binding.to_string().into()),
                    ),
                    (context)
                        .named_attribute("kind", AttributeValue::Str("object".to_string().into())),
                    context.named_attribute("align", AttributeValue::UInt(align)),
                ],
            },
            section.body().end(),
        );
        if let (true, Some(text)) = (relocations.is_empty(), c_string(&bytes)) {
            LiteralOp::new(
                context,
                tir::attributes::AttributeValue::Str(("asciz").into()),
                tir::NewOpParts {
                    attributes: vec![
                        context.named_attribute("value", AttributeValue::Str(text.into())),
                    ],
                },
                symbol.body().end(),
            );
            SymbolEndOp::new(context, Default::default(), symbol.body().end());

            continue;
        }
        relocations.sort_by_key(|relocation| relocation.0);
        let mut cursor = 0;
        for (offset, target, addend, width) in relocations {
            for &value in &bytes[cursor..offset as usize] {
                byte(symbol.body().end(), value);
            }
            DataRelocOp::new(
                context,
                tir::attributes::AttributeValue::Str((target).into()),
                tir::attributes::AttributeValue::UInt(width),
                tir::attributes::AttributeValue::Int(addend),
                Default::default(),
                symbol.body().end(),
            );
            cursor = (offset + width) as usize;
        }
        for &value in &bytes[cursor..] {
            byte(symbol.body().end(), value);
        }
        SymbolEndOp::new(context, Default::default(), symbol.body().end());
    }
    SectionEndOp::new(context, Default::default(), section.body().end());
}

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
