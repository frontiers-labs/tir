//! Invalidate derived rows when one of their inputs changes, including raw engine edits.
use std::cell::Cell;
use tir_egglog::{Database, EGraph, Schedule, TableId};

struct Inputs<const N: usize> {
    tables: [TableId; N],
    last: Cell<Option<[u64; N]>>,
}

impl<const N: usize> Inputs<N> {
    fn new(db: &Database, names: [&str; N]) -> Self {
        Self {
            tables: names.map(|name| db.table(name).expect("core IR relation")),
            last: Cell::new(None),
        }
    }

    fn versions(&self, db: &Database) -> [u64; N] {
        self.tables.map(|table| db.table_version(table))
    }

    fn refresh(&self, engine: &mut EGraph, rules: &str, outputs: &[&str]) {
        if self.last.get() == Some(self.versions(engine.database())) {
            return;
        }
        for name in outputs {
            engine.clear_function(name).expect("derived IR relation");
        }
        let previous = engine.seminaive;
        engine.seminaive = false;
        let result = engine.run_schedule(&Schedule::saturate(rules));
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
                    "Operation",
                    "Region",
                    "Block",
                    "Value",
                    "Operand",
                    "Yield",
                    "ReaderAt",
                    "DependsOn",
                ],
            ),
            flow: Inputs::new(
                db,
                [
                    "DependsOn",
                    "Operation",
                    "Region",
                    "Block",
                    "Value",
                    "Operand",
                    "Successor",
                    "CyclePath",
                    "Bypass",
                ],
            ),
        }
    }

    pub fn structure(&self, engine: &mut EGraph) {
        self.structure
            .refresh(engine, "structure", &["ReaderAt", "DependsOn"]);
    }

    pub fn verification(&self, engine: &mut EGraph) {
        self.structure(engine);
        self.flow.refresh(engine, "flow", &["CyclePath", "Bypass"]);
    }

    pub fn invalidate(&self) {
        self.structure.last.set(None);
        self.flow.last.set(None);
    }
}
