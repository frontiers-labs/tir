//! Compilation of rule semantic expressions into matchable patterns.

use super::cover::BoundaryDemand;
use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;
use tir::{
    Context, NodeId,
    sem::{
        Prov, SemGraph, SemNode, SemType, SymKind, SymPayload, TypeUnifier, Width,
        egraph::{SemEGraph, class_int_binding, class_semantic_type},
        infer_types, template_node,
    },
};
use tir_adt::APInt;
use tir_relational::ClassId as Id;
use tir_relational::{
    Atom, Cmp, ColumnId, Expr, Guard, Match, NoExterns, Plan, Query, Ref, Scalar, Source,
};

use super::node::{class_register_type, is_memory_kind};
use super::{ImmRange, OperandConstraint, RegisterRequirement};

/// One node of a rule's pattern: a template operator, or an operand the rule
/// names.
#[derive(Clone)]
pub(crate) enum PatternNode {
    Template(SemNode),
    Capture(u32),
    Wildcard,
}

impl PatternNode {
    pub(crate) fn symbol(&self) -> Option<u32> {
        match self {
            PatternNode::Capture(symbol) => Some(*symbol),
            PatternNode::Template(_) | PatternNode::Wildcard => None,
        }
    }
}

/// One match of a pattern: the value it defines and the value every pattern
/// node binds, an immediate included.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    pub(crate) root: Ref,
    pub(crate) bindings: SmallVec<[Ref; 8]>,
}

/// An `Add` or `Sub` node whose one operand is an immediate: the value it
/// matches is its base plus or minus that immediate, read straight off the
/// value's reference rather than off a row. The query binds the base at the
/// class's offset zero and the whole offset as a scalar; completing the match
/// gives the immediate what its field holds.
#[derive(Clone, Copy, Debug)]
struct OffsetSite {
    key: u32,
    base: u32,
    immediate: u32,
    scalar: Scalar,
    /// The immediate's offset coefficient: one for an addend, minus one for a
    /// subtrahend.
    sign: i64,
    /// Whether the base is a register operand read nowhere else, so a part of
    /// the offset the field cannot hold can move to the base instead.
    split: bool,
}

/// An interior operator with offset coefficient one at a register operand:
/// a row of it at one offset of a class also matches the class at another,
/// the difference moving into that operand. `(b + i * 4) + 8` is
/// `(b + 8) + i * 4`, so a scaled access reads `b + 8` in a register.
#[derive(Clone, Copy, Debug)]
struct Absorb {
    operand: u32,
    scalar: Scalar,
}

/// Where a pattern's matches come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rooting {
    /// A row of its root operator: the function-wide search finds them.
    Row,
    /// The root is an offset split ([`OffsetSite`]): a value at a non-zero
    /// offset from a class is matched where it is demanded.
    Offset,
    /// A bare immediate: a constant its field can hold.
    Immediate,
}

/// What a pattern with one immediate and at most one register operand is,
/// evaluated over constants: how a constant root is matched where no row
/// spells it.
#[derive(Clone, Copy, Debug)]
struct Constant {
    immediate: u32,
    register: Option<u32>,
}

/// A rule's pattern compiled for matching: the query it is, the nodes it was
/// written as, and the per-node metadata the cover consults.
pub(crate) struct CompiledIselPattern {
    pub(crate) rule_index: usize,
    /// Which of the rule's results the pattern is the semantics of.
    pub(crate) port: usize,
    /// One entry per pattern node; a match binds one value per entry.
    pub(crate) nodes: Vec<PatternNode>,
    root: u32,
    plan: Plan<SemNode>,
    rooting: Rooting,
    offsets: Vec<OffsetSite>,
    absorbs: Vec<Absorb>,
    /// The two register operands of a root `Add`: a value at a non-zero offset
    /// is its class plus the offset materialized in a register.
    register_offset: Option<(u32, u32)>,
    /// The register operands of a root operator with offset coefficient one,
    /// each read once: a row at one offset of a class also matches the class
    /// at another, the difference moving into that operand.
    shifts: SmallVec<[u32; 2]>,
    constant: Option<Constant>,
    /// The capture nodes in the order a match reaches them.
    pub(crate) captures: Vec<u32>,
    /// Matching metadata for each pattern node (indexed by pattern node id).
    pub(crate) node_meta: Vec<PatternNodeMeta>,
    /// Number of type-constrained pattern nodes — how "specific" this pattern is.
    /// At equal instruction cost, a more specific match is preferred, so an i32
    /// `addw` (one typed node) beats the untyped `add` for an i32 value, while the
    /// untyped `add`/`and` still match every other width.
    pub(crate) specificity: usize,
    result_register: Option<RegisterRequirement>,
    copy: bool,
}

/// Per-pattern-node matching metadata.
#[derive(Clone, Default)]
pub(crate) struct PatternNodeMeta {
    /// An operand capture point (a `Var::Symbol` leaf).
    pub(crate) is_boundary: bool,
    /// The state operand appended to a memory node ([`memory_state_symbol`]):
    /// matched to name the chain the access reads, and nothing else. It is not
    /// an operand — no register, no immediate, no legality of its own — so the
    /// cover reads the chain off it without demanding anything for it.
    pub(crate) is_state: bool,
    /// A constant template: pure, folded into the encoding, never consumed by
    /// the match — boundary-like for the cover.
    pub(crate) is_constant: bool,
    /// Whether any number of matches may embed this node's value (operands,
    /// constants, and offset splits).
    pub(crate) duplicable: bool,
    pub(crate) constraint: Option<OperandConstraint>,
    /// Storage capability and bit demand of a physical register operand.
    pub(crate) register: Option<RegisterRequirement>,
    /// Encoding range of an immediate operand (see `Rule::operand_imm_ranges`).
    pub(crate) imm_range: Option<ImmRange>,
    /// The symbolic value type inferred from the semantic operator signatures.
    pub(crate) semantic_type: Option<SemType>,
    declared_type: Option<tir::TypeId>,
    /// What a match demands of the class bound here; meaningful for boundary
    /// and constant nodes.
    pub(crate) demand: BoundaryDemand,
}

impl PatternNodeMeta {
    /// Boundary-like for the cover: an operand capture, or a constant that is
    /// pure, folded into the encoding and never consumed by the match — so one
    /// constant class can sit inside one match and under a boundary of another.
    pub(crate) fn boundary_like(&self) -> bool {
        self.is_boundary || self.is_constant
    }

    /// Where this operand's register class views its storage element.
    pub(crate) fn view_offset(&self) -> u32 {
        self.register
            .map_or(0, |requirement| requirement.view_offset())
    }

    /// The width this operand reads its register at, when it reads it whole
    /// ([`RegisterRequirement::whole_width`]).
    pub(crate) fn whole_width(&self) -> Option<u32> {
        self.register
            .and_then(|requirement| requirement.whole_width())
    }

    /// The node demands its class in a register: a physical-register operand or
    /// an explicit register constraint.
    pub(crate) fn demands_register(&self) -> bool {
        self.register.is_some() || self.constraint == Some(OperandConstraint::Register)
    }
}

impl CompiledIselPattern {
    pub(crate) fn is_copy(&self) -> bool {
        self.copy
    }

    /// The pattern node the match roots on.
    pub(crate) fn root(&self) -> usize {
        self.root as usize
    }

    pub(crate) fn constant_materializer_range(&self) -> Option<ImmRange> {
        self.nodes[self.root as usize].symbol()?;
        let meta = &self.node_meta[self.root as usize];
        (meta.constraint == Some(OperandConstraint::Immediate))
            .then_some(meta.imm_range)
            .flatten()
    }

    pub(crate) fn rooting(&self) -> Rooting {
        self.rooting
    }

    /// Whether the pattern matches a constant by evaluation
    /// ([`Self::solve_constant`]).
    pub(crate) fn solves_constants(&self) -> bool {
        self.constant.is_some()
    }

    /// Whether the pattern produces a constant from its immediate alone.
    pub(crate) fn materializes_alone(&self) -> bool {
        self.rooting == Rooting::Immediate
            || self
                .constant
                .is_some_and(|constant| constant.register.is_none())
    }

    /// Whether the constant the pattern evaluates to needs a register operand
    /// as well as its immediate.
    pub(crate) fn reads_register_operand(&self) -> bool {
        self.constant
            .is_some_and(|constant| constant.register.is_some())
    }

    /// The matches at `value` that `found`, a match at a row of the same class
    /// at another offset, makes once the difference moves into an operand
    /// the root's operator adds: `sub(a, b) + k` is `sub(a + k, b)`.
    pub(crate) fn shifted(&self, egraph: &SemEGraph, found: &Found, value: Ref) -> Vec<Found> {
        let Some(width) = egraph.width(value.class) else {
            return Vec::new();
        };
        let difference = value.offset.wrapping_sub(found.root.offset) & mask(width);
        if found.root.class != value.class || difference == 0 || egraph.int_const(value).is_some() {
            return Vec::new();
        }
        self.shifts
            .iter()
            .filter_map(|&operand| {
                let read = found.bindings[operand as usize];
                // An operand of a class without offsets cannot take one.
                let read_width = egraph.width(read.class)?;
                let shifted = egraph.find(Ref::new(
                    read.class,
                    read.offset.wrapping_add(difference) & mask(read_width),
                ));
                // One landing on the value itself makes no progress.
                if shifted == value {
                    return None;
                }
                let mut bindings = found.bindings.clone();
                bindings[operand as usize] = shifted;
                bindings[self.root as usize] = value;
                Some(Found {
                    root: value,
                    bindings,
                })
            })
            .collect()
    }

    /// Whether a row of the pattern's root at one offset also matches its
    /// class at another ([`Self::shifted`]).
    pub(crate) fn shifts(&self) -> bool {
        !self.shifts.is_empty()
    }

    pub(crate) fn has_register_offset(&self) -> bool {
        self.register_offset.is_some()
    }

    fn match_types(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        found: &Found,
        pointer_width: Option<u32>,
    ) -> bool {
        let mut unifier = TypeUnifier::default();
        let nodes_match = self.node_meta.iter().enumerate().all(|(index, meta)| {
            if meta.is_boundary {
                return true;
            }
            let Some(expected) = &meta.semantic_type else {
                return true;
            };
            class_semantic_type(ctx, egraph, found.bindings[index].class)
                .is_none_or(|actual| unifier.unify(expected, &actual).is_ok())
        });
        // The root check must see pointer-typed classes at the data layout's
        // pointer width: `class_semantic_type` has no answer for them, and
        // accepting on `None` let an 8-bit immediate move claim a 64-bit
        // pointer constant.
        nodes_match
            && self.result_register.is_none_or(|register| {
                class_register_type(ctx, egraph, found.root.class, pointer_width)
                    .is_none_or(|actual| register.accepts(&actual))
            })
    }

    /// Whether `symbol` names a state operand this compilation appended to a
    /// memory node rather than an operand the rule declares. A chain is matched
    /// to name the access, never handed to an emitter.
    pub(crate) fn is_state_symbol(&self, symbol: u32) -> bool {
        (0..self.nodes.len()).any(|index| {
            self.node_meta[index].is_state && self.nodes[index].symbol() == Some(symbol)
        })
    }

    /// The operand symbols the pattern reads as registers.
    pub(crate) fn register_symbols(&self) -> HashSet<u32> {
        (0..self.nodes.len())
            .filter_map(|index| {
                let symbol = self.nodes[index].symbol()?;
                self.node_meta[index].demands_register().then_some(symbol)
            })
            .collect()
    }

    /// Whether `class` may bind under `pattern_node`: a width requirement rejects
    /// a value *known* to be of a different width than the instruction operates
    /// at. What a constant must be for an immediate is a question about a
    /// value, not a class, and [`Self::immediate_ok`] answers it once the match
    /// is complete. Register constraints are checked by the cover: a constant
    /// may bind here only if a selected materializer makes it available in a
    /// register.
    ///
    /// A rewrite-introduced class carries no width of its own, so nothing is
    /// known to reject here. What pins it is the cover, which lets an operand
    /// read whole meet only a tile defining exactly the bits it reads
    /// ([`super::cover::alternatives_compatible`]).
    pub(crate) fn boundary_ok(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        pattern_node: Id,
        class: Id,
        pointer_width: Option<u32>,
    ) -> bool {
        // A copy rule's query binds variables past the pattern's own nodes: the
        // view it roots on, and the literals fixing that view's bounds. The
        // pattern names none of them, so none of them carries an operand
        // constraint. Its one named operand is not checked here either: it is
        // read *through* the view, so the requirement that applies is
        // `accepts_low_view_source` rather than plain acceptance, and
        // `reads_the_source` is where that lives.
        if self.copy {
            return true;
        }
        let Some(meta) = self.node_meta.get(pattern_node.index()) else {
            return true;
        };
        if let Some(expected) = meta
            .declared_type
            .and_then(|ty| tir::sem::egraph::semantic_type(ctx, ty))
            && let Some(actual) = class_semantic_type(ctx, egraph, class)
            && TypeUnifier::default().unify(&expected, &actual).is_err()
        {
            return false;
        }
        if let Some(required) = meta.register
            && let Some(actual) = class_register_type(ctx, egraph, class, pointer_width)
            && !required.accepts(&actual)
        {
            return false;
        }
        true
    }

    /// Whether `value` may bind under `pattern_node` as far as its being a
    /// constant goes: an immediate needs one its encoding field represents, in
    /// the representative [`ImmRange::contains`] reads.
    fn immediate_ok(&self, egraph: &SemEGraph, pattern_node: usize, value: Ref) -> bool {
        let meta = &self.node_meta[pattern_node];
        if meta.constraint != Some(OperandConstraint::Immediate) && meta.imm_range.is_none() {
            return true;
        }
        let Some(constant) = immediate(egraph, value) else {
            return false;
        };
        meta.imm_range.is_none_or(|range| range.contains(&constant))
    }

    /// The classes the pattern's rows can root at: those holding its root
    /// operator. Only a [`Rooting::Row`] pattern has any.
    pub(crate) fn roots(&self, egraph: &SemEGraph) -> Vec<Id> {
        match self.rooting {
            Rooting::Row => self.plan.roots(egraph),
            Rooting::Offset | Rooting::Immediate => Vec::new(),
        }
    }

    /// The matches rooted at rows of `roots`, each at the value its row is.
    pub(crate) fn search_rows(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        roots: impl IntoIterator<Item = Id>,
        pointer_width: Option<u32>,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<Found> {
        if self.rooting != Rooting::Row {
            return Vec::new();
        }
        let allowed = |var: u32, class: Id| allowed(Id::from_raw(var), class);
        self.plan
            .search(egraph, roots, &allowed, false, &NoExterns)
            .into_iter()
            .flat_map(|matched| self.complete(egraph, ctx, &matched, None, pointer_width))
            .collect()
    }

    /// The matches at `value` no row roots: an offset split of it, a constant
    /// a bare immediate holds, or a constant the pattern evaluates to.
    pub(crate) fn search_value(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        value: Ref,
        pointer_width: Option<u32>,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<Found> {
        let value = egraph.find(value);
        let constant = egraph.int_const(value).is_some();
        let mut found = Vec::new();
        match self.rooting {
            Rooting::Immediate if constant => {
                let mut bindings: SmallVec<[Ref; 8]> = SmallVec::from_elem(value, self.nodes.len());
                bindings[self.root as usize] = value;
                let candidate = Found {
                    root: value,
                    bindings,
                };
                if self.immediate_ok(egraph, self.root as usize, value)
                    && self.match_types(egraph, ctx, &candidate, pointer_width)
                {
                    found.push(candidate);
                }
            }
            // A value at offset zero is its rows; a constant is matched by
            // evaluation below.
            Rooting::Offset if value.offset != 0 && !constant => {
                let allowed = |var: u32, class: Id| allowed(Id::from_raw(var), class);
                found.extend(
                    self.plan
                        .search(egraph, [value.class], &allowed, false, &NoExterns)
                        .into_iter()
                        .flat_map(|matched| {
                            self.complete(egraph, ctx, &matched, Some(value), pointer_width)
                        }),
                );
            }
            _ => {}
        }
        if let Some((base, addend)) = self.register_offset
            && value.offset != 0
            && !constant
            && self.typed_like(egraph, self.root, value.class)
            && let Some(width) = egraph.width(value.class)
            && let Some(addend_value) = zero(egraph, width).map(|zero| Ref::new(zero, value.offset))
        {
            let mut bindings: SmallVec<[Ref; 8]> = SmallVec::from_elem(value, self.nodes.len());
            bindings[base as usize] = Ref::from(value.class);
            bindings[addend as usize] = addend_value;
            let candidate = Found {
                root: value,
                bindings,
            };
            if self.match_types(egraph, ctx, &candidate, pointer_width) {
                found.push(candidate);
            }
        }
        found
    }

    /// The matches `matched` makes, its immediates bound to the constants
    /// their fields hold; none when one cannot be. A match of an
    /// [`Rooting::Offset`] pattern is completed at `root`, the value demanded
    /// of the class it was searched at.
    fn complete(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        matched: &Match,
        root: Option<Ref>,
        pointer_width: Option<u32>,
    ) -> SmallVec<[Found; 2]> {
        let mut bindings: SmallVec<[Ref; 8]> = (0..matched.bindings.len() as u32)
            .map(|var| {
                matched
                    .binding(var)
                    .map_or(UNSET, |value| egraph.find(value))
            })
            .collect();
        let mut scalars = matched.scalars.clone();
        if let Some(root) = root {
            bindings[self.root as usize] = root;
            if let Some(site) = self.offsets.iter().find(|site| site.key == self.root) {
                scalars[site.scalar as usize] = root
                    .offset
                    .wrapping_sub(bindings[site.base as usize].offset);
            }
        }
        let mut candidates: SmallVec<[SmallVec<[Ref; 8]>; 2]> = smallvec::smallvec![bindings];
        for site in &self.offsets {
            let mut next = SmallVec::new();
            for bindings in candidates {
                let key = bindings[site.key as usize];
                for part in self.offset_parts(egraph, site, key, scalars[site.scalar as usize]) {
                    let width = offset_width(egraph, key);
                    let mask = mask(width);
                    let Some(zero) = zero(egraph, width) else {
                        continue;
                    };
                    let mut bindings = bindings.clone();
                    let immediate = Ref::new(zero, part);
                    // An immediate an earlier split bound holds one value.
                    if ![UNSET, immediate].contains(&bindings[site.immediate as usize]) {
                        continue;
                    }
                    bindings[site.immediate as usize] = immediate;
                    // What the immediate does not hold, the base does.
                    let contribution =
                        (site.sign as u64).wrapping_mul(scalars[site.scalar as usize]) & mask;
                    let rest = (site.sign as u64).wrapping_mul(contribution.wrapping_sub(part));
                    let base = bindings[site.base as usize];
                    bindings[site.base as usize] =
                        egraph.find(Ref::new(base.class, base.offset.wrapping_add(rest) & mask));
                    next.push(bindings);
                }
            }
            candidates = next;
        }
        for absorb in &self.absorbs {
            let offset = scalars[absorb.scalar as usize];
            if offset == 0 {
                continue;
            }
            candidates.retain_mut(|bindings| {
                let read = bindings[absorb.operand as usize];
                let Some(width) = egraph.width(read.class) else {
                    return false;
                };
                bindings[absorb.operand as usize] = egraph.find(Ref::new(
                    read.class,
                    read.offset.wrapping_add(offset) & mask(width),
                ));
                true
            });
        }
        candidates
            .into_iter()
            .filter(|bindings| {
                !bindings.contains(&UNSET)
                    && (0..self.nodes.len())
                        .all(|index| self.immediate_ok(egraph, index, bindings[index]))
            })
            .map(|bindings| Found {
                root: bindings[self.plan.query().root as usize],
                bindings,
            })
            .filter(|found| {
                if self.copy {
                    self.reads_the_source(egraph, ctx, found)
                } else {
                    self.match_types(egraph, ctx, found, pointer_width)
                }
            })
            .collect()
    }

    /// The constants the immediate of `site` may hold of the value at `key`,
    /// `offset` past the base the query bound: the whole contribution if its
    /// field represents it, and otherwise, when the base can take the rest,
    /// the parts [`field_parts`] reads off the field.
    fn offset_parts(
        &self,
        egraph: &SemEGraph,
        site: &OffsetSite,
        key: Ref,
        offset: u64,
    ) -> SmallVec<[u64; 2]> {
        if !self.typed_like(egraph, site.key, key.class) {
            return SmallVec::new();
        }
        let width = offset_width(egraph, key);
        let mask = mask(width);
        let contribution = (site.sign as u64).wrapping_mul(offset) & mask;
        match &self.nodes[site.immediate as usize] {
            PatternNode::Template(constant) => constant
                .int()
                .map(|value| value.to_u64() & mask)
                .filter(|&value| value == contribution)
                .into_iter()
                .collect(),
            _ => {
                let range = self.node_meta[site.immediate as usize].imm_range;
                let fits = range.is_none_or(|range| {
                    range.contains(&signed(width, contribution))
                        && !(range.nonzero && contribution == 0)
                });
                match range {
                    _ if fits => smallvec::smallvec![contribution],
                    Some(range) if site.split => field_parts(range, width, contribution),
                    _ => SmallVec::new(),
                }
            }
        }
    }

    /// The match at a constant `value` the pattern evaluates to, its
    /// immediate holding what its field can of the constant and its register
    /// operand, if it has one, demanded as the constant the rest comes to.
    ///
    /// The candidate is read off the field: where the immediate scales into
    /// the result (an add, an inserted halfword, a shifted upper part) it
    /// takes the bits of the constant at that scale, and where it does not
    /// (a shift amount) it takes the constant's trailing zeros. Either way the
    /// candidate is kept only if evaluating the pattern over it gives back the
    /// constant, so what the cover selects computes exactly that value.
    pub(crate) fn solve_constant(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        value: Ref,
        pointer_width: Option<u32>,
    ) -> SmallVec<[Found; 2]> {
        let mut found = SmallVec::new();
        let Some(shape) = self.constant else {
            return found;
        };
        let value = egraph.find(value);
        let (Some(constant), Some(width)) = (egraph.int_const(value), egraph.width(value.class))
        else {
            return found;
        };
        if self
            .width_of(self.root as usize)
            .is_some_and(|root| root != width)
            || !self.typed_like(egraph, self.root, value.class)
        {
            return found;
        }
        let Some(zero) = zero(egraph, width) else {
            return found;
        };
        let mask = mask(width);
        let evaluate = |immediate: u64, register: u64| {
            let leaves = [
                (shape.immediate, immediate),
                (shape.register.unwrap_or(u32::MAX), register),
            ];
            self.evaluate(self.root as usize, width, &leaves)
                .map(|(result, _)| result & mask)
        };
        let (Some(base), Some(one)) = (evaluate(0, 0), evaluate(1, 0)) else {
            return found;
        };
        let range = self.node_meta[shape.immediate as usize].imm_range;
        let scale = one.wrapping_sub(base) & mask;
        let negated = scale.wrapping_neg() & mask;
        let mut candidates: SmallVec<[(u64, Option<u64>); 2]> = SmallVec::new();
        if scale.is_power_of_two() || negated.is_power_of_two() {
            let raw = if scale.is_power_of_two() {
                (constant.wrapping_sub(base) & mask) >> scale.trailing_zeros()
            } else {
                (base.wrapping_sub(constant) & mask) >> negated.trailing_zeros()
            };
            let immediates = match range {
                None => smallvec::smallvec![raw],
                Some(range) if range.contains(&signed(width, raw)) => smallvec::smallvec![raw],
                // An immediate at the low bits may also leave its base what
                // its field can hold at its other extreme.
                Some(range) if scale == 1 || negated == 1 => field_parts(range, width, raw),
                Some(range) => field_parts(range, width, raw).into_iter().take(1).collect(),
            };
            for immediate in immediates {
                let Some(part) = evaluate(immediate, 0) else {
                    continue;
                };
                let register = shape
                    .register
                    .map(|_| constant.wrapping_sub(part.wrapping_sub(base)) & mask);
                candidates.push((immediate, register));
            }
        } else if shape.register.is_some() && constant != 0 {
            // The immediate does not scale into the result: a shift amount,
            // which takes the trailing zeros the constant has.
            let most = range.map_or(u64::from(width) - 1, |range| mask_bits(range.width));
            let shift = u64::from(constant.trailing_zeros())
                .min(most)
                .min(u64::from(width) - 1);
            if shift != 0 {
                let register = (signed(width, constant).to_i64() >> shift) as u64 & mask;
                candidates.push((shift, Some(register)));
            }
        }
        for (immediate, register) in candidates {
            if evaluate(immediate, register.unwrap_or(0)) != Some(constant)
                || register == Some(constant)
            {
                continue;
            }
            let mut bindings: SmallVec<[Ref; 8]> = SmallVec::from_elem(value, self.nodes.len());
            bindings[shape.immediate as usize] = Ref::new(zero, immediate);
            if let (Some(var), Some(register)) = (shape.register, register) {
                bindings[var as usize] = Ref::new(zero, register);
            }
            let candidate = Found {
                root: value,
                bindings,
            };
            if self.immediate_ok(
                egraph,
                shape.immediate as usize,
                candidate.bindings[shape.immediate as usize],
            ) && self.match_types(egraph, ctx, &candidate, pointer_width)
            {
                found.push(candidate);
            }
        }
        found
    }

    /// Whether `class` has the type template `node` names, if it names one: a
    /// typed operator matched without a row matches a value of exactly its
    /// type, as its row would.
    fn typed_like(&self, egraph: &SemEGraph, node: u32, class: Id) -> bool {
        match &self.nodes[node as usize] {
            PatternNode::Template(template) => template.ty.is_none_or(|ty| {
                egraph.fact(ColumnId::Type, class) == Some(u64::from(ty.number()))
            }),
            _ => true,
        }
    }

    /// The width a pattern node's inferred type fixes, if it fixes one.
    fn width_of(&self, node: usize) -> Option<u32> {
        match &self.node_meta[node].semantic_type {
            Some(SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width))) => {
                Some(*width)
            }
            _ => None,
        }
    }

    /// The value pattern node `node` computes over constants, and its width:
    /// `leaves` gives the operands, and a node whose type fixes no width is
    /// `default` bits wide. `None` for an operator the evaluation does not
    /// know.
    fn evaluate(&self, node: usize, default: u32, leaves: &[(u32, u64)]) -> Option<(u64, u32)> {
        let width = self.width_of(node).unwrap_or(default);
        let template = match &self.nodes[node] {
            PatternNode::Capture(_) => {
                let value = leaves.iter().find(|&&(leaf, _)| leaf as usize == node)?.1;
                return Some((value & mask(width), width));
            }
            PatternNode::Wildcard => return None,
            PatternNode::Template(template) => template,
        };
        let kind = template.sym()?;
        if kind == SymKind::Constant {
            let value = template.int()?;
            return Some((value.to_u64(), value.width()));
        }
        let operand =
            |at: usize| self.evaluate(template.children.get(at)?.index(), default, leaves);
        let sign = |(value, width): (u64, u32)| signed(width, value).to_i64();
        let (a, b) = (operand(0), operand(1));
        let shift = |amount: u64, apply: &dyn Fn(u32) -> u64| -> u64 {
            if amount >= u64::from(width) {
                0
            } else {
                apply(amount as u32)
            }
        };
        let value = match kind {
            SymKind::Add => a?.0.wrapping_add(b?.0),
            SymKind::Sub => a?.0.wrapping_sub(b?.0),
            SymKind::Mul => a?.0.wrapping_mul(b?.0),
            SymKind::And => a?.0 & b?.0,
            SymKind::Or => a?.0 | b?.0,
            SymKind::Xor => a?.0 ^ b?.0,
            SymKind::Not => !a?.0,
            SymKind::Neg => a?.0.wrapping_neg(),
            SymKind::ShiftLeft => shift(b?.0, &|amount| a.unwrap().0 << amount),
            SymKind::ShiftRightLogic => shift(b?.0, &|amount| a.unwrap().0 >> amount),
            SymKind::ShiftRightArithmetic => {
                let amount = b?.0.min(63) as u32;
                (sign(a?) >> amount) as u64
            }
            SymKind::ZExt => a?.0,
            SymKind::SExt => sign(a?) as u64,
            SymKind::Extract => {
                let (hi, lo) = (b?.0, operand(2)?.0);
                if hi < lo || hi >= 64 {
                    return None;
                }
                let bits = (hi - lo + 1) as u32;
                return Some(((a?.0 >> lo) & mask(bits), bits));
            }
            _ => return None,
        };
        Some((value & mask(width), width))
    }

    /// Whether a copy rule may read the class its view is a truncation of. The
    /// query has already found the view and matched the width; what is left is
    /// the operand's own storage requirement, which is where every other
    /// pattern's type checking also lives.
    fn reads_the_source(&self, egraph: &SemEGraph, ctx: &Context, found: &Found) -> bool {
        let source = found.bindings[self.root as usize].class;
        let Some(source_ty) = class_semantic_type(ctx, egraph, source) else {
            return false;
        };
        self.node_meta[self.root as usize]
            .register
            .is_none_or(|register| register.accepts_low_view_source(&source_ty))
    }
}

/// What a compiled pattern node is shared by. A term is shared by the node that
/// spells it; an operand is shared by the symbol it names, so a rule reading one
/// operand twice matches one class rather than two unrelated ones.
#[derive(PartialEq, Eq, Hash)]
enum Shared {
    Term(NodeId),
    Operand(u32),
}

pub(crate) fn compile_isel_pattern(
    rule_index: usize,
    port: usize,
    expr: &SemGraph,
    operand_constraints: &[(u32, OperandConstraint)],
    operand_registers: &[(u32, RegisterRequirement)],
    operand_imm_ranges: &[(u32, ImmRange)],
    result_register: Option<RegisterRequirement>,
) -> Option<CompiledIselPattern> {
    let root = canonical_pattern_root(expr, expr.root()?);
    if operand_registers
        .iter()
        .any(|(_, requirement)| requirement.exclusive_float_type().is_err())
    {
        return None;
    }
    let inferred_types = infer_types(expr, |node| {
        let Some(SymPayload::SymbolId(symbol)) = expr.get_leaf_data(node) else {
            return None;
        };
        operand_registers
            .iter()
            .find(|(candidate, _)| candidate == symbol)
            .and_then(|(_, requirement)| {
                requirement
                    .exclusive_float_type()
                    .expect("float register formats were validated")
            })
    })
    .ok()?;
    let mut nodes: Vec<PatternNode> = Vec::new();
    let mut node_meta = Vec::new();
    let mut memo = HashMap::new();
    let pattern_root = compile_isel_pattern_node(
        expr,
        root,
        &mut nodes,
        &mut node_meta,
        &mut memo,
        &inferred_types,
        operand_constraints,
        operand_registers,
        operand_imm_ranges,
    )?;

    // A bare register-to-register copy cannot root on its own operand class
    // without becoming self-referential. A bare immediate rule is different:
    // it encodes the captured constant and therefore materializes that class.
    let copy = nodes.len() == 1 && node_meta[0].demands_register();
    assign_demands(&nodes, &mut node_meta);

    let specificity = nodes
        .iter()
        .filter(|node| matches!(node, PatternNode::Template(node) if node.ty.is_some()))
        .count();
    let (plan, captures, offsets, absorbs) = if copy {
        let (plan, captures) = lower_copy(pattern_root, result_register?)?;
        (plan, captures, Vec::new(), Vec::new())
    } else {
        lower(&nodes, &node_meta, pattern_root)
    };
    // An offset split computes its base plus a constant: whatever effect the
    // value has is its base's, a register the match reads, so any number of
    // matches may embed the split itself.
    for site in &offsets {
        node_meta[site.key as usize].duplicable = true;
    }
    let rooting = match &nodes[pattern_root.index()] {
        _ if offsets.iter().any(|site| site.key == pattern_root.0) => Rooting::Offset,
        PatternNode::Capture(_)
            if !copy && node_meta[pattern_root.index()].demand == BoundaryDemand::Immediate =>
        {
            Rooting::Immediate
        }
        _ => Rooting::Row,
    };
    let register_offset = match &nodes[pattern_root.index()] {
        PatternNode::Template(add) if add.sym() == Some(SymKind::Add) => {
            match add.children.as_slice() {
                &[base, addend]
                    if base != addend
                        && [base, addend].iter().all(|&operand| {
                            nodes[operand.index()].symbol().is_some()
                                && node_meta[operand.index()].demand == BoundaryDemand::Register
                        }) =>
                {
                    Some((base.0, addend.0))
                }
                _ => None,
            }
        }
        _ => None,
    };
    let constant = (!copy && rooting != Rooting::Immediate)
        .then(|| constant_shape(&nodes, &node_meta))
        .flatten();
    let shifts = match &nodes[pattern_root.index()] {
        PatternNode::Template(template) if rooting == Rooting::Row && !copy => template
            .children
            .iter()
            .enumerate()
            .filter(|&(at, &operand)| {
                template.sym().and_then(|kind| kind.offset_coefficient(at)) == Some(1)
                    && nodes[operand.index()].symbol().is_some()
                    && node_meta[operand.index()].demand == BoundaryDemand::Register
                    && template
                        .children
                        .iter()
                        .filter(|&&other| other == operand)
                        .count()
                        == 1
            })
            .map(|(_, operand)| operand.0)
            .collect(),
        _ => SmallVec::new(),
    };

    Some(CompiledIselPattern {
        rule_index,
        port,
        nodes,
        root: pattern_root.0,
        plan,
        rooting,
        offsets,
        absorbs,
        register_offset,
        shifts,
        constant,
        captures,
        node_meta,
        specificity,
        result_register,
        copy,
    })
}

/// The shape [`CompiledIselPattern::solve_constant`] evaluates: exactly one
/// immediate, at most one register operand, and otherwise constants, extension
/// widths and operators over them.
fn constant_shape(nodes: &[PatternNode], node_meta: &[PatternNodeMeta]) -> Option<Constant> {
    let mut immediate = None;
    let mut register = None;
    for (index, node) in nodes.iter().enumerate() {
        match node {
            PatternNode::Wildcard => return None,
            PatternNode::Capture(_) => match node_meta[index].demand {
                BoundaryDemand::Immediate if immediate.replace(index as u32).is_some() => {
                    return None;
                }
                BoundaryDemand::Register if register.replace(index as u32).is_some() => {
                    return None;
                }
                _ if node_meta[index].is_state => return None,
                _ => {}
            },
            PatternNode::Template(template) => {
                template.sym()?;
                if node_meta[index].is_state || is_memory_kind(template.sym()?) {
                    return None;
                }
            }
        }
    }
    Some(Constant {
        immediate: immediate?,
        register,
    })
}

/// What each boundary demands of its class. The width operand of an extension
/// is structural — the emitter reads it off the match — unless the same
/// operand also appears in a value position; an operand with no register or
/// immediate constraint is read from a register.
fn assign_demands(nodes: &[PatternNode], node_meta: &mut [PatternNodeMeta]) {
    let mut structural = HashSet::new();
    let mut value = HashSet::new();
    for node in nodes {
        let PatternNode::Template(node) = node else {
            continue;
        };
        for (operand, &child) in node.children.iter().enumerate() {
            if !node_meta[child.index()].is_boundary {
                continue;
            }
            if matches!(node.sym(), Some(SymKind::SExt | SymKind::ZExt)) && operand == 1 {
                structural.insert(child);
            } else {
                value.insert(child);
            }
        }
    }
    for (index, meta) in node_meta.iter_mut().enumerate() {
        let id = Id::from_raw(index as u32);
        let structural_only = structural.contains(&id) && !value.contains(&id);
        meta.demand = if meta.demands_register()
            || (meta.is_boundary
                && meta.constraint.is_none()
                && meta.register.is_none()
                && !structural_only)
        {
            BoundaryDemand::Register
        } else if meta.constraint == Some(OperandConstraint::Immediate) || meta.is_constant {
            BoundaryDemand::Immediate
        } else {
            BoundaryDemand::Structural
        };
    }
}

/// The query a copy rule is. A copy roots on the low-bit `Extract` view of a
/// wider class and binds its bare symbol to the view's *source*: rooting on the
/// source itself would make the copy self-referential, so the root is a class the
/// pattern does not name. Its variables therefore run past the pattern's own
/// nodes, which is why operand legality only speaks for the ones it does name.
///
/// `Extract(source, hi, lo)` with `lo = 0` is the view, and `hi + 1` is the width
/// it presents; the rule matches only where that is the width its result register
/// writes. A rule whose result is not an integer register writes no view at all.
fn lower_copy(source: Id, result: RegisterRequirement) -> Option<(Plan<SemNode>, Vec<u32>)> {
    if !result.capability.integer {
        return None;
    }
    let (view, hi, lo) = (source.0 + 1, source.0 + 2, source.0 + 3);
    let (lo_label, lo_value, hi_label, hi_value) = (0, 1, 2, 3);
    let mut extract = template_node(SymKind::Extract, None, None);
    extract.children = vec![source, Id::from_raw(hi), Id::from_raw(lo)];
    let query = Query {
        vars: source.0 + 4,
        scalars: 4,
        root: view,
        atoms: vec![
            Atom::Node {
                template: extract,
                args: SmallVec::from_slice(&[source.0, hi, lo]),
                class: view,
                row: None,
            },
            Atom::Fact {
                column: ColumnId::Const,
                key: lo,
                value: lo_label,
            },
            Atom::Fact {
                column: ColumnId::Const,
                key: hi,
                value: hi_label,
            },
        ],
        guards: vec![
            Guard::Read {
                term: Source::Label(lo_label),
                field: tir::sem::node::field::INT_VALUE,
                out: lo_value,
            },
            Guard::Cmp(Cmp::Eq, Expr::Scalar(lo_value), Expr::Lit(0)),
            Guard::Read {
                term: Source::Label(hi_label),
                field: tir::sem::node::field::INT_VALUE,
                out: hi_value,
            },
            Guard::Cmp(
                Cmp::Eq,
                Expr::Add(Box::new(Expr::Scalar(hi_value)), Box::new(Expr::Lit(1))),
                Expr::Lit(i64::from(result.capability.width)),
            ),
        ],
        nots: Vec::new(),
    };
    Some((Plan::compile(query), vec![source.0]))
}

/// Append a pattern node, returning the class variable it binds.
fn push(nodes: &mut Vec<PatternNode>, node: PatternNode) -> Id {
    nodes.push(node);
    Id::from_raw(nodes.len() as u32 - 1)
}

/// The query a pattern is, plus its captures in the order the nest reaches them
/// and its offset splits. Atoms go in the order a depth-first walk from the
/// root meets them, which is the order the matcher used to pop its goals in and
/// so the order matches come out in.
///
/// A constant is a reference to its carrier's zero, never a row, so a constant
/// node is a [`Atom::Literal`] read, and an `Add` or `Sub` of an immediate is
/// an [`Atom::Offset`] split of the value it matches ([`OffsetSite`]).
fn lower(
    nodes: &[PatternNode],
    node_meta: &[PatternNodeMeta],
    root: Id,
) -> (Plan<SemNode>, Vec<u32>, Vec<OffsetSite>, Vec<Absorb>) {
    let mut lowering = Lowering {
        nodes,
        node_meta,
        root,
        atoms: Vec::new(),
        captures: Vec::new(),
        offsets: Vec::new(),
        absorbs: Vec::new(),
        seen: vec![false; nodes.len()],
        vars: nodes.len() as u32,
        scalars: 0,
    };
    lowering.visit(root);
    let Lowering {
        atoms,
        captures,
        mut offsets,
        absorbs,
        vars,
        scalars,
        ..
    } = lowering;
    // An immediate two splits read (an access's address spelled for its read
    // and its write) holds one value, so neither can move part of it away.
    for index in 0..offsets.len() {
        let immediate = offsets[index].immediate;
        if offsets
            .iter()
            .filter(|site| site.immediate == immediate)
            .count()
            > 1
        {
            offsets[index].split = false;
        }
    }
    // One hole per operand, so a match binds an operand a rule reads twice once
    // and the query's own equality check answers for the second reading.
    debug_assert!(
        {
            let mut symbols: Vec<u32> = captures
                .iter()
                .filter_map(|&node| nodes[node as usize].symbol())
                .collect();
            symbols.sort_unstable();
            symbols.dedup();
            symbols.len() == captures.len()
        },
        "an operand is one capture"
    );
    let mut query = Query::tree(vars, root.0, atoms);
    query.scalars = scalars;
    (Plan::compile(query), captures, offsets, absorbs)
}

struct Lowering<'a> {
    nodes: &'a [PatternNode],
    node_meta: &'a [PatternNodeMeta],
    root: Id,
    atoms: Vec<Atom<SemNode>>,
    captures: Vec<u32>,
    offsets: Vec<OffsetSite>,
    absorbs: Vec<Absorb>,
    seen: Vec<bool>,
    /// The query's variables: one per pattern node, then one per row an
    /// [`Absorb`] reads at its class's offset zero.
    vars: u32,
    scalars: u32,
}

impl Lowering<'_> {
    fn scalar(&mut self) -> Scalar {
        self.scalars += 1;
        self.scalars - 1
    }

    fn visit(&mut self, node: Id) {
        if std::mem::replace(&mut self.seen[node.index()], true) {
            return;
        }
        match &self.nodes[node.index()] {
            PatternNode::Wildcard => {}
            PatternNode::Capture(_) => self.captures.push(node.0),
            PatternNode::Template(template) if template.sym() == Some(SymKind::Constant) => {
                self.atoms.push(Atom::Literal {
                    value: template.clone(),
                    class: node.0,
                });
            }
            PatternNode::Template(template) => {
                if let Some(mut site) = self.offset_site(node, template) {
                    site.scalar = self.scalar();
                    self.atoms.push(Atom::Offset {
                        key: node.0,
                        base: site.base,
                        offset: site.scalar,
                    });
                    self.offsets.push(site);
                    // The immediate is bound when the match is completed;
                    // the walk still meets it where the nest does.
                    for &child in &template.children {
                        if child.0 == site.immediate {
                            if !std::mem::replace(&mut self.seen[child.index()], true)
                                && self.nodes[child.index()].symbol().is_some()
                            {
                                self.captures.push(child.0);
                            }
                        } else {
                            self.visit(child);
                        }
                    }
                    return;
                }
                let mut class = node.0;
                if node != self.root
                    && let Some(operand) = self.absorber(template)
                {
                    class = self.vars;
                    self.vars += 1;
                    let scalar = self.scalar();
                    self.atoms.push(Atom::Offset {
                        key: node.0,
                        base: class,
                        offset: scalar,
                    });
                    self.absorbs.push(Absorb { operand, scalar });
                }
                self.atoms.push(Atom::Node {
                    template: template.clone(),
                    args: template.children.iter().map(|child| child.0).collect(),
                    class,
                    row: None,
                });
                for &child in &template.children {
                    self.visit(child);
                }
            }
        }
    }

    /// The register operand of `template` that takes an offset its row is
    /// not at ([`Absorb`]): the first with coefficient one, read nowhere else.
    fn absorber(&self, template: &SemNode) -> Option<u32> {
        let kind = template.sym()?;
        template
            .children
            .iter()
            .enumerate()
            .find(|&(at, &operand)| {
                kind.offset_coefficient(at) == Some(1)
                    && self.nodes[operand.index()].symbol().is_some()
                    && self.node_meta[operand.index()].demand == BoundaryDemand::Register
                    && self
                        .nodes
                        .iter()
                        .filter_map(|other| match other {
                            PatternNode::Template(other) => Some(&other.children),
                            _ => None,
                        })
                        .flatten()
                        .filter(|&&child| child == operand)
                        .count()
                        == 1
            })
            .map(|(_, operand)| operand.0)
    }

    /// `template` as an offset split, when it is an `Add` or `Sub` whose
    /// base has coefficient one and whose other operand is an immediate or a
    /// constant: the operators' own offset laws say the value is the base
    /// plus that operand, so no row of it exists to match.
    fn offset_site(&self, node: Id, template: &SemNode) -> Option<OffsetSite> {
        let kind = template.sym()?;
        let &[lhs, rhs] = template.children.as_slice() else {
            return None;
        };
        let immediate = |operand: Id| match &self.nodes[operand.index()] {
            PatternNode::Template(constant) => constant.sym() == Some(SymKind::Constant),
            PatternNode::Capture(_) => {
                self.node_meta[operand.index()].demand == BoundaryDemand::Immediate
                    && (!self.seen[operand.index()]
                        || self.offsets.iter().any(|site| site.immediate == operand.0))
            }
            PatternNode::Wildcard => false,
        };
        let (base, immediate) = [(lhs, rhs, 1), (rhs, lhs, 0)]
            .into_iter()
            .find(|&(base, operand, at)| {
                kind.offset_coefficient(1 - at) == Some(1)
                    && kind.offset_coefficient(at).is_some()
                    && immediate(operand)
                    && !immediate(base)
            })
            .map(|(base, operand, _)| (base, operand))?;
        let at = usize::from(immediate == rhs);
        let read_once = self
            .nodes
            .iter()
            .filter_map(|other| match other {
                PatternNode::Template(other) => Some(&other.children),
                _ => None,
            })
            .flatten()
            .filter(|&&child| child == base)
            .count()
            == 1;
        Some(OffsetSite {
            key: node.0,
            base: base.0,
            immediate: immediate.0,
            scalar: 0,
            sign: kind.offset_coefficient(at)?,
            split: read_once
                && self.nodes[base.index()].symbol().is_some()
                && self.node_meta[base.index()].demand == BoundaryDemand::Register,
        })
    }
}

/// The width the offsets of `key`'s class wrap at. A class without a carrier
/// is only ever at offset zero, which an immediate of a machine word holds.
fn offset_width(egraph: &SemEGraph, key: Ref) -> u32 {
    egraph.width(key.class).unwrap_or(64)
}

/// All ones at `width` bits, at most 63 of them.
fn mask_bits(width: u32) -> u64 {
    mask(width.min(63))
}

/// A binding no atom made: an immediate the completion fills in.
const UNSET: Ref = Ref {
    class: Id(u32::MAX),
    offset: 0,
};

/// All ones at `width` bits.
fn mask(width: u32) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1 << width) - 1
    }
}

/// `value`, a `width`-bit pattern, read as a two's complement integer.
fn signed(width: u32, value: u64) -> APInt {
    let width = width.clamp(1, 64);
    let shift = 64 - width;
    APInt::new_signed(width, ((value << shift) as i64) >> shift)
}

/// The zero class of the integers `width` bits wide.
fn zero(egraph: &SemEGraph, width: u32) -> Option<Id> {
    egraph
        .lookup(&SemNode::constant(APInt::new(width, 0), Prov::None))
        .map(|zero| zero.class)
}

/// The constant `value` is, as the immediate an encoding field reads it: two's
/// complement at its carrier's width, which [`ImmRange::contains`] turns into
/// whichever representative the field encodes.
pub(crate) fn immediate(egraph: &SemEGraph, value: Ref) -> Option<APInt> {
    let constant = class_int_binding(egraph, value)?;
    Some(signed(constant.width(), constant.to_u64()))
}

/// The parts of `value`, a `width`-bit constant, an immediate field can hold
/// when it cannot hold the whole: its low bits read the way the field reads
/// them, and, where what is left fits the field too, the field's extreme
/// towards `value`. Both are rounded towards zero to the field's alignment. A
/// zero part, and a negative value meeting a field that holds only
/// non-negative ones, give none.
fn field_parts(range: ImmRange, width: u32, value: u64) -> SmallVec<[u64; 2]> {
    let bits = range.width.min(width).min(63);
    let align = i64::from(range.align.max(1));
    let negative = signed(width, value).to_i64() < 0;
    if negative && !range.signed {
        return SmallVec::new();
    }
    let low = value & mask(bits);
    let low = if range.signed {
        let shift = 64 - bits;
        ((low << shift) as i64) >> shift
    } else {
        low as i64
    };
    let extreme = match (range.signed, negative) {
        (true, true) => -(1i64 << (bits - 1)),
        (true, false) => (1i64 << (bits - 1)) - 1,
        (false, _) => mask(bits) as i64,
    };
    let fits = |part: u64| part != 0 && range.contains(&signed(width, part));
    let mut parts: SmallVec<[u64; 2]> = SmallVec::new();
    for part in [low, extreme] {
        let part = (part - part % align) as u64 & mask(width);
        if fits(part) && !parts.contains(&part) {
            parts.push(part);
        }
    }
    // The extreme only helps where it leaves a rest the field holds whole.
    parts.retain(|part| {
        *part == (low - low % align) as u64 & mask(width)
            || fits(value.wrapping_sub(*part) & mask(width))
    });
    parts
}

/// The symbol naming the state operand of the memory node at `node`. Rule
/// operands are numbered from zero upwards, so counting down from the top keeps
/// the two apart, and one symbol per source node keeps two accesses in one
/// pattern from being forced onto one chain.
fn memory_state_symbol(node: NodeId) -> u32 {
    u32::MAX - node.index() as u32
}

fn canonical_pattern_root(expr: &SemGraph, root: NodeId) -> NodeId {
    if *expr.get_node(root) != SymKind::Add {
        return root;
    }
    let children: Vec<NodeId> = expr.children(root).collect();
    let [lhs, rhs] = children.as_slice() else {
        return root;
    };
    if is_extended_zero(expr, *lhs) {
        *rhs
    } else if is_extended_zero(expr, *rhs) {
        *lhs
    } else {
        root
    }
}

fn is_extended_zero(expr: &SemGraph, node: NodeId) -> bool {
    if *expr.get_node(node) != SymKind::ZExt {
        return false;
    }
    let children: Vec<NodeId> = expr.children(node).collect();
    let [value, _] = children.as_slice() else {
        return false;
    };
    matches!(
        expr.get_leaf_data(*value),
        Some(SymPayload::Int(value)) if value.to_u64() == 0
    )
}

#[allow(clippy::too_many_arguments)]
fn compile_isel_pattern_node(
    expr: &SemGraph,
    node: NodeId,
    nodes: &mut Vec<PatternNode>,
    node_meta: &mut Vec<PatternNodeMeta>,
    memo: &mut HashMap<Shared, Id>,
    inferred_types: &[SemType],
    operand_constraints: &[(u32, OperandConstraint)],
    operand_registers: &[(u32, RegisterRequirement)],
    operand_imm_ranges: &[(u32, ImmRange)],
) -> Option<Id> {
    let key = match (expr.get_node(node), expr.get_leaf_data(node)) {
        (SymKind::Symbol, Some(SymPayload::SymbolId(symbol))) => Shared::Operand(*symbol),
        _ => Shared::Term(node),
    };
    if let Some(compiled) = memo.get(&key).copied() {
        return Some(compiled);
    }

    let compiled = match expr.get_node(node) {
        SymKind::Symbol => {
            let Some(SymPayload::SymbolId(symbol)) = expr.get_leaf_data(node) else {
                return None;
            };
            let is_state = inferred_types[node.index()] == SemType::State;
            let compiled = push(nodes, PatternNode::Capture(*symbol));
            node_meta.push(PatternNodeMeta {
                is_boundary: !is_state,
                is_state,
                duplicable: true,
                constraint: operand_constraints
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, c)| *c),
                register: operand_registers
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, requirement)| *requirement),
                imm_range: operand_imm_ranges
                    .iter()
                    .find(|(s, _)| s == symbol)
                    .map(|(_, r)| *r),
                semantic_type: Some(inferred_types[node.index()].clone()),
                declared_type: expr.get_annotation(node).and_then(|m| m.actual_type),
                ..Default::default()
            });
            compiled
        }
        SymKind::Constant => match expr.get_leaf_data(node) {
            Some(SymPayload::Int(value)) => {
                let value = widen_pattern_literal(value, &inferred_types[node.index()]);
                let compiled = push(
                    nodes,
                    PatternNode::Template(template_node(
                        SymKind::Constant,
                        Some(SymPayload::Int(value)),
                        expr.get_annotation(node).and_then(|m| m.actual_type),
                    )),
                );
                // A constant is pure and folds into the encoding, so any number of
                // matches may embed the same constant class.
                node_meta.push(PatternNodeMeta {
                    is_constant: true,
                    duplicable: true,
                    semantic_type: Some(inferred_types[node.index()].clone()),
                    ..Default::default()
                });
                compiled
            }
            _ => return None,
        },
        kind => {
            // Children compile first: a pattern node's operands must have
            // smaller ids than the node itself.
            let mut children = expr
                .children(node)
                .enumerate()
                .map(|(index, child)| {
                    if (index == 0
                        && matches!(kind, SymKind::StateRead | SymKind::StateAssign)
                        && *expr.get_node(child) != SymKind::StateAssign)
                        || (index == 1 && *kind == SymKind::FPEffect)
                    {
                        let state = push(nodes, PatternNode::Wildcard);
                        node_meta.push(PatternNodeMeta {
                            is_state: true,
                            duplicable: true,
                            ..Default::default()
                        });
                        return Some(state);
                    }
                    compile_isel_pattern_node(
                        expr,
                        child,
                        nodes,
                        node_meta,
                        memo,
                        inferred_types,
                        operand_constraints,
                        operand_registers,
                        operand_imm_ranges,
                    )
                })
                .collect::<Option<Vec<Id>>>()?;
            // A rule spells a memory access at the target vocabulary's arity;
            // the program spells it over the state chain it reads, which is the
            // whole of its identity. The chain is matched and ignored, so the
            // two arities meet without the rule saying anything about state.
            if is_memory_kind(*kind) {
                let state = push(nodes, PatternNode::Capture(memory_state_symbol(node)));
                node_meta.push(PatternNodeMeta {
                    is_state: true,
                    duplicable: true,
                    ..Default::default()
                });
                children.push(state);
            }
            let pattern_type = (*kind != SymKind::StateRead)
                .then(|| expr.get_annotation(node).and_then(|m| m.actual_type))
                .flatten();
            let mut compiled = template_node(*kind, None, pattern_type);
            compiled.children = children;
            let compiled = push(nodes, PatternNode::Template(compiled));
            node_meta.push(PatternNodeMeta {
                is_state: inferred_types[node.index()] == SemType::State,
                semantic_type: Some(inferred_types[node.index()].clone()),
                ..Default::default()
            });
            compiled
        }
    };

    memo.insert(key, compiled);
    Some(compiled)
}

pub(super) fn widen_pattern_literal(value: &tir::utils::APInt, ty: &SemType) -> tir::utils::APInt {
    let width = match ty {
        SemType::Bits(Width::Const(width)) | SemType::RawBits(Width::Const(width)) => *width,
        _ => return value.clone(),
    };
    if width <= value.width() || width > 64 {
        return value.clone();
    }
    if value.is_signed() {
        value.sign_extend(width)
    } else {
        value.zero_extend(width)
    }
}
