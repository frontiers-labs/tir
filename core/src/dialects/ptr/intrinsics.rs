//! Memory intrinsic implementations shared by every target.

use crate::analysis::effects::{observed_state, produced_state};
use crate::attributes::AttributeValue;
use crate::builtin::{FnType, IntegerType, ModuleOp, StateResource, ops as b};
use crate::func::ops as func_ops;
use crate::ptr::{LoadOpBuilder, MemcpyOp, MemsetOp, PtrType, StoreOpBuilder, ops as p};
use crate::{
    ConstantLike, Context, Intrinsic, Operation, OperationRef, PassError, Symbol, TargetEnv,
    TypeId, ValueId,
};

impl Intrinsic for MemcpyOp {
    fn expand(&self, context: &Context, env: Option<&TargetEnv>) -> Result<(), PassError> {
        let operation = OperationRef::new(context.get_op(self.id()));
        let [destination, source, size] = self.operands()[..3] else {
            unreachable!()
        };
        if let Some(chunks) = inline_chunks(context, size, env)? {
            let mut state = observed_state(operation.op());
            for Chunk { offset, bytes } in chunks {
                let ty = IntegerType::new(context, bytes * 8);
                let src = address(context, &operation, source, offset)?;
                let dst = address(context, &operation, destination, offset)?;
                let mut load = LoadOpBuilder::new(context).ptr(src).result_type(ty);
                if let Some(s) = state {
                    load = load.state(s).state_result(context.get_value(s).ty());
                }
                let load = load.build();
                context.insert_op_before(&operation, &load)?;
                state = produced_state(&context.get_op(load.id()));
                state = store(context, &operation, dst, load.result(), state)?;
            }
            finish(context, &operation, state)?;
            return Ok(());
        }
        library_call(
            context,
            &operation,
            "memcpy",
            vec![destination, source, size],
        )
    }
}

impl Intrinsic for MemsetOp {
    fn expand(&self, context: &Context, env: Option<&TargetEnv>) -> Result<(), PassError> {
        let operation = OperationRef::new(context.get_op(self.id()));
        let [destination, value, size] = self.operands()[..3] else {
            unreachable!()
        };
        if let Some(chunks) = inline_chunks(context, size, env)? {
            let mut state = observed_state(operation.op());
            for Chunk { offset, bytes } in chunks {
                let dst = address(context, &operation, destination, offset)?;
                let mut fill = value;
                if bytes > 1 {
                    let ty = IntegerType::new(context, bytes * 8);
                    let extended = b::extui(context, value, ty).build();
                    context.insert_op_before(&operation, &extended)?;
                    let pattern = (0..bytes).fold(0u64, |bits, byte| bits | (1 << (byte * 8)));
                    let repeated = b::constant(context, pattern as i64, ty).build();
                    context.insert_op_before(&operation, &repeated)?;
                    let product =
                        b::muli(context, extended.result(), repeated.result(), ty).build();
                    context.insert_op_before(&operation, &product)?;
                    fill = product.result();
                }
                state = store(context, &operation, dst, fill, state)?;
            }
            finish(context, &operation, state)?;
            return Ok(());
        }
        library_call(
            context,
            &operation,
            "memset",
            vec![destination, value, size],
        )
    }
}

struct Chunk {
    offset: u64,
    bytes: u32,
}

/// An exact partition: no access may read or write beyond the copied range.
/// Declared widths are legal even at byte alignment. Missing facts permit only
/// byte accesses; in particular, native register width does not prove legality.
fn inline_chunks(
    context: &Context,
    size: ValueId,
    env: Option<&TargetEnv>,
) -> Result<Option<Vec<Chunk>>, PassError> {
    let limit = match env.and_then(|env| env.get("memory_inline_bytes")) {
        None => 64,
        Some(value) => unsigned(value)
            .ok_or_else(|| invalid("memory_inline_bytes must be an unsigned byte count"))?,
    };
    let mut widths = vec![1];
    if let Some(value) = env.and_then(|env| env.get("memory_scalar_bytes")) {
        let AttributeValue::Array(values) = value else {
            return Err(invalid("memory_scalar_bytes must be an array"));
        };
        for value in values {
            let bytes = unsigned(value)
                .and_then(|n| u32::try_from(n).ok())
                .filter(|n| n.is_power_of_two() && *n <= 8)
                .ok_or_else(|| {
                    invalid("memory_scalar_bytes contains an unsupported access width")
                })?;
            widths.push(bytes);
        }
    }
    let Some(constant) = context
        .get_value(size)
        .defining_op()
        .and_then(|id| context.get_op(id).as_interface::<dyn ConstantLike>())
    else {
        return Ok(None);
    };
    let bytes = constant.constant_value().to_u64();
    if bytes > limit {
        return Ok(None);
    }
    widths.sort_unstable_by(|a, b| b.cmp(a));
    let mut offset = 0;
    let mut chunks = Vec::new();
    while offset < bytes {
        // Bound emitted accesses even when the target only permits byte loads.
        if chunks.len() == 8 {
            return Ok(None);
        }
        let &width = widths
            .iter()
            .find(|width| u64::from(**width) <= bytes - offset)
            .expect("byte access is always available");
        chunks.push(Chunk {
            offset,
            bytes: width,
        });
        offset += u64::from(width);
    }
    Ok(Some(chunks))
}

fn unsigned(value: &AttributeValue) -> Option<u64> {
    match value {
        AttributeValue::UInt(n) => Some(*n),
        AttributeValue::Int(n) => u64::try_from(*n).ok(),
        _ => None,
    }
}

fn invalid(message: impl Into<String>) -> PassError {
    PassError::InvalidRuleSet(message.into())
}

fn address(
    context: &Context,
    before: &OperationRef,
    base: ValueId,
    offset: u64,
) -> Result<ValueId, PassError> {
    if offset == 0 {
        return Ok(base);
    }
    let index = b::constant(context, offset as i64, IntegerType::new(context, 64)).build();
    context.insert_op_before(before, &index)?;
    let address = p::ptradd(context, base, index.result(), context.get_value(base).ty()).build();
    context.insert_op_before(before, &address)?;
    Ok(address.result())
}

fn store(
    context: &Context,
    before: &OperationRef,
    ptr: ValueId,
    value: ValueId,
    state: Option<ValueId>,
) -> Result<Option<ValueId>, PassError> {
    let mut store = StoreOpBuilder::new(context).ptr(ptr).value(value);
    if let Some(s) = state {
        store = store.state(s).state_result(context.get_value(s).ty());
    }
    let store = store.build();
    context.insert_op_before(before, &store)?;
    Ok(produced_state(&context.get_op(store.id())))
}

/// Forward the outgoing state even when a zero-sized intrinsic emits no ops.
fn finish(
    context: &Context,
    operation: &OperationRef,
    state: Option<ValueId>,
) -> Result<(), PassError> {
    if let Some(old) = produced_state(operation.op()) {
        let new = state.ok_or_else(|| invalid("intrinsic expansion lost its memory state"))?;
        context.replace_value_uses(old, new);
        if let Some(region) = context.parent_nodes_region(operation.op().id) {
            context.rename_region_results(region, old, new, &[]);
        }
    }
    context.erase_op(operation)
}

fn library_call(
    context: &Context,
    operation: &OperationRef,
    name: &str,
    mut args: Vec<ValueId>,
) -> Result<(), PassError> {
    let mut parent = context.parent_op(operation.op().id);
    let module = loop {
        let id =
            parent.ok_or_else(|| invalid("intrinsic runtime call requires an enclosing module"))?;
        if let Some(module) = context.get_op(id).as_op::<ModuleOp>() {
            break module;
        }
        parent = context.parent_op(id);
    };
    let pointer = PtrType::opaque(context);
    let size = IntegerType::new(context, 64);
    let fill = IntegerType::new(context, 32);
    let types = if name == "memset" {
        [pointer, fill, size]
    } else {
        [pointer, pointer, size]
    };
    let callee = ensure_lambda(context, &module, name, pointer, &types)?;
    if name == "memset" {
        let extended = b::extui(context, args[1], fill).build();
        context.insert_op_before(operation, &extended)?;
        args[1] = extended.result();
    }
    let call = threaded_call(
        context,
        callee,
        args,
        pointer,
        observed_state(operation.op()),
    );
    context.insert_op_before(operation, &call)?;
    finish(
        context,
        operation,
        produced_state(&context.get_op(call.id())),
    )
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
        .resources(&[StateResource::Memory])
        .callee(callee)
        .args(args)
        .result_type(result_type);
    if let Some(state) = state {
        builder = builder
            .state(state)
            .state_result(context.get_value(state).ty());
    }
    builder.build()
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
