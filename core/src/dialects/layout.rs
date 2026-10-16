//! The `layout` dialect: the layout spec a scope carries and the ops that ask
//! or describe a layout.
//!
//! A spec is the `#layout.spec<..>` attribute. Its options set the byte order
//! and the target's widths; its entries give a type, or a pattern of types,
//! its size and alignment in bits as terms over the pattern's variables, in
//! the `!nat.*` functions:
//!
//! ```text
//! #layout.spec<endianness = "little", stack_align = 128, native = 64,
//!   !i<W> = {size = !nat.align_to<W, 8>, abi = !nat.min<!nat.pow2_ceil<W>, 64>},
//!   !f32 = {size = 32, abi = 32}, !ptr.p = {size = 64, abi = 64}>
//! ```
//!
//! A key leaves out what it does not constrain: `!ptr.p` gives every pointer
//! its layout. The most specific key matching a type wins, and a type no key
//! matches takes its layout from a definition's region or has none.
//!
//! `layout.sizeof`, `layout.alignof` and `layout.offsetof` are in bits. They
//! fold by running the definition's layout region, see [`crate::layout`].

use std::sync::Arc;

use tir_symbolic::term::{ctor, no_carrier, valid};

use crate::attributes::AttributeValue;
use crate::interp::{ExecutionState, InterpError, Value};
use crate::parse::Span;
use crate::parse::common::Cursor;
use crate::parse::text::Parser;
use crate::ty::{Ctor, Kind, Param, Terms, TypeArg, TypeCtor};
use crate::utils::APInt;
use crate::{Context, Error, IRFormatter, OpHandle, Terminator, TypeId, dialect, operation};

use crate as tir;

pub(crate) mod types;

pub mod ops {
    pub use super::{AlignOfOp, AtOp, CountOp, FieldOp, OffsetOfOp, ResultOp, SizeOfOp};
}

dialect! {
    LayoutDialect {
        name: "layout",
        operations: [SizeOfOp, AlignOfOp, OffsetOfOp, FieldOp, ResultOp, CountOp, AtOp],
        types: [
            Ctor(&SPEC), Ctor(&TYPE), Ctor(&FIELDS),
            Ctor(&ALIGN_TO), Ctor(&MIN), Ctor(&MAX), Ctor(&POW2_CEIL),
        ],
    }
}

const fn region_value(name: &'static str) -> Ctor {
    Ctor::new(TypeCtor {
        dialect: "layout",
        name,
        params: &[],
        kind: Kind::Data,
        validate: valid,
        carrier: no_carrier,
    })
}

// What a layout region's values are: the spec it reads, a type it measures,
// and the field types of an aggregate in order.
static SPEC: Ctor = region_value("spec");
static TYPE: Ctor = region_value("type");
static FIELDS: Ctor = region_value("fields");

const fn nat(name: &'static str, params: &'static [(&'static str, Param)]) -> Ctor {
    Ctor::new(TypeCtor {
        dialect: "nat",
        name,
        params,
        kind: Kind::Nat,
        validate: valid,
        carrier: no_carrier,
    })
}

const BINARY: &[(&str, Param)] = &[("a", Param::One(Kind::Nat)), ("b", Param::One(Kind::Nat))];

// The functions a spec entry computes a size or an alignment with.
static ALIGN_TO: Ctor = nat("align_to", BINARY);
static MIN: Ctor = nat("min", BINARY);
static MAX: Ctor = nat("max", BINARY);
static POW2_CEIL: Ctor = nat("pow2_ceil", &[("a", Param::One(Kind::Nat))]);

/// The size and alignments a spec entry gives the types its key matches.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeEntry {
    /// The type, or a pattern: a variable stands for any width, and an
    /// argument the key leaves out matches anything.
    pub key: TypeId,
    /// The names of the pattern's variables, by index.
    pub vars: Vec<String>,
    pub size: TypeArg,
    pub abi: TypeArg,
    pub preferred: Option<TypeArg>,
}

impl TypeEntry {
    // `{size = .., abi = .., preferred = ..}`: bit counts, or terms over the
    // key's variables.
    fn from_dict(key: TypeId, vars: Vec<String>, value: AttributeValue) -> Result<Self, String> {
        let AttributeValue::Dict(fields) = value else {
            return Err("a layout entry is {size = .., abi = ..}".into());
        };
        let mut terms = [None, None, None];
        for (field, value) in *fields {
            let slot = match field.as_str() {
                "size" => 0,
                "abi" => 1,
                "preferred" => 2,
                _ => {
                    return Err(format!(
                        "unknown layout field '{field}' (expected size, abi, preferred)"
                    ));
                }
            };
            terms[slot] = Some(match value {
                AttributeValue::Type(term) => TypeArg::Type(term),
                value => TypeArg::Nat(
                    value
                        .as_int()
                        .and_then(|bits| u64::try_from(bits).ok())
                        .ok_or_else(|| format!("layout field '{field}' is not a bit count"))?,
                ),
            });
        }
        let [Some(size), Some(abi), preferred] = terms else {
            return Err("a layout entry needs a size and an abi alignment".into());
        };
        Ok(Self {
            key,
            vars,
            size,
            abi,
            preferred,
        })
    }
}

/// The value of a `#layout.spec<..>` attribute.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayoutSpec {
    options: Vec<(String, AttributeValue)>,
    entries: Vec<TypeEntry>,
}

impl LayoutSpec {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the option `key`: `endianness`, `stack_align`, `native`, `index`,
    /// `cache_line`, or one a dialect defines.
    pub fn with_option(mut self, key: &str, value: AttributeValue) -> Self {
        self.set_option(key, value);
        self
    }

    /// Give `ty` a size and an ABI alignment, in bits.
    pub fn with_type(mut self, ty: TypeId, size: u64, abi: u64) -> Self {
        self.set_entry(TypeEntry {
            key: ty,
            vars: vec![],
            size: TypeArg::Nat(size),
            abi: TypeArg::Nat(abi),
            preferred: None,
        });
        self
    }

    /// Add `entry`, replacing the one with the same key.
    pub fn with_entry(mut self, entry: TypeEntry) -> Self {
        self.set_entry(entry);
        self
    }

    pub fn option(&self, key: &str) -> Option<&AttributeValue> {
        self.options
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    fn set_option(&mut self, key: &str, value: AttributeValue) {
        match self.options.iter_mut().find(|(name, _)| name == key) {
            Some((_, slot)) => *slot = value,
            None => self.options.push((key.to_string(), value)),
        }
    }

    fn set_entry(&mut self, entry: TypeEntry) {
        match self.entries.iter_mut().find(|old| old.key == entry.key) {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
    }

    /// Lay `inner`, a spec of a nested scope, over this one: its options and
    /// entries replace the ones with the same key.
    pub fn merge(&mut self, inner: &LayoutSpec) {
        for (key, value) in &inner.options {
            self.set_option(key, value.clone());
        }
        for entry in &inner.entries {
            self.set_entry(entry.clone());
        }
    }

    /// This spec with `f` applied to every entry's key and every type an
    /// option holds.
    pub fn map_types(&self, f: &mut dyn FnMut(TypeId) -> TypeId) -> Self {
        Self {
            options: self
                .options
                .iter()
                .map(|(key, value)| (key.clone(), value.clone().map_types(f)))
                .collect(),
            entries: self
                .entries
                .iter()
                .map(|entry| TypeEntry {
                    key: f(entry.key),
                    ..entry.clone()
                })
                .collect(),
        }
    }

    /// The most specific entry matching `ty`, with the widths its variables
    /// stand for.
    pub fn entry_for(&self, terms: &Terms, ty: TypeId) -> Option<(&TypeEntry, Vec<u64>)> {
        let mut best: Option<((bool, usize), &TypeEntry, Vec<u64>)> = None;
        for entry in &self.entries {
            let mut bound = vec![None; entry.vars.len()];
            if !matches(
                terms,
                &TypeArg::Type(entry.key),
                &TypeArg::Type(ty),
                &mut bound,
            ) {
                continue;
            }
            let Some(bound) = bound.into_iter().collect::<Option<Vec<_>>>() else {
                continue;
            };
            let rank = (entry.vars.is_empty(), terms.term(entry.key).args().len());
            if best.as_ref().is_none_or(|(best, ..)| rank >= *best) {
                best = Some((rank, entry, bound));
            }
        }
        best.map(|(_, entry, bound)| (entry, bound))
    }

    /// `ty`'s size, ABI and preferred alignment in bits, by its entry.
    pub fn entry_layout(&self, terms: &Terms, ty: TypeId) -> Option<(u64, u64, u64)> {
        let (entry, widths) = self.entry_for(terms, ty)?;
        let size = eval(terms, &entry.size, &widths)?;
        let abi = eval(terms, &entry.abi, &widths)?;
        let preferred = match &entry.preferred {
            Some(preferred) => eval(terms, preferred, &widths)?,
            None => abi,
        };
        Some((size, abi, preferred))
    }

    // `<option = value, !key = {size = .., abi = .., preferred = ..}, ..>`. A
    // key is a type in the generic codec; a name it leaves unbound is a
    // variable its entry's terms read.
    pub(crate) fn parse(parser: &mut Parser<'_>, context: &Context) -> Result<Self, (Span, Error)> {
        let mut spec = LayoutSpec::new();
        let items = parser.parse_delimited("<", ">", |parser| {
            let key = match parser.peek_char() {
                Some('!') => {
                    let enclosing = std::mem::take(&mut parser.type_params);
                    parser.open_type_params = true;
                    let key = parser.parse_type(context);
                    parser.open_type_params = false;
                    let vars = std::mem::replace(&mut parser.type_params, enclosing);
                    Ok((
                        key?.ok_or_else(|| (parser.span(), Error::ExpectedType))?,
                        vars,
                    ))
                }
                _ => Err(parser
                    .parse_ident()
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("a layout spec key")))?
                    .to_string()),
            };
            if !parser.parse_token("=") {
                return Err((parser.span(), Error::ExpectedToken("=")));
            }
            let vars = key
                .as_ref()
                .map(|(_, vars)| vars.clone())
                .unwrap_or_default();
            let enclosing = std::mem::replace(&mut parser.type_params, vars);
            let value = parser.parse_attribute_value(context);
            parser.type_params = enclosing;
            let value = value?.ok_or_else(|| (parser.span(), Error::ExpectedToken("attribute")))?;
            match key {
                Ok((key, vars)) => TypeEntry::from_dict(key, vars, value)
                    .map(Ok)
                    .map_err(|message| (parser.span(), Error::VerificationError(message))),
                Err(option) => Ok(Err((option, value))),
            }
        })?;
        let items = items.ok_or_else(|| (parser.span(), Error::ExpectedToken("<")))?;
        for item in items {
            match item {
                Ok(entry) => spec.set_entry(entry),
                Err((key, value)) => spec.set_option(&key, value),
            }
        }
        Ok(spec)
    }

    pub(crate) fn print(&self, fmt: &mut IRFormatter, context: &Context) -> std::fmt::Result {
        fmt.write("#layout.spec<")?;
        let mut first = true;
        let mut separate = |fmt: &mut IRFormatter| match std::mem::replace(&mut first, false) {
            true => Ok(()),
            false => fmt.write(", "),
        };
        for (key, value) in &self.options {
            separate(fmt)?;
            fmt.write(format!("{key} = "))?;
            value.print(fmt, context)?;
        }
        for entry in &self.entries {
            separate(fmt)?;
            let text = |arg: &TypeArg| context.type_arg_to_string(arg, &entry.vars);
            fmt.write(text(&TypeArg::Type(entry.key)))?;
            fmt.write(format!(
                " = {{size = {}, abi = {}",
                text(&entry.size),
                text(&entry.abi)
            ))?;
            if let Some(preferred) = &entry.preferred {
                fmt.write(format!(", preferred = {}", text(preferred)))?;
            }
            fmt.write("}")?;
        }
        fmt.write(">")
    }
}

fn matches(terms: &Terms, key: &TypeArg, ty: &TypeArg, bound: &mut [Option<u64>]) -> bool {
    match (key, ty) {
        (TypeArg::Var(index), TypeArg::Nat(width)) => {
            let slot = &mut bound[*index as usize];
            *slot.get_or_insert(*width) == *width
        }
        (TypeArg::Type(key), TypeArg::Type(ty)) if key == ty => true,
        (TypeArg::Type(key), TypeArg::Type(ty)) => {
            if terms.ctor(*key) != terms.ctor(*ty) {
                return false;
            }
            let (key, ty) = (terms.term(*key), terms.term(*ty));
            key.ctor() == ty.ctor()
                && key.args().len() <= ty.args().len()
                && key
                    .args()
                    .iter()
                    .zip(ty.args())
                    .all(|(key, ty)| matches(terms, key, ty, bound))
        }
        (TypeArg::List(keys), TypeArg::List(tys)) => {
            keys.len() == tys.len()
                && keys
                    .iter()
                    .zip(tys.iter())
                    .all(|(key, ty)| matches(terms, key, ty, bound))
        }
        (key, ty) => key == ty,
    }
}

/// The value of a width term, its variables replaced by `widths`.
pub fn eval(terms: &Terms, arg: &TypeArg, widths: &[u64]) -> Option<u64> {
    match arg {
        TypeArg::Nat(n) => Some(*n),
        TypeArg::Var(index) => widths.get(*index as usize).copied(),
        TypeArg::Type(id) => {
            let term = terms.term(*id);
            let args = match term.args() {
                [TypeArg::List(items)] if term.ctor() == ctor::NAT_ADD => items,
                args => args,
            };
            let values = args
                .iter()
                .map(|arg| eval(terms, arg, widths))
                .collect::<Option<Vec<_>>>()?;
            match (term.ctor(), values.as_slice()) {
                (ctor, values) if ctor == ctor::NAT_ADD => Some(values.iter().sum()),
                (ctor, &[a, b]) if ctor == ALIGN_TO.id(terms) => align_to(a, b),
                (ctor, &[a, b]) if ctor == MIN.id(terms) => Some(a.min(b)),
                (ctor, &[a, b]) if ctor == MAX.id(terms) => Some(a.max(b)),
                (ctor, &[a]) if ctor == POW2_CEIL.id(terms) => a.checked_next_power_of_two(),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) fn align_to(value: u64, align: u64) -> Option<u64> {
    if align == 0 {
        return Some(value);
    }
    value.div_ceil(align).checked_mul(align)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Measure {
    Size,
    Align,
}

fn int_result(op: &OpHandle, bits: u64) -> APInt {
    let width =
        crate::builtin::IntegerType::view(&op.context, op.context.get_value(op.results()[0]).ty())
            .map_or(64, |int| int.width());
    APInt::new(width, bits)
}

fn type_attr(op: &OpHandle) -> Option<TypeId> {
    match op.attr("ty")? {
        AttributeValue::Type(ty) => Some(ty),
        _ => None,
    }
}

// The size, in bits, of a type: `layout.sizeof {ty = !T} : !i64` asks the
// spec in scope; inside a layout region `layout.sizeof %t, %spec : !i64` asks
// the spec the region was handed.
operation! {
    SizeOfOp {
        name: "sizeof",
        dialect: "layout",
        scheme: "(_*) -> int<_>",
        attributes: A {
            ty: "?Type",
        },
        operands: O { args },
        results: R { result },
        interfaces: [crate::ConstantFold, crate::interp::Interp],
        sem: "(set result $folded)",
    }
}

// The ABI alignment, in bits, of a type; spelled as `layout.sizeof`.
operation! {
    AlignOfOp {
        name: "alignof",
        dialect: "layout",
        scheme: "(_*) -> int<_>",
        attributes: A {
            ty: "?Type",
        },
        operands: O { args },
        results: R { result },
        interfaces: [crate::ConstantFold, crate::interp::Interp],
        sem: "(set result $folded)",
    }
}

// The offset, in bits, of field `field` of `ty` under the spec in scope:
// `layout.offsetof {ty = !Node, field = "next"} : !i64`.
operation! {
    OffsetOfOp {
        name: "offsetof",
        dialect: "layout",
        scheme: "() -> int<_>",
        attributes: A {
            ty: "Type",
            field: "Str",
        },
        results: R { result },
        interfaces: [crate::ConstantFold, crate::interp::Interp],
        sem: "(set result $folded)",
    }
}

// Inside a layout region: field `index` sits at `offset` bits.
operation! {
    FieldOp {
        name: "field",
        dialect: "layout",
        scheme: "(int<_>, int<_>) -> ()",
        operands: O { index, offset },
        interfaces: [crate::interp::Interp],
    }
}

// Ends a layout region with the size and the alignment, in bits.
operation! {
    ResultOp {
        name: "result",
        dialect: "layout",
        scheme: "(int<_>, int<_>) -> ()",
        operands: O { size, align },
        interfaces: [Terminator],
    }
}

impl Terminator for ResultOp {}

// The number of fields of a `!layout.fields`.
operation! {
    CountOp {
        name: "count",
        dialect: "layout",
        scheme: "(layout.fields) -> int<_>",
        operands: O { fields },
        results: R { result },
        interfaces: [crate::interp::Interp],
    }
}

// The type of field `index` of a `!layout.fields`.
operation! {
    AtOp {
        name: "at",
        dialect: "layout",
        scheme: "(layout.fields, int<_>) -> layout.type",
        operands: O { fields, index },
        results: R { result },
        interfaces: [crate::interp::Interp],
    }
}

fn fold_static(op: &OpHandle, measure: Measure) -> Option<crate::sem::Value> {
    if !op.operands().is_empty() {
        return None;
    }
    let ty = type_attr(op)?;
    let layout = crate::DataLayout::for_op(&op.context, op.id)?;
    let bits = layout.measure(ty, measure)?;
    Some(crate::sem::Value::Int(int_result(op, bits)))
}

fn eval_measure(
    op: &OpHandle,
    operands: &[Value],
    state: &mut ExecutionState,
    measure: Measure,
) -> Result<Vec<Value>, InterpError> {
    let bits = match operands {
        [] => fold_static(op, measure).and_then(|value| match value {
            crate::sem::Value::Int(int) => Some(int.to_u64()),
            _ => None,
        }),
        [Value::Type(ty), Value::Layout(spec)] => {
            crate::layout::measure_in(state, spec, *ty, measure)
        }
        _ => {
            return Err(InterpError::Message(
                "layout query takes a type and a spec".into(),
            ));
        }
    };
    let bits = bits.ok_or_else(|| {
        InterpError::Message(format!("no layout for layout.{}", op.name().as_str()))
    })?;
    Ok(vec![Value::Int(int_result(op, bits))])
}

impl crate::ConstantFold for SizeOfOp {
    fn fold(&self, _: &[crate::sem::Value]) -> Option<crate::sem::Value> {
        fold_static(&self.0, Measure::Size)
    }
}

impl crate::ConstantFold for AlignOfOp {
    fn fold(&self, _: &[crate::sem::Value]) -> Option<crate::sem::Value> {
        fold_static(&self.0, Measure::Align)
    }
}

impl crate::ConstantFold for OffsetOfOp {
    fn fold(&self, _: &[crate::sem::Value]) -> Option<crate::sem::Value> {
        let layout = crate::DataLayout::for_op(&self.0.context, self.0.id)?;
        let bits = layout.offset_in_bits(self.ty(), &self.field())?;
        Some(crate::sem::Value::Int(int_result(&self.0, bits)))
    }
}

impl crate::interp::Interp for SizeOfOp {
    fn evaluate(
        &self,
        operands: &[Value],
        state: &mut ExecutionState,
    ) -> Result<Vec<Value>, InterpError> {
        eval_measure(&self.0, operands, state, Measure::Size)
    }
}

impl crate::interp::Interp for AlignOfOp {
    fn evaluate(
        &self,
        operands: &[Value],
        state: &mut ExecutionState,
    ) -> Result<Vec<Value>, InterpError> {
        eval_measure(&self.0, operands, state, Measure::Align)
    }
}

impl crate::interp::Interp for OffsetOfOp {
    fn evaluate(&self, _: &[Value], _: &mut ExecutionState) -> Result<Vec<Value>, InterpError> {
        match crate::ConstantFold::fold(self, &[]) {
            Some(crate::sem::Value::Int(int)) => Ok(vec![Value::Int(int)]),
            _ => Err(InterpError::Message(format!(
                "no offset for field '{}'",
                self.field()
            ))),
        }
    }
}

impl crate::interp::Interp for FieldOp {
    fn evaluate(
        &self,
        operands: &[Value],
        state: &mut ExecutionState,
    ) -> Result<Vec<Value>, InterpError> {
        let [Value::Int(index), Value::Int(offset)] = operands else {
            return Err(InterpError::Message(
                "layout.field takes two integers".into(),
            ));
        };
        crate::layout::record_field(state, index.to_u64(), offset.to_u64())
            .ok_or_else(|| InterpError::Message("layout.field outside a layout region".into()))?;
        Ok(vec![])
    }
}

impl crate::interp::Interp for CountOp {
    fn evaluate(
        &self,
        operands: &[Value],
        _: &mut ExecutionState,
    ) -> Result<Vec<Value>, InterpError> {
        let [Value::Tuple(fields)] = operands else {
            return Err(InterpError::Message("layout.count takes fields".into()));
        };
        Ok(vec![Value::Int(int_result(&self.0, fields.len() as u64))])
    }
}

impl crate::interp::Interp for AtOp {
    fn evaluate(
        &self,
        operands: &[Value],
        _: &mut ExecutionState,
    ) -> Result<Vec<Value>, InterpError> {
        let [Value::Tuple(fields), Value::Int(index)] = operands else {
            return Err(InterpError::Message(
                "layout.at takes fields and an index".into(),
            ));
        };
        fields
            .get(index.to_u64() as usize)
            .cloned()
            .map(|field| vec![field])
            .ok_or_else(|| InterpError::Message("layout.at past the last field".into()))
    }
}

// A folded query as the constant its semantics is, so the op is a value the
// rewriters fold; `None`, and no semantics, where it does not fold.
fn constant_node(
    value: Option<crate::sem::Value>,
    g: &mut crate::sem::SemGraph,
) -> Option<crate::NodeId> {
    let crate::sem::Value::Int(value) = value? else {
        return None;
    };
    let node = g.add_node(crate::sem::SymKind::Constant);
    g.set_leaf_data(node, crate::sem::SymPayload::Int(value));
    Some(node)
}

impl SizeOfOp {
    fn folded(&self, g: &mut crate::sem::SemGraph) -> Option<crate::NodeId> {
        constant_node(crate::ConstantFold::fold(self, &[]), g)
    }
}

impl AlignOfOp {
    fn folded(&self, g: &mut crate::sem::SemGraph) -> Option<crate::NodeId> {
        constant_node(crate::ConstantFold::fold(self, &[]), g)
    }
}

impl OffsetOfOp {
    fn folded(&self, g: &mut crate::sem::SemGraph) -> Option<crate::NodeId> {
        constant_node(crate::ConstantFold::fold(self, &[]), g)
    }
}

/// `layout.sizeof {ty = ty} : !i<width>`.
pub fn size_of(context: &Context, ty: TypeId, width: u32) -> SizeOfOpBuilder {
    SizeOfOpBuilder::new(context)
        .attr("ty", AttributeValue::Type(ty))
        .result_type(crate::builtin::IntegerType::new(context, u64::from(width)))
}

/// `layout.offsetof {ty = ty, field = field} : !i<width>`.
pub fn offset_of(context: &Context, ty: TypeId, field: &str, width: u32) -> OffsetOfOpBuilder {
    OffsetOfOpBuilder::new(context)
        .attr("ty", AttributeValue::Type(ty))
        .attr("field", AttributeValue::from(field))
        .result_type(crate::builtin::IntegerType::new(context, u64::from(width)))
}

impl From<LayoutSpec> for AttributeValue {
    fn from(spec: LayoutSpec) -> Self {
        AttributeValue::LayoutSpec(Arc::new(spec))
    }
}
