use std::collections::HashMap;
use std::error::Error;
use std::fmt::{self, Display};
use std::sync::Arc;

use tir::attributes::AttributeValue;
use tir::builtin::{ModuleEndOp, ModuleOp};
use tir::{Context, OpInstance, Operation};

use crate::backend::{
    BlockEndOp, MachineInstruction, SectionEndOp, SectionOp, SymbolEndOp, SymbolOp,
};

pub type AsmInstructionPrinter = fn(&OpInstance) -> Option<String>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AsmPrintError {
    MissingSymbolName,
    MissingInstructionPrinter { op: &'static str },
    InvalidInstruction { op: &'static str },
    UnsupportedOp { op: &'static str },
}

impl Display for AsmPrintError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AsmPrintError::MissingSymbolName => write!(f, "asm symbol is missing name"),
            AsmPrintError::MissingInstructionPrinter { op } => {
                write!(f, "no assembly printer registered for '{op}'")
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

pub struct AsmPrinter {
    instruction_printers: HashMap<String, AsmInstructionPrinter>,
}

impl AsmPrinter {
    pub fn new(instruction_printers: HashMap<String, AsmInstructionPrinter>) -> Self {
        Self {
            instruction_printers,
        }
    }

    pub fn print_instruction(&self, op: &OpInstance) -> Result<Option<String>, AsmPrintError> {
        let Some(printer) = self.instruction_printers.get(op.name()) else {
            return Ok(None);
        };
        printer(op)
            .map(Some)
            .ok_or(AsmPrintError::InvalidInstruction { op: op.name() })
    }

    pub fn print_module(
        &self,
        context: &Context,
        module: &ModuleOp,
    ) -> Result<String, AsmPrintError> {
        let mut out = String::new();
        self.print_block(context, module.body(), &mut out)?;
        Ok(out)
    }

    fn print_block(
        &self,
        context: &Context,
        block: Arc<tir::Block>,
        out: &mut String,
    ) -> Result<(), AsmPrintError> {
        for op_id in block.op_ids() {
            let op = context.get_op(op_id);
            if op.name() == ModuleEndOp::name()
                || op.name() == SectionEndOp::name()
                || op.name() == SymbolEndOp::name()
                || op.name() == BlockEndOp::name()
            {
                continue;
            }

            if let Some(section) = op.clone().as_op::<SectionOp>() {
                out.push_str(".text\n");
                self.print_block(context, section.body(), out)?;
                continue;
            }

            if let Some(symbol) = op.clone().as_op::<SymbolOp>() {
                let name = string_attr(&op, "name").ok_or(AsmPrintError::MissingSymbolName)?;
                out.push_str(".global ");
                out.push_str(name);
                out.push('\n');
                out.push_str(name);
                out.push_str(":\n");
                self.print_block(context, symbol.body(), out)?;
                continue;
            }

            if let Some(text) = self.print_instruction(&op)? {
                out.push('\t');
                out.push_str(&text);
                out.push('\n');
                continue;
            }

            if op
                .clone()
                .as_interface::<dyn MachineInstruction>()
                .is_some()
            {
                return Err(AsmPrintError::MissingInstructionPrinter { op: op.name() });
            }

            return Err(AsmPrintError::UnsupportedOp { op: op.name() });
        }
        Ok(())
    }
}

fn string_attr<'a>(op: &'a OpInstance, name: &str) -> Option<&'a str> {
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
