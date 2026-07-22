//! The e-node label: gates reuse GSA's [`GateNode`]; ops/constants key on their
//! value-signature for hash-consing, while `cost`/`prov`/`origin` ride as
//! extraction/write-back payload only.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use tir_symbolic::egraph::{ENode, Id};

use crate::analysis::GateNode;
use crate::attributes::{AttributeValue, NamedAttribute};
use crate::utils::APInt;
use crate::{OpCost, OpId, OpInstance, Operation, TypeId, ValueId};

/// The type sentinel an LHS template uses to mean "match any result type".
fn any_type() -> TypeId {
    TypeId::from_number(u32::MAX)
}

/// Write-back source for an op node: reuse a seeded IR op, or a rewrite-introduced one built at extraction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpProv {
    Seeded(OpId),
    Introduced(usize),
}

#[derive(Clone, Debug)]
pub enum Node {
    /// A GSA input or gate (never `GateNode::Op`); identity is the gate kind (+ input's `ValueId`).
    Gate(GateNode, Vec<Id>),
    /// Identity is `(dialect, name, ty, attrs)` + `args`; `cost`/`prov` are payload.
    Op {
        dialect: &'static str,
        name: &'static str,
        ty: TypeId,
        attrs: Vec<NamedAttribute>,
        commutative: bool,
        cost: u32,
        prov: OpProv,
        args: Vec<Id>,
    },
    /// Identity is `(width, bits)`; `origin` reuses a seeded constant op at write-back.
    Const { value: APInt, origin: Option<OpId> },
}

impl Node {
    /// A seeded op: identity/`ty`/attrs from `instance`, `cost` from its [`OpCost`] interface.
    pub fn seeded(
        instance: &Arc<OpInstance>,
        ty: TypeId,
        commutative: bool,
        args: Vec<Id>,
    ) -> Self {
        let cost = instance
            .clone()
            .as_interface::<dyn OpCost>()
            .map_or(1, |c| c.cost());
        Self::Op {
            dialect: instance.dialect().as_str(),
            name: instance.name().as_str(),
            ty,
            attrs: instance.attributes.clone(),
            commutative,
            cost,
            prov: OpProv::Seeded(instance.id),
            args,
        }
    }

    pub fn introduced<O: Operation>(ty: TypeId, cost: u32, idx: usize, args: Vec<Id>) -> Self {
        Self::Op {
            dialect: O::dialect(),
            name: O::name(),
            ty,
            attrs: Vec::new(),
            commutative: false,
            cost,
            prov: OpProv::Introduced(idx),
            args,
        }
    }

    pub fn pattern<O: Operation>(args: Vec<Id>) -> Self {
        Self::introduced::<O>(any_type(), 0, usize::MAX, args)
    }

    pub fn gamma_pattern(args: Vec<Id>) -> Self {
        let any = ValueId::from_number(u32::MAX);
        Self::Gate(
            GateNode::Gamma {
                value: any,
                cond: any,
            },
            args,
        )
    }

    /// A leaf gate standing for block-argument `value`.
    pub fn input(value: ValueId) -> Self {
        Node::Gate(GateNode::Input(value), Vec::new())
    }

    pub fn op_type(&self) -> Option<TypeId> {
        match self {
            Node::Op { ty, .. } => Some(*ty),
            _ => None,
        }
    }
}

/// High so extraction prefers any concrete value proven equal to a gate; chosen only when nothing else exists.
const GATE_COST: u64 = 1 << 20;

/// Extraction cost: an op's modeled cost, [`GATE_COST`] for gates, zero for constants.
pub fn cost(node: &Node) -> u64 {
    match node {
        Node::Op { cost, .. } => *cost as u64,
        Node::Gate(..) => GATE_COST,
        Node::Const { .. } => 0,
    }
}

impl ENode for Node {
    fn children(&self) -> &[Id] {
        match self {
            Node::Gate(_, args) | Node::Op { args, .. } => args,
            Node::Const { .. } => &[],
        }
    }

    fn children_mut(&mut self) -> &mut [Id] {
        match self {
            Node::Gate(_, args) | Node::Op { args, .. } => args,
            Node::Const { .. } => &mut [],
        }
    }

    fn commutative(&self) -> bool {
        matches!(
            self,
            Node::Op {
                commutative: true,
                ..
            }
        )
    }

    fn hash_cons(&self) -> u64 {
        let mut h = tir_adt::FxHasher::default();
        match self {
            Node::Gate(gate, _) => {
                0u8.hash(&mut h);
                hash_gate(gate, &mut h);
            }
            Node::Op {
                dialect,
                name,
                ty,
                attrs,
                ..
            } => {
                1u8.hash(&mut h);
                dialect.hash(&mut h);
                name.hash(&mut h);
                ty.hash(&mut h);
                hash_attrs(attrs, &mut h);
            }
            Node::Const { value, .. } => {
                2u8.hash(&mut h);
                value.width().hash(&mut h);
                value.to_u64().hash(&mut h);
            }
        }
        self.children().hash(&mut h);
        h.finish()
    }

    /// Search-index bucket: like [`hash_cons`](Self::hash_cons) but omits an `Op`'s result type, so a wildcard-typed ([`any_type`]) template buckets with the concrete ops it matches.
    fn op_key(&self) -> u64 {
        let mut h = tir_adt::FxHasher::default();
        match self {
            Node::Gate(gate, _) => {
                0u8.hash(&mut h);
                hash_gate(gate, &mut h);
            }
            Node::Op {
                dialect,
                name,
                attrs,
                ..
            } => {
                1u8.hash(&mut h);
                dialect.hash(&mut h);
                name.hash(&mut h);
                hash_attrs(attrs, &mut h);
            }
            Node::Const { value, .. } => {
                2u8.hash(&mut h);
                value.width().hash(&mut h);
                value.to_u64().hash(&mut h);
            }
        }
        h.finish()
    }

    /// Operator identity: `Op` on dialect+name, result type (a pattern wildcard matches anything), and value attributes; `Const` on width+bits; a gate on its kind.
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Node::Gate(a, _), Node::Gate(b, _)) => gate_matches(a, b),
            (
                Node::Op {
                    dialect: d1,
                    name: n1,
                    ty: t1,
                    attrs: a1,
                    ..
                },
                Node::Op {
                    dialect: d2,
                    name: n2,
                    ty: t2,
                    attrs: a2,
                    ..
                },
            ) => d1 == d2 && n1 == n2 && type_matches(*t1, *t2) && a1 == a2,
            (Node::Const { value: a, .. }, Node::Const { value: b, .. }) => {
                a.width() == b.width() && a.to_u64() == b.to_u64()
            }
            _ => false,
        }
    }

    fn from_int(value: tir_adt::APInt) -> Option<Self> {
        Some(Node::Const {
            value: APInt::new(value.width(), value.to_u64()),
            origin: None,
        })
    }
}

/// Result types are equal, or one side is the pattern wildcard.
fn type_matches(a: TypeId, b: TypeId) -> bool {
    a == b || a == any_type() || b == any_type()
}

/// A gate matches another of its kind (an input also on its `ValueId`); the `value`/`cond` payload is ignored.
fn gate_matches(a: &GateNode, b: &GateNode) -> bool {
    match (a, b) {
        (GateNode::Input(x), GateNode::Input(y)) => x == y,
        (GateNode::Gamma { .. }, GateNode::Gamma { .. }) => true,
        (GateNode::Mu { .. }, GateNode::Mu { .. }) => true,
        (GateNode::Phi { .. }, GateNode::Phi { .. }) => true,
        _ => false,
    }
}

fn hash_gate(gate: &GateNode, h: &mut impl Hasher) {
    match gate {
        GateNode::Input(v) => {
            0u8.hash(h);
            v.hash(h);
        }
        GateNode::Gamma { .. } => 1u8.hash(h),
        GateNode::Mu { .. } => 2u8.hash(h),
        GateNode::Phi { .. } => 3u8.hash(h),
        GateNode::Op(_) => unreachable!("an op is a Node::Op, never a Node::Gate"),
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
        // Register attributes are machine IR; InstCombine never sees them.
        AttributeValue::Register(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use tir_symbolic::egraph::{EGraph, Pattern, Var};

    use super::*;

    fn konst(width: u32, value: u64) -> Node {
        Node::Const {
            value: APInt::new(width, value),
            origin: None,
        }
    }

    fn ty(n: u32) -> TypeId {
        TypeId::from_number(n)
    }

    fn op(name: &'static str, ty: TypeId, attrs: Vec<NamedAttribute>, args: Vec<Id>) -> Node {
        Node::Op {
            dialect: "builtin",
            name,
            ty,
            attrs,
            commutative: false,
            cost: 1,
            prov: OpProv::Introduced(0),
            args,
        }
    }

    // Exercise the `ENode` hash-cons paths lit can't reach (`cmpi` and constant
    // width/bits identity); op result-type identity is covered by type_in_key.tir.

    #[test]
    fn equal_constants_share_a_class_distinct_widths_do_not() {
        let mut g: EGraph<Node> = EGraph::new();
        let a = g.add(konst(32, 0));
        let b = g.add(konst(32, 0));
        let c = g.add(konst(64, 0));
        assert_eq!(g.find(a), g.find(b));
        assert_ne!(g.find(a), g.find(c));
    }

    // Ops differing only in a value attribute must not hash-cons; identical ones must.
    #[test]
    fn ops_differing_in_attributes_stay_distinct() {
        let mut g: EGraph<Node> = EGraph::new();
        let x = g.add(konst(32, 0));
        let cmpi = |pred: &str, args: Vec<Id>| {
            let attrs = vec![NamedAttribute::new(
                "predicate",
                AttributeValue::Str(pred.to_string()),
            )];
            op("cmpi", ty(1), attrs, args)
        };
        let slt = g.add(cmpi("slt", vec![x]));
        let sgt = g.add(cmpi("sgt", vec![x]));
        let slt2 = g.add(cmpi("slt", vec![x]));
        assert_ne!(g.find(slt), g.find(sgt));
        assert_eq!(g.find(slt), g.find(slt2));
    }

    // `op_key` drops the result type, so `addi i32`/`addi i64` share a search bucket
    // (a wildcard visits both) yet must stay in distinct classes — merging would miscompile.
    #[test]
    fn wildcard_search_groups_result_types_without_merging_them() {
        let mut g: EGraph<Node> = EGraph::new();
        let x = g.add(konst(32, 0));
        let addi = |t: TypeId, args: Vec<Id>| op("addi", t, vec![], args);
        let a32 = g.add(addi(ty(32), vec![x, x]));
        let a64 = g.add(addi(ty(64), vec![x, x]));

        assert_ne!(g.find(a32), g.find(a64));
        assert_eq!(addi(ty(32), vec![]).op_key(), addi(ty(64), vec![]).op_key());

        let mut p: Pattern<Node, u32> = Pattern::new();
        let v0 = p.var(Var::Symbol(0));
        let v1 = p.var(Var::Symbol(1));
        p.add(addi(any_type(), vec![v0, v1]));
        let roots: std::collections::HashSet<Id> =
            p.search(&g).iter().map(|m| g.find(m.root)).collect();
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&g.find(a32)) && roots.contains(&g.find(a64)));

        // Searching is read-only: classes remain distinct.
        assert_ne!(g.find(a32), g.find(a64));
    }

    #[test]
    fn graph_operation_controls_commutative_matching() {
        let mut g: EGraph<Node> = EGraph::new();
        let zero = g.add(konst(32, 0));
        let x = g.add(konst(32, 1));
        let mut addi = op("addi", ty(32), vec![], vec![zero, x]);
        let Node::Op { commutative, .. } = &mut addi else {
            unreachable!()
        };
        *commutative = true;
        let root = g.add(addi);

        let mut pattern: Pattern<Node, u32> = Pattern::new();
        let variable = pattern.var(Var::Symbol(0));
        let literal = pattern.var(Var::Int(APInt::new(32, 0)));
        pattern.add(op("addi", any_type(), vec![], vec![variable, literal]));

        let matches = pattern.search(&g);
        assert_eq!(matches.len(), 1);
        assert_eq!(g.find(matches[0].root), g.find(root));
        assert_eq!(matches[0].subst.get(&Var::Symbol(0)), Some(g.find(x)));
    }
}
