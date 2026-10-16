//! The selection automaton's tables, built from the synthesized rules when
//! the pass is created (Ertl, Casey and Gregg: a tree-parsing automaton).
//!
//! A pattern is a prefix of some rule's left side: a row's label over the
//! patterns of its operands. Rules that share a prefix share its pattern, so
//! one step per row of a function matches every rule at once.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use smallvec::SmallVec;
pub(super) use tir::sem::egraph::canonical_type;
use tir::{
    Context, TypeId,
    sem::{
        IrOp, Kind, Prov, SemNode, SymKind, SymPayload,
        egraph::{LANES_WIDTH, carrier_of},
        node::{carrier_word, field},
    },
};
use tir_egraph::{ClassId, Label as _, Vocabulary};
use tir_relational::Words;

pub use super::pool::{Arg, Branch, Builder, Form, Operand, Row, Rows, Step, Synthesized, Tree};
use super::rule::ImmRange;
use super::synth::Instance;

/// A pattern's number: the state of its chain and the pattern of its form.
/// The seeds come first, then the patterns in the order the rules first
/// spell them.
pub type Pattern = u32;

/// What the seed chain of an operand's class says the operand is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Seed {
    /// A value of this type, a pointer as the opaque pointer and a vector
    /// as the vector of its element at any lane count.
    Register(TypeId),
    /// The zero of the integer carrier this wide: a constant is a reference
    /// to it at the constant's value.
    Constant(u32),
}

/// The children a form is reached through, from the form of a rule's root.
pub type Path = SmallVec<[u8; 4]>;

/// What a label reads at one of its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Child {
    /// An operand of the operation, whose values are `width` bits wide: the
    /// seed of a register holding one and, where a constant may define it,
    /// the seed of that constant.
    Operand {
        operand: u8,
        width: u32,
        register: Pattern,
        constant: Option<Pattern>,
    },
    /// A constant the label fixes, the bytes an access moves, and its seed.
    Fixed {
        value: u64,
        width: u32,
        constant: Pattern,
    },
    /// The lane count a vector row computes at, a constant `width` bits
    /// wide after the row's values, and its seed.
    Lanes { width: u32, constant: Pattern },
}

/// What the automaton reads of the operation a rule's row is.
#[derive(Clone, Debug)]
pub struct Described {
    /// The id, in the pass's vocabulary, of the label a row of the
    /// operation has in a function's graph.
    pub label: u32,
    /// The carrier a row of the operation has, as
    /// [`tir::sem::node::carrier_word`] spells it.
    pub carrier: u32,
    pub children: SmallVec<[Child; 5]>,
    /// The memory access the operation is, if it is one.
    pub access: Option<Access>,
    /// The operands a constant at which makes the row its other operand at
    /// that offset, and at the negated one: a function's graph then holds
    /// no row for it.
    pub offsets: u8,
    pub negated: u8,
}

/// What a row that reads or writes memory is to the order of the accesses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    /// The child that is the state the access observes.
    pub state: u8,
    /// Whether the row is the state the access leaves: a write. A read's
    /// row is the value read, and the state after it the one before it.
    pub writes: bool,
}

/// The seeds the rules' rows read, and the labels of those rows in the
/// pass's vocabulary.
#[derive(Default)]
pub struct Describing {
    pub seeds: Vec<Seed>,
}

/// Whether a row of `kind` is a memory access: the value a read yields, or
/// the state a write leaves.
pub fn accesses(kind: SymKind) -> bool {
    matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory)
}

/// The label of the chain of state `state`: its id in a vocabulary is the
/// word a step reads for an operand in that state.
pub fn chain(state: Pattern) -> SemNode {
    SemNode {
        kind: Kind::State(state),
        payload: None,
        ty: None,
        children: vec![ClassId::from_raw(0)],
        prov: Prov::None,
        carrier: None,
    }
}

/// The label of a form of `arity` children of the pattern `pattern`.
pub fn form(pattern: Pattern, arity: usize) -> SemNode {
    SemNode {
        kind: Kind::Form(pattern),
        payload: None,
        ty: None,
        children: vec![ClassId::from_raw(0); arity],
        prov: Prov::None,
        carrier: None,
    }
}

impl Describing {
    /// The number of `seed`, numbered on first sight.
    pub fn seed(&mut self, seed: Seed) -> Pattern {
        let known = self.seeds.iter().position(|known| *known == seed);
        let at = known.unwrap_or_else(|| {
            self.seeds.push(seed);
            self.seeds.len() - 1
        });
        at as Pattern
    }

    // Interned as the seeder builds it, so the id and the label's laws are the
    // ones a row of a function has.
    pub(super) fn describe(
        &mut self,
        context: &Context,
        pointer: u32,
        instance: &Instance,
        vocabulary: &mut Vocabulary<SemNode>,
    ) -> Option<Described> {
        let op = context.get_op(instance.op);
        let read = op.operands();
        let operand = |describing: &mut Self, operand: usize| {
            let ty = context.get_value(read[operand]).ty();
            let carrier = carrier_of(context, Some(pointer), ty);
            // A constant of a type the semantics read as bits is an integer
            // as wide, as the seeder spells the constant of any type but a
            // float.
            let carried = carrier.map(|carrier| u64::from(carrier.width)).or_else(|| {
                super::value_type(context.terms(), ty).and_then(|ty| context.terms().int_width(ty))
            });
            let width = carried
                .or(context.terms().bit_width(ty))
                .unwrap_or(64)
                .min(64) as u32;
            Child::Operand {
                operand: operand as u8,
                width,
                register: describing.seed(Seed::Register(canonical_type(context, ty))),
                constant: carried.map(|_| describing.seed(Seed::Constant(width))),
            }
        };
        let semantics = &instance.semantics;
        let head = *semantics.get_node(instance.root);
        let classes = |count: usize| vec![ClassId::from_raw(0); count];
        if accesses(head) {
            let mut children: SmallVec<[Child; 5]> = SmallVec::new();
            let mut state = None;
            for child in semantics.children(instance.root) {
                children.push(match semantics.get_leaf_data(child)? {
                    SymPayload::SymbolId(symbol) => {
                        let ty = context.get_value(read[*symbol as usize]).ty();
                        if context.is_state_type(ty) {
                            state = Some(children.len() as u8);
                        }
                        operand(self, *symbol as usize)
                    }
                    SymPayload::Int(value) => Child::Fixed {
                        value: value.to_u64(),
                        width: u32::try_from(value.width()).ok()?,
                        constant: self.seed(Seed::Constant(u32::try_from(value.width()).ok()?)),
                    },
                    _ => return None,
                });
            }
            // A read is the value it yields and a write the state it
            // publishes, as the seeder spells them.
            let value = *op.results().first()?;
            let mut node = SemNode::access(head, value, classes(children.len()));
            if head == SymKind::LoadMemory {
                let ty = context.get_value(value).ty();
                node = node
                    .typed(ty)
                    .carried(carrier_of(context, Some(pointer), ty));
            }
            return Some(Described {
                label: vocabulary.intern(&node).0,
                carrier: carrier_word(node.carrier) as u32,
                children,
                access: Some(Access {
                    state: state?,
                    writes: head == SymKind::StoreMemory,
                }),
                offsets: 0,
                negated: 0,
            });
        }
        let (values, states) = (op.value_results(), op.state_results());
        let value = *values.first().or(states.first())?;
        let ty = context.get_value(value).ty();
        let operator = IrOp::of(&op);
        let commutative = operator.commutative;
        let place = |value: &tir::ValueId| read.iter().position(|operand| operand == value);
        let (values, states) = (op.value_operands(), op.state_operands());
        let children = values.iter().chain(states.iter()).map(place);
        let children: SmallVec<[usize; 5]> = children.collect::<Option<_>>()?;
        // A vector term that names one symbol more than the operation has
        // operands names the lane count it computes at: a row of a function
        // holds that after its values, given as a length or read off its
        // type.
        let beyond = |node| {
            let named = semantics.get_leaf_data(node);
            matches!(named, Some(SymPayload::SymbolId(symbol)) if *symbol as usize == read.len())
        };
        let counted = instance.lanes && semantics.preorder(instance.root).any(beyond);
        let arity = children.len() + usize::from(counted);
        let labelled = canonical_type(context, ty);
        let node = SemNode::seeded_as(operator, &op, labelled, commutative, classes(arity))
            .carried(carrier_of(context, Some(pointer), ty));
        let moved = |by: i64| {
            let moves = |&(at, _): &(usize, &usize)| {
                node.carrier.is_some()
                    && children.len() == 2
                    && node.identity(at).is_some()
                    && node.offset_coefficient(at) == Some(by)
            };
            let moved = children.iter().enumerate().filter(moves);
            moved.fold(0, |held, (_, &operand)| held | 1 << operand)
        };
        let mut read: SmallVec<[Child; 5]> = children.iter().map(|&at| operand(self, at)).collect();
        if counted {
            let lanes = Child::Lanes {
                width: LANES_WIDTH,
                constant: self.seed(Seed::Constant(LANES_WIDTH)),
            };
            read.insert(values.len(), lanes);
        }
        Some(Described {
            label: vocabulary.intern(&node).0,
            carrier: carrier_word(node.carrier) as u32,
            offsets: moved(1),
            negated: moved(-1),
            children: read,
            access: None,
        })
    }
}

/// How a rule operand is held by the form it is bound through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// The form's child, read from a register.
    Register,
    /// The class the form's child is an offset of, read from a register.
    Base,
    /// The offset of the form's child from its base, encoded in a field.
    Offset,
    /// That offset negated, encoded in a field: what a subtraction of the
    /// field moves its register by.
    Negated,
    /// The form's child, a constant encoded in a field.
    Constant,
}

/// Where the form of an accepting pattern holds an operand of its rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bind {
    pub path: Path,
    pub held: Held,
}

/// What a pattern accepts: the instruction tree of the cheapest rule whose
/// left side it is.
#[derive(Clone, Debug)]
pub struct Accept {
    /// The instructions in the order they run; `Arg::Operand` indexes
    /// `operands`.
    pub tree: Tree,
    /// What a reader of the row reads: the last instruction, or with no
    /// instruction the operand or the constant the row is.
    pub value: Arg,
    pub cost: u32,
    /// Which of the values the last instruction defines the row is, and
    /// how many it defines.
    pub port: u8,
    pub results: u8,
    pub operands: SmallVec<[Bind; 4]>,
    /// Where the form holds the lane count [`Arg::Lanes`] reads, a constant.
    pub lanes: Option<Path>,
}

/// A rule whose root adds a constant to a register, or subtracts one from
/// it: a function's graph has no row for the sum, which is the register's
/// class at that offset, the negated constant for a subtraction. It is
/// the builder of such a reference where no field of its reader holds the
/// offset: `accept` over the register and the offset, for a base in state
/// `base` at an offset `class` admits (the last eight cells of an `imm`
/// row).
#[derive(Clone, Debug)]
pub struct Adder {
    pub base: Pattern,
    pub class: [u32; 8],
    pub accept: Accept,
    /// Whether the tree reads the offset from a register, which the
    /// builders of constants fill: the rule that adds two registers.
    pub built: bool,
}

/// What a pattern reads at a child: the state the child is in, and the
/// offsets a field admits where it holds what it can of the child's.
pub type Read = (Pattern, Option<[u32; 8]>);

/// The automaton. The tables are relations of 32-bit cells a rule joins.
///
/// For a row of `K` children, `L` the id of its label, `Ti` the id of the
/// label of the chain child `i` holds (see [`chain`]) and `mi` the class of
/// what child `i` is offset by:
/// - `step[K]` holds `(L, T1..TK, m1..mK, P, c)`: the row is pattern `P`,
///   and `c` is the carrier its form has, the row's own, so a reader holds
///   the form at an offset as it holds the row's value at one.
/// - `step_effect[K]` holds the same for the patterns a reader folds a
///   memory access through, which hold only where the reader is the
///   access's one reader, in its region.
/// - `step_over[K]` holds the same for the patterns some child of which
///   holds a folded access, `carried[P]` saying which: they hold only where
///   nothing else the pattern reads is computed after a write that follows
///   the access.
/// - `step_tied[K]` holds the same for the patterns that read one value at
///   two places, which `ties[P]` lists as pairs of paths.
/// - `effects[K]` holds `(L, s, w)` for a label that is a memory access:
///   child `s` is the state it observes, and `w` is one for a write.
/// - `imm[K]` holds `(L, T1..TK, i, m, W, add, sub, h, sgn, b, align,
///   nonzero)`: child `i` at offset `d` is of class `m` where, with
///   `x = (d + add - sub + ones(h) + sgn) & ones(W)`, `x == x & ones(b)`,
///   `d & align == 0` and `d != 0` if `nonzero`. Class zero is any offset,
///   kept beside the child: every class a child fits steps, and the
///   cheapest accepted tree wins.
pub struct Automaton {
    pub step: Vec<Arc<Words>>,
    pub step_effect: Vec<Arc<Words>>,
    pub step_over: Vec<Arc<Words>>,
    pub step_tied: Vec<Arc<Words>>,
    pub imm: Vec<Arc<Words>>,
    pub effects: Vec<Arc<Words>>,
    /// What each seed state says of an operand; the patterns are numbered
    /// after them.
    pub seeds: Vec<Seed>,
    /// By state, the id of its chain's label.
    pub chains: Vec<u32>,
    /// By pattern, the pairs of paths that hold one value.
    pub ties: Vec<SmallVec<[(Path, Path); 1]>>,
    /// By pattern, bit `i` set where a field holds the offset of child `i`.
    pub field: Vec<u8>,
    /// By pattern, the state each child is in, and for a register whose
    /// field holds what it can of any offset the offsets it admits (the
    /// last eight cells of an `imm` row): the register is then read at the
    /// rest. A seed has no children.
    pub children: Vec<SmallVec<[Read; 5]>>,
    /// By pattern, bit `i` set where child `i` holds a folded access.
    pub carried: Vec<u8>,
    /// By pattern, whether a reader folds it and its form holds an access,
    /// its own or a folded one: it holds only where the row has one reader,
    /// in its region.
    pub guarded: Vec<bool>,
    /// The operations synthesis made an instance of, by dialect and name,
    /// sorted, and the memory terms among them: what a rule has to cover.
    pub instantiated: Vec<(&'static str, &'static str)>,
    pub accessed: Vec<SymKind>,
    /// By pattern; `None` for a seed and for a pattern that is a prefix
    /// only.
    pub accept: Vec<Option<Accept>>,
    /// The conditional branch on each compare pattern's value and the one
    /// taken when it is false, over the operands `accept` binds, sorted by
    /// pattern.
    pub branches: Vec<(Pattern, Branch)>,
    /// The builders of a register at an offset, cheaper ones first.
    pub adders: Vec<Adder>,
    pub builders: Vec<Builder>,
    /// Forms that make a float of the bits a register holds: the form, the
    /// register operand, and the float's width.
    pub floats: Vec<(u32, u32, u32)>,
    /// What the trees' steps and the builders name.
    pub forms: Vec<Form>,
    /// The rules left out of the tables, by their place in the list, and
    /// why.
    pub unkeyed: Vec<(usize, Unkeyed)>,
    /// The rules whose pattern a cheaper rule accepts, by their place in
    /// the list.
    pub shadowed: Vec<usize>,
}

/// Why a rule steps no row of a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unkeyed {
    /// It folds a row that subtracts a constant, which a function's graph
    /// holds as the addition of the negated one.
    Negated,
    /// Its root adds a constant to another row: the sum is that row's class
    /// at an offset, which no row is and no register holds yet.
    Folded,
    /// It holds an offset of an offset, or a literal no cell spells.
    Unheld,
    /// It reads an instance no label was described for.
    Unlabelled,
}

// What an operand's offset is to a pattern: any, a class a field holds, or
// one value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Offset {
    Any,
    Field(ImmRange),
    Exact(u64),
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Shape {
    label: u32,
    carrier: u32,
    operands: SmallVec<[(Pattern, Offset); 5]>,
    foldable: bool,
    ties: SmallVec<[(Path, Path); 1]>,
}

// What a shape's children are beside their pattern: how wide, and the seed
// of the constant a register child may be instead.
type Sides = SmallVec<[(u32, Option<Pattern>); 5]>;

// What a rule names at a path of its root's form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Named {
    Operand(u8, Held),
    Row(u8),
    Lanes,
}

struct Building<'a> {
    described: &'a [Option<Described>],
    seeds: Pattern,
    shapes: Vec<Shape>,
    sides: Vec<Sides>,
    known: HashMap<Shape, Pattern>,
    accept: Vec<Option<Accept>>,
    branches: Vec<(Pattern, Branch)>,
    adders: Vec<Adder>,
    unkeyed: Vec<(usize, Unkeyed)>,
    shadowed: Vec<usize>,
}

impl Building<'_> {
    fn intern(&mut self, shape: Shape, sides: Sides) -> Pattern {
        if let Some(&known) = self.known.get(&shape) {
            return known;
        }
        self.shapes.push(shape.clone());
        self.sides.push(sides);
        self.accept.push(None);
        let pattern = self.seeds + self.shapes.len() as Pattern - 1;
        self.known.insert(shape, pattern);
        pattern
    }

    // Whether the form of `pattern` holds a folded access.
    fn holds(&self, pattern: Pattern) -> bool {
        let Some(at) = pattern.checked_sub(self.seeds) else {
            return false;
        };
        let shape = &self.shapes[at as usize];
        shape.foldable || shape.operands.iter().any(|&(under, _)| self.holds(under))
    }

    fn accepted(&mut self, pattern: Pattern) -> &mut Option<Accept> {
        &mut self.accept[(pattern - self.seeds) as usize]
    }

    // The operand `row` is an offset of and the constant it is offset by,
    // where a function's graph holds no row for it; `negated` for the
    // offsets a subtraction makes.
    fn offset(&self, rule: &Synthesized, row: usize, negated: bool) -> Option<(usize, usize)> {
        let own = &rule.rows[row];
        let described = self.described[own.instance as usize].as_ref()?;
        let offsets = match negated {
            true => described.negated,
            false => described.offsets,
        };
        if own.operands.len() != 2 {
            return None;
        }
        let constant = |at: usize| {
            let held = matches!(
                own.operands[at],
                Operand::Immediate(_) | Operand::Literal(_)
            );
            held && offsets & (1 << at) != 0
        };
        [(0, 1), (1, 0)].into_iter().find(|&(_, by)| constant(by))
    }

    fn child(&self, rule: &Synthesized, row: usize, operand: usize) -> Option<Child> {
        let described = self.described[rule.rows[row].instance as usize].as_ref()?;
        let children = described.children.iter();
        let mut children = children.filter(|child| {
            matches!(child, Child::Operand { operand: known, .. } if usize::from(*known) == operand)
        });
        children.next().copied()
    }

    // The pattern row `row` of `rule` is, with what the rule names under
    // `path`. A row of a commutative label is entered once: a rule over any
    // label reads such a row with its two children in either order.
    fn shape(
        &mut self,
        rule: &Synthesized,
        (row, folded): (usize, bool),
        path: &Path,
        named: &mut Vec<(Named, Path)>,
    ) -> Result<(Shape, Sides), Unkeyed> {
        let own = &rule.rows[row];
        let described = self.described;
        let described = described[own.instance as usize]
            .as_ref()
            .ok_or(Unkeyed::Unlabelled)?;
        let children = described.children.clone();
        let field = |operand: u8| {
            let field = rule.fields.iter().find(|(known, _)| *known == operand);
            field.map_or(Offset::Any, |&(_, field)| Offset::Field(field))
        };
        let mut operands = SmallVec::new();
        let mut sides = Sides::new();
        for (at, child) in children.iter().enumerate() {
            let mut here = path.clone();
            here.push(at as u8);
            let name = |named: &mut Vec<(Named, Path)>, what| named.push((what, here.clone()));
            let (operand, width, register, constant) = match *child {
                Child::Fixed {
                    value,
                    width,
                    constant,
                } => {
                    sides.push((width, None));
                    operands.push((constant, Offset::Exact(value)));
                    continue;
                }
                // Any count: the instruction is told it, as a field would
                // hold it.
                Child::Lanes { width, constant } => {
                    name(named, Named::Lanes);
                    let any = ImmRange {
                        width,
                        signed: false,
                        align: 1,
                        nonzero: false,
                    };
                    sides.push((width, None));
                    operands.push((constant, Offset::Field(any)));
                    continue;
                }
                Child::Operand {
                    operand,
                    width,
                    register,
                    constant,
                } => (
                    own.operands[usize::from(operand)],
                    width,
                    register,
                    constant,
                ),
            };
            let constant = constant.ok_or(Unkeyed::Unheld);
            let (entry, other) = match operand {
                Operand::Register(operand) => {
                    name(named, Named::Operand(operand, Held::Register));
                    ((register, Offset::Any), constant.ok())
                }
                Operand::Immediate(operand) => {
                    name(named, Named::Operand(operand, Held::Constant));
                    ((constant?, field(operand)), None)
                }
                Operand::Literal(value) => ((constant?, Offset::Exact(value)), None),
                Operand::Row(below) => {
                    name(named, Named::Row(below));
                    let below = usize::from(below);
                    if self.offset(rule, below, true).is_some() {
                        return Err(Unkeyed::Negated);
                    }
                    let Some((base, by)) = self.offset(rule, below, false) else {
                        // No register holds a folded row, so none is there
                        // to add an offset to.
                        let folded = self.shape(rule, (below, true), &here, named)?;
                        operands.push((self.intern(folded.0, folded.1), Offset::Exact(0)));
                        sides.push((width, None));
                        continue;
                    };
                    let by = match rule.rows[below].operands[by] {
                        Operand::Immediate(operand) => {
                            name(named, Named::Operand(operand, Held::Offset));
                            field(operand)
                        }
                        Operand::Literal(value) => Offset::Exact(value),
                        _ => return Err(Unkeyed::Unheld),
                    };
                    match rule.rows[below].operands[base] {
                        Operand::Register(operand) => {
                            name(named, Named::Operand(operand, Held::Base));
                            let Some(Child::Operand { register, .. }) =
                                self.child(rule, below, base)
                            else {
                                return Err(Unkeyed::Unlabelled);
                            };
                            ((register, by), None)
                        }
                        Operand::Row(under) => {
                            name(named, Named::Row(under));
                            let under = usize::from(under);
                            // An offset of an offset is one offset in a
                            // function's graph, which no one field holds.
                            let offset = |negated| self.offset(rule, under, negated).is_some();
                            if offset(false) || offset(true) {
                                return Err(Unkeyed::Unheld);
                            }
                            let folded = self.shape(rule, (under, true), &here, named)?;
                            ((self.intern(folded.0, folded.1), by), None)
                        }
                        _ => return Err(Unkeyed::Unheld),
                    }
                }
            };
            if fits(entry.1, width).is_none() {
                return Err(Unkeyed::Unheld);
            }
            operands.push(entry);
            sides.push((width, other));
        }
        // A row a reader folds with an access in its form is as good as
        // the access: folded where it has more readers, the access runs
        // once for each.
        let holds = operands.iter().any(|&(under, _)| self.holds(under));
        let shape = Shape {
            label: described.label,
            carrier: described.carrier,
            operands,
            foldable: folded && (described.access.is_some() || holds),
            ties: SmallVec::new(),
        };
        Ok((shape, sides))
    }

    // The accept of `rule` over what it names at the paths of its root's
    // form.
    fn accept(rule: &Synthesized, named: &[(Named, Path)]) -> Accept {
        let count = named.iter().filter_map(|(what, _)| match what {
            Named::Operand(operand, _) => Some(usize::from(*operand) + 1),
            Named::Row(_) | Named::Lanes => None,
        });
        let bind = |operand: usize| {
            let bound = named.iter().find_map(|(what, path)| match *what {
                Named::Operand(known, held) if usize::from(known) == operand => Some(Bind {
                    path: path.clone(),
                    held,
                }),
                _ => None,
            });
            bound.expect("a rule numbers its operands by first use")
        };
        Accept {
            tree: rule.tree.clone(),
            value: rule.value,
            cost: rule.cost,
            port: rule.port,
            results: rule.results,
            operands: (0..count.max().unwrap_or(0)).map(bind).collect(),
            lanes: named
                .iter()
                .find_map(|(what, path)| (*what == Named::Lanes).then(|| path.clone())),
        }
    }

    // A rule whose root adds a constant is the builder of its register at
    // that offset, and one whose root subtracts a constant at the negated
    // one.
    fn adder(
        &mut self,
        rule: &Synthesized,
        (base, by): (usize, usize),
        negated: bool,
    ) -> Result<(), Unkeyed> {
        let root = &rule.rows[0];
        let Operand::Register(register) = root.operands[base] else {
            return Err(Unkeyed::Folded);
        };
        let child = |at| self.child(rule, 0, at).ok_or(Unkeyed::Unlabelled);
        let (Child::Operand { register: seed, .. }, Child::Operand { width, .. }) =
            (child(base)?, child(by)?)
        else {
            return Err(Unkeyed::Unlabelled);
        };
        let path: Path = std::iter::once(0).collect();
        let mut named = vec![(Named::Operand(register, Held::Base), path.clone())];
        let offset = match root.operands[by] {
            Operand::Immediate(operand) => {
                let held = match negated {
                    true => Held::Negated,
                    false => Held::Offset,
                };
                named.push((Named::Operand(operand, held), path));
                let field = rule.fields.iter().find(|(known, _)| *known == operand);
                field.map_or(Offset::Any, |&(_, field)| Offset::Field(field))
            }
            Operand::Literal(value) => Offset::Exact(value),
            Operand::Register(operand) => {
                named.push((Named::Operand(operand, Held::Constant), path));
                Offset::Any
            }
            Operand::Row(_) => return Err(Unkeyed::Unheld),
        };
        let class = fits(offset, width).ok_or(Unkeyed::Unheld)?;
        self.adders.push(Adder {
            base: seed,
            class: match negated {
                true => opposite(class),
                false => class,
            },
            accept: Self::accept(rule, &named),
            built: matches!(root.operands[by], Operand::Register(_)),
        });
        Ok(())
    }

    // The operand a rule of one row over two registers adds to the other,
    // where a constant there makes the row no row of a function's graph:
    // the rule then builds a register at any offset, the offset built into
    // the register it reads.
    fn sum(&self, rule: &Synthesized) -> Option<(usize, usize)> {
        let [root] = &rule.rows[..] else {
            return None;
        };
        let described = self.described[root.instance as usize].as_ref()?;
        let (&[Operand::Register(first), Operand::Register(second)], true) =
            (&root.operands[..], rule.branch.is_none())
        else {
            return None;
        };
        let moved =
            |&(_, by): &(usize, usize)| first != second && described.offsets & (1 << by) != 0;
        [(0, 1), (1, 0)].into_iter().find(moved)
    }

    fn rule(&mut self, at: usize, rule: &Synthesized) -> Result<(), Unkeyed> {
        for negated in [false, true] {
            if let Some(sum) = self.offset(rule, 0, negated) {
                return self.adder(rule, sum, negated);
            }
        }
        if let Some(sum) = self.sum(rule) {
            self.adder(rule, sum, false)?;
        }
        let mut named = Vec::new();
        let (mut shape, sides) = self.shape(rule, (0, false), &Path::new(), &mut named)?;
        // What the rule names at two paths is one value read twice.
        let same = |a: &Named, b: &Named| match (a, b) {
            (Named::Operand(a, _), Named::Operand(b, _)) => a == b,
            (Named::Row(a), Named::Row(b)) => a == b,
            (Named::Lanes, Named::Lanes) => true,
            _ => false,
        };
        for (at, (what, path)) in named.iter().enumerate() {
            let first = named.iter().position(|(other, _)| same(other, what));
            if let Some(first) = first.filter(|&first| first != at) {
                shape.ties.push((named[first].1.clone(), path.clone()));
            }
        }
        let pattern = self.intern(shape, sides);
        if let Some(cheaper) = self.accepted(pattern) {
            // The same row with the same tree is one rule the pool holds
            // twice, for two instances a function's row does not tell
            // apart.
            if cheaper.tree != rule.tree {
                self.shadowed.push(at);
            }
            return Ok(());
        }
        *self.accepted(pattern) = Some(Self::accept(rule, &named));
        self.branches
            .extend(rule.branch.clone().map(|branch| (pattern, branch)));
        Ok(())
    }

    fn tables(mut self, seeds: Vec<Seed>, vocabulary: &mut Vocabulary<SemNode>) -> Automaton {
        let states = seeds.len() + self.shapes.len();
        let chains: Vec<u32> = (0..states as Pattern)
            .map(|state| vocabulary.intern(&chain(state)).0)
            .collect();
        // The forms too, as a rule's head spells them: what a rule reads a
        // form through is resolved before a function has one.
        for (pattern, shape) in (self.seeds..).zip(&self.shapes) {
            let fills = [
                (field::FORM, u64::from(pattern)),
                (field::CARRIER, u64::from(shape.carrier)),
            ];
            let form = form(pattern, shape.operands.len());
            let spelled = SemNode::fill(&form, vocabulary.names(), &fills);
            vocabulary.intern(&spelled.expect("a form of a carrier"));
        }
        let arity = self.shapes.iter().map(|shape| shape.operands.len()).max();
        let tables = |width: &dyn Fn(usize) -> usize| -> Vec<Words> {
            let arities = 0..=arity.unwrap_or(0);
            arities.map(|arity| Words::new(width(arity))).collect()
        };
        let mut plain = tables(&|arity| 2 * arity + 3);
        let mut effect = tables(&|arity| 2 * arity + 3);
        let mut over = tables(&|arity| 2 * arity + 3);
        let mut tied = tables(&|arity| 2 * arity + 3);
        let mut imm = tables(&|arity| arity + 11);
        let mut effects = tables(&|_| 3);
        let mut accesses: Vec<(u32, usize, Access)> = Vec::new();
        for described in self.described.iter().flatten() {
            if let Some(access) = described.access {
                accesses.push((described.label, described.children.len(), access));
            }
        }
        accesses.sort_unstable_by_key(|&(label, ..)| label);
        accesses.dedup();
        let accessing: HashSet<u32> = accesses.iter().map(|&(label, ..)| label).collect();
        for (label, arity, access) in accesses {
            if let Some(table) = effects.get_mut(arity) {
                table.push(&[label, u32::from(access.state), u32::from(access.writes)]);
            }
        }
        // A pattern is numbered after the patterns of its children.
        let mut carried: Vec<u8> = vec![0; seeds.len()];
        for shape in &self.shapes {
            let holds = |pattern: Pattern| {
                let folded = pattern.checked_sub(self.seeds);
                folded.is_some_and(|at| self.shapes[at as usize].foldable)
                    || carried[pattern as usize] != 0
            };
            let children = shape.operands.iter().enumerate();
            let held = children.filter(|&(_, &(pattern, _))| holds(pattern));
            carried.push(held.fold(0, |held, (child, _)| held | 1 << child));
        }
        // A child read from a register may be a constant instead, built
        // into one: each such child steps under the constant's seed too.
        type Key = (u32, SmallVec<[Pattern; 5]>);
        let mut keyed: Vec<(usize, Key)> = Vec::new();
        for (at, (shape, sides)) in self.shapes.iter().zip(&self.sides).enumerate() {
            let free: SmallVec<[usize; 5]> = (0..sides.len())
                .filter(|&child| sides[child].1.is_some())
                .collect();
            for built in 0..1u32 << free.len() {
                let mut patterns: SmallVec<[Pattern; 5]> =
                    shape.operands.iter().map(|&(pattern, _)| pattern).collect();
                for (bit, &child) in free.iter().enumerate() {
                    if let (true, Some(constant)) = (built & (1 << bit) != 0, sides[child].1) {
                        patterns[child] = constant;
                    }
                }
                keyed.push((at, (shape.label, patterns)));
            }
        }
        // The field of a register's offset holds the part of an offset it
        // can, and the register is built at the rest. A field that refuses
        // zero has no part to hold of an offset it has nothing of.
        let split = |state: Pattern, offset: Offset| match (seeds.get(state as usize), offset) {
            (Some(Seed::Register(_)), Offset::Field(field)) if !field.nonzero => Some(field),
            _ => None,
        };
        // The classes of offsets the transitions on one key have at one
        // child, numbered in the order of what they hold.
        let mut classes: HashMap<(&Key, usize), BTreeSet<Offset>> = HashMap::new();
        for (at, key) in &keyed {
            for (child, &(_, offset)) in self.shapes[*at].operands.iter().enumerate() {
                classes.entry((key, child)).or_default().insert(offset);
            }
        }
        let class = |classes: &BTreeSet<Offset>, offset: Offset| {
            let mut held = classes.iter().filter(|known| **known != Offset::Any);
            let held = held.position(|known| *known == offset);
            held.map_or(0, |at| at as u32 + 1)
        };
        let mut listed: HashSet<(&Key, usize)> = HashSet::new();
        let mut stepped = HashSet::new();
        for (at, key) in &keyed {
            let (shape, sides) = (&self.shapes[*at], &self.sides[*at]);
            let arity = shape.operands.len();
            let words = key.1.iter().map(|&pattern| chains[pattern as usize]);
            let words: SmallVec<[u32; 5]> = words.collect();
            let offsets = shape.operands.iter().enumerate();
            let offsets =
                offsets.map(|(child, &(_, offset))| class(&classes[&(key, child)], offset));
            let over = match carried[seeds.len() + *at] {
                0 => &mut plain[arity],
                _ => &mut over[arity],
            };
            let access = shape.foldable && accessing.contains(&shape.label);
            let step = match (shape.ties.is_empty(), access) {
                (false, _) => &mut tied[arity],
                (true, true) => &mut effect[arity],
                (true, false) => over,
            };
            let mut row: SmallVec<[u32; 12]> = SmallVec::new();
            row.push(shape.label);
            row.extend(words.iter().copied().chain(offsets));
            // A constant built into a register steps one pattern however
            // many rules read a register there: the first, the cheapest.
            if stepped.insert((row.clone(), shape.foldable, &shape.ties)) {
                row.extend([self.seeds + *at as Pattern, shape.carrier]);
                step.push(&row);
            }
            for (child, &(width, _)) in sides.iter().enumerate() {
                let known = &classes[&(key, child)];
                if !listed.insert((key, child)) {
                    continue;
                }
                for &offset in known {
                    let mut row: SmallVec<[u32; 16]> = SmallVec::new();
                    row.push(shape.label);
                    row.extend(words.iter().copied());
                    row.extend([child as u32, class(known, offset)]);
                    // A field a register's offset is split at steps on any
                    // offset: the form pays for the rest.
                    let admitted = match split(key.1[child], offset) {
                        Some(_) => Offset::Any,
                        None => offset,
                    };
                    row.extend(fits(admitted, width).expect("a shape holds what a cell spells"));
                    imm[arity].push(&row);
                }
            }
        }
        let mut field = vec![0; seeds.len()];
        let mut ties = vec![SmallVec::new(); seeds.len()];
        let mut children = vec![SmallVec::new(); seeds.len()];
        for (shape, sides) in self.shapes.iter().zip(&self.sides) {
            let held = shape.operands.iter().enumerate();
            let held = held.filter(|&(_, &(_, offset))| offset != Offset::Any);
            let held = held.fold(0, |held, (child, _)| held | 1 << child);
            field.push(held);
            ties.push(shape.ties.clone());
            let read = shape.operands.iter().zip(sides);
            let read = read.map(|(&(state, offset), &(width, _))| {
                let admitted = split(state, offset).map(Offset::Field);
                (state, admitted.and_then(|field| fits(field, width)))
            });
            children.push(read.collect());
        }
        let shared = |tables: Vec<Words>| tables.into_iter().map(Arc::new).collect();
        let none_guarded = std::iter::repeat_n(false, seeds.len());
        let guarded = self.shapes.iter().map(|shape| shape.foldable);
        let none = std::iter::repeat_n(None, seeds.len());
        self.branches.sort_by_key(|&(pattern, _)| pattern);
        Automaton {
            step: shared(plain),
            step_effect: shared(effect),
            step_over: shared(over),
            step_tied: shared(tied),
            imm: shared(imm),
            effects: shared(effects),
            ties,
            field,
            children,
            carried,
            guarded: none_guarded.chain(guarded).collect(),
            instantiated: Vec::new(),
            accessed: Vec::new(),
            accept: none.chain(self.accept).collect(),
            branches: self.branches,
            adders: self.adders,
            builders: Vec::new(),
            floats: Vec::new(),
            forms: Vec::new(),
            unkeyed: self.unkeyed,
            shadowed: self.shadowed,
            seeds,
            chains,
        }
    }
}

// The cells of an `imm` row that say which offsets of a child `width` bits
// wide are of a class: `W`, `add`, `sub`, `h`, `sgn`, `b`, `align`,
// `nonzero`. `None` for a literal further from zero than a cell holds.
fn fits(offset: Offset, width: u32) -> Option<[u32; 8]> {
    let width = width.clamp(1, 64);
    let any = [width, 0, 0, 0, 0, width, 0, 0];
    Some(match offset {
        Offset::Any => any,
        Offset::Exact(value) => {
            let value = tir::binding::signed_bits(value, width);
            let near = u32::try_from(value.unsigned_abs()).ok()?;
            match value < 0 {
                true => [width, near, 0, 0, 0, 0, 0, 0],
                false => [width, 0, near, 0, 0, 0, 0, 0],
            }
        }
        Offset::Field(field) => {
            let align = field.align.max(1).next_power_of_two() - 1;
            let nonzero = u32::from(field.nonzero);
            match (field.signed, field.width) {
                (_, held) if held >= width => [width, 0, 0, 0, 0, width, align, nonzero],
                (true, held) => [width, 0, 0, held.saturating_sub(1), 1, held, align, nonzero],
                (false, held) => [width, 0, 0, 0, 0, held, align, nonzero],
            }
        }
    })
}

// The cells of the offsets whose negation is of the class the cells spell.
// An offset `d` is of a class where `d + bias` fits `b` bits, `bias` being
// `add - sub + ones(h) + sgn`, so its negation is where `d + ones(b) - bias`
// does.
fn opposite([width, add, sub, high, sign, bits, align, nonzero]: [u32; 8]) -> [u32; 8] {
    let high = match sign {
        0 => bits,
        _ => high,
    };
    [width, sub, add, high, 0, bits, align, nonzero]
}

impl Automaton {
    /// The tables of `rules`, cheaper rules first: where two rules spell
    /// one pattern the first is the one it accepts. `described` has each
    /// instance the rules' rows name and `seeds` the seeds those name; the
    /// chain of every state is interned in `vocabulary`.
    pub fn build(
        rules: &[Synthesized],
        described: &[Option<Described>],
        seeds: Vec<Seed>,
        vocabulary: &mut Vocabulary<SemNode>,
    ) -> Self {
        let mut building = Building {
            described,
            seeds: seeds.len() as Pattern,
            shapes: Vec::new(),
            sides: Vec::new(),
            known: HashMap::new(),
            accept: Vec::new(),
            branches: Vec::new(),
            adders: Vec::new(),
            unkeyed: Vec::new(),
            shadowed: Vec::new(),
        };
        for (at, rule) in rules.iter().enumerate() {
            if let Err(why) = building.rule(at, rule) {
                building.unkeyed.push((at, why));
            }
        }
        building.tables(seeds, vocabulary)
    }

    /// How many states there are, the seeds among them.
    pub fn patterns(&self) -> usize {
        self.field.len()
    }

    /// The conditional branch on the value of `pattern`.
    pub fn branch(&self, pattern: Pattern) -> Option<&Branch> {
        let known = self
            .branches
            .binary_search_by_key(&pattern, |&(known, _)| known);
        known.ok().map(|at| &self.branches[at].1)
    }
}
