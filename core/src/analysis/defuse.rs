//! Def-use and use-def chains over the unified virtual-register space.
//!
//! An op's register operands live in two notations: SSA `operands`/`results`,
//! and [`RegisterAttr`] attributes tagged with an [`AttributeRole`] on machine
//! ops. A virtual register's `id` equals the SSA value number, so both name the
//! same register; [`op_regs`] resolves either notation into one `u32` space.
//!
//! [`DefUse`] indexes those per-op defs and uses across every op nested under a
//! root: def-use chains (`users_of`) answer "who reads this register", use-def
//! chains (`defs_of`) answer "who wrote the register this op reads". Liveness
//! and register allocation share [`op_regs`] for their own ordered scans.

use std::collections::HashMap;

use crate::attributes::{AttributeRole, AttributeValue, RegisterAttr, RegisterSemantics};
use crate::backend::regalloc::RegClassId;
use crate::{
    Context, OpHandle, OpId,
    analysis::{Analysis, AnalysisManager},
};

/// A register operand resolved from an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegRef {
    Virtual { id: u32, class: Option<RegClassId> },
    Physical { class: RegClassId, index: u16 },
}

/// The register operands of a single operation, split by direction. A
/// read-modify-write operand appears in both `defs` and `uses`.
#[derive(Clone, Debug, Default)]
pub struct OpRegs {
    pub defs: Vec<RegRef>,
    pub uses: Vec<RegRef>,
}

fn role_writes(role: AttributeRole) -> bool {
    matches!(
        role,
        AttributeRole::Def | AttributeRole::Clobber | AttributeRole::ReadWrite
    )
}

fn role_reads(role: AttributeRole) -> bool {
    matches!(role, AttributeRole::Use | AttributeRole::ReadWrite)
}

/// Resolve the register operands of one op from its SSA operands/results and its
/// register-valued attributes (consulting the opcode's
/// [`RegisterSemantics`] interface).
pub fn op_regs(op: &OpHandle) -> OpRegs {
    let attribute_roles = op
        .clone()
        .as_interface::<dyn RegisterSemantics>()
        .map(|semantics| semantics.attribute_roles())
        .unwrap_or_default();
    // The roles name attributes; resolve them once against this context's
    // interner so the per-attribute lookup below is a `u32` compare. An op with
    // no register semantics (every mid-end op) resolves nothing.
    let roles: Vec<(Option<tir_adt::Sym>, AttributeRole)> = if attribute_roles.is_empty() {
        Vec::new()
    } else {
        let context = op.context.upgrade();
        attribute_roles
            .iter()
            .map(|(name, role)| (context.sym(name), *role))
            .collect()
    };
    let mut regs = OpRegs::default();

    // Builtin SSA ops (e.g. the block terminator) name registers positionally.
    for result in op.results() {
        regs.defs.push(RegRef::Virtual {
            id: result.number(),
            class: None,
        });
    }
    for operand in op.operands() {
        regs.uses.push(RegRef::Virtual {
            id: operand.number(),
            class: None,
        });
    }

    // Machine ops carry their register operands in attributes, with a def/use role.
    // An array of registers (e.g. a call's caller-saved clobber list) applies the
    // attribute's role to every element.
    for attr in op.attributes() {
        let attr_regs: Vec<&RegisterAttr> = match &attr.value {
            AttributeValue::Register(reg) => vec![reg],
            AttributeValue::Array(items) => items
                .iter()
                .filter_map(|item| match item {
                    AttributeValue::Register(reg) => Some(reg),
                    _ => None,
                })
                .collect(),
            _ => continue,
        };
        let role = roles
            .iter()
            .find(|(name, _)| *name == Some(attr.name))
            .map(|(_, role)| *role)
            .unwrap_or(AttributeRole::None);

        for reg in attr_regs {
            let reg_ref = match reg {
                RegisterAttr::Virtual { id, class } => RegRef::Virtual {
                    id: *id,
                    class: *class,
                },
                RegisterAttr::FixedUse { id, class, .. } => RegRef::Virtual {
                    id: *id,
                    class: Some(*class),
                },
                RegisterAttr::FixedDef { id, class, .. } => RegRef::Virtual {
                    id: *id,
                    class: Some(*class),
                },
                RegisterAttr::Physical { class, index } => RegRef::Physical {
                    class: *class,
                    index: *index,
                },
            };
            if role_writes(role) {
                regs.defs.push(reg_ref);
            }
            if role_reads(role) {
                regs.uses.push(reg_ref);
            }
        }
    }

    regs
}

/// The architectural registers an operation reads and writes when it executes:
/// its operands, plus the fixed registers its behavior names by path (x86
/// `EFLAGS::zf`, `GPR::rax`), plus the read a write through a merging
/// sub-register view implies. This is the view a timing model reconstructs
/// dependencies from.
///
/// Register allocation uses [`op_regs`] instead: it assigns operands, and the
/// fixed-register protocol reaches it through the fixed-register operands
/// selection emits — the same accesses in the notation the allocator can act
/// on.
pub fn execution_regs(op: &OpHandle) -> OpRegs {
    let mut regs = op_regs(op);

    let implicit_regs = op
        .clone()
        .as_interface::<dyn RegisterSemantics>()
        .map(|semantics| semantics.implicit_regs())
        .unwrap_or_default();
    for implicit in implicit_regs {
        let reg_ref = RegRef::Physical {
            class: implicit.class,
            index: implicit.index,
        };
        if role_writes(implicit.role) {
            regs.defs.push(reg_ref);
        }
        if role_reads(implicit.role) {
            regs.uses.push(reg_ref);
        }
    }

    // A write through a merging sub-register view (an x86 8/16-bit destination)
    // preserves the rest of the physical register, so it reads it too. A
    // zero-extending view (x86 32-bit) writes the whole register and does not.
    let merging: Vec<RegRef> = regs
        .defs
        .iter()
        .filter(|def| match def {
            RegRef::Physical { class, .. } => class.view.merge,
            RegRef::Virtual { .. } => false,
        })
        .copied()
        .collect();
    regs.uses.extend(merging);

    regs
}

/// Def-use and use-def chains for every op nested under a root operation.
/// Physical registers are excluded: they are not SSA-numbered and their
/// lifetimes are liveness's business.
#[derive(Default)]
pub struct DefUse {
    /// Register → ops that read it, in walk order (def-use direction).
    users: HashMap<u32, Vec<OpId>>,
    /// Register → ops that write it (use-def direction). Empty for values
    /// defined by block arguments.
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
                    let regs = op_regs(&instance);
                    for def in regs.defs {
                        if let RegRef::Virtual { id, .. } = def {
                            result.defs.entry(id).or_default().push(op_id);
                        }
                    }
                    for used in regs.uses {
                        if let RegRef::Virtual { id, .. } = used {
                            result.users.entry(id).or_default().push(op_id);
                        }
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

    /// The ops reading `reg`.
    pub fn users_of(&self, reg: u32) -> &[OpId] {
        self.users.get(&reg).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The ops writing `reg`.
    pub fn defs_of(&self, reg: u32) -> &[OpId] {
        self.defs.get(&reg).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn is_used(&self, reg: u32) -> bool {
        !self.users_of(reg).is_empty()
    }

    /// The number of reading ops per register, as a mutable starting point for
    /// worklist algorithms that retire uses as they erase ops.
    pub fn use_counts(&self) -> HashMap<u32, usize> {
        self.users
            .iter()
            .map(|(&reg, users)| (reg, users.len()))
            .collect()
    }
}

impl Analysis for DefUse {
    fn build(_: &AnalysisManager, context: &Context, op: OpId) -> Self {
        Self::new(context, op)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Operation,
        builtin::{IntegerType, UnitType, ops},
        func::ops as func_ops,
    };

    #[test]
    fn chains_over_ssa_ops() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);

        let region = context.create_region();
        let arg = context.create_value(i32, None);
        let arg_id = arg.id();
        let block = context.create_block(vec![arg]);
        region.add_block(block.id());
        let func =
            func_ops::func(&context, "f", UnitType::new(&context), Some(region.id())).build();

        let b = block;
        let dead = b.append_op(ops::constant(&context, 7, i32).build());
        let sum = b.append_op(ops::addi(&context, arg_id, arg_id, i32).build());
        let sum_val = sum.result();
        let ret = b.append_op(func_ops::r#return(&context, sum_val).build());

        let am = AnalysisManager::new();
        let du = am.get::<DefUse>(&context, func.id());

        // The block argument is read twice by the add and defined by no op.
        assert_eq!(du.users_of(arg_id.number()), [sum.id(), sum.id()]);
        assert!(du.defs_of(arg_id.number()).is_empty());

        assert_eq!(du.defs_of(sum_val.number()), [sum.id()]);
        assert_eq!(du.users_of(sum_val.number()), [ret.id()]);

        assert!(!du.is_used(dead.result().number()));
        assert_eq!(du.use_counts()[&arg_id.number()], 2);
    }
}
