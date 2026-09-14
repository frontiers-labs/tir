mod raise_loops;

pub use raise_loops::RaiseLoopsPass;

use std::collections::HashMap;

use tir::analysis::AnalysisManager;
use tir::attributes::AttributeValue;
use tir::builtin::{IntegerType, ModuleOp, ops as b};
use tir::ptr::ops as p;
use tir::{Context, Operation, OperationRef, Pass, PassError, PassTarget};

use crate::cir;

#[derive(Clone)]
struct StructLayout {
    offsets: Vec<u64>,
    size: u64,
}

#[derive(Clone)]
pub struct LowerCirStructsPass;

impl LowerCirStructsPass {
    pub fn new() -> Self {
        Self
    }

    fn descendants(context: &Context, root: &tir::OpHandle) -> Vec<OperationRef> {
        fn visit(context: &Context, operation: &tir::OpHandle, result: &mut Vec<OperationRef>) {
            for region in operation.regions() {
                for block in context.get_region(region).iter(context.clone()) {
                    for operation in block.op_ids() {
                        let operation = context.get_op(operation);
                        result.push(OperationRef::new(operation.clone()));
                        visit(context, &operation, result);
                    }
                }
            }
        }

        let mut result = Vec::new();
        visit(context, root, &mut result);
        result
    }

    fn refresh(context: &Context, operation: &OperationRef) -> OperationRef {
        OperationRef::new(context.get_op(operation.op().id))
    }

    fn layouts(descendants: &[OperationRef]) -> HashMap<String, StructLayout> {
        descendants
            .iter()
            .filter_map(|operation| operation.as_op::<cir::DefineStructOp>())
            .map(|definition| {
                let name = definition.sym_name();
                let offsets = definition
                    .attr("fields")
                    .and_then(|value| match value {
                        AttributeValue::Array(fields) => Some(fields),
                        _ => None,
                    })
                    .unwrap()
                    .iter()
                    .map(|field| {
                        let AttributeValue::Dict(field) = field else {
                            unreachable!();
                        };
                        let AttributeValue::UInt(offset) = field["offset"] else {
                            unreachable!();
                        };
                        offset
                    })
                    .collect();
                (
                    name,
                    StructLayout {
                        offsets,
                        size: definition.size(),
                    },
                )
            })
            .collect()
    }
}

impl Default for LowerCirStructsPass {
    fn default() -> Self {
        Self::new()
    }
}

impl Pass for LowerCirStructsPass {
    fn name(&self) -> &'static str {
        "lower-cir-structs"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<ModuleOp>()
    }

    fn run(
        &mut self,
        operation: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if operation.as_op::<ModuleOp>().is_none() {
            return Ok(());
        }
        let descendants = Self::descendants(context, operation.op());
        let layouts = Self::layouts(&descendants);
        if layouts.is_empty() {
            return Ok(());
        }

        for target in &descendants {
            // An earlier rewrite may have erased this descendant; the list was
            // taken before any of them ran.
            if !target.op().is_live() {
                continue;
            }
            let target = Self::refresh(context, target);
            let Some(member) = target.as_op::<cir::GetMemberOp>() else {
                continue;
            };
            let name = member.struct_name();
            let field = member.field() as usize;
            let offset = layouts[&name].offsets[field];
            let result_type = context.get_value(member.result()).ty();
            let offset_value =
                b::constant(context, offset as i64, IntegerType::new(context, 64)).build();
            context.insert_op_before(&target, &offset_value)?;
            let pointer = p::ptradd(
                context,
                member.operands()[0],
                offset_value.result(),
                result_type,
            )
            .build();
            context.replace_op(&target, &pointer)?;
        }

        for target in &descendants {
            if !target.op().is_live() {
                continue;
            }
            if target.as_op::<cir::CopyStructOp>().is_none() {
                continue;
            }
            let target = Self::refresh(context, target);
            let copy = target.as_op::<cir::CopyStructOp>().unwrap();
            let size = b::constant(
                context,
                layouts[&copy.struct_name()].size as i64,
                IntegerType::new(context, 64),
            )
            .build();
            context.insert_op_before(&target, &size)?;
            let replacement = p::memcpy(
                context,
                copy.operands()[0],
                copy.operands()[1],
                size.result(),
            )
            .build();
            context.replace_op(&target, &replacement)?;
        }

        for target in &descendants {
            if !target.op().is_live() {
                continue;
            }
            if target.as_op::<cir::DefineStructOp>().is_none() {
                continue;
            }
            let target = Self::refresh(context, target);
            if target.as_op::<cir::DefineStructOp>().is_some() {
                context.erase_op(&target)?;
            }
        }
        Ok(())
    }
}
