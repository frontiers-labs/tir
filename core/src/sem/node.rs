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

use tir_adt::{APInt, FxHasher};
use tir_symbolic::egraph::{ENode, Id};

use crate::attributes::{AttributeValue, NamedAttribute};
use crate::sem::{SymKind, SymPayload};
use crate::{OpCost, OpHandle, OpId, Operation, TypeId, ValueId};

/// An IR operation's identity: `(dialect, name, attributes)`. `commutative` and
/// `cost` describe the operator but do not identify it.
#[derive(Clone, Debug)]
pub struct IrOp {
    pub dialect: &'static str,
    pub name: &'static str,
    pub attrs: Vec<NamedAttribute>,
    pub commutative: bool,
    pub cost: u32,
}

impl PartialEq for IrOp {
    fn eq(&self, other: &Self) -> bool {
        self.dialect == other.dialect && self.name == other.name && self.attrs == other.attrs
    }
}

impl Eq for IrOp {}

/// An e-node's operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Sym(SymKind),
    Ir(IrOp),
}

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
            }
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
}

/// An integer literal is its bits at its width: the `signed` flag is how a value
/// is read back, not what it is, so two spellings of one bit pattern share a
/// class.
impl PartialEq for SemPayload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SemPayload::Expr(SymPayload::Int(a)), SemPayload::Expr(SymPayload::Int(b))) => {
                a.width() == b.width() && a.to_u64() == b.to_u64()
            }
            (SemPayload::Expr(a), SemPayload::Expr(b)) => a == b,
            (SemPayload::Opaque(a), SemPayload::Opaque(b)) => a == b,
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
    /// A rewrite's emitter, by index into the ruleset that introduced it.
    Introduced(usize),
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
}

impl SemNode {
    /// A leaf standing for an IR value the graph does not model further.
    pub fn input(value: ValueId) -> Self {
        Self {
            kind: Kind::Sym(SymKind::Symbol),
            payload: Some(SemPayload::Expr(SymPayload::Value(value))),
            ty: None,
            children: Vec::new(),
            prov: Prov::Value(value),
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

    /// A memory access standing for `value` — the value a read yields, or the
    /// state a write publishes — over the vocabulary's operands and the state
    /// it reads.
    pub fn access(kind: SymKind, value: ValueId, args: Vec<Id>) -> Self {
        Self::projection(Kind::Sym(kind), value, args)
    }

    /// A term a law introduced over the operation that rebuilds it: a gate grows
    /// the port carrying the value out, an access is copied where it is needed.
    pub fn introduced_at(kind: SymKind, op: OpId, args: Vec<Id>) -> Self {
        Self {
            kind: Kind::Sym(kind),
            payload: None,
            ty: None,
            children: args,
            prov: Prov::Op(op),
        }
    }

    fn projection(kind: Kind, value: ValueId, args: Vec<Id>) -> Self {
        Self {
            kind,
            payload: None,
            ty: None,
            children: args,
            prov: Prov::Value(value),
        }
    }

    /// A seeded IR op: identity/`ty`/attrs from `instance`, `cost` from its
    /// [`OpCost`] interface.
    pub fn seeded(instance: &OpHandle, ty: TypeId, commutative: bool, args: Vec<Id>) -> Self {
        let cost = instance
            .clone()
            .as_interface::<dyn OpCost>()
            .map_or(1, |c| c.cost());
        Self::ir(
            IrOp {
                dialect: instance.dialect().as_str(),
                name: instance.name().as_str(),
                attrs: instance.attributes().to_vec(),
                commutative,
                cost,
            },
            Some(ty),
            args,
            Prov::Op(instance.id),
        )
    }

    /// An op a rewrite introduced, built by the ruleset's `idx`-th emitter.
    pub fn introduced<O: Operation>(ty: TypeId, cost: u32, idx: usize, args: Vec<Id>) -> Self {
        Self::ir(
            IrOp {
                dialect: O::dialect(),
                name: O::name(),
                attrs: Vec::new(),
                commutative: false,
                cost,
            },
            Some(ty),
            args,
            Prov::Introduced(idx),
        )
    }

    /// LHS template matching any op of `O`'s identity, at any result type.
    pub fn pattern<O: Operation>(args: Vec<Id>) -> Self {
        Self::ir(
            IrOp {
                dialect: O::dialect(),
                name: O::name(),
                attrs: Vec::new(),
                commutative: false,
                cost: 0,
            },
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
        }
    }

    /// LHS template matching any op sharing this node's operator identity, at any
    /// result type, with `args` as its pattern children. `None` for a node that is
    /// not an IR op.
    pub fn op_template(&self, args: Vec<Id>) -> Option<Self> {
        let op = self.kind.ir()?;
        Some(Self::ir(
            IrOp {
                dialect: op.dialect,
                name: op.name,
                attrs: op.attrs.clone(),
                commutative: false,
                cost: 0,
            },
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

    /// The same term, read at `ty`. A gate or a leaf carries the port's value
    /// type so the width-dependent rules bind on its class like any other.
    pub fn typed(mut self, ty: TypeId) -> Self {
        self.ty = Some(ty);
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
        Kind::Sym(_) => GATE_COST,
    }
}

/// Label equality, ignoring children — two e-nodes share an e-class iff their
/// labels are equal and their canonical children are equal (the [`ENode`] model).
impl PartialEq for SemNode {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.payload == other.payload && self.ty == other.ty
    }
}

impl Eq for SemNode {}

impl ENode for SemNode {
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

    /// The operator index buckets by operator alone: a pattern template with a
    /// wildcard type/payload must find every class holding its operator (the
    /// [`ENode::op_key`] contract for [`ENode::matches_template`]).
    fn op_key(&self) -> u64 {
        let mut h = FxHasher::default();
        self.kind.hash_into(&mut h);
        h.finish()
    }

    /// Operator/label equality, ignoring children: the kind, result type, and
    /// payload. A distinct opaque serial keeps memory effects and un-lowerable
    /// nodes from ever congruence-merging.
    fn matches(&self, other: &Self) -> bool {
        self == other
    }

    /// Template matching: a typed template only matches a node of exactly that
    /// type, an untyped one (`ty == None`) any type; a payload of `None` is a
    /// wildcard, `Some` matches by equality.
    fn matches_template(&self, target: &Self) -> bool {
        if self.kind != target.kind {
            return false;
        }
        if self.ty.is_some() && target.ty != self.ty {
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

    fn scalar(&self, field: u32) -> Option<u64> {
        match field {
            field::TY => self.type_key(),
            field::INT_VALUE => self.int().map(APInt::to_u64),
            field::INT_WIDTH => self.int().map(|value| value.width() as u64),
            field::INT_SIGNED => self.int().map(|value| value.to_i64() as u64),
            _ => None,
        }
    }

    fn fill(template: &Self, fills: &[(u32, u64)]) -> Option<Self> {
        let mut node = template.clone();
        let mut value = node.int().map(APInt::to_u64);
        let mut width = node.int().map(APInt::width);
        for &(field, word) in fills {
            match field {
                field::TY => node.ty = Some(TypeId::from_number(word as u32)),
                field::INT_VALUE => value = Some(word),
                field::INT_WIDTH => width = Some(word as u32),
                _ => return None,
            }
        }
        if let (Some(value), Some(width)) = (value, width) {
            node.payload = Some(SemPayload::Expr(SymPayload::Int(APInt::new(width, value))));
        }
        Some(node)
    }

    fn from_int(value: APInt) -> Option<Self> {
        Some(SemNode::constant(value, Prov::None))
    }
}

/// Hashes exactly the fields compared by [`SemNode`]'s label equality.
fn hash_label(node: &SemNode, state: &mut impl Hasher) {
    node.kind.hash_into(state);
    node.ty.hash(state);
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
        }
        Some(SemPayload::Expr(SymPayload::Float(f))) => {
            4u8.hash(state);
            f.to_f64().to_bits().hash(state);
        }
        Some(SemPayload::Opaque(serial)) => {
            5u8.hash(state);
            serial.hash(state);
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
        AttributeValue::Int(i) => i.hash(h),
        AttributeValue::UInt(u) => u.hash(h),
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
        // Register and value attributes are machine IR; the value vocabulary
        // never sees them.
        AttributeValue::Register(_) | AttributeValue::Value(_) => {}
    }
}
