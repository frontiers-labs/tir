use std::collections::HashMap;

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::ty::{Ctor, Kind, Param, TypeArg, TypeCtor, TypeSpelling};
use tir::{Context, Error, OpHandle, TypeId, ValueId, parse::Span};
use tir_symbolic::term::{no_carrier, valid};

use crate::analysis::defuse::PhysReg;
use crate::backend::regalloc::RegClassId;

static REG: Ctor = Ctor::new(TypeCtor {
    dialect: "backend",
    name: "reg",
    params: &[
        ("target", Param::One(Kind::Sym)),
        ("class", Param::One(Kind::Sym)),
    ],
    kind: Kind::Data,
    validate: valid,
    carrier: no_carrier,
});

/// The type of a virtual register: its register class, the term
/// `backend.reg<target, class>` written `!<dialect>.<class>` (`!x86_64.GPR`).
///
/// Every value a machine instruction reads or writes carries one, so a value's
/// class is a type read rather than a search for an attribute naming it. Targets
/// register one parse key per class through [`reg_class_type_parser`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegClassType(RegClassId);

impl RegClassType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, class: RegClassId) -> TypeId {
        let terms = context.terms();
        let args = vec![
            TypeArg::Sym(terms.name(class.dialect())),
            TypeArg::Sym(terms.name(class.name())),
        ];
        // A term holds the class's names; its register table is reachable
        // only through the context's registry.
        context.note_reg_class(class);
        terms.intern(REG.id(terms), args).expect("a register class")
    }

    /// The class `ty` is, when it was made through [`RegClassType::new`].
    pub fn view(context: &Context, ty: TypeId) -> Option<Self> {
        let terms = context.terms();
        if !REG.is(terms, ty) {
            return None;
        }
        let name = |index| match terms.arg(ty, index) {
            Some(TypeArg::Sym(name)) => Some(name),
            _ => None,
        };
        context.reg_class(name(0)?, name(1)?).map(Self)
    }

    pub fn class(&self) -> RegClassId {
        self.0
    }

    /// Prints `target.class`; a target parses its classes through
    /// [`reg_class_type_parser`], so the spelling lists no keys.
    pub fn spelling(context: &Context) -> TypeSpelling {
        TypeSpelling {
            ctor: REG.id(context),
            keys: &[],
            parse: |mnemonic, parser, _| {
                Err((
                    Span(parser.pos()),
                    Error::UnknownType(String::new(), mnemonic.to_string()),
                ))
            },
            print: |spellings, ty, fmt| {
                let name = |index| match spellings.terms().arg(ty, index) {
                    Some(TypeArg::Sym(name)) => spellings.terms().name_text(name),
                    _ => unreachable!("a register class names its target and class"),
                };
                fmt.write(format!("{}.{}", name(0), name(1)))
            },
            variadic_tail: false,
        }
    }
}

/// The [`tir::TypeParser`] a target registers under each of its register class
/// names, resolving the mnemonic against the target's own class table so two
/// targets declaring a `GPR` stay distinct.
pub fn reg_class_type_parser(
    classes: &'static [crate::backend::regalloc::RegClassInfo],
    dialect: &'static str,
) -> impl for<'src> Fn(
    &str,
    &mut tir::parse::text::Parser<'src>,
    &Context,
) -> Result<TypeId, (Span, Error)> {
    move |mnemonic, parser, context| {
        classes
            .iter()
            .find(|class| class.name == mnemonic)
            .map(|class| RegClassType::new(context, RegClassId::new(class)))
            .ok_or_else(|| {
                (
                    Span(parser.pos()),
                    Error::UnknownType(dialect.to_string(), mnemonic.to_string()),
                )
            })
    }
}

/// A fresh value of `class`: the type a machine instruction reads it through.
pub fn fresh_reg(context: &Context, class: RegClassId) -> ValueId {
    context
        .create_value(RegClassType::new(context, class), None)
        .id()
}

/// Give a value with no register class the class a machine instruction first
/// reads it through. Selection retypes the mid-end values it binds — a
/// constant or stack allocation a target pass materializes later, a call
/// argument — so every value a machine instruction names is a register.
pub fn retype_untyped(context: &Context, value: ValueId, class: RegClassId) {
    if value_class(context, value).is_none() {
        context.retype_value(value, RegClassType::new(context, class));
    }
}

/// The register class `value` holds, or `None` if it is not a register — or no
/// longer exists, which printing a stale reference must survive.
pub fn value_class(context: &Context, value: ValueId) -> Option<RegClassId> {
    context
        .has_value(value)
        .then(|| type_class(context, context.get_value(value).ty()))
        .flatten()
}

/// The register class `ty` denotes, or `None` if it is not a register class type.
pub fn type_class(context: &Context, ty: TypeId) -> Option<RegClassId> {
    if context.is_state_type(ty) {
        return None;
    }
    RegClassType::view(context, ty).map(|reg| reg.class())
}

/// Read an assembly operand attribute without constructing a register IR entity.
pub fn register_attr(op: &OpHandle, name: &str) -> Option<PhysReg> {
    match op.attr(name)? {
        AttributeValue::Register(RegisterAttr::Physical { class, index }) => Some((class, index)),
        _ => None,
    }
}

/// The register a function's values were allocated, written onto its
/// `asm.symbol` by register allocation and read by assembly printing and
/// encoding. Partial before allocation — precoloring pins entries — and total
/// over the function's register-typed values after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegAssignment(HashMap<ValueId, PhysReg>);

/// The attribute the total assignment is carried in, written by register
/// allocation onto the function's `asm.symbol`.
pub const ASSIGNMENT_ATTR: &str = "assignment";

/// The attribute an instruction carries its pre-coloring constraints in: the
/// physical register each named register slot must hold. A pin belongs to the
/// slot, not to the value in it — a rewrite that renames the value leaves the
/// instruction's requirement where it was.
pub const PINS_ATTR: &str = "pins";

/// The register the slot named `name` of `op` is pinned to.
pub fn slot_pin(op: &OpHandle, name: &str) -> Option<PhysReg> {
    let AttributeValue::Dict(pins) = op.attr(PINS_ATTR)? else {
        return None;
    };
    match pins.get(name)? {
        AttributeValue::Register(RegisterAttr::Physical { class, index }) => Some((*class, *index)),
        _ => None,
    }
}

/// Require `op`'s slot `name` to hold `register`.
pub fn pin_slot(context: &Context, op: &OpHandle, name: &str, register: PhysReg) {
    let mut pins = match op.attr(PINS_ATTR) {
        Some(AttributeValue::Dict(pins)) => *pins,
        _ => std::collections::BTreeMap::new(),
    };
    pins.insert(name.to_string(), phys_attr(register));
    let mut attributes = op.attributes().to_vec();
    attributes.retain(|attribute| context.resolve(attribute.name) != PINS_ATTR);
    attributes.push(context.named_attribute(PINS_ATTR, AttributeValue::Dict(Box::new(pins))));
    context.set_op_attributes(op.id, attributes);
}

impl RegAssignment {
    pub fn get(&self, value: ValueId) -> Option<PhysReg> {
        self.0.get(&value).copied()
    }

    pub fn insert(&mut self, value: ValueId, register: PhysReg) {
        self.0.insert(value, register);
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (ValueId, PhysReg)> + '_ {
        self.0.iter().map(|(value, register)| (*value, *register))
    }

    /// The map `op` carries under `attr` ([`ASSIGNMENT_ATTR`]), empty when it
    /// carries none.
    pub fn of_op(op: &OpHandle, attr: &str) -> Self {
        let Some(AttributeValue::Array(entries)) = op.attr(attr) else {
            return Self::default();
        };
        Self(
            entries
                .iter()
                .filter_map(|entry| match entry {
                    AttributeValue::Register(RegisterAttr::Assigned {
                        value,
                        class,
                        index,
                    }) => Some((*value, (*class, *index))),
                    _ => None,
                })
                .collect(),
        )
    }

    /// The attribute form, ordered by value so emission is deterministic.
    pub fn to_attribute(&self) -> AttributeValue {
        let mut entries: Vec<(ValueId, PhysReg)> = self.iter().collect();
        entries.sort_by_key(|(value, _)| value.number());
        AttributeValue::Array(
            entries
                .into_iter()
                .map(|(value, (class, index))| {
                    AttributeValue::Register(RegisterAttr::Assigned {
                        value,
                        class,
                        index,
                    })
                })
                .collect::<Vec<_>>()
                .into(),
        )
    }
}

impl FromIterator<(ValueId, PhysReg)> for RegAssignment {
    fn from_iter<I: IntoIterator<Item = (ValueId, PhysReg)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// A physical register as the attribute a machine op's slot names it with.
pub fn phys_attr(register: PhysReg) -> AttributeValue {
    AttributeValue::Register(RegisterAttr::Physical {
        class: register.0,
        index: register.1,
    })
}

/// The register allocation gave `value` in the function `op` belongs to.
pub fn assigned_register(context: &Context, op: &OpHandle, value: ValueId) -> Option<PhysReg> {
    let mut current = context.parent_op(op.id);
    while let Some(symbol) = current {
        if let Some(found) = context.with_attr(symbol, ASSIGNMENT_ATTR, |entries| {
            let AttributeValue::Array(entries) = entries else {
                return None;
            };
            let index = entries
                .binary_search_by_key(&value.number(), |entry| match entry {
                    AttributeValue::Register(RegisterAttr::Assigned { value, .. }) => {
                        value.number()
                    }
                    _ => u32::MAX,
                })
                .ok()?;
            match &entries[index] {
                AttributeValue::Register(RegisterAttr::Assigned { class, index, .. }) => {
                    Some((*class, *index))
                }
                _ => None,
            }
        }) {
            return found;
        }
        current = context.parent_op(symbol);
    }
    None
}
