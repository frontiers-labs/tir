//! The data layout in scope at an operation: the software half of a target's
//! ABI — byte order, the size and alignment of each data type, and the stack
//! alignment.
//!
//! A layout is a [`data_layout`](DATA_LAYOUT) dict attribute, resolved through
//! the enclosing scopes by [`scoped_dict`]. Every size and alignment is
//! expressed in **bits**, matching the width of an
//! [`IntegerType`](crate::builtin::IntegerType).
//!
//! ```text
//! #data_layout = {
//!   endianness = "little",
//!   stack_alignment = 128,
//!   types = {i32 = {abi = 32}, i64 = {abi = 64}, p = {size = 64, abi = 64}}
//! }
//! ```
//!
//! Type entries key on the type's layout class rather than its full spelling:
//! `i{width}` for integers, the mnemonic (`f32`, `bf16`) for floats and `p` for
//! pointers. Lookup is by exact key — a type whose class is not declared has no
//! layout, rather than borrowing a wider entry's.

use std::any::Any;

use crate::attributes::AttributeValue;
use crate::builtin::{FloatType, IntegerType};
use crate::scoped_attr::invalid_spec;
use crate::{AttributeDict, Context, OpId, TypeId, ptr::PtrType, scoped_dict};

/// The attribute a data layout spec is carried in.
pub const DATA_LAYOUT: &str = "data_layout";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endianness {
    Little,
    Big,
}

pub struct DataLayout {
    entries: AttributeDict,
}

impl DataLayout {
    /// The layout in scope at `op`, or `None` when no enclosing scope declares one.
    pub fn for_op(context: &Context, op: OpId) -> Option<Self> {
        scoped_dict(context, op, DATA_LAYOUT).map(|entries| Self { entries })
    }

    /// The layout in scope at `instance`: the scope chain of its id, falling back
    /// to the dict the instance carries itself. A detached instance — an op type
    /// probed with no IR home — belongs to no scope, so the layout resolved where
    /// the code came from rides along as its own attribute.
    pub fn for_instance(context: &Context, instance: &crate::OpHandle) -> Option<Self> {
        Self::for_op(context, instance.id).or_else(|| {
            instance
                .attr(DATA_LAYOUT)
                .as_ref()
                .and_then(Self::from_value)
        })
    }

    /// The layout in scope at `op` over `default`: the target's own spec acts as
    /// the outermost scope, so IR entries override it key by key while entries
    /// the IR never mentions keep the target's value.
    pub fn for_op_with_default(
        context: &Context,
        op: OpId,
        default: Option<&AttributeValue>,
    ) -> Option<Self> {
        let Some(AttributeValue::Dict(default)) = default else {
            return Self::for_op(context, op);
        };
        let mut entries = (**default).clone();
        if let Some(declared) = scoped_dict(context, op, DATA_LAYOUT) {
            crate::scoped_attr::merge_into(&mut entries, declared);
        }
        Some(Self { entries })
    }

    /// A layout read from a spec that is not attached to the IR — a target's
    /// default, which applies where the IR itself declares nothing.
    pub fn from_value(value: &AttributeValue) -> Option<Self> {
        match value {
            AttributeValue::Dict(entries) => Some(Self {
                entries: (**entries).clone(),
            }),
            _ => None,
        }
    }

    pub fn endianness(&self) -> Option<Endianness> {
        match self.entries.get("endianness")? {
            AttributeValue::Str(value) if &**value == "little" => Some(Endianness::Little),
            AttributeValue::Str(value) if &**value == "big" => Some(Endianness::Big),
            _ => None,
        }
    }

    /// Alignment the stack pointer is kept at, in bits.
    pub fn stack_alignment(&self) -> Option<u32> {
        bits(self.entries.get("stack_alignment"))
    }

    /// Bytes a cache line holds, which is what a locality model counts in.
    pub fn cache_line(&self) -> Option<u32> {
        bits(self.entries.get("cache_line"))
    }

    /// Width of a pointer, in bits.
    pub fn pointer_size(&self) -> Option<u32> {
        self.class_layout("p").map(|(size, _)| size)
    }

    /// Width of an index, in bits. An index addresses memory, so it is as wide
    /// as a pointer.
    pub fn index_width(&self) -> Option<u32> {
        self.pointer_size()
    }

    /// Size and ABI alignment of a layout class named directly (`i32`, `f64`,
    /// `p`), in bits. For front ends, which map their own types onto a class
    /// before they hold a TIR type to key on.
    pub fn class_layout(&self, class: &str) -> Option<(u32, u32)> {
        let entry = self.type_entry(class)?;
        Some((bits(entry.get("size"))?, bits(entry.get("abi"))?))
    }

    /// Width of `ty` in bits: the declared `size` of its layout class, falling
    /// back to the type's own width where it has one.
    pub fn size_in_bits(&self, context: &Context, ty: TypeId) -> Option<u32> {
        let key = layout_key(context, ty)?;
        self.type_entry(&key)
            .and_then(|entry| bits(entry.get("size")))
            .or_else(|| natural_width(context, ty))
    }

    /// Alignment `ty` must be given to satisfy the ABI, in bits.
    pub fn abi_alignment(&self, context: &Context, ty: TypeId) -> Option<u32> {
        let key = layout_key(context, ty)?;
        bits(self.type_entry(&key)?.get("abi"))
    }

    /// Alignment `ty` is given when nothing constrains it, in bits. Defaults to
    /// the ABI alignment.
    pub fn preferred_alignment(&self, context: &Context, ty: TypeId) -> Option<u32> {
        let key = layout_key(context, ty)?;
        let entry = self.type_entry(&key)?;
        bits(entry.get("preferred")).or_else(|| bits(entry.get("abi")))
    }

    /// A spec entry outside the predefined set, for metadata a dialect defines
    /// itself (e.g. a GPU address-space mapping).
    pub fn get(&self, key: &str) -> Option<&AttributeValue> {
        self.entries.get(key)
    }

    fn type_entry(&self, key: &str) -> Option<&AttributeDict> {
        let AttributeValue::Dict(types) = self.entries.get("types")? else {
            return None;
        };
        match types.get(key)? {
            AttributeValue::Dict(entry) => Some(entry),
            _ => None,
        }
    }
}

/// Fields a type entry may carry.
const TYPE_FIELDS: [&str; 3] = ["size", "abi", "preferred"];

/// Builds a [`DATA_LAYOUT`] spec, the form a target describes its own ABI in.
/// `types` gives each layout class its size and ABI alignment in bits.
pub fn data_layout_spec(
    endianness: Endianness,
    stack_alignment: u32,
    types: &[(&str, u32, u32)],
) -> AttributeValue {
    let order = match endianness {
        Endianness::Little => "little",
        Endianness::Big => "big",
    };
    let types = types.iter().map(|(name, size, abi)| {
        let entry = [
            ("size".to_string(), AttributeValue::UInt(u64::from(*size))),
            ("abi".to_string(), AttributeValue::UInt(u64::from(*abi))),
        ];
        (
            name.to_string(),
            AttributeValue::Dict(Box::new(entry.into())),
        )
    });
    AttributeValue::Dict(Box::new(
        [
            ("endianness".to_string(), AttributeValue::from(order)),
            (
                "stack_alignment".to_string(),
                AttributeValue::UInt(u64::from(stack_alignment)),
            ),
            ("cache_line".to_string(), AttributeValue::UInt(64)),
            (
                "types".to_string(),
                AttributeValue::Dict(Box::new(types.collect())),
            ),
        ]
        .into(),
    ))
}

/// Checks the shape of a [`DATA_LAYOUT`] spec. Entries outside the predefined
/// set are an extension point and pass unexamined.
pub(crate) fn verify_spec(value: &AttributeValue) -> Result<(), crate::Error> {
    let AttributeValue::Dict(entries) = value else {
        return Err(invalid_spec("data_layout must be a dictionary"));
    };

    for (key, value) in entries.iter() {
        match key.as_str() {
            "endianness" => match value {
                AttributeValue::Str(order) if &**order == "little" || &**order == "big" => {}
                _ => {
                    return Err(invalid_spec(
                        "data_layout 'endianness' must be \"little\" or \"big\"",
                    ));
                }
            },
            "stack_alignment" if bits(Some(value)).is_none() => {
                return Err(invalid_spec(
                    "data_layout 'stack_alignment' must be a bit count",
                ));
            }
            "types" => verify_types(value)?,
            _ => {}
        }
    }
    Ok(())
}

fn verify_types(value: &AttributeValue) -> Result<(), crate::Error> {
    let AttributeValue::Dict(types) = value else {
        return Err(invalid_spec("data_layout 'types' must be a dictionary"));
    };

    for (name, entry) in types.iter() {
        let AttributeValue::Dict(fields) = entry else {
            return Err(invalid_spec(format!(
                "data_layout type entry '{name}' must be a dictionary"
            )));
        };
        for (field, value) in fields.iter() {
            if !TYPE_FIELDS.contains(&field.as_str()) {
                return Err(invalid_spec(format!(
                    "unknown data_layout field '{field}' in type entry '{name}' (expected {})",
                    TYPE_FIELDS.join(", ")
                )));
            }
            if bits(Some(value)).is_none() {
                return Err(invalid_spec(format!(
                    "data_layout '{name}.{field}' must be a bit count"
                )));
            }
        }
    }
    Ok(())
}

/// The spec key describing `ty`'s layout class, or `None` for a type whose
/// layout the spec cannot express (an aggregate, say).
fn layout_key(context: &Context, ty: TypeId) -> Option<String> {
    let data = context.get_type_data(ty);
    let data = data.as_ref() as &dyn Any;
    if let Some(int) = data.downcast_ref::<IntegerType>() {
        return Some(format!("i{}", int.width()));
    }
    if data.downcast_ref::<FloatType>().is_some() {
        // Float classes are named by their mnemonic (`f32`, `bf16`), which is
        // exactly how the type prints.
        return Some(
            context
                .type_to_string(ty)
                .trim_start_matches('!')
                .to_string(),
        );
    }
    if data.downcast_ref::<PtrType>().is_some() {
        return Some("p".to_string());
    }
    None
}

fn natural_width(context: &Context, ty: TypeId) -> Option<u32> {
    let data = context.get_type_data(ty);
    let data = data.as_ref() as &dyn Any;
    if let Some(int) = data.downcast_ref::<IntegerType>() {
        return Some(int.width());
    }
    data.downcast_ref::<FloatType>().map(FloatType::bit_width)
}

/// Reads a bit count, which the IR parser yields as a plain (signed) integer.
fn bits(value: Option<&AttributeValue>) -> Option<u32> {
    match value? {
        AttributeValue::Int(value) => u32::try_from(*value).ok(),
        AttributeValue::UInt(value) => u32::try_from(*value).ok(),
        _ => None,
    }
}
