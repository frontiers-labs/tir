use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context as _};
use crossbeam::queue::SegQueue;
use isla_lib::bitvector::b129::B129;
use isla_lib::bitvector::BV;
use isla_lib::config::ISAConfig;
use isla_lib::error::ExecError;
use isla_lib::executor::{self, LocalFrame, StopConditions, TaskId, TaskState};
use isla_lib::init::{initialize_architecture, Initialized};
use isla_lib::ir::{AssertionMode, Def, IRTypeInfo, Loc, Name, Symtab, Val};
use isla_lib::ir_lexer::new_ir_lexer;
use isla_lib::ir_parser;
use isla_lib::memory::Memory;
use isla_lib::simplify;
use isla_lib::smt::{self, Event, Solver};
use isla_lib::value_parser;
use isla_lib::zencode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

mod smt_format;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TraceValue {
    pub smt: String,
    pub symbolic: bool,
    pub fields: HashMap<String, TraceValue>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum TraceEvent {
    Declare {
        declaration: String,
    },
    Define {
        variable: String,
        expression: String,
    },
    Assume {
        expression: String,
    },
    ReadRegister {
        name: String,
        fields: Vec<String>,
        value: TraceValue,
    },
    WriteRegister {
        name: String,
        fields: Vec<String>,
        value: TraceValue,
    },
    ReadMemory {
        kind: TraceValue,
        address: TraceValue,
        value: TraceValue,
        bytes: u32,
    },
    WriteMemory {
        kind: TraceValue,
        address: TraceValue,
        value: TraceValue,
        bytes: u32,
    },
}

pub struct Verifier {
    architecture: Initialized<'static, B129>,
    function: Name,
    threads: usize,
    timeout_seconds: u64,
    simplify: bool,
}

impl Verifier {
    pub fn load(
        snapshot: &Path,
        config: &Path,
        initial_registers: &[String],
        function: &str,
        threads: usize,
        timeout_seconds: u64,
        simplify: bool,
    ) -> anyhow::Result<Self> {
        let source = std::fs::read_to_string(snapshot)
            .with_context(|| format!("reading Sail snapshot {}", snapshot.display()))?;
        let source: &'static str = Box::leak(source.into_boxed_str());
        let mut symtab = Symtab::new();
        let definitions = ir_parser::IrParser::new()
            .parse(&mut symtab, new_ir_lexer(source))
            .map_err(|error| anyhow!("Sail IR parse error: {error}"))?;
        let definitions: &'static mut [Def<Name, B129>] = Box::leak(definitions.into_boxed_slice());
        let type_info = IRTypeInfo::new(definitions);
        let mut hasher = Sha256::new();
        let mut isa_config = ISAConfig::from_file(&mut hasher, config, None, &symtab, &type_info)
            .map_err(|error| anyhow!("Isla config error: {error}"))?;
        for assignment in initial_registers {
            let (location, value) = value_parser::AssignParser::new()
                .parse(&symtab, &type_info, new_ir_lexer(assignment))
                .map_err(|_| anyhow!("invalid Isla initial register assignment {assignment}"))?;
            let Loc::Id(register) = location else {
                return Err(anyhow!(
                    "initial assignment must name a register: {assignment}"
                ));
            };
            let register = symtab
                .get(&register)
                .ok_or_else(|| anyhow!("unknown Isla register in {assignment}"))?;
            isa_config.default_registers.insert(register, value);
        }
        let architecture = initialize_architecture(
            definitions,
            symtab,
            type_info,
            &isa_config,
            AssertionMode::Optimistic,
            true,
        );
        let function = architecture
            .shared_state
            .symtab
            .get(&zencode::encode(function))
            .ok_or_else(|| anyhow!("unknown Isla footprint function {function}"))?;
        Ok(Self {
            architecture,
            function,
            threads: threads.max(1),
            timeout_seconds,
            simplify,
        })
    }

    pub fn execute(
        &self,
        words: &[u128],
        widths: &[u32],
    ) -> anyhow::Result<HashMap<u128, Vec<Vec<TraceEvent>>>> {
        anyhow::ensure!(words.len() == widths.len(), "word/width count mismatch");
        let shared = &self.architecture.shared_state;
        let (args, return_type, instructions) = shared
            .functions
            .get(&self.function)
            .ok_or_else(|| anyhow!("footprint function body is missing"))?;
        let mut tasks = Vec::with_capacity(words.len());
        let mut task_words = HashMap::new();
        let task_state = TaskState::new();
        let stop_conditions = StopConditions::default();
        for (&word, &width) in words.iter().zip(widths) {
            anyhow::ensure!(
                (1..=128).contains(&width),
                "instruction width must be between 1 and 128 bits"
            );
            anyhow::ensure!(
                width == 128 || word < (1_u128 << width),
                "instruction word does not fit its declared width"
            );
            let low_width = width.min(64);
            let mut opcode = B129::zeros(width).set_slice(0, B129::new(word as u64, low_width));
            if width > 64 {
                opcode = opcode.set_slice(64, B129::new((word >> 64) as u64, width - 64));
            }
            let opcode = Val::Bits(opcode);
            let checkpoint = initial_checkpoint(&self.architecture);
            let task_id = TaskId::fresh();
            task_words.insert(task_id, word);
            let mut task = LocalFrame::new(
                self.function,
                args,
                return_type,
                Some(&[opcode]),
                instructions,
            )
            .add_lets(&self.architecture.lets)
            .add_regs(&self.architecture.regs)
            .set_memory(Memory::new())
            .task_with_checkpoint(task_id, &task_state, checkpoint);
            task.set_stop_conditions(&stop_conditions);
            tasks.push(task);
        }
        let queue: Arc<BatchQueue> = Arc::new(SegQueue::new());
        executor::start_multi(
            self.threads,
            Some(self.timeout_seconds),
            tasks,
            shared,
            queue.clone(),
            &batch_collector,
        );
        let mut traces: HashMap<u128, Vec<Vec<TraceEvent>>> = HashMap::new();
        let mut failed = HashSet::new();
        while let Some(result) = queue.pop() {
            let (task_id, mut events) = match result {
                Ok(result) => result,
                Err((task_id, _error)) => {
                    failed.insert(task_words[&task_id]);
                    continue;
                }
            };
            if self.simplify {
                simplify::hide_initialization(&mut events);
                simplify::remove_unused(&mut events);
                simplify::propagate_forwards_used_once(&mut events);
                simplify::commute_extract(&mut events);
                simplify::eval(&mut events);
            }
            let word = task_words[&task_id];
            let normalized = events
                .into_iter()
                .rev()
                .filter_map(|event| normalize_event(event, shared).transpose())
                .collect::<anyhow::Result<Vec<_>>>()?;
            traces.entry(word).or_default().push(normalized);
        }
        for word in failed {
            traces.remove(&word);
        }
        Ok(traces)
    }
}

type BatchQueue = SegQueue<Result<(TaskId, Vec<Event<B129>>), (TaskId, String)>>;

fn batch_collector<'ir>(
    _thread_id: usize,
    task_id: TaskId,
    result: Result<(executor::Run<B129>, LocalFrame<'ir, B129>), (ExecError, executor::Backtrace)>,
    _shared: &isla_lib::ir::SharedState<'ir, B129>,
    solver: Solver<B129>,
    queue: &BatchQueue,
) {
    match result {
        Ok((executor::Run::Finished(_) | executor::Run::Exit, _)) => {
            let mut events = solver.trace().to_vec();
            queue.push(Ok((task_id, events.drain(..).cloned().collect())));
        }
        Ok((executor::Run::Dead, _)) => {
            queue.push(Err((task_id, "execution path is dead".to_string())));
        }
        Ok((executor::Run::Suspended, _)) => {
            queue.push(Err((task_id, "execution path suspended".to_string())));
        }
        Err((error, backtrace)) => {
            queue.push(Err((task_id, format!("{error}; backtrace: {backtrace:?}"))));
        }
    }
}

fn value(
    value: &Val<B129>,
    shared: &isla_lib::ir::SharedState<'_, B129>,
) -> anyhow::Result<TraceValue> {
    let mut smt = Vec::new();
    value.write(&mut smt, shared)?;
    let fields = match value {
        Val::Struct(fields) => fields
            .iter()
            .map(|(name, value)| {
                Ok((
                    zencode::decode(shared.symtab.to_str(*name)),
                    self::value(value, shared)?,
                ))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?,
        _ => HashMap::new(),
    };
    Ok(TraceValue {
        smt: String::from_utf8(smt)?,
        symbolic: value.is_symbolic(),
        fields,
    })
}

fn register(
    name: Name,
    accessors: &[isla_lib::smt::Accessor],
    shared: &isla_lib::ir::SharedState<'_, B129>,
) -> (String, Vec<String>) {
    (
        zencode::decode(shared.symtab.to_str(name)),
        accessors
            .iter()
            .map(|accessor| match accessor {
                isla_lib::smt::Accessor::Field(field) => {
                    zencode::decode(shared.symtab.to_str(*field))
                }
            })
            .collect(),
    )
}

fn normalize_event(
    event: Event<B129>,
    shared: &isla_lib::ir::SharedState<'_, B129>,
) -> anyhow::Result<Option<TraceEvent>> {
    use isla_lib::smt::smtlib::Def as SmtDef;
    Ok(match event {
        Event::Smt(SmtDef::DeclareConst(variable, ty), _, _) => Some(TraceEvent::Declare {
            declaration: format!(
                "(declare-const v{variable} {})",
                smt_format::ty(&ty, &shared.symtab)?
            ),
        }),
        Event::Smt(SmtDef::DeclareFun(variable, args, result), _, _) => {
            let args = args
                .iter()
                .map(|ty| smt_format::ty(ty, &shared.symtab))
                .collect::<Result<Vec<_>, _>>()?
                .join(" ");
            Some(TraceEvent::Declare {
                declaration: format!(
                    "(declare-fun v{variable} ({args}) {})",
                    smt_format::ty(&result, &shared.symtab)?
                ),
            })
        }
        Event::Smt(SmtDef::DefineConst(variable, expression), _, _) => Some(TraceEvent::Define {
            variable: format!("v{variable}"),
            expression: smt_format::exp_sym(&expression, shared)?,
        }),
        Event::Smt(SmtDef::Assert(expression), _, _) => Some(TraceEvent::Assume {
            expression: smt_format::exp_sym(&expression, shared)?,
        }),
        Event::Smt(SmtDef::DefineEnum(_, _), _, _) => None,
        Event::Assume(expression) => Some(TraceEvent::Assume {
            expression: smt_format::exp_loc(&expression, shared)?,
        }),
        Event::ReadReg(name, accessors, register_value) => {
            let (name, fields) = register(name, &accessors, shared);
            Some(TraceEvent::ReadRegister {
                name,
                fields,
                value: value(&register_value, shared)?,
            })
        }
        Event::WriteReg(name, accessors, register_value) => {
            let (name, fields) = register(name, &accessors, shared);
            Some(TraceEvent::WriteRegister {
                name,
                fields,
                value: value(&register_value, shared)?,
            })
        }
        Event::ReadMem {
            value: read_value,
            read_kind,
            address,
            bytes,
            ..
        } => Some(TraceEvent::ReadMemory {
            kind: value(&read_kind, shared)?,
            address: value(&address, shared)?,
            value: value(&read_value, shared)?,
            bytes,
        }),
        Event::WriteMem {
            write_kind,
            address,
            data,
            bytes,
            ..
        } => Some(TraceEvent::WriteMemory {
            kind: value(&write_kind, shared)?,
            address: value(&address, shared)?,
            value: value(&data, shared)?,
            bytes,
        }),
        _ => None,
    })
}

fn initial_checkpoint(architecture: &Initialized<'_, B129>) -> smt::Checkpoint<B129> {
    let config = smt::Config::new();
    let context = smt::Context::new(config);
    let mut solver = Solver::from_checkpoint(&context, smt::Checkpoint::new());
    let mut registers: Vec<_> = architecture.regs.iter().collect();
    registers.sort_by_key(|(name, _)| *name);
    for (name, register) in registers {
        if let Some(value) = register.read_last_if_initialized() {
            solver.add_event(Event::AssumeReg(*name, vec![], value.clone()));
        }
    }
    smt::checkpoint(&mut solver)
}
