//! Escape facts: whether the address of an object stays inside the function.
//!
//! A pointer is local while every use of it is one the analysis can see
//! through — pointer arithmetic, or naming the location of a load or store.
//! Storing the pointer itself into memory captures it: anything loading that
//! memory back holds the address without the analysis being able to say so. A
//! call, a return, a branch carrying it into another block or region, or any
//! other use lets it escape outright. The verdict flows back down the `ptradd`
//! chain to the object the pointer was derived from, so an allocation's fact is
//! the join over every pointer into it; the generic [`solver`](super::solver)
//! runs that propagation.

use std::rc::Rc;

use crate::analysis::solver::{FactDomain, Facts, Lattice, solve};
use crate::analysis::{Analysis, AnalysisManager, DefUse};
use crate::ptr::{CmpOp, PtrAddOp, PtrType};
use crate::{Context, MemoryRead, MemoryWrite, OpHandle, OpId, ValueId};

/// How far the address a pointer holds travels.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Escape {
    /// Every use is a load, a store, or arithmetic the analysis follows.
    Local,
    /// Stored into memory, so a load may hand it back unrecognized.
    Captured,
    /// Handed to a call, returned, or carried somewhere the analysis cannot follow.
    Escapes,
}

impl Lattice for Escape {
    fn bottom() -> Self {
        Escape::Local
    }

    fn join(&self, other: &Self) -> Self {
        (*self).max(*other)
    }
}

/// Per-pointer escape verdicts for the IR under a function. Build through
/// [`AnalysisManager`].
pub struct EscapeFacts {
    facts: Facts<ValueId, Escape>,
}

impl EscapeFacts {
    /// How far the address `pointer` holds travels, through every pointer
    /// derived from it.
    pub fn escape(&self, pointer: ValueId) -> Escape {
        self.facts.get(pointer)
    }
}

impl Analysis for EscapeFacts {
    fn build(analyses: &AnalysisManager, context: &Context, op: OpId) -> Self {
        let uses = Uses {
            context,
            defuse: analyses.get::<DefUse>(context, op),
        };
        Self {
            facts: solve(&uses),
        }
    }
}

struct Uses<'a> {
    context: &'a Context,
    defuse: Rc<DefUse>,
}

impl FactDomain for Uses<'_> {
    type Node = ValueId;
    type Fact = Escape;

    fn seed(&self, facts: &mut Facts<ValueId, Escape>) {
        for &op in self.defuse.ops() {
            let instance = self.context.get_op(op);
            for operand in instance.operands().to_vec() {
                if is_pointer(self.context, operand) {
                    facts.raise(operand, use_of(&instance, operand));
                }
            }
        }
    }

    fn transfer(&self, node: ValueId, facts: &mut Facts<ValueId, Escape>) {
        let Some(defining) = self.context.get_value(node).defining_op() else {
            return;
        };
        let instance = self.context.get_op(defining);
        if instance.is::<PtrAddOp>() {
            facts.raise(instance.operands()[0], facts.get(node));
        }
    }
}

/// What `instance`'s use of `operand` does with the address it holds.
fn use_of(instance: &OpHandle, operand: ValueId) -> Escape {
    if instance.is::<PtrAddOp>() || instance.is::<CmpOp>() {
        return Escape::Local;
    }
    if let Some(read) = instance.clone().as_interface::<dyn MemoryRead>() {
        return if read.read_location() == operand {
            Escape::Local
        } else {
            Escape::Escapes
        };
    }
    if let Some(write) = instance.clone().as_interface::<dyn MemoryWrite>() {
        return if write.written_value() == operand {
            Escape::Captured
        } else if write.write_location() == operand {
            Escape::Local
        } else {
            Escape::Escapes
        };
    }
    Escape::Escapes
}

pub(crate) fn is_pointer(context: &Context, value: ValueId) -> bool {
    let ty = context.get_type_data(context.get_value(value).ty());
    (ty.as_ref() as &dyn std::any::Any)
        .downcast_ref::<PtrType>()
        .is_some()
}
