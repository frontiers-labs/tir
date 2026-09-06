use crate::analysis::AnalysisManager;
use crate::builtin::{FnType, IntegerType, ModuleOp, ops as b};
use crate::func::ops as func_ops;
use crate::ptr::{MemcpyOp, MemsetOp, PtrType};
use crate::{
    Context, OpHandle, Operation, OperationRef, Pass, PassError, PassTarget, Rewriter, Symbol,
    TypeId, ValueId,
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
                for operation in context.get_region(region).op_ids() {
                    let operation = context.get_op(operation);
                    if operation.is::<ModuleOp>() {
                        continue;
                    }
                    let operation = OperationRef::new(operation);
                    if operation.is::<MemcpyOp>() {
                        copies.push(operation.clone());
                    } else if operation.is::<MemsetOp>() {
                        sets.push(operation.clone());
                    }
                    visit(context, operation.op(), copies, sets);
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
        let value = IntegerType::new(context, 32);
        let memcpy = if copies.is_empty() {
            None
        } else {
            Some(ensure_lambda(
                context,
                &module,
                "memcpy",
                pointer,
                &[pointer, pointer, size],
            )?)
        };
        let memset = if sets.is_empty() {
            None
        } else {
            Some(ensure_lambda(
                context,
                &module,
                "memset",
                pointer,
                &[pointer, value, size],
            )?)
        };

        for operation in copies {
            let copy = operation
                .as_op::<MemcpyOp>()
                .expect("operation was collected as ptr.memcpy");
            let args = vec![copy.operands()[0], copy.operands()[1], copy.operands()[2]];
            let call = threaded_call(
                context,
                memcpy.expect("a copy to lower implies a memcpy declaration"),
                args,
                pointer,
                copy.state_operand(),
            );
            replace_threaded(context, rewriter, &operation, &call, copy.state_result())?;
        }
        for operation in sets {
            let set = operation
                .as_op::<MemsetOp>()
                .expect("operation was collected as ptr.memset");
            let extended = b::extui(context, set.operands()[1], value).build();
            rewriter.insert_op_before(&operation, &extended)?;
            let args = vec![set.operands()[0], extended.result(), set.operands()[2]];
            let call = threaded_call(
                context,
                memset.expect("a set to lower implies a memset declaration"),
                args,
                pointer,
                set.state_operand(),
            );
            replace_threaded(context, rewriter, &operation, &call, set.state_result())?;
        }
        Ok(())
    }
}

/// The call an intrinsic becomes, on the chain the intrinsic was on: the library
/// routine touches the same memory the intrinsic did, so it takes the state the
/// intrinsic observed and publishes the one it left.
fn threaded_call(
    context: &Context,
    callee: ValueId,
    args: Vec<ValueId>,
    result_type: TypeId,
    state: Option<ValueId>,
) -> impl Operation {
    let mut builder = func_ops::CallOpBuilder::new(context)
        .callee(callee)
        .args(args)
        .result_type(result_type);
    if let Some(state) = state {
        builder = builder.dep_operand(state).dep_result();
    }
    builder.build()
}

/// Replace `operation` by `call`, handing the state the intrinsic published to
/// the call's own. The shapes differ — a call yields a value the intrinsic did
/// not — so the generic result rewiring in [`Rewriter::replace_op`] does not
/// apply.
fn replace_threaded(
    context: &Context,
    rewriter: &mut Rewriter,
    operation: &OperationRef,
    call: &dyn Operation,
    published: Option<ValueId>,
) -> Result<(), PassError> {
    if let (Some(published), Some(new)) = (
        published,
        context.get_op(call.id()).dep_results().first().copied(),
    ) {
        context.replace_value_uses(published, new);
        // An unordered region names the state it leaves in its result list,
        // which no use list reaches.
        if let Some(region) = context.parent_nodes_region(operation.op().id) {
            context.rename_region_results(region, published, new, &[]);
        }
    }
    rewriter.replace_op(operation, call)
}

/// The λ value of `name`, declaring it at the top of the module when nothing in
/// it names that function yet.
fn ensure_lambda(
    context: &Context,
    module: &ModuleOp,
    name: &str,
    return_type: TypeId,
    argument_types: &[TypeId],
) -> Result<ValueId, PassError> {
    let expected = FnType::new(context, argument_types, return_type);
    let existing = module.body().op_ids().into_iter().find_map(|operation| {
        let instance = context.get_op(operation);
        let symbol = instance.clone().as_interface::<dyn Symbol>()?;
        (symbol.symbol_name() == name)
            .then(|| instance.results().first().copied())
            .flatten()
    });
    if let Some(value) = existing {
        if context.get_value(value).ty() != expected {
            return Err(PassError::InvalidRuleSet(format!(
                "existing {name} declaration has an incompatible type"
            )));
        }
        return Ok(value);
    }
    let declaration = func_ops::declare_op(context, name, return_type, argument_types);
    let value = declaration.fn_value();
    module.body().insert(0, declaration.id());
    Ok(value)
}
