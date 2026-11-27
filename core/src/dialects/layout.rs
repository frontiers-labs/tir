//! The `layout` dialect: the layout spec a scope carries and the ops that ask
//! for querying layouts.
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
//! its layout. The most specific key matching a type wins. A named type with
//! no matching key takes its layout from its body.
//!
//! `layout.sizeof`, `layout.alignof` and `layout.offsetof` query in bits.

use std::sync::Arc;

use tir_symbolic::term::{ctor, no_carrier, valid};

use crate::attributes::AttributeValue;
use crate::parse::Span;
use crate::parse::common::Cursor;
use crate::parse::text::Parser;
use crate::ty::{Ctor, Kind, Param, Terms, TypeArg, TypeCtor};
use crate::{Context, Error, IRFormatter, TypeId, dialect};

use crate as tir;

pub mod ops {
    pub use super::{AlignOfOp, OffsetOfOp, SizeOfOp};
}

dialect! {
    LayoutDialect {
        name: "layout",
        operations: [SizeOfOp, AlignOfOp, OffsetOfOp],
        types: [Ctor(&ALIGN_TO), Ctor(&MIN), Ctor(&MAX), Ctor(&POW2_CEIL)],
    }
}

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
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
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
    pub(crate) options: Vec<(String, AttributeValue)>,
    pub(crate) entries: Vec<TypeEntry>,
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

// The size, in bits, of a type under the spec in scope.
__cdsl_layout_SizeOfOp!();

// The ABI alignment, in bits, of a type under the spec in scope.
__cdsl_layout_AlignOfOp!();

// The offset, in bits, of field `field` of `ty` under the spec in scope:
// `layout.offsetof {ty = !Node, field = "next"} : !i64`.
__cdsl_layout_OffsetOfOp!();

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
