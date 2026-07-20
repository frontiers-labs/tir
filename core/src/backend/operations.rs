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
        operands: [value],
        interfaces: [Terminator],
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
        super::print_branch(fmt, self, "asm.vbr")
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
        },
        roles: R {
            clobbers: Clobber,
        },
    }
}

operation! {
    VirtualIndirectCallOp {
        name: "vcall_indirect",
        dialect: "asm",
        attributes: A {
            callee_reg: "Register",
        },
        roles: R {
            callee_reg: Use,
            clobbers: Clobber,
        },
    }
}
