//! The pool: one e-graph, built when the pass is created, that holds the
//! semantic term of every dialect operation instance over symbolic operands.
//! The shared axioms saturate it, the target's instruction patterns insert a
//! machine node into every class they compute, and the cheapest machine term
//! of a left side's class is the instruction tree its rule selects to. The
//! forward pass covers each instance alone; the backward pass tiles each
//! pattern of several operators with instances and covers each tiling.

use std::collections::{BTreeMap, HashMap};

use smallvec::{SmallVec, smallvec};
use tir::{
    Context, NodeId, Terms, TypeArg, TypeId,
    sem::{
        IrOp, Kind, SemEGraph, SemGraph, SemNode, SemPayload, SymKind, SymPayload, Theory,
        axioms::Shape,
        egraph::{carrier_of, class_int_binding, complement_comparison, minimal_unsigned_apint},
        infer_types,
        node::{carrier_word, field},
        rewrites, template_node,
    },
};
use tir_adt::APInt;
use tir_egraph::{
    Atom, ClassId, ColumnId, Externs, Extraction, Guard, HeadOp, LabelFill, Plan, Query, Ref,
    Rule as EngineRule,
};
use tir_relational::{Expr, Scalar};
use tir_symbolic::term::ctor;

use super::rule::{Emitter, ImmRange, RegisterRequirement, Rule};
use super::rules::OperandConstraint;
use super::synth::Instance;
use super::{held_type, value_type};

const UNCOVERED: u64 = 1 << 40;
// What a cost is scaled by to leave room below it for the preference of a
// rule that names its type.
const TYPED: u64 = 16;
// The field of an immediate operand a rule declares no range for.
const WHOLE: super::rule::ImmRange = super::rule::ImmRange {
    width: 64,
    signed: true,
    align: 1,
    nonzero: false,
};

mod call {
    pub(super) const ACCEPTS: u32 = 1 << 16;
    pub(super) const CARRIER: u32 = (1 << 16) + 1;
    pub(super) const FITS: u32 = (1 << 16) + 2;
    pub(super) const TYPED: u32 = (1 << 16) + 3;
}

/// One target pattern as the pool matches it: the rule and result it is, and
/// the operand symbol each child of its machine node binds.
pub struct Form {
    pub rule: usize,
    pub port: usize,
    pub symbols: Vec<u32>,
    /// The field each immediate operand is encoded in, by position in
    /// `symbols`; `None` for a register operand.
    pub ranges: Vec<Option<super::rule::ImmRange>>,
    /// Whether each operand is read from a register. One that is neither
    /// that nor an immediate is a constant the emitter reads off the match.
    pub registers: Vec<bool>,
    /// For a conditional branch, the operand symbol that is its target: the
    /// form then tests the class it matched instead of computing it.
    pub branch: Option<u32>,
    pub cost: u32,
}

/// What an instruction of a tree reads at one of its machine node's children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Arg {
    /// An operand of the operation the rule rewrites.
    Operand(u8),
    /// What an earlier instruction of the tree defines.
    Step(u8),
    /// A constant the rule itself names: its bits and its width.
    Literal(u64, u32),
    /// The lane count of the type the row computes at.
    Lanes,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub form: u32,
    pub args: SmallVec<[Arg; 4]>,
}

pub type Tree = SmallVec<[Step; 2]>;

// Deepest chain of instructions one rule may hold.
const DEPTH: usize = 4;
// Atoms of the largest pattern the pool matches.
const ATOMS: usize = 256;
// Rows a rule may fold into the row it rewrites, and the operands all of
// them may read.
pub(super) const LEVELS: usize = 3;
pub(super) const OPERANDS: usize = 16;

/// What a row of a rule's left side reads at one operand of its operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operand {
    /// A rule operand, read from a register.
    Register(u8),
    /// A rule operand some constant defines, held in the field the rule
    /// lists for it.
    Immediate(u8),
    /// A constant the rule itself names: its bits at the operand's width.
    Literal(u64),
    /// What another row of the rule computes.
    Row(u8),
}

/// One operation of a rule's left side: an instance over what it reads.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Row {
    pub instance: u32,
    pub operands: SmallVec<[Operand; 4]>,
}

/// A rule's left side, the row it rewrites first. An operand or a row named
/// at two places is one value read twice.
pub type Rows = SmallVec<[Row; 4]>;

/// The conditional branch on a rule's value, and the one taken when the
/// value is false. The branch is the last instruction of each tree.
#[derive(Clone, Debug)]
pub struct Branch {
    pub taken: Tree,
    pub inverse: Option<Tree>,
    /// What `taken` costs, priced as a rule's tree is.
    pub cost: u32,
}

/// A synthesized selection rule: the operations it rewrites and the
/// instructions that compute them.
#[derive(Clone, Debug)]
pub struct Synthesized {
    pub rows: Rows,
    /// The instructions in the order they run. `Arg::Operand` numbers the
    /// rule's operands.
    pub tree: Tree,
    /// What a reader of the root row reads: the last instruction, or with no
    /// instruction the operand or the constant the rows are.
    pub value: Arg,
    /// The instructions' latency times [`super::rule::LATENCY_COST_SCALE`]
    /// plus their bytes, summed, with the constants they read from registers
    /// built.
    pub cost: u32,
    /// The field each immediate operand is encoded in.
    pub fields: SmallVec<[(u8, ImmRange); 2]>,
    /// Which of the values the last instruction defines the root row is, and
    /// how many it defines.
    pub port: u8,
    pub results: u8,
    /// Whether the root row reads or writes memory: a rule folding it into a
    /// reader holds only where that reader is its one reader, in its region.
    pub effect: bool,
    pub branch: Option<Branch>,
}

// The rules whose left side is the row of `instance` at the root.
type Rooted = HashMap<u32, SmallVec<[u32; 4]>>;

// `rows` in the order a walk from the root meets them, operands numbered by
// first use, the first two operands of each row that `commutes` in whichever
// order spells the least such left side; and the number each operand got.
pub(super) fn canonical(rows: &Rows, commutes: &dyn Fn(u32) -> bool) -> (Rows, SmallVec<[u8; 16]>) {
    // The kind of an operand orders a spelling before any numbering does:
    // a row reading two kinds has its order by them, and only a row reading
    // one kind twice is tried both ways.
    let kind = |operand: &Operand| std::mem::discriminant(operand);
    let mut rows = rows.clone();
    let mut free: SmallVec<[usize; 4]> = SmallVec::new();
    for (at, row) in rows.iter_mut().enumerate() {
        if row.operands.len() < 2 || !commutes(row.instance) {
            continue;
        }
        if kind(&row.operands[0]) == kind(&row.operands[1]) {
            free.push(at);
        } else if row.operands[1] < row.operands[0] {
            row.operands.swap(0, 1);
        }
    }
    let mut best: Option<(Rows, SmallVec<[u8; 16]>)> = None;
    for exchanged in 0..1u32 << free.len() {
        let mut turned = rows.clone();
        for (bit, &at) in free.iter().enumerate() {
            if exchanged & (1 << bit) != 0 {
                turned[at].operands.swap(0, 1);
            }
        }
        let numbered = numbered(&turned);
        if best.as_ref().is_none_or(|best| numbered.0 < best.0) {
            best = Some(numbered);
        }
    }
    best.expect("a left side has one spelling at least")
}

fn numbered(rows: &Rows) -> (Rows, SmallVec<[u8; 16]>) {
    let mut order: SmallVec<[u8; 8]> = smallvec![0];
    let mut next = 0;
    while let Some(&at) = order.get(next) {
        next += 1;
        for operand in &rows[usize::from(at)].operands {
            if let Operand::Row(below) = *operand
                && !order.contains(&below)
            {
                order.push(below);
            }
        }
    }
    let place = |row: u8| order.iter().position(|&at| at == row).unwrap_or(0) as u8;
    let mut numbers: SmallVec<[u8; 16]> = SmallVec::new();
    let mut number = |operand: u8| {
        let operand = usize::from(operand);
        if numbers.len() <= operand {
            numbers.resize(operand + 1, u8::MAX);
        }
        if numbers[operand] == u8::MAX {
            numbers[operand] = numbers.iter().filter(|&&known| known != u8::MAX).count() as u8;
        }
        numbers[operand]
    };
    let renamed = order
        .iter()
        .map(|&at| {
            let row = &rows[usize::from(at)];
            let operands = row.operands.iter().map(|operand| match *operand {
                Operand::Register(operand) => Operand::Register(number(operand)),
                Operand::Immediate(operand) => Operand::Immediate(number(operand)),
                Operand::Row(below) => Operand::Row(place(below)),
                literal @ Operand::Literal(_) => literal,
            });
            Row {
                instance: row.instance,
                operands: operands.collect(),
            }
        })
        .collect();
    (renamed, numbers)
}

/// A form that builds a constant from an immediate: the constant is the
/// immediate shifted left by `shift`, plus what `register` holds where the
/// form reads one. A form that `shifts` builds what `register` holds shifted
/// left by the immediate instead. A form whose pattern names the type of
/// what it computes builds constants of that `width` only, as does one that
/// reads a register: synthesis keeps it for a width or drops it. One whose
/// rule names the register of its `result` builds only those the register
/// holds. What a form computes wraps at `bits`: its type's, else those of
/// the register of its result.
/// A form that `masks` builds the complement of its shifted immediate, or
/// what `register` holds with the immediate in place of the bits of its
/// field at `shift`.
#[derive(Clone, Copy)]
pub struct Builder {
    pub form: u32,
    pub symbol: u32,
    pub range: super::rule::ImmRange,
    pub shift: u32,
    pub register: Option<u32>,
    pub shifts: bool,
    pub width: Option<u32>,
    pub bits: u32,
    pub result: Option<super::RegisterRequirement>,
    pub masks: bool,
}

impl Builder {
    /// Whether the form builds constants `width` bits wide. The price of a
    /// constant and the instructions emitted for it both ask this.
    pub fn builds(&self, width: u32) -> bool {
        let held = |register: &super::RegisterRequirement| register.accepts_int(u64::from(width));
        self.width.is_none_or(|own| own == width) && self.result.as_ref().is_none_or(held)
    }
}

// In the order they run: the first builds from its immediate alone, each later
// one from what the one before it built.
pub(super) struct Built {
    pub(super) steps: SmallVec<[(Builder, APInt); 2]>,
}

// The most instructions a constant is built by: a 64-bit one from 12-bit
// immediates takes this many.
const STEPS: usize = 8;

// `builders` must be listed cheapest first.
pub(super) fn plan(
    builders: &[Builder],
    value: &APInt,
    usable: impl Fn(&Builder) -> bool,
) -> Option<Built> {
    let width = u32::try_from(value.width())
        .ok()
        .filter(|&width| width <= 64)?;
    let signed = tir::binding::signed_bits(value.to_u64(), width);
    let mut steps = SmallVec::new();
    built(builders, value, signed, &usable, STEPS, &mut steps).then_some(Built { steps })
}

// Append to `steps` the rules that build `wanted`, a part of `value`, in at
// most `budget` of them.
fn built(
    builders: &[Builder],
    value: &APInt,
    wanted: i64,
    usable: &dyn Fn(&Builder) -> bool,
    budget: usize,
    steps: &mut SmallVec<[(Builder, APInt); 2]>,
) -> bool {
    if budget == 0 {
        return false;
    }
    // `plan` builds no value wider than 64 bits.
    let width = value.width() as u32;
    let whole = wanted == tir::binding::signed_bits(value.to_u64(), width);
    // The immediate a rule builds `wanted` from alone.
    let alone = |builder: &Builder| {
        let wanted = match builder.masks {
            true => !wanted,
            false => wanted,
        };
        let immediate = wanted >> builder.shift;
        let fits =
            builder.register.is_none() && immediate << builder.shift == wanted && usable(builder);
        // A part wider than the value is an immediate of the register's
        // width.
        let narrow = tir::binding::signed_bits(immediate as u64, width) == immediate;
        let immediate = match (builder.shift, whole, builder.masks) {
            (0, true, false) => value.clone(),
            _ if narrow => APInt::new_signed(u64::from(width), immediate),
            _ => APInt::new_signed(64, immediate),
        };
        (fits && builder.range.contains(&immediate)).then_some(immediate)
    };
    let direct = builders
        .iter()
        .find_map(|builder| alone(builder).map(|by| (*builder, by)));
    if let Some(first) = direct {
        steps.push(first);
        return true;
    }
    let reading = |shifts: bool| {
        let reading = builders.iter().filter(|builder| builder.register.is_some());
        let reading = reading.filter(|builder| !builder.masks);
        reading.filter(move |builder| builder.shifts == shifts && usable(builder))
    };
    // The fields above the lowest are replaced first, the highest first, so
    // what is left is one a rule builds alone: over zeros or over ones,
    // whichever takes fewer rules.
    let field = |builder: &Builder| ((1i64 << builder.range.width) - 1) << builder.shift;
    let replaced = [0, -1].into_iter().filter_map(|fill: i64| {
        let replacing = builders.iter().filter(|builder| {
            builder.masks
                && builder.register.is_some()
                && builder.shift < width
                && (wanted ^ fill) & field(builder) != 0
                && usable(builder)
        });
        let replacing = replacing.max_by_key(|builder| builder.shift)?;
        let field = field(replacing);
        let by = APInt::new_signed(
            u64::from(width),
            ((wanted & field) as u64 >> replacing.shift) as i64,
        );
        let rest = (wanted & !field) | (fill & field);
        let rest = tir::binding::signed_bits(rest as u64, width);
        let mut chain = SmallVec::new();
        let found = replacing.range.contains(&by)
            && built(builders, value, rest, usable, budget - 1, &mut chain);
        found.then(|| {
            chain.push((*replacing, by));
            chain
        })
    });
    if let Some(chain) = replaced.min_by_key(|chain: &SmallVec<[(Builder, APInt); 2]>| chain.len())
    {
        steps.extend(chain);
        return true;
    }
    for adding in reading(false) {
        let field = adding.range.width;
        if !(1..64).contains(&field) {
            continue;
        }
        // The low bits the rule adds, read as the rule reads them.
        let low = wanted & ((1i64 << field) - 1);
        let low = match adding.range.signed {
            true => tir::binding::signed_bits(low as u64, field),
            false => low,
        };
        let by = APInt::new_signed(u64::from(width), low);
        // The rule adds at its own width: the rest is what its sum with
        // the low bits wraps to the wanted bits at.
        let rest = tir::binding::signed_bits(wanted.wrapping_sub(low) as u64, adding.bits);
        if low != 0
            && adding.range.contains(&by)
            && built(builders, value, rest, usable, budget - 1, steps)
        {
            steps.push((*adding, by));
            return true;
        }
    }
    for shifting in reading(true) {
        let zeros = wanted.trailing_zeros();
        let by = APInt::new_signed(u64::from(width), i64::from(zeros));
        if zeros != 0
            && wanted != 0
            && shifting.range.contains(&by)
            && built(builders, value, wanted >> zeros, usable, budget - 1, steps)
        {
            steps.push((*shifting, by));
            return true;
        }
    }
    false
}

// The value of `node` where it is a literal, extended, or a product of such:
// a mask spelled as its field times the power of two that shifts it.
fn fixed(pattern: &SemGraph, node: NodeId) -> Option<u64> {
    let children: SmallVec<[NodeId; 4]> = pattern.children(node).collect();
    match (
        pattern.get_node(node),
        pattern.get_leaf_data(node),
        children.as_slice(),
    ) {
        (SymKind::Constant, Some(SymPayload::Int(value)), _) => Some(value.to_u64()),
        (SymKind::SExt | SymKind::ZExt, _, &[value, ..]) => fixed(pattern, value),
        (SymKind::Mul, _, &[first, second]) => {
            fixed(pattern, first)?.checked_mul(fixed(pattern, second)?)
        }
        _ => None,
    }
}

// The immediate `pattern` builds a constant from, how far it shifts it, the
// register operand it adds, whether it shifts that one by the immediate
// instead and whether it masks, if the pattern is of that shape: an
// immediate, extended or shifted by a literal, alone, complemented or added
// to a register, a register shifted left by an immediate, or a register
// whose bits under the immediate's field are replaced by the immediate.
fn builder(rule: &Rule, pattern: &SemGraph) -> Option<(u32, u32, Option<u32>, bool, bool)> {
    let symbol = |node: NodeId| match pattern.get_leaf_data(node) {
        Some(SymPayload::SymbolId(symbol)) if *pattern.get_node(node) == SymKind::Symbol => {
            Some(*symbol)
        }
        _ => None,
    };
    let literal = |node: NodeId| match pattern.get_leaf_data(node) {
        Some(SymPayload::Int(value)) if *pattern.get_node(node) == SymKind::Constant => {
            Some(value.to_u64())
        }
        _ => None,
    };
    let nothing = |node: NodeId| {
        let inner = match pattern.get_node(node) {
            SymKind::SExt | SymKind::ZExt => pattern.children(node).next(),
            _ => Some(node),
        };
        inner.and_then(literal) == Some(0)
    };
    let immediate = |candidate: u32| {
        let constraints = rule.operand_constraints.iter();
        constraints
            .filter(|(symbol, _)| *symbol == candidate)
            .any(|(_, kind)| matches!(kind, OperandConstraint::Immediate))
    };
    let part = |mut node: NodeId| {
        let mut shift = 0;
        loop {
            let children: SmallVec<[NodeId; 4]> = pattern.children(node).collect();
            match (pattern.get_node(node), children.as_slice()) {
                (SymKind::ShiftLeft, &[value, by]) => {
                    shift += u32::try_from(literal(by)?).ok().filter(|&by| by < 64)?;
                    node = value;
                }
                (SymKind::Mul, &[value, by]) => {
                    let by = fixed(pattern, by).filter(|by| by.is_power_of_two())?;
                    shift += by.trailing_zeros();
                    node = value;
                }
                (SymKind::SExt | SymKind::ZExt, &[value, ..]) => node = value,
                // A hardwired zero register reads as the literal zero, bare
                // or extended to the register's width.
                (SymKind::Add, &[value, zero]) | (SymKind::Add, &[zero, value])
                    if nothing(zero) =>
                {
                    node = value;
                }
                (SymKind::Symbol, _) => {
                    let found = symbol(node).filter(|&found| immediate(found))?;
                    return Some((found, shift));
                }
                _ => return None,
            }
        }
    };
    let root = pattern.root()?;
    let children: SmallVec<[NodeId; 4]> = pattern.children(root).collect();
    if let (SymKind::Add, &[first, second]) = (pattern.get_node(root), children.as_slice()) {
        for (register, by) in [(first, second), (second, first)] {
            let register = symbol(register).filter(|&register| !immediate(register));
            if let (Some(register), Some((found, 0))) = (register, part(by)) {
                return Some((found, 0, Some(register), false, false));
            }
        }
    }
    let kept = |node: NodeId, field: u64| {
        let children: SmallVec<[NodeId; 4]> = pattern.children(node).collect();
        let (SymKind::And, &[register, mask]) = (pattern.get_node(node), children.as_slice())
        else {
            return None;
        };
        let cleared = match pattern.get_node(mask) {
            SymKind::Not => pattern.children(mask).next(),
            _ => None,
        };
        let cleared = cleared.and_then(|cleared| fixed(pattern, cleared));
        let register = symbol(register).filter(|&register| !immediate(register));
        register.filter(|_| cleared == Some(field))
    };
    if let (SymKind::Or, &[rest, by]) = (pattern.get_node(root), children.as_slice())
        && let Some((found, shift)) = part(by)
        && let Some(&(_, range)) = rule.operand_imm_ranges.iter().find(|(s, _)| *s == found)
        && !range.signed
        && range.width < 64
        && range.width + shift <= 64
        && let Some(register) = kept(rest, ((1u64 << range.width) - 1) << shift)
    {
        return Some((found, shift, Some(register), false, true));
    }
    if let (SymKind::Not, &[inner]) = (pattern.get_node(root), children.as_slice()) {
        return part(inner).map(|(found, shift)| (found, shift, None, false, true));
    }
    if let (SymKind::ShiftLeft, &[value, by]) = (pattern.get_node(root), children.as_slice()) {
        let register = symbol(value).filter(|&register| !immediate(register));
        if let (Some(register), Some((found, 0))) = (register, part(by)) {
            return Some((found, 0, Some(register), true, false));
        }
    }
    part(root).map(|(found, shift)| (found, shift, None, false, false))
}

// What tiling reads of an instance: its operator, the types of its operands
// (a pointer as the word it is) and of its result, what its head reads at
// each of its operands, which operands a constant may define, and whether
// rules over several rows are built of it.
struct Tile {
    head: SymKind,
    reads: SmallVec<[TypeId; 4]>,
    // How wide a literal at each operand is: an integer a register holds.
    widths: SmallVec<[Option<u32>; 4]>,
    result: Option<TypeId>,
    ports: SmallVec<[Port; 5]>,
    constants: u8,
    usable: bool,
    commutative: bool,
    access: bool,
}

// What an instance's head reads at one of its operands: an operand of the
// operation, or a constant the term fixes (the bytes an access moves).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Port {
    Operand(usize),
    Fixed(u64),
}

// Where a row reads an operand: the row and the operand of its operation.
type Place = (u8, u8);

// The operand of a row not given its content yet: the row that defines it,
// or what is read again of an earlier place.
const OPEN: Operand = Operand::Row(u8::MAX);

// A left side as tiling grows it. `pending` holds the operators not yet
// given a row: the order they are given one in (the first operand first),
// the operator, where it is read and its type. `read` has where each
// variable or symbol `K` was first read, `deferred` where each operator
// was, and `again` the places that read one of those once more.
#[derive(Clone)]
struct Partial<N, K> {
    rows: Rows,
    pending: Vec<(usize, N, Place, Option<TypeId>)>,
    read: Vec<(K, Place)>,
    deferred: Vec<(N, Place)>,
    again: Vec<(Place, Place)>,
    operands: u8,
    slots: usize,
    // The rows that count against [`LEVELS`]: those of an axiom's tile do
    // not.
    counted: usize,
}

impl<N: Copy, K: PartialEq> Partial<N, K> {
    fn new(root: N, ty: Option<TypeId>) -> Self {
        Self {
            rows: Rows::new(),
            pending: vec![(0, root, (0, 0), ty)],
            read: Vec::new(),
            deferred: Vec::new(),
            again: Vec::new(),
            operands: 0,
            slots: 0,
            counted: 0,
        }
    }

    fn next(&mut self) -> Option<(N, Place, Option<TypeId>)> {
        let next = (0..self.pending.len()).min_by_key(|&at| self.pending[at].0)?;
        let (_, node, place, ty) = self.pending.remove(next);
        Some((node, place, ty))
    }

    fn fresh(&mut self, immediate: bool) -> Operand {
        self.operands += 1;
        match immediate {
            true => Operand::Immediate(self.operands - 1),
            false => Operand::Register(self.operands - 1),
        }
    }

    // Where `key` was read before, noted as read again at `place`.
    fn before(&mut self, key: &K, place: Place) -> bool {
        let before = self.read.iter().find(|(seen, _)| seen == key);
        let Some(&(_, first)) = before else {
            return false;
        };
        self.again.push((place, first));
        true
    }

    // The operand at `place` is an operator, given its row later.
    fn defer(&mut self, node: N, place: Place, ty: TypeId) {
        let order = self.slots + usize::from(place.1);
        self.pending.push((order, node, place, Some(ty)));
        self.deferred.push((node, place));
    }

    // Add `row`, read at `place` by the row before it that waited for it.
    fn push(&mut self, row: Row, place: Place) {
        let at = self.rows.len() as u8;
        if at != 0 {
            self.rows[usize::from(place.0)].operands[usize::from(place.1)] = Operand::Row(at);
        }
        self.slots += row.operands.len();
        self.counted += 1;
        self.rows.push(row);
    }

    fn finished(mut self) -> Rows {
        for &((row, operand), (first, read)) in &self.again {
            self.rows[usize::from(row)].operands[usize::from(operand)] =
                self.rows[usize::from(first)].operands[usize::from(read)];
        }
        self.rows
    }
}

fn ones(width: u32) -> u64 {
    u64::MAX >> (64 - width.clamp(1, 64))
}

// How wide a literal operand of type `ty` is: an integer a register holds.
fn literal_width(terms: &Terms, ty: TypeId) -> Option<u32> {
    let integer = tir::builtin::IntegerType::view(terms, ty)?;
    u32::try_from(integer.width())
        .ok()
        .filter(|width| (1..=64).contains(width))
}

// The operand of a row that is a constant, if one is: its position in the
// variants a row is covered at, zero for a row that reads registers only.
fn constant(operands: &[Operand]) -> Option<usize> {
    let held = |operand: &Operand| matches!(operand, Operand::Immediate(_) | Operand::Literal(_));
    let mut constants = operands
        .iter()
        .enumerate()
        .filter(|(_, operand)| held(operand));
    let first = constants.next().map_or(0, |(at, _)| at + 1);
    constants.next().is_none().then_some(first)
}

// What each variable of an axiom is in the rows spelling its side.
type Variables = SmallVec<[(usize, Operand); 4]>;

// The rows spelling an axiom's left side, as the tile of the operator its
// right side is over variables alone: `holes` has, per operand of that
// operator, what the rows read there.
struct Spelled {
    head: SymKind,
    rows: Rows,
    holes: SmallVec<[Operand; 4]>,
    result: Option<TypeId>,
}

// Every left side of `tiles` that spells an axiom's side: a node of operator
// F is given a row of head F whose operand count and types fit, a variable
// is an operand of the rule, read from a register or defined by a constant,
// a constant is a literal of the row, and an operator is given its row next.
// What is spelled twice is read twice.
fn tile<'a>(
    tiles: &[Tile],
    covered: &[u8],
    mut partial: Partial<&'a Shape, &'a Shape>,
    out: &mut Vec<(Rows, Variables)>,
) {
    let Some((shape, place, ty)) = partial.next() else {
        let read = partial.read.clone();
        let rows = partial.finished();
        let variables = read.iter().filter_map(|&(shape, (row, at))| match shape {
            Shape::Variable(variable) => {
                Some((*variable, rows[usize::from(row)].operands[usize::from(at)]))
            }
            _ => None,
        });
        let variables = variables.collect();
        out.push((rows, variables));
        return;
    };
    let Shape::Operator(kind, children) = shape else {
        return;
    };
    let operands: SmallVec<[&Shape; 4]> = children
        .iter()
        .filter(|child| **child != Shape::Width)
        .collect();
    let at = partial.rows.len() as u8;
    for (instance, shape) in tiles.iter().enumerate() {
        if !shape.usable
            || shape.head != *kind
            || shape.reads.len() != operands.len()
            || ty.is_some_and(|ty| shape.result != Some(ty))
            || partial.slots + shape.reads.len() > OPERANDS
        {
            continue;
        }
        let constants = (0..shape.reads.len()).filter(|&at| shape.constants & (1 << at) != 0);
        for held in std::iter::once(None).chain(constants.map(Some)) {
            let mut longer = partial.clone();
            let spelled = operands.iter().enumerate().map(|(index, &child)| {
                let place = (at, index as u8);
                let held = held == Some(index);
                let width = shape.widths[index].filter(|_| held);
                match child {
                    Shape::Ones => width.map(|width| Operand::Literal(ones(width))),
                    Shape::Constant(value) => {
                        width.map(|width| Operand::Literal(value & ones(width)))
                    }
                    Shape::Width | Shape::Other => None,
                    _ if longer.before(&child, place) => (!held).then_some(OPEN),
                    Shape::Variable(_) => {
                        longer.read.push((child, place));
                        Some(longer.fresh(held))
                    }
                    Shape::Operator(..) => {
                        longer.read.push((child, place));
                        longer.defer(child, place, shape.reads[index]);
                        (!held).then_some(OPEN)
                    }
                }
            });
            let Some(operands) = spelled.collect::<Option<SmallVec<[Operand; 4]>>>() else {
                continue;
            };
            // A literal no field holds is built into a register: the row
            // over registers covers it.
            let literal = |at: usize| matches!(operands[at], Operand::Literal(_));
            let built = u8::from(held.is_some_and(literal));
            if covered[instance] & (built | 1 << held.map_or(0, |at| at + 1)) == 0 {
                continue;
            }
            let row = Row {
                instance: instance as u32,
                operands,
            };
            longer.push(row, place);
            tile(tiles, covered, longer, out);
        }
    }
}

// One target pattern as tiling reads it, and whether its `zero` form is
// spelled: a sum with a displacement is then its other operand.
struct Tiled<'a> {
    terms: &'a Terms,
    tiles: &'a [Tile],
    covered: &'a [u8],
    pattern: &'a SemGraph,
    types: &'a [TypeArg],
    immediates: &'a [u32],
    zero: bool,
    spelled: &'a [Spelled],
}

impl Tiled<'_> {
    // Every left side of the tiles that spells the pattern, its own shape
    // directing the search (Buchwald): a node of operator F is given a row
    // of head F whose operand count and types fit the node's children, a
    // symbol child is an operand of the rule, read again where the symbol
    // repeats, a constant child is a literal of the row, and an operator
    // child is given its row next.
    fn tilings(&self, mut partial: Partial<NodeId, u32>, out: &mut Vec<Rows>) {
        let Some((node, place, ty)) = partial.next() else {
            out.push(partial.finished());
            return;
        };
        if partial.counted > LEVELS {
            return;
        }
        let pattern = self.pattern;
        let kind = *pattern.get_node(node);
        if matches!(kind, SymKind::Symbol | SymKind::Constant) {
            return;
        }
        let children: SmallVec<[NodeId; 4]> = pattern.children(node).collect();
        // A pattern spells an access without the state it observes: the
        // row's last port.
        let access = matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory);
        for (instance, tile) in self.tiles.iter().enumerate() {
            if !tile.usable
                || tile.head != kind
                || tile.ports.len() != children.len() + usize::from(access)
                || ty.is_some_and(|ty| tile.result != Some(ty))
                || partial.slots + tile.reads.len() > OPERANDS
            {
                continue;
            }
            let mut longer = partial.clone();
            let Some(row) = self.row(instance, tile, &children, &mut longer) else {
                continue;
            };
            let covered = constant(&row.operands)
                .is_some_and(|held| self.covered[instance] & (1 << held) != 0);
            if covered {
                longer.push(row, place);
                self.tilings(longer, out);
            }
        }
        let fits = |spelled: &&Spelled| {
            spelled.head == kind
                && spelled.holes.len() == children.len()
                && ty.is_none_or(|ty| spelled.result == Some(ty))
        };
        for spelled in self.spelled.iter().filter(fits) {
            let mut longer = partial.clone();
            if self.spell(spelled, &children, place, &mut longer).is_some() {
                self.tilings(longer, out);
            }
        }
    }

    // The rows of `spelled` in place of the operator at `place`, what its
    // right side reads being the pattern's `children`: a symbol is an
    // operand of the rule, a constant a literal, and an operator is given
    // its row next, wherever the rows read it.
    fn spell(
        &self,
        spelled: &Spelled,
        children: &[NodeId],
        place: Place,
        partial: &mut Partial<NodeId, u32>,
    ) -> Option<()> {
        let pattern = self.pattern;
        let base = partial.rows.len() as u8;
        if base != 0 {
            partial.rows[usize::from(place.0)].operands[usize::from(place.1)] = Operand::Row(base);
        }
        for row in &spelled.rows {
            let operands = row.operands.iter().map(|operand| match *operand {
                Operand::Row(below) => Operand::Row(below + base),
                other => other,
            });
            partial.rows.push(Row {
                instance: row.instance,
                operands: operands.collect(),
            });
        }
        // What the rows name by number, in the order they first read it.
        let mut named: SmallVec<[(Operand, SmallVec<[Place; 2]>); 8]> = SmallVec::new();
        for (row, operands) in spelled.rows.iter().enumerate() {
            for (at, operand) in operands.operands.iter().enumerate() {
                if !matches!(operand, Operand::Register(_) | Operand::Immediate(_)) {
                    continue;
                }
                let place = (base + row as u8, at as u8);
                match named.iter_mut().find(|(known, _)| known == operand) {
                    Some((_, places)) => places.push(place),
                    None => named.push((*operand, smallvec![place])),
                }
            }
        }
        for (operand, places) in named {
            let encoded = matches!(operand, Operand::Immediate(_));
            let first = places[0];
            let tile = &self.tiles[partial.rows[usize::from(first.0)].instance as usize];
            let hole = spelled.holes.iter().position(|hole| *hole == operand);
            let child = hole.map(|hole| children[hole]);
            let leaf = child.map(|child| (pattern.get_node(child), pattern.get_leaf_data(child)));
            let content = match (child, leaf) {
                (None, _) => partial.fresh(encoded),
                (_, Some((SymKind::Symbol, Some(SymPayload::SymbolId(symbol))))) => {
                    if self.immediates.contains(symbol) != encoded {
                        return None;
                    }
                    match partial.read.iter().find(|(seen, _)| seen == symbol) {
                        Some(&(_, before)) => {
                            partial
                                .again
                                .extend(places.iter().map(|&place| (place, before)));
                            continue;
                        }
                        None => {
                            partial.read.push((*symbol, first));
                            partial.fresh(encoded)
                        }
                    }
                }
                (_, Some((SymKind::Constant, Some(SymPayload::Int(value))))) if encoded => {
                    let width = tile.widths[usize::from(first.1)]?;
                    Operand::Literal(value.to_u64() & ones(width))
                }
                (_, Some((SymKind::Symbol | SymKind::Constant, _))) => return None,
                (Some(child), _) => {
                    if encoded {
                        return None;
                    }
                    let known = partial.deferred.iter();
                    let mut known =
                        known.filter(|&&(known, _)| pattern.subgraph_eq(known, pattern, child));
                    let read = match known.next() {
                        Some(&(_, before)) => before,
                        None => {
                            partial.defer(child, first, tile.reads[usize::from(first.1)]);
                            first
                        }
                    };
                    let again = places.iter().filter(|&&place| place != read);
                    partial.again.extend(again.map(|&place| (place, read)));
                    OPEN
                }
            };
            for (row, at) in places {
                partial.rows[usize::from(row)].operands[usize::from(at)] = content;
            }
        }
        partial.slots += spelled
            .rows
            .iter()
            .map(|row| row.operands.len())
            .sum::<usize>();
        Some(())
    }

    // The row of `tile` whose head reads the pattern's `children`.
    fn row(
        &self,
        instance: usize,
        tile: &Tile,
        children: &[NodeId],
        partial: &mut Partial<NodeId, u32>,
    ) -> Option<Row> {
        let pattern = self.pattern;
        let at = partial.rows.len() as u8;
        let mut operands: SmallVec<[Option<Operand>; 4]> = smallvec![None; tile.reads.len()];
        for (&child, &port) in children.iter().zip(&tile.ports) {
            let child = match self.zero {
                true => undisplaced(pattern, self.immediates, child),
                false => child,
            };
            let literal = match (pattern.get_node(child), pattern.get_leaf_data(child)) {
                (SymKind::Constant, Some(SymPayload::Int(value))) => Some(value),
                _ => None,
            };
            // A constant the term fixes is matched, not read.
            let index = match port {
                Port::Fixed(fixed) if literal.is_some_and(|value| value.to_u64() == fixed) => {
                    continue;
                }
                Port::Fixed(_) => return None,
                Port::Operand(index) => index,
            };
            let place = (at, index as u8);
            let constant = tile.constants & (1 << index) != 0;
            let width = tile.widths[index].filter(|_| constant);
            let operand = match (pattern.get_node(child), pattern.get_leaf_data(child)) {
                // An immediate of the rule is a constant operand of the
                // row; any other symbol is a register.
                (SymKind::Symbol, Some(SymPayload::SymbolId(symbol))) => {
                    let immediate = self.immediates.contains(symbol);
                    if !constant && immediate {
                        return None;
                    }
                    if partial.before(symbol, place) {
                        OPEN
                    } else {
                        partial.read.push((*symbol, place));
                        partial.fresh(immediate)
                    }
                }
                (SymKind::Symbol | SymKind::Constant, _) => {
                    let (value, width) = literal.zip(width)?;
                    let all = value.to_u64() == ones(value.width().min(64) as u32);
                    Operand::Literal(if all {
                        ones(width)
                    } else {
                        value.to_u64() & ones(width)
                    })
                }
                _ => {
                    let spelled = self.types.get(child.index());
                    let wanted = spelled
                        .and_then(|ty| held_type(self.terms, ty))
                        .unwrap_or(tile.reads[index]);
                    if wanted != tile.reads[index] {
                        return None;
                    }
                    // A term the pattern spells twice is one row read twice.
                    let spelled = partial.deferred.iter();
                    let mut spelled =
                        spelled.filter(|&&(known, _)| pattern.subgraph_eq(known, pattern, child));
                    match spelled.next() {
                        Some(&(_, first)) => partial.again.push((place, first)),
                        None => partial.defer(child, place, wanted),
                    }
                    OPEN
                }
            };
            operands[index] = Some(operand);
        }
        let operands = operands.into_iter();
        Some(Row {
            instance: instance as u32,
            operands: operands
                .map(|operand| operand.unwrap_or_else(|| partial.fresh(false)))
                .collect(),
        })
    }
}

// The other operand of `node` where it adds one of `immediates`, bare or
// extended, to it; `node` itself where it does not.
fn undisplaced(pattern: &SemGraph, immediates: &[u32], node: NodeId) -> NodeId {
    let displacement = |node: NodeId| {
        let inner = match pattern.get_node(node) {
            SymKind::SExt | SymKind::ZExt => pattern.children(node).next(),
            _ => Some(node),
        };
        inner.is_some_and(|inner| {
            matches!(
                (pattern.get_node(inner), pattern.get_leaf_data(inner)),
                (SymKind::Symbol, Some(SymPayload::SymbolId(symbol))) if immediates.contains(symbol)
            )
        })
    };
    let children: SmallVec<[NodeId; 4]> = pattern.children(node).collect();
    match (pattern.get_node(node), children.as_slice()) {
        (SymKind::Add, &[base, by]) | (SymKind::Add, &[by, base])
            if displacement(by) && !displacement(base) =>
        {
            base
        }
        _ => node,
    }
}

pub(super) struct Pool {
    // Fewer rows first, then cheaper.
    pub(super) rules: Vec<Synthesized>,
    // Cheaper first.
    pub(super) materializers: Vec<Builder>,
    pub(super) floats: Vec<(u32, u32, u32)>,
    pub(super) forms: Vec<Form>,
}

struct Host<'a> {
    terms: &'a Terms,
    requirements: &'a [RegisterRequirement],
    ranges: &'a [super::rule::ImmRange],
    // Per type, by its number, what it means to an instruction and the
    // carrier of its values: read off the context once, so that a guard
    // asks no context and the searches of a round may run on any thread. A
    // type interned after has neither.
    meanings: Vec<Option<TypeArg>>,
    carriers: Vec<u64>,
}

impl<'a> Host<'a> {
    // `pointer` is how wide the target's pointers are: a pointer is the
    // bits of an address.
    fn new(
        context: &'a Context,
        pointer: u32,
        requirements: &'a [RegisterRequirement],
        ranges: &'a [super::rule::ImmRange],
    ) -> Self {
        let terms = context.terms();
        let address = terms.int(u64::from(pointer));
        let types = context.types_where(|_| true);
        let meaning = |&ty: &TypeId| {
            let pointer = || tir::ptr::PtrType::view(terms, ty).map(|_| address);
            value_type(terms, ty).or_else(pointer).map(TypeArg::Type)
        };
        let carrier = |&ty: &TypeId| carrier_word(carrier_of(context, Some(pointer), ty));
        Self {
            terms,
            requirements,
            ranges,
            meanings: types.iter().map(meaning).collect(),
            carriers: types.iter().map(carrier).collect(),
        }
    }

    fn meaning(&self, ty: u64) -> Option<&TypeArg> {
        self.meanings.get(ty as usize)?.as_ref()
    }
}

impl Externs<SemNode> for Host<'_> {
    fn call(&self, id: u32, _labels: &[&SemNode], args: &[u64], out: &mut [u64]) -> bool {
        match id {
            call::ACCEPTS => {
                let ty = self.meaning(args[1]);
                ty.is_some_and(|ty| self.requirements[args[0] as usize].accepts(self.terms, ty))
            }
            call::TYPED => {
                let named = self.meaning(args[0]);
                named.is_some() && named == self.meaning(args[1])
            }
            call::FITS => {
                let width = args[2] as u32;
                (1..=64).contains(&width) && {
                    let bits = crate::binding::signed_bits(args[1], width);
                    self.ranges[args[0] as usize]
                        .contains(&APInt::new_signed(u64::from(width), bits))
                }
            }
            call::CARRIER => match self.carriers.get(args[0] as usize) {
                Some(&carrier) => {
                    out[0] = carrier;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    fn shared(&self) -> Option<&(dyn Externs<SemNode> + Sync)> {
        Some(self)
    }
}

fn machine(form: usize, children: usize) -> SemNode {
    SemNode {
        kind: Kind::Ir(IrOp::template("isel", "inst", Vec::new())),
        payload: Some(SemPayload::Opaque(form as u32)),
        ty: None,
        children: vec![ClassId::from_raw(0); children],
        prov: tir::sem::Prov::None,
        carrier: None,
    }
}

fn immediate_leaf() -> SemNode {
    SemNode {
        kind: Kind::Ir(IrOp::template("isel", "imm", Vec::new())),
        payload: None,
        ty: None,
        children: Vec::new(),
        prov: tir::sem::Prov::None,
        carrier: None,
    }
}

fn lanes_leaf() -> SemNode {
    SemNode {
        kind: Kind::Ir(IrOp::template("isel", "lanes", Vec::new())),
        ..immediate_leaf()
    }
}

pub(super) fn form_of(node: &SemNode) -> Option<usize> {
    match (&node.kind, &node.payload) {
        (Kind::Ir(op), Some(SemPayload::Opaque(form))) if op.name == "inst" => Some(*form as usize),
        _ => None,
    }
}

struct Lowering<'a> {
    context: &'a Context,
    pointer: u32,
    graph: &'a mut SemEGraph,
}

impl Lowering<'_> {
    fn leaf(&mut self, node: SemNode, ty: Option<TypeId>) -> Ref {
        let carrier = ty.and_then(|ty| carrier_of(self.context, Some(self.pointer), ty));
        let reference = self.graph.add(node.carried(carrier));
        if let Some(ty) = ty {
            self.graph.raise_type(reference.class, ty.number() as u64);
        }
        reference
    }

    // An operand some constant defines. It has no type fact, so no register
    // operand of a pattern binds it: a rule over it reads the constant.
    fn immediate(&mut self, operand: usize, ty: TypeId) -> Ref {
        let carrier = carrier_of(self.context, Some(self.pointer), ty);
        let mut node = immediate_leaf();
        node.payload = Some(SemPayload::Opaque(operand as u32));
        self.graph.add(node.carried(carrier))
    }

    fn operator(&mut self, kind: SymKind, mut operands: Vec<Ref>, ty: Option<TypeId>) -> Ref {
        if matches!(kind, SymKind::SExt | SymKind::ZExt)
            && let Some(width) = class_int_binding(self.graph, operands[1])
        {
            let width = template_node(
                SymKind::Constant,
                Some(SymPayload::Int(minimal_unsigned_apint(width.to_u64()))),
                None,
            );
            operands[1] = self.graph.add(width);
        }
        if kind.is_commutative() {
            for operand in &mut operands {
                *operand = self.graph.find(*operand);
            }
            operands.sort();
        }
        let carrier = ty.and_then(|ty| carrier_of(self.context, Some(self.pointer), ty));
        let mut node = template_node(kind, None, ty).carried(carrier);
        node.children = operands.iter().map(|operand| operand.class).collect();
        let reference = self.graph.insert(node, &operands);
        if let Some(ty) = ty {
            self.graph.raise_type(reference.class, ty.number() as u64);
        }
        reference
    }

    // The constant `value` of type `ty`, a literal of a row.
    fn literal(&mut self, value: u64, ty: TypeId) -> Ref {
        let width = literal_width(self.context.terms(), ty).unwrap_or(64);
        let bits = value & ones(width);
        let node = template_node(
            SymKind::Constant,
            Some(SymPayload::Int(APInt::new(u64::from(width), bits))),
            Some(ty),
        );
        let leaf = self.leaf(node, Some(ty));
        // A pattern may spell the constant as its narrowest form, extended
        // to the width its context gives.
        let narrow = template_node(
            SymKind::Constant,
            Some(SymPayload::Int(minimal_unsigned_apint(bits))),
            None,
        );
        let narrow = self.graph.add(narrow);
        let extent = template_node(
            SymKind::Constant,
            Some(SymPayload::Int(minimal_unsigned_apint(u64::from(width)))),
            None,
        );
        let extent = self.graph.add(extent);
        let extended = self.operator(SymKind::ZExt, vec![narrow, extent], Some(ty));
        let _ = self.graph.union(extended, leaf);
        leaf
    }

    // The term of `rows` over the leaves of the rule's operands, and those
    // leaves by operand number.
    fn rows(&mut self, scratch: &Context, instances: &[Instance], rows: &Rows) -> (Ref, Vec<Ref>) {
        let mut terms: SmallVec<[Option<Ref>; 4]> = smallvec![None; rows.len()];
        let mut leaves = Vec::new();
        let root = self.row(scratch, instances, rows, 0, &mut terms, &mut leaves);
        let leaves = leaves.into_iter();
        let leaves = leaves.map(|leaf| leaf.unwrap_or_else(|| self.unnamed()));
        (root, leaves.collect())
    }

    fn row(
        &mut self,
        scratch: &Context,
        instances: &[Instance],
        rows: &Rows,
        at: usize,
        terms: &mut SmallVec<[Option<Ref>; 4]>,
        leaves: &mut Vec<Option<Ref>>,
    ) -> Ref {
        if let Some(term) = terms[at] {
            return term;
        }
        let instance = &instances[rows[at].instance as usize];
        let read = scratch.get_op(instance.op).operands();
        let mut operands: SmallVec<[Ref; 4]> = SmallVec::new();
        for (&value, &operand) in read.iter().zip(&rows[at].operands) {
            let ty = scratch.get_value(value).ty();
            let number = match operand {
                Operand::Row(below) => {
                    let below = usize::from(below);
                    operands.push(self.row(scratch, instances, rows, below, terms, leaves));
                    continue;
                }
                Operand::Literal(value) => {
                    operands.push(self.literal(value, ty));
                    continue;
                }
                Operand::Register(number) | Operand::Immediate(number) => usize::from(number),
            };
            if leaves.len() <= number {
                leaves.resize(number + 1, None);
            }
            let leaf = match (leaves[number], operand) {
                (Some(leaf), _) => leaf,
                (None, Operand::Immediate(_)) => self.immediate(number, ty),
                (None, _) => {
                    let symbol = Some(SymPayload::SymbolId(number as u32));
                    self.leaf(template_node(SymKind::Symbol, symbol, Some(ty)), Some(ty))
                }
            };
            leaves[number] = Some(leaf);
            operands.push(leaf);
        }
        // A vector operation's term names the length operand it goes
        // without where its type holds no lane count: the count is then the
        // one of the type a row computes at, whatever that is.
        let context = self.context;
        let lanes = instance
            .lanes
            .then(|| self.leaf(lanes_leaf(), Some(tir::builtin::IndexType::new(context))));
        operands.extend(lanes);
        let types = instance.types.as_deref();
        let root = self
            .term(&instance.semantics, instance.root, &operands, types)
            .unwrap_or_else(|| self.leaf(template_node(SymKind::Symbol, None, None), None));
        terms[at] = Some(root);
        root
    }

    fn unnamed(&mut self) -> Ref {
        let mut node = template_node(SymKind::Symbol, None, None);
        node.payload = Some(SemPayload::Opaque(u32::MAX));
        self.graph.add(node)
    }

    fn term(
        &mut self,
        graph: &SemGraph,
        node: NodeId,
        operands: &[Ref],
        types: Option<&[TypeArg]>,
    ) -> Option<Ref> {
        // A type that is bits under another name, an environment snapshot,
        // labels its term as the integer the patterns name.
        let terms = self.context.terms();
        let bits = |ty: TypeId| match value_type(terms, ty) {
            Some(meaning) if terms.ctor(meaning) == ctor::INT => meaning,
            _ => ty,
        };
        let ty = graph
            .get_annotation(node)
            .and_then(|meta| meta.actual_type)
            .map(bits)
            .or_else(|| types.and_then(|types| held_type(terms, &types[node.index()])));
        match graph.get_node(node) {
            SymKind::Symbol => match graph.get_leaf_data(node) {
                Some(SymPayload::SymbolId(id)) => operands.get(*id as usize).copied(),
                _ => None,
            },
            SymKind::Constant => match graph.get_leaf_data(node) {
                Some(SymPayload::Int(value)) => {
                    let node =
                        template_node(SymKind::Constant, Some(SymPayload::Int(value.clone())), ty);
                    Some(self.leaf(node, ty))
                }
                _ => None,
            },
            kind => {
                let children = graph
                    .children(node)
                    .map(|child| self.term(graph, child, operands, types))
                    .collect::<Option<Vec<Ref>>>()?;
                // An access term carries the state it observes after the
                // operands the vocabulary gives it.
                let stateful = matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory)
                    && children.len() == kind.arity() + 1;
                (stateful || kind.accepts_arity(children.len()))
                    .then(|| self.operator(*kind, children, ty))
            }
        }
    }
}

struct Compiler<'a> {
    terms: &'a Terms,
    rule: &'a Rule,
    pattern: &'a SemGraph,
    types: Vec<TypeArg>,
    vars: u32,
    scalars: u32,
    atoms: Vec<Atom<SemNode>>,
    guards: Vec<Guard>,
    captures: BTreeMap<u32, u32>,
    states: Vec<u32>,
    environment: Option<u32>,
    plain: std::collections::BTreeSet<u32>,
    requirements: &'a mut Vec<RegisterRequirement>,
    ranges: &'a mut Vec<super::rule::ImmRange>,
    literal: bool,
    zero: bool,
    zeros: Vec<(u32, u32)>,
}

impl Compiler<'_> {
    fn int_width(&self, node: NodeId) -> Option<u64> {
        self.terms.int_width(self.types[node.index()].as_type()?)
    }

    // The immediate operand `node` is, bare or extended, and its width.
    fn displacement(&self, node: NodeId) -> Option<(u32, u32)> {
        let inner = match self.pattern.get_node(node) {
            SymKind::SExt | SymKind::ZExt => self.pattern.children(node).next()?,
            _ => node,
        };
        let Some(SymPayload::SymbolId(symbol)) = self.pattern.get_leaf_data(inner) else {
            return None;
        };
        let constraints = self.rule.operand_constraints.iter();
        let immediate = constraints
            .filter(|(candidate, _)| candidate == symbol)
            .any(|(_, kind)| matches!(kind, OperandConstraint::Immediate));
        // A field that cannot hold zero has no zero form.
        let fields = self.rule.operand_imm_ranges.iter();
        let holds = fields
            .filter(|(candidate, _)| candidate == symbol)
            .all(|(_, range)| range.contains(&APInt::new(32, 0)));
        if !immediate || !holds || *self.pattern.get_node(inner) != SymKind::Symbol {
            return None;
        }
        // Zero is zero at any width: an immediate whose own width is not
        // spelled is taken as a 32-bit field.
        let width = match self.int_width(inner) {
            Some(width @ 1..=64) => width as u32,
            _ => 32,
        };
        Some((*symbol, width))
    }

    fn var(&mut self) -> u32 {
        self.vars += 1;
        self.vars - 1
    }

    fn scalar(&mut self) -> Scalar {
        self.scalars += 1;
        self.scalars - 1
    }

    fn requirement(&mut self, requirement: RegisterRequirement) -> i64 {
        let known = self
            .requirements
            .iter()
            .position(|known| *known == requirement);
        known.unwrap_or_else(|| {
            self.requirements.push(requirement);
            self.requirements.len() - 1
        }) as i64
    }

    fn accepts(&mut self, var: u32, requirement: RegisterRequirement) -> Scalar {
        let ty = self.scalar();
        self.atoms.push(Atom::Fact {
            column: ColumnId::Type,
            key: var,
            value: ty,
        });
        let requirement = self.requirement(requirement);
        self.guards.push(Guard::Extern {
            call: call::ACCEPTS,
            terms: SmallVec::new(),
            args: smallvec![Expr::Lit(requirement), Expr::Scalar(ty)],
            out: SmallVec::new(),
        });
        ty
    }

    // The variable an operand symbol binds: a register's value, an
    // immediate, or a structural integer.
    fn symbol(&mut self, node: NodeId, structural: bool) -> Option<u32> {
        let Some(SymPayload::SymbolId(symbol)) = self.pattern.get_leaf_data(node) else {
            return None;
        };
        let symbol = *symbol;
        if !structural {
            self.plain.insert(symbol);
        }
        if let Some(&var) = self.captures.get(&symbol) {
            return Some(var);
        }
        // An immediate taken as zero is nothing else anywhere.
        if self.zeros.iter().any(|&(zero, _)| zero == symbol) {
            return None;
        }
        let immediate = self
            .rule
            .operand_constraints
            .iter()
            .any(|(candidate, kind)| {
                *candidate == symbol && matches!(kind, OperandConstraint::Immediate)
            });
        let state = self.types[node.index()].as_type();
        if state.is_some_and(|ty| self.terms.ctor(ty) == ctor::SEM_STATE) {
            return None;
        }
        let var = self.var();
        self.captures.insert(symbol, var);
        if immediate && self.literal {
            // A constant the term itself names, such as the mask an
            // axiom spells an extension with: it has to fit the
            // field here, since no row will be asked.
            let (value, width) = (self.scalar(), self.scalar());
            self.atoms.push(Atom::Const { key: var, value });
            self.atoms.push(Atom::Width { key: var, width });
            let range = self
                .rule
                .operand_imm_ranges
                .iter()
                .find(|(s, _)| *s == symbol);
            let range = range.map_or(WHOLE, |&(_, range)| range);
            let known = self.ranges.iter().position(|known| *known == range);
            let known = known.unwrap_or_else(|| {
                self.ranges.push(range);
                self.ranges.len() - 1
            });
            self.guards.push(Guard::Extern {
                call: call::FITS,
                terms: SmallVec::new(),
                args: smallvec![
                    Expr::Lit(known as i64),
                    Expr::Scalar(value),
                    Expr::Scalar(width)
                ],
                out: SmallVec::new(),
            });
            return Some(var);
        }
        if immediate {
            self.atoms.push(Atom::Node {
                template: immediate_leaf(),
                args: SmallVec::new(),
                class: var,
                row: None,
            });
            return Some(var);
        }
        // A register operand reads a value with a type. A constant
        // operand's leaf has none, so only an immediate binds it.
        let requirement = self
            .rule
            .operand_registers
            .iter()
            .find(|(candidate, _)| *candidate == symbol);
        let ty = match requirement {
            Some(&(_, requirement)) => Some(self.accepts(var, requirement)),
            None if structural => None,
            None => {
                let ty = self.scalar();
                self.atoms.push(Atom::Fact {
                    column: ColumnId::Type,
                    key: var,
                    value: ty,
                });
                Some(ty)
            }
        };
        // A pattern that names its operand's type (the low word a
        // conversion reads) binds an operand of that type only.
        let named = self.pattern.get_annotation(node);
        if let (Some(ty), Some(named)) = (ty, named.and_then(|meta| meta.actual_type)) {
            self.guards.push(Guard::Extern {
                call: call::TYPED,
                terms: SmallVec::new(),
                args: smallvec![Expr::Lit(i64::from(named.number())), Expr::Scalar(ty)],
                out: SmallVec::new(),
            });
        }
        Some(var)
    }

    fn node(&mut self, node: NodeId, structural: bool) -> Option<u32> {
        match self.pattern.get_node(node) {
            SymKind::Symbol => self.symbol(node, structural),
            SymKind::Constant => {
                let Some(SymPayload::Int(value)) = self.pattern.get_leaf_data(node) else {
                    return None;
                };
                let value = match self.int_width(node) {
                    _ if structural => minimal_unsigned_apint(value.to_u64()),
                    Some(width) => APInt::new(width, value.to_u64()),
                    None => value.clone(),
                };
                let var = self.var();
                self.atoms.push(Atom::Literal {
                    value: SemNode::constant(value, tir::sem::Prov::None),
                    class: var,
                });
                Some(var)
            }
            kind => {
                let kind = *kind;
                let memory = matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory);
                let children: Vec<NodeId> = self.pattern.children(node).collect();
                // A value plus a zero displacement is the value: the form
                // of an instruction with a displacement field that reads an
                // address with none.
                if let (true, SymKind::Add, &[first, second]) =
                    (self.zero && self.vars != 0, kind, children.as_slice())
                {
                    for (base, by) in [(first, second), (second, first)] {
                        if let (Some((symbol, width)), None) =
                            (self.displacement(by), self.displacement(base))
                        {
                            if self.captures.contains_key(&symbol) {
                                return None;
                            }
                            self.zeros.push((symbol, width));
                            return self.node(base, structural);
                        }
                    }
                }
                let var = self.var();
                // A sum with a constant has no row in the pool: it is its
                // other operand's class at that offset. The root keeps its
                // row, which is what a plan starts from.
                // The engine keeps an offset modulo two to the carrier width.
                let literal = |child: &NodeId| match self.pattern.get_leaf_data(*child) {
                    Some(SymPayload::Int(value))
                        if *self.pattern.get_node(*child) == SymKind::Constant =>
                    {
                        Some(value.to_u64())
                    }
                    _ => None,
                };
                let width = match self.int_width(node) {
                    Some(width @ 1..=64) => Some(u64::MAX >> (64 - width)),
                    _ => None,
                };
                let summed = match (kind, children.as_slice()) {
                    _ if var == 0 => None,
                    (SymKind::Add, [base, by]) | (SymKind::Add, [by, base])
                        if literal(by).is_some() && literal(base).is_none() =>
                    {
                        literal(by).zip(width).map(|(by, mask)| (*base, by & mask))
                    }
                    (SymKind::Sub, [base, by]) if literal(base).is_none() => {
                        let by = literal(by).zip(width);
                        by.map(|(by, mask)| (*base, by.wrapping_neg() & mask))
                    }
                    _ => None,
                };
                if let Some((base, by)) = summed {
                    let base = self.node(base, false)?;
                    let offset = self.scalar();
                    self.atoms.push(Atom::Offset {
                        key: var,
                        base,
                        offset,
                    });
                    let (found, asked) = (Expr::Scalar(offset), Expr::Lit(by as i64));
                    self.guards
                        .push(Guard::Cmp(tir_relational::Cmp::Eq, found, asked));
                    return Some(var);
                }
                let mut args: SmallVec<[u32; 4]> = SmallVec::new();
                for (index, child) in children.into_iter().enumerate() {
                    // Extension and conversion widths, access sizes and
                    // address spaces are structural integers, spelled one
                    // way.
                    let structural = width_or_size(kind, index);
                    // The environment a pattern reads or assigns is the one
                    // the row observes, whatever defines it: one state for
                    // the whole pattern, threaded through the instruction.
                    let chained = *self.pattern.get_node(child) == SymKind::StateAssign;
                    let environment = match (kind, index) {
                        (SymKind::StateRead | SymKind::StateAssign, 0) => !chained,
                        (SymKind::FPEffect, 1) => true,
                        _ => false,
                    };
                    if environment {
                        let state = match self.environment {
                            Some(state) => state,
                            None => {
                                let state = self.var();
                                self.states.push(state);
                                self.environment = Some(state);
                                state
                            }
                        };
                        args.push(state);
                        continue;
                    }
                    args.push(self.node(child, structural)?);
                }
                // A rule spells an access without the state it observes.
                if memory {
                    let state = self.var();
                    args.push(state);
                    self.states.push(state);
                }
                // What a pattern reads off the environment is typed by its
                // register, not by the field read.
                let ty = self
                    .pattern
                    .get_annotation(node)
                    .and_then(|meta| meta.actual_type)
                    .filter(|_| kind != SymKind::StateRead);
                let mut template = template_node(kind, None, ty);
                template.children = vec![ClassId::from_raw(0); args.len()];
                // The root is variable zero and its atom the first one.
                self.atoms.push(Atom::Node {
                    template,
                    args,
                    class: var,
                    row: None,
                });
                Some(var)
            }
        }
    }
}

// Whether operand `index` of `kind` is a structural integer, spelled one
// way, and no value: an extension or conversion width, an access size, an
// address space.
fn width_or_size(kind: SymKind, index: usize) -> bool {
    matches!(
        (kind, index),
        (SymKind::SExt | SymKind::ZExt, 1)
            | (SymKind::LoadMemory, 1 | 2)
            | (SymKind::StoreMemory, 1 | 3)
            | (SymKind::FPToSI | SymKind::FPToUI, 1)
            | (SymKind::FPToSIRound | SymKind::FPToUIRound, 1)
            | (SymKind::SIToFP | SymKind::UIToFP, 1 | 2)
            | (SymKind::SIToFPRound | SymKind::UIToFPRound, 1 | 2)
    )
}

fn pattern_types(terms: &Terms, rule: &Rule, pattern: &SemGraph) -> Option<Vec<TypeArg>> {
    infer_types(terms, pattern, |node| {
        let Some(SymPayload::SymbolId(symbol)) = pattern.get_leaf_data(node) else {
            return None;
        };
        rule.operand_registers
            .iter()
            .find(|(candidate, _)| candidate == symbol)
            .and_then(|(_, requirement)| requirement.exclusive_float_type(terms).ok().flatten())
    })
    .ok()
}

// `None` for a pattern this step does not match yet: one with an immediate
// operand, a memory access, or a bare operand at the root.
#[allow(clippy::too_many_arguments)]
fn compile(
    terms: &Terms,
    rule: &Rule,
    pattern: &SemGraph,
    form: usize,
    requirements: &mut Vec<RegisterRequirement>,
    ranges: &mut Vec<super::rule::ImmRange>,
    literal: bool,
    zero: bool,
) -> Option<(EngineRule<SemNode>, Vec<u32>, Vec<bool>)> {
    let root = pattern.root()?;
    if matches!(pattern.get_node(root), SymKind::Symbol | SymKind::Constant) {
        return None;
    }
    // A value lives at bit zero of its register and no instruction moves
    // bits across views: a rule over a shifted view (x86 `ah`) reads and
    // defines none.
    let read = rule
        .operand_registers
        .iter()
        .map(|(_, requirement)| requirement);
    if read
        .chain(&rule.result_register)
        .any(|requirement| requirement.view_offset() != 0)
    {
        return None;
    }
    let types = pattern_types(terms, rule, pattern)?;
    let mut compiler = Compiler {
        terms,
        rule,
        pattern,
        types,
        vars: 0,
        scalars: 0,
        atoms: Vec::new(),
        guards: Vec::new(),
        captures: BTreeMap::new(),
        states: Vec::new(),
        environment: None,
        plain: Default::default(),
        requirements,
        ranges,
        literal,
        zero,
        zeros: Vec::new(),
    };
    // A register plus a literal is no row of the pool: it is the register's
    // class at that offset, so the pattern matches the register's leaf and
    // its instruction computes that class at the offset.
    let literal = |node: NodeId| match pattern.get_leaf_data(node) {
        Some(SymPayload::Int(value)) if *pattern.get_node(node) == SymKind::Constant => {
            Some(value.to_u64())
        }
        _ => None,
    };
    let leaf = |node: NodeId| *pattern.get_node(node) == SymKind::Symbol;
    let children: SmallVec<[NodeId; 4]> = pattern.children(root).collect();
    let shifted = match (pattern.get_node(root), children.as_slice()) {
        (SymKind::Add, &[base, by]) | (SymKind::Add, &[by, base]) if leaf(base) => {
            literal(by).map(|by| (base, by))
        }
        (SymKind::Sub, &[base, by]) if leaf(base) => {
            literal(by).map(|by| (base, by.wrapping_neg()))
        }
        _ => None,
    };
    let bound = match shifted {
        Some((base, _)) => {
            let bound = compiler.node(base, false)?;
            let operand = Atom::Node {
                template: template_node(SymKind::Symbol, None, None),
                args: SmallVec::new(),
                class: bound,
                row: None,
            };
            compiler.atoms.insert(0, operand);
            bound
        }
        None => compiler.node(root, false)?,
    };
    debug_assert_eq!(bound, 0);
    // The zero form of a pattern with no displacement is the pattern.
    if zero && compiler.zeros.is_empty() {
        return None;
    }
    // Planning a query costs more than its atoms many times over: a pattern
    // this large (a bitmask immediate spelled out, say) is left out.
    if compiler.atoms.len() > ATOMS {
        return None;
    }
    // The plan reads the root's row first whatever order the atoms came in.
    let ty = match rule.result_register {
        Some(requirement) => compiler.accepts(0, requirement),
        None => {
            let ty = compiler.scalar();
            compiler.atoms.push(Atom::Fact {
                column: ColumnId::Type,
                key: 0,
                value: ty,
            });
            ty
        }
    };
    let carrier = compiler.scalar();
    compiler.guards.push(Guard::Extern {
        call: call::CARRIER,
        terms: SmallVec::new(),
        args: smallvec![Expr::Scalar(ty)],
        out: smallvec![carrier],
    });
    // A displacement taken as zero is the constant zero, which the rule
    // itself puts in the pool.
    let mut head = Vec::new();
    let mut into = compiler.vars;
    for &(symbol, width) in &compiler.zeros {
        let zero = SemNode::constant(APInt::new(u64::from(width), 0), tir::sem::Prov::None);
        head.push(HeadOp::Insert {
            label: LabelFill::plain(zero),
            args: SmallVec::new(),
            into,
        });
        compiler.captures.insert(symbol, into);
        into += 1;
    }
    let symbols: Vec<u32> = compiler.captures.keys().copied().collect();
    // An operand is read from a register unless it is encoded or structural.
    let registers: Vec<bool> = symbols
        .iter()
        .map(|symbol| {
            let immediate = rule
                .operand_constraints
                .iter()
                .any(|(s, kind)| s == symbol && matches!(kind, OperandConstraint::Immediate));
            !immediate && compiler.plain.contains(symbol)
        })
        .collect();
    let args: SmallVec<[u32; 4]> = compiler
        .captures
        .values()
        .chain(&compiler.states)
        .copied()
        .collect();
    let computed = match shifted {
        Some((_, by)) => {
            head.push(HeadOp::Offset {
                base: 0,
                offset: Expr::Lit(by as i64),
                into: into + 1,
            });
            into + 1
        }
        None => 0,
    };
    head.extend([
        HeadOp::Insert {
            label: LabelFill {
                template: machine(form, args.len()),
                fills: smallvec![(field::TY, ty), (field::CARRIER, carrier)],
            },
            args,
            into,
        },
        HeadOp::Union(into, computed),
    ]);
    let zeros = compiler.zeros.len() as u32 + u32::from(shifted.is_some());
    let query = Query {
        vars: compiler.vars,
        scalars: compiler.scalars,
        root: 0,
        atoms: compiler.atoms,
        guards: compiler.guards,
        nots: Vec::new(),
    };
    Some((
        EngineRule {
            name: format!("inst-{}", rule.name),
            plan: Plan::compile(query),
            head,
            head_vars: zeros + 1,
            post_saturation: false,
        },
        symbols,
        registers,
    ))
}

// The instruction tree computing `reference` from the operand `leaves`,
// appended to `tree`; what a reader of `reference` reads.
fn cover(
    graph: &SemEGraph,
    extraction: &tir_egraph::Extraction<'_, SemNode>,
    forms: &[Form],
    leaves: &[Ref],
    reference: Ref,
    depth: usize,
    tree: &mut Tree,
) -> Option<Arg> {
    let reference = graph.find(reference);
    if let Some(at) = leaves
        .iter()
        .position(|&leaf| graph.find(leaf) == reference)
    {
        return u8::try_from(at).ok().map(Arg::Operand);
    }
    if let Some(value) = graph.int_const(reference)
        && graph
            .width(reference.class)
            .is_none_or(|width| (1..=64).contains(&width))
    {
        return Some(Arg::Literal(
            value,
            graph.width(reference.class).unwrap_or(64),
        ));
    }
    // A class is extracted at one offset: an instruction computing it at
    // another is a row of its own there.
    let computes = |row| match form_of(graph.node(row)) {
        Some(form) if forms[form].branch.is_none() && graph.value(row) == reference => {
            Some(forms[form].cost)
        }
        _ => None,
    };
    let row = extraction.row(reference.class)?;
    let row = match graph.value(row) == reference {
        true => row,
        false => graph
            .rows(reference.class)
            .filter(|&row| computes(row).is_some())
            .min_by_key(|&row| computes(row))?,
    };
    let counts = graph
        .node(row)
        .kind
        .ir()
        .is_some_and(|op| op.name == "lanes");
    if counts && graph.value(row) == reference {
        return Some(Arg::Lanes);
    }
    let read = graph.children(row);
    let apart = read
        .first()
        .is_some_and(|&source| graph.find(source) != reference);
    if apart && graph.value(row) == reference && view(graph, graph.node(row), &read) {
        return cover(graph, extraction, forms, leaves, read[0], depth, tree);
    }
    let form = form_of(graph.node(row))?;
    // Among the rows of this form over the same operands, as a commutative
    // instruction matches, the one reading them in the row's order: the
    // choice is the same whatever order the matches were found in.
    let reads = |row| {
        let mut reads: SmallVec<[Ref; 4]> = SmallVec::new();
        reads.extend(graph.children(row).iter().map(|&child| graph.find(child)));
        reads
    };
    let place = |row| -> SmallVec<[usize; 4]> {
        let reads = reads(row);
        let place = |read: &Ref| leaves.iter().position(|&leaf| graph.find(leaf) == *read);
        reads
            .iter()
            .map(|read| place(read).unwrap_or(usize::MAX))
            .collect()
    };
    let mut held = reads(row);
    held.sort_unstable();
    let row = graph
        .rows(reference.class)
        .filter(|&other| {
            form_of(graph.node(other)) == Some(form) && graph.value(other) == reference
        })
        .filter(|&other| {
            let mut reads = reads(other);
            reads.sort_unstable();
            reads == held
        })
        .min_by_key(|&other| place(other))
        .unwrap_or(row);
    // A branch computes no value, whatever the extraction settled on.
    if depth == 0 || graph.value(row) != reference || forms[form].branch.is_some() {
        return None;
    }
    let args = graph
        .children(row)
        .iter()
        .map(|&child| cover(graph, extraction, forms, leaves, child, depth - 1, tree))
        .collect::<Option<SmallVec<[Arg; 4]>>>()?;
    tree.push(Step {
        form: form as u32,
        args,
    });
    u8::try_from(tree.len() - 1).ok().map(Arg::Step)
}

// The tree of `root` whose last instruction `admits` allows: `tree`, the
// cheapest one, where it does, else the cheapest among the instructions of
// the root's class it allows, each over the cheapest trees of what it reads.
fn admitted(
    graph: &SemEGraph,
    extraction: &tir_egraph::Extraction<'_, SemNode>,
    forms: &[Form],
    leaves: &[Ref],
    (root, tree): (Ref, Tree),
    admits: &dyn Fn(&Step) -> bool,
) -> Option<Tree> {
    if tree.last().is_some_and(admits) {
        return Some(tree);
    }
    let price = |tree: &Tree| -> u64 {
        let costs = tree.iter().map(|step| forms[step.form as usize].cost);
        costs.map(u64::from).sum()
    };
    let mut best: Option<Tree> = None;
    for tree in spellings(graph, extraction, forms, leaves, root) {
        if tree.last().is_some_and(admits)
            && best.as_ref().is_none_or(|best| price(&tree) < price(best))
        {
            best = Some(tree);
        }
    }
    best
}

// The trees of `root` that end in each instruction of its class, over the
// cheapest trees of what that instruction reads.
fn spellings(
    graph: &SemEGraph,
    extraction: &tir_egraph::Extraction<'_, SemNode>,
    forms: &[Form],
    leaves: &[Ref],
    root: Ref,
) -> Vec<Tree> {
    let root = graph.find(root);
    let mut spelled = Vec::new();
    for row in graph.rows(root.class) {
        let Some(form) = form_of(graph.node(row)) else {
            continue;
        };
        if graph.value(row) != root || forms[form].branch.is_some() {
            continue;
        }
        let mut tree = Tree::new();
        let read = graph.children(row);
        let depth = DEPTH - 1;
        let args = read
            .iter()
            .map(|&child| cover(graph, extraction, forms, leaves, child, depth, &mut tree))
            .collect::<Option<SmallVec<[Arg; 4]>>>();
        if let Some(args) = args {
            tree.push(Step {
                form: form as u32,
                args,
            });
            spelled.push(tree);
        }
    }
    spelled
}

// Whether `node` over `operands` extracts a value's bits from bit zero up.
fn view(graph: &SemEGraph, node: &SemNode, operands: &[Ref]) -> bool {
    node.kind == SymKind::Extract && operands.len() == 3 && graph.int_const(operands[2]) == Some(0)
}

#[derive(Default)]
pub(super) struct Report {
    pub(super) lower_us: u128,
    pub(super) saturate_us: u128,
    pub(super) compile_us: u128,
    pub(super) match_us: u128,
    pub(super) extract_us: u128,
    pub(super) prune_us: u128,
    pub(super) seeded: usize,
    pub(super) saturated: usize,
    pub(super) matched: usize,
    pub(super) tiled: usize,
    pub(super) found: usize,
    pub(super) patterns: usize,
    pub(super) compiled: usize,
    pub(super) covered: usize,
    pub(super) uncovered: Vec<u32>,
}

// The target's patterns as the pool matches them: the forms, the engine
// rules inserting their machine nodes, what those rules ask the host, and
// the forms that build constants.
#[derive(Default)]
struct Compiled {
    forms: Vec<Form>,
    patterns: Vec<EngineRule<SemNode>>,
    requirements: Vec<RegisterRequirement>,
    fields: Vec<ImmRange>,
    // Whether each rule's pattern names the type of what it computes.
    typed: Vec<bool>,
    materializers: Vec<Builder>,
    floats: Vec<(u32, u32, u32)>,
}

impl Compiled {
    fn new(context: &Context, rules: &[Rule], report: &mut Report) -> Self {
        let mut compiled = Self::default();
        let mut queries = Vec::new();
        for (index, rule) in rules.iter().enumerate() {
            let root = rule.pattern.root();
            let meta = root.and_then(|root| rule.pattern.get_annotation(root));
            compiled
                .typed
                .push(meta.is_some_and(|meta| meta.actual_type.is_some()));
            let graphs = std::iter::once(&rule.pattern).chain(&rule.secondary);
            // An instruction that accesses memory cannot be emitted for a
            // value it computes beside the access (the base a post-indexed
            // store writes back): that pattern alone covers no access.
            let accesses = rule.emit.iter().any(Emitter::accesses_memory);
            for (port, pattern) in graphs.enumerate() {
                report.patterns += 1;
                let access = |node| {
                    let kind = pattern.get_node(NodeId::from_index(node));
                    matches!(kind, SymKind::LoadMemory | SymKind::StoreMemory)
                };
                if !accesses || (0..pattern.len()).any(access) {
                    compiled.pattern(context, (index, rule), (port, pattern), &mut queries);
                }
            }
        }
        report.compiled = queries.len();
        compiled.patterns = merged(queries);
        let forms = &compiled.forms;
        compiled
            .materializers
            .sort_by_key(|builder| forms[builder.form as usize].cost);
        compiled
    }

    // The forms of one pattern of `rule`: the builder it is, if it builds a
    // constant, and one form per way its immediates are bound.
    fn pattern(
        &mut self,
        context: &Context,
        (index, rule): (usize, &Rule),
        (port, pattern): (usize, &SemGraph),
        queries: &mut Vec<EngineRule<SemNode>>,
    ) {
        let encoded = |symbol: u32| {
            let constraints = rule.operand_constraints.iter();
            constraints
                .filter(|(candidate, _)| *candidate == symbol)
                .any(|(_, kind)| matches!(kind, OperandConstraint::Immediate))
        };
        let range = |symbol: u32| {
            let range = rule.operand_imm_ranges.iter().find(|(s, _)| *s == symbol);
            range.map_or(WHOLE, |&(_, range)| range)
        };
        let form = |symbols: Vec<u32>, registers: Vec<bool>| Form {
            rule: index,
            port,
            ranges: symbols
                .iter()
                .map(|&symbol| encoded(symbol).then(|| range(symbol)))
                .collect(),
            symbols,
            registers,
            branch: match rule.kind {
                super::rule::RuleKind::CondBranch { target_symbol } => Some(target_symbol),
                super::rule::RuleKind::Value => None,
            },
            cost: rule.base_cost,
        };
        // A rule that makes a float of a register's bits builds the float
        // constants, whose rows have no class in the pool.
        if let (Some(width), 0) = (rule.float_constant_width, port) {
            let held = (0..pattern.len()).map(NodeId::from_index).find_map(|node| {
                match pattern.get_leaf_data(node) {
                    Some(SymPayload::SymbolId(symbol))
                        if *pattern.get_node(node) == SymKind::Symbol =>
                    {
                        Some(*symbol)
                    }
                    _ => None,
                }
            });
            if let Some(symbol) = held.filter(|&symbol| !encoded(symbol)) {
                self.floats.push((self.forms.len() as u32, symbol, width));
                self.forms.push(Form {
                    branch: None,
                    ..form(vec![symbol], vec![true])
                });
            }
        }
        // A pattern that builds a constant from an immediate is kept for
        // the constant rows, which have no class in the pool: a bare
        // immediate matches nothing else and stops here.
        if rule.float_constant_width.is_none()
            && let Some((symbol, shift, register, shifts, masks)) = builder(rule, pattern)
        {
            let width = pattern
                .root()
                .and_then(|root| pattern.get_annotation(root))
                .and_then(|meta| meta.actual_type)
                .and_then(|ty| context.terms().bit_width(ty))
                .map(|width| width as u32);
            let held = rule.result_register.map(|register| register.width());
            self.materializers.push(Builder {
                form: self.forms.len() as u32,
                symbol,
                range: range(symbol),
                shift,
                register,
                shifts,
                width,
                bits: width.or(held).unwrap_or(64),
                result: rule.result_register,
                masks,
            });
            self.forms.push(Form {
                ranges: vec![Some(range(symbol))],
                branch: None,
                ..form(vec![symbol], vec![false])
            });
            let root = pattern.root();
            if root.is_some_and(|root| matches!(pattern.get_node(root), SymKind::Symbol)) {
                return;
            }
        }
        if std::env::var("TIR_ISEL_RULE").is_ok_and(|named| rule.name.contains(&named)) {
            eprintln!("isel-rule: {} port {port}", rule.name);
            for at in 0..pattern.len() {
                let node = NodeId::from_index(at);
                let read: Vec<usize> = pattern.children(node).map(|c| c.index()).collect();
                let meta = pattern.get_annotation(node).and_then(|m| m.actual_type);
                eprintln!(
                    "isel-rule:   {at}: {:?} {:?} {read:?} {:?}",
                    pattern.get_node(node),
                    pattern.get_leaf_data(node),
                    meta.map(|ty| context.type_to_string(ty))
                );
            }
        }
        let mut immediates = rule.operand_constraints.iter();
        let immediates = immediates.any(|(_, kind)| matches!(kind, OperandConstraint::Immediate));
        let modes = [(false, false), (true, false), (false, true), (true, true)];
        for (literal, zero) in modes {
            if (literal || zero) && !immediates {
                continue;
            }
            let (requirements, fields) = (&mut self.requirements, &mut self.fields);
            let at = self.forms.len();
            if let Some((engine, symbols, registers)) = compile(
                context.terms(),
                rule,
                pattern,
                at,
                requirements,
                fields,
                literal,
                zero,
            ) {
                self.forms.push(form(symbols, registers));
                queries.push(engine);
            }
        }
    }

    // What building the constant `bits` of `width` bits costs.
    fn built(&self, bits: u64, width: u32) -> u64 {
        let width = width.clamp(1, 64);
        let value = APInt::new(u64::from(width), bits);
        let usable = |builder: &Builder| builder.builds(width);
        plan(&self.materializers, &value, usable).map_or(UNCOVERED, |built| {
            let steps = built.steps.iter();
            steps
                .map(|(step, _)| u64::from(self.forms[step.form as usize].cost))
                .sum()
        })
    }

    // What `tree` costs: its instructions, and building each literal one of
    // them reads from a register.
    fn price(&self, tree: &Tree) -> u64 {
        let instruction = |step: &Step| {
            let form = &self.forms[step.form as usize];
            let read = step.args.iter().zip(&form.registers);
            let held = read.filter(|(_, register)| **register);
            let built = held.filter_map(|(arg, _)| match *arg {
                Arg::Literal(bits, width) => Some(self.built(bits, width)),
                _ => None,
            });
            u64::from(form.cost) + built.sum::<u64>()
        };
        tree.iter().map(instruction).sum()
    }
}

// Patterns that ask one query are searched as one rule, which inserts the
// machine node of each. Queries are compared among those of one size.
fn merged(queries: Vec<EngineRule<SemNode>>) -> Vec<EngineRule<SemNode>> {
    let size = |rule: &EngineRule<SemNode>| {
        let query = rule.plan.query();
        let sizes = [query.atoms.len(), query.guards.len(), rule.head.len()];
        (query.vars, query.scalars, sizes)
    };
    let mut sized: HashMap<_, SmallVec<[usize; 4]>> = HashMap::new();
    let mut patterns: Vec<EngineRule<SemNode>> = Vec::with_capacity(queries.len());
    for engine in queries {
        let alike = sized.entry(size(&engine)).or_default();
        let query = engine.plan.query();
        let zeros = engine.head.len() - 2;
        let asked = alike.iter().copied().find(|&known| {
            let known = &patterns[known];
            let (asked, head) = (known.plan.query(), &known.head[..zeros]);
            asked.atoms == query.atoms
                && asked.guards == query.guards
                && head == &engine.head[..zeros]
        });
        let Some(first) = asked else {
            alike.push(patterns.len());
            patterns.push(engine);
            continue;
        };
        let first = &mut patterns[first];
        let into = first.plan.query().vars + first.head_vars;
        let mut head = engine.head;
        head.truncate(zeros + 1);
        if let Some(HeadOp::Insert { label, args, .. }) = head.pop() {
            first.head.push(HeadOp::Insert { label, args, into });
            first.head.push(HeadOp::Union(into, 0));
            first.head_vars += 1;
        }
    }
    patterns
}

// Saturate what was lowered, match the patterns against it, and pick the
// cheapest row of each class.
fn settle(
    context: &Context,
    graph: &mut SemEGraph,
    theory: &Theory,
    compiled: &Compiled,
    pointer: u32,
    report: &mut Report,
) -> Extraction<'static, SemNode> {
    let started = std::time::Instant::now();
    graph.rebuild();
    let limits = rewrites::SaturationLimits {
        max_nodes: graph.total_size().saturating_add(100_000),
        ..Default::default()
    };
    rewrites::saturate(context, graph, theory, limits);
    report.saturated = graph.total_size();
    report.saturate_us += started.elapsed().as_micros();

    let started = std::time::Instant::now();
    // Every type a class of the graph has is interned by now.
    let host = Host::new(context, pointer, &compiled.requirements, &compiled.fields);
    graph.mark_all_changed();
    graph.saturate_rules(&compiled.patterns, &host, 2, graph.total_size() + 1_000_000);
    report.matched = graph.total_size();
    report.match_us += started.elapsed().as_micros();

    let started = std::time::Instant::now();
    let graph = &*graph;
    let forms = &compiled.forms;
    let extraction = graph.extract_best(|_, node, operands, _| match form_of(node) {
        // A branch computes nothing a reader could take.
        Some(form) if forms[form].branch.is_some() => UNCOVERED,
        Some(form) => {
            // A constant read from a register is built first, at what
            // building that constant takes. One no rule builds cannot be
            // read from a register at all.
            let read = forms[form].registers.iter().zip(operands);
            let held = read.filter(|(register, _)| **register);
            let built = held.filter_map(|(_, operand)| {
                let value = graph.int_const(*operand)?;
                let width = graph.width(operand.class).unwrap_or(64).clamp(1, 64);
                let built = plan(
                    &compiled.materializers,
                    &APInt::new(u64::from(width), value),
                    |builder| builder.builds(width),
                );
                Some(built.map_or(UNCOVERED, |built| {
                    let steps = built.steps.iter();
                    steps
                        .map(|(step, _)| u64::from(forms[step.form as usize].cost))
                        .sum()
                }))
            });
            // At one cost the rule that names its type wins.
            ((u64::from(forms[form].cost) + built.sum::<u64>()) * TYPED)
                .saturating_sub(u64::from(compiled.typed[forms[form].rule]))
        }
        None if node.kind == SymKind::Symbol || node.kind == SymKind::Constant => 0,
        None if node
            .kind
            .ir()
            .is_some_and(|op| op.name == "imm" || op.name == "lanes") =>
        {
            0
        }
        // The low bits of a value are that value's register, read narrower:
        // no instruction, but a tree without the view is preferred at one
        // price.
        None if view(graph, node, operands) => 1,
        None => UNCOVERED,
    });
    report.extract_us += started.elapsed().as_micros();
    extraction
}

// A left side lowered into the pool: its term, and the leaf of each of its
// operands.
struct Candidate {
    rows: Rows,
    root: Ref,
    leaves: Vec<Ref>,
}

// What the instances are to tiling.
fn tiles(context: &Context, scratch: &Context, instances: &[Instance], pointer: u32) -> Vec<Tile> {
    // An address is the bits of a word: a pattern computes it as one.
    let word = tir::builtin::IntegerType::new(context, u64::from(pointer));
    let is = |ty: TypeId| {
        (
            tir::builtin::IntegerType::view(context, ty).is_some(),
            tir::ptr::PtrType::view(context, ty).is_some(),
        )
    };
    let types = |values: &[tir::ValueId]| {
        values
            .iter()
            .map(|&value| scratch.get_value(value).ty())
            .map(|ty| if is(ty).1 { word } else { ty })
            .collect::<SmallVec<[TypeId; 4]>>()
    };
    let identity = |instance: &Instance| (instance.schema.dialect, instance.schema.name);
    let tile = |instance: &Instance| {
        let op = scratch.get_op(instance.op);
        let semantics = &instance.semantics;
        let head = *semantics.get_node(instance.root);
        let ports = semantics
            .children(instance.root)
            .filter_map(|child| match semantics.get_leaf_data(child) {
                Some(SymPayload::SymbolId(operand)) => Some(Port::Operand(*operand as usize)),
                Some(SymPayload::Int(value)) => Some(Port::Fixed(value.to_u64())),
                _ => None,
            })
            .collect::<SmallVec<[Port; 5]>>();
        let access = matches!(head, SymKind::LoadMemory | SymKind::StoreMemory);
        let read = op.operands();
        let constants = read.iter().enumerate().filter(|&(_, &operand)| {
            let (integer, pointer) = is(scratch.get_value(operand).ty());
            integer || pointer
        });
        Tile {
            head,
            reads: types(&read),
            widths: read
                .iter()
                .map(|&operand| literal_width(scratch.terms(), scratch.get_value(operand).ty()))
                .collect(),
            // A write computes nothing: its term is of the unit type.
            result: types(&op.value_results())
                .first()
                .copied()
                .or(access.then(|| tir::builtin::UnitType::new(context))),
            constants: constants.fold(0, |held, (operand, _)| held | 1 << operand),
            usable: (instance.plain || access)
                && ports.len() == semantics.children(instance.root).count(),
            ports,
            commutative: IrOp::declared(context, identity(instance), Vec::new(), 0).commutative,
            access,
        }
    };
    instances.iter().map(tile).collect()
}

// The constants the target's patterns and the shared axioms name as an
// operand of each operator: a row of that operator is tried with its
// constant operand each of them.
fn spelled(rules: &[Rule], theory: &Theory) -> std::collections::BTreeSet<(SymKind, u64)> {
    let mut spelled = std::collections::BTreeSet::new();
    let patterns = rules
        .iter()
        .flat_map(|rule| std::iter::once(&rule.pattern).chain(&rule.secondary));
    for pattern in patterns {
        for node in (0..pattern.len()).map(NodeId::from_index) {
            let kind = *pattern.get_node(node);
            let values = pattern.children(node).enumerate();
            for (_, child) in values.filter(|&(index, _)| !width_or_size(kind, index)) {
                let under = match pattern.get_node(child) {
                    SymKind::SExt | SymKind::ZExt => pattern.children(child).next(),
                    _ => None,
                };
                let literal = under.unwrap_or(child);
                if let Some(SymPayload::Int(value)) = pattern.get_leaf_data(literal)
                    && *pattern.get_node(literal) == SymKind::Constant
                    && value.to_u64() < 64
                {
                    spelled.insert((kind, value.to_u64()));
                }
            }
        }
    }
    fn named(shape: &Shape, spelled: &mut std::collections::BTreeSet<(SymKind, u64)>) {
        let Shape::Operator(kind, read) = shape else {
            return;
        };
        for (index, operand) in read.iter().enumerate() {
            match operand {
                Shape::Constant(value) if !width_or_size(*kind, index) => {
                    spelled.insert((*kind, *value));
                }
                operand => named(operand, spelled),
            }
        }
    }
    for axiom in theory.axioms() {
        let (left, right) = axiom.sides();
        named(&left, &mut spelled);
        named(&right, &mut spelled);
    }
    spelled
}

// The left sides of one row: each instance over registers, with each
// operand a constant may define an immediate, and with that operand each
// literal the patterns name under the instance's operator.
fn singles(tiles: &[Tile], spelled: &std::collections::BTreeSet<(SymKind, u64)>) -> Vec<Rows> {
    let mut singles = Vec::new();
    for (instance, tile) in tiles.iter().enumerate() {
        let row = |held: Option<(usize, Operand)>| -> Rows {
            let operands = (0..tile.reads.len()).map(|at| match held {
                Some((held, operand)) if held == at => operand,
                _ => Operand::Register(at as u8),
            });
            smallvec![Row {
                instance: instance as u32,
                operands: operands.collect(),
            }]
        };
        singles.push(row(None));
        for at in (0..tile.reads.len()).filter(|&at| tile.constants & (1 << at) != 0) {
            singles.push(row(Some((at, Operand::Immediate(at as u8)))));
            let Some(width) = tile.widths[at] else {
                continue;
            };
            let values = spelled.iter().filter(|&&(kind, _)| kind == tile.head);
            for &(_, value) in values {
                singles.push(row(Some((at, Operand::Literal(value & ones(width))))));
            }
        }
    }
    singles
}

fn lowered(
    context: &Context,
    scratch: &Context,
    instances: &[Instance],
    pointer: u32,
    graph: &mut SemEGraph,
    sides: Vec<Rows>,
) -> Vec<Candidate> {
    let mut lowering = Lowering {
        context,
        pointer,
        graph,
    };
    let lower = |rows: Rows| {
        let (root, leaves) = lowering.rows(scratch, instances, &rows);
        Candidate { rows, root, leaves }
    };
    sides.into_iter().map(lower).collect()
}

// What covers a lowered left side: the settled pool, and what the target's
// instructions are in it.
struct Covering<'a> {
    graph: &'a SemEGraph,
    extraction: &'a Extraction<'static, SemNode>,
    compiled: &'a Compiled,
    rules: &'a [Rule],
    tiles: &'a [Tile],
    admits: &'a dyn Fn(usize, &[Form], &Step) -> bool,
    // What `admits` said of an instance and a last instruction: the variants
    // of one instance end in the same few.
    admitted: &'a Admitted,
}

type Admitted = std::cell::RefCell<HashMap<(usize, u32, SmallVec<[Arg; 4]>), bool>>;

impl Covering<'_> {
    // The rules of `candidate`: its cheapest tree, and where it holds an
    // immediate the tree ending in each other instruction of its class,
    // which may encode constants the cheapest does not. A left side that is
    // one of its operands or a constant whatever it reads takes no
    // instruction.
    fn rules(&self, candidate: &Candidate, out: &mut Vec<Synthesized>) {
        let Candidate { rows, root, leaves } = candidate;
        let (graph, extraction, forms) = (self.graph, self.extraction, &self.compiled.forms);
        let mut tree = Tree::new();
        let value = cover(graph, extraction, forms, leaves, *root, DEPTH, &mut tree);
        // One branch tests the value whatever tree computes it.
        let branch_of = || {
            let taken = self.branch(*root, leaves)?;
            Some(Branch {
                cost: u32::try_from(self.compiled.price(&taken)).ok()?,
                taken,
                inverse: self.inverse(*root, leaves),
            })
        };
        if let Some(value @ (Arg::Operand(_) | Arg::Literal(..))) = value {
            out.extend(self.rule(candidate, Tree::new(), value, branch_of()));
            return;
        }
        // A row that raises floating-point flags takes a tree only where
        // the tree's last instruction raises exactly those.
        let instance = rows[0].instance as usize;
        let admits = |step: &Step| {
            let asked = (instance, step.form, step.args.clone());
            let known = self.admitted.borrow().get(&asked).copied();
            rows.len() > 1
                || known.unwrap_or_else(|| {
                    let admits = (self.admits)(instance, forms, step);
                    self.admitted.borrow_mut().insert(asked, admits);
                    admits
                })
        };
        let held = |operand: &Operand| matches!(operand, Operand::Immediate(_));
        let encoded = rows.iter().any(|row| row.operands.iter().any(held));
        if !matches!(value, Some(Arg::Step(_))) {
            return;
        }
        let cheapest = admitted(graph, extraction, forms, leaves, (*root, tree), &admits);
        let others = (encoded && cheapest.is_some())
            .then(|| spellings(graph, extraction, forms, leaves, *root));
        let others = others.into_iter().flatten();
        let others = others.filter(|tree| tree.last().is_some_and(admits));
        let mut branch = None;
        for tree in cheapest.into_iter().chain(others) {
            let value = Arg::Step(tree.len() as u8 - 1);
            let branch = branch.get_or_insert_with(branch_of).clone();
            out.extend(self.rule(candidate, tree, value, branch));
        }
    }

    fn rule(
        &self,
        candidate: &Candidate,
        tree: Tree,
        value: Arg,
        branch: Option<Branch>,
    ) -> Option<Synthesized> {
        let rows = &candidate.rows;
        let forms = &self.compiled.forms;
        let cost = self.compiled.price(&tree);
        let cost = u32::try_from(cost).ok().filter(|_| cost < UNCOVERED)?;
        let mut fields: SmallVec<[(u8, ImmRange); 2]> = SmallVec::new();
        for operand in rows.iter().flat_map(|row| &row.operands) {
            let Operand::Immediate(number) = *operand else {
                continue;
            };
            if fields.iter().any(|&(known, _)| known == number) {
                continue;
            }
            let mut held = tree.iter().flat_map(|step| {
                let read = step.args.iter().zip(&forms[step.form as usize].ranges);
                let read = read.filter(move |(arg, _)| **arg == Arg::Operand(number));
                read.map(|(_, range)| range.unwrap_or(WHOLE))
            });
            let field = held.next().unwrap_or(WHOLE);
            // An immediate two fields hold has no one class of constants.
            if held.any(|other| other != field) {
                return None;
            }
            fields.push((number, field));
        }
        let last = tree.last().map(|step| &forms[step.form as usize]);
        Some(Synthesized {
            rows: rows.clone(),
            value,
            cost,
            fields,
            port: last.map_or(0, |form| form.port as u8),
            results: last.map_or(1, |form| 1 + self.rules[form.rule].secondary.len() as u8),
            effect: self.tiles[rows[0].instance as usize].access,
            branch,
            tree,
        })
    }

    // The branch testing the value of `root`: the cheapest branch row of
    // its class whose operands the pool covers.
    fn branch(&self, root: Ref, leaves: &[Ref]) -> Option<Tree> {
        let (graph, forms) = (self.graph, &self.compiled.forms);
        let root = graph.find(root);
        let mut best: Option<Tree> = None;
        for row in graph.rows(root.class) {
            let Some(form) = form_of(graph.node(row)) else {
                continue;
            };
            if forms[form].branch.is_none() || graph.value(row) != root {
                continue;
            }
            let mut tree = Tree::new();
            let read = graph.children(row);
            let args = read.iter().map(|&child| {
                let depth = DEPTH - 1;
                cover(
                    graph,
                    self.extraction,
                    forms,
                    leaves,
                    child,
                    depth,
                    &mut tree,
                )
            });
            let Some(args) = args.collect::<Option<SmallVec<[Arg; 4]>>>() else {
                continue;
            };
            tree.push(Step {
                form: form as u32,
                args,
            });
            let price = |tree: &Tree| self.compiled.price(tree);
            if best.as_ref().is_none_or(|best| price(&tree) < price(best)) {
                best = Some(tree);
            }
        }
        best
    }

    // The branch taken when `root` is false: the branch on the complement
    // of the comparison `root` is, over the same operands.
    fn inverse(&self, root: Ref, leaves: &[Ref]) -> Option<Tree> {
        let graph = self.graph;
        let root = graph.find(root);
        let complements = graph.rows(root.class).filter_map(|row| {
            let node = graph.node(row);
            let complement = complement_comparison(node.kind.sym()?)?;
            let mut complement = template_node(complement, None, node.ty).carried(node.carrier);
            let read = graph.children(row);
            complement.children = read.iter().map(|operand| operand.class).collect();
            graph.lookup_at(&complement, &read)
        });
        let branches = complements.filter_map(|complement| self.branch(complement, leaves));
        branches.min_by_key(|tree| self.compiled.price(tree))
    }
}

// The left sides of several rows: each pattern of more than one operator
// tiled with rows, top-down from its root, and the rows spelling each
// axiom's left side.
struct Backward<'a> {
    terms: &'a Terms,
    tiles: &'a [Tile],
    covered: &'a [u8],
}

impl Backward<'_> {
    fn patterns(&self, rules: &[Rule], spelled: &[Spelled], out: &mut Vec<Rows>) {
        for rule in rules {
            for pattern in std::iter::once(&rule.pattern).chain(&rule.secondary) {
                let operator = |node: &NodeId| {
                    !matches!(pattern.get_node(*node), SymKind::Symbol | SymKind::Constant)
                };
                let nodes = (0..pattern.len()).map(NodeId::from_index);
                let operators = nodes.clone().filter(operator).count();
                let Some(root) = pattern.root().filter(|_| operators > 1) else {
                    continue;
                };
                let Some(types) = pattern_types(self.terms, rule, pattern) else {
                    continue;
                };
                // A rule whose result lives in a float register computes a
                // float, whatever bits its pattern moves: a float load.
                let float = rule
                    .result_register
                    .filter(|_| std::ptr::eq(pattern, &rule.pattern));
                let float = float
                    .and_then(|register| register.exclusive_float_type(self.terms).ok().flatten());
                let wanted = float.map(TypeArg::Type);
                let wanted = wanted.or_else(|| types.get(root.index()).cloned());
                let wanted = wanted.and_then(|ty| held_type(self.terms, &ty));
                let immediates: Vec<u32> = rule
                    .operand_constraints
                    .iter()
                    .filter(|(_, constraint)| matches!(constraint, OperandConstraint::Immediate))
                    .map(|&(symbol, _)| symbol)
                    .collect();
                let displaced = nodes
                    .clone()
                    .any(|node| node != root && undisplaced(pattern, &immediates, node) != node);
                for zero in [false, true] {
                    if zero && !displaced {
                        continue;
                    }
                    let tiled = Tiled {
                        terms: self.terms,
                        tiles: self.tiles,
                        covered: self.covered,
                        pattern,
                        types: &types,
                        immediates: &immediates,
                        zero,
                        spelled,
                    };
                    tiled.tilings(Partial::new(root, wanted), out);
                }
            }
        }
    }

    // The left sides the settled pool holds for a row alone. A node of the
    // row's class is the row of each tile of its operator; an operand of it
    // is the rule operand whose leaf it is, a constant, or a row tiled the
    // same way from the nodes of the operand's class.
    //
    // An immediate's leaf has no type, so no axiom that names a width fires
    // over it. A row holding one is tiled from the class of the same row
    // over registers instead: what equals that row whatever its operands
    // hold equals it where a constant defines one. Those left sides are
    // listed in `instances` with the row they equal, since the pool does
    // not hold them in its class.
    fn roots(
        &self,
        graph: &SemEGraph,
        alone: &[Candidate],
        out: &mut Vec<Rows>,
        instances: &mut Vec<(Rows, Row)>,
    ) {
        let literal = |operand: &Operand| matches!(operand, Operand::Literal(_));
        let register = |operand: &Operand| matches!(operand, Operand::Register(_));
        let general: HashMap<u32, &Candidate> = alone
            .iter()
            .filter(|candidate| candidate.rows[0].operands.iter().all(register))
            .map(|candidate| (candidate.rows[0].instance, candidate))
            .collect();
        let mut headed: HashMap<SymKind, Vec<usize>> = HashMap::new();
        for (instance, tile) in self
            .tiles
            .iter()
            .enumerate()
            .filter(|(_, tile)| tile.usable)
        {
            headed.entry(tile.head).or_default().push(instance);
        }
        for candidate in alone {
            let [row] = &candidate.rows[..] else {
                continue;
            };
            let own = &self.tiles[row.instance as usize];
            if !own.usable || row.operands.iter().any(literal) {
                continue;
            }
            let Some(general) = general.get(&row.instance) else {
                continue;
            };
            let leaves = general.leaves.iter().zip(&own.reads).zip(&row.operands);
            let leaves: SmallVec<[(Ref, TypeId, Operand); 4]> = leaves
                .map(|((&leaf, &ty), &operand)| (graph.find(leaf), ty, operand))
                .collect();
            let pending = vec![(general.root, (0, 0), own.result)];
            let before = out.len();
            self.rooted((graph, &headed), &leaves, Rows::new(), pending, out);
            if !row.operands.iter().all(register) {
                let tiled = out[before..].iter().filter(|rows| rows.len() > 1);
                instances.extend(tiled.map(|rows| (rows.clone(), row.clone())));
            }
        }
    }

    // [`Self::roots`] for the rows tiled so far and the classes still to be
    // given a row, each with the place that reads it and the type read;
    // `headed` has the usable tiles of each operator.
    fn rooted(
        &self,
        (graph, headed): (&SemEGraph, &HashMap<SymKind, Vec<usize>>),
        leaves: &[(Ref, TypeId, Operand)],
        rows: Rows,
        mut pending: Vec<(Ref, Place, Option<TypeId>)>,
        out: &mut Vec<Rows>,
    ) {
        let Some((class, place, ty)) = pending.pop() else {
            out.push(rows);
            return;
        };
        let slots: usize = rows.iter().map(|row| row.operands.len()).sum();
        if rows.len() > LEVELS {
            return;
        }
        let (class, at) = (graph.find(class), rows.len() as u8);
        for node in graph.rows(class.class) {
            let Some(head) = graph.node(node).kind.sym() else {
                continue;
            };
            let Some(tiles) = headed.get(&head).filter(|_| graph.value(node) == class) else {
                continue;
            };
            let read = graph.children(node);
            let fits = |tile: &Tile| {
                tile.ports.len() == read.len()
                    && ty.is_none_or(|ty| tile.result == Some(ty))
                    && slots + tile.reads.len() <= OPERANDS
            };
            for &instance in tiles {
                let tile = &self.tiles[instance];
                if !fits(tile) {
                    continue;
                }
                let mut below = pending.clone();
                let (mut constant, mut built) = (None, 0);
                let mut operands: SmallVec<[Option<Operand>; 4]> =
                    smallvec![None; tile.reads.len()];
                let mut spelled = true;
                for (port, &child) in tile.ports.iter().zip(read.iter()) {
                    let operand = match *port {
                        Port::Fixed(value) => {
                            spelled &= graph.int_const(child) == Some(value);
                            continue;
                        }
                        Port::Operand(operand) => operand,
                    };
                    let (child, read) = (graph.find(child), tile.reads[operand]);
                    let leaf = leaves.iter().find(|leaf| (leaf.0, leaf.1) == (child, read));
                    let held = tile.constants & (1 << operand) != 0 && constant.is_none();
                    let width = tile.widths[operand].filter(|_| held);
                    operands[operand] = Some(match (leaf, graph.int_const(child), width) {
                        (Some(&(.., leaf @ Operand::Immediate(_))), ..) if held => {
                            constant = Some(operand);
                            leaf
                        }
                        (Some(&(.., leaf @ Operand::Register(_))), ..) => leaf,
                        (Some(_), ..) => {
                            spelled = false;
                            continue;
                        }
                        (None, Some(bits), Some(width)) => {
                            constant = Some(operand);
                            built = 1;
                            Operand::Literal(bits & ones(width))
                        }
                        (None, Some(_), None) => {
                            spelled = false;
                            continue;
                        }
                        (None, None, _) => {
                            below.push((child, (at, operand as u8), Some(read)));
                            OPEN
                        }
                    });
                }
                let Some(operands) = operands.into_iter().collect::<Option<_>>() else {
                    continue;
                };
                // A literal no field holds is built into a register: the
                // row over registers covers it.
                let variants = built | 1 << constant.map_or(0, |held| held + 1);
                if !spelled || self.covered[instance] & variants == 0 {
                    continue;
                }
                let mut longer = rows.clone();
                if at != 0 {
                    longer[usize::from(place.0)].operands[usize::from(place.1)] = Operand::Row(at);
                }
                longer.push(Row {
                    instance: instance as u32,
                    operands,
                });
                self.rooted((graph, headed), leaves, longer, below, out);
            }
        }
    }

    // A row of a tiling that holds a literal may be any row the pool proves
    // computes the same from the same operand: a multiply by four where the
    // pattern shifts by two.
    fn equal(&self, graph: &SemEGraph, literals: &[(Row, Ref)], sides: &mut Vec<Rows>) {
        let literal = |row: &Row| {
            let held = |operand: &Operand| matches!(operand, Operand::Literal(_));
            row.operands.iter().position(held)
        };
        let mut classes: HashMap<Ref, SmallVec<[usize; 4]>> = HashMap::new();
        let mut known: HashMap<(u32, usize, Operand), Ref> = HashMap::new();
        for (at, (row, root)) in literals.iter().enumerate() {
            let Some(held) = literal(row) else {
                continue;
            };
            let class = graph.find(*root);
            classes.entry(class).or_default().push(at);
            known.insert((row.instance, held, row.operands[held]), class);
        }
        let mut distinct: std::collections::HashSet<Rows> = sides.iter().cloned().collect();
        let mut next = 0;
        while let Some(rows) = sides.get(next).cloned() {
            next += 1;
            for (index, row) in rows.iter().enumerate().skip(1) {
                let Some(held) = literal(row) else {
                    continue;
                };
                let Some(class) = known.get(&(row.instance, held, row.operands[held])) else {
                    continue;
                };
                let own = &self.tiles[row.instance as usize];
                for &other in &classes[class] {
                    let (other, _) = &literals[other];
                    let tile = &self.tiles[other.instance as usize];
                    if !tile.usable
                        || literal(other) != Some(held)
                        || tile.reads != own.reads
                        || tile.result != own.result
                    {
                        continue;
                    }
                    let mut spelled = rows.clone();
                    spelled[index].instance = other.instance;
                    spelled[index].operands[held] = other.operands[held];
                    if distinct.insert(spelled.clone()) {
                        sides.push(spelled);
                    }
                }
            }
        }
    }

    // The rows spelling an axiom's left side are a rule where the pool
    // covers what the axiom makes of them: a conditional move for the mask
    // form of a select.
    fn axioms(&self, theory: &Theory, out: &mut Vec<Rows>) -> Vec<Spelled> {
        let mut spelled = Vec::new();
        for axiom in theory.axioms() {
            let (left, right) = axiom.sides();
            let mut tiled = Vec::new();
            let whole = Partial::new(&left, None);
            tile(self.tiles, self.covered, whole, &mut tiled);
            // A right side that is one operator over variables makes the
            // rows the tile of that operator in a pattern.
            let variable = |shape: &Shape| match shape {
                Shape::Variable(variable) => Some(*variable),
                _ => None,
            };
            let over = match &right {
                Shape::Operator(head, read) => {
                    let read = read.iter().map(variable);
                    Some(*head).zip(read.collect::<Option<SmallVec<[usize; 4]>>>())
                }
                _ => None,
            };
            for (rows, variables) in tiled {
                let hole = |read: &usize| {
                    let mut known = variables.iter();
                    known
                        .find(|(known, _)| known == read)
                        .map(|&(_, held)| held)
                };
                let holes = over.as_ref().filter(|_| rows.len() > 1);
                let holes = holes.and_then(|(head, read)| {
                    let holes = read.iter().map(hole);
                    Some((*head, holes.collect::<Option<SmallVec<[Operand; 4]>>>()?))
                });
                if let Some((head, holes)) = holes {
                    spelled.push(Spelled {
                        head,
                        holes,
                        result: self.tiles[rows[0].instance as usize].result,
                        rows: rows.clone(),
                    });
                }
                out.push(rows);
            }
        }
        spelled
    }
}

// What covering a left side with rules kept so far costs.
struct Kept<'a> {
    rules: &'a [Synthesized],
    rooted: &'a Rooted,
    compiled: &'a Compiled,
    tiles: &'a [Tile],
    // What each row of the left side asked about costs, once known.
    costs: std::cell::RefCell<SmallVec<[Option<Option<u64>>; 8]>>,
}

impl Kept<'_> {
    // The least cost of the rules that together compute row `at` of
    // `candidate`, each row under them computed the same way.
    fn cost(&self, candidate: &Synthesized, at: usize) -> Option<u64> {
        if let Some(known) = self.costs.borrow()[at] {
            return known;
        }
        let cost = self.least(candidate, at, &|rule| Some(rule.cost));
        self.costs.borrow_mut()[at] = Some(cost);
        cost
    }

    // The least of what `priced` says the rules at row `at` cost, each with
    // the rows under it computed.
    fn least(
        &self,
        candidate: &Synthesized,
        at: usize,
        priced: &dyn Fn(&Synthesized) -> Option<u32>,
    ) -> Option<u64> {
        let rooted = self.rooted.get(&candidate.rows[at].instance);
        let costs = rooted.into_iter().flatten().filter_map(|&rule| {
            let rule = &self.rules[rule as usize];
            let cost = priced(rule)?;
            let mut bound = Bound::default();
            let below = self.embeds(rule, 0, candidate, at, &mut bound)?;
            Some(u64::from(cost) + below)
        });
        costs.min()
    }

    // Whether row `row` of `rule` and the rows under it are row `at` of
    // `candidate` and the rows under that, with what the rule leaves to
    // others: the rows its register operands read, and the literals built
    // for them.
    fn embeds(
        &self,
        rule: &Synthesized,
        row: usize,
        candidate: &Synthesized,
        at: usize,
        bound: &mut Bound,
    ) -> Option<u64> {
        let (own, other) = (&rule.rows[row], &candidate.rows[at]);
        if own.instance != other.instance {
            return None;
        }
        let field = |rule: &Synthesized, operand: u8| {
            let mut fields = rule.fields.iter();
            let field = fields.find(|(known, _)| *known == operand);
            field.map_or(WHOLE, |&(_, field)| field)
        };
        let tile = &self.tiles[own.instance as usize];
        let mut left = 0;
        for (index, (&own, &other)) in own.operands.iter().zip(&other.operands).enumerate() {
            let width = tile.widths[index].unwrap_or(64);
            left += match (own, other) {
                (Operand::Register(operand), _) => {
                    match bound.operands.iter().find(|(known, _)| *known == operand) {
                        Some(&(_, before)) if before == other => 0,
                        Some(_) => return None,
                        None => {
                            bound.operands.push((operand, other));
                            match other {
                                Operand::Register(_) => 0,
                                Operand::Row(below) => self.cost(candidate, usize::from(below))?,
                                Operand::Literal(value) => Some(self.compiled.built(value, width))
                                    .filter(|&built| built < UNCOVERED)?,
                                Operand::Immediate(_) => return None,
                            }
                        }
                    }
                }
                (Operand::Immediate(operand), Operand::Immediate(held)) => {
                    let (wide, narrow) = (field(rule, operand), field(candidate, held));
                    wide.holds(&narrow).then_some(0)?
                }
                (Operand::Immediate(operand), Operand::Literal(value)) => {
                    let value = APInt::new_signed(
                        u64::from(width),
                        tir::binding::signed_bits(value, width),
                    );
                    field(rule, operand).contains(&value).then_some(0)?
                }
                (Operand::Literal(own), Operand::Literal(other)) => (own == other).then_some(0)?,
                (Operand::Row(own), Operand::Row(other)) => {
                    match bound.rows.iter().find(|(known, _)| *known == own) {
                        Some(&(_, before)) if before == other => 0,
                        Some(_) => return None,
                        None => {
                            bound.rows.push((own, other));
                            let (own, other) = (usize::from(own), usize::from(other));
                            self.embeds(rule, own, candidate, other, bound)?
                        }
                    }
                }
                _ => return None,
            };
        }
        Some(left)
    }
}

// What the operands and the rows of a rule are in the left side it is
// matched to.
#[derive(Default)]
struct Bound {
    operands: SmallVec<[(u8, Operand); 8]>,
    rows: SmallVec<[(u8, u8); 4]>,
}

// The rules kept of `found` (Daly et al., sections IV and V). Rules are
// tried by row count and then by cost ascending, and one is kept unless the
// rules kept before it compute its left side at no more than its cost: a
// duplicate (the same left side after canonical order, found before at no
// more), a composite of smaller rules no dearer together, a literal an
// immediate field holds as cheaply. A rule whose field holds constants a
// cheaper one's does not is none of these, and stays; so does one whose
// branch is cheaper than the branches of the rules that compute it.
fn pruned(mut found: Vec<Synthesized>, compiled: &Compiled, tiles: &[Tile]) -> Vec<Synthesized> {
    let commutes = |instance: u32| tiles[instance as usize].commutative;
    // A left side of several rows was lowered in its canonical order.
    for rule in found.iter_mut().filter(|rule| rule.rows.len() == 1) {
        let (rows, numbers) = canonical(&rule.rows, &commutes);
        let renumber = |arg: &mut Arg| {
            if let Arg::Operand(operand) = arg {
                *operand = numbers[usize::from(*operand)];
            }
        };
        let trees = rule
            .branch
            .iter_mut()
            .flat_map(|branch| std::iter::once(&mut branch.taken).chain(&mut branch.inverse));
        for step in std::iter::once(&mut rule.tree).chain(trees).flatten() {
            step.args.iter_mut().for_each(renumber);
        }
        renumber(&mut rule.value);
        for (operand, _) in &mut rule.fields {
            *operand = numbers[usize::from(*operand)];
        }
        rule.rows = rows;
    }
    found.sort_by_key(|rule| (rule.rows.len(), rule.cost));
    let mut kept: Vec<Synthesized> = Vec::new();
    let mut rooted = Rooted::new();
    for rule in found {
        let before = Kept {
            rules: &kept,
            rooted: &rooted,
            compiled,
            tiles,
            costs: std::cell::RefCell::new(smallvec![None; rule.rows.len()]),
        };
        let computed = before.cost(&rule, 0);
        let branched = |branch: &Branch| {
            let branches = |rule: &Synthesized| rule.branch.as_ref().map(|branch| branch.cost);
            let cost = before.least(&rule, 0, &branches);
            cost.is_some_and(|cost| cost <= u64::from(branch.cost))
        };
        if computed.is_some_and(|cost| cost <= u64::from(rule.cost))
            && rule.branch.as_ref().is_none_or(branched)
        {
            continue;
        }
        let root = rule.rows[0].instance;
        rooted.entry(root).or_default().push(kept.len() as u32);
        kept.push(rule);
    }
    kept
}

// The builders at the widths each builds. One that adds to a register or
// shifts one is the instruction of a rule over a register and a constant:
// it builds at the widths `rules` keep that instruction alone for such a
// row, and at no other.
fn keyed(
    builders: Vec<Builder>,
    forms: &[Form],
    rules: &[Synthesized],
    tiles: &[Tile],
) -> Vec<Builder> {
    let kept = |builder: &Builder| {
        let instruction = forms[builder.form as usize].rule;
        let mut widths = SmallVec::<[u32; 8]>::new();
        for rule in rules {
            let ([row], [step]) = (&rule.rows[..], &rule.tree[..]) else {
                continue;
            };
            let register = match row.operands[..] {
                [Operand::Register(_), Operand::Immediate(_)] => 0,
                [Operand::Immediate(_), Operand::Register(_)] => 1,
                _ => continue,
            };
            let width = tiles[row.instance as usize].widths[register];
            let width = width.filter(|&width| builder.width.is_none_or(|own| own == width));
            if let Some(width) = width.filter(|width| !widths.contains(width))
                && forms[step.form as usize].rule == instruction
            {
                widths.push(width);
            }
        }
        widths
    };
    let mut keyed = Vec::new();
    for builder in builders {
        if builder.register.is_none() || builder.masks {
            keyed.push(builder);
            continue;
        }
        let at = |width| Builder {
            width: Some(width),
            ..builder
        };
        keyed.extend(kept(&builder).into_iter().map(at));
    }
    keyed
}

impl Pool {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn build(
        context: &Context,
        scratch: &Context,
        instances: &[Instance],
        rules: &[Rule],
        theory: &Theory,
        pointer: u32,
        admits: &dyn Fn(usize, &[Form], &Step) -> bool,
        report: &mut Report,
    ) -> Self {
        let started = std::time::Instant::now();
        let compiled = Compiled::new(context, rules, report);
        report.compile_us = started.elapsed().as_micros();

        // The forward pass: every instance alone, covered by the cheapest
        // machine tree of its class.
        let started = std::time::Instant::now();
        let tiles = tiles(context, scratch, instances, pointer);
        let mut graph = SemEGraph::over(tir_egraph::Vocabulary::new(context.names()));
        let spelled = spelled(rules, theory);
        if std::env::var_os("TIR_ISEL_SYNTH").is_some() {
            eprintln!("isel-pool: literals {spelled:?}");
        }
        let sides = singles(&tiles, &spelled);
        let lower = |graph: &mut SemEGraph, sides: Vec<Rows>| {
            lowered(context, scratch, instances, pointer, graph, sides)
        };
        let alone = lower(&mut graph, sides);
        graph.rebuild();
        report.seeded = graph.total_size();
        report.lower_us = started.elapsed().as_micros();
        let extraction = settle(context, &mut graph, theory, &compiled, pointer, report);
        let started = std::time::Instant::now();
        let mut found = Vec::new();
        let mut covered = vec![0u8; instances.len()];
        let mut literals = Vec::new();
        let admitted = Admitted::default();
        let covering = Covering {
            graph: &graph,
            extraction: &extraction,
            compiled: &compiled,
            rules,
            tiles: &tiles,
            admits,
            admitted: &admitted,
        };
        for candidate in &alone {
            let before = found.len();
            covering.rules(candidate, &mut found);
            let row = &candidate.rows[0];
            let literal = |operand: &Operand| matches!(operand, Operand::Literal(_));
            if row.operands.iter().any(literal) {
                literals.push((row.clone(), candidate.root));
                continue;
            }
            let emitted = found[before..].iter().any(|rule| !rule.tree.is_empty());
            match constant(&row.operands) {
                Some(held) if emitted => covered[row.instance as usize] |= 1 << held,
                Some(0) if found.len() == before => report.uncovered.push(row.instance),
                _ => {}
            }
        }
        report.covered = covered.iter().filter(|held| **held & 1 != 0).count();
        report.extract_us += started.elapsed().as_micros();

        // The backward pass: each left side of several rows lowered as one
        // term, and covered by the cheapest machine tree of its class.
        let started = std::time::Instant::now();
        let backward = Backward {
            terms: context.terms(),
            tiles: &tiles,
            covered: &covered,
        };
        let mut seeds = Vec::new();
        let spelled = backward.axioms(theory, &mut seeds);
        let mut sides = Vec::new();
        backward.patterns(rules, &spelled, &mut sides);
        let mut instantiated = Vec::new();
        backward.roots(&graph, &alone, &mut sides, &mut instantiated);
        sides.retain(|rows| rows.len() > 1);
        backward.equal(&graph, &literals, &mut sides);
        sides.extend(seeds);
        let commutes = |instance: u32| tiles[instance as usize].commutative;
        let mut spelled = std::collections::HashSet::new();
        let mut distinct = std::collections::HashSet::new();
        let sides = sides.into_iter().filter_map(|rows| {
            if rows.len() < 2 || !spelled.insert(rows.clone()) {
                return None;
            }
            let (rows, _) = canonical(&rows, &commutes);
            distinct.insert(rows.clone()).then_some(rows)
        });
        let folded = lower(&mut graph, sides.collect());
        for (rows, row) in instantiated {
            let (rows, numbers) = canonical(&rows, &commutes);
            let number = |operand: u8| {
                let number = numbers.get(usize::from(operand)).copied();
                number.filter(|&number| number != u8::MAX)
            };
            let operands = row.operands.iter().map(|operand| match *operand {
                Operand::Register(operand) => number(operand).map(Operand::Register),
                Operand::Immediate(operand) => number(operand).map(Operand::Immediate),
                other => Some(other),
            });
            // A row with an operand the left side does not read has no
            // spelling over the left side's operands.
            let Some(operands) = operands.collect::<Option<SmallVec<[Operand; 4]>>>() else {
                continue;
            };
            let instance = row.instance;
            let sides = vec![rows, smallvec![Row { instance, operands }]];
            if let [tiled, row] = &lower(&mut graph, sides)[..] {
                let _ = graph.union(tiled.root, row.root);
            }
        }
        report.tiled = folded.len();
        report.lower_us += started.elapsed().as_micros();
        if !folded.is_empty() {
            let extraction = settle(context, &mut graph, theory, &compiled, pointer, report);
            let started = std::time::Instant::now();
            let covering = Covering {
                graph: &graph,
                extraction: &extraction,
                compiled: &compiled,
                rules,
                tiles: &tiles,
                admits,
                admitted: &admitted,
            };
            for candidate in &folded {
                covering.rules(candidate, &mut found);
            }
            report.extract_us += started.elapsed().as_micros();
        }
        report.found = found.len();
        let started = std::time::Instant::now();
        let rules = pruned(found, &compiled, &tiles);
        report.prune_us = started.elapsed().as_micros();
        Self {
            materializers: keyed(compiled.materializers, &compiled.forms, &rules, &tiles),
            rules,
            floats: compiled.floats,
            forms: compiled.forms,
        }
    }
}
