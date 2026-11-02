// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, core-relations/src/base_values/mod.rs. See libs/egglog/LICENSE-MIT.

use std::any::{Any, TypeId};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;

use tir_adt::FxBuildHasher;

use super::Value;

/// A plain Rust type whose values a [`Database`](crate::Database) stores in
/// its cells.
///
/// A value is interned, so equal values get one [`Value`]. A type with small
/// values can set `MAY_UNBOX` and store them in the cell itself instead:
/// `try_box` returns the cell for a value that fits and `try_unbox` reads it
/// back. A type that unboxes some values and interns the rest must keep the
/// top bit of its boxed cells clear: interned cells have it set.
pub trait BaseValue: Clone + Hash + Eq + Debug + Send + Sync + 'static {
    const MAY_UNBOX: bool = false;

    fn try_box(&self) -> Option<Value> {
        None
    }

    fn try_unbox(_value: Value) -> Option<Self> {
        None
    }
}

impl BaseValue for String {}

macro_rules! impl_small_base_value {
    ($($ty:ty),+) => {$(
        impl BaseValue for $ty {
            const MAY_UNBOX: bool = true;
            fn try_box(&self) -> Option<Value> {
                Some(Value::new(*self as u32))
            }
            fn try_unbox(value: Value) -> Option<Self> {
                Some(value.raw() as $ty)
            }
        }
    )+};
}

macro_rules! impl_medium_base_value {
    ($($ty:ty),+) => {$(
        impl BaseValue for $ty {
            const MAY_UNBOX: bool = true;
            fn try_box(&self) -> Option<Value> {
                (*self & (TOP_BIT - 1) as $ty == *self).then(|| Value::new(*self as u32))
            }
            fn try_unbox(value: Value) -> Option<Self> {
                (value.raw() & TOP_BIT == 0).then_some(value.raw() as $ty)
            }
        }
    )+};
}

impl_small_base_value!(u8, u16, i8, i16, u32, i32);
impl_medium_base_value!(u64, i64, usize, isize);

impl BaseValue for bool {
    const MAY_UNBOX: bool = true;

    fn try_box(&self) -> Option<Value> {
        Some(Value::new(u32::from(*self)))
    }

    fn try_unbox(value: Value) -> Option<Self> {
        Some(value.raw() != 0)
    }
}

impl BaseValue for () {
    const MAY_UNBOX: bool = true;

    fn try_box(&self) -> Option<Value> {
        Some(Value::new(0))
    }

    fn try_unbox(_value: Value) -> Option<Self> {
        Some(())
    }
}

/// An `f64` with a total order, so it can be a [`BaseValue`]: two values are
/// equal when their bits are, and they order as [`f64::total_cmp`] does.
#[derive(Clone, Copy, Debug)]
pub struct F64(pub f64);

impl PartialEq for F64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for F64 {}

impl Hash for F64 {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl PartialOrd for F64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for F64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl BaseValue for F64 {}

const TOP_BIT: u32 = 1 << 31;

#[derive(Clone)]
struct InternTable<P> {
    ids: HashMap<P, u32, FxBuildHasher>,
    values: Vec<P>,
}

trait Interned: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn clone_box(&self) -> Box<dyn Interned>;
}

impl<P: BaseValue> Interned for InternTable<P> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn clone_box(&self) -> Box<dyn Interned> {
        Box::new(self.clone())
    }
}

/// The base values of a [`Database`](crate::Database): one intern table per
/// base type, made on the first value of that type. Interning goes through a
/// shared reference, so readers can turn Rust values into cells.
#[derive(Default)]
pub struct BaseValues {
    types: Mutex<HashMap<TypeId, Box<dyn Interned>, FxBuildHasher>>,
}

impl Clone for BaseValues {
    fn clone(&self) -> Self {
        let types = self.types.lock().expect("an intern lock");
        let types = types.iter().map(|(&ty, table)| (ty, table.clone_box()));
        Self {
            types: Mutex::new(types.collect()),
        }
    }
}

impl BaseValues {
    /// The cell of `p`, interning it if it is new.
    pub fn get<P: BaseValue>(&self, p: P) -> Value {
        if P::MAY_UNBOX
            && let Some(value) = p.try_box()
        {
            return value;
        }
        let mut types = self.types.lock().expect("an intern lock");
        let table = types
            .entry(TypeId::of::<P>())
            .or_insert_with(|| {
                Box::new(InternTable::<P> {
                    ids: HashMap::default(),
                    values: Vec::new(),
                })
            })
            .as_any_mut()
            .downcast_mut::<InternTable<P>>()
            .expect("one table per type");
        let next = u32::try_from(table.values.len()).expect("interned values fit in u32");
        let id = *table.ids.entry(p).or_insert_with_key(|p| {
            table.values.push(p.clone());
            next
        });
        let offset = if P::MAY_UNBOX { TOP_BIT } else { 0 };
        Value::new(
            id.checked_add(offset)
                .expect("interned values fit below the top bit"),
        )
    }

    /// The `P` that `value` stands for; `value` must come from
    /// [`BaseValues::get`] with a `P`.
    pub fn unwrap<P: BaseValue>(&self, value: Value) -> P {
        if P::MAY_UNBOX
            && let Some(p) = P::try_unbox(value)
        {
            return p;
        }
        let offset = if P::MAY_UNBOX { TOP_BIT } else { 0 };
        let types = self.types.lock().expect("an intern lock");
        let table = types[&TypeId::of::<P>()]
            .as_any()
            .downcast_ref::<InternTable<P>>()
            .expect("one table per type");
        table.values[(value.raw() - offset) as usize].clone()
    }
}
