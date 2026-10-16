//! The one e-node label TIR's e-graphs speak.
//!
//! Two operator families share it. [`Kind::Sym`] is the semantic vocabulary —
//! what the target rules, the axioms and the value gates (`If` for γ, `Theta`
//! for the per-value projection of a θ) are written in. [`Kind::Ir`] is the
//! identity of an IR operation the semantic vocabulary does not name, which is
//! what a peephole over live IR rewrites.
//!
//! A label identifies a term — operator, payload and result type, over the
//! canonical children. [`SemNode::prov`] rides alongside as write-back
//! provenance and never takes part in that identity.

use std::hash::{Hash, Hasher};

use tir_adt::{APFloat, APInt, FxHasher};
use tir_egraph::{Carrier, ClassId as Id, Label as ENode};

use crate::attributes::{AttributeValue, NamedAttribute};
use crate::sem::{SymKind, SymPayload};
use crate::{OpCost, OpHandle, OpId, Operation, TypeId, ValueId};

/// An IR operation's identity: `(dialect, name, attributes)`. `commutative`,
/// `cost` and the offset laws describe the operator but do not identify it.
#[derive(Clone, Debug)]
pub struct IrOp {
    pub dialect: &'static str,
    pub name: &'static str,
    pub attrs: Vec<NamedAttribute>,
    pub commutative: bool,
    pub cost: u32,
    /// The semantic operator whose offset laws the op shares, from its
    /// [`crate::Additive`] or [`crate::Subtractive`] declaration.
    pub law: Option<SymKind>,
    /// Bit `i` set when operand `i` has the result's type, so an identity
    /// element at the other operand returns it.
    pub result_typed: u8,
    /// In a template, the attributes a rule binds: any value matches them.
    pub bound: Vec<tir_adt::Sym>,
    /// The op's flags (`nsw`, `nuw`): facts its result relies on, poison
    /// where they fail. They keep a flagged op apart from an unflagged one,
    /// but a template that names none matches either: forgetting a flag only
    /// makes the op more defined.
    pub flags: Vec<NamedAttribute>,
    /// In a template a rule head fills, the attributes the head gives values,
    /// each with the kind of value its word stands for.
    pub unfilled: Vec<(tir_adt::Sym, AttributeWord)>,
}

/// What the word a rule head fills an attribute with stands for; see
/// [`attribute_word`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttributeWord {
    Int,
    UInt,
    Bool,
    Type,
    Str,
}

impl IrOp {
    /// The operator `identity` names, as `context` registered it: whether it
    /// commutes, and the laws it declares. Read off the name alone, so an op a
    /// rewrite introduces has the laws of a seeded one.
    pub fn declared(
        context: &crate::Context,
        identity: (&'static str, &'static str),
        attrs: Vec<NamedAttribute>,
        cost: u32,
    ) -> Self {
        let named = context.intern_op_name(identity.0, identity.1);
        let law = if context
            .find_op_interface::<dyn crate::Additive>(named)
            .is_some()
        {
            Some(SymKind::Add)
        } else if context
            .find_op_interface::<dyn crate::Subtractive>(named)
            .is_some()
        {
            Some(SymKind::Sub)
        } else {
            None
        };
        let uniform = context
            .find_op_interface::<dyn crate::SameOperandAndResultType>(named)
            .is_some();
        Self {
            dialect: identity.0,
            name: identity.1,
            attrs,
            commutative: context
                .find_op_interface::<dyn crate::Commutative>(named)
                .is_some(),
            cost,
            law,
            result_typed: if uniform { u8::MAX } else { 0 },
            bound: Vec::new(),
            flags: Vec::new(),
            unfilled: Vec::new(),
        }
    }

    /// The operator `instance` is, which its kind and attributes decide:
    /// its declaration, its [`OpCost`], and the attributes that identify it.
    pub fn of(instance: &OpHandle) -> Self {
        let cost = instance
            .clone()
            .as_interface::<dyn OpCost>()
            .map_or(1, |c| c.cost());
        let context = &instance.context;
        let mut op = Self::declared(
            context,
            (instance.dialect().as_str(), instance.name().as_str()),
            instance.attributes().to_vec(),
            cost,
        );
        // The attributes identify what the op computes, in one spelling:
        // one the op does not declare and floating-point semantics equal to
        // the operation's defaults add nothing, and a builder's order is not
        // an identity.
        let declared = declared_attributes(context, op.dialect, op.name);
        op.attrs.retain(|attribute| {
            declared
                .iter()
                .any(|declared| context.sym(declared.name) == Some(attribute.name))
                && !matches!(
                    &attribute.value,
                    AttributeValue::FpSemantics(semantics)
                        if **semantics == crate::fp::Semantics::default_for_operation(instance.name().as_str())
                )
        });
        op.split_flags(context, declared);
        op
    }

    fn split_flags(&mut self, context: &crate::Context, declared: &[crate::AttrSchema]) {
        self.attrs.sort_by_key(|attribute| attribute.name);
        let is_flag = |attribute: &NamedAttribute| {
            declared.iter().any(|declared| {
                declared.ty == "?Flag" && context.sym(declared.name) == Some(attribute.name)
            })
        };
        (self.flags, self.attrs) = self.attrs.drain(..).partition(is_flag);
        self.flags
            .retain(|flag| matches!(flag.value, AttributeValue::Bool(true)));
    }

    /// This operator without its flags: what selection, which has no
    /// poison to keep, reads it as.
    pub fn without_flags(mut self) -> Self {
        self.flags.clear();
        self
    }

    /// The bit of [`Self::result_typed`] operand `index` has; none past the
    /// eighth.
    pub fn operand_bit(index: usize) -> u8 {
        1u8.checked_shl(index as u32).unwrap_or(0)
    }

    /// A pattern over the operator alone, with no laws: a template only
    /// matches.
    pub fn template(dialect: &'static str, name: &'static str, attrs: Vec<NamedAttribute>) -> Self {
        Self {
            dialect,
            name,
            attrs,
            commutative: false,
            cost: 0,
            law: None,
            result_typed: 0,
            bound: Vec::new(),
            flags: Vec::new(),
            unfilled: Vec::new(),
        }
    }

    // Whether this template's attributes admit `target`'s: the fixed ones are
    // there with the same value, and every other one is bound.
    fn admits(&self, target: &IrOp) -> bool {
        self.dialect == target.dialect
            && self.name == target.name
            && self.flags.iter().all(|flag| target.flags.contains(flag))
            && self.attrs.iter().all(|fixed| target.attrs.contains(fixed))
            && target.attrs.iter().all(|attribute| {
                self.attrs.contains(attribute) || self.bound.contains(&attribute.name)
            })
    }
}

impl PartialEq for IrOp {
    fn eq(&self, other: &Self) -> bool {
        self.dialect == other.dialect
            && self.name == other.name
            && self.attrs == other.attrs
            && self.flags == other.flags
    }
}

impl Eq for IrOp {}

/// An e-node's operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Sym(SymKind),
    Ir(IrOp),
    /// An automaton state the selection rules insert: the chain `st(P)(f)`
    /// puts the instruction tree pattern `P` accepts in the value class of
    /// the row it selects, and its one child is the form the accepted tree
    /// is over. The seed chain of an operand's class is over that class
    /// itself. [`ANY_STATE`] in a template matches every state.
    State(u32),
    /// A selected row as pattern `P` over its operands' forms,
    /// `form(P)(w1..wK)`, in a class of its own: it emits nothing, and a
    /// chain names it. [`ANY_FORM`] in a template matches every form of the
    /// template's arity.
    Form(u32),
    /// Value `index` of an operation the graph does not describe, over what
    /// the operation reads: its operands in order, then the results of the
    /// regions it holds. The value is the operation's `index`-th result, or
    /// the `index`-th port of a loop's body, which reads what the loop is
    /// entered on. `changes` says the operation changes a state it
    /// publishes, so each of its results follows that write.
    ///
    /// With the [`SemPayload::Input`] payload the node is all the graph
    /// holds of the value: which operation it is the node's provenance
    /// says, the label is the same in every function, and each such node is
    /// a class of its own. Without it the node sits beside the term of a
    /// value a gate or a loop chooses, a function of what it reads like that
    /// term.
    Result {
        index: u32,
        changes: bool,
    },
    /// Some of the values an operation reads, where a [`Kind::Result`] does
    /// not hold them all: its children are then rows of this kind, each over
    /// [`WIDEST_RESULT`] of the values at most, or over such rows again.
    Operands,
}

/// The most children a [`Kind::Result`] or a [`Kind::Operands`] node has, so
/// the rules written per number of children cover them whatever a function
/// calls.
pub const WIDEST_RESULT: usize = 5;

/// The state a template names to match every state.
pub const ANY_STATE: u32 = u32::MAX;

/// The pattern a template names to match every form.
pub const ANY_FORM: u32 = u32::MAX;

impl Kind {
    /// The semantic operator, or `None` for a label the semantic vocabulary does
    /// not name.
    pub fn sym(&self) -> Option<SymKind> {
        match self {
            Kind::Sym(kind) => Some(*kind),
            _ => None,
        }
    }

    pub fn ir(&self) -> Option<&IrOp> {
        match self {
            Kind::Ir(op) => Some(op),
            _ => None,
        }
    }

    // Every state is one operator and every form another, so a template over
    // any of either finds them all.
    fn hash_into(&self, h: &mut impl Hasher) {
        match self {
            Kind::Sym(kind) => {
                0u8.hash(h);
                kind.hash(h);
            }
            Kind::Ir(op) => {
                1u8.hash(h);
                op.dialect.hash(h);
                op.name.hash(h);
                hash_attrs(&op.attrs, h);
                if !op.flags.is_empty() {
                    hash_attrs(&op.flags, h);
                }
            }
            Kind::State(_) => 2u8.hash(h),
            Kind::Form(_) => 3u8.hash(h),
            Kind::Result { .. } => 4u8.hash(h),
            Kind::Operands => 5u8.hash(h),
        }
    }
}

/// So the semantic sites keep reading `node.kind == SymKind::Extract`.
impl PartialEq<SymKind> for Kind {
    fn eq(&self, other: &SymKind) -> bool {
        matches!(self, Kind::Sym(kind) if kind == other)
    }
}

/// A node label payload: a semantic-expression payload, or an opaque marker for
/// an un-lowerable sub-expression. Each opaque leaf carries a unique serial so
/// two unrelated unknown computations never hash-cons into the same e-class.
#[derive(Clone, Debug)]
pub enum SemPayload {
    Expr(SymPayload<ValueId>),
    Opaque(u32),
    /// An IR value the graph does not model further. Which one is the
    /// node's provenance: the label is the same in every function, and each
    /// such node is a class of its own.
    Input,
}

/// An integer literal is its bits at its width: the `signed` flag is how a value
/// is read back, not what it is, so two spellings of one bit pattern share a
/// class.
impl PartialEq for SemPayload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SemPayload::Expr(SymPayload::Int(a)), SemPayload::Expr(SymPayload::Int(b))) => {
                a.width() == b.width()
                    && a.to_u64() == b.to_u64()
                    && a.high_limbs() == b.high_limbs()
            }
            (SemPayload::Expr(a), SemPayload::Expr(b)) => a == b,
            (SemPayload::Opaque(a), SemPayload::Opaque(b)) => a == b,
            (SemPayload::Input, SemPayload::Input) => true,
            _ => false,
        }
    }
}

/// Where the value a node stands for comes from at write-back. Never part of the
/// label: two terms that compute the same thing share a class however each was
/// spelled in the IR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Prov {
    #[default]
    None,
    /// The IR value the node stands for — a gate, or an opaque leaf.
    Value(ValueId),
    /// An IR operation that already computes it.
    Op(OpId),
    /// An operation a rewrite introduced, which the commit writes from the
    /// node's own label.
    Introduced,
}

/// An e-graph node label: the operator identity (kind/payload) plus the IR type of
/// the value it represents, and its operand e-classes carried inline (the
/// [`ENode`] contract). Hash-consing and pattern matching compare only the label
/// (kind/payload/type) and the canonical children.
///
/// `ty` is the result type for an op node, the value type for a leaf. `None` on a
/// *pattern* node means "match any type"; `None` on a *graph* node means the type
/// is unknown. The type is stored verbatim from the IR — no width is collapsed or
/// normalized — so every target can constrain on exactly the widths/classes it
/// distinguishes (x86/AArch64 8/16/32/64-bit forms, RISC-V word vs XLEN, vector
/// element types, floats), and untyped rules stay width-agnostic.
#[derive(Clone, Debug)]
pub struct SemNode {
    pub kind: Kind,
    pub payload: Option<SemPayload>,
    pub ty: Option<TypeId>,
    pub children: Vec<Id>,
    pub prov: Prov,
    /// The integer or pointer carrier the value lives in, read off its type
    /// where the node was built: a [`TypeId`] needs the context to read, and a
    /// pointer's width the data layout. Part of the label, so two nodes alike
    /// but for their carrier never share laws. An integer literal's carrier is
    /// its own width whatever this says.
    pub carrier: Option<Carrier>,
}

impl SemNode {
    /// A leaf standing for an IR value the graph does not model further.
    pub fn input(value: ValueId) -> Self {
        Self {
            kind: Kind::Sym(SymKind::Symbol),
            payload: Some(SemPayload::Input),
            ty: None,
            children: Vec::new(),
            prov: Prov::Value(value),
            carrier: None,
        }
    }

    /// An integer literal, identified by its width and bits alone.
    pub fn constant(value: APInt, prov: Prov) -> Self {
        Self {
            kind: Kind::Sym(SymKind::Constant),
            payload: Some(SemPayload::Expr(SymPayload::Int(value))),
            ty: None,
            children: Vec::new(),
            prov,
            carrier: None,
        }
    }

    /// A float literal, identified by its format and bits. No integer carrier
    /// holds it, so it is a row of its own class.
    pub fn float(value: APFloat, prov: Prov) -> Self {
        Self {
            payload: Some(SemPayload::Expr(SymPayload::Float(value))),
            ..Self::constant(APInt::new(1, 0), prov)
        }
    }

    /// The value-level γ projection standing for `value`: `If` over the condition
    /// and the two arms' classes.
    pub fn gamma(value: ValueId, args: Vec<Id>) -> Self {
        Self::projection(Kind::Sym(SymKind::If), value, args)
    }

    /// The per-value θ projection standing for `value`: `Theta(init, edges…)`,
    /// the value a port is entered with and the one each edge back into it — a
    /// latch, a `continue`, a `break` — carries.
    pub fn theta(value: ValueId, args: Vec<Id>) -> Self {
        Self::projection(Kind::Sym(SymKind::Theta), value, args)
    }

    /// The value a loop produces for one carried port, standing for `value`:
    /// `Loop(init, next, exit, pred)`, the port read inside the last three
    /// through a [`SemNode::port`].
    pub fn loop_(value: ValueId, args: Vec<Id>) -> Self {
        Self::projection(Kind::Sym(SymKind::Loop), value, args)
    }

    /// The carried value of a loop at the current iteration, standing for the
    /// port `value`; the child identifies the port.
    pub fn port(value: ValueId, args: Vec<Id>) -> Self {
        Self::projection(Kind::Sym(SymKind::Port), value, args)
    }

    /// Value `index` of an operation the graph does not describe, standing
    /// for `value` in a class of its own, over what the operation reads; see
    /// [`Kind::Result`].
    pub fn result(value: ValueId, index: usize, changes: bool, args: Vec<Id>) -> Self {
        Self {
            payload: Some(SemPayload::Input),
            ..Self::chosen(value, index, changes, args)
        }
    }

    /// What `value`, result `index` of a gate or a loop the graph has a term
    /// for, waits for beside that term; see [`Kind::Result`].
    pub fn chosen(value: ValueId, index: usize, changes: bool, args: Vec<Id>) -> Self {
        let index = index as u32;
        Self::projection(Kind::Result { index, changes }, value, args)
    }

    /// Some of what an operation reads, for its results to read; see
    /// [`Kind::Operands`].
    pub fn operands(args: Vec<Id>) -> Self {
        Self {
            kind: Kind::Operands,
            payload: None,
            ty: None,
            children: args,
            prov: Prov::None,
            carrier: None,
        }
    }

    /// A memory access standing for `value` — the value a read yields, or the
    /// state a write publishes — over the vocabulary's operands and the state
    /// it reads.
    pub fn access(kind: SymKind, value: ValueId, args: Vec<Id>) -> Self {
        Self::projection(Kind::Sym(kind), value, args)
    }

    fn projection(kind: Kind, value: ValueId, args: Vec<Id>) -> Self {
        Self {
            kind,
            payload: None,
            ty: None,
            children: args,
            prov: Prov::Value(value),
            carrier: None,
        }
    }

    /// A seeded IR op: identity/`ty`/attrs from `instance`, `cost` from its
    /// [`OpCost`] interface, laws from its declarations. `commutative` may
    /// widen what the op declares, for a merge whose inputs are unordered.
    pub fn seeded(instance: &OpHandle, ty: TypeId, commutative: bool, args: Vec<Id>) -> Self {
        Self::seeded_as(IrOp::of(instance), instance, ty, commutative, args)
    }

    /// [`Self::seeded`] for an `instance` whose operator, [`IrOp::of`] it, the
    /// caller already holds.
    pub fn seeded_as(
        mut op: IrOp,
        instance: &OpHandle,
        ty: TypeId,
        commutative: bool,
        args: Vec<Id>,
    ) -> Self {
        let context = &instance.context;
        op.commutative |= commutative;
        let result_typed = instance
            .operands()
            .iter()
            .enumerate()
            .filter(|&(_, &operand)| context.get_value(operand).ty() == ty)
            .fold(0, |bits, (index, _)| bits | IrOp::operand_bit(index));
        Self::seeded_row(op, instance.id, ty, result_typed, args)
    }

    /// [`Self::seeded_as`] for the op `id` whose row the caller read:
    /// `result_typed` is [`IrOp::result_typed`], an [`IrOp::operand_bit`] per
    /// operand of type `ty`.
    pub fn seeded_row(mut op: IrOp, id: OpId, ty: TypeId, result_typed: u8, args: Vec<Id>) -> Self {
        op.result_typed = result_typed;
        Self::ir(op, Some(ty), args, Prov::Op(id))
    }

    /// An op a rewrite introduces, before its head fills in its type and
    /// carrier.
    pub fn introduced<O: Operation>(context: &crate::Context, cost: u32, args: Vec<Id>) -> Self {
        Self::ir(
            IrOp::declared(context, (O::dialect(), O::name()), Vec::new(), cost),
            None,
            args,
            Prov::Introduced,
        )
    }

    /// [`Self::introduced`] carrying the attributes `names`, whose values a
    /// head fills in through [`attribute_field`]. One of a kind no word
    /// holds is left out, so a head filling it spells nothing.
    pub fn introduced_with<O: Operation>(
        context: &crate::Context,
        cost: u32,
        names: &[&str],
        args: Vec<Id>,
    ) -> Self {
        let declared = declared_attributes(context, O::dialect(), O::name());
        let unfilled = names
            .iter()
            .filter_map(|&name| {
                let ty = declared.iter().find(|declared| declared.name == name)?.ty;
                Some((context.intern(name), word_kind(ty)?))
            })
            .collect();
        let mut node = Self::introduced::<O>(context, cost, args);
        if let Kind::Ir(op) = &mut node.kind {
            op.unfilled = unfilled;
        }
        node
    }

    /// [`Self::pattern`] that binds the attributes `bound` and requires
    /// `fixed`.
    pub fn pattern_binding<O: Operation>(
        context: &crate::Context,
        fixed: Vec<(&str, AttributeValue)>,
        bound: &[&str],
        args: Vec<Id>,
    ) -> Self {
        let fixed = fixed
            .into_iter()
            .map(|(name, value)| NamedAttribute::new(context.intern(name), value))
            .collect();
        let mut op = IrOp::template(O::dialect(), O::name(), fixed);
        let declared = declared_attributes(context, O::dialect(), O::name());
        op.split_flags(context, declared);
        op.bound = bound.iter().map(|name| context.intern(name)).collect();
        Self::ir(op, None, args, Prov::None)
    }

    /// LHS template matching any op of `O`'s identity, at any result type.
    pub fn pattern<O: Operation>(args: Vec<Id>) -> Self {
        Self::ir(
            IrOp::template(O::dialect(), O::name(), Vec::new()),
            None,
            args,
            Prov::None,
        )
    }

    /// LHS template matching any node of the semantic operator `kind` over `args`,
    /// at any type.
    pub fn sym_pattern(kind: SymKind, args: Vec<Id>) -> Self {
        Self {
            kind: Kind::Sym(kind),
            payload: None,
            ty: None,
            children: args,
            prov: Prov::None,
            carrier: None,
        }
    }

    /// LHS template matching any γ gate over `args`.
    pub fn gamma_pattern(args: Vec<Id>) -> Self {
        Self::sym_pattern(SymKind::If, args)
    }

    fn ir(op: IrOp, ty: Option<TypeId>, args: Vec<Id>, prov: Prov) -> Self {
        Self {
            kind: Kind::Ir(op),
            payload: None,
            ty,
            children: args,
            prov,
            carrier: None,
        }
    }

    /// LHS template matching any op sharing this node's operator identity, at any
    /// result type, with `args` as its pattern children. `None` for a node that is
    /// not an IR op.
    pub fn op_template(&self, args: Vec<Id>) -> Option<Self> {
        let op = self.kind.ir()?;
        Some(Self::ir(
            IrOp::template(op.dialect, op.name, op.attrs.clone()),
            None,
            args,
            Prov::None,
        ))
    }

    /// The result type of an IR op node.
    pub fn op_type(&self) -> Option<TypeId> {
        self.kind.ir().and(self.ty)
    }

    /// The semantic operator this node is, or `None` for an IR op or a merge.
    pub fn sym(&self) -> Option<SymKind> {
        self.kind.sym()
    }

    /// The integer literal this node is, if it is one.
    pub fn int(&self) -> Option<&APInt> {
        match &self.payload {
            Some(SemPayload::Expr(SymPayload::Int(value))) => Some(value),
            _ => None,
        }
    }

    /// The bits of the literal this node is, an integer's or a float's of at
    /// most 64.
    pub fn bits(&self) -> Option<APInt> {
        match &self.payload {
            Some(SemPayload::Expr(SymPayload::Int(value))) => Some(value.clone()),
            Some(SemPayload::Expr(SymPayload::Float(value))) if value.bit_width() <= 64 => Some(
                APInt::new(u64::from(value.bit_width()), value.to_bits() as u64),
            ),
            _ => None,
        }
    }

    /// The same term, read at `ty`. A gate or a leaf carries the port's value
    /// type so the width-dependent rules bind on its class like any other.
    pub fn typed(mut self, ty: TypeId) -> Self {
        self.ty = Some(ty);
        self
    }

    /// The same term, its value living in `carrier`.
    pub fn carried(mut self, carrier: Option<Carrier>) -> Self {
        self.carrier = carrier;
        self
    }

    /// The IR value the node stands for at write-back.
    pub fn value(&self) -> Option<ValueId> {
        match self.prov {
            Prov::Value(value) => Some(value),
            _ => None,
        }
    }
}

/// The fields of a [`SemNode`] a rule reads off a label or fills into one.
pub mod field {
    /// The result type, as [`crate::TypeId::number`].
    pub const TY: u32 = 0;
    /// An integer literal's value.
    pub const INT_VALUE: u32 = 1;
    /// An integer literal's width.
    pub const INT_WIDTH: u32 = 2;
    /// An integer literal read as two's complement at its own width, so a
    /// negative step in a pointer chain is a negative distance.
    pub const INT_SIGNED: u32 = 3;
    /// The carrier, as [`super::carrier_word`] spells it; zero for none.
    pub const CARRIER: u32 = 4;
    /// A selection state's number.
    pub const STATE: u32 = 5;
    /// A selection form's pattern number.
    pub const FORM: u32 = 6;
    /// One for a [`super::Kind::Result`] of an operation that changes a
    /// state, zero for any other node.
    pub const CHANGES: u32 = 7;
    /// Bits of the operands of an introduced op that have its result's type,
    /// joined to the ones it declares: where an identity element returns
    /// the other operand.
    pub const RESULT_TYPED: u32 = 8;
    /// The first attribute field: an op's attribute named `sym` is field
    /// `ATTRIBUTE + sym`, its value the word [`super::attribute_word`] gives.
    /// An attribute no word holds is never read or filled.
    pub const ATTRIBUTE: u32 = 1 << 16;
}

/// The field an op's attribute named `name` is read and written through.
pub fn attribute_field(name: tir_adt::Sym) -> u32 {
    field::ATTRIBUTE + name.index() as u32
}

/// The word standing for attribute `value` in the context `names` serves,
/// so a rule moves an attribute from the op it matched to an op it builds: a
/// number is itself, a type its id, a string its interned name. `None` for a
/// value of another kind.
pub fn attribute_word(names: &crate::Names, value: &AttributeValue) -> Option<u64> {
    Some(match value {
        AttributeValue::Int(value) => *value as u64,
        AttributeValue::UInt(value) => *value,
        AttributeValue::Bool(value) => u64::from(*value),
        AttributeValue::Type(ty) => u64::from(ty.number()),
        AttributeValue::Str(name) => names.intern(name).index() as u64,
        _ => return None,
    })
}

fn word_attribute(names: &crate::Names, kind: AttributeWord, word: u64) -> Option<AttributeValue> {
    Some(match kind {
        AttributeWord::Int => AttributeValue::Int(word as i64),
        AttributeWord::UInt => AttributeValue::UInt(word),
        AttributeWord::Bool => AttributeValue::Bool(word != 0),
        AttributeWord::Type => AttributeValue::Type(TypeId::from_number(word.try_into().ok()?)),
        AttributeWord::Str => AttributeValue::Str(names.name_at(word as usize)?.into()),
    })
}

fn word_kind(ty: &str) -> Option<AttributeWord> {
    Some(match ty.trim_start_matches('?') {
        "Int" => AttributeWord::Int,
        "UInt" => AttributeWord::UInt,
        "Bool" | "Flag" => AttributeWord::Bool,
        "Type" => AttributeWord::Type,
        "Str" => AttributeWord::Str,
        _ => return None,
    })
}

fn declared_attributes(
    context: &crate::Context,
    dialect: &str,
    name: &str,
) -> &'static [crate::AttrSchema] {
    context
        .op_schema(dialect, name)
        .map_or(&[], |schema| schema.attributes)
}

/// The carrier of an integer `width` bits wide. Every integer of one width
/// shares it, typed or not, so a literal is one value however it is spelled.
pub fn int_carrier(width: u32) -> Option<Carrier> {
    (1..=64).contains(&width).then_some(Carrier {
        width: width as u8,
        key: u64::from(width),
    })
}

/// The carrier of a pointer `width` bits wide, apart from the integers of that
/// width.
pub fn pointer_carrier(width: u32) -> Option<Carrier> {
    int_carrier(width).map(|carrier| Carrier {
        key: POINTER | carrier.key,
        ..carrier
    })
}

/// Marks a pointer carrier's key.
const POINTER: u64 = 1 << 8;

/// `carrier` as one word, for a rule that fills it in: its key, which holds
/// its width in the low byte. Zero is no carrier.
pub fn carrier_word(carrier: Option<Carrier>) -> u64 {
    carrier.map_or(0, |carrier| carrier.key)
}

fn carrier_from_word(word: u64) -> Option<Carrier> {
    (word != 0).then_some(Carrier {
        width: (word & 0xff) as u8,
        key: word,
    })
}

pub fn template_node(
    kind: SymKind,
    payload: Option<SymPayload<ValueId>>,
    ty: Option<TypeId>,
) -> SemNode {
    SemNode {
        kind: Kind::Sym(kind),
        payload: payload.map(SemPayload::Expr),
        ty,
        children: Vec::new(),
        prov: Prov::None,
        carrier: None,
    }
}

/// High so extraction prefers any concrete value proven equal to a gate; chosen
/// only when nothing else exists.
const GATE_COST: u64 = 1 << 20;

/// Extraction cost for the IR-op vocabulary: an op's modeled cost, [`GATE_COST`]
/// for a gate or a leaf, zero for a constant.
pub fn cost(node: &SemNode) -> u64 {
    match &node.kind {
        Kind::Ir(op) => op.cost as u64,
        Kind::Sym(SymKind::Constant) => 0,
        Kind::Sym(_) | Kind::Result { .. } => GATE_COST,
        Kind::Operands => 0,
        Kind::State(_) | Kind::Form(_) => u64::MAX,
    }
}

/// Label equality, ignoring children — two e-nodes share an e-class iff their
/// labels are equal and their canonical children are equal (the [`ENode`] model).
impl PartialEq for SemNode {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.payload == other.payload
            && self.label_type() == other.label_type()
            && self.carrier == other.carrier
    }
}

impl Eq for SemNode {}

impl ENode for SemNode {
    type Names = crate::Names;
    type SortKey = Vec<u8>;

    /// The label spelled field by field: operator, attributes by name,
    /// arity, payload, type, carrier. A type and an attribute name are
    /// spelled out, their ids being the order the context met them in. A
    /// value or a block a label names keeps its number: only the function
    /// that owns it holds the label, and its own values are numbered in the
    /// order it made them.
    fn sort_key(&self, names: &crate::Names) -> Vec<u8> {
        let mut key = Vec::new();
        match &self.kind {
            Kind::Sym(kind) => {
                key.push(0);
                key.extend_from_slice(&(*kind as u16).to_be_bytes());
            }
            Kind::Ir(op) => {
                key.push(1);
                spell_text(op.dialect, &mut key);
                spell_text(op.name, &mut key);
                spell_attrs(&op.attrs, names, &mut key);
                if !op.flags.is_empty() {
                    spell_attrs(&op.flags, names, &mut key);
                }
            }
            Kind::State(id) => {
                key.push(2);
                key.extend_from_slice(&id.to_be_bytes());
            }
            Kind::Form(id) => {
                key.push(3);
                key.extend_from_slice(&id.to_be_bytes());
            }
            Kind::Result { index, changes } => {
                key.push(4);
                key.extend_from_slice(&index.to_be_bytes());
                key.push(u8::from(*changes));
            }
            Kind::Operands => key.push(5),
        }
        key.extend_from_slice(&(self.children.len() as u32).to_be_bytes());
        match &self.payload {
            None => key.push(0),
            Some(SemPayload::Expr(SymPayload::SymbolId(symbol))) => {
                key.push(1);
                key.extend_from_slice(&symbol.to_be_bytes());
            }
            Some(SemPayload::Expr(SymPayload::Value(value))) => {
                key.push(2);
                key.extend_from_slice(&value.number().to_be_bytes());
            }
            Some(SemPayload::Expr(SymPayload::Int(value))) => {
                key.push(3);
                key.extend_from_slice(&value.width().to_be_bytes());
                key.extend_from_slice(&value.to_u64().to_be_bytes());
                for limb in value.high_limbs() {
                    key.extend_from_slice(&limb.to_be_bytes());
                }
            }
            Some(SemPayload::Expr(SymPayload::Float(value))) => {
                key.push(4);
                spell_text(&format!("{value:?}"), &mut key);
            }
            Some(SemPayload::Opaque(serial)) => {
                key.push(5);
                key.extend_from_slice(&serial.to_be_bytes());
            }
            Some(SemPayload::Input) => key.push(6),
        }
        match self.label_type() {
            None => key.push(0),
            Some(ty) => {
                key.push(1);
                spell_text(&names.type_spelling(ty), &mut key);
            }
        }
        if let Some(carrier) = self.carrier {
            key.push(carrier.width);
            key.extend_from_slice(&carrier.key.to_be_bytes());
        }
        key
    }

    fn children(&self) -> &[Id] {
        &self.children
    }

    fn children_mut(&mut self) -> &mut [Id] {
        &mut self.children
    }

    fn hash_cons(&self) -> u64 {
        let mut h = FxHasher::default();
        hash_label(self, &mut h);
        self.children.hash(&mut h);
        h.finish()
    }

    fn label_hash(&self) -> u64 {
        let mut h = FxHasher::default();
        hash_label(self, &mut h);
        h.finish()
    }

    /// The operator index buckets by operator and attributes, never type or
    /// payload, which a template may leave open. Nor flags: a template that
    /// names none matches a flagged op.
    fn op_key(&self) -> u64 {
        let mut h = FxHasher::default();
        match &self.kind {
            Kind::Ir(op) => {
                1u8.hash(&mut h);
                op.dialect.hash(&mut h);
                op.name.hash(&mut h);
                hash_attrs(&op.attrs, &mut h);
            }
            kind => kind.hash_into(&mut h),
        }
        h.finish()
    }

    /// A template that binds attributes matches the ops of every bucket its
    /// attribute values key.
    fn template_key(&self) -> Option<u64> {
        match &self.kind {
            Kind::Ir(op) if !op.bound.is_empty() => None,
            _ => Some(self.op_key()),
        }
    }

    /// Operator/label equality, ignoring children: the kind, result type, and
    /// payload. A distinct opaque serial keeps memory effects and un-lowerable
    /// nodes from ever congruence-merging.
    fn matches(&self, other: &Self) -> bool {
        self == other
    }

    fn bare(&self) -> Self {
        Self {
            prov: Prov::None,
            ty: self.label_type(),
            ..self.clone()
        }
    }

    /// Template matching: a typed template only matches a node of exactly that
    /// type, an untyped one (`ty == None`) any type; a payload of `None` is a
    /// wildcard, `Some` matches by equality.
    fn matches_template(&self, target: &Self) -> bool {
        let any = matches!(
            (&self.kind, &target.kind),
            (Kind::State(ANY_STATE), Kind::State(_)) | (Kind::Form(ANY_FORM), Kind::Form(_))
        );
        let admitted = match (&self.kind, &target.kind) {
            (Kind::Ir(template), Kind::Ir(op)) => template.admits(op),
            _ => self.kind == target.kind,
        };
        if !any && !admitted {
            return false;
        }
        if self.ty.is_some() && target.label_type() != self.label_type() {
            return false;
        }
        match (&self.payload, &target.payload) {
            (None, _) => true,
            (Some(expected), Some(actual)) => expected == actual,
            (Some(_), None) => false,
        }
    }

    fn commutative(&self) -> bool {
        match &self.kind {
            Kind::Sym(kind) => kind.is_commutative(),
            Kind::Ir(op) => op.commutative,
            Kind::State(_) | Kind::Form(_) | Kind::Result { .. } | Kind::Operands => false,
        }
    }

    /// An integer literal, spelled untyped: the value is what the class is known
    /// to be, and the same number carried at a type and without one is one fact.
    fn constant(&self) -> Option<Self> {
        (self.sym() == Some(SymKind::Constant))
            .then(|| self.int())
            .flatten()
            .map(|value| SemNode::constant(value.clone(), Prov::None))
    }

    fn type_key(&self) -> Option<u64> {
        self.ty.map(|ty| ty.number() as u64)
    }

    fn scalar(&self, names: &crate::Names, field: u32) -> Option<u64> {
        match field {
            field::TY => self.type_key(),
            field::INT_VALUE => self.int().map(APInt::to_u64),
            field::INT_WIDTH => self.int().map(|value| value.width()),
            field::INT_SIGNED => self.int().map(|value| value.to_i64() as u64),
            field::STATE => match self.kind {
                Kind::State(state) => Some(u64::from(state)),
                _ => None,
            },
            field::FORM => match self.kind {
                Kind::Form(pattern) => Some(u64::from(pattern)),
                _ => None,
            },
            field::CHANGES => {
                let changes = matches!(self.kind, Kind::Result { changes: true, .. });
                Some(u64::from(changes))
            }
            field if field >= field::ATTRIBUTE => {
                let Kind::Ir(op) = &self.kind else {
                    return None;
                };
                let attribute = op
                    .attrs
                    .iter()
                    .find(|attribute| attribute_field(attribute.name) == field)?;
                attribute_word(names, &attribute.value)
            }
            _ => None,
        }
    }

    fn fill(template: &Self, names: &crate::Names, fills: &[(u32, u64)]) -> Option<Self> {
        let mut node = template.clone();
        let mut value = node.int().map(APInt::to_u64);
        let mut width = node.int().map(APInt::width);
        for &(field, word) in fills {
            match field {
                field::TY => node.ty = Some(TypeId::from_number(word as u32)),
                field::INT_VALUE => value = Some(word),
                field::INT_WIDTH => width = Some(word),
                field::CARRIER => node.carrier = carrier_from_word(word),
                field::STATE => node.kind = Kind::State(word as u32),
                field::FORM => node.kind = Kind::Form(word as u32),
                field::RESULT_TYPED => {
                    let Kind::Ir(op) = &mut node.kind else {
                        return None;
                    };
                    op.result_typed |= word as u8;
                }
                field if field >= field::ATTRIBUTE => {
                    let Kind::Ir(op) = &mut node.kind else {
                        return None;
                    };
                    let &(name, kind) = op
                        .unfilled
                        .iter()
                        .find(|(name, _)| attribute_field(*name) == field)?;
                    op.attrs.push(NamedAttribute::new(
                        name,
                        word_attribute(names, kind, word)?,
                    ));
                }
                _ => return None,
            }
        }
        if let Kind::Ir(op) = &mut node.kind
            && !op.unfilled.is_empty()
        {
            op.unfilled.clear();
            op.attrs.sort_by_key(|attribute| attribute.name);
        }
        if let (Some(value), Some(width)) = (value, width) {
            node.payload = Some(SemPayload::Expr(SymPayload::Int(APInt::new(width, value))));
        }
        Some(node)
    }

    fn is_unique(&self) -> bool {
        matches!(self.payload, Some(SemPayload::Input))
    }

    fn carrier(&self) -> Option<Carrier> {
        match self.literal() {
            Some(value) => self
                .carrier
                .or_else(|| int_carrier(u32::try_from(value.width()).ok()?)),
            None => self.carrier,
        }
    }

    fn offset_coefficient(&self, operand: usize) -> Option<i64> {
        self.law()?.offset_coefficient(operand)
    }

    /// An IR op's identity holds only where the operand it returns has the
    /// result's type: a pointer plus a zero integer is the pointer, but a null
    /// pointer plus an integer is no integer.
    fn identity(&self, operand: usize) -> Option<u64> {
        let law = self.law()?;
        if let Kind::Ir(op) = &self.kind
            && op.result_typed & (1 << (1 - operand.min(1))) == 0
        {
            return None;
        }
        law.identity(operand)
    }

    fn bias(&self) -> u64 {
        self.law().map_or(0, |law| law.bias())
    }

    fn int_value(&self) -> Option<u64> {
        self.literal().map(APInt::to_u64)
    }

    /// A pointer's constant is the address, carried as a pointer: its zero
    /// is the null address, apart from the zero of the integers as wide.
    fn constant_of(&self, value: u64) -> Option<Self> {
        let carrier = self.carrier()?;
        let pointer = carrier.key & POINTER != 0;
        let constant = SemNode::constant(APInt::new(u64::from(carrier.width), value), Prov::None);
        Some(constant.carried(pointer.then_some(carrier)))
    }
}

impl SemNode {
    // A pointer is the opaque pointer: what it points at is no part of what a
    // term computes.
    fn label_type(&self) -> Option<TypeId> {
        let pointer = self
            .carrier
            .is_some_and(|carrier| carrier.key & POINTER != 0);
        self.ty.filter(|_| !pointer)
    }

    /// The integer literal this node is, as a constant of the vocabulary.
    fn literal(&self) -> Option<&APInt> {
        (self.kind == SymKind::Constant)
            .then(|| self.int())
            .flatten()
    }

    /// The semantic operator whose offset laws this node has.
    fn law(&self) -> Option<SymKind> {
        match &self.kind {
            Kind::Sym(kind) => Some(*kind),
            Kind::Ir(op) => op.law,
            Kind::State(_) | Kind::Form(_) | Kind::Result { .. } | Kind::Operands => None,
        }
    }
}

/// Hashes exactly the fields compared by [`SemNode`]'s label equality.
fn hash_label(node: &SemNode, state: &mut impl Hasher) {
    node.kind.hash_into(state);
    match node.kind {
        Kind::State(id) | Kind::Form(id) => id.hash(state),
        Kind::Result { index, changes } => (index, changes).hash(state),
        Kind::Sym(_) | Kind::Ir(_) | Kind::Operands => {}
    }
    node.label_type().hash(state);
    node.carrier.map(|carrier| carrier.key).hash(state);
    match &node.payload {
        None => 0u8.hash(state),
        Some(SemPayload::Expr(SymPayload::SymbolId(s))) => {
            1u8.hash(state);
            s.hash(state);
        }
        Some(SemPayload::Expr(SymPayload::Value(v))) => {
            2u8.hash(state);
            v.number().hash(state);
        }
        Some(SemPayload::Expr(SymPayload::Int(i))) => {
            3u8.hash(state);
            i.width().hash(state);
            i.to_u64().hash(state);
            i.high_limbs().hash(state);
        }
        Some(SemPayload::Expr(SymPayload::Float(f))) => {
            4u8.hash(state);
            f.to_f64().to_bits().hash(state);
        }
        Some(SemPayload::Opaque(serial)) => {
            5u8.hash(state);
            serial.hash(state);
        }
        Some(SemPayload::Input) => 6u8.hash(state),
    }
}

fn spell_text(text: &str, key: &mut Vec<u8>) {
    key.extend_from_slice(text.as_bytes());
    key.push(0);
}

fn spell_attrs(attrs: &[NamedAttribute], names: &crate::Names, key: &mut Vec<u8>) {
    let mut named: Vec<(String, &AttributeValue)> = attrs
        .iter()
        .map(|attribute| (names.name(attribute.name), &attribute.value))
        .collect();
    named.sort_by(|a, b| a.0.cmp(&b.0));
    key.extend_from_slice(&(named.len() as u32).to_be_bytes());
    for (name, value) in named {
        spell_text(&name, key);
        spell_attr_value(value, names, key);
    }
}

fn spell_attr_value(value: &AttributeValue, names: &crate::Names, key: &mut Vec<u8>) {
    match value {
        AttributeValue::Str(text) => {
            key.push(0);
            spell_text(text, key);
        }
        AttributeValue::Int(value) => {
            key.push(1);
            key.extend_from_slice(&value.to_be_bytes());
        }
        AttributeValue::UInt(value) => {
            key.push(2);
            key.extend_from_slice(&value.to_be_bytes());
        }
        AttributeValue::Wide(limbs) => {
            key.push(16);
            key.extend_from_slice(&(limbs.len() as u32).to_be_bytes());
            limbs
                .iter()
                .for_each(|limb| key.extend_from_slice(&limb.to_be_bytes()));
        }
        AttributeValue::F32(value) => {
            key.push(3);
            key.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        AttributeValue::F64(value) => {
            key.push(4);
            key.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        AttributeValue::Bool(value) => key.extend_from_slice(&[5, u8::from(*value)]),
        AttributeValue::Array(values) => {
            key.push(6);
            key.extend_from_slice(&(values.len() as u32).to_be_bytes());
            for value in values.iter() {
                spell_attr_value(value, names, key);
            }
        }
        AttributeValue::Dict(entries) => {
            key.push(7);
            key.extend_from_slice(&(entries.len() as u32).to_be_bytes());
            for (name, value) in entries.iter() {
                spell_text(name, key);
                spell_attr_value(value, names, key);
            }
        }
        AttributeValue::FpSemantics(semantics) => {
            key.push(8);
            spell_text(&format!("{semantics:?}"), key);
        }
        AttributeValue::EvaluationContract(contract) => {
            key.push(9);
            let untyped = crate::fp::EvaluationContract {
                result_formats: Vec::new(),
                ..(**contract).clone()
            };
            spell_text(&format!("{untyped:?}"), key);
            key.extend_from_slice(&(contract.result_formats.len() as u32).to_be_bytes());
            for &format in &contract.result_formats {
                spell_text(&names.type_spelling(format), key);
            }
        }
        AttributeValue::LayoutSpec(spec) => {
            key.push(15);
            spell_text(&format!("{spec:?}"), key);
        }
        AttributeValue::Register(register) => {
            key.push(10);
            spell_text(&format!("{register:?}"), key);
        }
        AttributeValue::Predicate(predicate) => key.extend_from_slice(&[11, *predicate as u8]),
        AttributeValue::Value(value) => {
            key.push(12);
            key.extend_from_slice(&value.number().to_be_bytes());
        }
        AttributeValue::Type(ty) => {
            key.push(13);
            spell_text(&names.type_spelling(*ty), key);
        }
        AttributeValue::Block(block) => {
            key.push(14);
            spell_text(&format!("{block:?}"), key);
        }
    }
}

fn hash_attrs(attrs: &[NamedAttribute], h: &mut impl Hasher) {
    attrs.len().hash(h);
    for attr in attrs {
        attr.name.hash(h);
        hash_attr_value(&attr.value, h);
    }
}

fn hash_attr_value(value: &AttributeValue, h: &mut impl Hasher) {
    std::mem::discriminant(value).hash(h);
    match value {
        AttributeValue::Str(s) => s.hash(h),
        AttributeValue::Predicate(p) => (*p as u8).hash(h),
        AttributeValue::Int(i) => i.hash(h),
        AttributeValue::UInt(u) => u.hash(h),
        AttributeValue::Wide(limbs) => limbs.hash(h),
        AttributeValue::F32(f) => f.to_bits().hash(h),
        AttributeValue::F64(f) => f.to_bits().hash(h),
        AttributeValue::Bool(b) => b.hash(h),
        AttributeValue::Type(t) => t.hash(h),
        AttributeValue::Block(b) => b.hash(h),
        AttributeValue::Array(a) => a.iter().for_each(|v| hash_attr_value(v, h)),
        AttributeValue::Dict(d) => d.iter().for_each(|(k, v)| {
            k.hash(h);
            hash_attr_value(v, h);
        }),
        AttributeValue::EvaluationContract(contract) => {
            contract.hash(h);
        }
        // Register and value attributes are machine IR; the value vocabulary
        // never sees them.
        AttributeValue::Register(_)
        | AttributeValue::Value(_)
        | AttributeValue::FpSemantics(_)
        | AttributeValue::LayoutSpec(_) => {}
    }
}
