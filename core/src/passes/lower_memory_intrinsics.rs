use crate::analysis::AnalysisManager;
use crate::attributes::AttributeValue;
use crate::builtin::{IntegerType, ModuleOp, ops as b};
use crate::func::{DeclareOp, ops as func_ops};
use crate::ptr::{MemcpyOp, MemsetOp, PtrType};
use crate::{
    Context, OpHandle, Operation, OperationRef, Pass, PassError, PassTarget, Rewriter, TypeId,
};

pub struct LowerMemoryIntrinsicsPass;

impl LowerMemoryIntrinsicsPass {
    pub fn new() -> Self {
        Self
    }

    fn intrinsics(context: &Context, root: &OpHandle) -> (Vec<OperationRef>, Vec<OperationRef>) {
        fn visit(
            context: &Context,
            operation: &OpHandle,
            copies: &mut Vec<OperationRef>,
            sets: &mut Vec<OperationRef>,
        ) {
            for region in operation.regions() {
                for block in context.get_region(region).iter(context.clone()) {
                    for operation in block.op_ids() {
                        let operation = context.get_op(operation);
                        if operation.is::<ModuleOp>() {
                            continue;
                        }
                        let operation = OperationRef::new(operation, Some(block.clone()), None);
                        if operation.is::<MemcpyOp>() {
                            copies.push(operation.clone());
                        } else if operation.is::<MemsetOp>() {
                            sets.push(operation.clone());
                        }
                        visit(context, operation.op(), copies, sets);
                    }
                }
            }
        }

        let mut copies = Vec::new();
        let mut sets = Vec::new();
        visit(context, root, &mut copies, &mut sets);
        (copies, sets)
    }
}

impl Default for LowerMemoryIntrinsicsPass {
    fn default() -> Self {
        Self::new()
    }
}

crate::register_pass!(LowerMemoryIntrinsicsPass, "lower-memory-intrinsics");

impl Pass for LowerMemoryIntrinsicsPass {
    fn name(&self) -> &'static str {
        "lower-memory-intrinsics"
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
        let module = operation
            .as_op::<ModuleOp>()
            .expect("pass target guarantees builtin.module");
        let (copies, sets) = Self::intrinsics(context, operation.op());
        if copies.is_empty() && sets.is_empty() {
            return Ok(());
        }

        let pointer = PtrType::opaque(context);
        let size = IntegerType::new(context, 64);
        if !copies.is_empty() {
            ensure_declaration(
                context,
                &module,
                "memcpy",
                pointer,
                &[pointer, pointer, size],
            )?;
        }
        let value = IntegerType::new(context, 32);
        if !sets.is_empty() {
            ensure_declaration(context, &module, "memset", pointer, &[pointer, value, size])?;
        }

        for operation in copies {
            let copy = operation
                .as_op::<MemcpyOp>()
                .expect("operation was collected as ptr.memcpy");
            let call = func_ops::CallOpBuilder::new(context)
                .args(copy.operands().to_vec())
                .attr("callee", AttributeValue::Str("memcpy".to_string().into()))
                .result_type(pointer)
                .build();
            rewriter.replace_op(&operation, &call)?;
        }
        for operation in sets {
            let set = operation
                .as_op::<MemsetOp>()
                .expect("operation was collected as ptr.memset");
            let extended = b::extui(context, set.operands()[1], value).build();
            rewriter.insert_op_before(&operation, &extended)?;
            let call = func_ops::CallOpBuilder::new(context)
                .args(vec![
                    set.operands()[0],
                    extended.result(),
                    set.operands()[2],
                ])
                .attr("callee", AttributeValue::Str("memset".to_string().into()))
                .result_type(pointer)
                .build();
            rewriter.replace_op(&operation, &call)?;
        }
        Ok(())
    }
}

fn ensure_declaration(
    context: &Context,
    module: &ModuleOp,
    name: &str,
    return_type: TypeId,
    argument_types: &[TypeId],
) -> Result<(), PassError> {
    let declaration = module.body().op_ids().into_iter().find_map(|operation| {
        context
            .get_op(operation)
            .as_op::<DeclareOp>()
            .filter(|declaration| declaration.sym_name() == name)
    });
    if let Some(declaration) = declaration {
        if declaration.ret_type() != Some(return_type)
            || declaration.arg_types().as_deref() != Some(argument_types)
        {
            return Err(PassError::InvalidRuleSet(format!(
                "existing {name} declaration has an incompatible type"
            )));
        }
    } else {
        let declaration = func_ops::declare_op(context, name, return_type, argument_types);
        module.body().insert(0, declaration.id());
    }
    Ok(())
}
