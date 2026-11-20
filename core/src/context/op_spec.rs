//! The compiled contract and registered interfaces for each operation kind.
use super::Context;
use crate::operation::OpInterfaceConverter;
use crate::schema::{OP_SCHEMAS, OpSchema};
use std::any::{TypeId, type_name};
use std::hash::{Hash, Hasher};
use tir_egglog::{BaseValue, EGraph, Value};

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
struct Declaration {
    dialect: &'static str,
    name: &'static str,
    schema: Option<usize>,
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

fn declare(engine: &mut EGraph, declaration: Declaration) {
    let db = engine.database_mut();
    let table = db.table("OpSpec").expect("operation registry");
    let key = [
        db.base(format!("{}.{}", declaration.dialect, declaration.name)),
        db.base(String::new()),
        db.base(-1_i64),
    ];
    if db.lookup(table, &key).is_some() {
        return;
    }
    let id = db.base(declaration).raw();
    db.insert(table, &[key[0], key[1], key[2], db.base(i64::from(id))])
        .expect("unique operation declaration");
}

pub(super) fn install(engine: &mut EGraph) {
    for (index, schema) in OP_SCHEMAS.iter().enumerate() {
        declare(
            engine,
            Declaration {
                dialect: schema.dialect,
                name: schema.name,
                schema: Some(index),
            },
        );
    }
}

impl Context {
    pub(crate) fn register_op_name(&self, dialect: &'static str, name: &'static str) {
        declare(
            &mut self.engine(),
            Declaration {
                dialect,
                name,
                schema: None,
            },
        )
    }

    fn declaration(&self, qualified: &str) -> Option<Declaration> {
        let handle = self.lookup("OpSpec", (qualified, "", -1_i64))?;
        let raw = self.integer(handle) as u32;
        Some(self.engine().value_to_base(Value::new(raw)))
    }

    pub(crate) fn schema_for_kind(&self, qualified: &str) -> Option<&'static OpSchema> {
        Some(&OP_SCHEMAS[self.declaration(qualified)?.schema?])
    }

    pub(crate) fn op_identity(&self, op: crate::OpId) -> (&'static str, &'static str) {
        let kind = self.operation_kind(op).expect("live operation");
        let declaration = self.declaration(&kind).expect("registered operation kind");
        (declaration.dialect, declaration.name)
    }

    pub fn register_op_interface<I: ?Sized + 'static>(
        &self,
        dialect: &'static str,
        op_name: &'static str,
        converter: OpInterfaceConverter,
    ) {
        self.register_op_name(dialect, op_name);
        let iface = self
            .engine()
            .base_to_value(Interface(TypeId::of::<I>()))
            .raw();
        let value = self.engine().base_to_value(Converter(converter)).raw();
        self.put(
            "OpSpec",
            vec![
                self.text(format!("{dialect}.{op_name}")),
                self.text(type_name::<I>()),
                self.int(iface),
            ],
            self.int(value),
        );
    }

    pub(crate) fn find_op_interface<I: ?Sized + 'static>(
        &self,
        kind: &str,
    ) -> Option<OpInterfaceConverter> {
        let iface = self
            .engine()
            .base_to_value(Interface(TypeId::of::<I>()))
            .raw();
        let value = self.lookup("OpSpec", (kind, type_name::<I>(), i64::from(iface)))?;
        let raw = self.integer(value) as u32;
        Some(self.engine().value_to_base::<Converter>(Value::new(raw)).0)
    }

    pub(crate) fn kind_has_interface<I: ?Sized + 'static>(&self, kind: &str) -> bool {
        self.find_op_interface::<I>(kind).is_some()
    }
}
