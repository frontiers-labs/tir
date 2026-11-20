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

fn bind_unary<I: BaseValue, O: BaseValue>(
    db: &mut tir_egglog::Database,
    name: &str,
    input: tir_egglog::SortId,
    output: tir_egglog::SortId,
    project: impl Fn(&tir_egglog::PrimCtx, I) -> Option<O> + Send + Sync + 'static,
) {
    db.add_primitive(
        name,
        tir_egglog::Signature {
            inputs: vec![input],
            output,
        },
        move |p, args| {
            let [value] = args else { return None };
            Some(p.base(project(p, p.base_value::<I>(*value))?))
        },
    );
}

pub(super) fn bind_integer_attributes(
    db: &mut tir_egglog::Database,
) -> Result<(), tir_egglog::Error> {
    use tir_egglog::{PrimCtx, Signature, sort::BigInt};
    let int = db.sort("i64")?;
    let boolean = db.sort("bool")?;
    let bigint = db.sort("BigInt")?;
    let text = db.sort("String")?;
    let read = |p: &PrimCtx, handle: i64| {
        let raw = u32::try_from(handle).ok()?;
        Some(p.base_value::<Stored>(Value::new(raw)).0)
    };
    bind_unary(db, "eqsat-attr-bool", int, boolean, move |p, handle| {
        let AttributeValue::Bool(value) = *read(p, handle)? else {
            return None;
        };
        Some(value)
    });
    bind_unary(
        db,
        "eqsat-attr-int",
        int,
        bigint,
        move |p, handle| match read(p, handle)?.as_ref() {
            AttributeValue::Int(value) => Some(BigInt::new(*value)),
            AttributeValue::UInt(value) => value.to_string().parse().ok(),
            AttributeValue::Wide(words) => crate::utils::APInt::from_limbs(
                u64::try_from(words.len()).ok()?.checked_mul(64)?,
                words,
            )
            .to_string()
            .parse()
            .ok(),
            _ => None,
        },
    );
    bind_unary(db, "eqsat-attr-case", int, text, move |p, handle| {
        let AttributeValue::Predicate(value) = *read(p, handle)? else {
            return None;
        };
        Some(value.name().to_owned())
    });
    bind_unary(db, "eqsat-amount", bigint, int, |_, value: BigInt| {
        value.to_string().parse::<i64>().ok()
    });
    bind_unary(db, "eqsat-bool-bit", boolean, bigint, |_, value: bool| {
        Some(BigInt::new(i64::from(value)))
    });
    db.add_primitive(
        "eqsat-pick-case",
        Signature {
            inputs: vec![text, text, boolean, boolean],
            output: boolean,
        },
        |p, args| {
            let [key, label, yes, no] = args else {
                return None;
            };
            Some(
                if p.base_value::<String>(*key) == p.base_value::<String>(*label) {
                    *yes
                } else {
                    *no
                },
            )
        },
    );
    Ok(())
}
