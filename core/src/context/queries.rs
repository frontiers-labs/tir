//! Invalidate derived rows when one of their inputs changes, including raw engine edits.
use std::cell::Cell;
use tir_egglog::{Database, EGraph, Error, RunReport, TableId};

use super::ir_program;

type ResolveTable = fn(&Database) -> Result<TableId, Error>;

struct Inputs<const N: usize> {
    tables: [TableId; N],
    last: Cell<Option<[u64; N]>>,
}

impl<const N: usize> Inputs<N> {
    fn new(db: &Database, tables: [ResolveTable; N]) -> Self {
        Self {
            tables: tables.map(|table| table(db).expect("core IR relation")),
            last: Cell::new(None),
        }
    }

    fn versions(&self, db: &Database) -> [u64; N] {
        self.tables.map(|table| db.table_version(table))
    }

    fn refresh(&self, engine: &mut EGraph, run: fn(&mut EGraph) -> Result<RunReport, Error>) {
        if self.last.get() == Some(self.versions(engine.database())) {
            return;
        }
        for table in &self.tables[N - 2..] {
            engine.database_mut().clear(*table);
        }
        let previous = engine.seminaive;
        engine.seminaive = false;
        let result = run(engine);
        engine.seminaive = previous;
        result.expect("core IR rules");
        self.last.set(Some(self.versions(engine.database())));
    }
}

pub(super) struct Queries {
    structure: Inputs<8>,
    flow: Inputs<9>,
}

impl Queries {
    pub fn new(db: &Database) -> Self {
        Self {
            structure: Inputs::new(
                db,
                [
                    ir_program::table_operation,
                    ir_program::table_region,
                    ir_program::table_block,
                    ir_program::table_value,
                    ir_program::table_operand,
                    ir_program::table_yield,
                    ir_program::table_reader_at,
                    ir_program::table_depends_on,
                ],
            ),
            flow: Inputs::new(
                db,
                [
                    ir_program::table_depends_on,
                    ir_program::table_operation,
                    ir_program::table_region,
                    ir_program::table_block,
                    ir_program::table_value,
                    ir_program::table_operand,
                    ir_program::table_successor,
                    ir_program::table_cycle_path,
                    ir_program::table_bypass,
                ],
            ),
        }
    }

    pub fn structure(&self, engine: &mut EGraph) {
        self.structure
            .refresh(engine, ir_program::run_derive_structure);
    }

    pub fn verification(&self, engine: &mut EGraph) {
        self.structure(engine);
        self.flow.refresh(engine, ir_program::run_derive_flow);
    }

    pub fn invalidate(&self) {
        self.structure.last.set(None);
        self.flow.last.set(None);
    }
}
