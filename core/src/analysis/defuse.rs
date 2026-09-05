//! Def-use and use-def chains over machine and mid-end IR.
//!
//! A register operand is an SSA operand or result — [`op_regs`] reads them
//! directly — plus the physical registers an instruction names without an SSA
//! value: the literals its register slots hold, and the caller-saved set a call
//! destroys.
//!
//! [`DefUse`] indexes the defs of every op nested under a root, so use-def
//! chains (`defs_of`) answer "who wrote the value this op reads"; the readers
//! of a value come from the context's use lists ([`Context::users_of`]).
//! Liveness and register allocation share [`op_regs`] for their own ordered
//! scans.

use std::collections::HashMap;

use crate::attributes::{AttributeRole, AttributeValue, RegisterAttr};
use crate::backend::regalloc::RegClassId;
use crate::{
    Context, OpHandle, OpId, ValueId,
    analysis::{Analysis, AnalysisManager},
};

/// A physical register: its class handle and encoding index.
pub type PhysReg = (RegClassId, u16);

/// The attribute a call carries the registers it destroys in: an array of
/// physical registers, which are not values and so are not in its operands.
pub const CLOBBERS_ATTR: &str = "clobbers";

/// The attribute an operation carries the physical registers it reads without
/// naming them in an operand: a call's argument registers and the stack pointer
/// it pushes the return address on. A placement is not a value, so nothing else
/// would keep the copy that made it alive to the operation that reads it.
pub const USES_ATTR: &str = "uses";

/// The registers of a single operation, split by direction. Values are SSA
/// operands and results; physical registers are the ones the instruction names
/// directly and are not SSA.
#[derive(Clone, Debug, Default)]
pub struct OpRegs {
    pub defs: Vec<ValueId>,
    pub uses: Vec<ValueId>,
    pub phys_defs: Vec<PhysReg>,
    pub phys_uses: Vec<PhysReg>,
}

/// The registers one op reads and writes: its value results and operands, the
/// physical registers its register slots name directly, and the registers a
/// call destroys. Dependencies live in no register and are not among them.
pub fn op_regs(op: &OpHandle) -> OpRegs {
    op_regs_from(op, &crate::backend::reg_slots(op))
}

/// [`op_regs`] over slots the caller has already resolved, for a scan that
/// wants both them and the registers — liveness reads every instruction once.
pub fn op_regs_from(op: &OpHandle, slots: &[crate::backend::SlotRef]) -> OpRegs {
    let mut regs = OpRegs {
        defs: op.value_results().to_vec(),
        uses: op.value_operands().to_vec(),
        phys_defs: Vec::new(),
        phys_uses: Vec::new(),
    };
    for slot in slots {
        if let crate::backend::RegSlot::Phys(register) = slot.slot {
            if slot.port.def {
                regs.phys_defs.push(register);
            } else {
                regs.phys_uses.push(register);
            }
        }
    }
    // A two-address destination whose tied source slot is absent — an
    // assembled instruction, whose tie no lowering has turned into a copy —
    // is read as well as written.
    for port in crate::backend::reg_ports(op) {
        let Some(destination) = port.tied_to else {
            continue;
        };
        if slots.iter().any(|slot| slot.port.name == port.name) {
            continue;
        }
        match slots
            .iter()
            .find(|slot| slot.port.name == destination)
            .map(|slot| slot.slot)
        {
            Some(crate::backend::RegSlot::Phys(register)) => regs.phys_uses.push(register),
            Some(crate::backend::RegSlot::Value(value)) => regs.uses.push(value),
            None => {}
        }
    }
    let context = op.context.upgrade();
    let physical = |attr: &str, into: &mut Vec<PhysReg>| {
        context.with_attr(op.id, attr, |registers| {
            let AttributeValue::Array(registers) = registers else {
                return;
            };
            for register in registers.iter() {
                if let AttributeValue::Register(RegisterAttr::Physical { class, index }) = register
                {
                    into.push((*class, *index));
                }
            }
        });
    };
    physical(CLOBBERS_ATTR, &mut regs.phys_defs);
    physical(USES_ATTR, &mut regs.phys_uses);
    regs
}

/// The architectural registers an operation reads and writes when it executes:
/// its operands, plus the fixed registers its behavior names by path (x86
/// `EFLAGS::zf`, `GPR::rax`), plus the read a write through a merging
/// sub-register view implies. This is the view a timing model reconstructs
/// dependencies from.
pub fn execution_regs(op: &OpHandle) -> OpRegs {
    let mut regs = op_regs(op);

    let implicit_regs = op
        .clone()
        .as_interface::<dyn crate::backend::MachineInstruction>()
        .map(|mi| mi.info().implicit_regs)
        .unwrap_or_default();
    for implicit in implicit_regs {
        let register = (implicit.class, implicit.index);
        if matches!(
            implicit.role,
            AttributeRole::Def | AttributeRole::Clobber | AttributeRole::ReadWrite
        ) {
            regs.phys_defs.push(register);
        }
        if matches!(implicit.role, AttributeRole::Use | AttributeRole::ReadWrite) {
            regs.phys_uses.push(register);
        }
    }

    // A write through a merging sub-register view (an x86 8/16-bit destination)
    // preserves the rest of the physical register, so it reads it too. A
    // zero-extending view (x86 32-bit) writes the whole register and does not.
    let merging: Vec<PhysReg> = regs
        .phys_defs
        .iter()
        .filter(|(class, _)| class.view.merge)
        .copied()
        .collect();
    regs.phys_uses.extend(merging);

    regs
}

/// Use-def chains and a walk order for every op nested under a root operation.
///
/// Def-use chains live on the [`Context`] itself, which keeps a use list per
/// value; what a walk still has to answer is which ops write a value — machine
/// IR after block-parameter destruction has several producers per value — and
/// in which order the ops appear.
#[derive(Default)]
pub struct DefUse {
    /// Value → ops that write it (use-def direction). Empty for values defined
    /// by block arguments.
    defs: HashMap<u32, Vec<OpId>>,
    /// Every op visited, in walk order.
    ops: Vec<OpId>,
}

impl DefUse {
    /// Index the ops nested under `root` directly, for a caller with no
    /// [`AnalysisManager`] to cache the result in.
    pub fn new<O: Into<OpId>>(context: &Context, root: O) -> Self {
        let mut result = Self::default();
        let mut stack = context.get_op(root.into()).regions().to_vec();
        while let Some(region) = stack.pop() {
            for block in context.get_region(region).iter(context.clone()) {
                for op_id in block.op_ids() {
                    let instance = context.get_op(op_id);
                    stack.extend(instance.regions().iter().copied());
                    result.ops.push(op_id);
                    for def in instance.results() {
                        result.defs.entry(def.number()).or_default().push(op_id);
                    }
                }
            }
        }
        result
    }

    /// Every op nested under the root, in walk order.
    pub fn ops(&self) -> &[OpId] {
        &self.ops
    }

    /// The ops writing `value`.
    pub fn defs_of(&self, value: u32) -> &[OpId] {
        self.defs.get(&value).map(Vec::as_slice).unwrap_or(&[])
    }
}

impl Analysis for DefUse {
    fn build(_: &AnalysisManager, context: &Context, op: OpId) -> Self {
        Self::new(context, op)
    }
}
