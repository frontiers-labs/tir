use std::collections::HashMap;

use tir::backend::abi::{ArgumentGroupAlignment, ClassifierKind, Overflow, ValueKind};
use tir::graph::{Dag, MutDag, NodeId};

use crate::ast::{
    ArrayLength, Ast, AstKind, AstLeaf, CParam, CType, InitializerDesignator, RecordId, RecordKind,
};
use crate::diagnostics::{
    ArgumentMismatch, CalledObjectNotFunction, CompleteObjectTypeRequired, ConflictingDeclaration,
    Diagnostic, DuplicateLabel, DuplicateSwitchLabel, IncompatibleConversion,
    IntegerConstantRequired, InvalidControllingExpression, InvalidIntegerLiteral, InvalidOperands,
    InvalidReturn, InvalidTypeQualifier, InvalidTypeSpecifiers, MisplacedBreak, MisplacedContinue,
    MisplacedSwitchLabel, ModifiableLvalueRequired, Redefinition, Span, UndeclaredIdentifier,
    UnknownLabel,
};
use crate::lang_options::{LangOptions, StdVersion};
use crate::lexer::decode_character_constant;

mod references;
use references::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeId(u32);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Qualifiers(u8);

impl Qualifiers {
    const CONST: u8 = 1;
    const VOLATILE: u8 = 2;
    const RESTRICT: u8 = 4;

    pub fn is_const(self) -> bool {
        self.0 & Self::CONST != 0
    }

    pub fn is_restrict(self) -> bool {
        self.0 & Self::RESTRICT != 0
    }

    fn with(self, flag: u8) -> Self {
        Self(self.0 | flag)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct QualType {
    pub id: TypeId,
    pub qualifiers: Qualifiers,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EntityId(u32);

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

/// The TIR layout classes fcc's scalar C types map onto. Which class a C type
/// takes is C's rule; how wide and how aligned that class is, is the target's
/// to declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LayoutClass {
    I8,
    I16,
    I32,
    I64,
    F32,
    F64,
    Pointer,
}

impl LayoutClass {
    const ALL: [Self; 7] = [
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::F32,
        Self::F64,
        Self::Pointer,
    ];

    fn key(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F32 => "f32",
            Self::F64 => "f64",
            Self::Pointer => "p",
        }
    }

    fn of_width(bits: u32) -> Self {
        match bits {
            8 => Self::I8,
            16 => Self::I16,
            32 => Self::I32,
            _ => Self::I64,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetProfile {
    /// Size and ABI alignment of each [`LayoutClass`], in bytes, as the
    /// target's data layout declares them.
    scalars: [(u64, u64); LayoutClass::ALL.len()],
    plain_char_signed: bool,
    abi_classifier: ClassifierKind,
    integer_argument_registers: usize,
    float_argument_registers: usize,
    float_argument_overflow: Overflow,
    argument_group_alignment: Option<ArgumentGroupAlignment>,
    indirect_result_argument_slots: Option<(ValueKind, usize)>,
}

impl TargetProfile {
    pub fn for_march(march: &str) -> Result<Self, String> {
        let target = tir::backend::select_target(march, None, None)?;
        Self::for_target(march, target.as_ref())
    }

    /// Derive the C profile from what the target itself declares: scalar sizes
    /// and alignments from its data layout, the rest from its ABI. fcc keeps no
    /// table of its own, so every spelling the backend accepts works here too.
    pub(crate) fn for_target(
        march: &str,
        machine: &dyn tir::backend::TargetMachine,
    ) -> Result<Self, String> {
        let layout = machine
            .data_layout()
            .as_ref()
            .and_then(tir::DataLayout::from_value)
            .ok_or_else(|| format!("target '{march}' declares no data layout"))?;
        let mut scalars = [(0, 0); LayoutClass::ALL.len()];
        for class in LayoutClass::ALL {
            let (size, align) = layout
                .class_layout(class.key())
                .ok_or_else(|| format!("target '{march}' declares no '{}' layout", class.key()))?;
            scalars[class as usize] = (u64::from(size / 8), u64::from(align / 8));
        }
        let abi = machine.abi();
        let float_arguments = abi
            .args
            .iter()
            .find(|sequence| sequence.kind == ValueKind::Float);
        Ok(Self {
            scalars,
            plain_char_signed: abi.classifier != ClassifierKind::Aapcs64,
            abi_classifier: abi.classifier,
            integer_argument_registers: abi
                .args
                .iter()
                .find(|sequence| sequence.kind == ValueKind::Int)
                .map_or(0, |sequence| sequence.regs.len()),
            float_argument_registers: float_arguments.map_or(0, |sequence| sequence.regs.len()),
            float_argument_overflow: float_arguments
                .map_or(Overflow::Chain(ValueKind::Int), |sequence| {
                    sequence.overflow
                }),
            argument_group_alignment: abi.argument_group_alignment,
            indirect_result_argument_slots: abi.indirect_result_argument_slots(),
        })
    }

    pub fn host() -> Result<Self, String> {
        Self::for_march(std::env::consts::ARCH)
    }

    pub fn pointer_width(self) -> u32 {
        self.class_layout(LayoutClass::Pointer).0 as u32 * 8
    }

    /// Size and ABI alignment of a layout class, in bytes.
    pub(crate) fn class_layout(self, class: LayoutClass) -> (u64, u64) {
        self.scalars[class as usize]
    }

    /// Size and ABI alignment of a scalar C type, in bytes. Aggregates have no
    /// layout class of their own and are laid out from their members instead.
    pub(crate) fn scalar_layout(self, kind: &TypeKind) -> Option<(u64, u64)> {
        let class = match kind {
            TypeKind::Integer(kind) => LayoutClass::of_width(self.integer_width(*kind)),
            TypeKind::Enum(_) => LayoutClass::I32,
            TypeKind::Float => LayoutClass::F32,
            TypeKind::Double => LayoutClass::F64,
            // `long double` maps onto no TIR layout class: fcc carries it as an
            // opaque object rather than an arithmetic type, so its ABI size and
            // alignment have nowhere in the data layout to come from.
            TypeKind::LongDouble => return Some((16, 16)),
            TypeKind::Pointer(_) => LayoutClass::Pointer,
            _ => return None,
        };
        Some(self.class_layout(class))
    }

    /// The C integer kind of a pointer difference, and of an object size. A
    /// 32-bit layout ranks them as `int`, every wider one as `long`.
    fn pointer_sized_integer(self) -> IntegerKind {
        if self.pointer_width() == 32 {
            IntegerKind::Int
        } else {
            IntegerKind::Long
        }
    }

    pub fn integer_width(self, kind: IntegerKind) -> u32 {
        match kind {
            IntegerKind::Bool
            | IntegerKind::Char
            | IntegerKind::SignedChar
            | IntegerKind::UnsignedChar => 8,
            IntegerKind::Short | IntegerKind::UnsignedShort => 16,
            IntegerKind::Int | IntegerKind::UnsignedInt => 32,
            IntegerKind::Long | IntegerKind::UnsignedLong => self.pointer_width(),
            IntegerKind::LongLong | IntegerKind::UnsignedLongLong => 64,
        }
    }

    pub(crate) fn uses_riscv_hard_float_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Riscv && self.float_argument_registers > 0
    }

    pub(crate) fn uses_riscv_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Riscv
    }

    pub(crate) fn uses_aapcs64_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Aapcs64
    }

    pub(crate) fn uses_sysv_abi(self) -> bool {
        self.abi_classifier == ClassifierKind::Sysv
    }

    pub(crate) fn argument_registers(self, kind: ValueKind) -> usize {
        match kind {
            ValueKind::Int => self.integer_argument_registers,
            ValueKind::Float => self.float_argument_registers,
            ValueKind::Vector => 0,
        }
    }

    pub(crate) fn float_argument_overflow(self) -> Overflow {
        self.float_argument_overflow
    }

    pub(crate) fn align_argument_slot(
        self,
        kind: ValueKind,
        source_alignment: u64,
        slot: usize,
    ) -> usize {
        self.argument_group_alignment.map_or(slot, |alignment| {
            alignment.align_slot(kind, source_alignment, slot)
        })
    }

    pub(crate) fn indirect_result_argument_slots(self) -> Option<(ValueKind, usize)> {
        self.indirect_result_argument_slots
    }
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
    fn intern(&mut self, kind: TypeKind) -> QualType {
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
    ast: Ast,
    types: TypeInterner,
    target: TargetProfile,
    records: Vec<RecordDefinition>,
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

#[derive(Clone)]
struct Symbol {
    span: Span,
    ty: QualType,
    entity: EntityId,
    typedef: bool,
    defined: bool,
    constant: Option<i64>,
}

pub fn analyze(ast: Ast, options: LangOptions) -> Result<TypedAst, Vec<Diagnostic>> {
    // Where the host has no backend, fall back to a modeled target rather than
    // a hand-written profile, so the layout still comes from a description.
    let target = TargetProfile::host()
        .or_else(|_| TargetProfile::for_march("x86_64"))
        .expect("x86_64 is always modeled");
    analyze_with_target(ast, options, target)
}

pub fn analyze_with_target(
    mut ast: Ast,
    options: LangOptions,
    target: TargetProfile,
) -> Result<TypedAst, Vec<Diagnostic>> {
    let (types, records, diagnostics) = {
        let mut analyzer = Analyzer {
            ast: &mut ast,
            options,
            types: TypeInterner::default(),
            scopes: Vec::new(),
            diagnostics: Vec::new(),
            current_return: None,
            loop_depth: 0,
            switch_depth: 0,
            labels: HashMap::new(),
            switches: Vec::new(),
            target,
            next_entity: 0,
            records: Vec::new(),
            record_indices: HashMap::new(),
        };
        analyzer.translation_unit();
        (analyzer.types, analyzer.records, analyzer.diagnostics)
    };
    if diagnostics.is_empty() {
        Ok(TypedAst {
            ast,
            types,
            target,
            records,
        })
    } else {
        Err(diagnostics)
    }
}

struct Analyzer<'a> {
    ast: &'a mut Ast,
    options: LangOptions,
    types: TypeInterner,
    scopes: Vec<HashMap<String, Symbol>>,
    diagnostics: Vec<Diagnostic>,
    current_return: Option<QualType>,
    loop_depth: usize,
    switch_depth: usize,
    labels: HashMap<String, Span>,
    switches: Vec<SwitchContext>,
    target: TargetProfile,
    next_entity: u32,
    records: Vec<RecordDefinition>,
    record_indices: HashMap<RecordId, usize>,
}

#[derive(Default)]
struct SwitchContext {
    cases: HashMap<i64, Span>,
    default: Option<Span>,
}

impl Analyzer<'_> {
    fn new_entity(&mut self) -> EntityId {
        let entity = EntityId(self.next_entity);
        self.next_entity += 1;
        entity
    }

    fn translation_unit(&mut self) {
        let Some(root) = self.ast.root() else {
            return;
        };
        self.scopes.push(HashMap::new());
        let items = self.ast.children(root).collect::<Vec<_>>();
        for item in items {
            match self.ast.get_node(item).kind {
                AstKind::RecordDecl => self.record_declaration(item),
                AstKind::EnumDecl => self.enum_declaration(item),
                AstKind::Function => {
                    self.declare_file_item(item);
                    self.function(item);
                }
                AstKind::Global => {
                    self.declare_file_item(item);
                    self.global_initializer(item);
                }
                AstKind::Prototype | AstKind::Typedef => {
                    self.declare_file_item(item);
                }
                AstKind::DeclGroup => {
                    let declarations = self.ast.children(item).collect::<Vec<_>>();
                    for declaration in declarations {
                        if self.ast.get_node(declaration).kind == AstKind::RecordDecl {
                            self.record_declaration(declaration);
                        } else if self.ast.get_node(declaration).kind == AstKind::EnumDecl {
                            self.enum_declaration(declaration);
                        } else {
                            self.declare_file_item(declaration);
                            if self.ast.get_node(declaration).kind == AstKind::Global {
                                self.global_initializer(declaration);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.scopes.pop();
    }

    fn record_declaration(&mut self, node: NodeId) {
        let Some(AstLeaf::Record { id, kind, name }) = self.ast.get_leaf_data(node).cloned() else {
            return;
        };
        let name = name.unwrap_or_else(|| format!("__fcc_anon_struct.{}", id.number()));
        let index = if let Some(&index) = self.record_indices.get(&id) {
            index
        } else {
            let index = self.records.len();
            self.record_indices.insert(id, index);
            self.records.push(RecordDefinition {
                id,
                kind,
                name,
                fields: Vec::new(),
                size: 0,
                align: 1,
            });
            index
        };

        let children = self.ast.children(node).collect::<Vec<_>>();
        if children.is_empty() {
            return;
        }
        let mut fields = Vec::with_capacity(children.len());
        let mut field_spans = HashMap::new();
        let mut offset = 0;
        let mut record_align = 1;
        for field in children {
            if self.ast.get_node(field).kind == AstKind::RecordDecl {
                self.record_declaration(field);
                continue;
            }
            let Some(AstLeaf::Field { name, ty }) = self.ast.get_leaf_data(field).cloned() else {
                continue;
            };
            let span = self.ast.get_node(field).span;
            if let Some(previous) = field_spans.insert(name.clone(), span) {
                self.diagnostics.push(
                    Redefinition::new(span, previous, name, redefinition_reference(self.options))
                        .into(),
                );
                continue;
            }
            let ty = self.canonical_type(&ty);
            let (size, align) = self.type_layout(ty).unwrap_or((0, 1));
            let field_offset = match kind {
                RecordKind::Struct => align_to(offset, align),
                RecordKind::Union => 0,
            };
            fields.push(RecordField {
                name,
                ty,
                offset: field_offset,
            });
            offset = match kind {
                RecordKind::Struct => field_offset + size,
                RecordKind::Union => offset.max(size),
            };
            record_align = record_align.max(align);
        }
        self.records[index].size = align_to(offset, record_align);
        self.records[index].align = record_align;
        self.records[index].fields = fields;
    }

    fn declare_file_item(&mut self, node: NodeId) {
        let leaf = self.ast.get_leaf_data(node).cloned();
        let (name, ty, typedef) = match leaf {
            Some(AstLeaf::Function {
                name,
                ret,
                has_parameter_type_list,
                ..
            }) => (
                name,
                self.function_type(node, ret, has_parameter_type_list),
                false,
            ),
            Some(AstLeaf::Global { name, ty, .. }) => (name, self.canonical_type(&ty), false),
            Some(AstLeaf::Typedef { name, ty }) => (name, self.canonical_type(&ty), true),
            _ => return,
        };
        let span = self.ast.get_node(node).span;
        if self.ast.get_node(node).kind == AstKind::Global
            && matches!(self.types.kind(ty), TypeKind::Void)
        {
            self.diagnostics.push(
                CompleteObjectTypeRequired::new(
                    span,
                    format!("object '{name}' cannot have void type"),
                    object_type_reference(self.options),
                )
                .into(),
            );
        }
        let defined = self.ast.get_node(node).kind == AstKind::Function
            || (self.ast.get_node(node).kind == AstKind::Global
                && self.ast.children(node).next().is_some());
        let previous = self.scopes[0].get(&name).cloned();
        let entity = previous
            .as_ref()
            .map(|symbol| symbol.entity)
            .unwrap_or_else(|| self.new_entity());
        self.ast.set_annotation(
            node,
            NodeSemantics {
                ty: Some(ty),
                entity: Some(entity),
                ..NodeSemantics::default()
            },
        );
        if let Some(previous) = previous {
            if !self.declaration_types_compatible(previous.ty, ty) || previous.typedef != typedef {
                self.diagnostics.push(
                    ConflictingDeclaration::new(
                        span,
                        previous.span,
                        name,
                        conflicting_declaration_reference(self.options),
                    )
                    .into(),
                );
            } else if defined && previous.defined {
                self.diagnostics.push(
                    Redefinition::new(
                        span,
                        previous.span,
                        name,
                        redefinition_reference(self.options),
                    )
                    .into(),
                );
            } else if defined {
                self.scopes[0].get_mut(&name).unwrap().defined = true;
            }
        } else {
            self.scopes[0].insert(
                name,
                Symbol {
                    span,
                    ty,
                    entity,
                    typedef,
                    defined,
                    constant: None,
                },
            );
        }
    }

    fn declaration_types_compatible(&self, left: QualType, right: QualType) -> bool {
        if left == right {
            return true;
        }
        match (self.types.kind(left), self.types.kind(right)) {
            (
                TypeKind::Function {
                    ret: left_ret,
                    params: left_params,
                    varargs: left_varargs,
                    prototype: left_prototype,
                },
                TypeKind::Function {
                    ret: right_ret,
                    params: right_params,
                    varargs: right_varargs,
                    prototype: right_prototype,
                },
            ) => {
                left_ret == right_ret
                    && left_params.is_empty()
                    && right_params.is_empty()
                    && !left_varargs
                    && !right_varargs
                    && left_prototype != right_prototype
            }
            _ => false,
        }
    }

    fn enum_declaration(&mut self, node: NodeId) {
        let int = self.types.intern(TypeKind::Integer(IntegerKind::Int));
        let mut previous = -1_i64;
        for enumerator in self.ast.children(node).collect::<Vec<_>>() {
            let Some(AstLeaf::Enumerator { name }) = self.ast.get_leaf_data(enumerator).cloned()
            else {
                continue;
            };
            let explicit = self.ast.children(enumerator).next();
            if let Some(expression) = explicit {
                self.node(expression);
            }
            let value = explicit
                .and_then(|expression| self.ast.get_annotation(expression))
                .and_then(|info| info.constant)
                .or_else(|| {
                    explicit
                        .is_none()
                        .then(|| previous.checked_add(1))
                        .flatten()
                });
            let Some(value) = value else {
                self.diagnostics.push(
                    IntegerConstantRequired::new(
                        self.ast.get_node(enumerator).span,
                        "enumerator value is not an integer constant expression",
                        initializer_reference(self.options),
                    )
                    .into(),
                );
                continue;
            };
            previous = value;
            let span = self.ast.get_node(enumerator).span;
            let entity = self.new_entity();
            self.ast.set_annotation(
                enumerator,
                NodeSemantics {
                    ty: Some(int),
                    entity: Some(entity),
                    category: ValueCategory::Value,
                    constant: Some(value),
                    ..NodeSemantics::default()
                },
            );
            let scope = self.scopes.last_mut().unwrap();
            if let Some(existing) = scope.get(&name) {
                self.diagnostics.push(
                    Redefinition::new(
                        span,
                        existing.span,
                        name,
                        redefinition_reference(self.options),
                    )
                    .into(),
                );
            } else {
                scope.insert(
                    name,
                    Symbol {
                        span,
                        ty: int,
                        entity,
                        typedef: false,
                        defined: true,
                        constant: Some(value),
                    },
                );
            }
        }
    }

    fn function_type(
        &mut self,
        node: NodeId,
        ret: CType,
        has_parameter_type_list: bool,
    ) -> QualType {
        let ret = self.canonical_type(&ret);
        let children = self.ast.children(node).collect::<Vec<_>>();
        let mut params = Vec::new();
        let mut varargs = false;
        for child in children {
            match self.ast.get_leaf_data(child).cloned() {
                Some(AstLeaf::Param { ty, .. }) => {
                    let ty = self.canonical_parameter_type(&ty);
                    self.ast.set_annotation(
                        child,
                        NodeSemantics {
                            ty: Some(ty),
                            category: ValueCategory::Lvalue,
                            ..NodeSemantics::default()
                        },
                    );
                    params.push(ty);
                }
                _ if self.ast.get_node(child).kind == AstKind::VarArgs => varargs = true,
                _ => break,
            }
        }
        self.types.intern(TypeKind::Function {
            ret,
            params,
            varargs,
            prototype: has_parameter_type_list || self.options.std_version == StdVersion::C23,
        })
    }

    fn global_initializer(&mut self, node: NodeId) {
        let Some(initializer) = self.ast.children(node).next() else {
            return;
        };
        self.node(initializer);
        let mut target = self
            .ast
            .get_annotation(node)
            .and_then(|info| info.ty)
            .unwrap();
        if self.ast.get_node(initializer).kind == AstKind::InitializerList
            && let TypeKind::Array(element, None) = self.types.kind(target)
            && let Some(length) = self.inferred_array_length(initializer)
        {
            target = self.types.intern(TypeKind::Array(*element, Some(length)));
            let mut semantics = self.ast.get_annotation(node).cloned().unwrap();
            semantics.ty = Some(target);
            self.ast.set_annotation(node, semantics);
            let Some(AstLeaf::Global { name, .. }) = self.ast.get_leaf_data(node) else {
                unreachable!();
            };
            self.scopes[0].get_mut(name).unwrap().ty = target;
        }
        self.validate_initializer(target, initializer);
    }

    fn function(&mut self, function: NodeId) {
        let previous_return = self.current_return;
        if let Some(AstLeaf::Function { ret, .. }) = self.ast.get_leaf_data(function).cloned() {
            self.current_return = Some(self.canonical_type(&ret));
        }
        self.scopes.push(HashMap::new());
        let children = self.ast.children(function).collect::<Vec<_>>();
        self.labels.clear();
        for &child in &children {
            self.collect_labels(child);
        }
        for child in children {
            match self.ast.get_node(child).kind {
                AstKind::Param | AstKind::Decl | AstKind::Typedef => self.declaration(child),
                AstKind::EnumDecl => self.enum_declaration(child),
                _ => self.node(child),
            }
        }
        self.scopes.pop();
        self.current_return = previous_return;
    }

    fn collect_labels(&mut self, node: NodeId) {
        if self.ast.get_node(node).kind == AstKind::Label
            && let Some(AstLeaf::Label(name)) = self.ast.get_leaf_data(node).cloned()
        {
            let span = self.ast.get_node(node).span;
            if let Some(&previous) = self.labels.get(&name) {
                self.diagnostics.push(
                    DuplicateLabel::new(span, previous, name, label_reference(self.options)).into(),
                );
            } else {
                self.labels.insert(name, span);
            }
        }
        let children = self.ast.children(node).collect::<Vec<_>>();
        for child in children {
            self.collect_labels(child);
        }
    }

    fn node(&mut self, node: NodeId) {
        let kind = self.ast.get_node(node).kind;
        let scoped = matches!(kind, AstKind::Block | AstKind::For);
        if scoped {
            self.scopes.push(HashMap::new());
        }
        let is_loop = matches!(kind, AstKind::While | AstKind::DoWhile | AstKind::For);
        let is_switch = kind == AstKind::Switch;
        if is_loop {
            self.loop_depth += 1;
        }
        if is_switch {
            self.switch_depth += 1;
            self.switches.push(SwitchContext::default());
        }
        let children = self.ast.children(node).collect::<Vec<_>>();
        for child in children {
            match self.ast.get_node(child).kind {
                AstKind::Decl | AstKind::Typedef => self.declaration(child),
                AstKind::EnumDecl => self.enum_declaration(child),
                _ => self.node(child),
            }
        }
        self.infer_expression(node);
        self.validate_statement(node);
        if is_loop {
            self.loop_depth -= 1;
        }
        if is_switch {
            self.switch_depth -= 1;
            self.switches.pop();
        }
        if scoped {
            self.scopes.pop();
        }
    }

    fn validate_statement(&mut self, node: NodeId) {
        match self.ast.get_node(node).kind {
            AstKind::Goto => {
                if let Some(AstLeaf::Label(name)) = self.ast.get_leaf_data(node).cloned()
                    && !self.labels.contains_key(&name)
                {
                    self.diagnostics.push(
                        UnknownLabel::new(
                            self.ast.get_node(node).span,
                            name,
                            goto_reference(self.options),
                        )
                        .into(),
                    );
                }
                return;
            }
            AstKind::Case | AstKind::Default => {
                self.validate_switch_label(node);
                return;
            }
            AstKind::If | AstKind::While | AstKind::DoWhile | AstKind::For | AstKind::Switch => {
                self.validate_condition(node);
                return;
            }
            AstKind::Break if self.loop_depth == 0 && self.switch_depth == 0 => {
                self.diagnostics.push(
                    MisplacedBreak::new(
                        self.ast.get_node(node).span,
                        break_reference(self.options),
                    )
                    .into(),
                );
                return;
            }
            AstKind::Continue if self.loop_depth == 0 => {
                self.diagnostics.push(
                    MisplacedContinue::new(
                        self.ast.get_node(node).span,
                        continue_reference(self.options),
                    )
                    .into(),
                );
                return;
            }
            AstKind::Return => {}
            _ => return,
        }
        let Some(return_ty) = self.current_return else {
            return;
        };
        let has_value = self.ast.children(node).next().is_some();
        let is_void = matches!(self.types.kind(return_ty), TypeKind::Void);
        let message = match (is_void, has_value) {
            (true, true) => Some("void function must not return a value"),
            (false, false) => Some("non-void function must return a value"),
            _ => None,
        };
        if let Some(message) = message {
            self.diagnostics.push(
                InvalidReturn::new(
                    self.ast.get_node(node).span,
                    message,
                    return_reference(self.options),
                )
                .into(),
            );
            return;
        }
        if !is_void && has_value {
            let expression = self.ast.children(node).next().unwrap();
            let source = self
                .ast
                .get_annotation(expression)
                .and_then(|info| info.ty)
                .unwrap_or(return_ty);
            let source = self.assignment_source(return_ty, source, expression);
            if !self.assignment_compatible(return_ty, source, expression) {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(expression).span,
                        None,
                        format!(
                            "cannot return value of {} type as {}",
                            self.type_category(source),
                            self.type_category(return_ty)
                        ),
                        return_conversion_reference(self.options),
                    )
                    .into(),
                );
            } else {
                self.record_conversion(expression, return_ty);
            }
        }
    }

    fn validate_switch_label(&mut self, node: NodeId) {
        let Some(context) = self.switches.last_mut() else {
            self.diagnostics.push(
                MisplacedSwitchLabel::new(
                    self.ast.get_node(node).span,
                    switch_label_reference(self.options),
                )
                .into(),
            );
            return;
        };
        let span = self.ast.get_node(node).span;
        if self.ast.get_node(node).kind == AstKind::Default {
            if let Some(previous) = context.default.replace(span) {
                self.diagnostics.push(
                    DuplicateSwitchLabel::new(
                        span,
                        previous,
                        "duplicate default label",
                        switch_case_reference(self.options),
                    )
                    .into(),
                );
            }
            return;
        }
        let expression = self.ast.children(node).next().unwrap();
        let Some(value) = self
            .ast
            .get_annotation(expression)
            .and_then(|info| info.constant)
        else {
            self.diagnostics.push(
                IntegerConstantRequired::new(
                    self.ast.get_node(expression).span,
                    "case label is not an integer constant expression",
                    switch_case_reference(self.options),
                )
                .into(),
            );
            return;
        };
        if let Some(previous) = context.cases.insert(value, span) {
            self.diagnostics.push(
                DuplicateSwitchLabel::new(
                    span,
                    previous,
                    format!("duplicate case value {value}"),
                    switch_case_reference(self.options),
                )
                .into(),
            );
        }
    }

    fn validate_condition(&mut self, node: NodeId) {
        let kind = self.ast.get_node(node).kind;
        let children = self.ast.children(node).collect::<Vec<_>>();
        let index = match kind {
            AstKind::DoWhile | AstKind::For => 1,
            _ => 0,
        };
        let Some(condition) = children.get(index).copied() else {
            return;
        };
        if self.ast.get_node(condition).kind == AstKind::Empty {
            return;
        }
        let Some(ty) = self.ast.get_annotation(condition).and_then(|info| info.ty) else {
            return;
        };
        if self.types.kind(ty) == &TypeKind::Error {
            return;
        }
        let valid = if kind == AstKind::Switch {
            matches!(
                self.types.kind(ty),
                TypeKind::Integer(_) | TypeKind::Enum(_)
            )
        } else {
            self.is_arithmetic(ty) || matches!(self.types.kind(ty), TypeKind::Pointer(_))
        };
        if valid {
            if kind == AstKind::Switch {
                let promoted = self.integer_promotion(ty);
                self.record_conversion(condition, promoted);
            }
            return;
        }
        let statement = match kind {
            AstKind::If => "if",
            AstKind::While => "while",
            AstKind::DoWhile => "do-while",
            AstKind::For => "for",
            AstKind::Switch => "switch",
            _ => unreachable!(),
        };
        let expected = if kind == AstKind::Switch {
            "integer"
        } else {
            "scalar"
        };
        self.diagnostics.push(
            InvalidControllingExpression::new(
                self.ast.get_node(condition).span,
                format!("{statement} condition must have {expected} type"),
                condition_reference(self.options, kind),
            )
            .into(),
        );
    }

    fn declaration(&mut self, node: NodeId) {
        let leaf = self.ast.get_leaf_data(node).cloned();
        let (name, parsed_ty, typedef) = match leaf {
            Some(AstLeaf::Param { name, ty }) | Some(AstLeaf::Decl { name, ty }) => {
                (name, ty, false)
            }
            Some(AstLeaf::Typedef { name, ty }) => (name, ty, true),
            _ => return,
        };
        if name.is_empty() {
            return;
        }
        let span = self.ast.get_node(node).span;
        self.validate_parsed_type(span, &parsed_ty);
        let children = self.ast.children(node).collect::<Vec<_>>();
        let mut ty = if self.ast.get_node(node).kind == AstKind::Param {
            self.canonical_parameter_type(&parsed_ty)
        } else {
            self.canonical_type(&parsed_ty)
        };
        let previous = self.scopes.last().unwrap().get(&name).cloned();
        let redefined = previous.is_some();
        let entity = self.new_entity();
        self.ast.set_annotation(
            node,
            NodeSemantics {
                ty: Some(ty),
                entity: Some(entity),
                category: ValueCategory::Lvalue,
                ..NodeSemantics::default()
            },
        );
        if let Some(previous) = previous {
            self.diagnostics.push(
                Redefinition::new(
                    span,
                    previous.span,
                    name.clone(),
                    redefinition_reference(self.options),
                )
                .into(),
            );
        } else {
            self.scopes.last_mut().unwrap().insert(
                name.clone(),
                Symbol {
                    span,
                    ty,
                    entity,
                    typedef,
                    defined: true,
                    constant: None,
                },
            );
        }
        for &child in &children {
            self.node(child);
        }
        if self.ast.get_node(node).kind == AstKind::Decl
            && let Some(&initializer) = children.first()
            && self.ast.get_node(initializer).kind == AstKind::InitializerList
            && let TypeKind::Array(element, None) = self.types.kind(ty)
            && let Some(length) = self.inferred_array_length(initializer)
        {
            ty = self.types.intern(TypeKind::Array(*element, Some(length)));
            self.ast.set_annotation(
                node,
                NodeSemantics {
                    ty: Some(ty),
                    entity: Some(entity),
                    category: ValueCategory::Lvalue,
                    ..NodeSemantics::default()
                },
            );
            if !redefined {
                self.scopes.last_mut().unwrap().get_mut(&name).unwrap().ty = ty;
            }
        }
        if !typedef {
            let message = match self.types.kind(ty) {
                TypeKind::Void => Some(format!("object '{name}' cannot have void type")),
                TypeKind::Record(_) if self.type_layout(ty).is_none() => {
                    Some(format!("object '{name}' has incomplete struct type"))
                }
                TypeKind::Array(_, _) if self.type_layout(ty).is_none() => {
                    Some(format!("object '{name}' has incomplete array type"))
                }
                _ => None,
            };
            if let Some(message) = message {
                self.diagnostics.push(
                    CompleteObjectTypeRequired::new(
                        span,
                        message,
                        object_type_reference(self.options),
                    )
                    .into(),
                );
            }
        }
        if ty.qualifiers.is_restrict() && !matches!(self.types.kind(ty), TypeKind::Pointer(_)) {
            self.diagnostics.push(
                InvalidTypeQualifier::new(
                    span,
                    "restrict qualifier requires a pointer-derived object type",
                    qualifier_reference(self.options),
                )
                .into(),
            );
        }
        if self.ast.get_node(node).kind == AstKind::Decl
            && let Some(&initializer) = children.first()
        {
            self.validate_initializer(ty, initializer);
        }
    }

    fn inferred_array_length(&self, initializer: NodeId) -> Option<u64> {
        let mut next = 0_u64;
        let mut length = 0_u64;
        for value in self.ast.children(initializer) {
            if let Some(AstLeaf::DesignatedInitializer(InitializerDesignator::Index)) =
                self.ast.get_leaf_data(value)
            {
                let index = self.ast.children(value).next().unwrap();
                next = self.ast.get_annotation(index)?.constant?.try_into().ok()?;
            }
            next = next.checked_add(1)?;
            length = length.max(next);
        }
        Some(length)
    }

    fn validate_initializer(&mut self, target: QualType, initializer: NodeId) {
        if self.ast.get_node(initializer).kind == AstKind::InitializerList {
            self.validate_initializer_list(target, initializer);
            return;
        }
        let source = self
            .ast
            .get_annotation(initializer)
            .and_then(|info| info.ty)
            .unwrap_or(target);
        let source = self.assignment_source(target, source, initializer);
        if !self.assignment_compatible(target, source, initializer) {
            self.diagnostics.push(
                IncompatibleConversion::new(
                    self.ast.get_node(initializer).span,
                    None,
                    format!(
                        "cannot initialize {} with {} value",
                        self.type_category(target),
                        self.type_category(source)
                    ),
                    initializer_reference(self.options),
                )
                .into(),
            );
        } else {
            self.record_conversion(initializer, target);
        }
    }

    fn validate_initializer_list(&mut self, target: QualType, initializer: NodeId) {
        let aggregate = match self.types.kind(target) {
            TypeKind::Record(id) => {
                if self.records[self.record_indices[id]].kind == RecordKind::Union {
                    "union"
                } else {
                    "record"
                }
            }
            TypeKind::Array(_, Some(_)) => "array",
            _ => {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(initializer).span,
                        None,
                        "brace initializer requires an aggregate object",
                        initializer_reference(self.options),
                    )
                    .into(),
                );
                return;
            }
        };
        let values = self.ast.children(initializer).collect::<Vec<_>>();
        let mut next_path = self.first_initializer_path(target);
        for value in values {
            if self.ast.get_node(value).kind == AstKind::DesignatedInitializer {
                if let Some(path) = self.validate_designated_initializer(target, value) {
                    self.set_initializer_path(value, path.clone());
                    next_path = self.next_initializer_path(target, &path);
                }
            } else if let Some(path) = next_path {
                let selected_type = self.initializer_path_type(target, &path).unwrap();
                self.set_initializer_path(value, path.clone());
                self.validate_initializer_value(selected_type, value);
                next_path = self.next_initializer_path(target, &path);
            } else {
                self.diagnostics.push(
                    InvalidOperands::new(
                        self.ast.get_node(value).span,
                        format!("too many initializers for {aggregate}"),
                        initializer_reference(self.options),
                    )
                    .into(),
                );
            }
        }
    }

    fn first_initializer_path(&self, target: QualType) -> Option<Vec<usize>> {
        (self.initializer_member_count(target)? != 0).then(|| vec![0])
    }

    fn initializer_member_count(&self, target: QualType) -> Option<usize> {
        match self.types.kind(target) {
            TypeKind::Record(id) => {
                let record = &self.records[self.record_indices[id]];
                Some(if record.kind == RecordKind::Union {
                    usize::from(!record.fields.is_empty())
                } else {
                    record.fields.len()
                })
            }
            TypeKind::Array(_, Some(length)) => Some(*length as usize),
            _ => None,
        }
    }

    fn initializer_member_type(&self, target: QualType, index: usize) -> Option<QualType> {
        match self.types.kind(target) {
            TypeKind::Record(id) => self.records[self.record_indices[id]]
                .fields
                .get(index)
                .map(|field| field.ty),
            TypeKind::Array(element, Some(length)) if index < *length as usize => Some(*element),
            _ => None,
        }
    }

    fn initializer_path_type(&self, target: QualType, path: &[usize]) -> Option<QualType> {
        path.iter().try_fold(target, |target, &index| {
            self.initializer_member_type(target, index)
        })
    }

    fn next_initializer_path(&self, target: QualType, path: &[usize]) -> Option<Vec<usize>> {
        let mut next = path.to_vec();
        while let Some(index) = next.pop() {
            let parent = self.initializer_path_type(target, &next)?;
            if index + 1 < self.initializer_member_count(parent)? {
                next.push(index + 1);
                return Some(next);
            }
        }
        None
    }

    fn set_initializer_path(&mut self, initializer: NodeId, path: Vec<usize>) {
        let mut semantics = self
            .ast
            .get_annotation(initializer)
            .cloned()
            .unwrap_or_default();
        semantics.initializer_path = Some(path);
        self.ast.set_annotation(initializer, semantics);
    }

    fn validate_designated_initializer(
        &mut self,
        target: QualType,
        initializer: NodeId,
    ) -> Option<Vec<usize>> {
        let (index, selected, selected_type) = match self.ast.get_leaf_data(initializer).cloned()? {
            AstLeaf::DesignatedInitializer(InitializerDesignator::Field(name)) => {
                let TypeKind::Record(id) = self.types.kind(target) else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "field designator requires a record",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                let Some((index, field)) = self.records[self.record_indices[id]]
                    .fields
                    .iter()
                    .enumerate()
                    .find(|(_, field)| field.name == name)
                    .map(|(index, field)| (index, field.ty))
                else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            format!("record has no member named '{name}'"),
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                (index, self.ast.children(initializer).next().unwrap(), field)
            }
            AstLeaf::DesignatedInitializer(InitializerDesignator::Index) => {
                let TypeKind::Array(element, Some(length)) = self.types.kind(target) else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "array designator requires an array",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                let (element, length) = (*element, *length);
                let mut children = self.ast.children(initializer);
                let index_expression = children.next().unwrap();
                let selected = children.next().unwrap();
                let Some(index) = self
                    .ast
                    .get_annotation(index_expression)
                    .and_then(|info| info.constant)
                    .filter(|index| *index >= 0)
                    .map(|index| index as usize)
                else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "array designator requires a nonnegative integer constant",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                };
                if index >= length as usize {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(initializer).span,
                            "array designator index exceeds array bounds",
                            initializer_reference(self.options),
                        )
                        .into(),
                    );
                    return None;
                }
                (index, selected, element)
            }
            _ => unreachable!(),
        };
        self.ast.set_annotation(
            initializer,
            NodeSemantics {
                member_index: Some(index),
                ..NodeSemantics::default()
            },
        );
        let mut path = vec![index];
        if self.ast.get_node(selected).kind == AstKind::DesignatedInitializer {
            path.extend(self.validate_designated_initializer(selected_type, selected)?);
        } else {
            self.validate_initializer_value(selected_type, selected);
        }
        Some(path)
    }

    fn validate_initializer_value(&mut self, target: QualType, value: NodeId) {
        if self.ast.get_node(value).kind == AstKind::DesignatedInitializer {
            self.validate_designated_initializer(target, value);
            return;
        }
        if self.ast.get_node(value).kind == AstKind::InitializerList {
            self.validate_initializer_list(target, value);
            return;
        }
        let source = self
            .ast
            .get_annotation(value)
            .and_then(|info| info.ty)
            .unwrap_or(target);
        let source = self.assignment_source(target, source, value);
        if !self.assignment_compatible(target, source, value) {
            self.diagnostics.push(
                IncompatibleConversion::new(
                    self.ast.get_node(value).span,
                    None,
                    format!(
                        "cannot initialize array element of {} type with {} value",
                        self.type_category(target),
                        self.type_category(source)
                    ),
                    initializer_reference(self.options),
                )
                .into(),
            );
        } else {
            self.record_conversion(value, target);
        }
    }

    fn validate_parsed_type(&mut self, span: Span, parsed: &CType) {
        match parsed {
            CType::Invalid(spelling) => self.diagnostics.push(
                InvalidTypeSpecifiers::new(
                    span,
                    spelling.clone(),
                    type_specifier_reference(self.options),
                )
                .into(),
            ),
            CType::Const(inner)
            | CType::Volatile(inner)
            | CType::Restrict(inner)
            | CType::Pointer(inner)
            | CType::Array(inner, _)
            | CType::Attributed(inner, _) => self.validate_parsed_type(span, inner),
            CType::Function { ret, params, .. } => {
                self.validate_parsed_type(span, ret);
                for param in params {
                    self.validate_parsed_type(span, &param.ty);
                }
            }
            _ => {}
        }
    }

    fn lookup(&self, name: &str) -> Option<Symbol> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    fn require_name(&mut self, node: NodeId, name: &str) -> Option<Symbol> {
        let symbol = self.lookup(name);
        if symbol.is_none() {
            self.diagnostics.push(
                UndeclaredIdentifier::new(
                    self.ast.get_node(node).span,
                    name,
                    undeclared_reference(self.options),
                )
                .into(),
            );
        }
        symbol
    }

    fn infer_expression(&mut self, node: NodeId) {
        let kind = self.ast.get_node(node).kind;
        let int = self.types.intern(TypeKind::Integer(IntegerKind::Int));
        let error = self.types.intern(TypeKind::Error);
        let mut entity = None;
        let mut member_index = None;
        let mut call_designator_ty = None;
        let mut named_constant = None;
        let (ty, category) = match kind {
            AstKind::Int => {
                let Some(AstLeaf::Int(literal)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                (
                    self.integer_literal_type(node, &literal.spelling, literal.value.to_u64()),
                    ValueCategory::Value,
                )
            }
            AstKind::FloatLiteral => (self.types.intern(TypeKind::Double), ValueCategory::Value),
            AstKind::Character => (int, ValueCategory::Value),
            AstKind::String => {
                let char_ty = self.types.intern(TypeKind::Integer(IntegerKind::Char));
                (
                    self.types.intern(TypeKind::Array(char_ty, None)),
                    ValueCategory::Lvalue,
                )
            }
            AstKind::Var => {
                let Some(AstLeaf::Var(name)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                match self.require_name(node, &name) {
                    Some(symbol) if !symbol.typedef => {
                        entity = Some(symbol.entity);
                        named_constant = symbol.constant;
                        let category = if symbol.constant.is_some() {
                            ValueCategory::Value
                        } else if matches!(self.types.kind(symbol.ty), TypeKind::Function { .. }) {
                            ValueCategory::Function
                        } else {
                            ValueCategory::Lvalue
                        };
                        (symbol.ty, category)
                    }
                    _ => (error, ValueCategory::Value),
                }
            }
            AstKind::Member => {
                let Some(AstLeaf::Member { name, indirect }) =
                    self.ast.get_leaf_data(node).cloned()
                else {
                    return;
                };
                let base = self.ast.children(node).next().unwrap();
                let base_ty = self
                    .ast
                    .get_annotation(base)
                    .and_then(|info| info.ty)
                    .unwrap_or(error);
                let record = if indirect {
                    match self.types.kind(base_ty) {
                        TypeKind::Pointer(pointee) => match self.types.kind(*pointee) {
                            TypeKind::Record(id) => Some(*id),
                            _ => None,
                        },
                        _ => None,
                    }
                } else {
                    match self.types.kind(base_ty) {
                        TypeKind::Record(id) => Some(*id),
                        _ => None,
                    }
                };
                let field = record
                    .and_then(|id| self.record_indices.get(&id).copied())
                    .and_then(|index| {
                        self.records[index]
                            .fields
                            .iter()
                            .enumerate()
                            .find(|(_, field)| field.name == name)
                    });
                if let Some((index, field)) = field {
                    member_index = Some(index);
                    (field.ty, ValueCategory::Lvalue)
                } else {
                    if let Some(id) = record {
                        let record_name = self.records[self.record_indices[&id]].name.clone();
                        self.diagnostics.push(
                            InvalidOperands::new(
                                self.ast.get_node(node).span,
                                format!("struct '{record_name}' has no member named '{name}'"),
                                member_reference(self.options),
                            )
                            .into(),
                        );
                    } else if !matches!(self.types.kind(base_ty), TypeKind::Error) {
                        self.diagnostics.push(
                            InvalidOperands::new(
                                self.ast.get_node(node).span,
                                "member access requires a struct operand".to_string(),
                                member_reference(self.options),
                            )
                            .into(),
                        );
                    }
                    (error, ValueCategory::Value)
                }
            }
            AstKind::Call => {
                let Some(AstLeaf::Call(name)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                match self.require_name(node, &name) {
                    Some(symbol) => {
                        let function_ty = match self.types.kind(symbol.ty) {
                            TypeKind::Function { .. } => Some(symbol.ty),
                            TypeKind::Pointer(pointee)
                                if matches!(
                                    self.types.kind(*pointee),
                                    TypeKind::Function { .. }
                                ) =>
                            {
                                Some(*pointee)
                            }
                            _ => None,
                        };
                        if let Some(function_ty) = function_ty {
                            call_designator_ty = Some(symbol.ty);
                            entity = Some(symbol.entity);
                            let arguments = self.ast.children(node).collect::<Vec<_>>();
                            self.check_call(
                                node,
                                &name,
                                symbol.span,
                                function_ty,
                                &arguments,
                                error,
                            )
                        } else {
                            self.diagnostics.push(
                                CalledObjectNotFunction::new(
                                    self.ast.get_node(node).span,
                                    symbol.span,
                                    name,
                                    call_designator_reference(self.options),
                                )
                                .into(),
                            );
                            (error, ValueCategory::Value)
                        }
                    }
                    None => (error, ValueCategory::Value),
                }
            }
            AstKind::CallExpr => {
                let children = self.ast.children(node).collect::<Vec<_>>();
                let callee = children[0];
                let callee_info = self.ast.get_annotation(callee).cloned().unwrap_or_default();
                let designator_ty = callee_info.ty.unwrap_or(error);
                let function_ty = match self.types.kind(designator_ty) {
                    TypeKind::Function { .. } => Some(designator_ty),
                    TypeKind::Pointer(pointee)
                        if matches!(self.types.kind(*pointee), TypeKind::Function { .. }) =>
                    {
                        Some(*pointee)
                    }
                    _ => None,
                };
                if let Some(function_ty) = function_ty {
                    call_designator_ty = Some(designator_ty);
                    self.check_call(
                        node,
                        "called expression",
                        self.ast.get_node(callee).span,
                        function_ty,
                        &children[1..],
                        error,
                    )
                } else {
                    self.diagnostics.push(
                        CalledObjectNotFunction::new(
                            self.ast.get_node(node).span,
                            self.ast.get_node(callee).span,
                            "expression".to_string(),
                            call_designator_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::Add | AstKind::Sub | AstKind::Mul | AstKind::Div => {
                let children = self.ast.children(node).collect::<Vec<_>>();
                let mut operands = self.child_types(node);
                for (&child, operand) in children.iter().zip(&mut operands) {
                    let element = match self.types.kind(*operand) {
                        TypeKind::Array(element, _) => Some(*element),
                        _ => None,
                    };
                    if let Some(element) = element {
                        let pointer = self.types.intern(TypeKind::Pointer(element));
                        self.record_conversion(child, pointer);
                        *operand = pointer;
                    }
                }
                let pointer_result = if operands.len() == 2 {
                    match (
                        self.types.kind(operands[0]),
                        self.types.kind(operands[1]),
                        kind,
                    ) {
                        (
                            TypeKind::Pointer(_),
                            TypeKind::Integer(_) | TypeKind::Enum(_),
                            AstKind::Add | AstKind::Sub,
                        ) => Some(operands[0]),
                        (
                            TypeKind::Integer(_) | TypeKind::Enum(_),
                            TypeKind::Pointer(_),
                            AstKind::Add,
                        ) => Some(operands[1]),
                        _ => None,
                    }
                } else {
                    None
                };
                let pointer_difference = if operands.len() == 2 && kind == AstKind::Sub {
                    match (self.types.kind(operands[0]), self.types.kind(operands[1])) {
                        (TypeKind::Pointer(left), TypeKind::Pointer(right))
                            if self.types.kind(*left) == self.types.kind(*right)
                                && self.type_size(*left).is_some() =>
                        {
                            Some(
                                self.types
                                    .intern(TypeKind::Integer(self.target.pointer_sized_integer())),
                            )
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let subtracts_pointers = operands.len() == 2
                    && kind == AstKind::Sub
                    && operands
                        .iter()
                        .all(|ty| matches!(self.types.kind(*ty), TypeKind::Pointer(_)));
                if let Some(result) = pointer_difference {
                    (result, ValueCategory::Value)
                } else if subtracts_pointers {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            "pointer subtraction requires pointers to compatible complete object types",
                            operand_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                } else if let Some(result) = pointer_result {
                    // The scaling in codegen reads the index operand's width, so an
                    // enum index has to reach it as its promoted integer type.
                    for (index, &operand) in operands.iter().enumerate() {
                        if matches!(self.types.kind(operand), TypeKind::Enum(_)) {
                            let promoted = self.integer_promotion(operand);
                            self.record_conversion(children[index], promoted);
                        }
                    }
                    let TypeKind::Pointer(pointee) = self.types.kind(result) else {
                        unreachable!("pointer arithmetic result has pointer type")
                    };
                    if self.type_size(*pointee).is_some() {
                        (result, ValueCategory::Value)
                    } else {
                        self.diagnostics.push(
                            InvalidOperands::new(
                                self.ast.get_node(node).span,
                                "pointer arithmetic requires a pointer to a complete object type",
                                operand_reference(self.options, kind),
                            )
                            .into(),
                        );
                        (error, ValueCategory::Value)
                    }
                } else if operands.len() == 2 && operands.iter().all(|&ty| self.is_arithmetic(ty)) {
                    let result = self.common_arithmetic_type(operands[0], operands[1]);
                    self.record_operand_conversions(node, &operands, result);
                    (result, ValueCategory::Value)
                } else if operands
                    .iter()
                    .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
                {
                    (error, ValueCategory::Value)
                } else {
                    let operator = operator_text(kind);
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            format!("operator '{operator}' requires arithmetic operands"),
                            operand_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::Mod
            | AstKind::Shl
            | AstKind::Shr
            | AstKind::BitAnd
            | AstKind::BitXor
            | AstKind::BitOr => {
                let operands = self.child_types(node);
                if operands.len() == 2 && operands.iter().all(|&ty| self.is_integer(ty)) {
                    let result = self.common_arithmetic_type(operands[0], operands[1]);
                    self.record_operand_conversions(node, &operands, result);
                    (result, ValueCategory::Value)
                } else if operands
                    .iter()
                    .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
                {
                    (error, ValueCategory::Value)
                } else {
                    let operator = operator_text(kind);
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            format!("operator '{operator}' requires integer operands"),
                            operand_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::Lt | AstKind::Gt | AstKind::Le | AstKind::Ge | AstKind::Eq | AstKind::Ne => {
                let operands = self.child_types(node);
                let arithmetic =
                    operands.len() == 2 && operands.iter().all(|&ty| self.is_arithmetic(ty));
                let pointers = operands.len() == 2
                    && operands
                        .iter()
                        .all(|&ty| matches!(self.types.kind(ty), TypeKind::Pointer(_)));
                if arithmetic {
                    // The result is `int`, but the operands are still brought to
                    // their common type first, so the comparison never sees two
                    // different widths.
                    let common = self.common_arithmetic_type(operands[0], operands[1]);
                    self.record_operand_conversions(node, &operands, common);
                    (int, ValueCategory::Value)
                } else if pointers {
                    (int, ValueCategory::Value)
                } else if operands
                    .iter()
                    .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
                {
                    (error, ValueCategory::Value)
                } else {
                    let operator = operator_text(kind);
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            format!("operator '{operator}' requires compatible scalar operands"),
                            operand_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::LogAnd | AstKind::LogOr => {
                let operands = self.child_types(node);
                if operands.len() == 2 && operands.iter().all(|&ty| self.is_scalar(ty)) {
                    (int, ValueCategory::Value)
                } else if operands
                    .iter()
                    .any(|&ty| self.types.kind(ty) == &TypeKind::Error)
                {
                    (error, ValueCategory::Value)
                } else {
                    let operator = operator_text(kind);
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            format!("operator '{operator}' requires scalar operands"),
                            operand_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::Neg | AstKind::Pos | AstKind::BitNot | AstKind::Not => {
                let operand = self.child_types(node).first().copied().unwrap_or(error);
                let valid = match kind {
                    AstKind::Neg | AstKind::Pos => self.is_arithmetic(operand),
                    AstKind::BitNot => self.is_integer(operand),
                    AstKind::Not => self.is_scalar(operand),
                    _ => unreachable!(),
                };
                if valid {
                    if kind == AstKind::Not {
                        (int, ValueCategory::Value)
                    } else {
                        // The operation runs at the promoted type, so the
                        // operand is converted to it like a binary operand is.
                        let result = self.integer_promotion(operand);
                        self.record_operand_conversions(node, &[operand], result);
                        (result, ValueCategory::Value)
                    }
                } else if self.types.kind(operand) == &TypeKind::Error {
                    (error, ValueCategory::Value)
                } else {
                    let expected = if kind == AstKind::BitNot {
                        "integer"
                    } else if kind == AstKind::Not {
                        "scalar"
                    } else {
                        "arithmetic"
                    };
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            format!(
                                "operator '{}' requires an {expected} operand",
                                operator_text(kind)
                            ),
                            operand_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::AddressOf => {
                let child = self.ast.children(node).next().unwrap();
                let info = self.ast.get_annotation(child).cloned().unwrap_or_default();
                let operand = info.ty.unwrap_or(error);
                if matches!(
                    info.category,
                    ValueCategory::Lvalue | ValueCategory::Function
                ) {
                    (
                        self.types.intern(TypeKind::Pointer(operand)),
                        ValueCategory::Value,
                    )
                } else if self.types.kind(operand) == &TypeKind::Error {
                    (error, ValueCategory::Value)
                } else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            "operator '&' requires an lvalue or function operand",
                            operand_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::Deref => {
                let operand = self.child_types(node).first().copied().unwrap_or(error);
                match self.types.kind(operand).clone() {
                    TypeKind::Pointer(pointee) => {
                        let category =
                            if matches!(self.types.kind(pointee), TypeKind::Function { .. }) {
                                ValueCategory::Function
                            } else {
                                ValueCategory::Lvalue
                            };
                        (pointee, category)
                    }
                    TypeKind::Error => (error, ValueCategory::Value),
                    _ => {
                        self.diagnostics.push(
                            InvalidOperands::new(
                                self.ast.get_node(node).span,
                                "operator '*' requires a pointer operand",
                                operand_reference(self.options, kind),
                            )
                            .into(),
                        );
                        (error, ValueCategory::Value)
                    }
                }
            }
            AstKind::Comma => {
                let operands = self.child_types(node);
                (
                    operands.last().copied().unwrap_or(error),
                    ValueCategory::Value,
                )
            }
            AstKind::Conditional => {
                let children = self.ast.children(node).collect::<Vec<_>>();
                let types = self.child_types(node);
                if children.len() != 3 || types.len() != 3 {
                    (error, ValueCategory::Value)
                } else if !self.is_scalar(types[0]) {
                    if self.types.kind(types[0]) != &TypeKind::Error {
                        self.diagnostics.push(
                            InvalidOperands::new(
                                self.ast.get_node(children[0]).span,
                                "conditional operator requires a scalar condition",
                                conditional_reference(self.options),
                            )
                            .into(),
                        );
                    }
                    (error, ValueCategory::Value)
                } else if self.is_arithmetic(types[1]) && self.is_arithmetic(types[2]) {
                    let common = self.common_arithmetic_type(types[1], types[2]);
                    self.record_conversion(children[1], common);
                    self.record_conversion(children[2], common);
                    (common, ValueCategory::Value)
                } else if types[1] == types[2]
                    || matches!(self.types.kind(types[1]), TypeKind::Pointer(_))
                        && self
                            .ast
                            .get_annotation(children[2])
                            .and_then(|info| info.constant)
                            == Some(0)
                {
                    self.record_conversion(children[2], types[1]);
                    (types[1], ValueCategory::Value)
                } else if matches!(self.types.kind(types[2]), TypeKind::Pointer(_))
                    && self
                        .ast
                        .get_annotation(children[1])
                        .and_then(|info| info.constant)
                        == Some(0)
                {
                    self.record_conversion(children[1], types[2]);
                    (types[2], ValueCategory::Value)
                } else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            "conditional operator has incompatible result operands",
                            conditional_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            AstKind::Cast => {
                let Some(AstLeaf::Type(parsed)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                let target = self.canonical_type(&parsed);
                let operand = self.ast.children(node).next().unwrap();
                let source = self.child_types(node).first().copied().unwrap_or(error);
                let source = self.value_conversion(operand, source);
                let valid = matches!(self.types.kind(target), TypeKind::Void)
                    || self.is_scalar(target) && self.is_scalar(source);
                if !valid
                    && self.types.kind(target) != &TypeKind::Error
                    && self.types.kind(source) != &TypeKind::Error
                {
                    self.diagnostics.push(
                        IncompatibleConversion::new(
                            self.ast.get_node(node).span,
                            None,
                            format!(
                                "cannot cast {} expression to {} type",
                                self.type_category(source),
                                self.type_category(target)
                            ),
                            cast_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                } else {
                    (target, ValueCategory::Value)
                }
            }
            AstKind::SizeofType | AstKind::SizeofExpr => {
                let operand_ty = if kind == AstKind::SizeofType {
                    let Some(AstLeaf::Type(parsed)) = self.ast.get_leaf_data(node).cloned() else {
                        return;
                    };
                    self.canonical_type(&parsed)
                } else {
                    self.child_types(node).first().copied().unwrap_or(error)
                };
                let size = self.type_size(operand_ty);
                if size.is_none()
                    && matches!(
                        self.types.kind(operand_ty),
                        TypeKind::Void | TypeKind::Function { .. } | TypeKind::Array(_, None)
                    )
                {
                    self.diagnostics.push(
                        CompleteObjectTypeRequired::new(
                            self.ast.get_node(node).span,
                            "sizeof requires a complete object type",
                            sizeof_reference(self.options),
                        )
                        .into(),
                    );
                }
                let size_ty = self.types.intern(TypeKind::Integer(unsigned_corresponding(
                    self.target.pointer_sized_integer(),
                )));
                self.ast.set_annotation(
                    node,
                    NodeSemantics {
                        ty: Some(size_ty),
                        entity: None,
                        category: ValueCategory::Value,
                        constant: size.map(|value| value as i64),
                        conversions: Vec::new(),
                        member_index: None,
                        initializer_path: None,
                        call_designator_ty: None,
                    },
                );
                return;
            }
            AstKind::Assign => {
                let Some(AstLeaf::Assign(name)) = self.ast.get_leaf_data(node).cloned() else {
                    return;
                };
                let Some(symbol) = self.require_name(node, &name) else {
                    self.ast.set_annotation(
                        node,
                        NodeSemantics {
                            ty: Some(error),
                            ..NodeSemantics::default()
                        },
                    );
                    return;
                };
                entity = Some(symbol.entity);
                let rhs = self.ast.children(node).next().unwrap();
                let source = self
                    .ast
                    .get_annotation(rhs)
                    .and_then(|info| info.ty)
                    .unwrap_or(error);
                let source = self.assignment_source(symbol.ty, source, rhs);
                if symbol.ty.qualifiers.is_const() {
                    self.diagnostics.push(
                        ModifiableLvalueRequired::new(
                            self.ast.get_node(node).span,
                            "left operand is not a modifiable lvalue",
                            assignment_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                } else if !self.assignment_compatible(symbol.ty, source, rhs) {
                    self.diagnostics.push(
                        IncompatibleConversion::new(
                            self.ast.get_node(node).span,
                            None,
                            self.conversion_message(symbol.ty, source),
                            simple_assignment_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                } else {
                    self.record_conversion(rhs, symbol.ty);
                    (symbol.ty, ValueCategory::Value)
                }
            }
            AstKind::AssignExpr => {
                let children = self.ast.children(node).collect::<Vec<_>>();
                let lhs = children[0];
                let rhs = children[1];
                let lhs_info = self.ast.get_annotation(lhs).cloned().unwrap_or_default();
                let lhs_ty = lhs_info.ty.unwrap_or(error);
                if lhs_info.category != ValueCategory::Lvalue || lhs_ty.qualifiers.is_const() {
                    self.diagnostics.push(
                        ModifiableLvalueRequired::new(
                            self.ast.get_node(node).span,
                            "left operand is not a modifiable lvalue",
                            assignment_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                } else {
                    let source = self
                        .ast
                        .get_annotation(rhs)
                        .and_then(|info| info.ty)
                        .unwrap_or(error);
                    let source = self.assignment_source(lhs_ty, source, rhs);
                    if !self.assignment_compatible(lhs_ty, source, rhs) {
                        self.diagnostics.push(
                            IncompatibleConversion::new(
                                self.ast.get_node(node).span,
                                None,
                                self.conversion_message(lhs_ty, source),
                                simple_assignment_reference(self.options),
                            )
                            .into(),
                        );
                        (error, ValueCategory::Value)
                    } else {
                        self.record_conversion(rhs, lhs_ty);
                        (lhs_ty, ValueCategory::Value)
                    }
                }
            }
            AstKind::AddAssign
            | AstKind::SubAssign
            | AstKind::MulAssign
            | AstKind::DivAssign
            | AstKind::ModAssign
            | AstKind::ShlAssign
            | AstKind::ShrAssign
            | AstKind::AndAssign
            | AstKind::XorAssign
            | AstKind::OrAssign => {
                let children = self.ast.children(node).collect::<Vec<_>>();
                let lhs = children.first().copied();
                let lhs_info = lhs.and_then(|child| self.ast.get_annotation(child));
                if lhs_info.is_none_or(|info| info.category != ValueCategory::Lvalue) {
                    self.diagnostics.push(
                        ModifiableLvalueRequired::new(
                            self.ast.get_node(node).span,
                            "left operand is not a modifiable lvalue",
                            assignment_reference(self.options),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                } else {
                    let lhs_ty = lhs_info.and_then(|info| info.ty).unwrap_or(error);
                    if lhs_ty.qualifiers.is_const() {
                        self.diagnostics.push(
                            ModifiableLvalueRequired::new(
                                self.ast.get_node(node).span,
                                "left operand is not a modifiable lvalue",
                                assignment_reference(self.options),
                            )
                            .into(),
                        );
                        (error, ValueCategory::Value)
                    } else {
                        // `E1 op= E2` is `E1 = E1 op E2`: the operation itself
                        // runs at the usual-arithmetic-conversions common type,
                        // and only the store narrows back to E1's type.
                        if let Some(rhs) = children.get(1).copied()
                            && let Some(rhs_ty) =
                                self.ast.get_annotation(rhs).and_then(|info| info.ty)
                            && self.is_arithmetic(lhs_ty)
                            && self.is_arithmetic(rhs_ty)
                        {
                            let common = self.common_arithmetic_type(lhs_ty, rhs_ty);
                            self.record_conversion(rhs, common);
                        }
                        (lhs_ty, ValueCategory::Value)
                    }
                }
            }
            AstKind::PreInc | AstKind::PreDec | AstKind::PostInc | AstKind::PostDec => {
                let child = self.ast.children(node).next().unwrap();
                let info = self.ast.get_annotation(child).cloned().unwrap_or_default();
                let operand_ty = info.ty.unwrap_or(error);
                if info.category != ValueCategory::Lvalue || operand_ty.qualifiers.is_const() {
                    self.diagnostics.push(
                        ModifiableLvalueRequired::new(
                            self.ast.get_node(node).span,
                            "operand is not a modifiable lvalue",
                            increment_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                } else if self.is_arithmetic(operand_ty)
                    || matches!(self.types.kind(operand_ty), TypeKind::Pointer(_))
                {
                    (operand_ty, ValueCategory::Value)
                } else {
                    self.diagnostics.push(
                        InvalidOperands::new(
                            self.ast.get_node(node).span,
                            "increment and decrement require real or pointer operands",
                            increment_reference(self.options, kind),
                        )
                        .into(),
                    );
                    (error, ValueCategory::Value)
                }
            }
            _ => return,
        };
        let constant = named_constant.or_else(|| self.constant_value(node, kind, ty));
        self.ast.set_annotation(
            node,
            NodeSemantics {
                ty: Some(ty),
                entity,
                category,
                conversions: Vec::new(),
                constant,
                member_index,
                initializer_path: None,
                call_designator_ty,
            },
        );
    }

    fn check_call(
        &mut self,
        node: NodeId,
        callee: &str,
        declaration_span: Span,
        function_ty: QualType,
        arguments: &[NodeId],
        error: QualType,
    ) -> (QualType, ValueCategory) {
        let TypeKind::Function {
            ret,
            params,
            varargs,
            prototype,
        } = self.types.kind(function_ty).clone()
        else {
            unreachable!("call checker receives a function type")
        };
        let actual = arguments.len();
        let valid = if !prototype {
            true
        } else if varargs {
            actual >= params.len()
        } else {
            actual == params.len()
        };
        if !valid {
            self.diagnostics.push(
                ArgumentMismatch::new(
                    self.ast.get_node(node).span,
                    declaration_span,
                    format!(
                        "function '{callee}' expects {} arguments but {actual} was provided",
                        params.len()
                    ),
                    call_reference(self.options),
                )
                .into(),
            );
        }
        for (index, (&argument, &parameter)) in arguments.iter().zip(&params).enumerate() {
            let source = self
                .ast
                .get_annotation(argument)
                .and_then(|info| info.ty)
                .unwrap_or(error);
            let source = self.assignment_source(parameter, source, argument);
            if !self.assignment_compatible(parameter, source, argument) {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(argument).span,
                        Some(declaration_span),
                        format!(
                            "argument {} to '{callee}' has incompatible {} type",
                            index + 1,
                            self.type_category(source)
                        ),
                        call_reference(self.options),
                    )
                    .into(),
                );
            } else {
                self.record_conversion(argument, parameter);
            }
        }
        if varargs {
            for &argument in arguments.iter().skip(params.len()) {
                let source = self
                    .ast
                    .get_annotation(argument)
                    .and_then(|info| info.ty)
                    .unwrap_or(error);
                if self.is_integer(source) {
                    let promoted = self.integer_promotion(source);
                    self.record_conversion(argument, promoted);
                }
            }
        }
        (ret, ValueCategory::Value)
    }

    fn constant_value(&self, node: NodeId, kind: AstKind, result_ty: QualType) -> Option<i64> {
        match kind {
            AstKind::Int => {
                let AstLeaf::Int(value) = self.ast.get_leaf_data(node)? else {
                    return None;
                };
                return Some(value.value.to_i64());
            }
            AstKind::Character => {
                let AstLeaf::Character(value) = self.ast.get_leaf_data(node)? else {
                    return None;
                };
                return decode_character_constant(value);
            }
            _ => {}
        }
        let children = self.ast.children(node).collect::<Vec<_>>();
        let child_constant = |child| {
            self.ast
                .get_annotation(child)
                .and_then(|info| info.constant)
        };
        if let (AstKind::Cast, [child], TypeKind::Integer(integer)) =
            (kind, children.as_slice(), self.types.kind(result_ty))
            && let Some(AstLeaf::Float(value)) = self.ast.get_leaf_data(*child)
        {
            return self.cast_float_constant(value.value, *integer);
        }
        match (kind, children.as_slice()) {
            (AstKind::LogAnd, [left, right]) => {
                return child_constant(*left).and_then(|left| {
                    if left == 0 {
                        Some(0)
                    } else {
                        child_constant(*right).map(|right| i64::from(right != 0))
                    }
                });
            }
            (AstKind::LogOr, [left, right]) => {
                return child_constant(*left).and_then(|left| {
                    if left != 0 {
                        Some(1)
                    } else {
                        child_constant(*right).map(|right| i64::from(right != 0))
                    }
                });
            }
            (AstKind::Conditional, [condition, when_true, when_false]) => {
                return child_constant(*condition).and_then(|condition| {
                    child_constant(if condition != 0 {
                        *when_true
                    } else {
                        *when_false
                    })
                });
            }
            _ => {}
        }
        let values = self
            .ast
            .children(node)
            .map(child_constant)
            .collect::<Option<Vec<_>>>()?;
        match (kind, values.as_slice()) {
            (AstKind::Add, [left, right]) => left.checked_add(*right),
            (AstKind::Sub, [left, right]) => left.checked_sub(*right),
            (AstKind::Mul, [left, right]) => left.checked_mul(*right),
            (AstKind::Div, [_, 0]) => None,
            (AstKind::Div, [left, right]) => left.checked_div(*right),
            (AstKind::Mod, [_, 0]) => None,
            (AstKind::Mod, [left, right]) => left.checked_rem(*right),
            (AstKind::Shl, [left, right]) => u32::try_from(*right)
                .ok()
                .and_then(|shift| left.checked_shl(shift)),
            (AstKind::Shr, [left, right]) => u32::try_from(*right)
                .ok()
                .and_then(|shift| left.checked_shr(shift)),
            (AstKind::BitAnd, [left, right]) => Some(left & right),
            (AstKind::BitXor, [left, right]) => Some(left ^ right),
            (AstKind::BitOr, [left, right]) => Some(left | right),
            (AstKind::Lt, [left, right]) => Some(i64::from(left < right)),
            (AstKind::Gt, [left, right]) => Some(i64::from(left > right)),
            (AstKind::Le, [left, right]) => Some(i64::from(left <= right)),
            (AstKind::Ge, [left, right]) => Some(i64::from(left >= right)),
            (AstKind::Eq, [left, right]) => Some(i64::from(left == right)),
            (AstKind::Ne, [left, right]) => Some(i64::from(left != right)),
            (AstKind::Cast, [value]) => match self.types.kind(result_ty) {
                TypeKind::Integer(kind) => Some(self.cast_integer_constant(*value, *kind)),
                _ => None,
            },
            (AstKind::Neg, [value]) => value.checked_neg(),
            (AstKind::Pos, [value]) => Some(*value),
            (AstKind::Not, [value]) => Some(i64::from(*value == 0)),
            (AstKind::BitNot, [value]) => Some(!value),
            _ => None,
        }
    }

    fn cast_integer_constant(&self, value: i64, kind: IntegerKind) -> i64 {
        if kind == IntegerKind::Bool {
            return i64::from(value != 0);
        }
        let width = self.target.integer_width(kind);
        if width == 64 {
            return value;
        }
        let mask = (1_u64 << width) - 1;
        let bits = (value as u64) & mask;
        if is_signed_integer(kind, self.target) {
            let shift = 64 - width;
            ((bits << shift) as i64) >> shift
        } else {
            bits as i64
        }
    }

    fn cast_float_constant(&self, value: f64, kind: IntegerKind) -> Option<i64> {
        if kind == IntegerKind::Bool {
            return Some(i64::from(value != 0.0));
        }
        let value = value.trunc();
        if !value.is_finite() {
            return None;
        }
        let width = self.target.integer_width(kind);
        if is_signed_integer(kind, self.target) {
            let limit = 2_f64.powi((width - 1) as i32);
            (-limit..limit).contains(&value).then_some(value as i64)
        } else {
            let limit = 2_f64.powi(width as i32);
            (0.0..limit)
                .contains(&value)
                .then_some((value as u64) as i64)
        }
    }

    fn child_types(&self, node: NodeId) -> Vec<QualType> {
        self.ast
            .children(node)
            .filter_map(|child| self.ast.get_annotation(child).and_then(|info| info.ty))
            .collect()
    }

    fn record_operand_conversions(
        &mut self,
        parent: NodeId,
        operands: &[QualType],
        target: QualType,
    ) {
        let children = self.ast.children(parent).collect::<Vec<_>>();
        for (&child, &source) in children.iter().zip(operands) {
            if source != target {
                let mut semantics = self.ast.get_annotation(child).cloned().unwrap_or_default();
                semantics.conversions.push(target);
                self.ast.set_annotation(child, semantics);
            }
        }
    }

    fn record_conversion(&mut self, node: NodeId, target: QualType) {
        let mut semantics = self.ast.get_annotation(node).cloned().unwrap_or_default();
        if semantics.ty != Some(target) {
            semantics.conversions.push(target);
            self.ast.set_annotation(node, semantics);
        }
    }

    fn assignment_source(
        &mut self,
        target: QualType,
        source: QualType,
        source_node: NodeId,
    ) -> QualType {
        if matches!(self.types.kind(target), TypeKind::Array(_, _)) {
            source
        } else {
            self.value_conversion(source_node, source)
        }
    }

    fn value_conversion(&mut self, node: NodeId, source: QualType) -> QualType {
        let target = match self.types.kind(source).clone() {
            TypeKind::Array(element, _) => self.types.intern(TypeKind::Pointer(element)),
            TypeKind::Function { .. } => self.types.intern(TypeKind::Pointer(source)),
            _ => return source,
        };
        self.record_conversion(node, target);
        target
    }

    fn is_arithmetic(&self, ty: QualType) -> bool {
        matches!(
            self.types.kind(ty),
            TypeKind::Integer(_)
                | TypeKind::Enum(_)
                | TypeKind::Float
                | TypeKind::Double
                | TypeKind::LongDouble
        )
    }

    fn is_integer(&self, ty: QualType) -> bool {
        matches!(
            self.types.kind(ty),
            TypeKind::Integer(_) | TypeKind::Enum(_)
        )
    }

    fn is_scalar(&self, ty: QualType) -> bool {
        self.is_arithmetic(ty) || matches!(self.types.kind(ty), TypeKind::Pointer(_))
    }

    fn assignment_compatible(
        &self,
        target: QualType,
        source: QualType,
        source_node: NodeId,
    ) -> bool {
        match (self.types.kind(target), self.types.kind(source)) {
            (TypeKind::Error, _) | (_, TypeKind::Error) => true,
            (
                TypeKind::Integer(_) | TypeKind::Enum(_),
                TypeKind::Integer(_)
                | TypeKind::Enum(_)
                | TypeKind::Float
                | TypeKind::Double
                | TypeKind::LongDouble,
            ) => true,
            (
                TypeKind::Float | TypeKind::Double | TypeKind::LongDouble,
                TypeKind::Integer(_)
                | TypeKind::Enum(_)
                | TypeKind::Float
                | TypeKind::Double
                | TypeKind::LongDouble,
            ) => true,
            (TypeKind::Pointer(_), TypeKind::Pointer(_)) => true,
            (TypeKind::Pointer(target), TypeKind::Array(source, _)) => {
                self.types.kind(*target) == self.types.kind(*source)
            }
            (TypeKind::Pointer(target), TypeKind::Function { .. }) => {
                self.types.kind(*target) == self.types.kind(source)
            }
            (TypeKind::Pointer(_), TypeKind::Integer(_)) => {
                self.ast
                    .get_annotation(source_node)
                    .and_then(|info| info.constant)
                    == Some(0)
            }
            (left, right) => left == right,
        }
    }

    fn conversion_message(&self, target: QualType, source: QualType) -> String {
        let target = self.type_category(target);
        let source = self.type_category(source);
        format!("cannot assign value of {source} type to {target}")
    }

    fn type_category(&self, ty: QualType) -> &'static str {
        match self.types.kind(ty) {
            TypeKind::Integer(_) => "integer",
            TypeKind::Float | TypeKind::Double | TypeKind::LongDouble => "floating",
            TypeKind::Pointer(_) => "pointer",
            TypeKind::Array(_, _) => "array",
            TypeKind::Function { .. } => "function",
            TypeKind::Record(_) => "record",
            TypeKind::Enum(_) => "enumeration",
            TypeKind::Void => "void",
            TypeKind::Error => "invalid",
        }
    }

    fn type_size(&self, ty: QualType) -> Option<u64> {
        self.type_layout(ty).map(|(size, _)| size)
    }

    fn type_layout(&self, ty: QualType) -> Option<(u64, u64)> {
        match self.types.kind(ty) {
            TypeKind::Array(element, Some(length)) => {
                let (size, align) = self.type_layout(*element)?;
                Some((size.checked_mul(*length)?, align))
            }
            TypeKind::Record(id) => self
                .record_indices
                .get(id)
                .map(|&index| &self.records[index])
                .filter(|record| record.size != 0)
                .map(|record| (record.size, record.align)),
            kind => self.target.scalar_layout(kind),
        }
    }

    fn integer_promotion(&mut self, ty: QualType) -> QualType {
        if self.integer_rank(ty) < 3 {
            self.types.intern(TypeKind::Integer(IntegerKind::Int))
        } else {
            ty
        }
    }

    fn integer_literal_type(&mut self, node: NodeId, spelling: &str, value: u64) -> QualType {
        let suffix_start = spelling.trim_end_matches(['u', 'U', 'l', 'L']).len();
        let suffix = spelling[suffix_start..].to_ascii_lowercase();
        if !matches!(
            suffix.as_str(),
            "" | "u" | "l" | "ul" | "lu" | "ll" | "ull" | "llu"
        ) {
            self.diagnostics.push(
                InvalidIntegerLiteral::new(
                    self.ast.get_node(node).span,
                    format!("invalid integer suffix in '{spelling}'"),
                    integer_literal_reference(self.options),
                )
                .into(),
            );
            return self.types.intern(TypeKind::Error);
        }
        let digits = &spelling[..suffix_start];
        let decimal = !(digits.starts_with("0x")
            || digits.starts_with("0X")
            || digits.starts_with("0b")
            || digits.starts_with("0B")
            || (digits.len() > 1 && digits.starts_with('0')));
        let unsigned = suffix.contains('u');
        let long_count = suffix.chars().filter(|&ch| ch == 'l').count();
        let candidates: &[IntegerKind] = match (decimal, unsigned, long_count) {
            (_, true, 2) => &[IntegerKind::UnsignedLongLong],
            (_, true, 1) => &[IntegerKind::UnsignedLong, IntegerKind::UnsignedLongLong],
            (_, true, 0) => &[
                IntegerKind::UnsignedInt,
                IntegerKind::UnsignedLong,
                IntegerKind::UnsignedLongLong,
            ],
            (true, false, 2) => &[IntegerKind::LongLong],
            (true, false, 1) => &[IntegerKind::Long, IntegerKind::LongLong],
            (true, false, 0) => &[IntegerKind::Int, IntegerKind::Long, IntegerKind::LongLong],
            (false, false, 2) => &[IntegerKind::LongLong, IntegerKind::UnsignedLongLong],
            (false, false, 1) => &[
                IntegerKind::Long,
                IntegerKind::UnsignedLong,
                IntegerKind::LongLong,
                IntegerKind::UnsignedLongLong,
            ],
            (false, false, 0) => &[
                IntegerKind::Int,
                IntegerKind::UnsignedInt,
                IntegerKind::Long,
                IntegerKind::UnsignedLong,
                IntegerKind::LongLong,
                IntegerKind::UnsignedLongLong,
            ],
            _ => unreachable!(),
        };
        for &kind in candidates {
            let width = self.target.integer_width(kind);
            let signed = is_signed_integer(kind, self.target);
            let fits = if signed {
                width == 64 && value <= i64::MAX as u64
                    || width < 64 && value < (1u64 << (width - 1))
            } else {
                width == 64 || value < (1u64 << width)
            };
            if fits {
                return self.types.intern(TypeKind::Integer(kind));
            }
        }
        self.diagnostics.push(
            InvalidIntegerLiteral::new(
                self.ast.get_node(node).span,
                format!("integer literal '{spelling}' is too large for its candidate types"),
                integer_literal_reference(self.options),
            )
            .into(),
        );
        self.types.intern(TypeKind::Error)
    }

    fn common_arithmetic_type(&mut self, left: QualType, right: QualType) -> QualType {
        let left = self.integer_promotion(left);
        let right = self.integer_promotion(right);
        if left == right {
            return left;
        }
        let (TypeKind::Integer(left_kind), TypeKind::Integer(right_kind)) =
            (self.types.kind(left), self.types.kind(right))
        else {
            return if self.integer_rank(left) >= self.integer_rank(right) {
                left
            } else {
                right
            };
        };
        let (left_kind, right_kind) = (*left_kind, *right_kind);
        let left_signed = is_signed_integer(left_kind, self.target);
        let right_signed = is_signed_integer(right_kind, self.target);
        if left_signed == right_signed {
            return if self.integer_rank(left) >= self.integer_rank(right) {
                left
            } else {
                right
            };
        }
        let (unsigned, unsigned_kind, signed, signed_kind) = if left_signed {
            (right, right_kind, left, left_kind)
        } else {
            (left, left_kind, right, right_kind)
        };
        if self.integer_rank(unsigned) >= self.integer_rank(signed) {
            return unsigned;
        }
        if self.target.integer_width(signed_kind) > self.target.integer_width(unsigned_kind) {
            return signed;
        }
        self.types
            .intern(TypeKind::Integer(unsigned_corresponding(signed_kind)))
    }

    fn integer_rank(&self, ty: QualType) -> u8 {
        match self.types.kind(ty) {
            TypeKind::Integer(IntegerKind::Bool) => 0,
            TypeKind::Integer(
                IntegerKind::Char | IntegerKind::SignedChar | IntegerKind::UnsignedChar,
            ) => 1,
            TypeKind::Integer(IntegerKind::Short | IntegerKind::UnsignedShort) => 2,
            TypeKind::Integer(IntegerKind::Int | IntegerKind::UnsignedInt) => 3,
            TypeKind::Integer(IntegerKind::Long | IntegerKind::UnsignedLong) => 4,
            TypeKind::Integer(IntegerKind::LongLong | IntegerKind::UnsignedLongLong) => 5,
            TypeKind::Float => 6,
            TypeKind::Double => 7,
            TypeKind::LongDouble => 8,
            _ => 0,
        }
    }

    /// Declarators nest as deeply as the source spells them, so the
    /// pointer/array/qualifier spine is unwound iteratively; recursing once per
    /// level overflows the stack on inputs with thousands of `*`.
    fn canonical_type(&mut self, parsed: &CType) -> QualType {
        let mut spine = Vec::new();
        let mut leaf = parsed;
        while let Some(inner) = leaf.derived_inner() {
            spine.push(leaf);
            leaf = inner;
        }

        let mut ty = self.canonical_leaf_type(leaf);
        while let Some(derived) = spine.pop() {
            ty = self.derive_canonical_type(derived, ty);
        }
        ty
    }

    fn derive_canonical_type(&mut self, derived: &CType, inner: QualType) -> QualType {
        match derived {
            CType::Pointer(_) => self.types.intern(TypeKind::Pointer(inner)),
            CType::Array(_, length) => {
                let length = length
                    .as_ref()
                    .and_then(|length| self.constant_array_length(length));
                self.types.intern(TypeKind::Array(inner, length))
            }
            CType::Const(_) => with_qualifier(inner, Qualifiers::CONST),
            CType::Volatile(_) => with_qualifier(inner, Qualifiers::VOLATILE),
            CType::Restrict(_) => with_qualifier(inner, Qualifiers::RESTRICT),
            CType::Attributed(_, _) => inner,
            _ => unreachable!("not a derived type"),
        }
    }

    fn constant_array_length(&mut self, length: &ArrayLength) -> Option<u64> {
        let expression = length.expression()?;
        if self.ast.get_annotation(expression).is_none() {
            self.node(expression);
        }
        self.ast
            .get_annotation(expression)
            .and_then(|semantics| semantics.constant)
            .and_then(|value| value.try_into().ok())
    }

    fn canonical_leaf_type(&mut self, parsed: &CType) -> QualType {
        match parsed {
            CType::Invalid(_) => self.types.intern(TypeKind::Error),
            CType::Void => self.types.intern(TypeKind::Void),
            CType::Bool => self.types.intern(TypeKind::Integer(IntegerKind::Bool)),
            CType::Char => self.types.intern(TypeKind::Integer(IntegerKind::Char)),
            CType::SignedChar => self
                .types
                .intern(TypeKind::Integer(IntegerKind::SignedChar)),
            CType::UnsignedChar => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedChar)),
            CType::Short => self.types.intern(TypeKind::Integer(IntegerKind::Short)),
            CType::UnsignedShort => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedShort)),
            CType::Int => self.types.intern(TypeKind::Integer(IntegerKind::Int)),
            CType::UnsignedInt => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedInt)),
            CType::Long => self.types.intern(TypeKind::Integer(IntegerKind::Long)),
            CType::UnsignedLong => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedLong)),
            CType::LongLong => self.types.intern(TypeKind::Integer(IntegerKind::LongLong)),
            CType::UnsignedLongLong => self
                .types
                .intern(TypeKind::Integer(IntegerKind::UnsignedLongLong)),
            CType::Float => self.types.intern(TypeKind::Float),
            CType::Double => self.types.intern(TypeKind::Double),
            CType::LongDouble => self.types.intern(TypeKind::LongDouble),
            CType::Function {
                ret,
                params,
                varargs,
                has_parameter_type_list,
            } => {
                let ret = self.canonical_type(ret);
                let params = self.canonical_params(params);
                self.types.intern(TypeKind::Function {
                    ret,
                    params,
                    varargs: *varargs,
                    prototype: *has_parameter_type_list
                        || self.options.std_version == StdVersion::C23,
                })
            }
            CType::Record(_, id, _) => self.types.intern(TypeKind::Record(*id)),
            CType::Enum(name) => self.types.intern(TypeKind::Enum(name.clone())),
            CType::Named(name) => self
                .lookup(name)
                .filter(|symbol| symbol.typedef)
                .map(|symbol| symbol.ty)
                .unwrap_or_else(|| self.types.intern(TypeKind::Error)),
            CType::Builtin(_) => self.types.intern(TypeKind::Error),
            CType::Pointer(_)
            | CType::Array(..)
            | CType::Const(_)
            | CType::Volatile(_)
            | CType::Restrict(_)
            | CType::Attributed(..) => unreachable!("derived type is not a leaf"),
        }
    }

    fn canonical_params(&mut self, params: &[CParam]) -> Vec<QualType> {
        params
            .iter()
            .map(|param| self.canonical_parameter_type(&param.ty))
            .collect()
    }

    fn canonical_parameter_type(&mut self, parsed: &CType) -> QualType {
        let ty = self.canonical_type(parsed);
        match self.types.kind(ty).clone() {
            TypeKind::Array(element, _) => self.types.intern(TypeKind::Pointer(element)),
            TypeKind::Function { .. } => self.types.intern(TypeKind::Pointer(ty)),
            _ => ty,
        }
    }
}

fn with_qualifier(mut ty: QualType, qualifier: u8) -> QualType {
    ty.qualifiers = ty.qualifiers.with(qualifier);
    ty
}

fn align_to(value: u64, align: u64) -> u64 {
    value.div_ceil(align) * align
}

fn operator_text(kind: AstKind) -> &'static str {
    match kind {
        AstKind::Add => "+",
        AstKind::Sub => "-",
        AstKind::Mul => "*",
        AstKind::Div => "/",
        AstKind::Mod => "%",
        AstKind::Shl => "<<",
        AstKind::Shr => ">>",
        AstKind::BitAnd => "&",
        AstKind::BitXor => "^",
        AstKind::BitOr => "|",
        AstKind::Lt => "<",
        AstKind::Gt => ">",
        AstKind::Le => "<=",
        AstKind::Ge => ">=",
        AstKind::Eq => "==",
        AstKind::Ne => "!=",
        AstKind::LogAnd => "&&",
        AstKind::LogOr => "||",
        AstKind::Neg => "-",
        AstKind::Pos => "+",
        AstKind::BitNot => "~",
        AstKind::Not => "!",
        AstKind::AddressOf => "&",
        AstKind::Deref => "*",
        _ => unreachable!(),
    }
}

fn is_signed_integer(kind: IntegerKind, target: TargetProfile) -> bool {
    match kind {
        IntegerKind::UnsignedChar
        | IntegerKind::UnsignedShort
        | IntegerKind::UnsignedInt
        | IntegerKind::UnsignedLong
        | IntegerKind::UnsignedLongLong => false,
        IntegerKind::Char => target.plain_char_signed,
        _ => true,
    }
}

fn unsigned_corresponding(kind: IntegerKind) -> IntegerKind {
    match kind {
        IntegerKind::Char | IntegerKind::SignedChar => IntegerKind::UnsignedChar,
        IntegerKind::Short => IntegerKind::UnsignedShort,
        IntegerKind::Int => IntegerKind::UnsignedInt,
        IntegerKind::Long => IntegerKind::UnsignedLong,
        IntegerKind::LongLong => IntegerKind::UnsignedLongLong,
        _ => kind,
    }
}
