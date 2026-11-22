//! Verified integer register programs. Block zero is a function's entry.
//! Only block parameters are live on entry to a block.

mod execute;
mod lower;
mod verify;

pub use execute::{Fault, Load, Outcome};
pub use lower::{BuildError, build};
pub use tir_adt::APInt;
pub use verify::VerifyError;

/// A function-local register, indexed into the function's slot widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reg(pub u32);

/// A function in program order.
#[derive(Debug, Clone, Copy)]
pub struct Func(pub u32);

/// A constant in program order.
#[derive(Debug, Clone, Copy)]
pub struct Const(pub u32);

/// A branch target and its argument list, indexed in the current function.
#[derive(Debug, Clone, Copy)]
pub struct EdgeId(pub u32);

/// A function-local list of registers to transfer.
#[derive(Debug, Clone, Copy)]
pub struct Args(pub u32);

/// Four-word in-memory instructions. Binary operations take destination, left,
/// and right registers. Arithmetic wraps at the operands' common width.
#[repr(C, u32)]
#[derive(Debug, Clone, Copy)]
pub enum Inst {
    Constant(Reg, Const),
    /// Copy the value unless the i1 condition is true or poison.
    PoisonIf(Reg, Reg, Reg),
    Add(Reg, Reg, Reg),
    Sub(Reg, Reg, Reg),
    Eq(Reg, Reg, Reg),
    Ult(Reg, Reg, Reg),
    /// Fail if the i1 input is false.
    Require(Reg),
    /// Suspend with an integer address and the destination slot width.
    Load(Reg, Reg),
    Call(Reg, Func, Args),
    Jump(EdgeId),
    Branch(Reg, EdgeId, EdgeId),
    Return(Reg),
    Trap,
}

const _: () = assert!(size_of::<Inst>() == 4 * size_of::<u32>());

/// A block receives its edge or entry arguments in parameter order.
pub struct Block {
    pub params: Vec<Reg>,
    pub code: Vec<Inst>,
}

/// An edge within a function. Arguments are copied simultaneously.
pub struct Edge {
    pub block: u32,
    pub args: Args,
}

/// All integer widths are bit counts. A function returns one integer.
pub struct Function {
    pub widths: Vec<u32>,
    pub result_width: u32,
    pub blocks: Vec<Block>,
    pub edges: Vec<Edge>,
    pub args: Vec<Vec<Reg>>,
}

/// Assembly tables in index order.
#[derive(Default)]
pub struct Builder {
    pub constants: Vec<APInt>,
    pub functions: Vec<Function>,
}

/// Verified assembly. Execution borrows only this owned program.
pub struct Program(Builder);
