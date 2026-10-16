//! The definitions of the type constructors whose layouts the IR computes,
//! `!struct`, `!union`, `!variant` and `!array`, built with the builders a
//! frontend defines its own types with. Sizes and alignments are in bits.

use crate::attributes::{AttributeValue, Predicate};
use crate::builtin::{
    AddIOpBuilder, AggregateKind, AggregateType, CmpIOpBuilder, ConstantOpBuilder, DivUIOpBuilder,
    IntegerType, ModuleEndOpBuilder, ModuleOp, ModuleOpBuilder, MulIOpBuilder, SubIOpBuilder,
    TypeDefOpBuilder,
};
use crate::scf::{OrderedForOpBuilder, SwitchOpBuilder, YieldOpBuilder};
use crate::ty::{Ctor, TypeArg};
use crate::{BlockHandle, Context, Operation, RegionId, TypeId, ValueId};

use super::{
    AlignOfOpBuilder, AtOpBuilder, CountOpBuilder, FIELDS, FieldOpBuilder, ResultOpBuilder, SPEC,
    SizeOfOpBuilder, TYPE,
};

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

/// Each definition's default for its omitted last parameter, by name: what
/// every term of its constructor interns under, known before the
/// definitions are built.
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
    let definitions = [
        definition(context, NAMES[0], &struct_params).region(vec![struct_region]),
        definition(context, NAMES[1], &variant_params).region(vec![variant_region]),
        definition(context, NAMES[2], &union_params).body(union_body(context)),
        definition(context, NAMES[3], &array_params).region(vec![array_region]),
    ];
    let module = ModuleOpBuilder::new(context).build();
    for definition in definitions {
        module.body().append_op(definition.build());
    }
    module
        .body()
        .append_op(ModuleEndOpBuilder::new(context).build());
    module
}

fn definition(context: &Context, name: &str, params: &[Param]) -> TypeDefOpBuilder {
    let params: Vec<_> = params
        .iter()
        .map(|(name, kind, default)| {
            let mut param = vec![AttributeValue::from(*name), AttributeValue::from(*kind)];
            param.extend(default.clone());
            AttributeValue::Array(param.into())
        })
        .collect();
    TypeDefOpBuilder::new(context)
        .sym_name(name)
        .attr("params", AttributeValue::Array(params.into()))
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

    fn append<T: crate::Operation>(&self, op: T) -> T {
        self.block.append_op(op)
    }

    fn constant(&self, value: i64) -> ValueId {
        let op = ConstantOpBuilder::new(self.context)
            .attr("value", AttributeValue::Int(value))
            .result_type(self.bits);
        self.append(op.build()).result()
    }

    fn add(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let op = AddIOpBuilder::new(self.context).lhs(lhs).rhs(rhs);
        self.append(op.result_type(self.bits).build()).result()
    }

    fn mul(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let op = MulIOpBuilder::new(self.context).lhs(lhs).rhs(rhs);
        self.append(op.result_type(self.bits).build()).result()
    }

    fn sub(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let op = SubIOpBuilder::new(self.context).lhs(lhs).rhs(rhs);
        self.append(op.result_type(self.bits).build()).result()
    }

    fn div(&self, lhs: ValueId, rhs: ValueId) -> ValueId {
        let op = DivUIOpBuilder::new(self.context).lhs(lhs).rhs(rhs);
        self.append(op.result_type(self.bits).build()).result()
    }

    // `value` rounded up to a multiple of `align`.
    fn align_to(&self, value: ValueId, align: ValueId) -> ValueId {
        let raised = self.add(value, self.sub(align, self.constant(1)));
        self.mul(self.div(raised, align), align)
    }

    fn max(&self, a: ValueId, b: ValueId) -> ValueId {
        let greater = CmpIOpBuilder::new(self.context)
            .lhs(a)
            .rhs(b)
            .predicate(Predicate::Ugt)
            .result_type(IntegerType::new(self.context, 1));
        self.select(self.append(greater.build()).result(), b, a)
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
        let switch = SwitchOpBuilder::new(self.context)
            .predicate(flag)
            .inputs(vec![clear, set])
            .arms(vec![arm(0), arm(1)])
            .result_types(vec![self.bits]);
        self.append(switch.build()).handle().value_results()[0]
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
        inner.append(
            YieldOpBuilder::new(self.context)
                .values(next.to_vec())
                .build(),
        );
        let region = self.context.create_region();
        region.add_block(inner.block.id());
        let looped = OrderedForOpBuilder::new(self.context)
            .lb(self.constant(0))
            .inits(start.to_vec())
            .ub(count)
            .step(self.constant(1))
            .body(region.id())
            .result_types(vec![self.bits; K + 1]);
        let results = self.append(looped.build()).handle().value_results();
        std::array::from_fn(|index| results[index + 1])
    }

    fn count(&self, fields: ValueId) -> ValueId {
        let op = CountOpBuilder::new(self.context).fields(fields);
        self.append(op.result_type(self.bits).build()).result()
    }

    fn at(&self, fields: ValueId, index: ValueId) -> ValueId {
        let field_type = self
            .context
            .terms()
            .intern(TYPE.id(self.context), vec![])
            .unwrap();
        let op = AtOpBuilder::new(self.context).fields(fields).index(index);
        self.append(op.result_type(field_type).build()).result()
    }

    fn size_of(&self, ty: ValueId, spec: ValueId) -> ValueId {
        let op = SizeOfOpBuilder::new(self.context).args(vec![ty, spec]);
        self.append(op.result_type(self.bits).build()).result()
    }

    fn align_of(&self, ty: ValueId, spec: ValueId) -> ValueId {
        let op = AlignOfOpBuilder::new(self.context).args(vec![ty, spec]);
        self.append(op.result_type(self.bits).build()).result()
    }

    fn field(&self, index: ValueId, offset: ValueId) {
        self.append(
            FieldOpBuilder::new(self.context)
                .index(index)
                .offset(offset)
                .build(),
        );
    }

    fn result(&self, size: ValueId, align: ValueId) {
        self.append(
            ResultOpBuilder::new(self.context)
                .size(size)
                .align(align)
                .build(),
        );
    }
}
