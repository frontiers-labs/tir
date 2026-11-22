use crate::{Context, OpId, PassError};
use std::{
    cell::{RefCell, RefMut},
    collections::{BTreeMap, BTreeSet},
};
use tir_egglog::{BaseValue, Database, EGraph, Error, Producer, Program, Row, TableId, Value};

const ANALYSIS_INPUTS: &str = concat!(
    include_str!("../../defs/ir.cdsl"),
    "\n",
    include_str!("../../defs/analysis/root.cdsl"),
);

#[derive(Default)]
pub struct AnalysisManager(RefCell<Option<Catalog>>);

/// The owner of one or more analysis tables in a context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct AnalysisId(usize);

/// The producers a pass allows to stay valid, subject to version checks.
pub enum Preserved {
    All,
    Only(Box<[AnalysisId]>),
}

/// A completed analysis table. Its rows stay valid for the lifetime of the view.
pub struct Facts<'a> {
    engine: RefMut<'a, EGraph>,
    table: TableId,
    pub producer: AnalysisId,
}

impl Facts<'_> {
    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> {
        self.engine.database().rows(self.table)
    }

    pub fn base_value<T: BaseValue>(&self, value: Value) -> T {
        self.engine.database().base_value(value)
    }
}

struct Catalog {
    root: TableId,
    existing_tables: usize,
    program: Program,
    owners: BTreeMap<TableId, AnalysisId>,
    producers: Vec<State>,
}

struct State {
    producer: Producer,
    order: Vec<AnalysisId>,
    version: Option<u128>,
}

impl State {
    fn version(&self, db: &Database) -> u128 {
        let tables = self.producer.reads.union(&self.producer.writes);
        tables
            .map(|&table| u128::from(db.table_version(table)))
            .sum()
    }
}

impl AnalysisManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a producer catalog is already installed on this context.
    pub fn is_installed(&self) -> bool {
        self.0.borrow().is_some()
    }

    /// Install the checked catalog once, before the pipeline requests facts.
    pub fn install(context: &Context, source: &str) -> Result<(), Error> {
        let mut catalog = context.analyses().0.borrow_mut();
        if catalog.is_some() {
            return Err(Error::Unsupported("duplicate analysis catalog".into()));
        }
        let mut live = context.egraph();
        let mut snapshot = live.clone();
        let int = snapshot.database().sort("i64")?;
        let db = snapshot.database_mut();
        if db.table("AnalysisRoot").is_ok() {
            return Err(Error::Unsupported("reserved analysis root".into()));
        }
        let existing_tables = db.table_count();
        let root = db.add_relation("AnalysisRoot", &[int]);
        let (engine, program, producers) =
            tir_cdsl::Schema::compile_producers(snapshot, source, ANALYSIS_INPUTS)
                .map_err(|e| Error::Unsupported(e.to_string()))?;
        let installed = Self::build_catalog(root, existing_tables, program, producers)?;
        *catalog = Some(installed);
        *live = engine;
        Ok(())
    }

    pub(crate) fn ensure_default(context: &Context) -> Result<(), PassError> {
        if !context.analyses().is_installed() {
            Self::install_default(context)
                .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?;
        }
        Ok(())
    }

    fn install_default(context: &Context) -> Result<(), Error> {
        let mut catalog = context.analyses().0.borrow_mut();
        if catalog.is_some() {
            return Err(Error::Unsupported("duplicate analysis catalog".into()));
        }
        let mut live = context.egraph();
        let mut snapshot = live.clone();
        let db = snapshot.database_mut();
        if super::generated::table_root_analysis_root(db).is_ok() {
            return Err(Error::Unsupported("reserved analysis root".into()));
        }
        let existing_tables = db.table_count();
        let (program, producers) = super::generated::load_producers(db)
            .map_err(|error| Error::Unsupported(error.to_string()))?;
        let root = super::generated::table_root_analysis_root(db)?;
        let installed = Self::build_catalog(root, existing_tables, program, producers)?;
        *catalog = Some(installed);
        *live = snapshot;
        Ok(())
    }

    fn build_catalog(
        root: TableId,
        existing_tables: usize,
        program: Program,
        producers: Vec<Producer>,
    ) -> Result<Catalog, Error> {
        let mut owners = BTreeMap::new();
        for (id, producer) in producers.iter().enumerate() {
            for &table in &producer.writes {
                if table == root
                    || table.index() < existing_tables
                    || owners.insert(table, AnalysisId(id)).is_some()
                {
                    return Err(Error::Unsupported("invalid output owner".into()));
                }
            }
        }
        let producers = producers
            .into_iter()
            .map(|producer| State {
                producer,
                order: Vec::new(),
                version: None,
            })
            .collect();
        let mut installed = Catalog {
            program,
            owners,
            producers,
            root,
            existing_tables,
        };
        for id in 0..installed.producers.len() {
            let mut order = Vec::new();
            installed.order(AnalysisId(id), &mut BTreeSet::new(), &mut order)?;
            installed.producers[id].order = order;
        }
        Ok(installed)
    }

    /// Refresh the requested table and its prerequisites on the published IR.
    pub fn require<'a>(context: &'a Context, root: OpId, table: &str) -> Result<Facts<'a>, Error> {
        Self::require_table_with(
            context,
            root,
            |db| db.table(table),
            || Error::UnknownName(table.into()),
        )
    }

    pub(crate) fn require_table(
        context: &Context,
        root: OpId,
        resolve: impl FnOnce(&Database) -> Result<TableId, Error>,
    ) -> Result<Facts<'_>, Error> {
        Self::require_table_with(context, root, resolve, || {
            Error::Unsupported("no producer for requested analysis".into())
        })
    }

    fn require_table_with<'a>(
        context: &'a Context,
        root: OpId,
        resolve: impl FnOnce(&Database) -> Result<TableId, Error>,
        missing: impl Fn() -> Error,
    ) -> Result<Facts<'a>, Error> {
        let mut state = context.analyses().0.borrow_mut();
        let catalog = state.as_mut().ok_or_else(&missing)?;
        let mut engine = context.egraph();
        let db = engine.database_mut();
        let table = resolve(db)?;
        let producer = *catalog.owners.get(&table).ok_or_else(missing)?;
        let root = db.base(i64::from(root.raw()));
        let mut roots = db.rows(catalog.root);
        let visible = roots
            .next()
            .is_some_and(|row| row[0] == root && !row.is_subsumed());
        if !visible || roots.next().is_some() {
            db.clear(catalog.root);
            db.insert(catalog.root, &[root])?;
        }
        for id in catalog.producers[producer.0].order.clone() {
            catalog.refresh(db, id)?;
        }
        Ok(Facts {
            engine,
            table,
            producer,
        })
    }

    pub(crate) fn ir_tables(&self, db: &Database) -> usize {
        let catalog = self.0.borrow();
        catalog
            .as_ref()
            .map_or(db.table_count(), |c| c.existing_tables)
    }

    pub(crate) fn finish(&self, context: &Context, preserved: Option<Preserved>) {
        if let Some(catalog) = self.0.borrow_mut().as_mut() {
            let engine = context.egraph();
            for (id, state) in catalog.producers.iter_mut().enumerate() {
                let keep = matches!(&preserved, Some(Preserved::All))
                    || matches!(&preserved, Some(Preserved::Only(ids)) if ids.contains(&AnalysisId(id)));
                if !keep || state.version != Some(state.version(engine.database())) {
                    state.version = None;
                }
            }
        }
    }
}

impl Catalog {
    fn order(
        &self,
        id: AnalysisId,
        active: &mut BTreeSet<AnalysisId>,
        order: &mut Vec<AnalysisId>,
    ) -> Result<(), Error> {
        if order.contains(&id) {
            return Ok(());
        }
        if !active.insert(id) {
            return Err(Error::Unsupported("cyclic analysis producers".into()));
        }
        for table in &self.producers[id.0].producer.reads {
            if let Some(&dependency) = self.owners.get(table)
                && dependency != id
            {
                self.order(dependency, active, order)?;
            }
        }
        active.remove(&id);
        order.push(id);
        Ok(())
    }

    fn refresh(&mut self, db: &mut Database, id: AnalysisId) -> Result<(), Error> {
        let state = &mut self.producers[id.0];
        if state.version == Some(state.version(db)) {
            return Ok(());
        }
        state.version = None;
        let writes = &state.producer.writes;
        let clear = |db: &mut Database| writes.iter().for_each(|&table| db.clear(table));
        clear(db);
        self.program
            .run(db, &state.producer.schedule, &[])
            .inspect_err(|_| clear(db))?;
        state.version = Some(state.version(db));
        Ok(())
    }
}
