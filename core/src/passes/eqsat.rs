//! Equality saturation driven by an explicit typed PDL library.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tir_egglog::{
    ArcSort, Core, EGraph, Extractor, Primitive, PurePrim, PureState, Read, Value, Write,
};

use crate::{AnalysisManager, Context, OpId, Pass, PassError};

#[path = "eqsat_library.rs"]
mod library;
pub use library::{Capture, CheckedRewrite, EqSatLibrary, forward_memory};

mod default_library {
    include!(concat!(env!("OUT_DIR"), "/eqsat-pdl.rs"));
}

#[derive(Clone)]
struct IntegerBits([ArcSort; 4]);

#[derive(Clone)]
enum IntegerStorage {
    Bits(ArcSort),
    Width { bits: ArcSort, integer: ArcSort },
}

impl Primitive for IntegerStorage {
    fn name(&self) -> &str {
        match self {
            Self::Bits(_) => "eqsat-storage-bits",
            Self::Width { .. } => "eqsat-storage-width",
        }
    }
    fn signature(&self) -> Vec<ArcSort> {
        match self {
            Self::Bits(bits) => vec![bits.clone(); 2],
            Self::Width { bits, integer } => vec![bits.clone(), integer.clone()],
        }
    }
}

impl PurePrim for IntegerStorage {
    fn apply<'a, 'db>(&self, state: PureState<'a, 'db>, args: &[Value]) -> Option<Value> {
        let bits = state.value_to_base::<String>(args[0]);
        let digits = bits.len().div_ceil(16).checked_mul(16)?;
        match self {
            Self::Bits(_) => Some(state.base_to_value(format!("{bits:0>digits$}"))),
            Self::Width { .. } => {
                Some(state.base_to_value(i64::try_from(digits).ok()?.checked_mul(4)?))
            }
        }
    }
}

impl Primitive for IntegerBits {
    fn name(&self) -> &str {
        "eqsat-int-bits"
    }

    fn signature(&self) -> Vec<ArcSort> {
        self.0.to_vec()
    }
}

impl PurePrim for IntegerBits {
    fn apply<'a, 'db>(&self, state: PureState<'a, 'db>, args: &[Value]) -> Option<Value> {
        use crate::attributes::AttributeValue;

        let width = state.value_to_base::<tir_egglog::sort::BigInt>(args[0]);
        let width = width
            .to_string()
            .parse::<u64>()
            .ok()
            .filter(|width| *width > 0)?;
        let meaning = state.value_to_base::<String>(args[1]);
        let bits = state.value_to_base::<String>(args[2]);
        let attribute = match meaning.as_str() {
            "$attr.signed" => AttributeValue::Int(u64::from_str_radix(&bits, 16).ok()? as i64),
            "$attr.unsigned" => AttributeValue::UInt(u64::from_str_radix(&bits, 16).ok()?),
            "$attr.wide" => AttributeValue::Wide(
                bits.as_bytes()
                    .rchunks(16)
                    .map(|chunk| u64::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok())
                    .collect::<Option<Vec<_>>>()?
                    .into(),
            ),
            _ => return None,
        };
        Some(state.base_to_value(format!("{:x}", attribute.int_at(width)?)))
    }
}

#[derive(Clone)]
enum IntegerEval {
    Binary([ArcSort; 5]),
    Unary([ArcSort; 4]),
}

impl Primitive for IntegerEval {
    fn name(&self) -> &str {
        "eqsat-int-eval"
    }
    fn signature(&self) -> Vec<ArcSort> {
        match self {
            Self::Binary(sorts) => sorts.to_vec(),
            Self::Unary(sorts) => sorts.to_vec(),
        }
    }
}

impl PurePrim for IntegerEval {
    fn apply<'a, 'db>(&self, state: PureState<'a, 'db>, args: &[Value]) -> Option<Value> {
        let name = state.value_to_base::<String>(args[0]);
        let width = state.value_to_base::<tir_egglog::sort::BigInt>(args[1]);
        let width = width
            .to_string()
            .parse::<u64>()
            .ok()
            .filter(|width| *width > 0)?;
        let lhs = state.value_to_base::<String>(args[2]);
        let lhs = bitmask(&lhs, width)?;
        let result = match self {
            Self::Binary(_) => {
                let operation = crate::sem::scalar_op_named(&name)?;
                if operation.arity != 2
                    || !matches!(
                        operation.width,
                        crate::sem::WidthRule::First | crate::sem::WidthRule::Bool
                    )
                {
                    return None;
                }
                let rhs = state.value_to_base::<String>(args[3]);
                operation.eval_int(&[lhs, bitmask(&rhs, width)?])
            }
            Self::Unary(_) => {
                let value = match name.as_str() {
                    "ctz" => lhs.count_trailing_zeros(),
                    "popcount" => lhs.count_ones(),
                    _ => return None,
                };
                crate::utils::APInt::new(width, value)
            }
        };
        Some(state.base_to_value(format!("{result:x}")))
    }
}

#[derive(Clone)]
enum Bitwise {
    And(ArcSort),
    Or(ArcSort),
    Not { width: ArcSort, bits: ArcSort },
}

impl Primitive for Bitwise {
    fn name(&self) -> &str {
        match self {
            Self::And(_) => "eqsat-bits-and",
            Self::Or(_) => "eqsat-bits-or",
            Self::Not { .. } => "eqsat-bits-not",
        }
    }
    fn signature(&self) -> Vec<ArcSort> {
        match self {
            Self::And(bits) | Self::Or(bits) => vec![bits.clone(); 3],
            Self::Not { width, bits } => vec![width.clone(), bits.clone(), bits.clone()],
        }
    }
}

fn bitmask(bits: &str, width: u64) -> Option<crate::utils::APInt> {
    let limbs = bits
        .as_bytes()
        .rchunks(16)
        .map(|chunk| u64::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    Some(crate::utils::APInt::from_limbs(width, &limbs))
}

impl PurePrim for Bitwise {
    fn apply<'a, 'db>(&self, state: PureState<'a, 'db>, args: &[Value]) -> Option<Value> {
        use crate::utils::APInt;

        let right = state.value_to_base::<String>(args[1]);
        let binary: fn(&APInt, &APInt) -> APInt = match self {
            Self::And(_) => APInt::and,
            Self::Or(_) => APInt::or,
            Self::Not { .. } => {
                let width = state.value_to_base::<tir_egglog::sort::BigInt>(args[0]);
                let width = width.to_string().parse::<u64>().ok().filter(|w| *w > 0)?;
                return Some(state.base_to_value(format!("{:x}", bitmask(&right, width)?.not())));
            }
        };
        let left = state.value_to_base::<String>(args[0]);
        let width = u64::try_from(left.len().max(right.len()))
            .ok()?
            .checked_mul(4)?
            .max(1);
        Some(state.base_to_value(format!(
            "{:x}",
            binary(&bitmask(&left, width)?, &bitmask(&right, width)?)
        )))
    }
}

/// Applies a checked subset of PDL equalities over the context's IR relations.
/// The initial library uses an explicit trusted wrapping-integer contract.
#[derive(Clone)]
pub struct EqSatPass {
    library: EqSatLibrary,
    prefix: String,
    iterations: usize,
    operation_cost: Arc<dyn Fn(&str) -> u64 + Send + Sync>,
}

impl Default for EqSatPass {
    fn default() -> Self {
        Self::new()
    }
}

impl EqSatPass {
    /// Use the checked-in handwritten library and a bounded saturation run.
    pub fn new() -> Self {
        Self {
            library: EqSatLibrary::new(default_library::file())
                .bind("forward-integer", forward_memory)
                .bind("forward-pointer", forward_memory),
            prefix: "EqSat0".into(),
            iterations: 32,
            operation_cost: Arc::new(|kind| if kind == "builtin.muli" { 2 } else { 1 }),
        }
    }

    /// Supply typed PDL. Unsupported syntax and contracts are errors.
    /// Accepts trusted integer patterns and the supported known-bits guards.
    pub fn with_library(library: impl Into<EqSatLibrary>) -> Result<Self, PassError> {
        let library = library.into();
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let prefix = format!("EqSat{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let program =
            tir_pdl::emit_egglog(&library.file, &prefix).map_err(PassError::InvalidRuleSet)?;
        if program.obligations.iter().any(|o| {
            library
                .bindings
                .iter()
                .filter(|(rule, _)| rule == &o.rule)
                .count()
                != 1
        }) {
            return Err(PassError::InvalidRuleSet(
                "unbound checked proof obligations".into(),
            ));
        }
        Ok(Self {
            library,
            prefix,
            iterations: 32,
            operation_cost: Arc::new(|kind| if kind == "builtin.muli" { 2 } else { 1 }),
        })
    }

    /// Bound rewriting iterations. Projection retains the original finite witnesses.
    pub fn with_iteration_limit(mut self, iterations: std::num::NonZeroUsize) -> Self {
        self.iterations = iterations.get();
        self
    }

    /// Set the marginal cost of each materialized scalar operation, including constants.
    /// Each concrete name is evaluated once per extraction; arguments and metadata cost zero.
    /// Weights must fit `i64`, otherwise running the pass returns an error.
    /// An unrepresentable total retains the original scope.
    pub fn with_operation_cost(
        mut self,
        cost: impl Fn(&str) -> u64 + Send + Sync + 'static,
    ) -> Self {
        self.operation_cost = Arc::new(cost);
        self
    }

    fn run_engine(
        &self,
        engine: &mut EGraph,
        root: OpId,
        program: &tir_pdl::CompiledEgglog,
        annotations: &[(&str, OpId)],
        static_folds: &[(OpId, crate::utils::APInt)],
    ) -> Result<bool, tir_egglog::Error> {
        let root_table = format!("{}Root", self.prefix);
        if engine
            .read(|state| state.table_subtype(&root_table))
            .is_none()
        {
            let sort = |name| {
                engine.get_sort_by_name(name).cloned().ok_or_else(|| {
                    tir_egglog::Error::Unsupported(format!("eqsat requires sort {name}"))
                })
            };
            let bigint = sort("BigInt")?;
            let string = sort("String")?;
            let integer = sort("i64")?;
            engine.add_pure_primitive(IntegerBits([
                bigint.clone(),
                string.clone(),
                string.clone(),
                string.clone(),
            ]));
            engine.add_pure_primitive(IntegerEval::Binary([
                string.clone(),
                bigint.clone(),
                string.clone(),
                string.clone(),
                string.clone(),
            ]));
            engine.add_pure_primitive(IntegerEval::Unary([
                string.clone(),
                bigint.clone(),
                string.clone(),
                string.clone(),
            ]));
            engine.add_pure_primitive(IntegerStorage::Bits(string.clone()));
            engine.add_pure_primitive(IntegerStorage::Width {
                bits: string.clone(),
                integer,
            });
            for primitive in [
                Bitwise::And(string.clone()),
                Bitwise::Or(string.clone()),
                Bitwise::Not {
                    width: bigint,
                    bits: string,
                },
            ] {
                engine.add_pure_primitive(primitive);
            }
            let mut source = program.source.clone();
            if !program.obligations.is_empty()
                && program.obligations.len() == self.library.file.items.len()
            {
                source += &tir_pdl::emit_egglog(&tir_pdl::File { items: Vec::new() }, &self.prefix)
                    .map_err(tir_egglog::Error::Unsupported)?
                    .source;
            }
            source += &replacement_protocol(engine, &self.prefix)?;
            source += &include_str!("../../defs/eqsat-scalar.egg").replace("EQ", &self.prefix);
            source += &include_str!("../../defs/eqsat-scope.egg").replace("EQ", &self.prefix);
            source +=
                &include_str!("../../defs/eqsat-private-memory.egg").replace("EQ", &self.prefix);
            source +=
                &include_str!("../../defs/eqsat-gamma-memory.egg").replace("EQ", &self.prefix);
            source +=
                &include_str!("../../defs/eqsat-theta-memory.egg").replace("EQ", &self.prefix);
            source += &include_str!("../../defs/eqsat-fp-motion.egg").replace("EQ", &self.prefix);
            source +=
                &include_str!("../../defs/eqsat-pointer-addresses.egg").replace("EQ", &self.prefix);
            source +=
                &include_str!("../../defs/eqsat-ordered-memory.egg").replace("EQ", &self.prefix);
            source += &include_str!("../../defs/eqsat-static-fold.egg").replace("EQ", &self.prefix);
            let commands = engine.parse_program("eqsat PDL", &source)?;
            engine.run_program(commands)?;
            for phase in [
                "bound-request-types",
                "bound-requests",
                "scopes",
                "assumptions",
                "project",
                "rewrite",
                "output",
                "choose",
                "scoped-edits",
                "exact-edits",
                "planned-erases",
                "scalar-scope",
                "scalar-check",
                "scalar-validate",
                "scalar-complete",
                "scalar-ports",
                "scalar-bundle",
                "scalar-slots",
                "scalar-pool",
                "scalar-witness",
                "scalar-assignment",
                "scalar-legality",
                "scalar-edits",
            ] {
                engine.set_ruleset_colored(&format!("{}{phase}", self.prefix));
            }
        }
        engine.update(|mut state| {
            state.add("Scope", i64::from(root.raw()))?;
            Ok(())
        })?;
        engine.run_program(vec![tir_egglog::ast::Command::RunSchedule(
            tir_egglog::Schedule::saturate(tir_egglog::Schedule::run("ir-index")),
        )])?;
        engine.step_rules(&format!("{}clearfacts", self.prefix))?;
        let root_value = engine.database().base(i64::from(root.raw()));
        let root_id = engine.database().table(&root_table)?;
        engine.database_mut().insert(root_id, &[root_value])?;
        let base_facts = (|| {
            for phase in ["base-needed", "facts"] {
                engine.run_program(vec![tir_egglog::ast::Command::RunSchedule(
                    tir_egglog::Schedule::saturate(tir_egglog::Schedule::run(&format!(
                        "{}{phase}",
                        self.prefix
                    ))),
                )])?;
            }
            Ok::<_, tir_egglog::Error>(())
        })();
        engine.database_mut().remove(root_id, &[root_value]);
        base_facts?;
        let mark = engine.database().mark();
        let source = engine.database_mut().color(&[]);
        static INVOCATION: AtomicU64 = AtomicU64::new(1);
        let invocation = INVOCATION.fetch_add(1, Ordering::Relaxed) as i64;
        let mut colors = vec![source];
        let result: Result<(), tir_egglog::Error> = (|| {
            let db = engine.database_mut();
            let table = db.table(&root_table)?;
            db.insert_in(source, table, &[db.base(i64::from(root.raw()))])?;
            let identity = db.table(&format!("{}Invocation", self.prefix))?;
            db.insert_in(
                source,
                identity,
                &[db.base(invocation), db.base(i64::from(root.raw()))],
            )?;
            let db = engine.database_mut();
            for &(property, op) in annotations {
                let table = db.table(&format!("{}{property}", self.prefix))?;
                db.insert_in(source, table, &[db.base(i64::from(op.raw()))])?;
            }
            let table = db.table(&format!("{}StaticIntegerFold", self.prefix))?;
            for (op, bits) in static_folds {
                let width = i64::try_from(bits.width()).map_err(|_| {
                    tir_egglog::Error::Unsupported("static integer fold width exceeds i64".into())
                })?;
                db.insert_in(
                    source,
                    table,
                    &[
                        db.base(i64::from(op.raw())),
                        db.base(tir_egglog::sort::BigInt::new(width)),
                        db.base(format!("{bits:x}")),
                    ],
                )?;
            }
            for phase in [
                "scalar-scope",
                "scalar-check",
                "scalar-validate",
                "scalar-complete",
                "scalar-ports",
            ] {
                self.saturate(engine, phase)?;
            }
            engine.run_program(vec![tir_egglog::ast::Command::RunSchedule(
                tir_egglog::Schedule::saturate(tir_egglog::Schedule::run(&format!(
                    "{}scopes",
                    self.prefix
                ))),
            )])?;
            let db = engine.database_mut();
            let scopes = db.table(&format!("{}ScopeRecord", self.prefix))?;
            let parents = db.table(&format!("{}ParentRecord", self.prefix))?;
            let seed = db.table(&format!("{}Seed", self.prefix))?;
            colors.extend(db.scope_colors(source, scopes, parents, seed)?);
            for phase in ["scalar-complete", "assumptions", "project", "scalar-ports"] {
                self.saturate(engine, phase)?;
            }
            for &color in &colors[1..] {
                let db = engine.database();
                let seed = db.table(&format!("{}Seed", self.prefix))?;
                let roots = db
                    .own_rows_in(color, seed)
                    .map(|row| row[0])
                    .collect::<Vec<_>>();
                self.rank_scalar(
                    engine,
                    color,
                    &roots,
                    "ScalarPort",
                    "ScalarPortRank",
                    "ScalarPortCount",
                )?;
            }
            for phase in [
                "scalar-bundle",
                "scalar-slots",
                "scalar-pool",
                "rewrite",
                "output",
            ] {
                let run = tir_egglog::Schedule::run(&format!("{}{phase}", self.prefix));
                let schedule = if phase == "rewrite" {
                    tir_egglog::Schedule::repeat(self.iterations, run)
                } else {
                    tir_egglog::Schedule::saturate(run)
                };
                engine.run_program(vec![tir_egglog::ast::Command::RunSchedule(schedule)])?;
            }
            for &color in &colors[1..] {
                let db = engine.database();
                let output = db.table(&format!("{}Output", self.prefix))?;
                let extractor = Extractor::new_in(db, color);
                let selections = db
                    .own_rows_in(color, output)
                    .map(|row| {
                        let (table, inputs) = extractor.best_enode(row[1])?;
                        let name = db.schema().table(table).name();
                        let suffix = name.strip_prefix(&self.prefix).ok_or_else(|| {
                            tir_egglog::Error::Unsupported(
                                "eqsat extracted a non-semantic constructor".into(),
                            )
                        })?;
                        let picked = db.table(&format!("{}Pick{suffix}", self.prefix))?;
                        let fields = std::iter::once(row[0]).chain(inputs).collect::<Vec<_>>();
                        Ok((picked, fields))
                    })
                    .collect::<Result<Vec<_>, tir_egglog::Error>>()?;
                drop(extractor);
                for (table, fields) in selections {
                    engine.database_mut().insert_in(color, table, &fields)?;
                }
            }
            engine.step_rules(&format!("{}choose", self.prefix))?;
            engine.step_rules(&format!("{}scoped-edits", self.prefix))?;
            Ok(())
        })();
        let result = result.and_then(|()| {
            for &color in &colors[1..] {
                self.run_scalar(engine, color)?;
            }
            Ok(())
        });
        if result.is_err() {
            let db = engine.database_mut();
            db.drop_color(source);
            let publication = db.color(&[]);
            db.adopt_rows(publication, &[], mark);
        }
        result?;
        self.saturate(engine, "planned-erases")?;
        self.saturate(engine, "exact-edits")?;
        self.publish(engine, mark, &colors)
    }
}

fn static_integer_fold(context: &Context, op: &crate::OpHandle) -> Option<crate::utils::APInt> {
    use crate::{
        Operation,
        sem::{SemGraph, SymKind, SymPayload, Value},
    };

    let results = op.results();
    if !op.operands().is_empty() || results.len() != 1 || !op.regions().is_empty() {
        return None;
    }
    let definition = op.definition()?;
    if definition.poison.is_some()
        || definition.requires.is_some()
        || op.has_interface::<dyn crate::HasResourceSemantics>()
        || op
            .clone()
            .as_interface::<dyn crate::ResourceEffects>()
            .is_some_and(|effects| !effects.resource_effects().is_empty())
    {
        return None;
    }
    let ty = crate::builtin::IntegerType::view(context, context.get_value(results[0]).ty())?;
    let Value::Int(bits) = op
        .clone()
        .as_interface::<dyn crate::ConstantFold>()?
        .fold(&[])?
    else {
        return None;
    };
    if bits.width() != ty.width() {
        return None;
    }
    let mut graph = SemGraph::new();
    let root = op.semantic_expr(&mut graph)?;
    (graph.len() == 1
        && *graph.get_node(root) == SymKind::Constant
        && graph.get_leaf_data(root) == Some(&SymPayload::Int(bits.clone())))
    .then_some(bits)
}

crate::register_pass!(EqSatPass, "eqsat");

impl Pass for EqSatPass {
    fn name(&self) -> &'static str {
        "eqsat"
    }

    fn run(
        &mut self,
        root: OpId,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let program = tir_pdl::emit_egglog(&self.library.file, &self.prefix)
            .map_err(PassError::InvalidRuleSet)?;
        for _ in 0..self.iterations {
            let mut annotations = Vec::new();
            let mut static_folds = Vec::new();
            for id in context.subtree_ops(&[root]) {
                let op = context.get_op(id);
                if let Some(bits) = static_integer_fold(context, &op) {
                    static_folds.push((id, bits));
                }
                if op
                    .clone()
                    .as_interface::<dyn crate::Speculatable>()
                    .is_some_and(|interface| interface.is_speculatable())
                {
                    annotations.push(("CanSpeculate", id));
                    if op
                        .clone()
                        .as_interface::<dyn crate::ResourceEffects>()
                        .is_some_and(|interface| interface.resource_effects().is_empty())
                    {
                        annotations.push(("EffectFree", id));
                    }
                }
                if (op.has_interface::<dyn crate::MemoryRead>()
                    || op.has_interface::<dyn crate::MemoryWrite>()
                    || op.has_interface::<dyn crate::PromotableAllocation>())
                    && crate::sem::memory::ordinary_layout(
                        crate::layout::DataLayout::for_op(context, id).as_ref(),
                        false,
                    )
                {
                    annotations.push(("OrdinaryLayout", id));
                }
            }
            let native_changed = {
                let mut engine = context
                    .egraph_with_rules()
                    .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?;
                self.run_engine(&mut engine, root, &program, &annotations, &static_folds)
                    .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?
            };
            let checked_changed = self
                .run_checked(context, root, &program)
                .map_err(|error| PassError::InvalidRuleSet(error.to_string()))?;
            if !native_changed && !checked_changed {
                break;
            }
        }
        crate::verify_op_tree(context, root).map_err(|error| PassError::InvalidIR {
            pass: "eqsat",
            error,
        })
    }
}

// The protocol is schema typed. Only selected deltas and their constructor
// closure leave the temporary semantic color.
const REPLACEMENT_TABLES: &[&str] = &[
    "OpKind",
    "OpPlacement",
    "OpLocation",
    "GraphLayout",
    "Operand",
    "Successor",
    "SuccessorArg",
    "Yield",
    "ValueDefinition",
    "DeclaredType",
    "ValueType",
    "EntryBinding",
    "FeedbackBinding",
    "ExitBinding",
    "Attribute",
    "RegionOwner",
    "RegionSlot",
    "Mode",
    "Entry",
    "Gamma",
    "GammaPredicate",
    "GammaArm",
    "GammaDefaultArm",
];

fn replacement_protocol(engine: &EGraph, prefix: &str) -> Result<String, tir_egglog::Error> {
    use std::fmt::Write as _;
    let db = engine.database();
    let mut declarations = format!(
        "(datatype {prefix}Plan ({prefix}Proposal i64 i64 i64 i64))\n\
         (relation {prefix}Invocation (i64 i64))\n"
    );
    for &name in REPLACEMENT_TABLES {
        let table = db.schema().table(db.table(name)?);
        let sorts = table
            .columns()
            .iter()
            .map(|&sort| db.schema().sort(sort).name())
            .collect::<Vec<_>>();
        for (action, count) in [("Erase", table.key_arity()), ("Put", sorts.len())] {
            writeln!(
                declarations,
                "(relation {prefix}{action}{name} ({prefix}Plan {}))",
                sorts[..count].join(" ")
            )
            .map_err(|error| tir_egglog::Error::Unsupported(error.to_string()))?;
        }
    }
    Ok(declarations)
}

struct ScalarCost(Vec<(String, u64)>);

impl tir_egglog::extract::DagCostModel<u64> for ScalarCost {
    fn base_value_cost(&self, _: &tir_egglog::Database, _: &ArcSort, _: Value) -> u64 {
        0
    }

    fn enode_cost(
        &self,
        _: &tir_egglog::Database,
        table: &tir_egglog::TableSchema,
        _: &tir_egglog::Enode<'_>,
    ) -> u64 {
        self.0
            .iter()
            .find(|(name, _)| name == table.name())
            .map_or(0, |(_, cost)| *cost)
    }
}

impl EqSatPass {
    fn saturate(&self, engine: &mut EGraph, phase: &str) -> Result<(), tir_egglog::Error> {
        engine.run_program(vec![tir_egglog::ast::Command::RunSchedule(
            tir_egglog::Schedule::saturate(tir_egglog::Schedule::run(&format!(
                "{}{phase}",
                self.prefix
            ))),
        )])?;
        Ok(())
    }

    // These are numeric protocol projections, not IR matching or scheduling.
    fn rank_scalar(
        &self,
        engine: &mut EGraph,
        color: tir_egglog::Color,
        roots: &[Value],
        source: &str,
        target: &str,
        count: &str,
    ) -> Result<(), tir_egglog::Error> {
        let db = engine.database();
        let table = db.table(&format!("{}{source}", self.prefix))?;
        let mut ranked = Vec::new();
        for &root in roots {
            let mut rows = db
                .rows_in(color, table)
                .filter(|row| row[0] == root)
                .map(|row| row.values().collect::<Vec<_>>())
                .collect::<Vec<_>>();
            rows.sort_by_key(|row| {
                std::iter::once(row[row.len() - 1])
                    .chain(row[1..row.len() - 1].iter().copied())
                    .map(|value| db.base_value::<i64>(value))
                    .collect::<Vec<_>>()
            });
            let size = db.base(rows.len() as i64);
            ranked.push((count, vec![root, size]));
            for (rank, mut row) in rows.into_iter().enumerate() {
                row.pop();
                row.extend([db.base(rank as i64), size]);
                ranked.push((target, row));
            }
        }
        let db = engine.database_mut();
        for (name, fields) in ranked {
            db.insert_in(color, db.table(&format!("{}{name}", self.prefix))?, &fields)?;
        }
        Ok(())
    }

    fn run_scalar(
        &self,
        engine: &mut EGraph,
        color: tir_egglog::Color,
    ) -> Result<(), tir_egglog::Error> {
        use tir_egglog::{Term, TermDag, ast::Literal, extract::GreedyDagExtractor};
        let db = engine.database();
        let pure = db.table(&format!("{}ScalarPure", self.prefix))?;
        let seed = db.table(&format!("{}Seed", self.prefix))?;
        let roots = db
            .own_rows_in(color, seed)
            .map(|row| row[0])
            .collect::<Vec<_>>();
        let frames = db
            .rows_in(color, pure)
            .filter(|row| roots.contains(&row[0]))
            .map(|row| (row[0], row[2]))
            .collect::<Vec<_>>();
        if frames.is_empty() {
            return Ok(());
        }
        let roots = frames.iter().map(|&(root, _)| root).collect::<Vec<_>>();
        let requests = db.table(&format!("{}ScalarCostRequest", self.prefix))?;
        let mut names = db
            .own_rows_in(color, requests)
            .map(|row| db.base_value::<String>(row[1]))
            .collect::<Vec<_>>();
        names.sort();
        names.dedup();
        let weights = names
            .into_iter()
            .map(|name| {
                let weight = i64::try_from((self.operation_cost)(&name)).map_err(|_| {
                    tir_egglog::Error::Unsupported("scalar operation cost exceeds i64".into())
                })?;
                Ok((name, weight))
            })
            .collect::<Result<Vec<_>, tir_egglog::Error>>()?;
        let db = engine.database_mut();
        let weight_table = db.table(&format!("{}ScalarWeight", self.prefix))?;
        for (name, weight) in weights {
            db.insert_in(color, weight_table, &[db.base(name), db.base(weight)])?;
        }
        self.rank_scalar(
            engine,
            color,
            &roots,
            "ScalarPool",
            "ScalarPoolRank",
            "ScalarPoolCount",
        )?;
        self.rank_scalar(
            engine,
            color,
            &roots,
            "ScalarSafePool",
            "ScalarSourceRank",
            "ScalarSourceCount",
        )?;
        self.saturate(engine, "scalar-witness")?;
        let db = engine.database();
        let costs = db.table(&format!("{}ScalarCtorCost", self.prefix))?;
        let cost_model = ScalarCost(
            db.own_rows_in(color, costs)
                .map(|row| {
                    (
                        db.base_value::<String>(row[0]),
                        db.base_value::<i64>(row[1]) as u64,
                    )
                })
                .collect(),
        );
        let sort = engine
            .get_sort_by_name(&format!("{}Outputs", self.prefix))
            .cloned()
            .ok_or_else(|| tir_egglog::Error::Unsupported("missing scalar output sort".into()))?;
        let ty_sort = engine
            .get_sort_by_name("TypeExpr")
            .cloned()
            .ok_or_else(|| tir_egglog::Error::Unsupported("missing structural type sort".into()))?;
        let bundles = db.table(&format!("{}ScalarBundle", self.prefix))?;
        let requested = db
            .own_rows_in(color, bundles)
            .map(|row| (sort.clone(), row[1]))
            .collect::<Vec<_>>();
        let extractor = GreedyDagExtractor::with_cost_model_in(db, color, &requested, cost_model)?;
        let type_extractor = Extractor::new_in(db, color);
        let mut records = Vec::new();
        for (root, ty) in frames {
            let Some(bundle) = db.lookup_in(color, bundles, &[root]) else {
                continue;
            };
            let mut dag = TermDag::default();
            let Some(witness) = extractor.extract_best_with_sort(&mut dag, bundle, sort.clone())
            else {
                continue;
            };
            let Ok(cost) = i64::try_from(witness.cost) else {
                continue;
            };
            let expected = type_extractor
                .extract_best_with_sort(&mut dag, ty, ty_sort.clone())
                .ok_or_else(|| {
                    tir_egglog::Error::Unsupported("scalar type has no finite witness".into())
                })?;
            records.push((
                "ScalarWitness",
                vec![
                    root,
                    db.base(witness.term as i64),
                    db.base(expected.term as i64),
                    db.base(cost),
                ],
            ));
            for id in 0..dag.size() {
                let node = db.base(id as i64);
                match dag.get(id) {
                    Term::App(name, children) => {
                        records
                            .push(("ScalarWitnessCtor", vec![root, node, db.base(name.clone())]));
                        for (slot, &child) in children.iter().enumerate() {
                            records.push((
                                "ScalarWitnessChild",
                                vec![root, node, db.base(slot as i64), db.base(child as i64)],
                            ));
                        }
                    }
                    Term::Lit(Literal::Int(value)) => {
                        records.push(("ScalarWitnessInt", vec![root, node, db.base(*value)]))
                    }
                    Term::Lit(Literal::String(value)) => records.push((
                        "ScalarWitnessString",
                        vec![root, node, db.base(value.clone())],
                    )),
                    _ => {
                        return Err(tir_egglog::Error::Unsupported(
                            "unsupported scalar witness literal".into(),
                        ));
                    }
                }
            }
        }
        drop(extractor);
        drop(type_extractor);
        let db = engine.database_mut();
        for (suffix, fields) in records {
            db.insert_in(
                color,
                db.table(&format!("{}{suffix}", self.prefix))?,
                &fields,
            )?;
        }
        self.saturate(engine, "scalar-witness")?;
        self.rank_scalar(
            engine,
            color,
            &roots,
            "ScalarChosenOp",
            "ScalarNodeRank",
            "ScalarNodeCount",
        )?;
        for phase in ["scalar-assignment", "scalar-legality", "scalar-edits"] {
            self.saturate(engine, phase)?;
        }
        Ok(())
    }
}

struct CheckedCandidate {
    rule: usize,
    captures: Vec<Capture>,
    proof: CheckedRewrite,
}

impl EqSatPass {
    fn run_checked(
        &self,
        context: &Context,
        root: OpId,
        program: &tir_pdl::CompiledEgglog,
    ) -> Result<bool, tir_egglog::Error> {
        let invalid = |message: &str| tir_egglog::Error::Unsupported(message.into());
        if program.obligations.is_empty() {
            return Ok(false);
        }
        let mut engine = context.egraph_with_rules()?;
        engine.run_program(vec![tir_egglog::ast::Command::RunSchedule(
            tir_egglog::Schedule::saturate(tir_egglog::Schedule::run("ir-index")),
        )])?;
        let db = engine.database_mut();
        let color = db.color(&[]);
        db.insert_in(
            color,
            db.table(&format!("{}RequestScope", self.prefix))?,
            &[db.base(i64::from(root.raw()))],
        )?;
        // Keep request derivations private to this source revision.
        let requests = (|| {
            for phase in ["bound-request-types", "bound-requests"] {
                self.saturate(&mut engine, phase)?;
            }
            let db = engine.database();
            let within = db.table("Within")?;
            let mut requests = Vec::new();
            for (index, obligation) in program.obligations.iter().enumerate() {
                for row in db.own_rows_in(color, db.table(&obligation.relation)?) {
                    let id = row[obligation.root];
                    if !db
                        .rows(within)
                        .any(|r| r[0] == db.base(i64::from(root.raw())) && r[1] == id)
                    {
                        continue;
                    }
                    let captures = obligation
                        .captures
                        .iter()
                        .enumerate()
                        .map(|(column, descriptor)| {
                            let number = u32::try_from(db.base_value::<i64>(row[column]))
                                .map_err(|_| invalid("invalid captured entity"))?;
                            Ok(match descriptor.role {
                                tir_pdl::CaptureRole::Operation => {
                                    Capture::Operation(OpId::from_number(number))
                                }
                                tir_pdl::CaptureRole::Data => {
                                    Capture::Data(crate::ValueId::from_number(number))
                                }
                                tir_pdl::CaptureRole::Dependency => {
                                    Capture::Dependency(crate::ValueId::from_number(number))
                                }
                            })
                        })
                        .collect::<Result<Vec<_>, tir_egglog::Error>>()?;
                    requests.push((index, captures));
                }
            }
            Ok::<_, tir_egglog::Error>(requests)
        })();
        // Request heads contain only captured entity data, never new terms.
        engine.database_mut().drop_color(color);
        drop(engine);
        let requests = requests?;
        let checked = (|| {
            let mut checked = Vec::new();
            let mut weights = Vec::<(String, i64)>::new();
            for (rule, captures) in requests {
                let obligation = &program.obligations[rule];
                let Capture::Operation(covered) = captures[obligation.root] else {
                    return Err(invalid("checked root is not an operation"));
                };
                if !context.has_operation(covered) {
                    return Err(invalid("stale checked request root"));
                }
                let operation = context.get_op(covered);
                let name = format!("{}.{}", operation.dialect(), operation.name());
                let cost = if let Some((_, cost)) = weights.iter().find(|(known, _)| known == &name)
                {
                    *cost
                } else {
                    let cost = i64::try_from((self.operation_cost)(&name))
                        .map_err(|_| invalid("checked operation cost exceeds i64"))?;
                    weights.push((name, cost));
                    cost
                };
                if cost == 0 {
                    continue;
                }
                let checker = self
                    .library
                    .bindings
                    .iter()
                    .find(|(name, _)| name == &obligation.rule)
                    .ok_or_else(|| invalid("unbound checked obligation"))?;
                let Some(proof) = checker.1(context, obligation, &captures) else {
                    continue;
                };
                if captures[obligation.root] != Capture::Operation(proof.covered_root()) {
                    return Err(invalid("proof candidate mismatch"));
                }
                let (old, new) = obligation.requested_mapping;
                if !proof.mappings().iter().any(|&(a, b)| {
                    captures[old] == Capture::Data(a) && captures[new] == Capture::Data(b)
                }) {
                    return Err(invalid("proof does not establish requested RHS"));
                }
                if !context.has_operation(proof.covered_root()) {
                    return Err(invalid("stale checked proof root"));
                }
                let results = context.get_op(proof.covered_root()).results().to_vec();
                if proof.mappings().len() != results.len()
                    || results.iter().any(|result| {
                        proof
                            .mappings()
                            .iter()
                            .filter(|(old, _)| old == result)
                            .count()
                            != 1
                    })
                {
                    return Err(invalid("incomplete checked result bundle"));
                }
                checked.push(CheckedCandidate {
                    rule,
                    captures,
                    proof,
                });
            }
            Ok::<_, tir_egglog::Error>(checked)
        })();
        let checked = checked?;
        // Revalidate immediately before taking the exclusive immutable source borrow.
        let valid = checked
            .iter()
            .all(|candidate| candidate.proof.validate(context));
        let mut engine = context.egraph_with_rules()?;
        let color = engine.database_mut().color(&[]);
        let result = (|| {
            if !valid {
                return Err(invalid("stale checked source"));
            }
            {
                let db = engine.database_mut();
                db.insert_in(
                    color,
                    db.table(&format!("{}RequestScope", self.prefix))?,
                    &[db.base(i64::from(root.raw()))],
                )?;
            }
            for phase in ["bound-request-types", "bound-requests"] {
                self.saturate(&mut engine, phase)?;
            }
            for candidate in &checked {
                let db = engine.database();
                let obligation = &program.obligations[candidate.rule];
                let current = db
                    .own_rows_in(color, db.table(&obligation.relation)?)
                    .any(|row| {
                        candidate
                            .captures
                            .iter()
                            .enumerate()
                            .all(|(index, capture)| {
                                let number = match capture {
                                    Capture::Operation(op) => op.number(),
                                    Capture::Data(value) | Capture::Dependency(value) => {
                                        value.number()
                                    }
                                };
                                row[index] == db.base(i64::from(number))
                            })
                    });
                if !current {
                    return Err(invalid("checked source request changed"));
                }
            }
            Ok::<_, tir_egglog::Error>(())
        })();
        engine.database_mut().drop_color(color);
        drop(engine);
        result?;
        let mut checked = checked;
        checked.sort_by_key(|candidate| candidate.proof.covered_root());
        checked.dedup_by_key(|candidate| candidate.proof.covered_root());
        let replacements = checked
            .iter()
            .flat_map(|candidate| candidate.proof.mappings().iter().copied())
            .collect::<Vec<_>>();
        let resolved = replacements
            .iter()
            .map(|&(old, new)| {
                let mut target = new;
                for _ in 0..replacements.len() {
                    match replacements.iter().find(|(old, _)| *old == target) {
                        Some(&(_, next)) => target = next,
                        None => return Ok((old, target)),
                    }
                }
                Err(invalid("cyclic checked result mappings"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (old, new) in resolved {
            context.replace_value_uses(old, new);
            for (region, slot) in context.result_uses(old) {
                let mut values = context.get_region(region).results();
                values[slot] = new;
                context.set_region_results(region, values);
            }
        }
        let changed = !checked.is_empty();
        for candidate in checked {
            context
                .erase_op(&crate::OperationRef::new(
                    context.get_op(candidate.proof.covered_root()),
                ))
                .map_err(|error| invalid(&error.to_string()))?;
        }
        Ok(changed)
    }
}

impl EqSatPass {
    fn publish(
        &self,
        engine: &mut EGraph,
        mark: u32,
        colors: &[tir_egglog::Color],
    ) -> Result<bool, tir_egglog::Error> {
        use tir_egglog::TermDag;

        let db = engine.database();
        let mut dag = TermDag::default();
        let mut erased = Vec::new();
        let mut writes = Vec::new();
        for &color in colors {
            let mut selected = Vec::new();
            for authorization in ["ScalarAuthorized", "ScopedAuthorized"] {
                let authorized = db.table(&format!("{}{authorization}", self.prefix))?;
                selected.extend(db.own_rows_in(color, authorized).map(|row| row[0]));
            }
            let extractor = Extractor::new_in(db, color);
            for &name in REPLACEMENT_TABLES {
                let target = db.table(name)?;
                let schema = db.schema().table(target);
                for (action, put) in [("Erase", false), ("Put", true)] {
                    let relation = db.table(&format!("{}{action}{name}", self.prefix))?;
                    for row in db
                        .own_rows_in(color, relation)
                        .filter(|row| selected.contains(&row[0]))
                    {
                        let fields = row.values().skip(1).collect::<Vec<_>>();
                        if !put {
                            erased.push((target, fields));
                            continue;
                        }
                        let terms = fields
                            .iter()
                            .zip(schema.columns())
                            .map(|(&value, &sort)| {
                                let sort = db.schema().sort(sort).arcsort().clone();
                                let term = extractor
                                    .extract_best_with_sort(&mut dag, value, sort)
                                    .ok_or_else(|| {
                                        tir_egglog::Error::Unsupported(
                                            "selected replacement field has no finite term".into(),
                                        )
                                    })?;
                                Ok(dag.term_to_expr(&term.term, tir_egglog::span!()))
                            })
                            .collect::<Result<Vec<_>, tir_egglog::Error>>()?;
                        writes.push((target, terms));
                    }
                }
            }
        }
        let changed = !erased.is_empty() || !writes.is_empty();
        engine.database_mut().adopt_scoped_rows(colors, &[], mark)?;
        let mut resolved = Vec::new();
        for (table, fields) in writes {
            let fields = fields
                .iter()
                .map(|expr| engine.eval_expr(expr).map(|(_, value)| value))
                .collect::<Result<Vec<_>, _>>()?;
            resolved.push((table, fields));
        }
        let db = engine.database_mut();
        let mark = db.mark();
        let publication = db.color(&[]);
        let mut tables = Vec::new();
        let result = (|| {
            for (table, key) in erased {
                db.remove_in(publication, table, &key);
                tables.push(table);
            }
            for (table, fields) in &resolved {
                let arity = db.schema().table(*table).key_arity();
                db.remove_in(publication, *table, &fields[..arity]);
                tables.push(*table);
            }
            for (table, fields) in resolved {
                db.insert_in(publication, table, &fields)?;
            }
            Ok(())
        })();
        if result.is_err() {
            tables.clear();
        }
        tables.sort_unstable();
        tables.dedup();
        db.adopt_rows(publication, &tables, mark);
        result.map(|()| changed)
    }
}
