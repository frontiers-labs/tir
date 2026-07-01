//! A small AST for the subset of LLVM textual IR this prototype understands.
//!
//! The parser is deliberately permissive: any instruction it does not recognise
//! is captured as [`Inst::Unsupported`] with its source text, so the whole
//! module still parses and conversion can report precisely what it cannot lower.

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    /// `iN`
    Int(u32),
    /// `ptr` (opaque) or `T*` (typed, carrying its pointee)
    Ptr(Option<Box<Type>>),
    /// `void`
    Void,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// An SSA value reference, e.g. `%3` or `%sum` (kept verbatim, `%` included).
    Ref(String),
    /// An inline integer literal; materialised as a `builtin.constant` on lowering.
    ConstInt(i64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CastOp {
    SExt,
    ZExt,
    Trunc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inst {
    Binary {
        result: String,
        op: BinOp,
        ty: Type,
        lhs: Operand,
        rhs: Operand,
    },
    ICmp {
        result: String,
        pred: String,
        ty: Type,
        lhs: Operand,
        rhs: Operand,
    },
    Cast {
        result: String,
        op: CastOp,
        from: Type,
        value: Operand,
        to: Type,
    },
    Alloca {
        result: String,
        ty: Type,
    },
    Load {
        result: String,
        ty: Type,
        ptr: Operand,
    },
    Store {
        ty: Type,
        value: Operand,
        ptr: Operand,
    },
    Br {
        dest: String,
    },
    CondBr {
        cond: Operand,
        if_true: String,
        if_false: String,
    },
    Ret {
        value: Option<(Type, Operand)>,
    },
    Call {
        result: Option<String>,
        ret: Type,
        callee: String,
        args: Vec<(Type, Operand)>,
    },
    /// An instruction the parser recognised structurally but that has no TIR
    /// equivalent. Carries the opcode so conversion can fail with a useful
    /// message rather than dropping it silently.
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub label: Option<String>,
    pub insts: Vec<Inst>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub ret: Type,
    pub params: Vec<Param>,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub functions: Vec<Function>,
}
