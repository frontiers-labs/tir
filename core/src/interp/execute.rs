use super::*;

/// Runtime failures. Observing poison includes returning it to the host,
/// branching on it, requiring it, or issuing a load with a poisoned address.
#[derive(Debug, PartialEq, Eq)]
pub enum Fault {
    InvalidEntry,
    InvalidArguments,
    InvalidLoadWidth,
    ObservedPoison,
    FailedPrecondition,
    Trap,
    FuelExhausted,
}

/// Execution either returns a concrete value or yields one load to the host.
pub enum Outcome<'p> {
    Returned(APInt),
    Load(Load<'p>),
}

/// An outstanding load. Resuming consumes the suspended machine.
pub struct Load<'p> {
    address: APInt,
    dest: Reg,
    machine: Machine<'p>,
}

impl<'p> Load<'p> {
    pub fn address(&self) -> &APInt {
        &self.address
    }

    pub fn width(&self) -> u32 {
        self.machine.frame.function.widths[self.dest.0 as usize]
    }

    /// Supply the loaded bits and continue with the remaining fuel.
    pub fn resume(mut self, value: APInt) -> Result<Outcome<'p>, Fault> {
        if value.width() != u64::from(self.width()) {
            return Err(Fault::InvalidLoadWidth);
        }
        self.machine.frame.put(self.dest, Some(value));
        self.machine.run()
    }
}

// None denotes poison. Verification prevents reads of uninitialized slots.
type Value = Option<APInt>;

struct Frame<'p> {
    function: &'p Function,
    slots: Vec<Value>,
    block: usize,
    pc: usize,
}

impl<'p> Frame<'p> {
    fn new(function: &'p Function, args: Vec<Value>) -> Self {
        let mut frame = Self {
            function,
            slots: vec![None; function.widths.len()],
            block: 0,
            pc: 0,
        };
        frame.enter(0, args);
        frame
    }

    fn put(&mut self, reg: Reg, value: Value) {
        self.slots[reg.0 as usize] = value;
    }

    fn observe(&self, reg: Reg) -> Result<&APInt, Fault> {
        self.slots[reg.0 as usize]
            .as_ref()
            .ok_or(Fault::ObservedPoison)
    }

    fn arguments(&self, args: Args) -> Vec<Value> {
        self.function.args[args.0 as usize]
            .iter()
            .map(|r| self.slots[r.0 as usize].clone())
            .collect()
    }

    fn enter(&mut self, block: usize, args: Vec<Value>) {
        for (&dst, value) in self.function.blocks[block].params.iter().zip(args) {
            self.put(dst, value);
        }
        self.block = block;
        self.pc = 0;
    }

    fn jump(&mut self, id: EdgeId) {
        let edge = &self.function.edges[id.0 as usize];
        self.enter(edge.block as usize, self.arguments(edge.args));
    }

    fn binary(&mut self, d: Reg, a: Reg, b: Reg, op: impl FnOnce(&APInt, &APInt) -> APInt) {
        let value = self.slots[a.0 as usize]
            .as_ref()
            .zip(self.slots[b.0 as usize].as_ref())
            .map(|(a, b)| op(a, b));
        self.put(d, value);
    }
}

struct Machine<'p> {
    program: &'p Program,
    frame: Frame<'p>,
    callers: Vec<(Frame<'p>, Reg)>,
    fuel: u64,
}

impl Program {
    /// Run a function with concrete arguments. Every instruction costs one fuel,
    /// including the instruction that issues a load.
    pub fn run(&self, entry: Func, args: Vec<APInt>, fuel: u64) -> Result<Outcome<'_>, Fault> {
        let f = self
            .0
            .functions
            .get(entry.0 as usize)
            .ok_or(Fault::InvalidEntry)?;
        let params = &f.blocks[0].params;
        if params.len() != args.len()
            || params
                .iter()
                .zip(&args)
                .any(|(r, v)| u64::from(f.widths[r.0 as usize]) != v.width())
        {
            return Err(Fault::InvalidArguments);
        }
        Machine {
            program: self,
            frame: Frame::new(f, args.into_iter().map(Some).collect()),
            callers: vec![],
            fuel,
        }
        .run()
    }
}

impl<'p> Machine<'p> {
    fn run(mut self) -> Result<Outcome<'p>, Fault> {
        use Inst::*;
        loop {
            self.fuel = self.fuel.checked_sub(1).ok_or(Fault::FuelExhausted)?;
            let f = &mut self.frame;
            let inst = f.function.blocks[f.block].code[f.pc];
            f.pc += 1;
            match inst {
                Constant(d, k) => f.put(d, Some(self.program.0.constants[k.0 as usize].clone())),
                PoisonIf(d, v, c) => {
                    let value = match &f.slots[c.0 as usize] {
                        Some(cond) if cond.is_zero() => f.slots[v.0 as usize].clone(),
                        _ => None,
                    };
                    f.put(d, value);
                }
                Add(d, a, b) => f.binary(d, a, b, APInt::add),
                Sub(d, a, b) => f.binary(d, a, b, APInt::sub),
                Eq(d, a, b) => f.binary(d, a, b, |a, b| APInt::new(1, a.ucmp(b).is_eq().into())),
                Ult(d, a, b) => f.binary(d, a, b, |a, b| APInt::new(1, a.ult(b).into())),
                Require(r) => {
                    if f.observe(r)?.is_zero() {
                        return Err(Fault::FailedPrecondition);
                    }
                }
                Trap => return Err(Fault::Trap),
                Jump(e) => f.jump(e),
                Branch(r, t, e) => f.jump(if f.observe(r)?.is_zero() { e } else { t }),
                Call(d, id, args) => {
                    let callee =
                        Frame::new(&self.program.0.functions[id.0 as usize], f.arguments(args));
                    self.callers.push((std::mem::replace(f, callee), d));
                }
                Return(r) => {
                    let value = f.slots[r.0 as usize].take();
                    let Some((mut caller, d)) = self.callers.pop() else {
                        return Ok(Outcome::Returned(value.ok_or(Fault::ObservedPoison)?));
                    };
                    caller.put(d, value);
                    self.frame = caller;
                }
                Load(dest, a) => {
                    let address = f.observe(a)?.clone();
                    return Ok(Outcome::Load(super::Load {
                        address,
                        dest,
                        machine: self,
                    }));
                }
            }
        }
    }
}
