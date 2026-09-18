use std::error::Error;
use std::fmt::{self, Display};

use tir::attributes::AttributeValue;
use tir::builtin::ModuleOp;
use tir::{Context, OpHandle, Operation};

use crate::backend::{
    AsmItem, DataRelocOp, LiteralOp, MachineInstruction, as_int_attr, as_string_attr,
    symbol_body_blocks,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsmPrintError {
    MissingSymbolName,
    NoAssemblySyntax { op: &'static str },
    InvalidInstruction { op: &'static str },
    UnsupportedOp { op: &'static str },
}

impl Display for AsmPrintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AsmPrintError::MissingSymbolName => write!(f, "asm symbol is missing name"),
            AsmPrintError::NoAssemblySyntax { op } => {
                write!(f, "'{op}' has no assembly syntax")
            }
            AsmPrintError::InvalidInstruction { op } => {
                write!(f, "assembly printer rejected '{op}'")
            }
            AsmPrintError::UnsupportedOp { op } => {
                write!(f, "cannot print '{op}' as assembly")
            }
        }
    }
}

impl Error for AsmPrintError {}

/// Renders lowered machine IR as this target's textual assembly. Stateless: an
/// instruction's syntax is a field of its [`crate::backend::InstrInfo`].
#[derive(Default)]
pub struct AsmPrinter;

impl AsmPrinter {
    pub fn new() -> Self {
        AsmPrinter
    }

    /// Render one instruction, or `None` if `op` is not a machine instruction.
    /// `assignment` places the values its register slots hold.
    pub fn print_instruction(
        &self,
        context: &Context,
        op: &OpHandle,
        assignment: &crate::backend::RegAssignment,
    ) -> Result<Option<String>, AsmPrintError> {
        let Some(mi) = op.clone().as_interface::<dyn MachineInstruction>() else {
            return Ok(None);
        };
        let Some(desc) = mi.info().asm else {
            return Err(AsmPrintError::NoAssemblySyntax {
                op: op.name().as_str(),
            });
        };
        crate::backend::asm_desc::print(desc, context, op, assignment)
            .map(Some)
            .ok_or(AsmPrintError::InvalidInstruction {
                op: op.name().as_str(),
            })
    }

    pub fn print_module(
        &self,
        context: &Context,
        module: &ModuleOp,
    ) -> Result<String, AsmPrintError> {
        let mut out = String::new();
        self.print_block(
            context,
            module.body(),
            &mut out,
            &crate::backend::RegAssignment::default(),
        )?;
        Ok(out)
    }

    fn print_block(
        &self,
        context: &Context,
        block: tir::BlockHandle,
        out: &mut String,
        assignment: &crate::backend::RegAssignment,
    ) -> Result<(), AsmPrintError> {
        for op_id in block.op_ids() {
            self.print_op_in(context, &context.get_op(op_id), out, assignment)?;
        }
        Ok(())
    }

    /// Print one operation of a module body. A driver emitting the module symbol
    /// by symbol calls this directly; [`AsmPrinter::print_module`] loops over it.
    pub fn print_op(
        &self,
        context: &Context,
        op: &OpHandle,
        out: &mut String,
    ) -> Result<(), AsmPrintError> {
        self.print_op_in(context, op, out, &crate::backend::RegAssignment::default())
    }

    fn print_op_in(
        &self,
        context: &Context,
        op: &OpHandle,
        out: &mut String,
        assignment: &crate::backend::RegAssignment,
    ) -> Result<(), AsmPrintError> {
        let item = crate::backend::asm_item(op);
        if matches!(item, AsmItem::Skip) {
            return Ok(());
        }

        if let AsmItem::Section(section) = item {
            let name = as_string_attr(op.attr("name")).unwrap_or_else(|| ".text".to_string());
            if name == ".text" {
                out.push_str(".text\n");
            } else {
                out.push_str(".section ");
                out.push_str(&name);
                out.push('\n');
            }
            self.print_block(context, section.body(), out, assignment)?;
            return Ok(());
        }

        if matches!(item, AsmItem::Symbol) {
            // Register allocation left the values in place and recorded where it
            // put them; this is where that map is read.
            let assignment =
                &crate::backend::RegAssignment::of_op(op, crate::backend::ASSIGNMENT_ATTR);
            let name = as_string_attr(op.attr("name")).ok_or(AsmPrintError::MissingSymbolName)?;
            if as_string_attr(op.attr("binding")).as_deref() != Some("local") {
                out.push_str(".global ");
                out.push_str(&name);
                out.push('\n');
            }
            if let Some(align) = as_int_attr(op.attr("align"))
                && align > 1
            {
                out.push_str("\t.balign ");
                out.push_str(&align.to_string());
                out.push('\n');
            }
            out.push_str(&name);
            out.push_str(":\n");
            // The symbol label above names the entry block, so only non-entry
            // blocks emit their own label (branch targets must be defined).
            let blocks = symbol_body_blocks(context, op);
            for (index, &block_id) in blocks.iter().enumerate() {
                let block = context.get_block(block_id);
                if index > 0 {
                    match block.attr("name") {
                        Some(AttributeValue::Str(label)) => out.push_str(&label),
                        _ => {
                            out.push_str(".L");
                            out.push_str(&block_id.number().to_string());
                        }
                    }
                    out.push_str(":\n");
                }
                let omitted = blocks
                    .get(index + 1)
                    .and_then(|&next| crate::backend::fallthrough_branch(context, block_id, next));
                for op_id in block.op_ids() {
                    if Some(op_id) != omitted {
                        self.print_op_in(context, &context.get_op(op_id), out, assignment)?;
                    }
                }
            }
            return Ok(());
        }

        if matches!(item, AsmItem::Literal) {
            let kind = as_string_attr(op.attr("kind")).ok_or(AsmPrintError::UnsupportedOp {
                op: LiteralOp::name(),
            })?;
            out.push_str("\t.");
            out.push_str(&kind);
            match kind.as_str() {
                "byte" | "half" | "word" | "dword" | "space" => {
                    let value =
                        as_int_attr(op.attr("value")).ok_or(AsmPrintError::UnsupportedOp {
                            op: LiteralOp::name(),
                        })?;
                    out.push(' ');
                    out.push_str(&value.to_string());
                    out.push('\n');
                }
                _ => {
                    let value =
                        as_string_attr(op.attr("value")).ok_or(AsmPrintError::UnsupportedOp {
                            op: LiteralOp::name(),
                        })?;
                    out.push_str(" \"");
                    out.push_str(&escape_asm_string(&value));
                    out.push_str("\"\n");
                }
            }
            return Ok(());
        }

        if matches!(item, AsmItem::DataReloc) {
            let unsupported = || AsmPrintError::UnsupportedOp {
                op: DataRelocOp::name(),
            };
            let symbol = as_string_attr(op.attr("symbol")).ok_or_else(unsupported)?;
            let directive = match as_int_attr(op.attr("width")) {
                Some(4) => "word",
                Some(8) => "quad",
                _ => return Err(unsupported()),
            };
            let addend = as_int_attr(op.attr("addend")).ok_or_else(unsupported)?;
            out.push_str("\t.");
            out.push_str(directive);
            out.push(' ');
            out.push_str(&symbol);
            if addend > 0 {
                out.push('+');
                out.push_str(&addend.to_string());
            } else if addend < 0 {
                out.push_str(&addend.to_string());
            }
            out.push('\n');
            return Ok(());
        }

        if let Some(text) = self.print_instruction(context, op, assignment)? {
            out.push('\t');
            out.push_str(&text);
            out.push('\n');
            return Ok(());
        }

        Err(AsmPrintError::UnsupportedOp {
            op: op.name().as_str(),
        })
    }
}

/// Escape a literal for a quoted assembler string directive.
fn escape_asm_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}
