//! The semantic-graph substrate shared by the IR and by TMDL codegen: the graph
//! shape semantic expressions are built into, its serialized wire format, and the
//! equivalence oracles that prove two graphs equal.
//!
//! The graph is parameterized by its per-node annotation so a consumer that has
//! no IR types (TMDL) can build the very same expressions the IR does.

use std::collections::HashMap;

use tir_graph::{Dag, MutDag, NodeId, PostOrderDag};

use crate::lang::{SymKind, SymPayload};

mod discover;
mod float;

pub use discover::{
    EquivalenceOracle, FuzzOracle, SmtOracle, con, confirm_bool_via_if,
    confirm_extension_via_shifts, op, sym,
};
pub use float::cmpf_semantics;

/// An SSA value's identity, as it appears in a semantic-graph leaf.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct ValueId(u32);

impl ValueId {
    pub fn number(&self) -> u32 {
        self.0
    }

    pub fn from_number(n: u32) -> Self {
        Self(n)
    }

    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// The post-order graph semantic expressions are built into: [`SymKind`] nodes
/// over `SymPayload<ValueId>` leaves, annotated with `A` so a node can carry
/// whatever provenance its producer tracks (the IR carries `tir::graph::NodeMeta`;
/// TMDL carries nothing).
pub type SemGraph<A = ()> = PostOrderDag<SymKind, SymPayload<ValueId>, A>;

/// What [`copy_subgraph_with`] makes of one source node.
pub enum CopyAction {
    /// The node and its payload, as they are.
    Keep,
    /// The node, carrying this payload instead of its own.
    Payload(SymPayload<ValueId>),
    /// This node of the destination, standing in for the source node and
    /// everything under it.
    Replace(NodeId),
}

/// Copy `node`'s subgraph from `src` into `dst`, preserving sharing through
/// `memo`. Children are copied first, keeping `dst` in post order.
pub fn copy_subgraph<A, B>(
    dst: &mut SemGraph<B>,
    src: &SemGraph<A>,
    node: NodeId,
    memo: &mut HashMap<usize, NodeId>,
) -> NodeId {
    copy_subgraph_with(dst, src, node, memo, &mut |_, _| CopyAction::Keep)
}

/// [`copy_subgraph`] with `act` deciding what each source node becomes. It runs
/// before the node's children are copied and receives `dst`, so a replacement
/// can be built there.
pub fn copy_subgraph_with<A, B>(
    dst: &mut SemGraph<B>,
    src: &SemGraph<A>,
    node: NodeId,
    memo: &mut HashMap<usize, NodeId>,
    act: &mut dyn FnMut(&mut SemGraph<B>, NodeId) -> CopyAction,
) -> NodeId {
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let payload = match act(dst, node) {
        CopyAction::Keep => src.get_leaf_data(node).cloned(),
        CopyAction::Payload(payload) => Some(payload),
        CopyAction::Replace(copied) => {
            memo.insert(node.index(), copied);
            return copied;
        }
    };
    let children: Vec<NodeId> = src.children(node).collect();
    let copied_children: Vec<NodeId> = children
        .into_iter()
        .map(|child| copy_subgraph_with(dst, src, child, memo, act))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(payload) = payload {
        dst.set_leaf_data(copied, payload);
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    memo.insert(node.index(), copied);
    copied
}

/// A payload literal in a serialized [`SemOp`] program; decoded to
/// [`SymPayload`] on replay.
#[derive(Clone, Copy, Debug)]
pub enum SemPayloadDesc {
    SymbolId(u32),
    Value(u32),
    Int {
        width: u32,
        value: u64,
        signed: bool,
    },
    Float(f64),
}

impl SemPayloadDesc {
    fn decode(self) -> SymPayload<ValueId> {
        match self {
            SemPayloadDesc::SymbolId(id) => SymPayload::SymbolId(id),
            SemPayloadDesc::Value(number) => SymPayload::Value(ValueId::from_number(number)),
            SemPayloadDesc::Int {
                width,
                value,
                signed,
            } => int_payload(width, value, signed),
            SemPayloadDesc::Float(value) => float_payload(value),
        }
    }
}

/// One step of a serialized semantic-graph construction. TMDL-generated
/// backends used to build every graph with one `add_node`/`add_edge` call per
/// statement; hundreds of thousands of such statements dominated rustc time on
/// the generated crates. Codegen therefore serializes these steps into a binary
/// blob ([`SemBlobBuilder`]) the generated crate embeds with `include_bytes!`,
/// so rustc sees one constant per crate instead of one per step.
/// `Payload`/`Typed` apply to the most recently added node; `Edge` indices count
/// nodes of the current program from 0.
#[derive(Clone, Copy, Debug)]
pub enum SemOp {
    Node(SymKind),
    Payload(SemPayloadDesc),
    Typed(u32),
    Edge(u32, u32),
}

fn replay_sem_ops<G, F>(g: &mut G, ops: &[SemOp], mut set_type: F) -> NodeId
where
    G: MutDag<Node = SymKind, Leaf = SymPayload<ValueId>>,
    F: FnMut(&mut G, NodeId, u32),
{
    let mut nodes: Vec<NodeId> = Vec::new();
    for op in ops {
        match *op {
            SemOp::Node(kind) => nodes.push(g.add_node(kind)),
            SemOp::Payload(desc) => {
                let node = *nodes.last().expect("payload before any node");
                g.set_leaf_data(node, desc.decode());
            }
            SemOp::Typed(width) => {
                let node = *nodes.last().expect("type before any node");
                set_type(g, node, width);
            }
            SemOp::Edge(parent, child) => {
                g.add_edge(nodes[parent as usize], nodes[child as usize]);
            }
        }
    }
    *nodes.last().expect("empty sem op array")
}

// ── Serialized sem programs ─────────────────────────────────────────────────

const SEM_BLOB_MAGIC: [u8; 4] = *b"TSEM";
const SEM_BLOB_VERSION: u8 = 1;

const TAG_NODE: u8 = 0;
const TAG_SYMBOL_ID: u8 = 1;
const TAG_VALUE: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_FLOAT: u8 = 4;
const TAG_TYPED: u8 = 5;
const TAG_EDGE: u8 = 6;

/// Serializes sem programs into one blob: a `TSEM` header followed by records,
/// each a byte length and that many bytes of tagged [`SemOp`]s. [`Self::intern`]
/// returns the offset a program was placed at, which is all the generated crate
/// embeds per program. Node kinds are indices into the [`Self::finish`] kind
/// table rather than raw discriminants, so the blob needs no stable numbering
/// for [`SymKind`].
///
/// Output is a pure function of the interned programs and their order: the maps
/// are only ever looked up in, never iterated.
pub struct SemBlobBuilder {
    bytes: Vec<u8>,
    kinds: Vec<SymKind>,
    records: std::collections::HashMap<Vec<u8>, u32>,
}

impl Default for SemBlobBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SemBlobBuilder {
    pub fn new() -> Self {
        let mut bytes = SEM_BLOB_MAGIC.to_vec();
        bytes.push(SEM_BLOB_VERSION);
        Self {
            bytes,
            kinds: Vec::new(),
            records: std::collections::HashMap::new(),
        }
    }

    /// Appends `ops` as a record, reusing an identical earlier one; returns the
    /// record's offset.
    pub fn intern(&mut self, ops: &[SemOp]) -> u32 {
        let mut body = Vec::new();
        for op in ops {
            self.encode_op(*op, &mut body);
        }
        if let Some(&existing) = self.records.get(&body) {
            return existing;
        }
        let offset = self.bytes.len() as u32;
        self.bytes
            .extend_from_slice(&(body.len() as u32).to_le_bytes());
        self.bytes.extend_from_slice(&body);
        self.records.insert(body, offset);
        offset
    }

    /// The blob and the kind table its node codes index.
    pub fn finish(self) -> (Vec<u8>, Vec<SymKind>) {
        (self.bytes, self.kinds)
    }

    fn encode_op(&mut self, op: SemOp, out: &mut Vec<u8>) {
        match op {
            SemOp::Node(kind) => {
                let code = self.kind_code(kind);
                out.push(TAG_NODE);
                out.extend_from_slice(&code.to_le_bytes());
            }
            SemOp::Payload(SemPayloadDesc::SymbolId(id)) => {
                out.push(TAG_SYMBOL_ID);
                out.extend_from_slice(&id.to_le_bytes());
            }
            SemOp::Payload(SemPayloadDesc::Value(number)) => {
                out.push(TAG_VALUE);
                out.extend_from_slice(&number.to_le_bytes());
            }
            SemOp::Payload(SemPayloadDesc::Int {
                width,
                value,
                signed,
            }) => {
                out.push(TAG_INT);
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&value.to_le_bytes());
                out.push(u8::from(signed));
            }
            SemOp::Payload(SemPayloadDesc::Float(value)) => {
                out.push(TAG_FLOAT);
                out.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            SemOp::Typed(width) => {
                out.push(TAG_TYPED);
                out.extend_from_slice(&width.to_le_bytes());
            }
            SemOp::Edge(parent, child) => {
                out.push(TAG_EDGE);
                out.extend_from_slice(&parent.to_le_bytes());
                out.extend_from_slice(&child.to_le_bytes());
            }
        }
    }

    fn kind_code(&mut self, kind: SymKind) -> u16 {
        match self.kinds.iter().position(|known| *known == kind) {
            Some(code) => code as u16,
            None => {
                self.kinds.push(kind);
                (self.kinds.len() - 1) as u16
            }
        }
    }
}

struct SemBlobReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl SemBlobReader<'_> {
    fn u8(&mut self) -> u8 {
        let value = self.bytes[self.pos];
        self.pos += 1;
        value
    }

    fn u16(&mut self) -> u16 {
        let value = u16::from_le_bytes(self.bytes[self.pos..self.pos + 2].try_into().unwrap());
        self.pos += 2;
        value
    }

    fn u32(&mut self) -> u32 {
        let value = u32::from_le_bytes(self.bytes[self.pos..self.pos + 4].try_into().unwrap());
        self.pos += 4;
        value
    }

    fn u64(&mut self) -> u64 {
        let value = u64::from_le_bytes(self.bytes[self.pos..self.pos + 8].try_into().unwrap());
        self.pos += 8;
        value
    }
}

/// Decodes the program at `offset` of a [`SemBlobBuilder`] blob; `kinds` is the
/// table returned alongside it.
pub fn decode_sem_ops(blob: &[u8], offset: u32, kinds: &[SymKind]) -> Vec<SemOp> {
    assert_eq!(blob[..4], SEM_BLOB_MAGIC, "not a sem blob");
    assert_eq!(blob[4], SEM_BLOB_VERSION, "unsupported sem blob version");
    let mut reader = SemBlobReader {
        bytes: blob,
        pos: offset as usize,
    };
    let end = reader.pos + 4 + reader.u32() as usize;
    let mut ops = Vec::new();
    while reader.pos < end {
        ops.push(match reader.u8() {
            TAG_NODE => SemOp::Node(kinds[reader.u16() as usize]),
            TAG_SYMBOL_ID => SemOp::Payload(SemPayloadDesc::SymbolId(reader.u32())),
            TAG_VALUE => SemOp::Payload(SemPayloadDesc::Value(reader.u32())),
            TAG_INT => SemOp::Payload(SemPayloadDesc::Int {
                width: reader.u32(),
                value: reader.u64(),
                signed: reader.u8() != 0,
            }),
            TAG_FLOAT => SemOp::Payload(SemPayloadDesc::Float(f64::from_bits(reader.u64()))),
            TAG_TYPED => SemOp::Typed(reader.u32()),
            TAG_EDGE => SemOp::Edge(reader.u32(), reader.u32()),
            tag => unreachable!("unknown sem op tag {tag}"),
        });
    }
    ops
}

/// Replays the program at `offset` into any semantic-graph sink; returns the
/// root (the last node, as programs are serialized in post order).
pub trait ExtendSemBytes: MutDag<Node = SymKind, Leaf = SymPayload<ValueId>> + Sized {
    fn extend_sem_bytes(&mut self, kinds: &[SymKind], blob: &[u8], offset: u32) -> NodeId {
        self.extend_sem_bytes_with(kinds, blob, offset, |_, _, _| {
            unreachable!("SemOp::Typed requires a type sink")
        })
    }

    /// [`Self::extend_sem_bytes`] with a sink for [`SemOp::Typed`] widths, so a
    /// graph that can name types resolves them however it interns them.
    fn extend_sem_bytes_with(
        &mut self,
        kinds: &[SymKind],
        blob: &[u8],
        offset: u32,
        set_type: impl FnMut(&mut Self, NodeId, u32),
    ) -> NodeId {
        replay_sem_ops(self, &decode_sem_ops(blob, offset, kinds), set_type)
    }
}

impl<G: MutDag<Node = SymKind, Leaf = SymPayload<ValueId>>> ExtendSemBytes for G {}

// ── APInt boundary helpers ──────────────────────────────────────────────────
//
// These let TMDL-generated backend code construct sem payloads without naming
// `tir-adt` directly.

/// An integer payload literal for graph construction (`signed` picks the
/// constructor); used by TMDL codegen in place of a raw `APInt`.
pub fn int_payload(width: u32, value: u64, signed: bool) -> SymPayload<ValueId> {
    let v = if signed {
        tir_adt::APInt::new_signed(width, value as i64)
    } else {
        tir_adt::APInt::new(width, value)
    };
    SymPayload::Int(v)
}

/// A float payload literal for graph construction.
pub fn float_payload(value: f64) -> SymPayload<ValueId> {
    SymPayload::Float(tir_adt::APFloat::from_f64(value))
}
