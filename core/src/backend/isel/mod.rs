//! Instruction selection.
//!
//! Creating the pass synthesizes its selection rules (`synth`, `pool`)
//! from the dialects the context holds and the target's instructions, on a
//! thread the first function selected joins.

pub mod automaton;
pub mod edges;
mod flags;
mod float_refinement;
mod fp_environment;
mod pool;
mod relaxation;
mod rule;
mod rules;
mod score;
pub mod select;
mod synth;

use tir::{
    AnalysisManager, Context, OperationRef, Pass, PassError, PassTarget, Terms, TypeArg, TypeId,
    builtin::TupleType,
};
use tir_symbolic::term::ctor;

use automaton::Automaton;
pub use fp_environment::check_default_fp_environment;
pub use relaxation::{GuardedRelaxationProofs, prove_guarded_relaxations};
pub use rule::{
    BranchEmitters, EmitRequest, Emitter, FpFlags, FunctionCheck, ImmRange, LATENCY_COST_SCALE,
    OpLowering, RegisterCapability, RegisterRequirement, Rule, RuleEmitFn, RuleKind, RuleMatch,
    StatePorts,
};
pub use rules::{
    CapabilityKind, EmitAttr, EmitSpec, OperandConstraint, PatternRef, RegOperandSpec,
    ResultRegSpec, RuleSpec, build_rules,
};
pub(crate) use score::PlanScore;
pub use tir::sem::{SaturationLimits, SemEGraph, SemNode, SemPayload, Theory};

use tir::sem::rewrites::discover_rewrites;

pub struct InstructionSelectPass {
    automaton: Option<Automaton>,
    selection: Option<select::Rules>,
    cases: edges::Cases,
    // The tables key rows by label id, so every function's graph is built over
    // this vocabulary and hands it back.
    vocabulary: tir_egraph::Vocabulary<SemNode>,
    seeded: crate::rewrite::Kinds,
    pending: Option<std::thread::JoinHandle<Joined>>,
    // Sorted.
    kinds: Vec<u32>,
    pointer: u32,
    widths: Vec<u32>,
    synthesized: bool,
    rules: Vec<Rule>,
    /// The target's own data layout, applying where the IR declares none.
    default_layout: Option<crate::attributes::AttributeValue>,
    layout: Option<crate::attributes::AttributeValue>,
    /// The shared axioms the selection rules are synthesized with.
    theory: Theory,
    /// Target hooks for terminator lowering; branch selection is off without them
    /// (terminators are then left to the target's op lowerings).
    branch_emitters: Option<BranchEmitters>,
    op_lowerings: Vec<OpLowering>,
    function_check: Option<FunctionCheck>,
    call_lowering: Option<crate::backend::call_lowering::CallLowering>,
}

struct Joined {
    rules: Vec<Rule>,
    theory: Theory,
    synthesis: std::thread::Result<Synthesis>,
}

struct Synthesis {
    automaton: Automaton,
    selection: select::Rules,
    cases: edges::Cases,
    vocabulary: tir_egraph::Vocabulary<SemNode>,
    kinds: Vec<u32>,
    pointer: u32,
}

/// A function selected, emitted and recovered in a private context. Its score
/// describes the machine function it holds, which is the one adoption
/// publishes.
pub(crate) struct PreparedSelection {
    staged: Context,
    function: tir::OpId,
}

impl PreparedSelection {
    pub(crate) fn score(&self) -> PlanScore {
        PlanScore::of(&self.staged, self.function)
    }

    fn adopt(self, context: &Context) {
        context.adopt(self.staged);
    }
}

impl InstructionSelectPass {
    /// Build the pass for `rules`. Its selection rules are synthesized by
    /// [`Self::synthesized`], once the target has installed its theory and
    /// data layout.
    pub fn new(context: &Context, rules: Vec<Rule>) -> Self {
        Self {
            automaton: None,
            selection: None,
            cases: edges::Cases::default(),
            vocabulary: tir_egraph::Vocabulary::new(context.names()),
            seeded: crate::rewrite::Kinds::unflagged(),
            pending: None,
            kinds: Vec::new(),
            pointer: 64,
            widths: synth::INTEGER_WIDTHS.to_vec(),
            synthesized: false,
            rules,
            default_layout: None,
            layout: None,
            theory: discover_rewrites(),
            branch_emitters: None,
            op_lowerings: vec![],
            function_check: None,
            call_lowering: None,
        }
    }

    /// Synthesize the selection rules from the target's instructions, the
    /// dialects `context` holds and the theory, in full, on a thread of their
    /// own. Call it last: the rules are synthesized from what the pass is
    /// configured with by then.
    ///
    /// The operations are instantiated here, so the types and names the
    /// rules refer to have their ids before the thread starts, and the
    /// thread interns nothing while the caller goes on building IR: where it
    /// would, it stops, and the rules are synthesized by the function that
    /// joins it. The first function selected joins the thread; a pass that
    /// selects nothing leaves it detached.
    pub fn synthesized(mut self, context: &Context) -> Self {
        self.layout = self.default_layout.clone();
        let started = std::time::Instant::now();
        let (scratch, instances, pointer, tests) = self.instantiate(context, started);
        let scratch = scratch.detach();
        let rules = std::mem::take(&mut self.rules);
        let theory = std::mem::take(&mut self.theory);
        let thread = std::thread::Builder::new().name("isel-synth".into());
        let pending = thread.spawn(move || {
            let scratch = Context::open_frozen(scratch);
            let compile = || {
                Self::compile(
                    &scratch, &instances, &rules, &theory, pointer, &tests, started,
                )
            };
            let synthesis = std::panic::catch_unwind(std::panic::AssertUnwindSafe(compile));
            Joined {
                rules,
                theory,
                synthesis,
            }
        });
        self.pending = Some(pending.expect("a thread to synthesize the selection rules on"));
        self
    }

    fn synthesize(&mut self, context: &Context) {
        let started = std::time::Instant::now();
        let (scratch, instances, pointer, tests) = self.instantiate(context, started);
        let (rules, theory) = (&self.rules, &self.theory);
        let synthesis = Self::compile(
            &scratch, &instances, rules, theory, pointer, &tests, started,
        );
        self.install(synthesis);
    }

    fn install(&mut self, synthesis: Synthesis) {
        self.automaton = Some(synthesis.automaton);
        self.selection = Some(synthesis.selection);
        self.cases = synthesis.cases;
        self.vocabulary = synthesis.vocabulary;
        self.kinds = synthesis.kinds;
        self.pointer = synthesis.pointer;
        self.synthesized = true;
    }

    fn join(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let waited = std::time::Instant::now();
        let joined = pending
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        if std::env::var_os("TIR_ISEL_SYNTH").is_some() {
            eprintln!(
                "isel-synth: joined after {} us",
                waited.elapsed().as_micros()
            );
        }
        self.rules = joined.rules;
        self.theory = joined.theory;
        match joined.synthesis {
            Ok(synthesis) => self.install(synthesis),
            Err(panic) => match panic.downcast::<crate::context::Refused>() {
                // The rules stay unsynthesized, and `settle` goes on to
                // synthesize them here, where interning is in order.
                Ok(refused) => {
                    if std::env::var_os("TIR_ISEL_SYNTH").is_some() {
                        eprintln!("isel-synth: the thread stopped at {}", refused.0);
                    }
                }
                Err(panic) => std::panic::resume_unwind(panic),
            },
        }
    }

    fn instantiate(
        &self,
        context: &Context,
        started: std::time::Instant,
    ) -> (
        Context,
        Vec<synth::Instance>,
        u32,
        Vec<(tir::TypeId, usize)>,
    ) {
        let scratch = context.fork();
        let layout = self.layout.as_ref();
        let mut skipped = Vec::new();
        let vectors = synth::vectors(&scratch, &self.rules, &self.widths);
        let types = (self.widths.as_slice(), vectors.as_slice());
        let mut instances = synth::instances(&scratch, types, layout, &mut skipped);
        // A target that declares no layout holds a pointer in 64 bits.
        let pointer = layout
            .and_then(|layout| tir::DataLayout::from_value(&scratch, layout))
            .and_then(|layout| layout.pointer_size())
            .unwrap_or(64);
        synth::accesses(&scratch, pointer, &mut skipped, &mut instances);
        if std::env::var_os("TIR_ISEL_SYNTH").is_some() {
            eprintln!(
                "isel-synth: {} schemas, {} instances, {} skipped, {} us",
                tir::schema::OP_SCHEMAS.len(),
                instances.len(),
                skipped.len(),
                started.elapsed().as_micros()
            );
            let mut kinds = std::collections::BTreeMap::new();
            let mut nodes = 0;
            for instance in &instances {
                *kinds
                    .entry((instance.schema.dialect, instance.schema.name))
                    .or_insert(0usize) += 1;
                nodes += instance.semantics.len();
                debug_assert!(scratch.has_operation(instance.op));
                debug_assert!(instance.root.index() < instance.semantics.len());
            }
            eprintln!("isel-synth: {nodes} semantic nodes; instances {kinds:?}");
            let mut reasons = std::collections::BTreeMap::new();
            for &(dialect, name, why) in &skipped {
                *reasons.entry((why, dialect, name)).or_insert(0usize) += 1;
            }
            eprintln!("isel-synth: skipped {reasons:?}");
        }
        // The instance recovery's own test of a case is, per integer type:
        // its row is the compare a control with no compare of its own
        // branches on.
        let tested = |&width: &u32| {
            let ty = context.terms().int(u64::from(width));
            let operand = || scratch.create_value(ty, None).id();
            let test = crate::passes::destructure::case_test(&scratch, operand(), operand());
            let test = scratch.get_op(tir::Operation::id(&test));
            let types = |op: &tir::OpHandle| -> Vec<tir::TypeId> {
                let values = op.operands().into_iter().chain(op.results());
                values.map(|value| scratch.get_value(value).ty()).collect()
            };
            // An instance names the data layout it was made under as well.
            let same = |instance: &synth::Instance| {
                let op = scratch.get_op(instance.op);
                let named = op.attributes();
                scratch.op_name_id(op.id) == scratch.op_name_id(test.id)
                    && test.attributes().iter().all(|held| named.contains(held))
                    && types(&op) == types(&test)
            };
            Some((ty, instances.iter().position(same)?))
        };
        let tests = self.widths.iter().filter_map(tested).collect();
        (scratch, instances, pointer, tests)
    }

    fn named(scratch: &Context, instance: &synth::Instance) -> String {
        let op = scratch.get_op(instance.op);
        let types: Vec<String> = op
            .operands()
            .iter()
            .chain(op.results().iter())
            .map(|&value| scratch.type_to_string(scratch.get_value(value).ty()))
            .collect();
        let attribute = |name| op.attr(name).map(|held| format!(" {held:?}"));
        format!(
            "{}.{}({}){}{}",
            instance.schema.dialect,
            instance.schema.name,
            types.join(","),
            attribute("predicate").unwrap_or_default(),
            attribute("semantics").unwrap_or_default()
        )
    }

    fn spelled(
        scratch: &Context,
        instances: &[synth::Instance],
        rows: &pool::Rows,
        at: usize,
    ) -> String {
        let row = &rows[at];
        let operands: Vec<String> = row
            .operands
            .iter()
            .map(|operand| match *operand {
                pool::Operand::Register(operand) => format!("r{operand}"),
                pool::Operand::Immediate(operand) => format!("i{operand}"),
                pool::Operand::Literal(value) => format!("{value}"),
                pool::Operand::Row(below) => {
                    Self::spelled(scratch, instances, rows, usize::from(below))
                }
            })
            .collect();
        let name = Self::named(scratch, &instances[row.instance as usize]);
        format!("{name}[{}]", operands.join(", "))
    }

    // The rule list and the tables, for TIR_ISEL_SYNTH.
    fn report(
        scratch: &Context,
        instances: &[synth::Instance],
        rules: &[Rule],
        synthesized: &[pool::Synthesized],
        automaton: &Automaton,
    ) {
        let names = |tree: &pool::Tree| -> Vec<&str> {
            let steps = tree.iter();
            steps
                .map(|step| rules[automaton.forms[step.form as usize].rule].name)
                .collect()
        };
        for rule in synthesized {
            let branch = rule
                .branch
                .as_ref()
                .map(|branch| (names(&branch.taken), branch.inverse.as_ref().map(names)));
            eprintln!(
                "isel-rule: {} => {:?} {:?} cost {} fields {:?} port {}/{} effect {} branch {branch:?}",
                Self::spelled(scratch, instances, &rule.rows, 0),
                names(&rule.tree),
                rule.value,
                rule.cost,
                rule.fields,
                rule.port,
                rule.results,
                rule.effect,
            );
        }
        for (pattern, accept) in automaton.accept.iter().enumerate() {
            let Some(accept) = accept else {
                continue;
            };
            let operands: Vec<_> = accept
                .operands
                .iter()
                .map(|bind| (bind.path.as_slice(), bind.held))
                .collect();
            eprintln!(
                "isel-accept: {pattern} => {:?} {:?} cost {} port {}/{} operands {operands:?} held {:#b} ties {:?}",
                names(&accept.tree),
                accept.value,
                accept.cost,
                accept.port,
                accept.results,
                automaton.field[pattern],
                automaton.ties[pattern],
            );
        }
        for builder in &automaton.builders {
            eprintln!(
                "isel-builder: {} symbol {} field {:?} shift {} register {:?} shifts {} masks {} width {:?}",
                rules[automaton.forms[builder.form as usize].rule].name,
                builder.symbol,
                builder.range,
                builder.shift,
                builder.register,
                builder.shifts,
                builder.masks,
                builder.width,
            );
        }
        let mut sizes = std::collections::BTreeMap::new();
        for rule in synthesized {
            *sizes.entry(rule.rows.len()).or_insert(0usize) += 1;
        }
        let rows = |tables: &[std::sync::Arc<tir_relational::Words>]| -> usize {
            tables.iter().map(|table| table.len()).sum()
        };
        // The keys of the plain step table that hold several patterns.
        let mut keys = std::collections::HashMap::new();
        for table in &automaton.step {
            for row in 0..table.len() {
                let key = (0..table.width() - 2).map(|column| table.column(column)[row]);
                let pattern = table.column(table.width() - 2)[row];
                keys.entry(key.collect::<Vec<u32>>())
                    .or_insert_with(Vec::new)
                    .push(pattern);
            }
        }
        for (key, patterns) in keys.iter().filter(|(_, patterns)| patterns.len() > 1) {
            eprintln!("isel-several: {key:?} => {patterns:?}");
        }
        for adder in &automaton.adders {
            eprintln!(
                "isel-adder: {:?} class {:?} => {:?} cost {}",
                automaton.seeds[adder.base as usize],
                adder.class,
                names(&adder.accept.tree),
                adder.accept.cost,
            );
        }
        let spell = |rule: usize| Self::spelled(scratch, instances, &synthesized[rule].rows, 0);
        for &(rule, why) in &automaton.unkeyed {
            eprintln!(
                "isel-unkeyed: {why:?} {} => {:?}",
                spell(rule),
                names(&synthesized[rule].tree)
            );
        }
        for &rule in &automaton.shadowed {
            eprintln!(
                "isel-shadowed: {} => {:?}",
                spell(rule),
                names(&synthesized[rule].tree)
            );
        }
        let arities: Vec<usize> = (0..automaton.step.len())
            .filter(|&arity| {
                let tables = [
                    &automaton.step,
                    &automaton.step_effect,
                    &automaton.step_over,
                    &automaton.step_tied,
                ];
                tables.iter().any(|tables| !tables[arity].is_empty())
            })
            .collect();
        eprintln!(
            "isel-auto: rules {} by rows {sizes:?} unkeyed {} shadowed {}; seeds {} states {}; arities {arities:?}; step rows {} effect {} over {} tied {} keys {} with several patterns {}; imm rows {}; accepts {}; branches {} with inverse {}; adders {} builders {} floats {}",
            synthesized.len(),
            automaton.unkeyed.len(),
            automaton.shadowed.len(),
            automaton.seeds.len(),
            automaton.patterns(),
            rows(&automaton.step),
            rows(&automaton.step_effect),
            rows(&automaton.step_over),
            rows(&automaton.step_tied),
            keys.len(),
            keys.values().filter(|held| held.len() > 1).count(),
            rows(&automaton.imm),
            automaton.accept.iter().flatten().count(),
            automaton.branches.len(),
            automaton
                .branches
                .iter()
                .filter(|(_, branch)| branch.inverse.is_some())
                .count(),
            automaton.adders.len(),
            automaton.builders.len(),
            automaton.floats.len(),
        );
    }

    fn compile(
        scratch: &Context,
        instances: &[synth::Instance],
        rules: &[Rule],
        theory: &Theory,
        pointer: u32,
        tests: &[(tir::TypeId, usize)],
        started: std::time::Instant,
    ) -> Synthesis {
        // A row that raises floating-point flags takes a tree only where
        // the tree's last instruction raises exactly those.
        let noisy = std::env::var_os("TIR_ISEL_SYNTH").is_some();
        let created = started;
        let admits = |instance: usize, forms: &[pool::Form], last: &pool::Step| {
            let form = &forms[last.form as usize];
            let bound: Vec<(u32, usize)> = form
                .symbols
                .iter()
                .zip(&last.args)
                .filter_map(|(&symbol, arg)| match arg {
                    pool::Arg::Operand(operand) => Some((symbol, usize::from(*operand))),
                    _ => None,
                })
                .collect();
            let op = scratch.get_op(instances[instance].op);
            let raises = flags::raises(scratch, &op, &rules[form.rule], &bound);
            if noisy && !raises {
                eprintln!(
                    "isel-admits: {}.{} {:?} refuses {}",
                    instances[instance].schema.dialect,
                    instances[instance].schema.name,
                    op.attr("semantics"),
                    rules[form.rule].name
                );
            }
            raises
        };
        let mut report = pool::Report::default();
        let pool = pool::Pool::build(
            scratch,
            scratch,
            instances,
            rules,
            theory,
            pointer,
            &admits,
            &mut report,
        );
        if std::env::var_os("TIR_ISEL_SYNTH").is_some() {
            eprintln!(
                "isel-pool: lower {} us, saturate {} us, compile {} us, match {} us, extract {} us, prune {} us, total {} us",
                report.lower_us,
                report.saturate_us,
                report.compile_us,
                report.match_us,
                report.extract_us,
                report.prune_us,
                started.elapsed().as_micros()
            );
            eprintln!(
                "isel-pool: nodes seeded {} saturated {} with machine nodes {}; patterns {} compiled {}; forms {}; instances {} covered {}; tiled {}; found {}",
                report.seeded,
                report.saturated,
                report.matched,
                report.patterns,
                report.compiled,
                pool.forms.len(),
                instances.len(),
                report.covered,
                report.tiled,
                report.found
            );
        }
        let started = std::time::Instant::now();
        let mut describing = automaton::Describing::default();
        let mut vocabulary = tir_egraph::Vocabulary::new(scratch.names());
        let mut described = vec![None; instances.len()];
        for row in pool.rules.iter().flat_map(|rule| &rule.rows) {
            let at = row.instance as usize;
            if described[at].is_none() {
                let instance = &instances[at];
                described[at] = describing.describe(scratch, pointer, instance, &mut vocabulary);
            }
        }
        let seeds = describing.seeds;
        let compares: Vec<(automaton::Pattern, u32, u32)> = tests
            .iter()
            .filter_map(|&(ty, instance)| {
                let seed = automaton::Seed::Register(automaton::canonical_type(scratch, ty));
                let seed = seeds.iter().position(|known| *known == seed)?;
                let width = u32::try_from(scratch.terms().bit_width(ty)?).ok()?;
                Some((seed as u32, described[instance].as_ref()?.label, width))
            })
            .collect();
        let mut automaton = Automaton::build(&pool.rules, &described, seeds, &mut vocabulary);
        if noisy {
            eprintln!("isel-cases: tests {tests:?} compares {compares:?}");
        }
        automaton.builders = pool.materializers;
        let names = instances.iter();
        automaton.instantiated = names
            .map(|instance| (instance.schema.dialect, instance.schema.name))
            .collect();
        automaton.instantiated.sort_unstable();
        automaton.instantiated.dedup();
        let terms = instances.iter();
        let terms = terms.map(|instance| *instance.semantics.get_node(instance.root));
        automaton.accessed = terms.filter(|&term| automaton::accesses(term)).collect();
        automaton.accessed.dedup();
        automaton.floats = pool.floats;
        automaton.forms = pool.forms;
        if noisy {
            eprintln!(
                "isel-auto: tables {} us, creation {} us",
                started.elapsed().as_micros(),
                created.elapsed().as_micros()
            );
            Self::report(scratch, instances, rules, &pool.rules, &automaton);
            let uncovered: Vec<String> = report
                .uncovered
                .iter()
                .map(|&instance| Self::named(scratch, &instances[instance as usize]))
                .collect();
            eprintln!("isel-pool: uncovered {uncovered:?}");
        }
        let mut kinds: Vec<u32> = instances
            .iter()
            .map(|instance| scratch.op_name_id(instance.op).index() as u32)
            .collect();
        kinds.sort_unstable();
        kinds.dedup();
        let selection = select::Rules::of(&automaton);
        let vocabulary = selection.warm(vocabulary);
        if noisy {
            let selecting = selection.selecting.iter().map(|rule| &**rule);
            for rule in selection.ordering.iter().chain(selecting) {
                eprintln!("isel-generic: {} {:?}", rule.name, rule.plan.steps());
            }
        }
        Synthesis {
            cases: edges::Cases::of(&automaton, &compares),
            automaton,
            selection,
            vocabulary,
            kinds,
            pointer,
        }
    }

    /// Install the target's terminator emitters, enabling rule-driven selection
    /// of conditional branches (and generic lowering of unconditional ones).
    pub fn with_branch_emitters(mut self, emitters: BranchEmitters) -> Self {
        self.branch_emitters = Some(emitters);
        self
    }

    /// Install semantic invariants the selection rules are synthesized with.
    pub fn with_theory(mut self, theory: Theory) -> Self {
        self.theory = theory;
        self
    }

    /// Install read-zero and ignored-write facts derived from target register traits.
    pub fn with_hardwired_zero_fields(
        mut self,
        fields: &[(tir::sem::StateResourceKind, tir::sem::StateFieldKind)],
    ) -> Self {
        self.theory = self.theory.with_hardwired_zero_fields(fields);
        self
    }

    /// Check the target's function preconditions before selection begins.
    pub fn with_function_check(mut self, check: FunctionCheck) -> Self {
        self.function_check = Some(check);
        self
    }

    /// Install the target's own data layout, which the pass reads where the IR
    /// declares none (see [`DataLayout::for_op_with_default`](crate::DataLayout)).
    pub fn with_data_layout(mut self, spec: Option<crate::attributes::AttributeValue>) -> Self {
        self.default_layout = spec;
        self
    }

    pub fn with_op_lowering(
        mut self,
        lowering: impl Fn(&Context, &OperationRef) -> Result<bool, PassError> + Send + Sync + 'static,
    ) -> Self {
        self.op_lowerings.push(Box::new(lowering));
        self
    }

    pub fn with_call_lowering(
        mut self,
        abi: &'static crate::backend::abi::AbiInfo,
        emitter: Box<dyn crate::backend::call_lowering::CallEmitter>,
    ) -> Self {
        self.call_lowering = Some(crate::backend::call_lowering::CallLowering::new(
            abi, emitter,
        ));
        self
    }

    // A pipeline step of its own, so the wait on synthesis is timed apart from
    // selection.
    pub(crate) fn settle(&mut self, context: &Context, function: &OperationRef) {
        // The rules are for one width of pointer: the one of the target's
        // data layout under what the function's scope declares.
        let layout = self.default_layout.as_ref();
        let layout = tir::DataLayout::for_op_with_default(context, function.op().id, layout);
        let pointer = layout.as_ref().and_then(tir::DataLayout::pointer_size);
        self.join();
        if !self.synthesized || pointer.unwrap_or(64) != self.pointer {
            self.layout = layout.map(tir::DataLayout::into_value);
            self.synthesize(context);
        }
    }

    /// Select `function` in a fork of `context`, recover its blocks there and
    /// score the result.
    pub(crate) fn prepare(
        &mut self,
        context: &Context,
        function: &OperationRef,
    ) -> Result<PreparedSelection, PassError> {
        self.settle(context, function);
        if let Some(check) = self.function_check {
            check(context, function)?;
        }
        let emitters = self.branch_emitters.as_ref().ok_or_else(|| {
            PassError::InvalidRuleSet("the target installs no branch emitters".into())
        })?;
        let source = function.op().id;
        let staged = context.fork();
        if let Some(lowering) = &mut self.call_lowering {
            lowering.reset();
            lowering.prepare_function(&staged, &OperationRef::new(staged.get_op(source)))?;
        }
        let (Some(automaton), Some(rules)) = (self.automaton.as_ref(), self.selection.as_ref())
        else {
            return Err(PassError::InvalidRuleSet(
                "the selection rules are not synthesized".into(),
            ));
        };
        // A call reads the elements of a tuple argument off to the side.
        let implicit = self.call_lowering.as_ref();
        let implicit = implicit.map(|lowering| lowering.implicit_inputs(&staged));
        let commit = select::Commit {
            select: select::Select {
                automaton,
                rules,
                cases: &self.cases,
            },
            set: rules.set(),
            target: &self.rules,
            kinds: &self.kinds,
            emitted: select::Emitted::after(implicit.into_iter().flatten()).into(),
        };
        let mut trace = select::Trace::default();
        let mut clock = std::time::Instant::now();
        let mut lap = || {
            let from = std::mem::replace(&mut clock, std::time::Instant::now());
            from.elapsed().as_micros()
        };
        let lent = tir_egraph::Vocabulary::new(context.names());
        let vocabulary = std::mem::replace(&mut self.vocabulary, lent);
        let mut seeded = crate::rewrite::seed(&staged, source, vocabulary, &mut self.seeded, true);
        select::declare(&mut seeded.eg);
        trace.rows = seeded.eg.total_size();
        trace.seed_us = lap();
        trace.seeds = commit.select.seed_chains(&staged, &mut seeded.eg);
        trace.chains_us = lap();
        commit
            .select
            .facts(&staged, &mut seeded.eg, &seeded.value_class);
        trace.facts_us = lap();
        let mut driver =
            crate::rewrite::Driver::new(&staged, seeded, &commit, "instruction-select");
        rules.order(&mut driver.eg, trace.rows);
        trace.order_us = lap();
        driver.saturate();
        trace.saturate_us = lap();
        let extraction = driver.extract();
        trace.extract_us = lap();
        let traced = std::env::var_os("TIR_ISEL_TRACE").is_some();
        let name = format!("{:?}", function.op().attr("sym_name"));
        if traced {
            let values = driver.value_class.iter();
            trace.values = values.map(|(&value, &class)| (value, class)).collect();
            commit
                .select
                .report(&staged, &driver.eg, &extraction, &name, &trace);
        }
        tir_relational::report_saturation("instruction-select");
        lap();
        if let Err(uncovered) = commit.select.covered(&staged, &driver.eg, &extraction) {
            self.vocabulary = driver.eg.release();
            return Err(uncovered);
        }
        let body = staged.get_op(source).regions()[0];
        let mut commit_us = 0;
        // Recovery reads the graph and the extraction for its branches.
        let recovered = (|| {
            driver.commit(body, &extraction, &mut Default::default())?;
            let graph = (&driver.eg, &driver.value_class);
            let emitted = commit.finish(&staged, graph, &extraction, source)?;
            commit_us = lap();
            let edges = edges::Machine {
                context: &staged,
                emitters,
                commit: &commit,
                emitted: &emitted,
                graph,
                extraction: &extraction,
                types: Default::default(),
                reads: Default::default(),
            };
            crate::passes::destructure::recover_cfg(&staged, body, &edges)?;
            commit.branched(&staged, body)
        })();
        self.vocabulary = driver.eg.release();
        recovered?;
        // Every block is read before one is written: a read after a write
        // brings the lists of all of them up to date again.
        let blocks = staged.get_region(body).block_ids().into_iter();
        let blocks: Vec<_> = blocks.map(|block| staged.get_block(block)).collect();
        let held: Vec<_> = blocks.iter().map(|block| block.op_ids()).collect();
        let unassigned = crate::backend::RegAssignment::default();
        for (block, ops) in blocks.iter().zip(held) {
            let dependences = crate::backend::Dependences::of_ops(&staged, &ops, &unassigned);
            let order = dependences.linearize().ok_or_else(|| {
                let block = block.id();
                PassError::InvalidRuleSet(format!("cyclic instruction dependences in {block:?}"))
            })?;
            block.set_ops(order);
        }
        if traced || std::env::var_os("TIR_ISEL_TIMES").is_some() {
            eprintln!(
                "isel-times: {name} us seed {} chains {} facts {} saturate {} extract {} commit {commit_us} recovery {}",
                trace.seed_us,
                trace.chains_us,
                trace.facts_us,
                trace.saturate_us,
                trace.extract_us,
                lap()
            );
        }
        Ok(PreparedSelection {
            staged,
            function: source,
        })
    }
}

impl Pass for InstructionSelectPass {
    fn name(&self) -> &'static str {
        "instruction-select"
    }

    fn target(&self) -> PassTarget {
        PassTarget::Any
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        if !op.op().regions().is_empty() {
            self.prepare(context, op)?.adopt(context);
        }
        for lowering in &self.op_lowerings {
            if lowering(context, op)? {
                return Ok(());
            }
        }
        if let Some(lowering) = &mut self.call_lowering
            && lowering.lower(context, op)?
        {
            return Ok(());
        }
        Ok(())
    }
}

// What selection reads a value of IR type `ty` as. A rounding mode or a
// floating-point environment is read as the integer of its carrier, which is
// what the semantics of the operations taking one name. Types selection does
// not read (states, pointers, vectors) have none.
fn value_type(terms: &Terms, ty: TypeId) -> Option<TypeId> {
    match terms.ctor(ty) {
        ctor::INT | ctor::FLOAT => Some(ty),
        ctor::TUPLE => match TupleType::view(terms, ty)?.elements()[..] {
            [first, second] => Some(pair(
                terms,
                value_type(terms, first)?,
                value_type(terms, second)?,
            )),
            _ => None,
        },
        _ if tir::fp::RoundingType::view(terms, ty).is_some()
            || tir::fp::EnvironmentType::view(terms, ty).is_some() =>
        {
            terms.bit_width(ty).map(|width| terms.int(width))
        }
        _ => None,
    }
}

// The IR type a value of semantic type `ty` is held in: raw bits are held as
// an integer, and a type with a width still unknown has none.
fn held_type(terms: &Terms, ty: &TypeArg) -> Option<TypeId> {
    let ty = ty.as_type()?;
    match terms.ctor(ty) {
        ctor::INT | ctor::RAW => terms.nat(ty, 0).map(|width| terms.int(width)),
        ctor::FLOAT => terms.float_format(ty).map(|_| ty),
        ctor::UNIT => Some(ty),
        ctor::TUPLE => match TupleType::view(terms, ty)?.elements()[..] {
            [first, second] => Some(pair(
                terms,
                held_type(terms, &TypeArg::Type(first))?,
                held_type(terms, &TypeArg::Type(second))?,
            )),
            _ => None,
        },
        _ => None,
    }
}

fn pair(terms: &Terms, first: TypeId, second: TypeId) -> TypeId {
    let elements = TypeArg::List([TypeArg::Type(first), TypeArg::Type(second)].into());
    terms
        .intern(ctor::TUPLE, vec![elements])
        .expect("a pair of data types")
}
