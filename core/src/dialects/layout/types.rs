//! The definitions of the type constructors whose layouts the IR computes,
//! `!struct`, `!union`, `!variant` and `!array`, built with the constructors a
//! frontend defines its own types with. Sizes and alignments are in bits.

use crate::attributes::{AttributeValue, Predicate};
use crate::builtin::{
    AddIOp, AggregateKind, AggregateType, CmpIOp, ConstantOp, DivUIOp, IntegerType, ModuleEndOp,
    ModuleOp, MulIOp, SubIOp, TypeDefOp,
};
use crate::scf::{OrderedForOp, SwitchOp, YieldOp};
use crate::ty::{Ctor, TypeArg};
use crate::{BlockHandle, Context, Operation, RegionId, TypeId, ValueId};

use super::{AlignOfOp, AtOp, CountOp, FIELDS, FieldOp, ResultOp, SPEC, SizeOfOp, TYPE};

type Param = (&'static str, &'static str, Option<AttributeValue>);

/// The names the definitions define.
pub(crate) const NAMES: [&str; 4] = ["struct", "variant", "union", "array"];

// Each definition's parameters, in the order of `NAMES`.
fn params(context: &Context) -> [Vec<Param>; 4] {
    let i32 = IntegerType::new(context, 32);
    let field_list: Param = ("F", "fields", None);
    let packed: Param = ("packed", "bool", Some(AttributeValue::Bool(false)));
    let tag: Param = ("tag", "data", Some(AttributeValue::Type(i32)));
    [
        vec![field_list.clone(), packed],
        vec![field_list.clone(), tag],
        vec![field_list],
        vec![("N", "nat", None), ("T", "data", None)],
    ]
}

pub(crate) fn defaults(context: &Context) -> Vec<(&'static str, AttributeValue)> {
    let last = |params: Vec<Param>| params.last()?.2.clone();
    NAMES
        .into_iter()
        .zip(params(context))
        .filter_map(|(name, params)| Some((name, last(params)?)))
        .collect()
}

// A module of the definitions, in `context`.
pub(crate) fn define(context: &Context) -> ModuleOp {
    let unit = |ctor: &'static Ctor| context.terms().intern(ctor.id(context), vec![]).unwrap();
    let (spec, fields, ty) = (unit(&SPEC), unit(&FIELDS), unit(&TYPE));
    let (flag, i64) = (IntegerType::new(context, 1), IntegerType::new(context, 64));

    let [struct_params, variant_params, union_params, array_params] = params(context);
    let struct_region = layout_region(context, [spec, fields, flag], struct_layout);
    let variant_region = layout_region(context, [spec, fields, ty], variant_layout);
    let array_region = layout_region(context, [spec, i64, ty], array_layout);
    let module = ModuleOp::new(
        context,
        None,
        Default::default(),
        crate::InsertionPoint::Root,
    );
    let definitions = [
        (NAMES[0], &struct_params, vec![struct_region], None),
        (NAMES[1], &variant_params, vec![variant_region], None),
        (NAMES[2], &union_params, vec![], Some(union_body(context))),
        (NAMES[3], &array_params, vec![array_region], None),
    ];
    for (name, params, regions, body) in definitions {
        definition(
            context,
            crate::InsertionPoint::Block {
                block: module.body().id(),
                position: module.body().len(),
            },
            name,
            params,
            regions,
            body,
        );
    }
    ModuleEndOp::new(
        context,
        Default::default(),
        crate::InsertionPoint::Block {
            block: module.body().id(),
            position: module.body().len(),
        },
    );
    module
}

fn definition(
    context: &Context,
    insertion: crate::InsertionPoint,
    name: &str,
    params: &[Param],
    regions: Vec<RegionId>,
    body: Option<TypeId>,
) -> TypeDefOp {
    let params: Vec<_> = params
        .iter()
        .map(|(name, kind, default)| {
            let mut param = vec![AttributeValue::from(*name), AttributeValue::from(*kind)];
            param.extend(default.clone());
            AttributeValue::Array(param.into())
        })
        .collect();
    let mut attributes =
        vec![context.named_attribute("params", AttributeValue::Array(params.into()))];
    attributes.extend(body.map(|body| context.named_attribute("body", AttributeValue::Type(body))));
    TypeDefOp::new(
        context,
        name,
        regions,
        crate::NewOpParts { attributes },
        insertion,
    )
}

// Fields in order, each at the next offset its alignment allows; a packed
// struct aligns every field to a byte. The size rounds up to the alignment.
fn struct_layout(region: &Emit, [spec, fields, packed]: [ValueId; 3]) {
    let byte = region.constant(8);
    let count = region.count(fields);
    let start = [region.constant(0), byte];
    let [end, align] = region.for_each(count, start, |body, index, [end, align]| {
        let ty = body.at(fields, index);
        let field_align = body.select(packed, body.align_of(ty, spec), byte);
        let offset = body.align_to(end, field_align);
        body.field(index, offset);
        let next = body.add(offset, body.size_of(ty, spec));
        [next, body.max(align, field_align)]
    });
    region.result(region.align_to(end, align), align);
}

// The tag first, then every alternative at one offset past it, the size of
// the largest.
fn variant_layout(region: &Emit, [spec, fields, tag]: [ValueId; 3]) {
    let count = region.count(fields);
    let start = [region.constant(0), region.align_of(tag, spec)];
    let [largest, align] = region.for_each(count, start, |body, index, [largest, align]| {
        let ty = body.at(fields, index);
        let largest = body.max(largest, body.size_of(ty, spec));
        [largest, body.max(align, body.align_of(ty, spec))]
    });
    let payload = region.align_to(region.size_of(tag, spec), align);
    region.for_each(count, [], |body, index, []| {
        body.field(index, payload);
        []
    });
    let end = region.add(payload, largest);
    region.result(region.align_to(end, align), align);
}

// Every field at offset zero: a variant whose tag takes no room.
fn union_body(context: &Context) -> TypeId {
    let terms = context.terms();
    let variant = terms.ctor_named("type", "variant").unwrap();
    let empty = AggregateType::get(context, AggregateKind::Struct, &[]).unwrap();
    terms
        .intern(variant, vec![TypeArg::Var(0), TypeArg::Type(empty)])
        .unwrap()
}

// `N` elements of `T`, each at `T`'s alignment: a spec entry may give `T`
// a size that is no multiple of it.
fn array_layout(region: &Emit, [spec, count, element]: [ValueId; 3]) {
    let align = region.align_of(element, spec);
    let stride = region.align_to(region.size_of(element, spec), align);
    region.result(region.mul(count, stride), align);
}

fn layout_region<const N: usize>(
    context: &Context,
    arguments: [TypeId; N],
    build: impl FnOnce(&Emit, [ValueId; N]),
) -> RegionId {
    let (emit, arguments) = Emit::block(context, arguments);
    let region = context.create_region();
    region.add_block(emit.block.id());
    build(&emit, arguments);
    region.id()
}

// Appends the ops of a layout region to one of its blocks. Every number is
// an `!i64` count of bits.
struct Emit<'c> {
    context: &'c Context,
    block: BlockHandle,
    bits: TypeId,
}

impl<'c> Emit<'c> {
    fn block<const N: usize>(context: &'c Context, arguments: [TypeId; N]) -> (Self, [ValueId; N]) {
        let arguments = arguments.map(|ty| context.create_value(ty, None));
        let ids = arguments.each_ref().map(|value| value.id());
        let emit = Self {
            context,
            block: context.create_block(arguments.to_vec()),
            bits: IntegerType::new(context, 64),
        };
        (emit, ids)
    }

    fn constant(&self, value: i64) -> ValueId {
        ConstantOp::new(
            self.context,
            AttributeValue::Int(value),
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn add(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        AddIOp::new(
            self.context,
            lhs,
            rhs,
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn mul(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        MulIOp::new(
            self.context,
            lhs,
            rhs,
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn sub(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        SubIOp::new(
            self.context,
            lhs,
            rhs,
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn div(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        DivUIOp::new(
            self.context,
            lhs,
            rhs,
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    // `value` rounded up to a multiple of `align`.
    fn align_to(&self, value: ValueId, align: ValueId) -> ValueId {
        let raised = self.add(value, self.sub(align, self.constant(1)));
        self.mul(self.div(raised, align), align)
    }

    fn max(&self, a: ValueId, b: ValueId) -> ValueId {
        let greater = CmpIOp::new(
            self.context,
            a,
            b,
            Predicate::Ugt,
            IntegerType::new(self.context, 1),
            Default::default(),
            self.block.end(),
        );
        self.select(greater.result(), b, a)
    }

    // `set` where `flag` is, else `clear`: a two-armed `scf.switch`.
    fn select(&self, flag: ValueId, clear: ValueId, set: ValueId) -> ValueId {
        let arm = |chosen: usize| {
            let ports = vec![
                self.context.create_value(self.bits, None),
                self.context.create_value(self.bits, None),
            ];
            let result = ports[chosen].id();
            self.context
                .create_nodes_region(ports, vec![], vec![result])
                .id()
        };
        SwitchOp::new(
            self.context,
            flag,
            vec![clear, set],
            vec![self.bits],
            vec![arm(0), arm(1)],
            Default::default(),
            self.block.end(),
        )
        .handle()
        .value_results()[0]
    }

    // An `scf.ordered_for` over `0..count`: `body` takes the index and the
    // carried values and gives the next ones, which the loop ends with.
    fn for_each<const K: usize>(
        &self,
        count: ValueId,
        start: [ValueId; K],
        body: impl FnOnce(&Emit, ValueId, [ValueId; K]) -> [ValueId; K],
    ) -> [ValueId; K] {
        let (inner, ports) = Emit::block(self.context, [self.bits; 1]);
        let carried = start.map(|_| {
            self.context
                .append_block_argument(inner.block.id(), self.bits)
                .id()
        });
        let next = body(&inner, ports[0], carried);
        YieldOp::new(
            self.context,
            next.to_vec(),
            Default::default(),
            inner.block.end(),
        );
        let region = self.context.create_region();
        region.add_block(inner.block.id());
        let lb = self.constant(0);
        let step = self.constant(1);
        let looped = OrderedForOp::new(
            self.context,
            lb,
            start.to_vec(),
            count,
            step,
            vec![self.bits; K + 1],
            Some(region.id()),
            Default::default(),
            self.block.end(),
        );
        let results = looped.handle().value_results();
        std::array::from_fn(|index| results[index + 1])
    }

    fn count(&self, fields: ValueId) -> ValueId {
        CountOp::new(
            self.context,
            fields,
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn at(&self, fields: ValueId, index: ValueId) -> ValueId {
        let field_type = self
            .context
            .terms()
            .intern(TYPE.id(self.context), vec![])
            .unwrap();
        AtOp::new(
            self.context,
            fields,
            index,
            field_type,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn size_of(&self, ty: ValueId, spec: ValueId) -> ValueId {
        SizeOfOp::new(
            self.context,
            vec![ty, spec],
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn align_of(&self, ty: ValueId, spec: ValueId) -> ValueId {
        AlignOfOp::new(
            self.context,
            vec![ty, spec],
            self.bits,
            Default::default(),
            self.block.end(),
        )
        .result()
    }

    fn field(&self, index: ValueId, offset: ValueId) {
        FieldOp::new(
            self.context,
            index,
            offset,
            Default::default(),
            self.block.end(),
        );
    }

    fn result(&self, size: ValueId, align: ValueId) {
        ResultOp::new(
            self.context,
            size,
            align,
            Default::default(),
            self.block.end(),
        );
    }
}
