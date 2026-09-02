//! Compilation of rule semantic expressions into matchable patterns.

use std::collections::{HashMap, HashSet};

use smallvec::SmallVec;
use tir::{
    Context,
    graph::{Dag, MetaDag, NodeId, OperandConstraint},
    sem::{
        SemGraph, SemNode, SemType, SymKind, SymPayload, TypeUnifier,
        egraph::{SemEGraph, class_int_binding, class_semantic_type},
        infer_types, template_node,
    },
};
use tir_relational::ClassId as Id;
use tir_relational::{Atom, Cmp, ColumnId, Expr, Guard, Match, NoExterns, Plan, Query, Source};

use super::node::{class_register_type, is_memory_kind};
use super::{ImmRange, RegisterRequirement};

/// One node of a rule's pattern: a template operator, or an operand the rule
/// names.
#[derive(Clone)]
pub(crate) enum PatternNode {
    Template(SemNode),
    Capture(u32),
}

impl PatternNode {
    pub(crate) fn symbol(&self) -> Option<u32> {
        match self {
            PatternNode::Capture(symbol) => Some(*symbol),
            PatternNode::Template(_) => None,
        }
    }
}

/// A rule's pattern compiled for matching: the query it is, the nodes it was
/// written as, and the per-node metadata the cover consults.
pub(crate) struct CompiledIselPattern {
    pub(crate) rule_index: usize,
    /// One entry per pattern node; a match binds one class variable per entry.
    pub(crate) nodes: Vec<PatternNode>,
    root: u32,
    plan: Plan<SemNode>,
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
    /// Whether any number of matches may embed this node's class (operands and
    /// constants).
    pub(crate) duplicable: bool,
    pub(crate) constraint: Option<OperandConstraint>,
    /// Storage capability and bit demand of a physical register operand.
    pub(crate) register: Option<RegisterRequirement>,
    /// Encoding range of an immediate operand (see `Rule::operand_imm_ranges`).
    pub(crate) imm_range: Option<ImmRange>,
    /// The symbolic value type inferred from the semantic operator signatures.
    pub(crate) semantic_type: Option<SemType>,
}

impl PatternNodeMeta {
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

    /// The class a match bound to pattern node `index`.
    pub(crate) fn binding(matched: &Match, index: usize) -> Id {
        matched.bindings[index].expect("every pattern node is reached from the root")
    }

    fn match_types(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        matched: &Match,
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
            let class = Self::binding(matched, index);
            class_semantic_type(ctx, egraph, class)
                .is_none_or(|actual| unifier.unify(expected, &actual).is_ok())
        });
        // The root check must see pointer-typed classes at the data layout's
        // pointer width: `class_semantic_type` has no answer for them, and
        // accepting on `None` let an 8-bit immediate move claim a 64-bit
        // pointer constant.
        nodes_match
            && self.result_register.is_none_or(|register| {
                class_register_type(ctx, egraph, matched.root, pointer_width)
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
    /// at (a rewrite-introduced class of unknown width is produced at register
    /// width, so it still matches), an immediate range rejects a constant the
    /// encoding field cannot represent, and an immediate constraint requires a
    /// constant member. Register constraints are checked by the cover: a constant
    /// may bind here only if a selected materializer makes it available in a
    /// register.
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
        if let Some(required) = meta.register
            && let Some(actual) = class_register_type(ctx, egraph, class, pointer_width)
            && !required.accepts(&actual)
        {
            return false;
        }
        if let Some(range) = meta.imm_range
            && let Some(value) = class_int_binding(egraph, class)
            && !range.contains(&value)
        {
            return false;
        }
        match meta.constraint {
            Some(OperandConstraint::Register) => true,
            Some(OperandConstraint::Immediate) => egraph
                .nodes(class)
                .chain(egraph.const_of(class))
                .any(|n| n.kind == SymKind::Constant),
            None => true,
        }
    }

    /// The classes the pattern can root at: those holding its root operator, or
    /// every class where it roots on a bare symbol or a low-bit view.
    pub(crate) fn roots(&self, egraph: &SemEGraph) -> Vec<Id> {
        self.plan.roots(egraph)
    }

    pub(crate) fn search_roots_with_legality(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        roots: impl IntoIterator<Item = Id>,
        pointer_width: Option<u32>,
        allowed: &dyn Fn(Id, Id) -> bool,
    ) -> Vec<Match> {
        let allowed = |var: u32, class: Id| allowed(Id::from_raw(var), class);
        self.plan
            .search(egraph, roots, &allowed, false, &NoExterns)
            .into_iter()
            .filter(|matched| {
                if self.copy {
                    self.reads_the_source(egraph, ctx, matched)
                } else {
                    self.match_types(egraph, ctx, matched, pointer_width)
                }
            })
            .collect()
    }

    pub(crate) fn search_roots(
        &self,
        egraph: &SemEGraph,
        ctx: &Context,
        roots: impl IntoIterator<Item = Id>,
        pointer_width: Option<u32>,
    ) -> Vec<Match> {
        self.search_roots_with_legality(egraph, ctx, roots, pointer_width, &|node, class| {
            self.boundary_ok(egraph, ctx, node, class, pointer_width)
        })
    }

    /// Whether a copy rule may read the class its view is a truncation of. The
    /// query has already found the view and matched the width; what is left is
    /// the operand's own storage requirement, which is where every other
    /// pattern's type checking also lives.
    fn reads_the_source(&self, egraph: &SemEGraph, ctx: &Context, matched: &Match) -> bool {
        let Some(source) = matched.bindings[self.root as usize] else {
            return false;
        };
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
    expr: &SemGraph,
    operand_constraints: &[(u32, OperandConstraint)],
    operand_registers: &[(u32, RegisterRequirement)],
    operand_imm_ranges: &[(u32, ImmRange)],
    result_register: Option<RegisterRequirement>,
) -> Option<CompiledIselPattern> {
    let root = canonical_pattern_root(expr, expr.root()?);
    let inferred_types = infer_types(expr, |_| None).ok()?;
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

    let specificity = nodes
        .iter()
        .filter(|node| matches!(node, PatternNode::Template(node) if node.ty.is_some()))
        .count();
    let (plan, captures) = if copy {
        lower_copy(pattern_root, result_register?)?
    } else {
        lower(&nodes, pattern_root)
    };

    Some(CompiledIselPattern {
        rule_index,
        nodes,
        root: pattern_root.0,
        plan,
        captures,
        node_meta,
        specificity,
        result_register,
        copy,
    })
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

/// The query a pattern is, plus its captures in the order the nest reaches them.
/// Atoms go in the order a depth-first walk from the root meets them, which is
/// the order the matcher used to pop its goals in and so the order matches come
/// out in.
fn lower(nodes: &[PatternNode], root: Id) -> (Plan<SemNode>, Vec<u32>) {
    let mut atoms = Vec::new();
    let mut captures = Vec::new();
    let mut seen = vec![false; nodes.len()];
    visit(nodes, root, &mut seen, &mut atoms, &mut captures);
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
    (
        Plan::compile(Query::tree(nodes.len() as u32, root.0, atoms)),
        captures,
    )
}

fn visit(
    nodes: &[PatternNode],
    node: Id,
    seen: &mut [bool],
    atoms: &mut Vec<Atom<SemNode>>,
    captures: &mut Vec<u32>,
) {
    if std::mem::replace(&mut seen[node.index()], true) {
        return;
    }
    match &nodes[node.index()] {
        PatternNode::Capture(_) => captures.push(node.0),
        PatternNode::Template(template) => {
            atoms.push(Atom::Node {
                template: template.clone(),
                args: template.children.iter().map(|child| child.0).collect(),
                class: node.0,
                row: None,
            });
            for &child in &template.children {
                visit(nodes, child, seen, atoms, captures);
            }
        }
    }
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
            let compiled = push(nodes, PatternNode::Capture(*symbol));
            node_meta.push(PatternNodeMeta {
                is_boundary: true,
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
                ..Default::default()
            });
            compiled
        }
        SymKind::Constant => match expr.get_leaf_data(node) {
            Some(SymPayload::Int(value)) => {
                let compiled = push(
                    nodes,
                    PatternNode::Template(template_node(
                        SymKind::Constant,
                        Some(SymPayload::Int(value.clone())),
                        expr.get_actual_type(node),
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
                .map(|child| {
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
            let mut compiled = template_node(*kind, None, expr.get_actual_type(node));
            compiled.children = children;
            let compiled = push(nodes, PatternNode::Template(compiled));
            node_meta.push(PatternNodeMeta {
                semantic_type: Some(inferred_types[node.index()].clone()),
                ..Default::default()
            });
            compiled
        }
    };

    memo.insert(key, compiled);
    Some(compiled)
}
