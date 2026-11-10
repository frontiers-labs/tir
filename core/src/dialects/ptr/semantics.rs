//! Ordinary byte-sized scalar access contract. Cells contain byte8 and initialized1. Updates
//! preserve the ordinary readable/writable range domain; reads have no effect.
//! Mutable symbolic callbacks and MMIO do not implement this contract.

use crate::sem::{SemGraph, SymKind, con, op};
use crate::{Context, NodeId, ResourceSemantics, TypeId};

/// The concrete adapter and graph builder instantiate this same access recipe.
/// Adapter primitives, including range checks before effects, are trusted.
#[derive(Clone, Copy)]
pub(crate) enum Scalar {
    Add,
    And,
    Not,
    Extract,
    Concat,
    If,
}

pub(crate) trait Access {
    type Bits: Copy;
    type Error;
    fn constant(&mut self, value: u64, width: u32) -> Self::Bits;
    fn scalar(&mut self, kind: Scalar, args: &[Self::Bits]) -> Self::Bits;
    fn range(&mut self, address: Self::Bits, size: Self::Bits) -> Result<Self::Bits, Self::Error>;
    fn read(&mut self, address: Self::Bits) -> Self::Bits;
    fn write(&mut self, address: Self::Bits, cell: Self::Bits);
}

pub(crate) struct Loaded<B> {
    pub normal: B,
    pub value: B,
    pub poison: B,
}

pub(crate) fn load<C: Access>(
    c: &mut C,
    p: C::Bits,
    width: u32,
) -> Result<Loaded<C::Bits>, C::Error> {
    let size = c.constant(u64::from(width / 8), 64);
    let normal = c.range(p, size)?;
    let mut value = c.constant(0, width);
    let mut initialized = c.constant(1, 1);
    for i in 0..u64::from(width / 8) {
        let offset = c.constant(i, 64);
        let address = c.scalar(Scalar::Add, &[p, offset]);
        let cell = c.read(address);
        let high = c.constant(8, 64);
        let init = c.scalar(Scalar::Extract, &[cell, high, high]);
        initialized = c.scalar(Scalar::And, &[initialized, init]);
        let high = c.constant(7, 64);
        let low = c.constant(0, 64);
        let byte = c.scalar(Scalar::Extract, &[cell, high, low]);
        value = if i == 0 {
            byte
        } else {
            c.scalar(Scalar::Concat, &[byte, value])
        };
    }
    let poison = c.scalar(Scalar::Not, &[initialized]);
    Ok(Loaded {
        normal,
        value,
        poison,
    })
}

pub(crate) fn store<C: Access>(
    c: &mut C,
    p: C::Bits,
    value: C::Bits,
    poison: C::Bits,
    width: u32,
) -> Result<C::Bits, C::Error> {
    let size = c.constant(u64::from(width / 8), 64);
    let normal = c.range(p, size)?;
    let init = c.scalar(Scalar::Not, &[poison]);
    for i in 0..u64::from(width / 8) {
        let offset = c.constant(i, 64);
        let address = c.scalar(Scalar::Add, &[p, offset]);
        let old = c.read(address);
        let high = c.constant(7, 64);
        let low = c.constant(0, 64);
        let old = c.scalar(Scalar::Extract, &[old, high, low]);
        let high = c.constant(i * 8 + 7, 64);
        let low = c.constant(i * 8, 64);
        let byte = c.scalar(Scalar::Extract, &[value, high, low]);
        let byte = c.scalar(Scalar::If, &[poison, old, byte]);
        let cell = c.scalar(Scalar::Concat, &[init, byte]);
        c.write(address, cell);
    }
    Ok(normal)
}

pub(crate) struct Body {
    pub resources: ResourceSemantics,
    pub normal: NodeId,
    pub address_poison: NodeId,
    pub value_poison: NodeId,
}

struct Graph {
    graph: SemGraph,
    current: NodeId,
}
impl Access for Graph {
    type Bits = NodeId;
    type Error = std::convert::Infallible;
    fn constant(&mut self, v: u64, w: u32) -> NodeId {
        con(&mut self.graph, v, w)
    }
    fn scalar(&mut self, k: Scalar, a: &[NodeId]) -> NodeId {
        let k = match k {
            Scalar::Add => SymKind::Add,
            Scalar::And => SymKind::And,
            Scalar::Not => SymKind::Not,
            Scalar::Extract => SymKind::Extract,
            Scalar::Concat => SymKind::Concat,
            Scalar::If => SymKind::If,
        };
        op(&mut self.graph, k, a)
    }
    fn range(&mut self, p: NodeId, n: NodeId) -> Result<NodeId, Self::Error> {
        Ok(op(
            &mut self.graph,
            SymKind::MemoryRangeValid,
            &[self.current, p, n],
        ))
    }
    fn read(&mut self, p: NodeId) -> NodeId {
        op(&mut self.graph, SymKind::MemoryByteRead, &[self.current, p])
    }
    fn write(&mut self, p: NodeId, v: NodeId) {
        self.current = op(
            &mut self.graph,
            SymKind::MemoryByteWrite,
            &[self.current, p, v],
        );
    }
}

pub(crate) fn ordinary_width(context: &Context, ty: TypeId) -> Option<u32> {
    if let Some(integer) = crate::builtin::IntegerType::view(context, ty) {
        return match integer.width() {
            8 | 16 | 32 | 64 => Some(integer.width() as u32),
            _ => None,
        };
    }
    crate::ptr::PtrType::view(context, ty).map(|_| 64)
}

// Symbols 0/1/2 are memory/address64/address-poison1. Store alone also
// binds value/value-poison1 at 3/4. Every primitive sees the current state.
fn typed_symbol(graph: &mut SemGraph, id: u32, ty: TypeId) -> NodeId {
    let node = tir_symbolic::sem::sym(graph, id);
    graph.annotation_mut(node).actual_type = Some(ty);
    node
}

fn body(writing: bool, context: &Context, width: u32) -> Body {
    let mut graph = SemGraph::new();
    let state = typed_symbol(&mut graph, 0, TypeId::STATE);
    let p = typed_symbol(&mut graph, 1, context.terms().int(64));
    let address_poison = typed_symbol(&mut graph, 2, context.terms().int(1));
    let mut g = Graph {
        graph,
        current: state,
    };
    let (normal, values, value_poison) = if writing {
        let value = typed_symbol(&mut g.graph, 3, context.terms().int(u64::from(width)));
        let poison = typed_symbol(&mut g.graph, 4, context.terms().int(1));
        let Ok(normal) = store(&mut g, p, value, poison, width);
        (normal, vec![], poison)
    } else {
        let Ok(Loaded {
            normal,
            value,
            poison,
        }) = load(&mut g, p, width);
        let result = op(&mut g.graph, SymKind::Poison, &[poison, value]);
        (normal, vec![result], poison)
    };
    let state = g.current;
    let address_valid = op(&mut g.graph, SymKind::Not, &[address_poison]);
    let normal = op(&mut g.graph, SymKind::And, &[address_valid, normal]);
    Body {
        resources: ResourceSemantics {
            graph: g.graph,
            root: state,
            raised_flags: None,
            value_results: values,
            state_results: vec![state],
        },
        normal,
        address_poison,
        value_poison,
    }
}

pub(crate) fn load_memory_semantics(
    operation: &crate::OpHandle,
    context: &Context,
) -> Option<Body> {
    let width = ordinary_width(
        context,
        context.get_value(crate::ptr::load_result(operation)).ty(),
    )?;
    ordinary_contract(context, operation, false).then(|| body(false, context, width))
}

pub(crate) fn store_memory_semantics(
    operation: &crate::OpHandle,
    context: &Context,
) -> Option<Body> {
    let width = ordinary_width(context, context.get_value(operation.operands()[0]).ty())?;
    ordinary_contract(context, operation, true).then(|| body(true, context, width))
}

fn ordinary_contract(context: &Context, handle: &crate::OpHandle, writing: bool) -> bool {
    use crate::attributes::AttributeValue;
    let expected: &[u64] = if writing { &[1, 1, 1] } else { &[1, 1] };
    let operation = handle.clone().as_dyn_op();
    // Extra generic conditions are not part of this ordinary resource body.
    if operation.requires_expr(&mut SemGraph::new()).is_some()
        || operation.poison_expr(&mut SemGraph::new()).is_some()
    {
        return false;
    }
    let attrs = handle.attributes();
    // Port grouping is mandatory parser bookkeeping, not an access modifier.
    attrs.len() == 1
        && Some(attrs[0].name) == context.sym("operand_segment_sizes")
        && attrs[0].value
            == AttributeValue::Array(expected.iter().copied().map(AttributeValue::UInt).collect())
}
