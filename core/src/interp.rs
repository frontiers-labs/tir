//! Executable semantics over green IR.
//!
//! Leaf operations evaluate themselves through the [`Interp`] interface or,
//! when they declare a semantic expression, through the shared tir-symbolic
//! evaluator — one definition of scalar arithmetic serves both folding and
//! interpretation. Control flow (func/scf/cfg) lives here in the driver, which
//! walks regions and calls [`Interp`] only for leaf ops.

use std::collections::HashMap;

use tir_adt::{APFloat, APInt};

use crate::{
    BlockId, Conditional, ConstantLike, Context, CountedLoop, DataLayout, LoopLike, OpId,
    Operation, RegionId, Symbol, ValueId,
    builtin::{
        ConstantFOp, ConstantOp, FloatType, IntegerType, MakeTupleOp, StateType, TokenType,
        TupleGetOp, UnitType,
    },
    func::{CallOp, FuncOp, ReturnOp},
    ptr::{AllocaOp, LoadOp, MemcpyOp, MemsetOp, PtrType, StoreOp},
    scf::{BreakOp, ConditionOp, ContinueOp, ForOp, IfOp, SwitchOp, WhileOp, YieldOp},
    sem,
    state::EntryStateOp,
};

/// A concrete interpreter value: integers of explicit width, floats, tuples,
/// pointers as byte offsets into one flat memory, and the linear/ordering
/// tokens (`!state`, `!token`) that carry no bits.
#[derive(Clone, Debug)]
pub enum Value {
    Int(APInt),
    Float(APFloat),
    Tuple(Vec<Value>),
    Ptr(u64),
    /// A λ node: what a call takes as its callee.
    Function(OpId),
    State,
    Token,
    Unit,
}

impl Value {
    /// The value as a signed decimal integer, for tools and diagnostics.
    pub fn to_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(i.to_i64()),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum InterpError {
    /// The op neither implements [`Interp`] nor declares a semantic expression.
    Unsupported(String),
    OutOfBounds {
        address: u64,
        size: u64,
    },
    Uninitialized {
        address: u64,
        size: u64,
    },
    Message(String),
}

impl std::fmt::Display for InterpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InterpError::Unsupported(name) => write!(f, "cannot interpret {name}"),
            InterpError::OutOfBounds { address, size } => {
                write!(
                    f,
                    "access of [{address:#x}..{:#x}) is out of bounds",
                    address + size
                )
            }
            InterpError::Uninitialized { address, size } => write!(
                f,
                "access of [{address:#x}..{:#x}) hits uninitialized bytes",
                address + size
            ),
            InterpError::Message(text) => write!(f, "{text}"),
        }
    }
}

type Result<T> = std::result::Result<T, InterpError>;

/// The one flat byte-addressed memory an interpretation runs against.
/// Allocations bump upward from a base that keeps null distinct; every byte
/// tracks initialization so an unwritten read aborts instead of inventing data.
pub struct Memory {
    bytes: Vec<u8>,
    written: Vec<bool>,
    next: u64,
}

const MEMORY_BASE: u64 = 1 << 20;

impl Default for Memory {
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            written: Vec::new(),
            next: MEMORY_BASE,
        }
    }
}

impl Memory {
    pub fn alloc(&mut self, size: u64, align: u64) -> u64 {
        let align = align.max(1);
        let aligned = self.next.div_ceil(align) * align;
        self.next = aligned + size;
        let used = (self.next - MEMORY_BASE) as usize;
        self.bytes.resize(used, 0);
        self.written.resize(used, false);
        aligned
    }

    fn check(&self, address: u64, size: u64) -> Result<usize> {
        let offset = address
            .checked_sub(MEMORY_BASE)
            .ok_or(InterpError::OutOfBounds { address, size })?;
        let end = offset + size;
        if end > self.bytes.len() as u64 {
            return Err(InterpError::OutOfBounds { address, size });
        }
        if self.written[offset as usize..end as usize]
            .iter()
            .any(|&w| !w)
        {
            return Err(InterpError::Uninitialized { address, size });
        }
        Ok(offset as usize)
    }

    pub fn read(&self, address: u64, size: u64) -> Result<Vec<u8>> {
        let offset = self.check(address, size)?;
        Ok(self.bytes[offset..offset + size as usize].to_vec())
    }

    pub fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let offset = self.check_bounds(address, bytes.len() as u64)?;
        self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
        for flag in &mut self.written[offset..offset + bytes.len()] {
            *flag = true;
        }
        Ok(())
    }

    fn check_bounds(&self, address: u64, size: u64) -> Result<usize> {
        let offset = address
            .checked_sub(MEMORY_BASE)
            .ok_or(InterpError::OutOfBounds { address, size })?;
        if offset + size > self.bytes.len() as u64 {
            return Err(InterpError::OutOfBounds { address, size });
        }
        Ok(offset as usize)
    }

    fn read_int(&self, address: u64, width_bits: u32) -> Result<APInt> {
        let bytes = self.read(address, width_bits as u64 / 8)?;
        let mut raw = 0u64;
        for (index, byte) in bytes.iter().enumerate() {
            raw |= u64::from(*byte) << (index * 8);
        }
        Ok(APInt::new(width_bits, raw))
    }

    fn write_int(&mut self, address: u64, value: &APInt) -> Result<()> {
        let raw = value.to_u64();
        let bytes: Vec<u8> = (0..value.width() as usize / 8)
            .map(|i| (raw >> (i * 8)) as u8)
            .collect();
        self.write(address, &bytes)
    }
}

/// Concrete evaluation of one leaf operation: `operands[i]` is the value of
/// operand `i`; the returned values bind to the op's results in order,
/// including any trailing `!state` result.
pub trait Interp {
    fn evaluate(&self, operands: &[Value], memory: &mut Memory) -> Result<Vec<Value>>;

    fn verify_interface(
        &self,
        _this: &dyn Operation,
        _context: &Context,
    ) -> std::result::Result<(), crate::Error> {
        Ok(())
    }
}

/// Interpret `function` (a `func.func`) over `arguments` and return the values
/// its `func.return` carries.
pub fn run_function(
    context: &Context,
    function: OpId,
    arguments: Vec<Value>,
) -> Result<Vec<Value>> {
    let mut interp = Interpreter {
        context,
        memory: Memory::default(),
        env: HashMap::new(),
    };
    interp.call_function(function, arguments)
}

fn function_region(context: &Context, function: OpId, index: usize) -> RegionId {
    context.get_op(function).regions()[index]
}

fn region_arguments(context: &Context, region: RegionId) -> Vec<ValueId> {
    let block = context.get_block(context.get_region(region).block_ids()[0]);
    block.arguments().iter().map(|v| v.id()).collect()
}

struct Interpreter<'c> {
    context: &'c Context,
    memory: Memory,
    env: HashMap<ValueId, Value>,
}

/// How one executed region hands control back to its parent.
#[derive(Debug)]
enum Flow {
    /// Values binding to the enclosing op's results, in order.
    Values(Vec<Value>),
    /// An exit terminator leaving the innermost enclosing loop; a `break` or
    /// `continue` can only name that loop's scope token, since `!token` values
    /// cannot cross region boundaries any other way.
    Break(Vec<Value>),
    Continue(Vec<Value>),
    Return(Vec<Value>),
    Goto(BlockId, Vec<Value>),
}

impl Interpreter<'_> {
    fn exec_region(&mut self, region: RegionId) -> Result<Flow> {
        let mut current = self.context.get_region(region).block_ids()[0];
        loop {
            match self.exec_block(current)? {
                Flow::Goto(dest, args) => {
                    let arguments: Vec<ValueId> = self
                        .context
                        .get_block(dest)
                        .arguments()
                        .iter()
                        .map(|v| v.id())
                        .collect();
                    for (argument, value) in arguments.into_iter().zip(args) {
                        self.env.insert(argument, value);
                    }
                    current = dest;
                }
                flow => return Ok(flow),
            }
        }
    }

    fn exec_block(&mut self, block: BlockId) -> Result<Flow> {
        for op_id in self.context.get_block(block).op_ids() {
            if let Some(flow) = self.exec_control(op_id)? {
                return Ok(flow);
            }
        }
        Err(InterpError::Message(
            "block fell through without a terminator".into(),
        ))
    }

    fn bind_results(&mut self, op_id: OpId, values: Vec<Value>) {
        let results = self.context.get_op(op_id).results().to_vec();
        for (result, value) in results.into_iter().zip(values) {
            self.env.insert(result, value);
        }
    }

    /// One operation. Returns the `Flow` leaving the block, or `None` when
    /// execution continues with the next op: value-producing structured ops
    /// bind their own results here, and leaves are evaluated here too.
    fn exec_control(&mut self, op_id: OpId) -> Result<Option<Flow>> {
        let instance = self.context.get_op(op_id);
        if std::env::var_os("TIR_INTERP_TRACE").is_some() {
            eprintln!(
                "interp: {}.{} %{}",
                instance.dialect(),
                instance.name(),
                op_id.number()
            );
        }
        if instance.is::<YieldOp>() || instance.is::<ConditionOp>() {
            // A structured join yields its operands; `scf.condition` forwards
            // [decision, carried ports..] to the while driver the same way.
            return Ok(Some(Flow::Values(self.operand_values(&instance)?)));
        }
        if instance.is::<ReturnOp>() {
            let mut values = self.operand_values(&instance)?;
            // A threaded `!state` operand names memory flowing out of the
            // function; it is not part of the returned tuple.
            let state = StateType::new(self.context);
            if instance
                .operands()
                .last()
                .is_some_and(|&last| self.context.get_value(last).ty() == state)
            {
                values.pop();
            }
            return Ok(Some(Flow::Return(values)));
        }
        if instance.is::<BreakOp>() || instance.is::<ContinueOp>() {
            let mut carried = self.operand_values(&instance)?;
            // A loop exit consumes its scope token when the body declares one;
            // everything past it is the carried values.
            let token = TokenType::new(self.context);
            if self.context.get_value(instance.operands()[0]).ty() == token {
                carried.remove(0);
            }
            return Ok(Some(if instance.is::<BreakOp>() {
                Flow::Break(carried)
            } else {
                Flow::Continue(carried)
            }));
        }
        if instance.is::<IfOp>() {
            let flow = self.exec_if(op_id)?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is::<SwitchOp>() {
            let flow = self.exec_switch(op_id)?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is::<ForOp>() {
            let flow = self.exec_for(op_id)?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is::<WhileOp>() {
            let flow = self.exec_while(op_id)?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is::<CallOp>() {
            let flow = self.exec_call(op_id)?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is::<crate::cfg::BranchOp>() || instance.is::<crate::cfg::CondBranchOp>() {
            return Ok(Some(self.exec_branch(&instance)?));
        }
        let values = self.eval_leaf(op_id)?;
        self.bind_results(op_id, values);
        Ok(None)
    }

    /// A γ gate, loop, or call: run its regions, bind the op's results, and
    /// let the block continue — unless an exit flow leaves through a nested
    /// region, which ends this block too.
    fn exec_value_flow(&mut self, op_id: OpId, flow: Flow) -> Result<Option<Flow>> {
        match flow {
            Flow::Values(values) => {
                self.bind_results(op_id, values);
                Ok(None)
            }
            propagated => Ok(Some(propagated)),
        }
    }

    fn exec_branch(&mut self, instance: &crate::OpHandle) -> Result<Flow> {
        let terminator = instance
            .clone()
            .as_interface::<dyn crate::BranchTerminator>()
            .expect("checked by exec_control");
        let edges = terminator.successor_operands();
        let (dest, arg_ids) = if instance.is::<crate::cfg::CondBranchOp>() {
            let condition = self.value_of(instance.operands()[0])?;
            let taken = condition.to_i64().unwrap_or_default() != 0;
            edges[if taken { 0 } else { 1 }].clone()
        } else {
            edges[0].clone()
        };
        let args = arg_ids
            .iter()
            .map(|&id| self.value_of(id))
            .collect::<Result<Vec<_>>>()?;
        Ok(Flow::Goto(dest, args))
    }

    fn exec_if(&mut self, op_id: OpId) -> Result<Flow> {
        let op = IfOp::from_op_instance(self.context.get_op(op_id));
        let decision = self.value_of(op.decision())?;
        let taken = decision.to_i64().unwrap_or_default() != 0;
        let regions = op.guarded_regions();
        let region = regions[if taken { 0 } else { 1 }].0;
        self.exec_gamma_arm(op_id, region)
    }

    fn exec_switch(&mut self, op_id: OpId) -> Result<Flow> {
        let op = SwitchOp::from_op_instance(self.context.get_op(op_id));
        let predicate = self.value_of(op.decision())?;
        let value = predicate.to_i64().unwrap_or_default();
        let cases = op.case_values();
        let region = cases
            .iter()
            .find(|(_, case)| *case == Some(value))
            .map(|(region, _)| *region)
            .unwrap_or_else(|| cases.last().expect("switch always has a default").0);
        self.exec_gamma_arm(op_id, region)
    }

    /// Run one γ arm: bind its entry arguments to the forwarded inputs, run it,
    /// and turn its yield into the gate's result values.
    fn exec_gamma_arm(&mut self, op_id: OpId, region: RegionId) -> Result<Flow> {
        let inputs = self.context.get_op(op_id).operands()[1..].to_vec();
        let arguments = region_arguments(self.context, region);
        for (argument, &input) in arguments.into_iter().zip(inputs.iter()) {
            let value = self.value_of(input)?;
            self.env.insert(argument, value);
        }
        self.exec_region(region)
    }

    fn exec_for(&mut self, op_id: OpId) -> Result<Flow> {
        let op = ForOp::from_op_instance(self.context.get_op(op_id));
        let lower = self
            .value_of(op.lower_bound())?
            .to_i64()
            .unwrap_or_default();
        let upper = self
            .value_of(op.upper_bound())?
            .to_i64()
            .unwrap_or_default();
        let step = self.value_of(op.step())?.to_i64().unwrap_or_default();

        let body_region = op.handle().regions()[0];
        let token = TokenType::new(self.context);
        let carried_args = carried_arguments(self.context, body_region, token);
        let mut carried: Vec<Value> = op
            .inits()
            .iter()
            .map(|&init| self.value_of(init))
            .collect::<Result<_>>()?;

        let mut counter = lower;
        while counter < upper {
            let flow = self.enter_loop_body(body_region, &carried_args, &carried)?;
            match flow {
                Flow::Values(values) | Flow::Continue(values) => carried = values,
                Flow::Break(values) => return Ok(Flow::Values(values)),
                flow => return Ok(flow),
            }
            counter += step;
        }
        Ok(Flow::Values(carried))
    }

    fn exec_while(&mut self, op_id: OpId) -> Result<Flow> {
        let op = WhileOp::from_op_instance(self.context.get_op(op_id));
        let condition_region = op.handle().regions()[0];
        let body_region = op.handle().regions()[1];
        let condition_args = region_arguments(self.context, condition_region);
        let token = TokenType::new(self.context);
        let body_args = carried_arguments(self.context, body_region, token);

        let mut carried: Vec<Value> = op
            .inits()
            .iter()
            .map(|&init| self.value_of(init))
            .collect::<Result<_>>()?;

        loop {
            for (argument, value) in condition_args.iter().zip(&carried) {
                self.env.insert(*argument, value.clone());
            }
            let forwarded = match self.exec_region(condition_region)? {
                Flow::Values(values) => values,
                flow => return Ok(flow),
            };
            let (decision, forwarded) = forwarded.split_first().expect("scf.condition decides");
            if decision.to_i64().unwrap_or_default() == 0 {
                return Ok(Flow::Values(forwarded.to_vec()));
            }
            let flow = self.enter_loop_body(body_region, &body_args, forwarded)?;
            match flow {
                Flow::Values(values) | Flow::Continue(values) => carried = values,
                Flow::Break(values) => return Ok(Flow::Values(values)),
                flow => return Ok(flow),
            }
        }
    }

    fn enter_loop_body(
        &mut self,
        body_region: RegionId,
        carried_args: &[ValueId],
        carried: &[Value],
    ) -> Result<Flow> {
        let token = TokenType::new(self.context);
        for argument in region_arguments(self.context, body_region) {
            let value = if self.context.get_value(argument).ty() == token {
                Value::Token
            } else {
                carried[carried_args
                    .iter()
                    .position(|&arg| arg == argument)
                    .expect("loop body argument must be a scope token or a carried port")]
                .clone()
            };
            self.env.insert(argument, value);
        }
        self.exec_region(body_region)
    }

    fn exec_call(&mut self, op_id: OpId) -> Result<Flow> {
        let op = CallOp::from_op_instance(self.context.get_op(op_id));
        let Value::Function(definition) = self.value_of(op.callee())? else {
            return Err(InterpError::Unsupported(
                "a call whose callee is not a definition".into(),
            ));
        };
        let arguments = op
            .args()
            .iter()
            .map(|&arg| self.value_of(arg))
            .collect::<Result<_>>()?;
        let mut returned = self.call_function(definition, arguments)?;
        // The call's own `!state` result, when present, is the callee's
        // outgoing memory chain.
        if op.state_result().is_some() {
            returned.push(Value::State);
        }
        Ok(Flow::Values(returned))
    }

    fn call_function(&mut self, function: OpId, arguments: Vec<Value>) -> Result<Vec<Value>> {
        let func = FuncOp::from_op_instance(self.context.get_op(function));
        let body_region = function_region(self.context, function, 0);
        let params = region_arguments(self.context, body_region);
        if params.len() != arguments.len() {
            return Err(InterpError::Message(format!(
                "@{} takes {} arguments, got {}",
                func.symbol_name(),
                params.len(),
                arguments.len()
            )));
        }
        let saved: Vec<(ValueId, Value)> = params
            .iter()
            .filter_map(|param| self.env.get(param).cloned().map(|v| (*param, v)))
            .collect();
        for (param, value) in params.into_iter().zip(arguments) {
            self.env.insert(param, value);
        }
        let outcome = self.exec_region(body_region);
        let result = match outcome? {
            Flow::Return(values) => Ok(values),
            other => Err(InterpError::Message(format!(
                "@{} ended without func.return ({other:?})",
                func.symbol_name()
            ))),
        };
        for (param, value) in saved {
            self.env.insert(param, value);
        }
        result
    }

    fn operand_values(&self, instance: &crate::OpHandle) -> Result<Vec<Value>> {
        instance
            .operands()
            .iter()
            .map(|&operand| self.value_of(operand))
            .collect()
    }

    /// A λ of the module is a constant of the program: it needs no binding, so
    /// a value the environment does not hold may still be one.
    fn value_of(&self, value: ValueId) -> Result<Value> {
        if let Some(bound) = self.env.get(&value) {
            return Ok(bound.clone());
        }
        self.lambda(value).ok_or_else(|| missing_value_id(value))
    }

    fn lambda(&self, value: ValueId) -> Option<Value> {
        let definition = self.context.get_value(value).defining_op()?;
        self.context
            .get_op(definition)
            .is::<FuncOp>()
            .then_some(Value::Function(definition))
    }

    /// Evaluate a leaf op: through its [`Interp`] impl when it has one, else
    /// through its declared semantic expression.
    fn eval_leaf(&mut self, op_id: OpId) -> Result<Vec<Value>> {
        let instance = self.context.get_op(op_id);
        let operands = self.operand_values(&instance)?;
        if let Some(interp) = instance.clone().as_interface::<dyn Interp>() {
            return interp.evaluate(&operands, &mut self.memory);
        }
        self.eval_semantic(&instance, &operands)
    }

    fn eval_semantic(&self, instance: &crate::OpHandle, operands: &[Value]) -> Result<Vec<Value>> {
        let spelled = format!("{}.{}", instance.dialect(), instance.name());
        let mut graph = sem::SemGraph::new();
        if instance
            .clone()
            .as_dyn_op()
            .semantic_expr(&mut graph)
            .is_none()
        {
            return Err(InterpError::Unsupported(spelled));
        }
        let pointer_width = pointer_width(self.context, instance);
        let symbols: Vec<sem::Value> = operands
            .iter()
            .map(|value| to_sem_value(value, pointer_width))
            .collect::<Result<_>>()?;
        let result = sem::execute(&graph, &symbols);
        let results = instance.results();
        if results.len() != 1 {
            return Err(InterpError::Message(format!(
                "{spelled} must have exactly one result"
            )));
        }
        let ty = self.context.get_value(results[0]).ty();
        Ok(vec![from_sem_value(
            self.context,
            result,
            ty,
            pointer_width,
        )?])
    }
}

fn missing_value_id(value: ValueId) -> InterpError {
    InterpError::Message(format!(
        "value %{n} used before it is defined",
        n = value.number()
    ))
}

fn pointer_width(context: &Context, instance: &crate::OpHandle) -> u32 {
    DataLayout::for_instance(context, instance)
        .and_then(|layout| layout.pointer_size())
        .unwrap_or(64)
}

/// The loop body's carried arguments: every entry argument but the token scope.
fn carried_arguments(context: &Context, region: RegionId, token: crate::TypeId) -> Vec<ValueId> {
    region_arguments(context, region)
        .into_iter()
        .filter(|&argument| context.get_value(argument).ty() != token)
        .collect()
}

fn to_sem_value(value: &Value, pointer_width: u32) -> Result<sem::Value> {
    Ok(match value {
        Value::Int(int) => sem::Value::Int(int.clone()),
        Value::Float(float) => sem::Value::Float(float.clone()),
        Value::Ptr(address) => sem::Value::Int(APInt::new(pointer_width, *address)),
        Value::Tuple(_) | Value::Function(_) | Value::State | Value::Token | Value::Unit => {
            return Err(InterpError::Message(
                "value kind has no semantic-expression form".into(),
            ));
        }
    })
}

fn from_sem_value(
    context: &Context,
    value: sem::Value,
    ty: crate::TypeId,
    pointer_width: u32,
) -> Result<Value> {
    let ty_data = context.get_type_data(ty);
    if (ty_data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<PtrType>()
        .is_some()
    {
        let sem::Value::Int(int) = value else {
            return Err(InterpError::Message(
                "pointer result must be integral".into(),
            ));
        };
        return Ok(Value::Ptr(int.to_u64()));
    }
    if let Some(float) = (ty_data.as_ref() as &dyn std::any::Any).downcast_ref::<FloatType>() {
        let sem::Value::Float(float_value) = value else {
            return Err(InterpError::Message("float result must be a float".into()));
        };
        return Ok(Value::Float(float_value.convert(
            float.exp_width(),
            float.mant_width(),
            false,
        )));
    }
    if (ty_data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<UnitType>()
        .is_some()
    {
        return Ok(Value::Unit);
    }
    let int = match value {
        sem::Value::Int(int) => int,
        sem::Value::RawBits(bits) => bits.to_apint(),
        _ => {
            return Err(InterpError::Message(
                "integer result must be integral".into(),
            ));
        }
    };
    Ok(widen_int_to_type(context, int, ty, pointer_width))
}

/// Semantic evaluation coerces widths freely; pin the result back to the width
/// the IR declares.
fn widen_int_to_type(
    context: &Context,
    int: APInt,
    ty: crate::TypeId,
    pointer_width: u32,
) -> Value {
    let ty_data = context.get_type_data(ty);
    let width = if (ty_data.as_ref() as &dyn std::any::Any)
        .downcast_ref::<PtrType>()
        .is_some()
    {
        pointer_width
    } else if let Some(integer) =
        (ty_data.as_ref() as &dyn std::any::Any).downcast_ref::<IntegerType>()
    {
        integer.width()
    } else {
        return Value::Int(int);
    };
    if int.width() == width {
        Value::Int(int)
    } else if int.width() > width {
        Value::Int(int.truncate(width))
    } else if int.is_signed() {
        Value::Int(int.sign_extend(width))
    } else {
        Value::Int(int.zero_extend(width))
    }
}

// ── Leaf op implementations ────────────────────────────────────────────────

impl Interp for ConstantOp {
    fn evaluate(&self, _operands: &[Value], _memory: &mut Memory) -> Result<Vec<Value>> {
        Ok(vec![Value::Int(self.constant_value())])
    }
}

impl Interp for ConstantFOp {
    fn evaluate(&self, _operands: &[Value], _memory: &mut Memory) -> Result<Vec<Value>> {
        let context = self.handle().context.upgrade();
        let value = match self.attr("value") {
            Some(crate::attributes::AttributeValue::F64(value)) => value,
            _ => {
                return Err(InterpError::Message(
                    "constantf must carry an F64 value".into(),
                ));
            }
        };
        let ty = context.get_value(self.result()).ty();
        let ty_data = context.get_type_data(ty);
        let float = (ty_data.as_ref() as &dyn std::any::Any)
            .downcast_ref::<FloatType>()
            .expect("constantf result must be a float");
        let converted =
            APFloat::from_f64(value).convert(float.exp_width(), float.mant_width(), false);
        Ok(vec![Value::Float(converted)])
    }
}

impl Interp for MakeTupleOp {
    fn evaluate(&self, operands: &[Value], _memory: &mut Memory) -> Result<Vec<Value>> {
        Ok(vec![Value::Tuple(operands.to_vec())])
    }
}

impl Interp for TupleGetOp {
    fn evaluate(&self, operands: &[Value], _memory: &mut Memory) -> Result<Vec<Value>> {
        let Value::Tuple(elements) = &operands[0] else {
            return Err(InterpError::Message(
                "tuple_get operand must be a tuple".into(),
            ));
        };
        let element = elements.get(self.index()).ok_or_else(|| {
            InterpError::Message(format!("tuple_get index {} out of bounds", self.index()))
        })?;
        Ok(vec![element.clone()])
    }
}

impl Interp for EntryStateOp {
    fn evaluate(&self, _operands: &[Value], _memory: &mut Memory) -> Result<Vec<Value>> {
        Ok(vec![Value::State])
    }
}

impl Interp for AllocaOp {
    fn evaluate(&self, _operands: &[Value], memory: &mut Memory) -> Result<Vec<Value>> {
        let address = memory.alloc(self.size(), self.align());
        let mut results = vec![Value::Ptr(address)];
        if self.state_result().is_some() {
            results.push(Value::State);
        }
        Ok(results)
    }
}

impl Interp for LoadOp {
    fn evaluate(&self, operands: &[Value], memory: &mut Memory) -> Result<Vec<Value>> {
        let context = self.handle().context.upgrade();
        let Value::Ptr(address) = &operands[0] else {
            return Err(InterpError::Message(
                "ptr.load operand must be a pointer".into(),
            ));
        };
        let ty = context.get_value(self.result()).ty();
        let value = read_typed(memory, context.get_type_data(ty).as_ref(), *address)?;
        let mut results = vec![value];
        if self.state_result().is_some() {
            results.push(Value::State);
        }
        Ok(results)
    }
}

impl Interp for StoreOp {
    fn evaluate(&self, operands: &[Value], memory: &mut Memory) -> Result<Vec<Value>> {
        let context = self.handle().context.upgrade();
        let Value::Ptr(address) = &operands[1] else {
            return Err(InterpError::Message(
                "ptr.store destination must be a pointer".into(),
            ));
        };
        let ty = context.get_value(self.operands()[0]).ty();
        write_typed(
            memory,
            context.get_type_data(ty).as_ref(),
            *address,
            &operands[0],
        )?;
        let mut results = Vec::new();
        if self.state_result().is_some() {
            results.push(Value::State);
        }
        Ok(results)
    }
}

impl Interp for MemcpyOp {
    fn evaluate(&self, operands: &[Value], memory: &mut Memory) -> Result<Vec<Value>> {
        let Value::Ptr(destination) = &operands[0] else {
            return Err(InterpError::Message(
                "memcpy destination must be a pointer".into(),
            ));
        };
        let Value::Ptr(source) = &operands[1] else {
            return Err(InterpError::Message(
                "memcpy source must be a pointer".into(),
            ));
        };
        let size = operands[2].to_i64().unwrap_or_default() as u64;
        let bytes = memory.read(*source, size)?;
        memory.write(*destination, &bytes)?;
        let mut results = Vec::new();
        if self.state_result().is_some() {
            results.push(Value::State);
        }
        Ok(results)
    }
}

impl Interp for MemsetOp {
    fn evaluate(&self, operands: &[Value], memory: &mut Memory) -> Result<Vec<Value>> {
        let Value::Ptr(destination) = &operands[0] else {
            return Err(InterpError::Message(
                "memset destination must be a pointer".into(),
            ));
        };
        let fill = operands[1].to_i64().unwrap_or_default() as u8;
        let size = operands[2].to_i64().unwrap_or_default() as u64;
        memory.write(*destination, &vec![fill; size as usize])?;
        let mut results = Vec::new();
        if self.state_result().is_some() {
            results.push(Value::State);
        }
        Ok(results)
    }
}

fn int_width(ty: &dyn crate::Type) -> Option<u32> {
    let integer = (ty as &dyn std::any::Any).downcast_ref::<IntegerType>()?;
    Some(integer.width())
}

fn read_typed(memory: &Memory, ty: &dyn crate::Type, address: u64) -> Result<Value> {
    if let Some(width) = int_width(ty) {
        return memory.read_int(address, width).map(Value::Int);
    }
    if let Some(float) = (ty as &dyn std::any::Any).downcast_ref::<FloatType>() {
        let bits = memory.read_int(address, float.bit_width())?;
        return Ok(Value::Float(APFloat::from_bits(
            float.exp_width(),
            float.mant_width(),
            false,
            bits.to_u64() as u128,
        )));
    }
    if (ty as &dyn std::any::Any)
        .downcast_ref::<PtrType>()
        .is_some()
    {
        return memory
            .read_int(address, 64)
            .map(|bits| Value::Ptr(bits.to_u64()));
    }
    Err(InterpError::Message(
        "loads of this type are not interpretable yet".into(),
    ))
}

fn write_typed(
    memory: &mut Memory,
    ty: &dyn crate::Type,
    address: u64,
    value: &Value,
) -> Result<()> {
    let mismatch = || InterpError::Message("store value must match the pointee".into());
    if int_width(ty).is_some() {
        let Value::Int(int) = value else {
            return Err(mismatch());
        };
        return memory.write_int(address, int);
    }
    if let Some(float) = (ty as &dyn std::any::Any).downcast_ref::<FloatType>() {
        let Value::Float(float_value) = value else {
            return Err(mismatch());
        };
        let bits = APInt::new(float.bit_width(), float_value.to_bits() as u64);
        return memory.write_int(address, &bits);
    }
    if (ty as &dyn std::any::Any)
        .downcast_ref::<PtrType>()
        .is_some()
    {
        let Value::Ptr(target) = value else {
            return Err(mismatch());
        };
        return memory.write_int(address, &APInt::new(64, *target));
    }
    Err(InterpError::Message(
        "stores of this type are not interpretable yet".into(),
    ))
}
