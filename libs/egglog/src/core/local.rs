use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Weak};

use tir_adt::FxBuildHasher;
use tir_adt::simd::hash_row;

use super::database::{RowIds, Write};
use super::uf::ClassRoots;
use super::{
    Color, Database, DatabaseId, Error, IndexId, Rows, Table, TableId, TableKind, TableSchema,
    Value,
};
use crate::{Extractor, TermDag, TermId};

#[derive(Debug)]
struct Lease {
    database: DatabaseId,
    table: TableId,
}

/// Keeps the rows and indexes of a database-local table alive.
#[derive(Clone, Debug)]
pub struct LocalTable(Arc<Lease>);

impl LocalTable {
    /// Gets the table ID in the owning database. Keep a handle alive while using the ID.
    pub fn id(&self, db: &Database) -> Result<TableId, Error> {
        if self.0.database != db.id() {
            return Err(Error::ForeignLocalTable);
        }
        Ok(self.0.table)
    }

    /// Captures the current rows and e-class equality of the rebuilt color.
    pub fn capture(&self, db: &Database, color: Color) -> Result<CapturedTable, Error> {
        self.capture_with_terms(db, color, &[])
    }

    /// Also captures the extractable terms of the e-classes in `columns` of
    /// the nonsubsumed rows.
    pub fn capture_with_terms(
        &self,
        db: &Database,
        color: Color,
        columns: &[usize],
    ) -> Result<CapturedTable, Error> {
        let id = self.id(db)?;
        if !db.colors.contains(color) {
            return Err(Error::Unsupported(
                "cannot capture a table in a dropped color".to_owned(),
            ));
        }
        let schema = &db.tables[id.index()].schema;
        if !columns.iter().all(|column| schema.ids.contains(column)) {
            return Err(Error::Unsupported(
                "captured term columns must hold e-classes".to_owned(),
            ));
        }
        let mut table = Table::new(schema.clone());
        let mut cells = Vec::with_capacity(schema.columns.len());
        let mut roots = BTreeSet::new();
        for row in db.rows_in(color, id) {
            cells.clear();
            cells.extend(row.values().map(Value::raw));
            let subsumed = row.is_subsumed();
            if !subsumed {
                roots.extend(columns.iter().map(|&column| db.find_in(color, row[column])));
            }
            table.push(
                &cells,
                hash_row(&cells[..schema.keys]),
                row.timestamp(),
                subsumed,
            );
        }
        let mut terms = TermDag::default();
        let term_roots = if roots.is_empty() {
            HashMap::default()
        } else {
            Extractor::new_in(db, color).best_terms(&mut terms, roots)
        };
        Ok(CapturedTable(Arc::new(Captured {
            local: self.clone(),
            table,
            base: db.uf.capture(),
            color: db.colors.capture_classes(color),
            terms,
            term_roots,
        })))
    }
}

/// The visible rows and e-class equality of a local table at capture time.
#[derive(Clone)]
pub struct CapturedTable(Arc<Captured>);

struct Captured {
    local: LocalTable,
    table: Table,
    base: Arc<ClassRoots>,
    color: ClassRoots,
    terms: TermDag,
    term_roots: HashMap<Value, TermId, FxBuildHasher>,
}

impl CapturedTable {
    /// The live local table handle for binding the table to a compiled schedule.
    pub fn table(&self) -> &LocalTable {
        &self.0.local
    }

    /// The captured rows, including subsumed rows.
    pub fn rows(&self) -> Rows<'_> {
        Rows {
            table: &self.0.table,
            ids: RowIds::Log(0..self.0.table.len() as u32),
        }
    }

    /// Resolves an e-class in the equality view at capture time.
    pub fn find(&self, value: Value) -> Value {
        let id = value.raw();
        let root = self.0.base.get(&id).copied().unwrap_or(id);
        Value::new(self.0.color.get(&root).copied().unwrap_or(root))
    }

    /// The captured term for an e-class in the requested columns.
    pub fn best(&self, value: Value) -> Result<(&TermDag, TermId), Error> {
        self.0
            .term_roots
            .get(&self.find(value))
            .map(|&term| (&self.0.terms, term))
            .ok_or_else(|| Error::ExtractError(format!("no term for {value:?}")))
    }
}

#[derive(PartialEq, Eq, Hash)]
struct MemoKey {
    function: &'static str,
    args: Vec<Value>,
    color: Color,
}

struct MemoEntry {
    revision: Vec<u64>,
    result: Box<dyn Any + Send + Sync>,
}

#[derive(Clone)]
struct LocalSlot {
    lease: Weak<Lease>,
    dependencies: Vec<TableId>,
}

#[derive(Default)]
pub(crate) struct LocalTables {
    // A vacant slot stays out of the named schema until an anonymous table reuses it.
    slots: BTreeMap<TableId, Option<LocalSlot>>,
    pub(crate) vacant_tables: Vec<TableId>,
    pub(crate) vacant_indexes: Vec<IndexId>,
    memo: HashMap<MemoKey, MemoEntry, FxBuildHasher>,
}

impl Clone for LocalTables {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            vacant_tables: self.vacant_tables.clone(),
            vacant_indexes: self.vacant_indexes.clone(),
            memo: HashMap::default(),
        }
    }
}

impl LocalTables {
    pub(crate) fn contains(&self, table: TableId) -> bool {
        self.slots.contains_key(&table)
    }

    fn vacant(&self, table: TableId) -> bool {
        matches!(self.slots.get(&table), Some(None))
    }

    pub(crate) fn forget_colors(&mut self, gone: &[bool]) {
        self.memo
            .retain(|key, _| !gone.get(key.color.index()).is_some_and(|&dropped| dropped));
    }
}

impl Database {
    pub(crate) fn install_local(&mut self, id: TableId, schema: TableSchema) {
        let table = Table::new(schema);
        if id.index() == self.tables.len() {
            self.tables.push(table);
        } else {
            let vacant = self.locals.vacant_tables.pop();
            debug_assert_eq!(vacant, Some(id));
            self.colors.reset_table(id, &table.schema);
            self.tables[id.index()] = table;
        }
    }

    pub(crate) fn lease_local_table(
        &mut self,
        table: TableId,
        dependencies: Vec<TableId>,
    ) -> LocalTable {
        let lease = Arc::new(Lease {
            database: self.id(),
            table,
        });
        self.locals.slots.insert(
            table,
            Some(LocalSlot {
                lease: Arc::downgrade(&lease),
                dependencies,
            }),
        );
        LocalTable(lease)
    }

    /// Releases storage for local tables once their last handle has been dropped.
    pub fn collect_local_tables(&mut self) {
        let owner = self.id();
        let slots = &mut self.locals.slots;
        let mut retained = BTreeSet::new();
        let mut pending: Vec<TableId> = slots
            .values()
            .flatten()
            .filter_map(|slot| slot.lease.upgrade())
            .filter(|lease| lease.database == owner)
            .map(|lease| lease.table)
            .collect();
        while let Some(table) = pending.pop() {
            if retained.insert(table)
                && let Some(Some(slot)) = slots.get(&table)
            {
                pending.extend(&slot.dependencies);
            }
        }
        let mut collected = false;
        for (&id, slot) in slots {
            if slot.is_none() || retained.contains(&id) {
                continue;
            }
            *slot = None;
            collected = true;
            self.locals.vacant_tables.push(id);
            let schema = vacant_schema();
            self.colors.reset_table(id, &schema);
            self.tables[id.index()] = Table::new(schema);
        }
        if !collected {
            return;
        }
        for (i, (table, at)) in self.indexes.iter_mut().enumerate() {
            if *at != usize::MAX && self.locals.vacant(*table) {
                *at = usize::MAX;
                self.locals.vacant_indexes.push(IndexId::new(i));
            }
        }
        self.pending.retain(|write| {
            let (Write::Insert(table, _) | Write::Remove(table, _) | Write::Subsume(table, _)) =
                write;
            !self.locals.vacant(*table)
        });
        self.predicted
            .retain(|(table, _), _| !self.locals.vacant(*table));
    }

    /// Clones the database and retains the specified local tables in the clone.
    /// The returned handles follow the order in `tables`.
    pub fn clone_with_local_tables(
        &self,
        tables: &[&LocalTable],
    ) -> Result<(Self, Vec<LocalTable>), Error> {
        for table in tables {
            table.id(self)?;
        }
        let mut db = self.clone_storage();
        let mut retained = BTreeMap::new();
        let results = tables
            .iter()
            .map(|table| {
                let id = table.0.table;
                let handle = retained.entry(id).or_insert_with(|| {
                    let dependencies = self.locals.slots[&id]
                        .as_ref()
                        .map_or_else(Vec::new, |slot| slot.dependencies.clone());
                    db.lease_local_table(id, dependencies)
                });
                handle.clone()
            })
            .collect();
        db.collect_local_tables();
        Ok((db, results))
    }

    /// Gets a memoized result for this view and drops any stale result for the same key.
    pub fn memo_result<T: Clone + Send + Sync + 'static>(
        &mut self,
        function: &'static str,
        args: &[Value],
        color: Color,
        revision: &[u64],
    ) -> Option<T> {
        let key = MemoKey {
            function,
            args: args.to_vec(),
            color,
        };
        let entry = self.locals.memo.get(&key)?;
        if entry.revision == revision {
            return entry.result.downcast_ref::<T>().cloned();
        }
        self.locals.memo.remove(&key);
        None
    }

    /// Keeps a function's result alive while it is memoized.
    pub fn store_memo_result<T: Send + Sync + 'static>(
        &mut self,
        function: &'static str,
        args: &[Value],
        color: Color,
        revision: Vec<u64>,
        result: T,
    ) {
        let key = MemoKey {
            function,
            args: args.to_vec(),
            color,
        };
        self.locals.memo.insert(
            key,
            MemoEntry {
                revision,
                result: Box::new(result),
            },
        );
    }
}

fn vacant_schema() -> TableSchema {
    TableSchema {
        name: String::new(),
        kind: TableKind::Relation,
        columns: Vec::new(),
        keys: 0,
        merge: None,
        shapes: Vec::new(),
        ids: Vec::new(),
        native: Vec::new(),
        symmetries: Vec::new(),
        cost: None,
        unextractable: true,
        default: None,
    }
}
