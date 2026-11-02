/// One cell of a table: an e-class id, an interned or unboxed base value, or
/// a container id. The sort of the column says which.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(transparent)]
pub struct Value(u32);

impl Value {
    /// The value whose cell holds `raw`.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The cell this value is stored as.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl From<Value> for u64 {
    fn from(value: Value) -> u64 {
        value.0.into()
    }
}

pub(crate) fn as_cells(values: &[Value]) -> &[u32] {
    // SAFETY: `Value` is a transparent wrapper of `u32`.
    unsafe { std::slice::from_raw_parts(values.as_ptr().cast(), values.len()) }
}

pub(crate) fn as_value(cell: &u32) -> &Value {
    // SAFETY: `Value` is a transparent wrapper of `u32`.
    unsafe { &*std::ptr::from_ref(cell).cast() }
}

macro_rules! define_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        pub struct $name(u32);

        impl $name {
            pub(crate) fn new(index: usize) -> Self {
                Self(u32::try_from(index).expect("id fits in u32"))
            }

            /// Position in declaration order.
            pub fn index(self) -> usize {
                self.0 as usize
            }
        }
    };
}

define_id!(
    /// A sort of a [`Database`](crate::Database).
    SortId
);
define_id!(
    /// A table of a [`Database`](crate::Database).
    TableId
);

impl SortId {
    // The i64 sort, which every database declares first.
    pub(crate) const fn first() -> Self {
        Self(0)
    }
}
define_id!(
    /// A primitive of a [`Database`](crate::Database).
    ExternId
);
define_id!(
    /// An index over one table of a [`Database`](crate::Database).
    IndexId
);
