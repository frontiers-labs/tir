//! Classifies C function signatures for target ABIs.

use super::{lower_type, node_entity, node_type, source_type_layout};
use crate::ast::{AstLeaf, RecordKind};
use crate::cir::VarArgsType;
use crate::diagnostics::Diagnostic;
use crate::sema::{EntityId, QualType, TargetProfile, TypeKind, TypedAst};
use tir::backend::abi::{Overflow, ValueKind, type_kind};
use tir::builtin::{FloatType, IntegerType, TupleType, UnitType};
use tir::graph::{Dag, NodeId};
use tir::ptr::PtrType;
use tir::{Context, TypeId};

#[derive(Clone)]
pub(super) struct Signature {
    pub(super) ret: AbiReturn,
    pub(super) params: Vec<AbiParameter>,
    pub(super) varargs: bool,
}

#[derive(Clone)]
pub(super) struct AbiParameter {
    pub(super) pieces: Vec<AbiPiece>,
    pub(super) grouped: bool,
    pub(super) indirect: bool,
    pub(super) alignment: u64,
    /// The source declared it `restrict`: nothing else the function reaches
    /// names the memory it points at.
    pub(super) noalias: bool,
}

#[derive(Clone, Copy)]
pub(super) struct AbiPiece {
    pub(super) offset: u64,
    pub(super) ty: TypeId,
}

#[derive(Clone)]
pub(super) struct AbiReturn {
    pub(super) ty: TypeId,
    pub(super) aggregate: Option<Vec<AbiPiece>>,
    pub(super) indirect: bool,
}

#[derive(Default)]
pub(super) struct AbiRegisterUsage {
    pub(super) integers: usize,
    pub(super) floats: usize,
}

impl AbiRegisterUsage {
    fn reserve_indirect_result(&mut self, target: TargetProfile) {
        let Some((kind, slot)) = target.indirect_result_argument_slots() else {
            return;
        };
        match kind {
            ValueKind::Int => self.integers = self.integers.max(slot),
            ValueKind::Float => self.floats = self.floats.max(slot),
            ValueKind::Vector => {}
        }
    }

    fn align_group(
        &mut self,
        context: &Context,
        target: TargetProfile,
        source_alignment: u64,
        pieces: &[AbiPiece],
    ) {
        for kind in [ValueKind::Int, ValueKind::Float] {
            if !pieces
                .iter()
                .any(|piece| type_kind(context, piece.ty) == kind)
            {
                continue;
            }
            let slot = match kind {
                ValueKind::Int => &mut self.integers,
                ValueKind::Float => &mut self.floats,
                ValueKind::Vector => unreachable!(),
            };
            *slot = target.align_argument_slot(kind, source_alignment, *slot);
        }
    }

    fn has_direct_registers(
        &self,
        context: &Context,
        target: TargetProfile,
        pieces: &[AbiPiece],
    ) -> bool {
        let mut integers = 0;
        let mut floats = 0;
        for piece in pieces {
            match type_kind(context, piece.ty) {
                ValueKind::Int => integers += 1,
                ValueKind::Float => floats += 1,
                ValueKind::Vector => return false,
            }
        }
        self.integers + integers <= target.argument_registers(ValueKind::Int)
            && self.floats + floats <= target.argument_registers(ValueKind::Float)
    }

    fn consume(&mut self, context: &Context, target: TargetProfile, pieces: &[AbiPiece]) {
        let integer_limit = target.argument_registers(ValueKind::Int);
        let float_limit = target.argument_registers(ValueKind::Float);
        for piece in pieces {
            match type_kind(context, piece.ty) {
                ValueKind::Float if self.floats < float_limit => {
                    self.floats += 1;
                }
                ValueKind::Float
                    if target.float_argument_overflow() == Overflow::Chain(ValueKind::Int)
                        && self.integers < integer_limit =>
                {
                    self.integers += 1;
                }
                ValueKind::Int if self.integers < integer_limit => {
                    self.integers += 1;
                }
                _ => {}
            }
        }
    }

    fn consume_group(&mut self, context: &Context, target: TargetProfile, pieces: &[AbiPiece]) {
        if self.has_direct_registers(context, target, pieces) {
            self.consume(context, target, pieces);
        } else {
            for piece in pieces {
                match type_kind(context, piece.ty) {
                    ValueKind::Int => {
                        self.integers = target.argument_registers(ValueKind::Int);
                    }
                    ValueKind::Float => {
                        self.floats = target.argument_registers(ValueKind::Float);
                    }
                    ValueKind::Vector => {}
                }
            }
        }
    }
}

impl Signature {
    pub(super) fn argument_types(&self, context: &Context) -> Vec<TypeId> {
        let mut args = Vec::new();
        if self.ret.indirect {
            args.push(PtrType::opaque(context));
        }
        for parameter in &self.params {
            if parameter.grouped {
                args.push(TupleType::new(
                    context,
                    parameter.pieces.iter().map(|piece| piece.ty).collect(),
                ));
            } else {
                args.extend(parameter.pieces.iter().map(|piece| piece.ty));
            }
        }
        if self.varargs {
            args.push(VarArgsType::new(context));
        }
        args
    }

    /// The indices of the arguments a `restrict` parameter became, matching
    /// [`Signature::argument_types`] slot for slot.
    pub(super) fn noalias_arguments(&self) -> Vec<usize> {
        let mut arguments = Vec::new();
        let mut index = usize::from(self.ret.indirect);
        for parameter in &self.params {
            if parameter.noalias {
                arguments.push(index);
            }
            index += if parameter.grouped {
                1
            } else {
                parameter.pieces.len()
            };
        }
        arguments
    }

    pub(super) fn argument_alignments(&self) -> Vec<u64> {
        let mut alignments = Vec::new();
        if self.ret.indirect {
            alignments.push(1);
        }
        for parameter in &self.params {
            if parameter.grouped {
                alignments.push(parameter.alignment);
            } else {
                alignments.extend(std::iter::repeat_n(1, parameter.pieces.len()));
            }
        }
        alignments
    }
}

/// Lower a translation unit into a `builtin.module` in `context`.
pub(super) fn lower_signature(
    context: &Context,
    typed: &TypedAst,
    item: NodeId,
) -> Result<(EntityId, Signature), Diagnostic> {
    let ast = typed.ast();
    let AstLeaf::Function { .. } = ast.get_leaf_data(item).unwrap() else {
        unreachable!("function-like node carries a function payload");
    };
    Ok((
        node_entity(typed, item),
        classify_function_type(context, typed, node_type(typed, item)),
    ))
}

pub(super) fn classify_function_type(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Signature {
    let TypeKind::Function {
        ret,
        params: source_params,
        varargs,
        prototype,
        ..
    } = typed.types().kind(ty)
    else {
        unreachable!("function signature has function semantic type")
    };
    let ret = classify_abi_return(context, typed, *ret);
    let mut register_usage = AbiRegisterUsage::default();
    if ret.indirect {
        register_usage.reserve_indirect_result(typed.target());
    }
    let params = source_params
        .iter()
        .map(|&param| classify_abi_parameter(context, typed, param, &mut register_usage))
        .collect();
    Signature {
        ret,
        params,
        varargs: *varargs || !prototype,
    }
}

fn classify_abi_return(context: &Context, typed: &TypedAst, ty: QualType) -> AbiReturn {
    if let Some(pieces) = classify_sysv_eightbytes(context, typed, ty)
        .or_else(|| classify_riscv_fp_aggregate(context, typed, ty))
        .or_else(|| classify_aapcs64_hfa(context, typed, ty))
        .or_else(|| classify_aapcs64_composite(context, typed, ty))
        .or_else(|| {
            typed
                .target()
                .uses_riscv_abi()
                .then(|| classify_integer_carriers(context, typed, ty))
                .flatten()
        })
        .or_else(|| classify_integer_aggregate(context, typed, ty))
    {
        let ty = match pieces.as_slice() {
            [piece] => piece.ty,
            pieces => TupleType::new(context, pieces.iter().map(|piece| piece.ty).collect()),
        };
        return AbiReturn {
            ty,
            aggregate: Some(pieces),
            indirect: false,
        };
    }
    let record = matches!(typed.types().kind(ty), TypeKind::Record(_));
    if typed.target().uses_sysv_abi() && record {
        return AbiReturn {
            ty: PtrType::opaque(context),
            aggregate: None,
            indirect: true,
        };
    }
    if (typed.target().uses_aapcs64_abi() || typed.target().uses_riscv_abi())
        && record
        && source_type_layout(typed, ty).0 > 16
    {
        return AbiReturn {
            ty: UnitType::new(context),
            aggregate: None,
            indirect: true,
        };
    }
    AbiReturn {
        ty: lower_type(context, typed, ty),
        aggregate: None,
        indirect: false,
    }
}

fn classify_abi_parameter(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
    register_usage: &mut AbiRegisterUsage,
) -> AbiParameter {
    let sysv_pieces = classify_sysv_eightbytes(context, typed, ty);
    let riscv_pieces = classify_riscv_fp_aggregate(context, typed, ty);
    let hfa_pieces = classify_aapcs64_hfa(context, typed, ty);
    if sysv_pieces.is_none()
        && let Some(pieces) = classify_sysv_memory_carriers(context, typed, ty)
    {
        register_usage.consume_group(context, typed.target(), &pieces);
        return AbiParameter {
            pieces,
            grouped: true,
            indirect: false,
            alignment: 1,
            noalias: false,
        };
    }
    if riscv_pieces.is_none()
        && hfa_pieces.is_none()
        && (typed.target().uses_aapcs64_abi() || typed.target().uses_riscv_abi())
        && matches!(typed.types().kind(ty), TypeKind::Record(_))
        && source_type_layout(typed, ty).0 > 16
    {
        let pieces = vec![AbiPiece {
            offset: 0,
            ty: PtrType::opaque(context),
        }];
        register_usage.consume(context, typed.target(), &pieces);
        return AbiParameter {
            pieces,
            grouped: false,
            indirect: true,
            alignment: 1,
            noalias: false,
        };
    }
    let composite_pieces = hfa_pieces
        .is_none()
        .then(|| classify_aapcs64_composite(context, typed, ty))
        .flatten();
    let (pieces, grouped) = match sysv_pieces {
        Some(pieces) => {
            let grouped = pieces.len() > 1;
            (Some(pieces), grouped)
        }
        None => match riscv_pieces {
            Some(pieces)
                if register_usage.has_direct_registers(context, typed.target(), &pieces) =>
            {
                (Some(pieces), false)
            }
            Some(_) => (classify_integer_carriers(context, typed, ty), false),
            None => match hfa_pieces {
                Some(pieces) => {
                    let grouped = pieces.len() > 1;
                    (Some(pieces), grouped)
                }
                None => match composite_pieces {
                    Some(pieces) => {
                        let grouped = pieces.len() > 1;
                        (Some(pieces), grouped)
                    }
                    None if typed.target().uses_riscv_abi() => {
                        (classify_integer_carriers(context, typed, ty), false)
                    }
                    None => (classify_integer_aggregate(context, typed, ty), false),
                },
            },
        },
    };
    let pieces = pieces.unwrap_or_else(|| {
        vec![AbiPiece {
            offset: 0,
            ty: lower_type(context, typed, ty),
        }]
    });
    let source_alignment = source_type_layout(typed, ty).1;
    let alignment = if grouped
        && pieces.iter().any(|piece| {
            let kind = type_kind(context, piece.ty);
            typed
                .target()
                .align_argument_slot(kind, source_alignment, 1)
                != 1
        }) {
        source_alignment
    } else {
        1
    };
    if grouped {
        register_usage.align_group(context, typed.target(), alignment, &pieces);
        register_usage.consume_group(context, typed.target(), &pieces);
    } else {
        register_usage.consume(context, typed.target(), &pieces);
    }
    AbiParameter {
        pieces,
        grouped,
        indirect: false,
        alignment,
        // Only a pointer can be qualified `restrict`, and a pointer is one
        // ungrouped piece, so the guarantee lands on one argument.
        noalias: ty.qualifiers.is_restrict(),
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum SysvClass {
    #[default]
    None,
    Integer,
    Sse,
}

impl SysvClass {
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (class, other) if class == other => class,
            (Self::None, class) | (class, Self::None) => class,
            (Self::Integer, _) | (_, Self::Integer) => Self::Integer,
            _ => Self::Sse,
        }
    }
}

fn classify_sysv_eightbytes(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_sysv_abi() || !matches!(typed.types().kind(ty), TypeKind::Record(_)) {
        return None;
    }
    let (size, _) = source_type_layout(typed, ty);
    if !(1..=16).contains(&size) {
        return None;
    }
    let mut classes = vec![SysvClass::None; size.div_ceil(8) as usize];
    classify_sysv_fields(typed, ty, 0, &mut classes)?;
    Some(
        classes
            .into_iter()
            .enumerate()
            .filter_map(|(index, class)| {
                let ty = match class {
                    SysvClass::None => return None,
                    SysvClass::Integer => IntegerType::new(context, 64),
                    SysvClass::Sse => FloatType::f64(context),
                };
                Some(AbiPiece {
                    offset: index as u64 * 8,
                    ty,
                })
            })
            .collect(),
    )
}

fn classify_sysv_memory_carriers(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_sysv_abi() || !matches!(typed.types().kind(ty), TypeKind::Record(_)) {
        return None;
    }
    let (size, _) = source_type_layout(typed, ty);
    if size == 0 {
        return None;
    }
    let carrier = IntegerType::new(context, 64);
    Some(
        (0..size.div_ceil(8))
            .map(|index| AbiPiece {
                offset: index * 8,
                ty: carrier,
            })
            .collect(),
    )
}

fn classify_sysv_fields(
    typed: &TypedAst,
    ty: QualType,
    offset: u64,
    classes: &mut [SysvClass],
) -> Option<()> {
    let scalar_class = match typed.types().kind(ty) {
        TypeKind::Integer(_) | TypeKind::Enum(_) | TypeKind::Pointer(_) => Some(SysvClass::Integer),
        TypeKind::Float | TypeKind::Double => Some(SysvClass::Sse),
        _ => None,
    };
    if let Some(class) = scalar_class {
        let (size, align) = source_type_layout(typed, ty);
        if !offset.is_multiple_of(align) {
            return None;
        }
        let first = usize::try_from(offset / 8).ok()?;
        let last = usize::try_from((offset + size - 1) / 8).ok()?;
        for slot in classes.get_mut(first..=last)? {
            *slot = slot.merge(class);
        }
        return Some(());
    }

    match typed.types().kind(ty) {
        TypeKind::Record(id) => {
            let record = typed.record(*id)?;
            for field in &record.fields {
                let (_, align) = source_type_layout(typed, field.ty);
                let field_offset = offset + field.offset;
                if !field_offset.is_multiple_of(align) {
                    return None;
                }
                classify_sysv_fields(typed, field.ty, field_offset, classes)?;
            }
            Some(())
        }
        TypeKind::Array(element, Some(length)) => {
            let stride = source_type_layout(typed, *element).0;
            for index in 0..*length {
                classify_sysv_fields(typed, *element, offset + index * stride, classes)?;
            }
            Some(())
        }
        _ => None,
    }
}

fn classify_riscv_fp_aggregate(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_riscv_hard_float_abi() {
        return None;
    }
    let TypeKind::Record(id) = typed.types().kind(ty) else {
        return None;
    };
    let record = typed.record(*id)?;
    if record.kind != RecordKind::Struct {
        return None;
    }
    let mut pieces = vec![];
    if !flatten_aggregate_fields(context, typed, ty, 0, &mut pieces) {
        return None;
    }
    let kinds = pieces
        .iter()
        .map(|piece| type_kind(context, piece.ty))
        .collect::<Vec<_>>();
    use ValueKind::{Float, Int};
    if !matches!(
        kinds.as_slice(),
        [Float] | [Float, Float] | [Float, Int] | [Int, Float]
    ) {
        return None;
    }
    Some(pieces)
}

fn classify_aapcs64_hfa(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_aapcs64_abi() {
        return None;
    }
    let TypeKind::Record(id) = typed.types().kind(ty) else {
        return None;
    };
    if typed.record(*id)?.kind != RecordKind::Struct {
        return None;
    }
    let mut pieces = vec![];
    if !flatten_aggregate_fields(context, typed, ty, 0, &mut pieces)
        || !(1..=4).contains(&pieces.len())
        || pieces
            .iter()
            .any(|piece| type_kind(context, piece.ty) != ValueKind::Float)
        || pieces
            .iter()
            .any(|piece| piece.ty != pieces.first().unwrap().ty)
        || source_type_layout(typed, ty).0
            != pieces.len() as u64 * abi_piece_size(context, pieces[0].ty).unwrap()
    {
        return None;
    }
    Some(pieces)
}

fn classify_aapcs64_composite(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !typed.target().uses_aapcs64_abi()
        || !matches!(typed.types().kind(ty), TypeKind::Record(_))
        || !(1..=16).contains(&source_type_layout(typed, ty).0)
    {
        return None;
    }
    let size = source_type_layout(typed, ty).0;
    let carrier = IntegerType::new(context, 64);
    Some(
        (0..size.div_ceil(8))
            .map(|index| AbiPiece {
                offset: index * 8,
                ty: carrier,
            })
            .collect(),
    )
}

fn flatten_aggregate_fields(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
    offset: u64,
    pieces: &mut Vec<AbiPiece>,
) -> bool {
    match typed.types().kind(ty) {
        TypeKind::Float | TypeKind::Double => {
            pieces.push(AbiPiece {
                offset,
                ty: lower_type(context, typed, ty),
            });
            true
        }
        TypeKind::Integer(_) => {
            let Some(width) = typed.integer_width(ty) else {
                return false;
            };
            if width > typed.target().pointer_width() {
                return false;
            }
            pieces.push(AbiPiece {
                offset,
                ty: lower_type(context, typed, ty),
            });
            true
        }
        TypeKind::Enum(_) => {
            pieces.push(AbiPiece {
                offset,
                ty: IntegerType::new(context, 32),
            });
            true
        }
        TypeKind::Record(id) => {
            let Some(record) = typed.record(*id) else {
                return false;
            };
            record.kind == RecordKind::Struct
                && record.fields.iter().all(|field| {
                    flatten_aggregate_fields(
                        context,
                        typed,
                        field.ty,
                        offset + field.offset,
                        pieces,
                    )
                })
        }
        TypeKind::Array(element, Some(length)) => {
            let stride = source_type_layout(typed, *element).0;
            (0..*length).all(|index| {
                flatten_aggregate_fields(context, typed, *element, offset + index * stride, pieces)
            })
        }
        _ => false,
    }
}

fn classify_integer_aggregate(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !matches!(typed.types().kind(ty), TypeKind::Record(_)) || !is_integer_aggregate(typed, ty) {
        return None;
    }
    classify_integer_carriers(context, typed, ty)
}

fn classify_integer_carriers(
    context: &Context,
    typed: &TypedAst,
    ty: QualType,
) -> Option<Vec<AbiPiece>> {
    if !matches!(typed.types().kind(ty), TypeKind::Record(_)) {
        return None;
    }
    let (size, _) = source_type_layout(typed, ty);
    let scalar_width = u64::from(typed.target().pointer_width() / 8);
    if size <= scalar_width && size.is_power_of_two() {
        return Some(vec![AbiPiece {
            offset: 0,
            ty: IntegerType::new(context, (size * 8) as u32),
        }]);
    }
    if size == scalar_width * 2 {
        let carrier = IntegerType::new(context, typed.target().pointer_width());
        return Some(vec![
            AbiPiece {
                offset: 0,
                ty: carrier,
            },
            AbiPiece {
                offset: scalar_width,
                ty: carrier,
            },
        ]);
    }
    None
}

fn abi_piece_size(context: &Context, ty: TypeId) -> Option<u64> {
    let ty = context.get_type_data(ty);
    let ty = ty.as_ref() as &dyn std::any::Any;
    if let Some(integer) = ty.downcast_ref::<IntegerType>() {
        return Some(u64::from(integer.width().div_ceil(8)));
    }
    if let Some(float) = ty.downcast_ref::<FloatType>() {
        return Some(u64::from(float.bit_width() / 8));
    }
    None
}

pub(super) fn abi_storage_layout(context: &Context, pieces: &[AbiPiece]) -> Option<(u64, u64)> {
    pieces.iter().try_fold((0, 1), |(size, align), piece| {
        let piece_size = abi_piece_size(context, piece.ty)?;
        Some((size.max(piece.offset + piece_size), align.max(piece_size)))
    })
}

fn is_integer_aggregate(typed: &TypedAst, ty: QualType) -> bool {
    match typed.types().kind(ty) {
        TypeKind::Integer(_) | TypeKind::Enum(_) | TypeKind::Pointer(_) => true,
        TypeKind::Array(element, _) => is_integer_aggregate(typed, *element),
        TypeKind::Record(id) => typed.record(*id).is_some_and(|record| {
            record
                .fields
                .iter()
                .all(|field| is_integer_aggregate(typed, field.ty))
        }),
        _ => false,
    }
}
