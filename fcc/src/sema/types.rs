//! C type identities, interning, and annotations attached to analyzed syntax.

use super::{TargetProfile, is_signed_integer};
use crate::ast::{Ast, RecordId, RecordKind};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Qualifiers(u8);

impl Qualifiers {
    pub(super) const CONST: u8 = 1;
    pub(super) const VOLATILE: u8 = 2;
    pub(super) const RESTRICT: u8 = 4;

    pub fn is_const(self) -> bool {
        self.0 & Self::CONST != 0
    }

    pub fn is_restrict(self) -> bool {
        self.0 & Self::RESTRICT != 0
    }

    pub(super) fn with(self, flag: u8) -> Self {
        Self(self.0 | flag)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QualType {
    pub id: TypeId,
    pub qualifiers: Qualifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntityId(pub(super) u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntegerKind {
    Bool,
    Char,
    SignedChar,
    UnsignedChar,
    Short,
    UnsignedShort,
    Int,
    UnsignedInt,
    Long,
    UnsignedLong,
    LongLong,
    UnsignedLongLong,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TypeKind {
    Error,
    Void,
    Integer(IntegerKind),
    Float,
    Double,
    LongDouble,
    Pointer(QualType),
    Array(QualType, Option<u64>),
    Function {
        ret: QualType,
        params: Vec<QualType>,
        varargs: bool,
        prototype: bool,
    },
    Record(RecordId),
    Enum(Option<String>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ValueCategory {
    #[default]
    Value,
    Lvalue,
    Function,
}

#[derive(Clone, Debug, Default)]
pub struct NodeSemantics {
    pub ty: Option<QualType>,
    pub entity: Option<EntityId>,
    pub category: ValueCategory,
    pub conversions: Vec<QualType>,
    pub constant: Option<i64>,
    pub member_index: Option<usize>,
    pub initializer_path: Option<Vec<usize>>,
    pub call_designator_ty: Option<QualType>,
}

#[derive(Default)]
pub struct TypeInterner {
    kinds: Vec<TypeKind>,
    ids: HashMap<TypeKind, TypeId>,
}

impl TypeInterner {
    pub(super) fn intern(&mut self, kind: TypeKind) -> QualType {
        let id = if let Some(&id) = self.ids.get(&kind) {
            id
        } else {
            let id = TypeId(self.kinds.len() as u32);
            self.kinds.push(kind.clone());
            self.ids.insert(kind, id);
            id
        };
        QualType {
            id,
            qualifiers: Qualifiers::default(),
        }
    }

    pub fn kind(&self, ty: QualType) -> &TypeKind {
        &self.kinds[ty.id.0 as usize]
    }
}

pub struct TypedAst {
    pub(super) ast: Ast,
    pub(super) types: TypeInterner,
    pub(super) target: TargetProfile,
    pub(super) records: Vec<RecordDefinition>,
}

#[derive(Clone, Debug)]
pub struct RecordField {
    pub name: String,
    pub ty: QualType,
    pub offset: u64,
}

#[derive(Clone, Debug)]
pub struct RecordDefinition {
    pub id: RecordId,
    pub kind: RecordKind,
    pub name: String,
    pub fields: Vec<RecordField>,
    pub size: u64,
    pub align: u64,
}

impl TypedAst {
    pub fn ast(&self) -> &Ast {
        &self.ast
    }

    pub fn types(&self) -> &TypeInterner {
        &self.types
    }

    pub fn target(&self) -> TargetProfile {
        self.target
    }

    pub fn records(&self) -> impl Iterator<Item = &RecordDefinition> {
        self.records.iter()
    }

    pub fn record(&self, id: RecordId) -> Option<&RecordDefinition> {
        self.records.iter().find(|record| record.id == id)
    }

    pub fn integer_width(&self, ty: QualType) -> Option<u32> {
        match self.types.kind(ty) {
            TypeKind::Integer(kind) => Some(self.target.integer_width(*kind)),
            _ => None,
        }
    }

    pub fn integer_is_signed(&self, ty: QualType) -> Option<bool> {
        match self.types.kind(ty) {
            TypeKind::Integer(kind) => Some(is_signed_integer(*kind, self.target)),
            _ => None,
        }
    }
}
