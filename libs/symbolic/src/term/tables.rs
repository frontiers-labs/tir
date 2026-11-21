//! The three metadata relations shared with the IR context.

use std::{
    hash::{Hash, Hasher},
    ptr,
};
use tir_egglog::{BaseValue, Database, IndexId, TableId, Value};

use super::{Param, TypeArg, TypeCtor};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Arguments(pub Arc<[TypeArg]>);

impl BaseValue for Arguments {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct DefaultArg(pub Option<TypeArg>);

impl BaseValue for DefaultArg {}

// The first declaration supplies the callbacks. Later declarations of the same
// name can supply additional source-domain parameter shapes.
#[derive(Clone, Debug)]
pub(super) struct Declaration {
    pub info: &'static TypeCtor,
    pub additional: Vec<&'static [(&'static str, Param)]>,
}

impl PartialEq for Declaration {
    fn eq(&self, other: &Self) -> bool {
        ptr::eq(self.info, other.info) && self.additional == other.additional
    }
}

impl Eq for Declaration {}

impl Hash for Declaration {
    fn hash<H: Hasher>(&self, state: &mut H) {
        ptr::hash(self.info, state);
        self.additional.hash(state);
    }
}

impl BaseValue for Declaration {}

pub(super) struct Tables {
    pub term: TableId,
    pub ctor: TableId,
    pub symbol: TableId,
    pub term_id: IndexId,
    pub application: IndexId,
    pub ctor_id: IndexId,
    pub ctor_name: IndexId,
    pub symbol_id: IndexId,
    pub spelling: IndexId,
    pub entity: IndexId,
}

impl Tables {
    pub fn new(db: &mut Database) -> Self {
        let term = super::ir_catalog::table_type_term(db).expect("type term table");
        let ctor = super::ir_catalog::table_type_ctor(db).expect("type constructor table");
        let symbol = super::ir_catalog::table_symbol(db).expect("symbol table");
        for (table, key) in [
            (term, &[0][..]),
            (term, &[2, 1]),
            (ctor, &[0]),
            (ctor, &[1, 2]),
            (symbol, &[0]),
            (symbol, &[1]),
        ] {
            db.add_unique_key(table, key, None).expect("metadata key");
        }
        db.add_unique_key(symbol, &[2], Some((2, db.base(-1i64))))
            .expect("type name key");
        Self {
            term,
            ctor,
            symbol,
            term_id: db.add_kept_index(term, &[0], &[]),
            application: db.add_kept_index(term, &[2, 1], &[]),
            ctor_id: db.add_kept_index(ctor, &[0], &[]),
            ctor_name: db.add_kept_index(ctor, &[1, 2], &[]),
            symbol_id: db.add_kept_index(symbol, &[0], &[]),
            spelling: db.add_kept_index(symbol, &[1], &[]),
            entity: db.add_kept_index(symbol, &[2], &[]),
        }
    }
}

pub(super) fn replace<const N: usize>(
    db: &mut Database,
    table: TableId,
    old: [Value; N],
    col: usize,
    value: Value,
) {
    if old[col] != value {
        let mut new = old;
        new[col] = value;
        db.remove(table, &old);
        if let Err(error) = db.insert(table, &new) {
            db.insert(table, &old).expect("previous metadata row");
            panic!("invalid metadata edit: {error}");
        }
    }
}

// The CDSL schema exposes typed base payloads as integer handles.
pub(super) fn store<T: BaseValue>(db: &Database, value: T) -> Value {
    db.base(i64::from(db.base(value).raw()))
}

pub(super) fn load<T: BaseValue>(db: &Database, handle: Value) -> T {
    let raw = u32::try_from(db.base_value::<i64>(handle)).expect("metadata payload handle");
    db.base_value(Value::new(raw))
}
