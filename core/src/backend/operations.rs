use tir::helpers::operation;
use tir::{Any, Operation, Terminator};

operation! {
    SectionOp {
        name: "section",
        dialect: "asm",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SectionEndOp {
        name: "section_end",
        dialect: "asm",
        interfaces: [Terminator],
    }
}

impl Terminator for SectionEndOp {}

operation! {
    SymbolOp {
        name: "symbol",
        dialect: "asm",
        regions: R {
            body: Region {}
        }
    }
}

operation! {
    SymbolEndOp {
        name: "symbol_end",
        dialect: "asm",
        interfaces: [Terminator],
    }
}

impl Terminator for SymbolEndOp {}

// A data definition directive (`.dword 42`, `.string "hi"`, `.space 16`).
// `kind` names the directive, `value` holds the literal (Int or Str).
operation! {
    LiteralOp {
        name: "literal",
        dialect: "asm",
        attributes: A {
            kind: "Str",
        }
    }
}

operation! {
    DataRelocOp {
        name: "data_reloc",
        dialect: "asm",
        attributes: A {
            symbol: "Str",
            width: "UInt",
            addend: "Int",
        }
    }
}

operation! {
    BlockEndOp {
        name: "block_end",
        dialect: "asm",
        interfaces: [Terminator],
    }
}

impl Terminator for BlockEndOp {}

operation! {
    VirtualReturnOp {
        name: "vret",
        dialect: "asm",
        format: "custom",
        operands: O {
            values: "*Any",
        },
        interfaces: [Terminator],
    }
}

impl VirtualReturnOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        super::print_branch(fmt, self)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

impl VirtualReturnOpBuilder {
    pub fn value(self, value: tir::ValueId) -> Self {
        self.values(vec![value])
    }
}

impl Terminator for VirtualReturnOp {}

operation! {
    VirtualBranchOp {
        name: "vbr",
        dialect: "asm",
        format: "custom",
        operands: O {
            dest_args: "*Any",
        },
        attributes: A {
            dest: "Block",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for VirtualBranchOp {
    fn successors(&self) -> Vec<tir::BlockId> {
        super::branch_successors(self)
    }
}

impl VirtualBranchOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        super::print_branch(fmt, self)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}

operation! {
    VirtualCallOp {
        name: "vcall",
        dialect: "asm",
        attributes: A {
            callee: "Str",
            outgoing_stack_size: "UInt",
        },
        interfaces: [tir::attributes::RegisterSemantics],
    }
}

impl tir::attributes::RegisterSemantics for VirtualCallOp {
    fn attribute_roles(&self) -> &'static [(&'static str, tir::attributes::AttributeRole)] {
        &[("clobbers", tir::attributes::AttributeRole::Clobber)]
    }
}

operation! {
    VirtualIndirectCallOp {
        name: "vcall_indirect",
        dialect: "asm",
        attributes: A {
            callee_reg: "Register",
            outgoing_stack_size: "UInt",
        },
        interfaces: [tir::attributes::RegisterSemantics],
    }
}

impl tir::attributes::RegisterSemantics for VirtualIndirectCallOp {
    fn attribute_roles(&self) -> &'static [(&'static str, tir::attributes::AttributeRole)] {
        &[
            ("callee_reg", tir::attributes::AttributeRole::Use),
            ("clobbers", tir::attributes::AttributeRole::Clobber),
        ]
    }
}

impl VirtualCallOpBuilder {
    pub fn outgoing_stack_size(self, size: u64) -> Self {
        self.attr(
            "outgoing_stack_size",
            tir::attributes::AttributeValue::UInt(size),
        )
    }
}

impl VirtualIndirectCallOpBuilder {
    pub fn outgoing_stack_size(self, size: u64) -> Self {
        self.attr(
            "outgoing_stack_size",
            tir::attributes::AttributeValue::UInt(size),
        )
    }
}

impl VirtualCallOp {
    pub fn outgoing_stack_size(&self) -> u64 {
        outgoing_stack_size(self)
    }
}

impl VirtualIndirectCallOp {
    pub fn outgoing_stack_size(&self) -> u64 {
        outgoing_stack_size(self)
    }
}

fn outgoing_stack_size(op: &impl Operation) -> u64 {
    op.attributes()
        .iter()
        .find(|attribute| attribute.name == "outgoing_stack_size")
        .and_then(|attribute| match attribute.value {
            tir::attributes::AttributeValue::UInt(value) => Some(value),
            _ => None,
        })
        .expect("verified virtual calls have an outgoing stack size")
}
