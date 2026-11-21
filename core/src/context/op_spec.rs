//! The compiled contract and registered interfaces for each operation kind.
use super::Context;
use crate::schema::{OP_SCHEMAS, OpSchema};
use crate::{OpHandle, Operation, operation::OpInterfaceConverter};
use std::any::TypeId;
use std::hash::{Hash, Hasher};
use tir_egglog::{BaseValue, EGraph, NativeIdentity, Value};

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
struct Declaration {
    dialect: &'static str,
    name: &'static str,
    schema: Option<usize>,
    factory: Factory,
}

#[derive(Clone, Copy, Debug)]
struct Factory(fn(OpHandle) -> Box<dyn Operation>);

impl PartialEq for Factory {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::fn_addr_eq(self.0, other.0)
    }
}
impl Eq for Factory {}
impl Hash for Factory {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl BaseValue for Declaration {}

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
struct Interface(TypeId);

impl BaseValue for Interface {}

#[derive(Clone, Copy, Debug)]
struct Converter(OpInterfaceConverter);

impl PartialEq for Converter {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::fn_addr_eq(self.0, other.0)
    }
}

impl Eq for Converter {}

impl Hash for Converter {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl BaseValue for Converter {}

fn declare(engine: &mut EGraph, kind: NativeIdentity, declaration: Declaration) {
    let db = engine.database_mut();
    let table = db.table("OpSpec").expect("operation registry");
    let key = [db.base(kind), db.base(-1_i64)];
    if db.lookup(table, &key).is_some() {
        return;
    }
    let id = db.base(declaration).raw();
    db.insert(table, &[key[0], key[1], db.base(i64::from(id))])
        .expect("unique operation declaration");
}

pub(super) fn bind_native(engine: &mut EGraph) {
    for schema in OP_SCHEMAS {
        (schema.bind)(engine.database_mut()).expect("declare native operation");
    }
}

pub(super) fn install(engine: &mut EGraph) {
    for (index, schema) in OP_SCHEMAS.iter().enumerate() {
        declare(
            engine,
            (schema.declaration)(),
            Declaration {
                dialect: schema.dialect,
                name: schema.name,
                schema: Some(index),
                factory: Factory(schema.construct),
            },
        );
    }
}

impl Context {
    pub(crate) fn register_operation<T: Operation>(&self) {
        declare(
            &mut self.engine(),
            T::native_identity(),
            Declaration {
                dialect: T::dialect(),
                name: T::name(),
                schema: None,
                factory: Factory(T::from_op_instance_dyn),
            },
        );
    }

    pub(crate) fn resolve_native_operation(&self, name: (&str, &str)) -> Option<NativeIdentity> {
        if let Some(schema) = OP_SCHEMAS
            .iter()
            .find(|schema| (schema.dialect, schema.name) == name)
        {
            return Some((schema.declaration)());
        }
        let engine = self.engine();
        let db = engine.database();
        let table = db.table("OpSpec").ok()?;
        db.rows(table).find_map(|row| {
            if db.base_value::<i64>(row[1]) != -1 {
                return None;
            }
            let raw = u32::try_from(db.base_value::<i64>(row[2])).ok()?;
            let declaration = db.base_value::<Declaration>(Value::new(raw));
            ((declaration.dialect, declaration.name) == name).then(|| db.base_value(row[0]))
        })
    }

    pub(crate) fn import_declaration(&self, source: &Self, kind: NativeIdentity) {
        let declaration = source
            .declaration(kind)
            .expect("registered source operation");
        declare(&mut self.engine(), kind, declaration);
    }

    fn declaration(&self, kind: NativeIdentity) -> Option<Declaration> {
        let handle = self.lookup("OpSpec", (kind, -1_i64))?;
        let raw = self.integer(handle) as u32;
        Some(self.engine().value_to_base(Value::new(raw)))
    }

    pub(crate) fn schema_for_kind(&self, kind: NativeIdentity) -> Option<&'static OpSchema> {
        Some(&OP_SCHEMAS[self.declaration(kind)?.schema?])
    }

    pub(crate) fn operation_factory(
        &self,
        kind: NativeIdentity,
    ) -> Option<fn(OpHandle) -> Box<dyn Operation>> {
        Some(self.declaration(kind)?.factory.0)
    }

    pub(crate) fn op_identity(&self, op: crate::OpId) -> (&'static str, &'static str) {
        let kind = self.operation_kind(op).expect("live operation");
        let declaration = self.declaration(kind).expect("registered operation kind");
        (declaration.dialect, declaration.name)
    }

    pub fn register_op_interface<I: ?Sized + 'static>(
        &self,
        dialect: &'static str,
        op_name: &'static str,
        converter: OpInterfaceConverter,
    ) {
        let kind = self
            .resolve_native_operation((dialect, op_name))
            .expect("registered native operation");
        self.install_op_interface::<I>(kind, converter);
    }

    pub(super) fn install_op_interface<I: ?Sized + 'static>(
        &self,
        kind: NativeIdentity,
        converter: OpInterfaceConverter,
    ) {
        let iface = self
            .engine()
            .base_to_value(Interface(TypeId::of::<I>()))
            .raw();
        let value = self.engine().base_to_value(Converter(converter)).raw();
        let kind = self.engine().base_to_value(kind);
        self.put("OpSpec", vec![kind, self.int(iface)], self.int(value));
    }

    pub(crate) fn find_op_interface<I: ?Sized + 'static>(
        &self,
        kind: &NativeIdentity,
    ) -> Option<OpInterfaceConverter> {
        let iface = self
            .engine()
            .base_to_value(Interface(TypeId::of::<I>()))
            .raw();
        let value = self.lookup("OpSpec", (*kind, i64::from(iface)))?;
        let raw = self.integer(value) as u32;
        Some(self.engine().value_to_base::<Converter>(Value::new(raw)).0)
    }

    pub(crate) fn kinds_implementing<I: ?Sized + 'static>(&self) -> Vec<NativeIdentity> {
        let engine = self.engine();
        let db = engine.database();
        let iface = i64::from(db.base(Interface(TypeId::of::<I>())).raw());
        let table = db.table("OpSpec").expect("operation registry");
        db.rows(table)
            .filter(|row| db.base_value::<i64>(row[1]) == iface)
            .map(|row| db.base_value(row[0]))
            .collect()
    }

    pub(crate) fn kind_has_interface<I: ?Sized + 'static>(&self, kind: &NativeIdentity) -> bool {
        self.find_op_interface::<I>(kind).is_some()
    }
}
