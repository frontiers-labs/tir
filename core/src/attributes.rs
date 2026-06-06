use std::collections::BTreeMap;

use crate::{BlockId, Context, TypeId};

#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    Str(String),
    Int(i64),
    UInt(u64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Array(Vec<AttributeValue>),
    Dict(BTreeMap<String, AttributeValue>),
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

#[derive(Debug, Clone, PartialEq)]
pub enum RegisterAttr {
    Physical { class: String, index: u16 },
    Virtual { id: u32, class: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct NamedAttribute {
    pub name: String,
    pub value: AttributeValue,
}

impl NamedAttribute {
    pub fn new(name: impl Into<String>, value: AttributeValue) -> Self {
        Self {
            name: name.into(),
            value,
        }
    }
}

impl AttributeValue {
    pub fn print(
        &self,
        fmt: &mut crate::IRFormatter,
        context: &Context,
    ) -> Result<(), std::fmt::Error> {
        match self {
            AttributeValue::Str(s) => fmt.write(format!("\"{}\"", s)),
            AttributeValue::Int(i) => fmt.write(i.to_string()),
            AttributeValue::UInt(u) => fmt.write(u.to_string()),
            AttributeValue::F32(fv) => fmt.write(fv.to_string()),
            AttributeValue::F64(fv) => fmt.write(fv.to_string()),
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
                    fmt.write(format!("{}[{}]", class, index))
                }
                RegisterAttr::Virtual { id, class } => {
                    if let Some(cls) = class {
                        fmt.write(format!("%virt{}:{}", id, cls))
                    } else {
                        fmt.write(format!("%virt{}", id))
                    }
                }
            },
            AttributeValue::Type(ty) => context.print_type(*ty, fmt),
            AttributeValue::Block(block) => fmt.write(format!("^bb{}", block.number())),
        }
    }
}

impl From<String> for AttributeValue {
    fn from(value: String) -> Self {
        AttributeValue::Str(value)
    }
}

impl From<&str> for AttributeValue {
    fn from(value: &str) -> Self {
        AttributeValue::Str(value.to_string())
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
        AttributeValue::Array(value)
    }
}

impl From<BTreeMap<String, AttributeValue>> for AttributeValue {
    fn from(value: BTreeMap<String, AttributeValue>) -> Self {
        AttributeValue::Dict(value)
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
