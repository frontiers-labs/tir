use std::error::Error;
use std::fmt::{self, Display};

use tir::attributes::AttributeValue;
use tir::builtin::GlobalOp;
use tir::builtin::{ModuleEndOp, ModuleOp};
use tir::func::DeclareOp;
use tir::{Context, OpHandle, Operation};

use crate::backend::{
    BlockEndOp, DataRelocOp, LiteralOp, MachineInstruction, SectionEndOp, SectionOp, SymbolEndOp,
    SymbolOp,
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
        if op.is::<ModuleEndOp>()
            || op.is::<SectionEndOp>()
            || op.is::<SymbolEndOp>()
            || op.is::<BlockEndOp>()
            // Memory order is notation, not code: the ops that name a chain's
            // root and its merges assemble to nothing, like a label.
            || crate::backend::names_memory_state(op)
            // External declarations produce no assembly; references resolve
            // at link time.
            || op.is::<DeclareOp>()
            || op.clone().as_op::<GlobalOp>().is_some_and(|global| global.is_external())
        {
            return Ok(());
        }

        if let Some(section) = op.clone().as_op::<SectionOp>() {
            let name = string_attr(op, "name").unwrap_or_else(|| ".text".to_string());
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

        if op.clone().as_op::<SymbolOp>().is_some() {
            // Register allocation left the values in place and recorded where it
            // put them; this is where that map is read.
            let assignment =
                &crate::backend::RegAssignment::of_op(op, crate::backend::ASSIGNMENT_ATTR);
            let name = string_attr(op, "name").ok_or(AsmPrintError::MissingSymbolName)?;
            if string_attr(op, "binding").as_deref() != Some("local") {
                out.push_str(".global ");
                out.push_str(&name);
                out.push('\n');
            }
            if let Some(align) = int_attr(op, "align")
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
            let region = context.get_region(op.regions()[0]);
            for (index, block) in region.iter(context.clone()).enumerate() {
                if index > 0 {
                    match block.attr("name") {
                        Some(AttributeValue::Str(label)) => out.push_str(&label),
                        _ => {
                            out.push_str(".L");
                            out.push_str(&block.id().number().to_string());
                        }
                    }
                    out.push_str(":\n");
                }
                self.print_block(context, block, out, assignment)?;
            }
            return Ok(());
        }

        if op.clone().as_op::<LiteralOp>().is_some() {
            let kind = string_attr(op, "kind").ok_or(AsmPrintError::UnsupportedOp {
                op: LiteralOp::name(),
            })?;
            out.push_str("\t.");
            out.push_str(&kind);
            match kind.as_str() {
                "byte" | "half" | "word" | "dword" | "space" => {
                    let value = int_attr(op, "value").ok_or(AsmPrintError::UnsupportedOp {
                        op: LiteralOp::name(),
                    })?;
                    out.push(' ');
                    out.push_str(&value.to_string());
                    out.push('\n');
                }
                _ => {
                    let value = string_attr(op, "value").ok_or(AsmPrintError::UnsupportedOp {
                        op: LiteralOp::name(),
                    })?;
                    out.push_str(" \"");
                    out.push_str(&escape_asm_string(&value));
                    out.push_str("\"\n");
                }
            }
            return Ok(());
        }

        if op.clone().as_op::<DataRelocOp>().is_some() {
            let unsupported = || AsmPrintError::UnsupportedOp {
                op: DataRelocOp::name(),
            };
            let symbol = string_attr(op, "symbol").ok_or_else(unsupported)?;
            let directive = match int_attr(op, "width") {
                Some(4) => "word",
                Some(8) => "quad",
                _ => return Err(unsupported()),
            };
            let addend = int_attr(op, "addend").ok_or_else(unsupported)?;
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

fn int_attr(op: &OpHandle, name: &str) -> Option<i64> {
    op.attr(name).as_ref().and_then(AttributeValue::as_int)
}

fn string_attr(op: &OpHandle, name: &str) -> Option<String> {
    match op.attr(name)? {
        AttributeValue::Str(value) => Some(value.into_string()),
        _ => None,
    }
}
