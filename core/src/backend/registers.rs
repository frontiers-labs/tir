//! The one register notation of machine IR.
//!
//! A register operand of a machine instruction is an SSA operand or result
//! whose type is its register class ([`RegClassType`]). A physical register the
//! instruction names directly — an assembler input, a hardwired `x0`, the stack
//! pointer in prologue code — is an attribute literal in the same slot. Which
//! slots an opcode has, in which order, and what class each admits is a
//! per-opcode fact: [`InstrInfo::regs`](super::InstrInfo::regs).
//!
//! Register allocation does not rewrite those slots. It writes a
//! [`RegAssignment`] onto the function's `asm.symbol`, and assembly printing and
//! encoding resolve a slot holding a value through that map.

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use tir::attributes::{AttributeValue, RegisterAttr};
use tir::ty::TypeConstraint;
use tir::{Context, Error, IRFormatter, OpHandle, Type, TypeId, ValueId, parse::Span};

use crate::backend::liveness::PhysReg;
use crate::backend::regalloc::RegClassId;

/// The type of a virtual register: its register class, written
/// `!<dialect>.<class>` (`!x86_64.GPR`).
///
/// Every value a machine instruction reads or writes carries one, so a value's
/// class is a type read rather than a search for an attribute naming it. Targets
/// register one parse key per class through [`reg_class_type_parser`].
pub struct RegClassType(RegClassId);

impl RegClassType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, class: RegClassId) -> TypeId {
        context.get_type_id(Arc::new(Self(class)))
    }

    pub fn class(&self) -> RegClassId {
        self.0
    }
}

impl TypeConstraint for RegClassType {}

impl Type for RegClassType {
    fn dialect(&self) -> &'static str {
        self.0.dialect()
    }

    /// Unused: a target registers this type once per register class it declares,
    /// under the class's own name, through [`reg_class_type_parser`].
    fn parse_key() -> &'static str {
        "reg_class"
    }

    fn parse<'src>(
        mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        _context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Err((
            Span(parser.pos()),
            Error::UnknownType(String::new(), mnemonic.to_string()),
        ))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write(self.0.name())
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any)
            .downcast_ref::<RegClassType>()
            .is_some_and(|other| other.0 == self.0)
    }

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        std::ptr::hash(self.0.info(), &mut StdHasher(state));
    }
}

/// `std::ptr::hash` needs a sized `Hasher`; the type interner hands out a
/// `&mut dyn Hasher`.
struct StdHasher<'a>(&'a mut dyn std::hash::Hasher);

impl std::hash::Hasher for StdHasher<'_> {
    fn finish(&self) -> u64 {
        self.0.finish()
    }
    fn write(&mut self, bytes: &[u8]) {
        self.0.write(bytes)
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

/// The register class a slot holds: the physical register's own, or the class
/// the value's type names.
pub fn slot_class(context: &Context, slot: RegSlot) -> Option<RegClassId> {
    match slot {
        RegSlot::Phys((class, _)) => Some(class),
        RegSlot::Value(value) => value_class(context, value),
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
    if ty == TypeId::DEPENDENCY {
        return None;
    }
    let data = context.get_type_data(ty);
    (data.as_ref() as &dyn Any)
        .downcast_ref::<RegClassType>()
        .map(RegClassType::class)
}

/// One register slot of a machine opcode, in the order the opcode declares them.
///
/// A `Def` port is an SSA result, a `Use` port an SSA operand, both in port
/// order among the ports of their direction; a port whose slot holds a physical
/// literal on a given instance takes no SSA position there (see
/// [`reg_slots`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegPort {
    /// The name the opcode's assembly syntax, encoding and behavior use for this
    /// slot.
    pub name: &'static str,
    /// The class the instruction encodes here, narrowing the value's own class.
    /// `None` where the instruction admits whatever class the value carries.
    pub class: Option<RegClassId>,
    pub def: bool,
    /// The `Def` port this operand must share a register with, by name — a
    /// two-address destination read.
    pub tied_to: Option<&'static str>,
}

impl RegPort {
    pub const fn use_of(name: &'static str, class: RegClassId) -> Self {
        RegPort {
            name,
            class: Some(class),
            def: false,
            tied_to: None,
        }
    }

    pub const fn def_of(name: &'static str, class: RegClassId) -> Self {
        RegPort {
            name,
            class: Some(class),
            def: true,
            tied_to: None,
        }
    }
}

/// Where one register slot of an instruction instance lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegSlot {
    /// An SSA value: the operand or result at this port.
    Value(ValueId),
    /// A physical register the instruction names directly.
    Phys(PhysReg),
}

/// One resolved register slot of an instruction instance.
pub struct SlotRef {
    pub port: &'static RegPort,
    pub slot: RegSlot,
    /// Position among the op's operands (a use port) or results (a def port);
    /// `None` for a port naming a physical register.
    pub position: Option<usize>,
}

/// Resolve every register slot of `op` in port order.
///
/// A port whose attribute is set resolves to it — a physical register, or
/// nothing at all where a target pass has materialized the slot into a constant
/// — and consumes no SSA position; every other port takes the next operand (or
/// result) of its direction.
pub fn reg_slots(op: &OpHandle) -> Vec<SlotRef> {
    let Some(mi) = op.clone().as_interface::<dyn super::MachineInstruction>() else {
        return Vec::new();
    };
    let ports = mi.info().regs;
    if ports.is_empty() {
        return Vec::new();
    }
    let context = op.context.upgrade();
    let operands = op.value_operands();
    let results = op.value_results();
    let mut next_operand = 0;
    let mut next_result = 0;
    let mut slots = Vec::with_capacity(ports.len());
    for port in ports {
        // A slot holds either an attribute or an SSA position, never both: a
        // literal register is spelled as the attribute, and a demand slot the
        // target has materialized to a constant is no longer a register at all.
        // Read in place — this runs once per port of every instruction liveness
        // scans.
        let attribute = context.with_attr(op.id, port.name, |value| match value {
            AttributeValue::Register(RegisterAttr::Physical { class, index }) => {
                Some((*class, *index))
            }
            _ => None,
        });
        if let Some(register) = attribute {
            if let Some(register) = register {
                slots.push(SlotRef {
                    port,
                    slot: RegSlot::Phys(register),
                    position: None,
                });
            }
            continue;
        }
        let (value, position) = if port.def {
            let position = next_result;
            next_result += 1;
            (results.get(position).copied(), position)
        } else {
            let position = next_operand;
            next_operand += 1;
            (operands.get(position).copied(), position)
        };
        if let Some(value) = value {
            slots.push(SlotRef {
                port,
                slot: RegSlot::Value(value),
                position: Some(position),
            });
        }
    }
    slots
}

/// The register ports `op`'s opcode declares, in order.
pub fn reg_ports(op: &OpHandle) -> &'static [RegPort] {
    op.clone()
        .as_interface::<dyn super::MachineInstruction>()
        .map(|mi| mi.info().regs)
        .unwrap_or_default()
}

/// [`reg_slots`] for one port, by name.
pub fn reg_slot(op: &OpHandle, name: &str) -> Option<RegSlot> {
    reg_slots(op)
        .into_iter()
        .find(|slot| slot.port.name == name)
        .map(|slot| slot.slot)
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

/// The attribute an `asm.symbol` carries the calling convention's placement of
/// its arguments in: value → register, since an argument is placed by what it
/// is rather than by where an instruction reads it.
pub const ARG_PINS_ATTR: &str = "arg_pins";

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

    /// The map `op` carries under `attr` ([`ASSIGNMENT_ATTR`] or
    /// [`ARG_PINS_ATTR`]), empty when it carries none.
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

/// Bind a register use slot on a machine op builder: the SSA operand when the
/// slot holds a value, the physical register as the slot's attribute otherwise.
#[macro_export]
macro_rules! reg_use {
    ($builder:expr, $port:ident, $slot:expr) => {
        match $slot {
            $crate::backend::RegSlot::Value(value) => $builder.$port(value),
            $crate::backend::RegSlot::Phys(register) => {
                $builder.attr(stringify!($port), $crate::backend::phys_attr(register))
            }
        }
    };
}

/// [`reg_use`] for a def slot: the op's result when the slot holds a value.
#[macro_export]
macro_rules! reg_def {
    ($builder:expr, $port:ident, $slot:expr) => {
        match $slot {
            $crate::backend::RegSlot::Value(value) => $builder.result_values(vec![value]),
            $crate::backend::RegSlot::Phys(register) => {
                $builder.attr(stringify!($port), $crate::backend::phys_attr(register))
            }
        }
    };
}

/// The physical register a slot names, directly or through the assignment.
pub fn slot_register(slot: RegSlot, assignment: &RegAssignment) -> Option<PhysReg> {
    match slot {
        RegSlot::Phys(register) => Some(register),
        RegSlot::Value(value) => assignment.get(value),
    }
}

/// The physical register the slot named `name` of `op` ends up in: the register
/// it names directly, or the one allocation gave the value it holds.
///
/// For a caller holding one instruction rather than a whole function (a post-RA
/// encoding rewrite); the assignment entries are sorted by value, so the lookup
/// reads the enclosing symbol's map without copying it.
pub fn op_slot_register(context: &Context, op: &OpHandle, name: &str) -> Option<PhysReg> {
    match reg_slot(op, name)? {
        RegSlot::Phys(register) => Some(register),
        RegSlot::Value(value) => assigned_register(context, op, value),
    }
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
