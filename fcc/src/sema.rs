//! Semantic analysis entry points and analyzer state.

use crate::ast::{Ast, AstKind, RecordId};
use crate::diagnostics::{Diagnostic, Span};
use crate::lang_options::LangOptions;
use std::collections::HashMap;

mod conversions;
mod declarations;
mod expressions;
mod initialization;
mod references;
mod statements;
mod target;
mod types;
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
