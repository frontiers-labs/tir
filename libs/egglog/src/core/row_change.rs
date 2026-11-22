use super::{Database, TableId, Value};

impl Database {
    /// Start recording subsequent base-table changes. A database clone starts
    /// with no watches.
    pub fn watch_rows(&mut self, table: TableId) {
        self.tables[table.index()].changes.get_or_insert_default();
    }

    /// Drain rows inserted, removed, or subsumed in this table, in write order.
    /// A replacement records the removed row before its replacement.
    pub fn take_row_changes(&mut self, table: TableId) -> Vec<Box<[Value]>> {
        self.tables[table.index()]
            .changes
            .as_mut()
            .map(std::mem::take)
            .unwrap_or_default()
    }
}
