use super::protocol::{
    EffectRequest, EffectResponse, FrameYield, MemoryEffect, RequestId, ResponseValue,
};
use super::{Dest, Effect, ExecEnv, Program, init_syms, register_phys};
use crate::backend::{InstrInfo, MachineContext, MachineInstruction, RegisterValue, SimTrap};
use crate::sem::{Continuation, ExtendSemBytes, Memory, SemGraph, Value};
use crate::utils::RawBits;

struct Expression {
    graph: SemGraph,
    continuation: Continuation,
}

impl Expression {
    fn new(env: &ExecEnv, offset: u32) -> Self {
        let mut graph = SemGraph::new();
        graph.extend_sem_bytes(env.kinds, env.blob, offset);
        let continuation = Continuation::new(&graph);
        Self {
            graph,
            continuation,
        }
    }
}

enum Writeback {
    Pc(u64),
    Register(&'static str, u16, RegisterValue),
}

/// An instruction's semantic continuation and provisional architectural writes.
/// The caller services yielded requests once and commits only after completion.
pub struct SemanticFrame {
    instance: crate::OpHandle,
    mnemonic: &'static str,
    env: &'static ExecEnv,
    effects: &'static [Effect],
    symbols: Vec<Value>,
    statements: Vec<(&'static [Effect], usize)>,
    expression: Option<Expression>,
    writes: Vec<Writeback>,
    memory: YieldMemory,
    complete: bool,
    failed: bool,
}

impl SemanticFrame {
    /// Capture instruction-entry operands. `instruction` must be unique within
    /// the memory service's lifetime so a stale response cannot match a new frame.
    pub fn new(
        instruction: u64,
        pc: u64,
        machine_instruction: &dyn MachineInstruction,
        machine: &mut dyn MachineContext,
    ) -> Result<Self, SimTrap> {
        Self::from_program(
            instruction,
            pc,
            machine_instruction.instance(),
            machine_instruction.info(),
            machine,
        )
    }

    pub(super) fn from_program(
        instruction: u64,
        pc: u64,
        instance: &crate::OpHandle,
        info: &InstrInfo,
        machine: &mut dyn MachineContext,
    ) -> Result<Self, SimTrap> {
        let Program::Effects {
            env,
            sym_count,
            sources,
            effects,
        } = &info.program
        else {
            return Err(SimTrap::InvalidInstruction {
                op: info.mnemonic,
                reason: match &info.program {
                    Program::Unsupported(reason) => (*reason).into(),
                    Program::Effects { .. } => unreachable!(),
                },
            });
        };
        let instance = instance.clone();
        let symbols = init_syms(&instance, machine, info.mnemonic, *sym_count, sources)?;
        Ok(Self {
            instance,
            mnemonic: info.mnemonic,
            env,
            effects,
            symbols,
            statements: vec![(effects, 0)],
            expression: None,
            writes: Vec::new(),
            memory: YieldMemory {
                instruction,
                pc,
                sequence: 0,
                pending: None,
                response: None,
            },
            complete: false,
            failed: false,
        })
    }

    /// Sequence of the pending effect, or the next effect after completion.
    pub fn sequence(&self) -> u64 {
        self.memory.sequence
    }

    /// Resolve every access of a multi-access instruction before it starts.
    /// The runtime must reject inaccessible or device ranges before any effect.
    /// An empty result needs no additional multi-access preflight.
    pub fn preflight(&self) -> Result<Vec<super::MemoryRange>, SimTrap> {
        super::footprint::preflight(self.env, self.effects, &self.symbols).map_err(|reason| {
            SimTrap::InvalidInstruction {
                op: self.mnemonic,
                reason: reason.into(),
            }
        })
    }

    /// Continue from the saved expression position. With no response, an
    /// outstanding request is returned unchanged, allowing backpressure.
    pub fn resume(&mut self, response: Option<EffectResponse>) -> Result<FrameYield, SimTrap> {
        if self.failed || self.complete {
            return Err(protocol_error("instruction frame is no longer resumable"));
        }
        if let Some(response) = response {
            self.memory.respond(response)?;
        } else if let Some(request) = &self.memory.pending {
            return Ok(FrameYield::Effect(request.clone()));
        }
        match self.advance() {
            Ok(()) => {
                self.complete = true;
                Ok(FrameYield::Complete)
            }
            Err(EvalStop::Pending) => Ok(FrameYield::Effect(
                self.memory.pending.as_ref().unwrap().clone(),
            )),
            Err(EvalStop::Fault(trap)) => {
                self.failed = true;
                Err(trap)
            }
        }
    }

    /// Publish register and PC writes after the caller commits staged stores.
    /// Consuming the frame prevents a second commit.
    pub fn commit(self, machine: &mut dyn MachineContext) -> Result<(), SimTrap> {
        if !self.complete || self.failed {
            return Err(protocol_error("cannot commit an incomplete instruction"));
        }
        for write in self.writes {
            match write {
                Writeback::Pc(pc) => machine.write_pc(pc),
                Writeback::Register(class, index, value) => {
                    machine.write_register_value(class, index, value)?
                }
            }
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<(), EvalStop> {
        while let Some(&(effects, index)) = self.statements.last() {
            let Some(effect) = effects.get(index) else {
                self.statements.pop();
                continue;
            };
            let offset = match effect {
                Effect::Assign { offset, .. }
                | Effect::Bind { offset, .. }
                | Effect::Trap { offset } => *offset,
                Effect::If { cond, .. } => *cond,
            };
            let expression = self
                .expression
                .get_or_insert_with(|| Expression::new(self.env, offset));
            let value = expression.continuation.resume(
                &expression.graph,
                &self.symbols,
                &mut self.memory,
            )?;
            match effect {
                Effect::Assign { dest, .. } => self.stage(dest, value).map_err(EvalStop::Fault)?,
                Effect::Bind { sym, .. } => self.symbols[*sym] = value,
                Effect::Trap { .. } => {
                    let cause = register_value(value, self.mnemonic)
                        .map_err(EvalStop::Fault)?
                        .to_u64();
                    self.memory.access(MemoryEffect::Exception { cause })?;
                }
                Effect::If { then, els, .. } => {
                    let taken = register_value(value, self.mnemonic)
                        .map_err(EvalStop::Fault)?
                        .to_u64()
                        != 0;
                    self.statements.last_mut().unwrap().1 += 1;
                    self.statements.push((if taken { then } else { els }, 0));
                    self.expression = None;
                    continue;
                }
            }
            self.expression = None;
            self.statements.last_mut().unwrap().1 += 1;
        }
        Ok(())
    }

    fn stage(&mut self, dest: &Dest, value: Value) -> Result<(), SimTrap> {
        let value = register_value(value, self.mnemonic)?;
        let register = match dest {
            Dest::Pc => {
                self.writes.push(Writeback::Pc(value.to_u64()));
                return Ok(());
            }
            Dest::Discard => return Ok(()),
            Dest::Reg(name) => {
                let (class, index) = register_phys(&self.instance, self.mnemonic, name)?;
                (class.name(), index)
            }
            Dest::Fixed(class, index) => (*class, *index),
        };
        if !(self.env.is_hardwired_zero)(register.0, register.1) {
            self.writes
                .push(Writeback::Register(register.0, register.1, value));
        }
        Ok(())
    }
}

fn register_value(value: Value, mnemonic: &'static str) -> Result<RegisterValue, SimTrap> {
    match value {
        Value::Int(value) => Ok(RegisterValue::Int(value)),
        Value::Float(value) => Ok(RegisterValue::Bits(RawBits::from_apfloat(&value))),
        Value::RawBits(value) => Ok(RegisterValue::Bits(value)),
        Value::Iterator(_) => Err(SimTrap::InvalidInstruction {
            op: mnemonic,
            reason: "semantic expression is not a register value".into(),
        }),
    }
}

fn protocol_error(reason: &str) -> SimTrap {
    SimTrap::InvalidInstruction {
        op: "<effect-protocol>",
        reason: reason.into(),
    }
}

enum EvalStop {
    Pending,
    Fault(SimTrap),
}

struct YieldMemory {
    instruction: u64,
    pc: u64,
    sequence: u64,
    pending: Option<EffectRequest>,
    response: Option<Result<ResponseValue, SimTrap>>,
}

impl YieldMemory {
    fn respond(&mut self, response: EffectResponse) -> Result<(), SimTrap> {
        let request = self
            .pending
            .as_ref()
            .ok_or_else(|| protocol_error("unsolicited effect response"))?;
        if request.id != response.id || self.response.is_some() {
            return Err(protocol_error("stale or duplicate effect response"));
        }
        if let Ok(value) = &response.result
            && !request.effect.accepts(value)
        {
            return Err(protocol_error(
                "effect response has the wrong value type or size",
            ));
        }
        self.response = Some(response.result);
        Ok(())
    }

    fn access(&mut self, effect: MemoryEffect) -> Result<ResponseValue, EvalStop> {
        if let Some(request) = &self.pending {
            if request.effect != effect {
                return Err(EvalStop::Fault(protocol_error(
                    "resumed effect differs from pending request",
                )));
            }
            let Some(response) = self.response.take() else {
                return Err(EvalStop::Pending);
            };
            let value = response.map_err(EvalStop::Fault)?;
            self.pending = None;
            self.sequence += 1;
            return Ok(value);
        }
        self.pending = Some(EffectRequest {
            id: RequestId {
                instruction: self.instruction,
                sequence: self.sequence,
            },
            pc: self.pc,
            effect,
        });
        Err(EvalStop::Pending)
    }

    fn word(&mut self, effect: MemoryEffect) -> Result<u64, EvalStop> {
        match self.access(effect)? {
            ResponseValue::Word(value) => Ok(value),
            _ => unreachable!("response validated when received"),
        }
    }
}

impl Memory for YieldMemory {
    type Error = EvalStop;

    fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, EvalStop> {
        self.word(MemoryEffect::Read { address, size })
    }

    fn write_memory(&mut self, address: u64, size: usize, value: u64) -> Result<(), EvalStop> {
        self.access(MemoryEffect::Write {
            address,
            bytes: value.to_le_bytes()[..size].to_vec(),
        })?;
        Ok(())
    }

    fn read_memory_bytes(&mut self, address: u64, size: usize) -> Result<RawBits, EvalStop> {
        match self.access(MemoryEffect::Read { address, size })? {
            ResponseValue::Bytes(value) => Ok(value),
            _ => unreachable!("response validated when received"),
        }
    }

    fn write_memory_bytes(
        &mut self,
        address: u64,
        size: usize,
        value: RawBits,
    ) -> Result<(), EvalStop> {
        let mut bytes = value.bytes().to_vec();
        bytes.resize(size, 0);
        self.access(MemoryEffect::Write { address, bytes })?;
        Ok(())
    }

    fn load_reserved(
        &mut self,
        address: u64,
        size: usize,
        ordering: crate::sem::MemOrdering,
    ) -> Result<u64, EvalStop> {
        self.word(MemoryEffect::LoadReserved {
            address,
            size,
            ordering,
        })
    }

    fn store_conditional(
        &mut self,
        address: u64,
        size: usize,
        value: u64,
        ordering: crate::sem::MemOrdering,
    ) -> Result<bool, EvalStop> {
        self.word(MemoryEffect::StoreConditional {
            address,
            size,
            value,
            ordering,
        })
        .map(|value| value != 0)
    }

    fn atomic_rmw(
        &mut self,
        op: crate::sem::AtomicRmwOp,
        address: u64,
        size: usize,
        value: u64,
        ordering: crate::sem::MemOrdering,
    ) -> Result<u64, EvalStop> {
        self.word(MemoryEffect::AtomicRmw {
            op,
            address,
            size,
            value,
            ordering,
        })
    }

    fn fence(&mut self, pred: u32, succ: u32, kind: u32) -> Result<(), EvalStop> {
        self.access(MemoryEffect::Fence { pred, succ, kind })?;
        Ok(())
    }
}
