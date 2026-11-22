use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
};

use tir_egglog::{Database, DatabaseId, Error, TableId, Value};

use super::{Context, ir_program, records};
use crate::{BlockId, OpId, RegionId, ValueId};
use records::{BlockRow, OperationRow, Record, RegionRow, ValueRow};

type ResolveTable = fn(&Database) -> Result<TableId, Error>;

#[derive(Clone, Copy)]
enum Fact {
    Operation,
    Block,
    Region,
    Value,
    OwnedByOperation,
    OwnedByRegion,
    Operand,
    Yield,
    Attribute,
    Type,
    TypeConstructor,
    Registry,
}

const TABLES: &[(ResolveTable, Fact)] = &[
    (ir_program::table_operation, Fact::Operation),
    (ir_program::table_block, Fact::Block),
    (ir_program::table_region, Fact::Region),
    (ir_program::table_value, Fact::Value),
    (ir_program::table_operand, Fact::Operand),
    (ir_program::table_successor, Fact::OwnedByOperation),
    (ir_program::table_successor_argument, Fact::OwnedByOperation),
    (ir_program::table_control, Fact::OwnedByOperation),
    (ir_program::table_arm, Fact::OwnedByOperation),
    (ir_program::table_yield, Fact::Yield),
    (ir_program::table_binding, Fact::OwnedByRegion),
    (ir_program::table_attribute, Fact::Attribute),
    (ir_program::table_type_term, Fact::Type),
    (ir_program::table_type_ctor, Fact::TypeConstructor),
    (ir_program::table_symbol, Fact::Registry),
    (ir_program::table_op_spec, Fact::Registry),
];

pub(super) struct Revisions {
    tables: Box<[(TableId, Fact)]>,
    observed: RefCell<Observed>,
}

#[derive(Default)]
struct Observed {
    database: Option<DatabaseId>,
    revision: u64,
    roots: HashMap<OpId, u64>,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum Owner {
    Operation(OpId),
    Block(BlockId),
    Region(RegionId),
}

impl Owner {
    fn operation(id: u32) -> Self {
        Self::Operation(OpId::new(id))
    }

    fn block(id: u32) -> Self {
        Self::Block(BlockId::new(id))
    }

    fn region(id: u32) -> Self {
        Self::Region(RegionId::new(id))
    }

    fn named(kind: &str, id: i64) -> Option<Self> {
        let id = u32::try_from(id).ok()?;
        match kind {
            "operation" | "result" => Some(Self::operation(id)),
            "block" => Some(Self::block(id)),
            "region" | "graph" => Some(Self::region(id)),
            _ => None,
        }
    }

    fn parent(self, context: &Context) -> Option<Self> {
        match self {
            Self::Operation(op) => {
                let row = context.record::<OperationRow>(op.raw())?;
                Self::named(&row.placement, row.container)
            }
            Self::Block(block) => context.parent_region(block).map(Self::Region),
            Self::Region(region) => context.region_parent_op(region).map(Self::Operation),
        }
    }
}

#[derive(Default)]
struct Changed {
    owners: HashSet<Owner>,
    values: HashSet<ValueId>,
    used_values: HashSet<ValueId>,
    types: HashSet<Value>,
    constructors: bool,
}

impl Changed {
    fn record(&mut self, db: &Database, fact: Fact, cells: &[Value]) {
        let base = db.base_values();
        let ordinal = |column: usize| db.base_value::<i64>(cells[column]);
        let owner = match fact {
            Fact::Operation => {
                let row = OperationRow::decode(cells, base);
                self.owners.insert(Owner::operation(row.id));
                Owner::named(&row.placement, row.container)
            }
            Fact::Block => {
                let row = BlockRow::decode(cells, base);
                u32::try_from(row.region).ok().map(Owner::region)
            }
            Fact::Region => {
                let row = RegionRow::decode(cells, base);
                u32::try_from(row.owner).ok().map(Owner::operation)
            }
            Fact::Value => {
                let row = ValueRow::decode(cells, base);
                self.values.insert(ValueId::from_number(row.id));
                Owner::named(&row.definition, row.owner)
            }
            Fact::OwnedByOperation => Some(Owner::operation(ordinal(0) as u32)),
            Fact::OwnedByRegion => Some(Owner::region(ordinal(0) as u32)),
            Fact::Operand | Fact::Yield => {
                self.used_values
                    .insert(ValueId::from_number(ordinal(2) as u32));
                Some(match fact {
                    Fact::Operand => Owner::operation(ordinal(0) as u32),
                    _ => Owner::region(ordinal(0) as u32),
                })
            }
            Fact::Attribute => Owner::named(&db.base_value::<String>(cells[0]), ordinal(1)),
            Fact::Type => {
                self.types.insert(cells[0]);
                None
            }
            Fact::TypeConstructor => {
                self.constructors = true;
                None
            }
            Fact::Registry => None,
        };
        self.owners.extend(owner);
    }

    fn include_value_dependencies(&mut self, context: &Context) {
        for ty in self.types.drain() {
            for column in [1, 5] {
                for row in context.indexed(ValueRow::TABLE, &[column], &[ty], &[]) {
                    let id = ValueId::from_number(context.integer(row[0]) as u32);
                    self.values.insert(id);
                    let kind = context.string(row[2]);
                    self.owners
                        .extend(Owner::named(&kind, context.integer(row[3])));
                }
            }
        }
        for value in self.used_values.drain() {
            if let Some(row) = context.record::<ValueRow>(value.number()) {
                self.owners.extend(Owner::named(&row.definition, row.owner));
            }
        }
        for value in self.values.drain() {
            self.owners.extend(
                context
                    .uses_of(value)
                    .into_iter()
                    .map(|site| Owner::Operation(site.op)),
            );
            self.owners.extend(
                context
                    .result_uses(value)
                    .into_iter()
                    .map(|(region, _)| Owner::Region(region)),
            );
        }
    }
}

impl Revisions {
    pub(super) fn new(db: &Database) -> Self {
        Self {
            tables: TABLES
                .iter()
                .map(|(resolve, fact)| (resolve(db).expect("core IR relation"), *fact))
                .collect(),
            observed: RefCell::default(),
        }
    }

    pub(super) fn all(&self, db: &Database) -> (DatabaseId, u64) {
        (
            db.id(),
            self.tables
                .iter()
                .map(|&(table, _)| db.table_version(table))
                .sum(),
        )
    }

    pub(super) fn subtree(&self, context: &Context, root: OpId) -> u64 {
        let mut observed = self.observed.borrow_mut();
        let mut changed = Changed::default();
        let watched = || {
            self.tables
                .iter()
                .copied()
                .filter(|(_, fact)| !matches!(fact, Fact::Registry))
        };
        {
            let mut engine = context.engine();
            let db = engine.database_mut();
            if observed.database != Some(db.id()) {
                for (table, _) in watched() {
                    db.watch_rows(table);
                }
                observed.database = Some(db.id());
                observed.revision += 1;
                observed.roots.clear();
            }
            for (table, fact) in watched() {
                for change in db.take_row_changes(table) {
                    changed.record(db, fact, &change);
                }
            }
        }
        changed.include_value_dependencies(context);
        if changed.constructors || !changed.owners.is_empty() {
            observed.revision += 1;
            let revision = observed.revision;
            if changed.constructors {
                observed.roots.clear();
            } else {
                // The changed rows seed both sides of a move. Follow the current
                // containment links upward once per owner in the batch.
                let mut seen = HashSet::new();
                for owner in changed.owners {
                    let mut next = Some(owner);
                    while let Some(owner) = next.filter(|&owner| seen.insert(owner)) {
                        if let Owner::Operation(op) = owner
                            && let Some(stamp) = observed.roots.get_mut(&op)
                        {
                            *stamp = revision;
                        }
                        next = owner.parent(context);
                    }
                }
            }
        }
        let revision = observed.revision;
        *observed.roots.entry(root).or_insert(revision)
    }
}
