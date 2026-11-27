use std::{
    hash::{Hash, Hasher},
    sync::Arc,
};
use tir_egglog::{BaseValue, Value};

use crate::{Context, attributes::AttributeValue};

pub(super) fn encode(context: &Context, value: AttributeValue) -> Value {
    register_types(&value, context);
    let engine = context.engine();
    let handle = engine.base_to_value(Stored(Arc::new(value)));
    engine.base_to_value(i64::from(handle.raw()))
}

pub(super) fn get(context: &Context, handle: Value) -> Arc<AttributeValue> {
    let engine = context.engine();
    let raw = engine.value_to_base::<i64>(handle);
    engine
        .value_to_base::<Stored>(Value::new(u32::try_from(raw).expect("attribute handle")))
        .0
}

// Constructor lists interned their tails first. Preserve the allocation order
// of type and entity ids when several new types or classes occur together.
fn register_types(value: &AttributeValue, context: &crate::Context) {
    use AttributeValue::*;
    match value {
        F32(_) => {
            context.terms().float(8, 23);
        }
        F64(_) => {
            context.terms().float(11, 52);
        }
        Register(reg) => context.note_reg_class(reg.class()),
        Array(items) => {
            for item in items.iter().rev() {
                register_types(item, context);
            }
        }
        Dict(fields) => {
            for item in fields.values().rev() {
                register_types(item, context);
            }
        }
        LayoutSpec(spec) => {
            for (_, item) in spec.options.iter().rev() {
                register_types(item, context);
            }
        }
        _ => {}
    }
}

#[derive(Clone, Debug)]
struct Stored(Arc<AttributeValue>);

impl BaseValue for Stored {}

// Public attribute comparisons use IEEE equality. Storage also preserves
// signed zero and each NaN payload inside arrays, dictionaries and layout specs.
struct Bits<'a>(&'a AttributeValue);

impl PartialEq for Bits<'_> {
    fn eq(&self, other: &Self) -> bool {
        use AttributeValue::*;
        match (self.0, other.0) {
            (F32(a), F32(b)) => a.to_bits() == b.to_bits(),
            (F64(a), F64(b)) => a.to_bits() == b.to_bits(),
            (Array(a), Array(b)) => a.iter().map(Bits).eq(b.iter().map(Bits)),
            (Dict(a), Dict(b)) => a
                .iter()
                .map(|(k, v)| (k, Bits(v)))
                .eq(b.iter().map(|(k, v)| (k, Bits(v)))),
            (LayoutSpec(a), LayoutSpec(b)) => {
                a.entries == b.entries
                    && a.options
                        .iter()
                        .map(|(k, v)| (k, Bits(v)))
                        .eq(b.options.iter().map(|(k, v)| (k, Bits(v))))
            }
            (a, b) => a == b,
        }
    }
}

impl Hash for Bits<'_> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        use AttributeValue::*;
        std::mem::discriminant(self.0).hash(state);
        match self.0 {
            Str(v) => v.hash(state),
            Int(v) => v.hash(state),
            UInt(v) => v.hash(state),
            Wide(v) => v.hash(state),
            F32(v) => v.to_bits().hash(state),
            F64(v) => v.to_bits().hash(state),
            Bool(v) => v.hash(state),
            Array(v) => {
                v.len().hash(state);
                for item in v {
                    Bits(item).hash(state);
                }
            }
            Dict(v) => {
                v.len().hash(state);
                for (key, value) in v.iter() {
                    (key, Bits(value)).hash(state);
                }
            }
            LayoutSpec(v) => {
                v.entries.hash(state);
                v.options.len().hash(state);
                for (key, value) in &v.options {
                    (key, Bits(value)).hash(state);
                }
            }
            FpSemantics(v) => v.hash(state),
            EvaluationContract(v) => v.hash(state),
            Register(v) => v.hash(state),
            Predicate(v) => v.hash(state),
            Value(v) => v.hash(state),
            Type(v) => v.hash(state),
            Block(v) => v.hash(state),
        }
    }
}

impl Hash for Stored {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Bits(&self.0).hash(state);
    }
}

impl PartialEq for Stored {
    fn eq(&self, other: &Self) -> bool {
        Bits(&self.0) == Bits(&other.0)
    }
}

impl Eq for Stored {}
