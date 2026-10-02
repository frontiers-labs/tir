use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context as _};
use isla_lib::bitvector::b129::B129;
use isla_lib::bitvector::BV;
use isla_lib::config::ISAConfig;
use isla_lib::error::ExecError;
use isla_lib::executor::{self, LocalFrame, StopConditions, TaskId, TaskState};
use isla_lib::init::{initialize_architecture, Initialized};
use isla_lib::ir::{AssertionMode, BitsSegment, Def, IRTypeInfo, Instr, Loc, Name, Symtab, Val};
use isla_lib::ir_lexer::new_ir_lexer;
use isla_lib::ir_parser;
use isla_lib::memory::Memory;
use isla_lib::simplify;
use isla_lib::smt::{self, Event, Solver};
use isla_lib::value_parser;
use isla_lib::zencode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tir_symbolic::smtlib::ast::{Sort, Term};

mod constants;
mod smt_format;

/// A model value. Isla values outside SMT (structs, vectors, union
/// constructors) keep their shape so callers can pick the part they need.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum TraceValue {
    /// Bits, booleans, enum members and symbolic variables.
    Term {
        #[serde(with = "text")]
        term: Term,
        symbolic: bool,
    },
    Struct(BTreeMap<String, TraceValue>),
    Vector(Vec<TraceValue>),
    Ctor(String, Box<TraceValue>),
    /// Values no SMT term names (unit, strings, lists, references), as Isla
    /// prints them.
    Other {
        text: String,
        symbolic: bool,
    },
}

impl TraceValue {
    pub fn is_symbolic(&self) -> bool {
        match self {
            TraceValue::Term { symbolic, .. } | TraceValue::Other { symbolic, .. } => *symbolic,
            TraceValue::Struct(fields) => fields.values().any(TraceValue::is_symbolic),
            TraceValue::Vector(elements) => elements.iter().any(TraceValue::is_symbolic),
            TraceValue::Ctor(_, value) => value.is_symbolic(),
        }
    }

    pub fn term(&self) -> Option<&Term> {
        match self {
            TraceValue::Term { term, .. } => Some(term),
            _ => None,
        }
    }

    pub fn field(&self, name: &str) -> Option<&TraceValue> {
        match self {
            TraceValue::Struct(fields) => fields.get(name),
            _ => None,
        }
    }

    /// Every SMT term this value holds.
    pub fn terms(&self) -> Vec<&Term> {
        match self {
            TraceValue::Term { term, .. } => vec![term],
            TraceValue::Struct(fields) => fields.values().flat_map(TraceValue::terms).collect(),
            TraceValue::Vector(elements) => elements.iter().flat_map(TraceValue::terms).collect(),
            TraceValue::Ctor(_, value) => value.terms(),
            TraceValue::Other { .. } => Vec::new(),
        }
    }
}

impl std::fmt::Display for TraceValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TraceValue::Term { term, .. } => write!(f, "{term}"),
            TraceValue::Struct(fields) => {
                f.write_str("{")?;
                for (index, (name, value)) in fields.iter().enumerate() {
                    let separator = if index == 0 { "" } else { ", " };
                    write!(f, "{separator}{name}: {value}")?;
                }
                f.write_str("}")
            }
            TraceValue::Vector(elements) => {
                f.write_str("[")?;
                for (index, element) in elements.iter().enumerate() {
                    let separator = if index == 0 { "" } else { ", " };
                    write!(f, "{separator}{element}")?;
                }
                f.write_str("]")
            }
            TraceValue::Ctor(name, value) => write!(f, "{name}({value})"),
            TraceValue::Other { text, .. } => f.write_str(text),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum TraceEvent {
    Declare {
        variable: String,
        #[serde(with = "text")]
        sort: Sort,
    },
    Define {
        variable: String,
        #[serde(with = "text")]
        expression: Term,
    },
    Assume {
        #[serde(with = "text")]
        expression: Term,
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

/// Terms and sorts are cached as SMT-LIB text.
mod text {
    use std::fmt::Display;

    use serde::{Deserialize, Deserializer, Serializer};
    use tir_symbolic::smtlib::ast::{Sort, Term};
    use tir_symbolic::smtlib::parser::{parse_sort, parse_term};

    pub trait Parse: Sized {
        fn parse(src: &str) -> Result<Self, Vec<String>>;
    }

    impl Parse for Term {
        fn parse(src: &str) -> Result<Self, Vec<String>> {
            parse_term(src)
        }
    }

    impl Parse for Sort {
        fn parse(src: &str) -> Result<Self, Vec<String>> {
            parse_sort(src)
        }
    }

    pub fn serialize<T: Display, S: Serializer>(
        value: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_str(value)
    }

    pub fn deserialize<'de, T: Parse, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        let src = String::deserialize(deserializer)?;
        T::parse(&src).map_err(|errors| serde::de::Error::custom(errors.join("; ")))
    }
}

pub struct Verifier {
    architecture: Initialized<'static, B129>,
    function: Name,
    threads: usize,
    timeout_seconds: u64,
    simplify: bool,
    cache_fingerprint: u64,
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
        let config_source = std::fs::read_to_string(config)?;
        let configuration: constants::Config = toml::from_str(&config_source)?;
        let mut source = std::fs::read_to_string(snapshot)
            .with_context(|| format!("reading Sail snapshot {}", snapshot.display()))?;
        if let Some(wrapper) = &configuration.execution_wrapper {
            let path = config
                .parent()
                .context("Isla config has no parent directory")?
                .join(wrapper);
            source.push('\n');
            source.push_str(&std::fs::read_to_string(path)?);
        }
        let source: &'static str = Box::leak(source.into_boxed_str());
        let mut symtab = Symtab::new();
        let definitions = ir_parser::IrParser::new()
            .parse(&mut symtab, new_ir_lexer(source))
            .map_err(|error| anyhow!("Sail IR parse error: {error}"))?;
        let definitions: &'static mut [Def<Name, B129>] = Box::leak(definitions.into_boxed_slice());
        for name in &configuration.linearize {
            let function = symtab
                .get(&zencode::encode(name))
                .with_context(|| format!("unknown Sail function {name}"))?;
            let signature = definitions.iter().find_map(|definition| match definition {
                Def::Val(name, _, result) if *name == function => Some(result.clone()),
                _ => None,
            });
            if let Some(result) = signature {
                for definition in definitions.iter_mut() {
                    if let Def::Fn(name, _, body) = definition {
                        if *name == function {
                            *body = isla_lib::ir::linearize::linearize(
                                body.to_vec(),
                                &result,
                                &mut symtab,
                            );
                        }
                    }
                }
            }
        }
        if configuration.execution_wrapper.is_some() {
            let footprint = symtab
                .get(&zencode::encode(function))
                .context("unknown Sail footprint")?;
            let execute = symtab
                .get(&zencode::encode("execute"))
                .context("unknown Sail execute function")?;
            let wrapper = symtab
                .get(&zencode::encode("tir_verify_execute"))
                .context("unknown Sail execution wrapper")?;
            for definition in definitions.iter_mut() {
                if let Def::Fn(name, _, body) = definition {
                    if *name == footprint {
                        for instruction in body {
                            if let Instr::Call(_, _, target, _, _) = instruction {
                                if *target == execute {
                                    *target = wrapper;
                                }
                            }
                        }
                    }
                }
            }
        }
        let type_info = IRTypeInfo::new(definitions);
        let mut hasher = Sha256::new();
        let mut isa_config = ISAConfig::from_file(&mut hasher, config, None, &symtab, &type_info)
            .map_err(|error| anyhow!("Isla config error: {error}"))?;
        hasher.input(b"tir-sail-traces-v3\0");
        hasher.input(source.as_bytes());
        hasher.input(function.as_bytes());
        hasher.input([u8::from(simplify)]);
        for assignment in initial_registers {
            hasher.input([0]);
            hasher.input(assignment.as_bytes());
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
        configuration.apply(definitions, &symtab, &type_info)?;
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
        let digest = hasher.result();
        let cache_fingerprint = u64::from_le_bytes(digest[..8].try_into().unwrap());
        Ok(Self {
            architecture,
            function,
            threads: threads.max(1),
            timeout_seconds,
            simplify,
            cache_fingerprint,
        })
    }

    /// Identifies the model, configuration and trace normalization used by this verifier.
    pub fn cache_fingerprint(&self) -> u64 {
        self.cache_fingerprint
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
        let queue: Arc<BatchQueue> = Arc::new(Mutex::new(Vec::new()));
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
        for result in std::mem::take(&mut *queue.lock().unwrap()) {
            let (task_id, mut events) = match result {
                Ok(result) => result,
                Err((task_id, error)) => {
                    eprintln!(
                        "Sail execution failed for {:#x}: {error}",
                        task_words[&task_id]
                    );
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

type BatchQueue = Mutex<Vec<Result<(TaskId, Vec<Event<B129>>), (TaskId, String)>>>;

fn batch_collector<'ir>(
    _thread_id: usize,
    task_id: TaskId,
    result: Result<(executor::Run<B129>, LocalFrame<'ir, B129>), (ExecError, executor::Backtrace)>,
    shared: &isla_lib::ir::SharedState<'ir, B129>,
    solver: Solver<B129>,
    queue: &BatchQueue,
) {
    match result {
        Ok((executor::Run::Finished(_) | executor::Run::Exit, _)) => {
            let mut events = solver.trace().to_vec();
            queue
                .lock()
                .unwrap()
                .push(Ok((task_id, events.drain(..).cloned().collect())));
        }
        Ok((executor::Run::Dead, _)) => {
            queue
                .lock()
                .unwrap()
                .push(Err((task_id, "execution path is dead".to_string())));
        }
        Ok((executor::Run::Suspended, _)) => {
            queue
                .lock()
                .unwrap()
                .push(Err((task_id, "execution path suspended".to_string())));
        }
        Err((error, backtrace)) => {
            let backtrace = backtrace
                .iter()
                .map(|(name, pc)| format!("{}:{pc}", zencode::decode(shared.symtab.to_str(*name))))
                .collect::<Vec<_>>()
                .join(" -> ");
            queue
                .lock()
                .unwrap()
                .push(Err((task_id, format!("{error}; backtrace: {backtrace}"))));
        }
    }
}

fn value(
    value: &Val<B129>,
    shared: &isla_lib::ir::SharedState<'_, B129>,
) -> anyhow::Result<TraceValue> {
    let symbolic = value.is_symbolic();
    let term = |term| Ok(TraceValue::Term { term, symbolic });
    let name = |name: Name| zencode::decode(shared.symtab.to_str(name));
    match value {
        Val::Symbolic(variable) => term(Term::ident(smt_format::var(*variable))),
        Val::Bool(bit) => term(Term::bool(*bit)),
        Val::Bits(bits) => term(smt_format::bits_literal(&bits.to_string())?),
        Val::I64(n) => term(Term::bv(u128::from(*n as u64), 64)),
        Val::I128(n) => term(Term::bv(*n as u128, 128)),
        Val::MixedBits(segments) => {
            let segments = segments
                .iter()
                .map(|segment| match segment {
                    BitsSegment::Symbolic(variable) => Ok(Term::ident(smt_format::var(*variable))),
                    BitsSegment::Concrete(bits) => smt_format::bits_literal(&bits.to_string()),
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            term(Term::app("concat", segments))
        }
        Val::Enum(member) => {
            let members = shared
                .type_info
                .enums
                .get(&member.enum_id.to_name())
                .ok_or_else(|| anyhow!("missing Isla enum"))?;
            term(Term::ident(name(members[member.member])))
        }
        Val::Struct(fields) => Ok(TraceValue::Struct(
            fields
                .iter()
                .map(|(field, value)| Ok((name(*field), self::value(value, shared)?)))
                .collect::<anyhow::Result<_>>()?,
        )),
        Val::Vector(elements) => Ok(TraceValue::Vector(
            elements
                .iter()
                .map(|element| self::value(element, shared))
                .collect::<anyhow::Result<_>>()?,
        )),
        Val::Ctor(ctor, value) => Ok(TraceValue::Ctor(
            zencode::decode(shared.symtab.to_str_demangled(*ctor)),
            Box::new(self::value(value, shared)?),
        )),
        _ => Ok(TraceValue::Other {
            text: value.to_string(shared),
            symbolic,
        }),
    }
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
            variable: smt_format::var(variable),
            sort: smt_format::sort(&ty, &shared.symtab),
        }),
        Event::Smt(SmtDef::DeclareFun(..), _, _) => {
            anyhow::bail!("Isla trace declares an uninterpreted function")
        }
        Event::Smt(SmtDef::DefineConst(variable, expression), _, _) => Some(TraceEvent::Define {
            variable: smt_format::var(variable),
            expression: smt_format::term(&expression, shared)?,
        }),
        Event::Smt(SmtDef::Assert(expression), _, _) => Some(TraceEvent::Assume {
            expression: smt_format::term(&expression, shared)?,
        }),
        Event::Smt(SmtDef::DefineEnum(_, _), _, _) => None,
        Event::Assume(_) => anyhow::bail!("Isla trace assumes a register location"),
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
