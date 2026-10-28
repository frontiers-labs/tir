//! Semantic analysis entry points and analyzer state.

use crate::ast::{Ast, AstKind, RecordId};
use crate::diagnostics::{Diagnostic, Span};
use crate::lang_options::LangOptions;
use std::collections::HashMap;

mod conversions;
mod declarations;
mod expressions;
mod initialization;
mod layout;
pub(crate) mod references;
mod statements;
mod target;
mod types;
pub use layout::Layouts;
pub use target::TargetProfile;
pub use types::{
    EntityId, IntegerKind, NodeSemantics, QualType, Qualifiers, RecordDefinition, RecordField,
    TypeId, TypeInterner, TypeKind, TypedAst, ValueCategory,
};

#[derive(Clone)]
struct Symbol {
    span: Span,
    ty: QualType,
    entity: EntityId,
    typedef: bool,
    defined: bool,
    constant: Option<i64>,
}

pub fn analyze(
    context: &tir::Context,
    ast: Ast,
    options: LangOptions,
) -> Result<TypedAst, Vec<Diagnostic>> {
    // Where the host has no backend, fall back to a modeled target rather than
    // a hand-written profile, so the layout still comes from a description.
    let (march, machine) = [std::env::consts::ARCH, "x86_64"]
        .into_iter()
        .find_map(|march| Some((march, tir::backend::select_target(march, None, None).ok()?)))
        .expect("x86_64 is always modeled");
    let target =
        TargetProfile::for_target(context, march, machine.as_ref()).expect("a modeled target");
    let layout = target
        .layout_spec(context, machine.as_ref())
        .expect("a modeled target declares its layout");
    analyze_with_target(context, ast, options, target, layout)
}

/// Analyze `ast` for `target`, whose types `layout` lays out. The types are
/// interned in `context`, the one the unit is generated into.
pub fn analyze_with_target(
    context: &tir::Context,
    mut ast: Ast,
    options: LangOptions,
    target: TargetProfile,
    layout: tir::attributes::AttributeValue,
) -> Result<TypedAst, Vec<Diagnostic>> {
    let (types, records, layouts, diagnostics) = {
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
            layouts: Layouts::new(context, layout),
        };
        analyzer.translation_unit();
        let names = analyzer.records.iter().map(|record| record.name.as_str());
        analyzer.layouts.declare_incomplete(names);
        (
            analyzer.types,
            analyzer.records,
            analyzer.layouts,
            analyzer.diagnostics,
        )
    };
    if diagnostics.is_empty() {
        Ok(TypedAst {
            ast,
            types,
            target,
            records,
            layouts,
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
    layouts: Layouts,
}

#[derive(Default)]
struct SwitchContext {
    cases: HashMap<i64, Span>,
    default: Option<Span>,
}

fn with_qualifier(mut ty: QualType, qualifier: u8) -> QualType {
    ty.qualifiers = ty.qualifiers.with(qualifier);
    ty
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
        IntegerKind::BitInt { signed, .. } => signed,
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
        IntegerKind::BitInt { width, .. } => IntegerKind::BitInt {
            width,
            signed: false,
        },
        _ => kind,
    }
}
