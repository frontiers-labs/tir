//! Executable semantics over green IR.
//!
//! Leaf operations evaluate themselves through the [`Interp`] interface or,
//! when they declare a semantic expression, through the shared tir-symbolic
//! evaluator — one definition of scalar arithmetic serves both folding and
//! interpretation. Control flow (func/scf/cfg) lives here in the driver, which
//! walks regions and calls [`Interp`] only for leaf ops.

use std::collections::HashMap;

use tir_adt::{APFloat, APInt, RoundingMode};

use crate::{
    BlockId, Context, DataLayout, Gamma, OpId, Operation, RegionId, Theta, ValueId,
    builtin::{FloatType, IntegerType, UnitType},
    ptr::PtrType,
    sem,
};

/// A concrete interpreter value: integers of explicit width, floats, tuples,
/// and pointers as byte offsets into one flat memory. Dependencies are never
/// bound: they order evaluation and evaluate to nothing.
#[derive(Clone, Debug)]
pub enum Value {
    Int(APInt),
    Float(APFloat),
    Tuple(Vec<Value>),
    Ptr(u64),
    /// A λ node: what a call takes as its callee.
    Function(OpId),
    Rounding(RoundingMode),
    FpEnvironment(FloatEnvironment),
    /// A type a layout region measures.
    Type(crate::TypeId),
    /// The spec a layout region reads.
    Layout(std::sync::Arc<crate::layout::LayoutSpec>),
    Unit,
    /// Deferred undefined behavior: what a flagged op yields where its flag
    /// fails, and a load of memory nothing wrote. A pure op reading it yields
    /// it; a branch on it, or an access through it, is undefined.
    Poison,
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
    /// The run executed the operations it was allowed and stopped.
    StepLimit,
    /// An op ran where its `requires:` fails, or control or an access
    /// depends on poison.
    Undefined(String),
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
            InterpError::StepLimit => write!(f, "step limit exceeded"),
            InterpError::Undefined(what) => write!(f, "undefined behavior: {what}"),
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

pub use crate::fp::FloatEnvironment;

#[derive(Default)]
pub struct ExecutionState {
    pub memory: Memory,
    pub fp_environment: FloatEnvironment,
    pub(crate) layout: Option<crate::layout::LayoutRun>,
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
        if offset + size > self.bytes.len() as u64 {
            return Err(InterpError::OutOfBounds { address, size });
        }
        Ok(offset as usize)
    }

    pub fn read(&self, address: u64, size: u64) -> Result<Vec<u8>> {
        let offset = self.check(address, size)?;
        let end = offset + size as usize;
        if self.written[offset..end].iter().any(|&written| !written) {
            return Err(InterpError::Uninitialized { address, size });
        }
        Ok(self.bytes[offset..end].to_vec())
    }

    fn is_written(&self, address: u64, size: u64) -> Result<bool> {
        let offset = self.check(address, size)?;
        Ok(self.written[offset..offset + size as usize]
            .iter()
            .all(|&written| written))
    }

    fn forget(&mut self, address: u64, size: u64) -> Result<()> {
        let offset = self.check(address, size)?;
        self.written[offset..offset + size as usize].fill(false);
        Ok(())
    }

    fn copy(&mut self, destination: u64, source: u64, size: u64) -> Result<()> {
        let from = self.check(source, size)?;
        let to = self.check(destination, size)?;
        let size = size as usize;
        self.bytes.copy_within(from..from + size, to);
        self.written.copy_within(from..from + size, to);
        Ok(())
    }

    pub fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
        let offset = self.check(address, bytes.len() as u64)?;
        self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
        for flag in &mut self.written[offset..offset + bytes.len()] {
            *flag = true;
        }
        Ok(())
    }

    fn read_int(&self, address: u64, width_bits: u64) -> Result<APInt> {
        let bytes = self.read(address, width_bits / 8)?;
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

// The range primitive runs before the shared recipe can touch any cell.
struct ScalarMemory<'a>(&'a mut Memory);
impl crate::ptr::semantics::Access for ScalarMemory<'_> {
    type Bits = (u64, u32);
    type Error = InterpError;
    fn constant(&mut self, v: u64, w: u32) -> Self::Bits {
        (v, w)
    }
    fn scalar(&mut self, k: crate::ptr::semantics::Scalar, a: &[Self::Bits]) -> Self::Bits {
        use crate::ptr::semantics::Scalar;
        match k {
            Scalar::Add => (a[0].0.wrapping_add(a[1].0), a[0].1),
            Scalar::And => (a[0].0 & a[1].0, a[0].1),
            Scalar::Not => ((!a[0].0) & ((1u64 << a[0].1) - 1), a[0].1),
            Scalar::Extract => (
                (a[0].0 >> a[2].0) & ((1u64 << (a[1].0 - a[2].0 + 1)) - 1),
                (a[1].0 - a[2].0 + 1) as u32,
            ),
            Scalar::Concat => ((a[0].0 << a[1].1) | a[1].0, a[0].1 + a[1].1),
            Scalar::If => {
                if a[0].0 != 0 {
                    a[1]
                } else {
                    a[2]
                }
            }
        }
    }
    fn range(&mut self, p: Self::Bits, n: Self::Bits) -> Result<Self::Bits> {
        self.0.check(p.0, n.0)?;
        Ok((1, 1))
    }
    fn read(&mut self, p: Self::Bits) -> Self::Bits {
        let i = (p.0 - MEMORY_BASE) as usize;
        (
            u64::from(self.0.bytes[i]) | (u64::from(self.0.written[i]) << 8),
            9,
        )
    }
    fn write(&mut self, p: Self::Bits, cell: Self::Bits) {
        let i = (p.0 - MEMORY_BASE) as usize;
        self.0.bytes[i] = cell.0 as u8;
        self.0.written[i] = cell.0 & 256 != 0;
    }
}

/// Concrete evaluation of one leaf operation: `operands[i]` is the value of
/// value operand `i`; the returned values bind to the op's value results in
/// order. Dependencies carry nothing and are neither passed nor returned.
#[tir_macros::operation_interface]
pub trait Interp {
    fn evaluate(&self, operands: &[Value], state: &mut ExecutionState) -> Result<Vec<Value>>;
}

/// Interpret `function` (a `func.func`) over `arguments` and return the values
/// its `func.return` carries.
pub fn run_function(
    context: &Context,
    function: OpId,
    arguments: Vec<Value>,
) -> Result<Vec<Value>> {
    run_function_within(context, function, arguments, None)
}

/// [`run_function`] that gives up after `steps` operations: a program that
/// need not terminate can still be compared against another, since both fail
/// the same way.
pub fn run_function_within(
    context: &Context,
    function: OpId,
    arguments: Vec<Value>,
    steps: Option<u64>,
) -> Result<Vec<Value>> {
    run_function_in_state(
        context,
        function,
        arguments,
        &mut ExecutionState::default(),
        steps,
    )
}

pub fn run_function_in_state(
    context: &Context,
    function: OpId,
    arguments: Vec<Value>,
    state: &mut ExecutionState,
    steps: Option<u64>,
) -> Result<Vec<Value>> {
    Interpreter {
        context,
        state,
        env: HashMap::new(),
        steps_left: steps,
    }
    .call_function(function, arguments)
}

/// Run `region` with `arguments` bound to its entry, to the values a
/// terminator hands back: a `layout.result`, or an unordered body's results.
pub fn run_region(
    context: &Context,
    region: RegionId,
    arguments: Vec<Value>,
    state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let mut interpreter = Interpreter {
        context,
        state,
        env: HashMap::new(),
        steps_left: None,
    };
    for (argument, value) in region_arguments(context, region).into_iter().zip(arguments) {
        interpreter.env.insert(argument, value);
    }
    match interpreter.exec_region(region)? {
        Flow::Return(values) | Flow::Values(values) => Ok(values),
        flow => Err(InterpError::Message(format!(
            "a region ended without a result ({flow:?})"
        ))),
    }
}

fn function_region(context: &Context, function: OpId, index: usize) -> RegionId {
    context.get_op(function).regions()[index]
}

fn region_arguments(context: &Context, region: RegionId) -> Vec<ValueId> {
    context
        .get_region(region)
        .ports()
        .iter()
        .map(|v| v.id())
        .collect()
}

struct Interpreter<'c, 's> {
    context: &'c Context,
    state: &'s mut ExecutionState,
    env: HashMap<ValueId, Value>,
    /// Operations this run may still execute, where a limit was set.
    steps_left: Option<u64>,
}

/// How one executed region hands control back to its parent.
#[derive(Debug)]
enum Flow {
    /// Values binding to the enclosing op's results, in order.
    Values(Vec<Value>),
    Return(Vec<Value>),
    Goto(BlockId, Vec<Value>),
}

impl Interpreter<'_, '_> {
    fn exec_region(&mut self, region: RegionId) -> Result<Flow> {
        if self.context.get_region(region).is_nodes() {
            return self.exec_nodes_region(region);
        }
        let mut current = self.context.get_region(region).block_ids()[0];
        loop {
            match self.exec_block(current)? {
                Flow::Goto(dest, args) => {
                    let arguments: Vec<ValueId> = self
                        .context
                        .get_block(dest)
                        .value_arguments()
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

    fn exec_nodes_region(&mut self, region: RegionId) -> Result<Flow> {
        let results = self.context.get_region(region).results();
        let mut evaluated = std::collections::HashSet::new();
        for &result in &results {
            self.demand(result, region, &mut evaluated)?;
        }
        self.context
            .get_region(region)
            .value_results()
            .iter()
            .map(|&result| self.value_of(result))
            .collect::<Result<Vec<_>>>()
            .map(Flow::Values)
    }

    fn demand(
        &mut self,
        value: ValueId,
        region: RegionId,
        evaluated: &mut std::collections::HashSet<OpId>,
    ) -> Result<()> {
        let Some(op_id) = self.context.get_value(value).defining_op() else {
            return Ok(());
        };
        if self.context.parent_nodes_region(op_id) != Some(region) || !evaluated.insert(op_id) {
            return Ok(());
        }
        for value in crate::region::values_read(self.context, op_id) {
            self.demand(value, region, evaluated)?;
        }
        match self.exec_control(op_id)? {
            None => Ok(()),
            Some(flow) => Err(InterpError::Message(format!(
                "an unordered region has no control flow to take ({flow:?})"
            ))),
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
        let results = self.context.get_op(op_id).value_results().to_vec();
        for (result, value) in results.into_iter().zip(values) {
            self.env.insert(result, value);
        }
    }

    fn exec_control(&mut self, op_id: OpId) -> Result<Option<Flow>> {
        if let Some(left) = &mut self.steps_left {
            if *left == 0 {
                return Err(InterpError::StepLimit);
            }
            *left -= 1;
        }
        let instance = self.context.get_op(op_id);
        if std::env::var_os("TIR_INTERP_TRACE").is_some() {
            eprintln!(
                "interp: {}.{} %{}",
                instance.dialect(),
                instance.name(),
                op_id.number()
            );
        }
        if instance.is_operation(&crate::scf::YIELD) {
            // A loop's back edge yields what the next iteration carries.
            return Ok(Some(Flow::Values(self.operand_values(&instance)?)));
        }
        if instance.is_operation(&crate::func::RETURN)
            || instance.is_operation(&crate::layout::RESULT)
        {
            return Ok(Some(Flow::Return(self.operand_values(&instance)?)));
        }
        // A counted loop whose body is still a block list has no declared
        // binding, so it is driven off its own shape.
        if instance.is_operation(&crate::scf::ORDERED_FOR) {
            let flow = self.exec_ordered_for(op_id)?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is_operation(&crate::func::CALL) {
            let flow = self.exec_call(op_id)?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is_operation(&crate::fp::ops::ROUND) {
            let flow = self.exec_round(&instance)?;
            return self.exec_value_flow(op_id, flow);
        }
        if let Some(theta) = instance.clone().as_interface::<dyn Theta>() {
            let flow = self.exec_theta(&instance, theta.as_ref())?;
            return self.exec_value_flow(op_id, flow);
        }
        if let Some(gamma) = instance.clone().as_interface::<dyn Gamma>() {
            let flow = self.exec_gamma(&instance, gamma.as_ref())?;
            return self.exec_value_flow(op_id, flow);
        }
        if instance.is_operation(&crate::cfg::BR)
            || instance.is_operation(&crate::cfg::COND_BR)
            || instance.is_operation(&crate::cfg::SWITCH)
        {
            return Ok(Some(self.exec_branch(&instance)?));
        }
        let values = self.eval_leaf(op_id)?;
        self.bind_results(op_id, values);
        Ok(None)
    }

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
        let (dest, arg_ids) = if instance.is_operation(&crate::cfg::COND_BR) {
            let condition = self.decided_by(instance.operands()[0])?;
            let taken = condition.to_i64().unwrap_or_default() != 0;
            edges[if taken { 0 } else { 1 }].clone()
        } else if let Some(switch) = instance.clone().as_interface::<dyn crate::CaseGuard>() {
            let selector = match self.decided_by(switch.selector())? {
                Value::Int(bits) => bits.to_u64(),
                _ => 0,
            };
            let cases = switch.cases();
            let edge = cases
                .iter()
                .position(|&case| case == selector)
                .unwrap_or(cases.len());
            edges[edge].clone()
        } else {
            edges[0].clone()
        };
        // A dependency names no value: the edge carries it, nothing binds it.
        let args = arg_ids
            .iter()
            .filter(|&&id| !self.context.is_state_type(self.context.get_value(id).ty()))
            .map(|&id| self.value_of(id))
            .collect::<Result<Vec<_>>>()?;
        Ok(Flow::Goto(dest, args))
    }

    fn counter_bound(&mut self, value: ValueId) -> Result<APInt> {
        match self.value_of(value)? {
            Value::Int(bound) => Ok(bound.with_signed(true)),
            _ => Err(InterpError::Message(
                "scf.for bounds must be integers".into(),
            )),
        }
    }

    fn exec_ordered_for(&mut self, op_id: OpId) -> Result<Flow> {
        let op = self.context.get_op(op_id);
        let lower = self.counter_bound(crate::scf::ordered_for_countedloop_lower_bound(&op))?;
        let upper = self.counter_bound(crate::scf::ordered_for_countedloop_upper_bound(&op))?;
        let step = self.counter_bound(crate::scf::ordered_for_countedloop_step(&op))?;

        let instance = op.handle();
        let body_region = instance.regions()[0];
        let ports: Vec<ValueId> = self
            .context
            .get_region(body_region)
            .value_arguments()
            .iter()
            .map(crate::Value::id)
            .collect();
        let mut carried: Vec<Value> = self
            .context
            .values_among(&crate::scf::ordered_for_inits(&op))
            .iter()
            .map(|&init| self.value_of(init))
            .collect::<Result<_>>()?;

        let mut counter = lower;
        while counter.slt(&upper) {
            self.env.insert(ports[0], Value::Int(counter.clone()));
            for (&port, value) in ports[1..].iter().zip(&carried) {
                self.env.insert(port, value.clone());
            }
            match self.exec_region(body_region)? {
                Flow::Values(values) => carried = values,
                flow => return Ok(flow),
            }
            counter = counter.add(&step).with_signed(true);
        }
        let mut results = vec![Value::Int(counter)];
        results.extend(carried);
        Ok(Flow::Values(results))
    }

    fn exec_theta(&mut self, instance: &crate::OpHandle, theta: &dyn Theta) -> Result<Flow> {
        let body = theta.body();
        let sides = crate::binding::carried(self.context, instance);
        let ports: Vec<ValueId> = sides
            .iter()
            .map(|side| side.port)
            .filter(|&port| {
                !self
                    .context
                    .is_state_type(self.context.get_value(port).ty())
            })
            .collect();
        let predicate = theta.predicate();
        let continue_cone: Vec<ValueId> = sides.iter().map(|side| side.next).collect();
        let exit_cone: Vec<ValueId> = sides.iter().map(|side| side.exit).collect();
        let inits: Vec<ValueId> = sides.iter().map(|side| side.init).collect();

        let mut carried: Vec<Value> = self
            .context
            .values_among(&inits)
            .iter()
            .map(|&init| self.value_of(init))
            .collect::<Result<_>>()?;
        loop {
            for (port, value) in ports.iter().zip(&carried) {
                self.env.insert(*port, value.clone());
            }
            let mut evaluated = std::collections::HashSet::new();
            self.demand(predicate, body, &mut evaluated)?;
            let again = self.decided_by(predicate)?.to_i64().unwrap_or_default() != 0;
            let cone = if again { &continue_cone } else { &exit_cone };
            for &value in cone {
                self.demand(value, body, &mut evaluated)?;
            }
            let values = self
                .context
                .values_among(cone)
                .iter()
                .map(|&value| self.value_of(value))
                .collect::<Result<Vec<_>>>()?;
            if !again {
                return Ok(Flow::Values(values));
            }
            carried = values;
        }
    }

    fn exec_gamma(&mut self, instance: &crate::OpHandle, gamma: &dyn Gamma) -> Result<Flow> {
        let arms = gamma.arms();
        let binding = gamma.binding();
        // A case is bits, not a number: an `i1` predicate that is true is
        // case 1, whatever a signed reading of its one bit says.
        let chosen = match self.decided_by(gamma.predicate())? {
            Value::Int(bits) => bits.to_u64(),
            _ => 0,
        };
        let arm = arms[gamma.arm_for(chosen).min(arms.len() - 1)];
        let ports = self.context.get_region(arm).ports();
        let inputs = instance.operands();
        for (port, &input) in ports[binding.ports.clone()]
            .iter()
            .zip(&inputs[binding.operands.clone()])
        {
            if self.context.is_state_type(port.ty()) {
                continue;
            }
            let value = self.value_of(input)?;
            self.env.insert(port.id(), value);
        }
        self.exec_nodes_region(arm)
    }

    fn exec_call(&mut self, op_id: OpId) -> Result<Flow> {
        let op = self.context.get_op(op_id);
        let Value::Function(definition) = self.value_of(crate::func::call_callee(&op))? else {
            return Err(InterpError::Unsupported(
                "a call whose callee is not a definition".into(),
            ));
        };
        let arguments = crate::func::call_args(&op)
            .iter()
            .map(|&arg| self.value_of(arg))
            .collect::<Result<_>>()?;
        Ok(Flow::Values(self.call_function(definition, arguments)?))
    }

    fn exec_round(&mut self, instance: &crate::OpHandle) -> Result<Flow> {
        let op = instance.clone();
        if !crate::fp::round_contract(&op).reference_is_admissible() {
            return Err(InterpError::Unsupported(
                "fp.round reference does not satisfy its accuracy requirement".into(),
            ));
        }
        let reference = crate::fp::round_reference_region(&op);
        for (port, capture) in reference.ports().iter().zip(instance.operands()) {
            if self.context.is_state_type(port.ty()) {
                continue;
            }
            self.env.insert(port.id(), self.value_of(capture)?);
        }
        self.exec_nodes_region(reference.id())
    }

    fn call_function(&mut self, function: OpId, arguments: Vec<Value>) -> Result<Vec<Value>> {
        let func = self.context.get_op(function);
        let body_region = function_region(self.context, function, 0);
        let params = region_arguments(self.context, body_region);
        if params.len() != arguments.len() {
            return Err(InterpError::Message(format!(
                "@{} takes {} arguments, got {}",
                crate::func::func_symbol_symbol_name(&func),
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
        let nodes = self.context.get_region(body_region).is_nodes();
        let outcome = self.exec_region(body_region);
        let result = match outcome? {
            Flow::Return(values) => Ok(values),
            // An unordered body has no return: the values it names are what the
            // call produces.
            Flow::Values(values) if nodes => Ok(values),
            other => Err(InterpError::Message(format!(
                "@{} ended without func.return ({other:?})",
                crate::func::func_symbol_symbol_name(&func)
            ))),
        };
        for (param, value) in saved {
            self.env.insert(param, value);
        }
        result
    }

    fn operand_values(&self, instance: &crate::OpHandle) -> Result<Vec<Value>> {
        instance
            .value_operands()
            .iter()
            .map(|&operand| self.value_of(operand))
            .collect()
    }

    // A branch on poison is undefined, as in Alive2.
    fn decided_by(&self, value: ValueId) -> Result<Value> {
        match self.value_of(value)? {
            Value::Poison => Err(InterpError::Undefined(format!(
                "control flow depends on poison %{}",
                value.number()
            ))),
            decided => Ok(decided),
        }
    }

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
            .is_operation(&crate::func::FUNC)
            .then_some(Value::Function(definition))
    }

    fn eval_leaf(&mut self, op_id: OpId) -> Result<Vec<Value>> {
        let instance = self.context.get_op(op_id);
        let operands = self.operand_values(&instance)?;
        let spelled = || format!("{}.{}", instance.dialect(), instance.name());
        let poisoned = operands.iter().any(|value| matches!(value, Value::Poison));
        if let Some(holds) = self.condition(&instance, &operands, |op, g| op.requires_expr(g))?
            && (poisoned || !holds)
        {
            return Err(InterpError::Undefined(format!(
                "{} runs where its requires fails",
                spelled()
            )));
        }
        // A freeze reads poison, and so does a store of it: its address may not be.
        let reads_poison = instance.is_operation(&crate::builtin::FREEZE)
            || (instance.is_operation(&crate::ptr::STORE)
                && !matches!(operands.get(1), Some(Value::Poison)));
        if poisoned && !reads_poison {
            let accesses = instance.has_interface::<dyn crate::MemoryRead>()
                || instance.has_interface::<dyn crate::MemoryWrite>()
                || instance.has_interface::<dyn crate::Intrinsic>();
            if accesses {
                return Err(InterpError::Undefined(format!(
                    "{} accesses memory through poison",
                    spelled()
                )));
            }
            return Ok(vec![Value::Poison; instance.value_results().len()]);
        }
        let values = match instance.clone().as_interface::<dyn Interp>() {
            Some(interp) => interp.evaluate(&operands, self.state)?,
            None => self.eval_semantic(&instance, &operands)?,
        };
        if self.condition(&instance, &operands, |op, g| op.poison_expr(g))? == Some(true) {
            return Ok(vec![Value::Poison; values.len()]);
        }
        Ok(values)
    }

    fn condition(
        &self,
        instance: &crate::OpHandle,
        operands: &[Value],
        condition: fn(&crate::OpHandle, &mut sem::SemGraph) -> Option<crate::NodeId>,
    ) -> Result<Option<bool>> {
        let mut graph = sem::SemGraph::new();
        if condition(instance, &mut graph).is_none() {
            return Ok(None);
        }
        if operands.iter().any(|value| matches!(value, Value::Poison)) {
            return Ok(Some(true));
        }
        let pointer_width = pointer_width(self.context, instance);
        let symbols: Vec<sem::Value> = operands
            .iter()
            .map(|value| to_sem_value(value, pointer_width))
            .collect::<Result<_>>()?;
        match sem::execute(&graph, &symbols) {
            sem::Value::Int(bit) => Ok(Some(bit.to_u64() != 0)),
            other => Err(InterpError::Message(format!(
                "a condition evaluated to {other:?}"
            ))),
        }
    }

    fn eval_semantic(&self, instance: &crate::OpHandle, operands: &[Value]) -> Result<Vec<Value>> {
        let spelled = format!("{}.{}", instance.dialect(), instance.name());
        let mut graph = sem::SemGraph::new();
        if instance.semantic_expr(&mut graph).is_none() {
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

fn to_sem_value(value: &Value, pointer_width: u32) -> Result<sem::Value> {
    Ok(match value {
        Value::Int(int) => sem::Value::Int(int.clone()),
        Value::Float(float) => sem::Value::Float(float.clone()),
        Value::Ptr(address) => sem::Value::Int(APInt::new(u64::from(pointer_width), *address)),
        Value::Tuple(_)
        | Value::Function(_)
        | Value::Rounding(_)
        | Value::FpEnvironment(_)
        | Value::Type(_)
        | Value::Layout(_)
        | Value::Unit
        | Value::Poison => {
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
    if PtrType::view(context, ty).is_some() {
        let sem::Value::Int(int) = value else {
            return Err(InterpError::Message(
                "pointer result must be integral".into(),
            ));
        };
        return Ok(Value::Ptr(int.to_u64()));
    }
    if let Some(float) = FloatType::view(context, ty) {
        let float_value = match value {
            sem::Value::Float(float_value) => {
                float_value.convert(float.exp_width(), float.mant_width(), false)
            }
            sem::Value::RawBits(bits) => APFloat::from_bits(
                float.exp_width(),
                float.mant_width(),
                false,
                bits.to_apint().to_u64() as u128,
            ),
            _ => {
                return Err(InterpError::Message(
                    "float result must be floating or raw bits".into(),
                ));
            }
        };
        return Ok(Value::Float(float_value));
    }
    if UnitType::view(context, ty).is_some() {
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

fn widen_int_to_type(
    context: &Context,
    int: APInt,
    ty: crate::TypeId,
    pointer_width: u32,
) -> Value {
    let width = if PtrType::view(context, ty).is_some() {
        u64::from(pointer_width)
    } else if let Some(integer) = IntegerType::view(context, ty) {
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

pub(crate) fn constant_interp_evaluate(
    operation: &tir::OpHandle,
    _operands: &[Value],
    _state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    Ok(vec![Value::Int(
        crate::builtin::constant_constantlike_constant_value(operation),
    )])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_ConstantOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::constant_interp_evaluate,
    };

pub(crate) fn make_tuple_interp_evaluate(
    ______operation: &tir::OpHandle,
    operands: &[Value],
    _state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    Ok(vec![Value::Tuple(operands.to_vec())])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_MakeTupleOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::make_tuple_interp_evaluate,
    };

pub(crate) fn tuple_get_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[Value],
    _state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let Value::Tuple(elements) = &operands[0] else {
        return Err(InterpError::Message(
            "tuple_get operand must be a tuple".into(),
        ));
    };
    let element = elements
        .get(crate::builtin::tuple_get_index(operation) as usize)
        .ok_or_else(|| {
            InterpError::Message(format!(
                "tuple_get index {} out of bounds",
                crate::builtin::tuple_get_index(operation)
            ))
        })?;
    Ok(vec![element.clone()])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_TupleGetOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::tuple_get_interp_evaluate,
    };

pub(crate) fn entry_state_interp_evaluate(
    ______operation: &tir::OpHandle,
    _operands: &[Value],
    _state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_EntryStateOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::entry_state_interp_evaluate,
    };

pub(crate) fn join_interp_evaluate(
    ______operation: &tir::OpHandle,
    _operands: &[Value],
    _state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_JoinOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::join_interp_evaluate,
    };

pub(crate) fn split_interp_evaluate(
    ______operation: &tir::OpHandle,
    _operands: &[Value],
    _state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_SplitOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::split_interp_evaluate,
    };

pub(crate) fn alloca_interp_evaluate(
    operation: &tir::OpHandle,
    _operands: &[Value],
    state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let address = state.memory.alloc(
        crate::ptr::alloca_size(operation),
        crate::ptr::alloca_align(operation),
    );
    Ok(vec![Value::Ptr(address)])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_AllocaOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::alloca_interp_evaluate,
    };

pub(crate) fn load_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[Value],
    state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let context = operation.handle().context.clone();
    let Value::Ptr(address) = &operands[0] else {
        return Err(InterpError::Message(
            "ptr.load operand must be a pointer".into(),
        ));
    };
    let ty = context.get_value(crate::ptr::load_result(operation)).ty();
    let value = read_typed(&context, &mut state.memory, ty, *address)?;
    Ok(vec![value])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_LoadOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::load_interp_evaluate,
    };

pub(crate) fn store_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[Value],
    state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let context = operation.handle().context.clone();
    let Value::Ptr(address) = &operands[1] else {
        return Err(InterpError::Message(
            "ptr.store destination must be a pointer".into(),
        ));
    };
    let ty = context.get_value(operation.operands()[0]).ty();
    write_typed(&context, &mut state.memory, ty, *address, &operands[0])?;
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_StoreOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::store_interp_evaluate,
    };

pub(crate) fn memcpy_interp_evaluate(
    ______operation: &tir::OpHandle,
    operands: &[Value],
    state: &mut ExecutionState,
) -> Result<Vec<Value>> {
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
    state.memory.copy(*destination, *source, size)?;
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_MemcpyOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::memcpy_interp_evaluate,
    };

pub(crate) fn field_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[Value],
    _: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let Value::Ptr(base) = &operands[0] else {
        return Err(InterpError::Message(
            "ptr.field base must be a pointer".into(),
        ));
    };
    let offset = DataLayout::for_op(&operation.handle().context, operation.id())
        .and_then(|layout| {
            layout.offset_in_bits(
                crate::ptr::field_ty(operation),
                &crate::ptr::field_field(operation),
            )
        })
        .ok_or_else(|| {
            InterpError::Message(format!(
                "no offset for field '{}'",
                crate::ptr::field_field(operation)
            ))
        })?;
    Ok(vec![Value::Ptr(base.wrapping_add(offset / 8))])
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_FieldOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::field_interp_evaluate,
    };

pub(crate) fn copy_interp_evaluate(
    operation: &tir::OpHandle,
    operands: &[Value],
    state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let (Value::Ptr(destination), Value::Ptr(source)) = (&operands[0], &operands[1]) else {
        return Err(InterpError::Message(
            "ptr.copy operands must be pointers".into(),
        ));
    };
    let bits = DataLayout::for_op(&operation.handle().context, operation.id())
        .and_then(|layout| layout.size_in_bits(crate::ptr::copy_ty(operation)))
        .ok_or_else(|| InterpError::Message("ptr.copy of a type with no layout".into()))?;
    state
        .memory
        .copy(*destination, *source, u64::from(bits) / 8)?;
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_CopyOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::copy_interp_evaluate,
    };

pub(crate) fn memset_interp_evaluate(
    ______operation: &tir::OpHandle,
    operands: &[Value],
    state: &mut ExecutionState,
) -> Result<Vec<Value>> {
    let Value::Ptr(destination) = &operands[0] else {
        return Err(InterpError::Message(
            "memset destination must be a pointer".into(),
        ));
    };
    let fill = operands[1].to_i64().unwrap_or_default() as u8;
    let size = operands[2].to_i64().unwrap_or_default() as u64;
    state
        .memory
        .write(*destination, &vec![fill; size as usize])?;
    Ok(Vec::new())
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_MemsetOp_Interp: crate::interp::InterpCallbacks =
    crate::interp::InterpCallbacks {
        evaluate: crate::interp::memset_interp_evaluate,
    };

fn read_typed(
    context: &Context,
    memory: &mut Memory,
    ty: crate::TypeId,
    address: u64,
) -> Result<Value> {
    if let Some(width) = crate::ptr::semantics::ordinary_width(context, ty) {
        let loaded = crate::ptr::semantics::load(&mut ScalarMemory(memory), (address, 64), width)?;
        let (value, _) = loaded.value;
        let (poison, _) = loaded.poison;
        return Ok(if poison != 0 {
            Value::Poison
        } else if PtrType::view(context, ty).is_some() {
            Value::Ptr(value)
        } else {
            Value::Int(APInt::new(u64::from(width), value))
        });
    }
    let bits = IntegerType::view(context, ty)
        .map(|integer| integer.width())
        .or_else(|| FloatType::view(context, ty).map(|float| u64::from(float.bit_width())));
    if let Some(bits) = bits
        && !memory.is_written(address, bits / 8)?
    {
        return Ok(Value::Poison);
    }
    if let Some(integer) = IntegerType::view(context, ty) {
        return memory.read_int(address, integer.width()).map(Value::Int);
    }
    if let Some(float) = FloatType::view(context, ty) {
        let bits = memory.read_int(address, u64::from(float.bit_width()))?;
        return Ok(Value::Float(APFloat::from_bits(
            float.exp_width(),
            float.mant_width(),
            false,
            bits.to_u64() as u128,
        )));
    }
    Err(InterpError::Message(
        "loads of this type are not interpretable yet".into(),
    ))
}

fn write_typed(
    context: &Context,
    memory: &mut Memory,
    ty: crate::TypeId,
    address: u64,
    value: &Value,
) -> Result<()> {
    if let Some(width) = crate::ptr::semantics::ordinary_width(context, ty) {
        let (bits, poison) = match value {
            Value::Poison => (0, 1),
            Value::Int(v)
                if IntegerType::view(context, ty).is_some() && v.width() == u64::from(width) =>
            {
                (v.to_u64(), 0)
            }
            Value::Ptr(v) if PtrType::view(context, ty).is_some() => (*v, 0),
            _ => {
                return Err(InterpError::Message(
                    "store value must match the pointee".into(),
                ));
            }
        };
        crate::ptr::semantics::store(
            &mut ScalarMemory(memory),
            (address, 64),
            (bits, width),
            (poison, 1),
            width,
        )?;
        return Ok(());
    }
    if matches!(value, Value::Poison) {
        let bits = IntegerType::view(context, ty)
            .map(|integer| integer.width())
            .or_else(|| FloatType::view(context, ty).map(|float| u64::from(float.bit_width())))
            .ok_or_else(|| InterpError::Message("a store of poison of no size".into()))?;
        return memory.forget(address, bits / 8);
    }
    let mismatch = || InterpError::Message("store value must match the pointee".into());
    if IntegerType::view(context, ty).is_some() {
        let Value::Int(int) = value else {
            return Err(mismatch());
        };
        return memory.write_int(address, int);
    }
    if let Some(float) = FloatType::view(context, ty) {
        let Value::Float(float_value) = value else {
            return Err(mismatch());
        };
        let bits = APInt::new(u64::from(float.bit_width()), float_value.to_bits() as u64);
        return memory.write_int(address, &bits);
    }
    Err(InterpError::Message(
        "stores of this type are not interpretable yet".into(),
    ))
}
