//! Entity records and their encoding in the core relational schema.
use super::Context;
use crate::{BlockId, OpId, RegionId};
use smallvec::SmallVec;
use tir_egglog::{BaseValues, Database, FromValues, IntoValues, Value};

type Cells = SmallVec<[Value; 8]>;

pub(super) enum Placement {
    Root,
    Block(BlockId, usize),
    Graph(RegionId, usize),
}

#[derive(Clone, Copy)]
pub(super) enum Defines {
    Result(OpId),
    BlockArgument(BlockId),
    RegionPort(RegionId),
}

impl Defines {
    pub(super) fn key(self) -> (&'static str, u32) {
        match self {
            Self::Result(op) => ("result", op.raw()),
            Self::BlockArgument(block) => ("block", block.raw()),
            Self::RegionPort(region) => ("region", region.raw()),
        }
    }
}

pub(super) trait Record: Sized {
    const TABLE: &'static str;
    fn decode(cells: &[Value], base: &BaseValues) -> Self;
    fn encode(self, base: &BaseValues) -> Cells;

    fn write(self, context: &Context) {
        let mut engine = context.engine();
        let db = engine.database_mut();
        let table = db.table(Self::TABLE).expect("declared entity relation");
        let cells = self.encode(db.base_values());
        db.insert(table, &cells).expect("valid entity row");
    }
}

macro_rules! record {
    ($name:ident, $table:literal, { $($field:ident: $ty:ty),+ $(,)? }) => {
        pub(super) struct $name {
            pub id: u32,
            $(pub $field: $ty),+
        }

        impl Record for $name {
            const TABLE: &'static str = $table;

            fn decode(cells: &[Value], base: &BaseValues) -> Self {
                let (id, $($field),+): (i64, $($ty),+) = FromValues::from_values(cells, base);
                Self { id: id as u32, $($field),+ }
            }

            fn encode(self, base: &BaseValues) -> Cells {
                (i64::from(self.id), $(self.$field),+).into_values(base).collect()
            }
        }
    };
}

record!(OperationRow, "Operation", {
    kind: String, placement: String, container: i64, position: i64, display: i64, location: i64,
});
record!(RegionRow, "Region", { mode: String, owner: i64, slot: i64, entry: i64 });
record!(BlockRow, "Block", { region: i64, layout: i64 });
record!(ValueRow, "Value", { ty: i64, definition: String, owner: i64, slot: i64, declared: i64 });

fn entity<R: Record>(db: &mut Database, id: u32) -> Option<Cells> {
    let table = db.table(R::TABLE).expect("declared entity relation");
    let index = db.add_kept_index(table, &[0], &[]);
    let key = [db.base(i64::from(id))];
    db.listed_nth(index, &key, 0).map(|r| r.values().collect())
}

impl Context {
    pub(super) fn record<R: Record>(&self, id: u32) -> Option<R> {
        let mut engine = self.engine();
        let db = engine.database_mut();
        let row = entity::<R>(db, id)?;
        Some(R::decode(&row, db.base_values()))
    }

    pub(super) fn edit_record<R: Record>(&self, id: u32, edit: impl FnOnce(&mut R)) {
        let mut engine = self.engine();
        let db = engine.database_mut();
        let old = entity::<R>(db, id).expect("live entity");
        let mut record = R::decode(&old, db.base_values());
        edit(&mut record);
        let new = record.encode(db.base_values());
        if old == new {
            return;
        }
        let table = db.table(R::TABLE).expect("declared entity relation");
        db.remove(table, &old);
        if let Err(error) = db.insert(table, &new) {
            db.insert(table, &old).expect("previously valid entity");
            panic!("invalid {} edit: {error}", R::TABLE);
        }
    }
}

impl Context {
    pub(crate) fn operation_kind(&self, id: crate::OpId) -> Option<String> {
        self.record::<OperationRow>(id.number()).map(|row| row.kind)
    }
}
