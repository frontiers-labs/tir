//! Alias facts: which object every pointer names, and at what offset into it.
//!
//! A pointer reads back to the object it was derived from — a stack
//! allocation, a global, a parameter — plus the constant byte offset the
//! arithmetic on the way added, propagated down the `ptradd` chains by the
//! generic [`solver`](super::solver). Two accesses then alias by their objects:
//! distinct objects never alias, one object at disjoint offsets never aliases,
//! and a pointer the analysis cannot read back may alias anything — except a
//! stack allocation whose address never leaves the function
//! ([`EscapeFacts`]), which no such pointer can reach.
//!
//! Objects are whole: the analysis is field-insensitive, flow-insensitive, and
//! says nothing about what a call does to memory.

use std::rc::Rc;

use crate::analysis::escape_facts::is_pointer;
use crate::analysis::solver::{FactDomain, Facts, Lattice, solve};
use crate::analysis::{Analysis, AnalysisManager, DefUse, Escape, EscapeFacts};
use crate::builtin::GlobalOp;
use crate::func::FuncOp;
use crate::ptr::PtrAddOp;
use crate::{Context, OpId, PromotableAllocation, ValueId};

/// The object a pointer was derived from.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Base {
    /// A stack allocation, named by the pointer it opens.
    Alloca(ValueId),
    /// A global, named by the module-level value declaring it.
    Global(ValueId),
    /// A pointer the function was entered with. `noalias` where the caller
    /// guarantees nothing else the function reaches names that memory —
    /// `restrict` in C.
    Param { pointer: ValueId, noalias: bool },
}

impl Base {
    /// Whether two objects are known to be different memory. Two parameters, or
    /// a parameter and a global, may name the same memory; a stack allocation
    /// is fresh and a `noalias` parameter is guaranteed unaliased, so nothing
    /// else is either.
    pub fn distinct(self, other: Base) -> bool {
        self != other
            && (self.unshared()
                || other.unshared()
                || matches!((self, other), (Base::Global(_), Base::Global(_))))
    }

    /// Whether the object is nothing else the function names.
    fn unshared(self) -> bool {
        matches!(self, Base::Alloca(_) | Base::Param { noalias: true, .. })
    }
}

/// What the solver knows about one pointer.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PointerFact {
    /// Nothing has been proven yet.
    Unknown,
    /// Points into `base`, at `offset` bytes from its start where that is one
    /// constant on every path.
    Object { base: Base, offset: Option<i64> },
    /// Derived from more than one object, or from one the analysis cannot name.
    Overdefined,
}

impl Lattice for PointerFact {
    fn bottom() -> Self {
        PointerFact::Unknown
    }

    fn join(&self, other: &Self) -> Self {
        match (self, other) {
            (PointerFact::Unknown, fact) | (fact, PointerFact::Unknown) => fact.clone(),
            (
                PointerFact::Object { base, offset },
                PointerFact::Object {
                    base: other_base,
                    offset: other_offset,
                },
            ) if base == other_base => PointerFact::Object {
                base: *base,
                offset: if offset == other_offset {
                    *offset
                } else {
                    None
                },
            },
            _ => PointerFact::Overdefined,
        }
    }
}

/// Whether two accesses can be the same memory.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AliasResult {
    NoAlias,
    MayAlias,
    /// Both start at the same address.
    MustAlias,
}

/// Per-pointer object facts for the IR under a function, and the alias
/// verdicts they give. Build through [`AnalysisManager`].
pub struct AliasFacts {
    facts: Facts<ValueId, PointerFact>,
    escapes: Rc<EscapeFacts>,
}

impl AliasFacts {
    /// What is known about `pointer`, for a caller reading the lattice directly.
    pub fn fact(&self, pointer: ValueId) -> PointerFact {
        self.facts.get(pointer)
    }

    /// Whether `a_size` bytes at `a` and `b_size` bytes at `b` can be the same
    /// memory. A size nothing bounds reaches to the end of its object.
    pub fn alias(
        &self,
        a: ValueId,
        a_size: Option<u64>,
        b: ValueId,
        b_size: Option<u64>,
    ) -> AliasResult {
        match (self.fact(a), self.fact(b)) {
            (
                PointerFact::Object { base, offset },
                PointerFact::Object {
                    base: other_base,
                    offset: other_offset,
                },
            ) => {
                if base != other_base {
                    return if base.distinct(other_base) {
                        AliasResult::NoAlias
                    } else {
                        AliasResult::MayAlias
                    };
                }
                match (offset, other_offset) {
                    (Some(offset), Some(other)) if offset == other => AliasResult::MustAlias,
                    (Some(offset), Some(other)) if disjoint(offset, a_size, other, b_size) => {
                        AliasResult::NoAlias
                    }
                    _ => AliasResult::MayAlias,
                }
            }
            (PointerFact::Object { base, .. }, _) | (_, PointerFact::Object { base, .. })
                if self.is_private(base) =>
            {
                AliasResult::NoAlias
            }
            _ => AliasResult::MayAlias,
        }
    }

    /// A stack allocation nothing but the pointers derived from it can reach:
    /// no pointer the analysis cannot read back, and nothing outside the
    /// function — a call, the caller after a return.
    pub fn is_private(&self, base: Base) -> bool {
        match base {
            Base::Alloca(slot) => self.escapes.escape(slot) == Escape::Local,
            _ => false,
        }
    }
}

/// Whether `[a, a + a_size)` and `[b, b + b_size)` share no byte.
fn disjoint(a: i64, a_size: Option<u64>, b: i64, b_size: Option<u64>) -> bool {
    let ends_before = |start: i64, size: Option<u64>, other: i64| {
        size.is_some_and(|size| {
            i64::try_from(size).is_ok_and(|size| start.saturating_add(size) <= other)
        })
    };
    ends_before(a, a_size, b) || ends_before(b, b_size, a)
}

impl Analysis for AliasFacts {
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
        let derivation = Derivation {
            context,
            root: op,
            defuse: analyses.get::<DefUse>(context, op),
        };
        Self {
            facts: solve(&derivation),
            escapes: analyses.get::<EscapeFacts>(context, op),
        }
    }
}

struct Derivation<'a> {
    context: &'a Context,
    root: OpId,
    defuse: Rc<DefUse>,
}

impl FactDomain for Derivation<'_> {
    type Node = ValueId;
    type Fact = PointerFact;

    /// Every pointer is seeded with the object it opens, or with
    /// [`PointerFact::Overdefined`] where it opens none the analysis can name:
    /// a loaded pointer, a call's result, a block or region argument. Only a
    /// `ptradd` waits, for the transfer from its base.
    fn seed(&self, facts: &mut Facts<ValueId, PointerFact>) {
        let entry = self
            .context
            .get_op(self.root)
            .regions()
            .first()
            .map(|&body| self.context.get_region(body).block_ids()[0]);
        let noalias = self.noalias_parameters();
        for region in crate::passes::regions_under(self.context, self.root) {
            for block in self.context.get_region(region).iter(self.context.clone()) {
                let parameters = Some(block.id()) == entry;
                for argument in block.arguments() {
                    let pointer = argument.id();
                    if !is_pointer(self.context, pointer) {
                        continue;
                    }
                    let fact = if parameters {
                        object(Base::Param {
                            pointer,
                            noalias: noalias.contains(&pointer),
                        })
                    } else {
                        PointerFact::Overdefined
                    };
                    facts.raise(pointer, fact);
                }
            }
        }
        for &op in self.defuse.ops() {
            let instance = self.context.get_op(op);
            if let Some(allocation) = instance.clone().as_interface::<dyn PromotableAllocation>() {
                let slot = allocation.promoted_location();
                facts.raise(slot, object(Base::Alloca(slot)));
            } else if !instance.is::<PtrAddOp>() {
                for result in instance.results().to_vec() {
                    if is_pointer(self.context, result) {
                        facts.raise(result, PointerFact::Overdefined);
                    }
                }
            }
            for operand in instance.operands().to_vec() {
                if self.is_global(operand) {
                    facts.raise(operand, object(Base::Global(operand)));
                }
            }
        }
    }

    /// A `ptradd` of `node` points into the same object, further along by the
    /// offset it adds where that offset is a constant.
    fn transfer(&self, node: ValueId, facts: &mut Facts<ValueId, PointerFact>) {
        let fact = facts.get(node);
        for &user in self.defuse.users_of(node.number()) {
            let instance = self.context.get_op(user);
            if !instance.is::<PtrAddOp>() || instance.operands()[0] != node {
                continue;
            }
            let derived = match &fact {
                PointerFact::Object { base, offset } => PointerFact::Object {
                    base: *base,
                    offset: offset.and_then(|offset| {
                        let added = constant_offset(self.context, instance.operands()[1])?;
                        Some(offset.wrapping_add(added))
                    }),
                },
                other => other.clone(),
            };
            facts.raise(instance.results()[0], derived);
        }
    }
}

/// The literal a value holds, where an operation states it outright. A `ptradd`
/// whose offset is spelled any other way points somewhere in the object the
/// analysis cannot name, so the fact it derives carries no offset.
fn constant_offset(context: &Context, value: ValueId) -> Option<i64> {
    let op = context.get_value(value).defining_op()?;
    Some(
        context
            .get_op(op)
            .as_interface::<dyn crate::ConstantLike>()?
            .constant_value()
            .to_i64(),
    )
}

impl Derivation<'_> {
    /// The entry arguments the λ declares free of aliases.
    fn noalias_parameters(&self) -> Vec<ValueId> {
        let Some(function) = self.context.get_op(self.root).as_op::<FuncOp>() else {
            return Vec::new();
        };
        let arguments = function.body().arguments();
        function
            .noalias_arguments()
            .into_iter()
            .filter_map(|index| arguments.get(index).map(|argument| argument.id()))
            .collect()
    }

    fn is_global(&self, value: ValueId) -> bool {
        self.context
            .get_value(value)
            .defining_op()
            .is_some_and(|op| self.context.get_op(op).is::<GlobalOp>())
    }
}

fn object(base: Base) -> PointerFact {
    PointerFact::Object {
        base,
        offset: Some(0),
    }
}
