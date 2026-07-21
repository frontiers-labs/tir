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

use crate::attributes::{AttributeRole, AttributeValue, RegisterAttr};
use crate::backend::regalloc::RegClassId;
use crate::{
    Context, OpId, OpInstance,
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
/// register-valued attributes (consulting the op's `attribute_roles`).
pub fn op_regs(op: &OpInstance) -> OpRegs {
    let mut regs = OpRegs::default();

    // Builtin SSA ops (e.g. the block terminator) name registers positionally.
    for result in &op.results {
        regs.defs.push(RegRef::Virtual {
            id: result.number(),
            class: None,
        });
    }
    for operand in &op.operands {
        regs.uses.push(RegRef::Virtual {
            id: operand.number(),
            class: None,
        });
    }

    // Machine ops carry their register operands in attributes, with a def/use role.
    // An array of registers (e.g. a call's caller-saved clobber list) applies the
    // attribute's role to every element.
    for attr in &op.attributes {
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
        let role = op
            .attribute_roles
            .iter()
            .find(|(name, _)| *name == attr.name)
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
        let mut result = Self::default();
        let mut stack = context.get_op(op).regions.clone();
        while let Some(region) = stack.pop() {
            for block in context.get_region(region).iter(context.clone()) {
                for op_id in block.op_ids() {
                    let instance = context.get_op(op_id);
                    stack.extend(instance.regions.iter().copied());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        IRBuilder, Operation,
        builtin::{IntegerType, UnitType, ops},
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
        let func = ops::func(&context, "f", UnitType::new(&context), Some(region.id())).build();

        let mut b = IRBuilder::new(block);
        let dead = b.insert(ops::constant(&context, 7, i32).build());
        let sum = b.insert(ops::addi(&context, arg_id, arg_id, i32).build());
        let sum_val = sum.result();
        let ret = b.insert(ops::r#return(&context, sum_val).build());

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
