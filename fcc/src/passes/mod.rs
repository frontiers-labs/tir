mod raise_loops;

pub use raise_loops::RaiseLoopsPass;

use std::collections::HashMap;

use tir::analysis::AnalysisManager;
use tir::attributes::AttributeValue;
use tir::builtin::{IntegerType, ModuleOp, ops as b};
use tir::ptr::{PtrType, ops as p};
use tir::{Context, Operation, OperationRef, Pass, PassError, PassTarget, Rewriter, ValueId};

use crate::cir;

#[derive(Clone)]
struct StructFieldLayout {
    ty: tir::TypeId,
    offset: u64,
}

#[derive(Clone)]
struct StructLayout {
    fields: Vec<StructFieldLayout>,
}

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

    fn string_attribute(operation: &impl Operation, name: &str) -> String {
        match operation.attr(name) {
            Some(AttributeValue::Str(value)) => value.to_string(),
            _ => panic!("{name} must be a string attribute"),
        }
    }

    fn uint_attribute(operation: &impl Operation, name: &str) -> u64 {
        match operation.attr(name) {
            Some(AttributeValue::UInt(value)) => value,
            _ => panic!("{name} must be an unsigned integer attribute"),
        }
    }

    fn layouts(descendants: &[OperationRef]) -> HashMap<String, StructLayout> {
        descendants
            .iter()
            .filter_map(|operation| operation.as_op::<cir::DefineStructOp>())
            .map(|definition| {
                let name = Self::string_attribute(&definition, "sym_name");
                let fields = definition
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
                        let AttributeValue::Type(ty) = field["type"] else {
                            unreachable!();
                        };
                        let AttributeValue::UInt(offset) = field["offset"] else {
                            unreachable!();
                        };
                        StructFieldLayout { ty, offset }
                    })
                    .collect();
                (name, StructLayout { fields })
            })
            .collect()
    }

    fn offset_pointer(
        context: &Context,
        rewriter: &mut Rewriter,
        target: &OperationRef,
        base: ValueId,
        offset: u64,
        result_type: tir::TypeId,
    ) -> Result<ValueId, PassError> {
        let offset = b::constant(context, offset as i64, IntegerType::new(context, 64)).build();
        rewriter.insert_op_before(target, &offset)?;
        let pointer = p::ptradd(context, base, offset.result(), result_type).build();
        let result = pointer.result();
        rewriter.insert_op_before(target, &pointer)?;
        Ok(result)
    }

    fn insert_copy(
        context: &Context,
        rewriter: &mut Rewriter,
        target: &OperationRef,
        layouts: &HashMap<String, StructLayout>,
        name: &str,
        destination: ValueId,
        source: ValueId,
    ) -> Result<(), PassError> {
        for field in &layouts[name].fields {
            let pointer_type = PtrType::opaque(context);
            let destination = Self::offset_pointer(
                context,
                rewriter,
                target,
                destination,
                field.offset,
                pointer_type,
            )?;
            let source = Self::offset_pointer(
                context,
                rewriter,
                target,
                source,
                field.offset,
                pointer_type,
            )?;
            let field_type = context.get_type_data(field.ty);
            if let Some(structure) =
                (field_type.as_ref() as &dyn std::any::Any).downcast_ref::<cir::StructType>()
            {
                Self::insert_copy(
                    context,
                    rewriter,
                    target,
                    layouts,
                    structure.name(),
                    destination,
                    source,
                )?;
            } else {
                let load = p::load(context, source, field.ty).build();
                let value = load.result();
                rewriter.insert_op_before(target, &load)?;
                let store = p::store(context, value, destination).build();
                rewriter.insert_op_before(target, &store)?;
            }
        }
        Ok(())
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
        rewriter: &mut Rewriter,
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
            if !context.has_operation(target.op().id) {
                continue;
            }
            let target = Self::refresh(context, target);
            let Some(member) = target.as_op::<cir::GetMemberOp>() else {
                continue;
            };
            let name = Self::string_attribute(&member, "struct_name");
            let field = Self::uint_attribute(&member, "field") as usize;
            let offset = layouts[&name].fields[field].offset;
            let result_type = context.get_value(member.result()).ty();
            let offset_value =
                b::constant(context, offset as i64, IntegerType::new(context, 64)).build();
            rewriter.insert_op_before(&target, &offset_value)?;
            let pointer = p::ptradd(
                context,
                member.operands()[0],
                offset_value.result(),
                result_type,
            )
            .build();
            rewriter.replace_op(&target, &pointer)?;
        }

        for target in &descendants {
            if !context.has_operation(target.op().id) {
                continue;
            }
            if target.as_op::<cir::CopyStructOp>().is_none() {
                continue;
            }
            let target = Self::refresh(context, target);
            let copy = target.as_op::<cir::CopyStructOp>().unwrap();
            Self::insert_copy(
                context,
                rewriter,
                &target,
                &layouts,
                &Self::string_attribute(&copy, "struct_name"),
                copy.operands()[0],
                copy.operands()[1],
            )?;
            rewriter.erase_op(&target)?;
        }

        for target in &descendants {
            if !context.has_operation(target.op().id) {
                continue;
            }
            if target.as_op::<cir::DefineStructOp>().is_none() {
                continue;
            }
            let target = Self::refresh(context, target);
            if target.as_op::<cir::DefineStructOp>().is_some() {
                rewriter.erase_op(&target)?;
            }
        }
        Ok(())
    }
}
