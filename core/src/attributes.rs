use std::collections::BTreeMap;

use tir_adt::Sym;

use crate::backend::regalloc::RegClassId;
use crate::{BlockId, Context, TypeId};

#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    Str(Box<str>),
    Int(i64),
    UInt(u64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Array(Box<[AttributeValue]>),
    Dict(Box<BTreeMap<String, AttributeValue>>),
    Register(RegisterAttr),
    Type(TypeId),
    /// A reference to a basic block, used by terminators to name their successors
    /// (e.g. the targets of `br`/`cond_br`).
    Block(BlockId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeRole {
    None,
    Def,
    Use,
    Clobber,
    ReadWrite,
}

/// A fixed register an operation reads or writes without naming it in an
/// operand — the register paths its TMDL behavior mentions (x86 `EFLAGS::zf`,
/// `GPR::rax`). Which registers those are is a property of the opcode, so they
/// live in its `InstrInfo`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImplicitReg {
    pub class: RegClassId,
    pub index: u16,
    pub role: AttributeRole,
}

/// Per-opcode register semantics the generic IR cannot see from an instance
/// alone: the def/use role of each register-valued attribute.
///
/// An ordinary op interface: ops that have register semantics (machine
/// instructions; TMDL rustgen derives the tables from the instruction
/// definition) implement it and list it in their `interfaces:`, and analyses
/// resolve it through the interface registry like any other interface. The
/// generic core carries no knowledge of it beyond this definition.
pub trait RegisterSemantics {
    fn verify_interface(
        &self,
        _this: &dyn crate::Operation,
        _context: &Context,
    ) -> Result<(), crate::Error> {
        Ok(())
    }
    fn attribute_roles(&self) -> &'static [(&'static str, AttributeRole)] {
        &[]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RegisterAttr {
    Physical {
        class: RegClassId,
        index: u16,
    },
    Virtual {
        id: u32,
        class: Option<RegClassId>,
    },
    /// A virtual value read from one physical register at this operation.
    FixedUse {
        id: u32,
        class: RegClassId,
        index: u16,
    },
    /// A virtual value defined in one required physical register.
    FixedDef {
        id: u32,
        class: RegClassId,
        index: u16,
    },
}

impl RegisterAttr {
    pub fn class(&self) -> Option<RegClassId> {
        match self {
            Self::Physical { class, .. }
            | Self::FixedUse { class, .. }
            | Self::FixedDef { class, .. } => Some(*class),
            Self::Virtual { class, .. } => *class,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NamedAttribute {
    /// The name, interned in the context that owns the attribute (see
    /// [`Context::intern`]). Comparing attribute names is a `u32` compare.
    pub name: Sym,
    pub value: AttributeValue,
}

impl NamedAttribute {
    pub fn new(name: Sym, value: AttributeValue) -> Self {
        Self { name, value }
    }
}

/// The value of the attribute called `name`, resolving the name through
/// `context`'s interner. For a list not reached through an [`crate::OpInstance`],
/// which answers the same question with [`crate::OpInstance::attr`].
pub fn find_attribute<'a>(
    context: &Context,
    attributes: &'a [NamedAttribute],
    name: &str,
) -> Option<&'a NamedAttribute> {
    let sym = context.sym(name)?;
    attributes.iter().find(|attribute| attribute.name == sym)
}

impl AttributeValue {
    /// The integer this value holds, whichever signedness it was written with.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            AttributeValue::Int(value) => Some(*value),
            AttributeValue::UInt(value) => i64::try_from(*value).ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            AttributeValue::Str(value) => Some(value),
            _ => None,
        }
    }

    pub fn print(
        &self,
        fmt: &mut crate::IRFormatter,
        context: &Context,
    ) -> Result<(), std::fmt::Error> {
        match self {
            AttributeValue::Str(s) => fmt.write(format!("\"{}\"", s)),
            AttributeValue::Int(i) => fmt.write(i.to_string()),
            AttributeValue::UInt(u) => fmt.write(u.to_string()),
            // `{:?}` keeps the decimal point (`3.0`, not `3`) so a float
            // attribute never reparses as an integer.
            AttributeValue::F32(fv) => fmt.write(format!("{:?}", fv)),
            AttributeValue::F64(fv) => fmt.write(format!("{:?}", fv)),
            AttributeValue::Bool(b) => fmt.write(if *b { "true" } else { "false" }),
            AttributeValue::Array(arr) => {
                fmt.write("[")?;
                let mut first = true;
                for v in arr {
                    if !first {
                        fmt.write(", ")?;
                    }
                    first = false;
                    v.print(fmt, context)?;
                }
                fmt.write("]")
            }
            AttributeValue::Dict(map) => {
                if let Some(alias) = fmt.attribute_alias(self).map(str::to_string) {
                    return fmt.write(format!("#{alias}"));
                }
                fmt.write("{")?;
                let mut first = true;
                for (k, v) in map.iter() {
                    if !first {
                        fmt.write(", ")?;
                    }
                    first = false;
                    fmt.write(k)?;
                    fmt.write(" = ")?;
                    v.print(fmt, context)?;
                }
                fmt.write("}")
            }
            AttributeValue::Register(r) => match r {
                RegisterAttr::Physical { class, index } => {
                    fmt.write(format!("{}[{}]", class.name(), index))
                }
                RegisterAttr::Virtual { id, class } => {
                    if let Some(cls) = class {
                        fmt.write(format!("%virt{}:{}", id, cls.name()))
                    } else {
                        fmt.write(format!("%virt{}", id))
                    }
                }
                RegisterAttr::FixedUse { id, class, index } => {
                    fmt.write(format!("%virt{}:{}@{}", id, class.name(), index))
                }
                RegisterAttr::FixedDef { id, class, index } => {
                    fmt.write(format!("%virt{}:{}@{}", id, class.name(), index))
                }
            },
            AttributeValue::Type(ty) => context.print_type(*ty, fmt),
            AttributeValue::Block(block) => {
                fmt.write(format!("^bb{}", fmt.region_block_number(*block)))
            }
        }
    }
}

impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        AttributeValue::Str(value.into())
    }
}

impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        AttributeValue::Str(value.into())
    }
}

impl From<i64> for AttributeValue {
    fn from(value: i64) -> Self {
        AttributeValue::Int(value)
    }
}

impl From<i32> for AttributeValue {
    fn from(value: i32) -> Self {
        AttributeValue::Int(value as i64)
    }
}

impl From<i16> for AttributeValue {
    fn from(value: i16) -> Self {
        AttributeValue::Int(value as i64)
    }
}

impl From<i8> for AttributeValue {
    fn from(value: i8) -> Self {
        AttributeValue::Int(value as i64)
    }
}

impl From<u64> for AttributeValue {
    fn from(value: u64) -> Self {
        AttributeValue::UInt(value)
    }
}

impl From<u32> for AttributeValue {
    fn from(value: u32) -> Self {
        AttributeValue::UInt(value as u64)
    }
}

impl From<u16> for AttributeValue {
    fn from(value: u16) -> Self {
        AttributeValue::UInt(value as u64)
    }
}

impl From<u8> for AttributeValue {
    fn from(value: u8) -> Self {
        AttributeValue::UInt(value as u64)
    }
}

impl From<f32> for AttributeValue {
    fn from(value: f32) -> Self {
        AttributeValue::F32(value)
    }
}

impl From<f64> for AttributeValue {
    fn from(value: f64) -> Self {
        AttributeValue::F64(value)
    }
}

impl From<bool> for AttributeValue {
    fn from(value: bool) -> Self {
        AttributeValue::Bool(value)
    }
}

impl From<Vec<AttributeValue>> for AttributeValue {
    fn from(value: Vec<AttributeValue>) -> Self {
        AttributeValue::Array(value.into())
    }
}

impl From<BTreeMap<String, AttributeValue>> for AttributeValue {
    fn from(value: BTreeMap<String, AttributeValue>) -> Self {
        AttributeValue::Dict(Box::new(value))
    }
}

impl From<RegisterAttr> for AttributeValue {
    fn from(value: RegisterAttr) -> Self {
        AttributeValue::Register(value)
    }
}

impl From<TypeId> for AttributeValue {
    fn from(value: TypeId) -> Self {
        AttributeValue::Type(value)
    }
}

impl From<BlockId> for AttributeValue {
    fn from(value: BlockId) -> Self {
        AttributeValue::Block(value)
    }
}

/// An attribute value sits inline in every op that carries it, so its size is a
/// per-op cost paid across the whole program. The payloads that do not fit the
/// budget are the rare ones, and they are boxed.
const _: () = assert!(std::mem::size_of::<AttributeValue>() <= 24);
