use tir::Terminator;
use tir::helpers::operation;
use tir::{BlockId, Operation, ValueId, attributes::AttributeValue};

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
    BlockEndOp {
        name: "block_end",
        dialect: "asm",
        interfaces: [Terminator],
    }
}

impl Terminator for BlockEndOp {}

// A single-target conditional branch with fall-through: transfer to `dest` when
// `condition` is nonzero, else continue with the next op. It is the lowered form
// of one edge of `builtin.cond_br` (the other edge becomes a trailing
// `builtin.br`), produced before instruction selection so the branch condition
// participates in the e-graph cover like any other value. Deliberately *not* a
// `Terminator`: it sits mid-block, ahead of the block's real terminator.
operation! {
    CondBranchOp {
        name: "condbr",
        dialect: "asm",
        format: "custom",
        operands: O {
            condition: "tir::Any",
        },
        attributes: A {
            dest: "Block",
        },
    }
}

impl CondBranchOp {
    pub fn condition(&self) -> ValueId {
        self.operands()[0]
    }

    pub fn dest(&self) -> BlockId {
        self.attributes()
            .iter()
            .find_map(|a| match a.value {
                AttributeValue::Block(b) if a.name == "dest" => Some(b),
                _ => None,
            })
            .expect("condbr must have a 'dest' block attribute")
    }

    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        crate::print_branch(fmt, self, "asm.condbr")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        _context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        Err((tir::parse::Span(parser.pos()), tir::Error::ExpectedOpName))
    }
}
