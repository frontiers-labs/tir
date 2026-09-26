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
    Float(u32),
    Array(u64, Box<Type>),
    Vector(u32, Box<Type>),
    Named(String),
    Struct(Vec<Type>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    /// An SSA value reference, e.g. `%3` or `%sum` (kept verbatim, `%` included).
    Ref(String),
    /// An inline integer literal; materialised as a `builtin.constant` on lowering.
    ConstInt(i64),
    ConstFloat(f64),
    ConstFloatBits(u64),
    Global(String),
    Null,
    Undef,
    Poison,
    GetElementPtr {
        source: Type,
        base: Box<Operand>,
        indices: Vec<(Type, Operand)>,
    },
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
    SDiv,
    UDiv,
    SRem,
    URem,
    FAdd,
    FSub,
    FMul,
    FDiv,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CastOp {
    SExt,
    ZExt,
    Trunc,
    PtrToInt,
    IntToPtr,
    SIToFP,
    UIToFP,
    FPToSI,
    FPToUI,
    FPExt,
    FPTrunc,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Inst {
    Freeze {
        result: String,
        ty: Type,
        value: Operand,
    },
    FNeg {
        result: String,
        ty: Type,
        value: Operand,
    },
    Binary {
        result: String,
        op: BinOp,
        no_signed_wrap: bool,
        no_unsigned_wrap: bool,
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
    FCmp {
        result: String,
        pred: String,
        ty: Type,
        lhs: Operand,
        rhs: Operand,
    },
    Cast {
        result: String,
        op: CastOp,
        non_negative: bool,
        from: Type,
        value: Operand,
        to: Type,
    },
    Alloca {
        result: String,
        ty: Type,
        align: Option<u64>,
    },
    Load {
        result: String,
        ty: Type,
        ptr: Operand,
    },
    ExtractValue {
        result: String,
        aggregate: Type,
        value: Operand,
        indices: Vec<u32>,
    },
    InsertValue {
        result: String,
        aggregate: Type,
        value: Operand,
        element_type: Type,
        element: Operand,
        index: u32,
    },
    ExtractElement {
        result: String,
        vector: Type,
        value: Operand,
        index: Operand,
    },
    InsertElement {
        result: String,
        vector: Type,
        value: Operand,
        element: Operand,
        index: Operand,
    },
    Store {
        ty: Type,
        value: Operand,
        ptr: Operand,
    },
    GetElementPtr {
        result: String,
        source: Type,
        base: Operand,
        indices: Vec<(Type, Operand)>,
    },
    Phi {
        result: String,
        ty: Type,
        incoming: Vec<(Operand, String)>,
    },
    Select {
        result: String,
        cond: Operand,
        ty: Type,
        if_true: Operand,
        if_false: Operand,
    },
    Br {
        dest: String,
    },
    CondBr {
        cond: Operand,
        if_true: String,
        if_false: String,
    },
    Unreachable,
    Ret {
        value: Option<(Type, Operand)>,
    },
    Call {
        result: Option<String>,
        ret: Type,
        callee: Operand,
        args: Vec<CallArg>,
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
    pub abi: AbiAttrs,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AbiAttrs {
    pub sret: Option<Type>,
    pub byval: Option<Type>,
    pub align: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CallArg {
    pub ty: Type,
    pub value: Operand,
    pub abi: AbiAttrs,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: String,
    pub internal: bool,
    pub ret: Type,
    pub params: Vec<Param>,
    pub variadic: bool,
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub named_types: Vec<(String, Type)>,
    pub globals: Vec<Global>,
    pub declarations: Vec<Declaration>,
    pub functions: Vec<Function>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Declaration {
    pub name: String,
    pub ret: Type,
    pub params: Vec<Type>,
    pub variadic: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Global {
    pub name: String,
    pub ty: Type,
    pub initializer: GlobalInitializer,
    pub align: u64,
    pub private: bool,
    pub constant: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GlobalInitializer {
    External,
    Zero,
    Integer(i64),
    CString(Vec<u8>),
    Null,
    Symbols(Vec<String>),
    SymbolDifferences(Vec<SymbolDifference>),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct SymbolDifference {
    pub symbol: String,
    pub base: String,
}
