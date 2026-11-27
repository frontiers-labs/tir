//! Secondary keys for relations of base values.

use super::{Color, Database, Error, Table, TableId, TableKind, Value};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct UniqueKey {
    columns: Vec<usize>,
    index: usize,
    absent: Option<(usize, u32)>,
}

impl UniqueKey {
    fn conflict(&self, table: &Table) -> Error {
        Error::DuplicateKey {
            table: table.schema.name.clone(),
            columns: self.columns.clone(),
        }
    }

    fn exempt(&self, row: &[u32]) -> bool {
        self.absent.is_some_and(|(col, absent)| row[col] == absent)
    }

    fn check_view(&self, db: &Database, id: TableId, color: Color) -> Result<(), Error> {
        let table = &db.tables[id.index()];
        // Adoption publishes all symmetric orders, including orders declared
        // after a color was made. Repeated copies of one fact are idempotent.
        let identity: Vec<_> = (0..table.columns.len()).collect();
        let mut orders = Vec::new();
        for row in db.rows_in(color, id) {
            for order in std::iter::once(&identity).chain(&table.schema.symmetries) {
                if self
                    .absent
                    .is_some_and(|(col, absent)| row[order[col]].raw() == absent)
                {
                    continue;
                }
                let key: Vec<_> = self.columns.iter().map(|&col| row[order[col]]).collect();
                orders.push((key, row, order));
            }
        }
        orders.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        for [(a_key, a, left), (b_key, b, right)] in orders.array_windows() {
            if a_key == b_key && left.iter().zip(right.iter()).any(|(&i, &j)| a[i] != b[j]) {
                return Err(self.conflict(table));
            }
        }
        Ok(())
    }

    fn check(
        &self,
        table: &Table,
        cell: impl Fn(usize) -> u32,
        consider: impl Fn(u32) -> bool,
    ) -> Result<(), Error> {
        if self
            .absent
            .is_some_and(|(column, absent)| cell(column) == absent)
        {
            return Ok(());
        }
        let rows = table.indexes[self.index]
            .group_rows(cell(self.columns[0]))
            .expect("kept index on the first key column");
        let matches = |&row: &u32| {
            consider(row)
                && self.columns[1..]
                    .iter()
                    .all(|&col| table.cell(row, col) == cell(col))
        };
        if rows.iter().any(matches) {
            return Err(self.conflict(table));
        }
        Ok(())
    }
}

impl Table {
    pub(super) fn check_unique_keys(&self, row: &[u32]) -> Result<(), Error> {
        for key in &self.unique_keys {
            key.check(self, |col| row[col], |_| true)?;
        }
        Ok(())
    }
}

impl Database {
    pub(super) fn check_publication(&self, id: TableId, color: Color) -> Result<(), Error> {
        for key in &self.tables[id.index()].unique_keys {
            key.check_view(self, id, color)?;
        }
        Ok(())
    }

    pub(super) fn check_orders(
        &self,
        id: TableId,
        color: Color,
        orders: &[Vec<u32>],
    ) -> Result<(), Error> {
        let table = &self.tables[id.index()];
        let offset = usize::from(color != Color::BASE);
        for key in &table.unique_keys {
            for (i, order) in orders.iter().enumerate() {
                let row = &order[offset..];
                if key.exempt(row) {
                    continue;
                }
                let same = |col: usize, value: u32| row[col] == value;
                key.check(
                    table,
                    |col| row[col],
                    |at| {
                        (color == Color::BASE
                            || self.colors.visible(color.index() as u32, id, table, at))
                            && (0..row.len()).any(|col| !same(col, table.cell(at, col)))
                    },
                )?;
                let own = self.own_rows_in(color, id).any(|held| {
                    key.columns.iter().all(|&col| same(col, held[col].raw()))
                        && !held.values().map(Value::raw).eq(row.iter().copied())
                });
                let other_order = orders[..i].iter().any(|held| {
                    held != order && key.columns.iter().all(|&col| same(col, held[col + offset]))
                });
                if own || other_order {
                    return Err(key.conflict(table));
                }
            }
        }
        Ok(())
    }

    /// Require that the selected columns identify a single row of this base
    /// relation. The first column selects a kept index. Rows whose key has
    /// the specified absent value in its designated column are exempt.
    pub fn add_unique_key(
        &mut self,
        table: TableId,
        columns: &[usize],
        absent: Option<(usize, Value)>,
    ) -> Result<(), Error> {
        let id = table;
        let table = &mut self.tables[id.index()];
        if table.schema.kind != TableKind::Relation || !table.schema.ids.is_empty() {
            return Err(Error::Unsupported(
                "secondary keys require a relation of base values".into(),
            ));
        }
        if columns.is_empty()
            || columns.iter().any(|&col| col >= table.columns.len())
            || absent.is_some_and(|(col, _)| !columns.contains(&col))
        {
            return Err(Error::Unsupported(format!(
                "invalid secondary key for `{}`",
                table.schema.name
            )));
        }
        let key = UniqueKey {
            index: table.add_kept_index(&columns[..1], 1),
            columns: columns.to_vec(),
            absent: absent.map(|(col, value)| (col, value.raw())),
        };
        if table.unique_keys.contains(&key) {
            return Ok(());
        }
        for row in (0..table.len() as u32).filter(|&row| table.is_live(row)) {
            key.check(table, |col| table.cell(row, col), |other| other != row)?;
        }
        for color in self.colors.levels().into_iter().flatten() {
            key.check_view(self, id, color)?;
        }
        self.tables[id.index()].unique_keys.push(key);
        Ok(())
    }
}
