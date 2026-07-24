use std::collections::HashMap;

use tir::graph::{Dag, MutDag, NodeId};

use crate::ast::{Ast, AstKind, AstLeaf, CParam, CType, RecordId, RecordKind};
use crate::diagnostics::{
    ArgumentMismatch, CalledObjectNotFunction, CompleteObjectTypeRequired, ConflictingDeclaration,
    Diagnostic, DuplicateLabel, DuplicateSwitchLabel, IncompatibleConversion,
    IntegerConstantRequired, InvalidControllingExpression, InvalidIntegerLiteral, InvalidOperands,
    InvalidReturn, InvalidTypeQualifier, InvalidTypeSpecifiers, MisplacedBreak, MisplacedContinue,
    MisplacedSwitchLabel, ModifiableLvalueRequired, Redefinition, Span, UndeclaredIdentifier,
    UnknownLabel,
};
use crate::lang_options::{LangOptions, StdVersion};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataModel {
    Ilp32,
    Lp64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TargetProfile {
    model: DataModel,
    plain_char_signed: bool,
}

impl TargetProfile {
    pub fn for_march(march: &str) -> Result<Self, String> {
        let normalized = march.to_ascii_lowercase();
        if normalized.starts_with("riscv32") || normalized.starts_with("rv32") {
            Ok(Self {
                model: DataModel::Ilp32,
                plain_char_signed: true,
            })
        } else if normalized.starts_with("riscv64")
            || normalized.starts_with("rv64")
            || normalized == "x86_64"
        {
            Ok(Self {
                model: DataModel::Lp64,
                plain_char_signed: true,
            })
        } else if normalized.starts_with("arm64") || normalized.starts_with("aarch64") {
            Ok(Self {
                model: DataModel::Lp64,
                plain_char_signed: false,
            })
        } else {
            Err(format!("no C data model for target '{march}'"))
        }
    }

    pub fn host() -> Result<Self, String> {
        Self::for_march(std::env::consts::ARCH)
    }

    pub fn pointer_width(self) -> u32 {
        match self.model {
            DataModel::Ilp32 => 32,
            DataModel::Lp64 => 64,
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
}

pub fn analyze(ast: Ast, options: LangOptions) -> Result<TypedAst, Vec<Diagnostic>> {
    let target = TargetProfile::host().unwrap_or(TargetProfile {
        model: DataModel::Lp64,
        plain_char_signed: true,
    });
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
                AstKind::Function => {
                    self.declare_file_item(item);
                    self.function(item);
                }
                AstKind::Prototype | AstKind::Global | AstKind::Typedef => {
                    self.declare_file_item(item);
                }
                AstKind::DeclGroup => {
                    let declarations = self.ast.children(item).collect::<Vec<_>>();
                    for declaration in declarations {
                        if self.ast.get_node(declaration).kind == AstKind::RecordDecl {
                            self.record_declaration(declaration);
                        } else {
                            self.declare_file_item(declaration);
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
            offset = align_to(offset, align);
            fields.push(RecordField { name, ty, offset });
            offset += size;
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
            }) => (
                name,
                self.function_type(node, ret, has_parameter_type_list),
                false,
            ),
            Some(AstLeaf::Global { name, ty }) => (name, self.canonical_type(&ty), false),
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
        let defined = self.ast.get_node(node).kind == AstKind::Function;
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
            if previous.ty != ty || previous.typedef != typedef {
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
                },
            );
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
                    let ty = self.canonical_type(&ty);
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
        let ty = self.canonical_type(&parsed_ty);
        if !typedef {
            let message = match self.types.kind(ty) {
                TypeKind::Void => Some(format!("object '{name}' cannot have void type")),
                TypeKind::Record(_) if self.type_layout(ty).is_none() => {
                    Some(format!("object '{name}' has incomplete struct type"))
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
        let previous = self.scopes.last().unwrap().get(&name).cloned();
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
                name,
                Symbol {
                    span,
                    ty,
                    entity,
                    typedef,
                    defined: true,
                },
            );
        }
        let children = self.ast.children(node).collect::<Vec<_>>();
        for &child in &children {
            self.node(child);
        }
        if self.ast.get_node(node).kind == AstKind::Decl
            && let Some(&initializer) = children.first()
        {
            let source = self
                .ast
                .get_annotation(initializer)
                .and_then(|info| info.ty)
                .unwrap_or(ty);
            if !self.assignment_compatible(ty, source, initializer) {
                self.diagnostics.push(
                    IncompatibleConversion::new(
                        self.ast.get_node(initializer).span,
                        None,
                        format!(
                            "cannot initialize {} with {} value",
                            self.type_category(ty),
                            self.type_category(source)
                        ),
                        initializer_reference(self.options),
                    )
                    .into(),
                );
            } else {
                self.record_conversion(initializer, ty);
            }
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
                        let category =
                            if matches!(self.types.kind(symbol.ty), TypeKind::Function { .. }) {
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
                    Some(symbol) => match self.types.kind(symbol.ty).clone() {
                        TypeKind::Function {
                            ret,
                            params,
                            varargs,
                            prototype,
                        } => {
                            entity = Some(symbol.entity);
                            let actual = self.ast.children(node).count();
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
                                        symbol.span,
                                        format!(
                                            "function '{name}' expects {} arguments but {actual} was provided",
                                            params.len()
                                        ),
                                        call_reference(self.options),
                                    )
                                    .into(),
                                );
                            }
                            let arguments = self.ast.children(node).collect::<Vec<_>>();
                            for (index, (&argument, &parameter)) in
                                arguments.iter().zip(&params).enumerate()
                            {
                                let source = self
                                    .ast
                                    .get_annotation(argument)
                                    .and_then(|info| info.ty)
                                    .unwrap_or(error);
                                if !self.assignment_compatible(parameter, source, argument) {
                                    self.diagnostics.push(
                                        IncompatibleConversion::new(
                                            self.ast.get_node(argument).span,
                                            Some(symbol.span),
                                            format!(
                                                "argument {} to '{name}' has incompatible {} type",
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
                            (ret, ValueCategory::Value)
                        }
                        _ => {
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
                    },
                    None => (error, ValueCategory::Value),
                }
            }
            AstKind::Add | AstKind::Sub | AstKind::Mul | AstKind::Div => {
                let operands = self.child_types(node);
                let pointer_result = if operands.len() == 2 {
                    match (
                        self.types.kind(operands[0]),
                        self.types.kind(operands[1]),
                        kind,
                    ) {
                        (
                            TypeKind::Pointer(_),
                            TypeKind::Integer(_),
                            AstKind::Add | AstKind::Sub,
                        ) => Some(operands[0]),
                        (TypeKind::Integer(_), TypeKind::Pointer(_), AstKind::Add) => {
                            Some(operands[1])
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                if let Some(result) = pointer_result {
                    (result, ValueCategory::Value)
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
                if arithmetic || pointers {
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
                    let result = if kind == AstKind::Not {
                        int
                    } else {
                        self.integer_promotion(operand)
                    };
                    (result, ValueCategory::Value)
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
                    (
                        self.common_arithmetic_type(types[1], types[2]),
                        ValueCategory::Value,
                    )
                } else if types[1] == types[2]
                    || matches!(self.types.kind(types[1]), TypeKind::Pointer(_))
                        && self
                            .ast
                            .get_annotation(children[2])
                            .and_then(|info| info.constant)
                            == Some(0)
                {
                    (types[1], ValueCategory::Value)
                } else if matches!(self.types.kind(types[2]), TypeKind::Pointer(_))
                    && self
                        .ast
                        .get_annotation(children[1])
                        .and_then(|info| info.constant)
                        == Some(0)
                {
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
                let source = self.child_types(node).first().copied().unwrap_or(error);
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
                let size_ty = match self.target.model {
                    DataModel::Ilp32 => self
                        .types
                        .intern(TypeKind::Integer(IntegerKind::UnsignedInt)),
                    DataModel::Lp64 => self
                        .types
                        .intern(TypeKind::Integer(IntegerKind::UnsignedLong)),
                };
                self.ast.set_annotation(
                    node,
                    NodeSemantics {
                        ty: Some(size_ty),
                        entity: None,
                        category: ValueCategory::Value,
                        constant: size.map(|value| value as i64),
                        conversions: Vec::new(),
                        member_index: None,
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
            AstKind::AssignExpr
            | AstKind::AddAssign
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
        let constant = self.constant_value(node, kind);
        self.ast.set_annotation(
            node,
            NodeSemantics {
                ty: Some(ty),
                entity,
                category,
                conversions: Vec::new(),
                constant,
                member_index,
            },
        );
    }

    fn constant_value(&self, node: NodeId, kind: AstKind) -> Option<i64> {
        if kind == AstKind::Int {
            let AstLeaf::Int(value) = self.ast.get_leaf_data(node)? else {
                return None;
            };
            return Some(value.value.to_i64());
        }
        let values = self
            .ast
            .children(node)
            .map(|child| self.ast.get_annotation(child)?.constant)
            .collect::<Option<Vec<_>>>()?;
        match (kind, values.as_slice()) {
            (AstKind::Add, [left, right]) => left.checked_add(*right),
            (AstKind::Sub, [left, right]) => left.checked_sub(*right),
            (AstKind::Mul, [left, right]) => left.checked_mul(*right),
            (AstKind::Div, [_, 0]) => None,
            (AstKind::Div, [left, right]) => left.checked_div(*right),
            _ => None,
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

    fn is_arithmetic(&self, ty: QualType) -> bool {
        matches!(
            self.types.kind(ty),
            TypeKind::Integer(_) | TypeKind::Float | TypeKind::Double | TypeKind::LongDouble
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
                TypeKind::Integer(_),
                TypeKind::Integer(_) | TypeKind::Float | TypeKind::Double | TypeKind::LongDouble,
            ) => true,
            (
                TypeKind::Float | TypeKind::Double | TypeKind::LongDouble,
                TypeKind::Integer(_) | TypeKind::Float | TypeKind::Double | TypeKind::LongDouble,
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
            TypeKind::Integer(kind) => {
                let size = (self.target.integer_width(*kind) / 8) as u64;
                Some((size, size))
            }
            TypeKind::Float => Some((4, 4)),
            TypeKind::Double => Some((8, 8)),
            TypeKind::LongDouble => Some((16, 16)),
            TypeKind::Pointer(_) => {
                let size = (self.target.pointer_width() / 8) as u64;
                Some((size, size))
            }
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
            _ => None,
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

    fn canonical_type(&mut self, parsed: &CType) -> QualType {
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
            CType::Pointer(inner) => {
                let inner = self.canonical_type(inner);
                self.types.intern(TypeKind::Pointer(inner))
            }
            CType::Array(inner, length) => {
                let inner = self.canonical_type(inner);
                let length = length.as_deref().and_then(|value| value.parse().ok());
                self.types.intern(TypeKind::Array(inner, length))
            }
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
            CType::Const(inner) => {
                let mut ty = self.canonical_type(inner);
                ty.qualifiers = ty.qualifiers.with(Qualifiers::CONST);
                ty
            }
            CType::Volatile(inner) => {
                let mut ty = self.canonical_type(inner);
                ty.qualifiers = ty.qualifiers.with(Qualifiers::VOLATILE);
                ty
            }
            CType::Restrict(inner) => {
                let mut ty = self.canonical_type(inner);
                ty.qualifiers = ty.qualifiers.with(Qualifiers::RESTRICT);
                ty
            }
            CType::Attributed(inner, _) => self.canonical_type(inner),
            CType::Builtin(_) => self.types.intern(TypeKind::Error),
        }
    }

    fn canonical_params(&mut self, params: &[CParam]) -> Vec<QualType> {
        params
            .iter()
            .map(|param| self.canonical_type(&param.ty))
            .collect()
    }
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
