use crate::BlockHandle;

use linkme::distributed_slice;

use crate::{Context, OpHandle, OpId, Operation, RegionKind, analysis::AnalysisManager};

/// A pass made available to the pipeline parser by name.
///
/// Backends and libraries contribute entries with [`crate::register_pass!`]; the opt
/// tool builds pipelines purely from this registry, so adding a pass never
/// requires touching the tool.
pub struct PassInfo {
    pub name: &'static str,
    /// Builds the pass from the text a pipeline spelled between `<` and `>`.
    pub ctor: fn(&str) -> Result<Box<dyn Pass>, String>,
}

/// Link-time registry of every pass reachable in the final binary.
#[distributed_slice]
pub static PASSES: [PassInfo];

/// Names of all registered passes, for help text and diagnostics.
pub fn registered_passes() -> Vec<&'static str> {
    let mut names: Vec<_> = PASSES.iter().map(|p| p.name).collect();
    names.sort_unstable();
    names
}

/// Register a pass under `name` so the pipeline parser can build it.
///
/// `ty` must implement [`Pass`] and expose a `new() -> Self` constructor. The
/// three-argument form registers a pass a pipeline may parameterise as
/// `name<args>`; `parse` turns that text into the pass.
#[macro_export]
macro_rules! register_pass {
    ($ty:ty, $name:expr) => {
        const _: () = {
            #[$crate::linkme::distributed_slice($crate::PASSES)]
            #[linkme(crate = $crate::linkme)]
            static REGISTRATION: $crate::PassInfo = $crate::PassInfo {
                name: $name,
                ctor: |args| {
                    if !args.is_empty() {
                        return ::std::result::Result::Err(format!(
                            "pass '{}' takes no arguments",
                            $name
                        ));
                    }
                    ::std::result::Result::Ok(::std::boxed::Box::new(<$ty>::new()))
                },
            };
        };
    };
    ($ty:ty, $name:expr, $parse:path) => {
        const _: () = {
            #[$crate::linkme::distributed_slice($crate::PASSES)]
            #[linkme(crate = $crate::linkme)]
            static REGISTRATION: $crate::PassInfo = $crate::PassInfo {
                name: $name,
                ctor: |args| {
                    $parse(args).map(|pass| {
                        ::std::boxed::Box::new(pass) as ::std::boxed::Box<dyn $crate::Pass>
                    })
                },
            };
        };
    };
}

/// Parse an MLIR-style pass pipeline into a [`PassManager`].
///
/// The grammar is a comma-separated list of elements:
///
/// ```text
/// element := ident ('<' args '>')? ('(' list ')')?
/// ```
///
/// A bare ident is a registered pass. An ident with `<args>` is a pass that
/// parses those arguments itself, as `inline<40,5>` does. An ident with a
/// parenthesised list nests that list inside every matching op, and the name
/// may be dialect-qualified (`func.func`) or bare (`func`). `fixpoint` is the
/// one reserved name: `fixpoint<3>(func.func(instcombine))` repeats its list up
/// to three times, or until a round changes nothing.
pub fn parse_pipeline(spec: &str) -> Result<PassManager, String> {
    let mut parser = PipelineParser {
        bytes: spec.as_bytes(),
        pos: 0,
    };
    let mut pm = PassManager::new();
    parser.parse_list(&mut pm)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err(format!(
            "unexpected '{}' in pass pipeline",
            &spec[parser.pos..]
        ));
    }
    Ok(pm)
}

fn parse_cap(args: Option<&str>) -> Result<u8, String> {
    let digits = args.ok_or_else(|| "expected '<cap>' after 'fixpoint'".to_string())?;
    let cap: u8 = digits
        .parse()
        .map_err(|_| format!("invalid fixpoint cap '{digits}'"))?;
    if cap == 0 {
        return Err("fixpoint cap must be at least 1".to_string());
    }
    Ok(cap)
}

struct PipelineParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl PipelineParser<'_> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn parse_ident(&mut self) -> Result<String, String> {
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let c = self.bytes[self.pos];
            if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err("expected a pass or op name".to_string());
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned())
    }

    fn parse_list(&mut self, pm: &mut PassManager) -> Result<(), String> {
        loop {
            self.parse_element(pm)?;
            self.skip_ws();
            if self.pos < self.bytes.len() && self.bytes[self.pos] == b',' {
                self.pos += 1;
                continue;
            }
            return Ok(());
        }
    }

    fn parse_args(&mut self) -> Result<Option<String>, String> {
        if self.pos >= self.bytes.len() || self.bytes[self.pos] != b'<' {
            return Ok(None);
        }
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.bytes.len() && self.bytes[self.pos] != b'>' {
            self.pos += 1;
        }
        if self.pos == self.bytes.len() {
            return Err("missing '>' in pass arguments".to_string());
        }
        let args = String::from_utf8_lossy(&self.bytes[start..self.pos]).into_owned();
        self.pos += 1;
        Ok(Some(args))
    }

    fn parse_element(&mut self, pm: &mut PassManager) -> Result<(), String> {
        self.skip_ws();
        let name = self.parse_ident()?;
        let args = self.parse_args()?;
        self.skip_ws();
        let opens_list = self.pos < self.bytes.len() && self.bytes[self.pos] == b'(';

        if name == "fixpoint" {
            let cap = parse_cap(args.as_deref())?;
            if !opens_list {
                return Err("expected '(' after 'fixpoint<cap>'".to_string());
            }
            return self.parse_nested(pm.fixpoint(cap));
        }
        if opens_list {
            if args.is_some() {
                return Err(format!("op nesting '{name}' takes no arguments"));
            }
            return self.parse_nested(pm.nest_parsed(name));
        }
        let info = PASSES
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("unknown pass '{name}'"))?;
        let args = args.unwrap_or_default();
        let pass = (info.ctor)(&args)?;
        let ctor = info.ctor;
        pm.add_replicable(
            pass,
            Box::new(move || (ctor)(&args).expect("the arguments built the pass once")),
        );
        Ok(())
    }

    fn parse_nested(&mut self, nested: &mut PassManager) -> Result<(), String> {
        self.pos += 1;
        self.parse_list(nested)?;
        self.skip_ws();
        if self.pos >= self.bytes.len() || self.bytes[self.pos] != b')' {
            return Err("missing ')' in pass pipeline".to_string());
        }
        self.pos += 1;
        Ok(())
    }
}

#[derive(Debug)]
pub enum PassError {
    MissingBlock(&'static str),
    /// The pass walks one body kind and the operation holds the other.
    RegionKind {
        pass: &'static str,
        op: String,
        expected: RegionKind,
    },
    InvalidRuleSet(String),
    RewriteFailed(OpId),
    InvalidIR {
        pass: &'static str,
        error: crate::Error,
    },
    /// Two callables' tasks of one epoch wrote the same base entity.
    OverlappingEdits(String),
}

impl std::fmt::Display for PassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PassError::MissingBlock(name) => {
                write!(f, "operation '{name}' does not have a parent block")
            }
            PassError::RegionKind { pass, op, expected } => {
                let (wanted, held) = if *expected == RegionKind::Nodes {
                    ("an unordered", "an ordered")
                } else {
                    ("an ordered", "an unordered")
                };
                write!(
                    f,
                    "pass '{pass}' runs on {wanted} body, but {op} holds {held} one"
                )
            }
            PassError::InvalidRuleSet(message) => write!(f, "invalid rule set: {message}"),
            PassError::RewriteFailed(op) => write!(f, "failed to rewrite op {op:?}"),
            PassError::InvalidIR { pass, error } => {
                write!(f, "pass '{pass}' produced invalid IR: {error:?}")
            }
            PassError::OverlappingEdits(conflict) => {
                write!(f, "function tasks of one epoch overlap: {conflict}")
            }
        }
    }
}

impl std::error::Error for PassError {}

#[derive(Debug, Clone, Copy)]
/// Selects the operation kind on which a pass runs.
///
/// Operation targets must be constructed from an operation type:
///
/// ```compile_fail
/// let _ = tir::PassTarget::Operation("func.func");
/// ```
pub enum PassTarget {
    Any,
    Operation(OperationTarget),
}

#[derive(Debug, Clone, Copy)]
/// A type-safe operation target stored by [`PassTarget`]. `kind` names the
/// body kind the pass walks; a pass reading either leaves it `None`.
pub struct OperationTarget {
    dialect: &'static str,
    name: &'static str,
    kind: Option<RegionKind>,
}

impl PassTarget {
    /// Targets operations of type `T`, whichever kind of body they hold.
    pub fn operation<T: Operation>() -> Self {
        Self::Operation(OperationTarget {
            dialect: T::dialect(),
            name: T::name(),
            kind: None,
        })
    }

    /// Targets operations of type `T` whose regions are all of `kind`, `Blocks`
    /// or `Nodes`; the pass manager refuses to run the pass on one holding the
    /// other kind.
    pub fn operation_on<T: Operation>(kind: RegionKind) -> Self {
        assert_ne!(
            kind,
            RegionKind::Any,
            "a pass walking either kind declares none"
        );
        Self::Operation(OperationTarget {
            dialect: T::dialect(),
            name: T::name(),
            kind: Some(kind),
        })
    }

    fn matches(&self, op: &OpHandle) -> bool {
        match self {
            PassTarget::Any => true,
            PassTarget::Operation(target) => op.is_name(target.dialect, target.name),
        }
    }

    /// The body kind the target asks for when `op` holds a region of the other
    /// kind.
    fn kind_mismatch(&self, context: &Context, op: &OpHandle) -> Option<RegionKind> {
        let PassTarget::Operation(OperationTarget {
            kind: Some(kind), ..
        }) = self
        else {
            return None;
        };
        let wants_nodes = *kind == RegionKind::Nodes;
        op.regions()
            .iter()
            .any(|&region| context.get_region(region).is_nodes() != wants_nodes)
            .then_some(*kind)
    }
}

#[derive(Clone)]
pub struct OperationRef {
    op: OpHandle,
}

impl OperationRef {
    pub fn new(op: OpHandle) -> Self {
        Self { op }
    }

    pub fn op(&self) -> &OpHandle {
        &self.op
    }

    pub fn name(&self) -> crate::OperationName {
        self.op.name()
    }

    /// Returns whether the referenced operation has type `T`.
    pub fn is<T: Operation>(&self) -> bool {
        self.op.is::<T>()
    }

    pub fn as_op<T: Operation>(&self) -> Option<T> {
        self.op.clone().as_op::<T>()
    }

    pub fn as_interface<I: ?Sized + 'static>(&self) -> Option<Box<I>> {
        self.op.clone().as_interface::<I>()
    }
}

pub trait Pass: Send {
    fn name(&self) -> &'static str;
    fn target(&self) -> PassTarget {
        PassTarget::Any
    }

    /// Run on `op`. A pass reports nothing about what it changed: every edit
    /// bumps the version stamps of the ops it touched (see
    /// [`Context::op_version`]), which is what invalidates cached analyses and
    /// triggers post-pass verification.
    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError>;

    /// Whether a run is a function of the operation it runs on and nothing
    /// else in the module. The pass manager then skips a run on an operation
    /// the pass already left unchanged, for as long as nothing edits it.
    fn settles(&self) -> bool {
        false
    }
}

impl Context {
    /// Record that `target` became `new`, so [`refreshed`] can follow a
    /// pipeline root across the replacement.
    fn record_replacement(&self, target: &OperationRef, new: OpId) {
        self.record_replaced_op(target.op.id, new);
    }

    /// The block holding `target`, read live from the context.
    fn block_of(&self, target: &OperationRef) -> Result<BlockHandle, PassError> {
        self.parent_block(target.op.id)
            .map(|block| self.get_block(block))
            .ok_or(PassError::MissingBlock(target.name().as_str()))
    }

    /// Move everything `block` holds from `at` onward into a fresh block, which
    /// is returned detached: the caller decides where in a region it belongs.
    pub fn split_block(&self, block: crate::BlockId, at: usize) -> BlockHandle {
        let source = self.get_block(block);
        let tail = source.op_ids().split_off(at);
        let split = self.create_block(vec![]);
        for op in tail {
            source.remove_op(op);
            split.append(op);
        }
        split
    }

    /// Move every block of `source` to the end of `destination`, emptying
    /// `source`.
    pub fn splice_region(&self, source: crate::RegionId, destination: crate::RegionId) {
        let source = self.get_region(source);
        let destination = self.get_region(destination);
        for block in source
            .iter(self.clone())
            .map(|block| block.id())
            .collect::<Vec<_>>()
        {
            source.remove_block(block);
            destination.add_block(block);
        }
    }

    /// Detach `block` from the region holding it. The caller has already erased
    /// the operations it held; nothing may name it as a successor.
    pub fn erase_block(&self, block: crate::BlockId) -> bool {
        match self.parent_region(block) {
            Some(region) => self.get_region(region).remove_block(block),
            None => false,
        }
    }

    pub fn replace_op(
        &self,
        target: &OperationRef,
        new_op: &dyn Operation,
    ) -> Result<(), PassError> {
        let replaced = match self.parent_nodes_region(target.op.id) {
            Some(region) => {
                self.remove_from_region(region, target.op.id);
                self.add(region, new_op.id());
                true
            }
            None => self.block_of(target)?.replace_op(target.op.id, new_op.id()),
        };
        if replaced {
            self.record_replacement(target, new_op.id());
            // Rewrite SSA uses of the old results to the new op's results when the
            // shapes line up, so consumers don't dangle on the erased op's values.
            let new_results = self.get_op(new_op.id()).results().to_vec();
            if new_results.len() == target.op.results().len() {
                for (old, new) in target.op.results().iter().zip(new_results.iter()) {
                    self.replace_value_uses(*old, *new);
                }
            }
            // Drop the old op and what it owns so nothing lingers as a phantom,
            // except the values the replacement adopted as its own results.
            self.remove_operation_except(target.op.id, &new_results);
            Ok(())
        } else {
            Err(PassError::RewriteFailed(target.op.id))
        }
    }

    /// Erase `op` but keep the values it defined: a rewrite that hands them to
    /// another definition (selection replacing a covered op, call lowering, an
    /// allocator erasing a copy it granted one register at both ends) leaves
    /// the ops naming them intact.
    pub fn erase_op_keeping_results(&self, target: &OperationRef) -> Result<(), PassError> {
        let results = target.op.results().to_vec();
        if let Some(region) = self.parent_nodes_region(target.op.id) {
            self.remove_from_region(region, target.op.id);
            self.remove_operation_except(target.op.id, &results);
            return Ok(());
        }
        let block = self.block_of(target)?;
        if block.remove_op(target.op.id) {
            self.remove_operation_except(target.op.id, &results);
            Ok(())
        } else {
            Err(PassError::RewriteFailed(target.op.id))
        }
    }

    /// [`Context::replace_op`] keeping the values `target` defined. A function
    /// becoming an `asm.symbol` loses the SSA result that named it, but the
    /// calls in every other function of the module still name it: they resolve
    /// it by symbol name, and the value only has to stay readable until they do.
    pub fn replace_op_keeping_results(
        &self,
        target: &OperationRef,
        new_op: &dyn Operation,
    ) -> Result<(), PassError> {
        let replaced = match self.parent_nodes_region(target.op.id) {
            Some(region) => {
                self.remove_from_region(region, target.op.id);
                self.add(region, new_op.id());
                true
            }
            None => self.block_of(target)?.replace_op(target.op.id, new_op.id()),
        };
        if replaced {
            self.record_replacement(target, new_op.id());
            let results = target.op.results().to_vec();
            self.remove_operation_except(target.op.id, &results);
            Ok(())
        } else {
            Err(PassError::RewriteFailed(target.op.id))
        }
    }

    pub fn erase_op(&self, target: &OperationRef) -> Result<(), PassError> {
        if let Some(region) = self.parent_nodes_region(target.op.id) {
            self.remove_from_region(region, target.op.id);
            self.remove_operation(target.op.id);
            return Ok(());
        }
        let block = self.block_of(target)?;
        if block.remove_op(target.op.id) {
            self.remove_operation(target.op.id);
            Ok(())
        } else {
            Err(PassError::RewriteFailed(target.op.id))
        }
    }

    /// Insert `new_op` immediately before `target` in its block. Used when one
    /// source op lowers to several machine instructions (e.g. a sub-word sign
    /// extension becoming `slli` then `srai`): the feeding instructions are inserted
    /// ahead of the op that consumes them. Repeated calls before the same target
    /// preserve insertion order.
    pub fn insert_op_before(
        &self,
        target: &OperationRef,
        new_op: &dyn Operation,
    ) -> Result<(), PassError> {
        // An unordered region has no before: the op joins it, and what it
        // reads places it.
        if let Some(region) = self.parent_nodes_region(target.op.id) {
            self.add(region, new_op.id());
            return Ok(());
        }
        let block = self.block_of(target)?;
        let position = self
            .get_block(block.id())
            .op_ids()
            .iter()
            .position(|id| *id == target.op.id)
            .ok_or(PassError::RewriteFailed(target.op.id))?;
        block.insert(position, new_op.id());
        Ok(())
    }
}

/// Match an op against a nesting spec that is either a bare op name (`func`)
/// or a dialect-qualified name (`func.func`).
fn matches_op_name(op: &OpHandle, spec: &str) -> bool {
    match spec.split_once('.') {
        Some((dialect, name)) => op.is_name(dialect, name),
        None => op.name().as_str() == spec,
    }
}

/// `root` as it stands now: a pipeline holds its root across passes that erase
/// and replace it, and an [`OpHandle`] to an erased op reads as a panic. An
/// erased root is followed through the replacements the context recorded;
/// that is how selection's machine symbol takes over from the function it was
/// made of.
fn refreshed(context: &Context, root: &OperationRef) -> Option<OperationRef> {
    if root.op.is_live() {
        return Some(root.clone());
    }
    let mut id = context.replaced_op(root.op.id)?;
    while !context.has_operation(id) {
        id = context.replaced_op(id)?;
    }
    Some(OperationRef::new(context.get_op(id)))
}

/// Whether `op`'s tree has entered the machine layer — it holds a target
/// instruction, or one of the `asm` dialect's containers and pseudos. Machine
/// IR is not SSA (block-parameter destruction leaves a parameter defined once
/// per predecessor), so the contract [`crate::verify_op_tree`] checks does not
/// describe such a tree; [`crate::backend::verify_machine_ir`] states the one
/// that does.
fn is_machine_ir(context: &Context, op_id: OpId) -> bool {
    if !context.has_operation(op_id) {
        return false;
    }
    let instance = context.get_op(op_id);
    if instance.dialect().as_str() == "asm"
        || instance.has_interface::<dyn crate::backend::MachineInstruction>()
    {
        return true;
    }
    instance.regions().iter().any(|region_id| {
        context
            .get_region(*region_id)
            .iter(context.clone())
            .any(|block| {
                block
                    .op_ids()
                    .into_iter()
                    .any(|child| is_machine_ir(context, child))
            })
    })
}

/// Verify each edited subtree under `root`, instead of the whole tree: an op an
/// edit never reached still satisfies whatever it satisfied before. Subtrees the
/// edit later erased, and those outside `root`, are skipped.
fn verify_dirty_subtrees(
    context: &Context,
    root: OpId,
    dirty: &[OpId],
) -> Result<(), crate::Error> {
    let edited: Vec<OpId> = dirty
        .iter()
        .copied()
        .filter(|&op| context.has_operation(op) && encloses(context, root, op))
        .collect();
    crate::operation::verify_op_trees(context, &edited)
}

/// Whether `op` is `root` or sits somewhere under it.
fn encloses(context: &Context, root: OpId, op: OpId) -> bool {
    let mut current = Some(op);
    while let Some(id) = current {
        if id == root {
            return true;
        }
        current = context.parent_op(id);
    }
    false
}

/// Whether the pass manager re-verifies the IR after every pass that changed it.
/// On in debug builds; `TIR_VERIFY_IR` overrides either way.
fn ir_verification_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("TIR_VERIFY_IR").as_deref() {
        Ok("0") => false,
        Ok("1") => true,
        _ => cfg!(debug_assertions),
    })
}

/// Builds another instance of a pass, for a worker of its own.
type Replicator = Box<dyn Fn() -> Box<dyn Pass> + Send>;

enum PassNode {
    Pass {
        pass: Box<dyn Pass>,
        /// `None` for a pass added boxed: its nest then runs its callables
        /// one at a time.
        replicator: Option<Replicator>,
    },
    Nested {
        op_name: String,
        manager: PassManager,
    },
    Fixpoint {
        cap: u8,
        manager: PassManager,
    },
}

/// Whether the pipeline may commit: the top level owns the base, a callable's
/// task shares the context of the task and commits nothing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Exclusive,
    Shared,
}

pub struct PassManager {
    passes: Vec<PassNode>,
    workers: usize,
}

impl PassManager {
    pub fn new() -> Self {
        Self {
            passes: vec![],
            workers: 1,
        }
    }

    /// How many callables a nested pipeline runs at once. Every count reads
    /// the same epoch and commits in callable order, so the result is the
    /// same; what changes is wall time and the memory held while tasks run.
    pub fn set_workers(&mut self, workers: usize) {
        self.workers = workers.max(1);
        for node in &mut self.passes {
            if let PassNode::Nested { manager, .. } | PassNode::Fixpoint { manager, .. } = node {
                manager.set_workers(workers);
            }
        }
    }

    /// Add `pass`. It is `Clone` so a nest holding it can hand each worker a
    /// copy; a pass that cannot be cloned goes through
    /// [`PassManager::add_boxed_pass`].
    pub fn add_pass<P: Pass + Clone + 'static>(&mut self, pass: P) -> &mut Self {
        let template = pass.clone();
        self.add_replicable(Box::new(pass), Box::new(move || Box::new(template.clone())))
    }

    /// Add a pass that has no copy: the nest holding it runs its callables one
    /// at a time, each still in a context of its own.
    pub fn add_boxed_pass(&mut self, pass: Box<dyn Pass>) -> &mut Self {
        self.passes.push(PassNode::Pass {
            pass,
            replicator: None,
        });
        self
    }

    fn add_replicable(&mut self, pass: Box<dyn Pass>, replicator: Replicator) -> &mut Self {
        self.passes.push(PassNode::Pass {
            pass,
            replicator: Some(replicator),
        });
        self
    }

    /// Another instance of this pipeline, or `None` when a pass in it has no
    /// copy.
    fn replica(&self) -> Option<PassManager> {
        let passes = self
            .passes
            .iter()
            .map(|node| match node {
                PassNode::Pass { replicator, .. } => {
                    let replicator = replicator.as_ref()?;
                    Some(PassNode::Pass {
                        pass: replicator(),
                        replicator: None,
                    })
                }
                PassNode::Nested { op_name, manager } => Some(PassNode::Nested {
                    op_name: op_name.clone(),
                    manager: manager.replica()?,
                }),
                PassNode::Fixpoint { cap, manager } => Some(PassNode::Fixpoint {
                    cap: *cap,
                    manager: manager.replica()?,
                }),
            })
            .collect::<Option<Vec<_>>>()?;
        Some(PassManager {
            passes,
            workers: self.workers,
        })
    }

    /// Nest a sub-pipeline under every operation of type `T`.
    pub fn nest<T: Operation>(&mut self) -> &mut PassManager {
        self.nest_parsed(format!("{}.{}", T::dialect(), T::name()))
    }

    /// Repeat a sub-pipeline until it stops changing the IR, at most `cap`
    /// times. "Changed" is the version of the operation the fixpoint runs on
    /// (see [`Context::op_version`]): every edit under it bumps that stamp, so
    /// a round that rebuilds nothing rebuilds no analysis either.
    pub fn fixpoint(&mut self, cap: u8) -> &mut PassManager {
        self.passes.push(PassNode::Fixpoint {
            cap,
            manager: self.child(),
        });
        match self.passes.last_mut() {
            Some(PassNode::Fixpoint { manager, .. }) => manager,
            _ => unreachable!("fixpoint entry just added"),
        }
    }

    fn nest_parsed(&mut self, op_name: impl Into<String>) -> &mut PassManager {
        self.passes.push(PassNode::Nested {
            op_name: op_name.into(),
            manager: self.child(),
        });
        match self.passes.last_mut() {
            Some(PassNode::Nested { manager, .. }) => manager,
            _ => unreachable!("nested pass manager entry just added"),
        }
    }

    /// An empty pipeline nested in this one, running with its workers.
    fn child(&self) -> PassManager {
        PassManager {
            passes: vec![],
            workers: self.workers,
        }
    }

    pub fn run(&mut self, context: &Context, op: OpHandle) -> Result<(), PassError> {
        let root = OperationRef::new(op);
        let result = self.run_on_op_ref(context, root, &AnalysisManager::new());
        crate::memstats::summary();
        result.map(|_| ())
    }

    /// Run the pipeline over the subtree rooted at `root` and return the root as
    /// it stands afterwards: a pass may replace the root in place (selection
    /// turns a function into a machine symbol) and both the remaining passes
    /// and the caller follow the replacement. Each top-level entry's edits are
    /// committed before the next entry runs.
    pub fn run_on_op_ref(
        &mut self,
        context: &Context,
        root: OperationRef,
        analyses: &AnalysisManager,
    ) -> Result<OperationRef, PassError> {
        self.run_with(context, root, analyses, Mode::Exclusive)
    }

    /// [`PassManager::run_on_op_ref`] inside a task's context when `mode` is
    /// shared: a nested pipeline's edits stay pending with the enclosing one's.
    fn run_with(
        &mut self,
        context: &Context,
        mut root: OperationRef,
        analyses: &AnalysisManager,
        mode: Mode,
    ) -> Result<OperationRef, PassError> {
        let workers = self.workers;
        for entry in &mut self.passes {
            Self::run_entry(entry, context, &root, analyses, mode, workers)?;
            if let Some(current) = refreshed(context, &root) {
                root = current;
            }
            if mode == Mode::Exclusive {
                context.commit();
            }
        }
        Ok(root)
    }

    fn run_entry(
        entry: &mut PassNode,
        context: &Context,
        root: &OperationRef,
        analyses: &AnalysisManager,
        mode: Mode,
        workers: usize,
    ) -> Result<(), PassError> {
        match entry {
            PassNode::Pass { pass, .. } => {
                let scope = crate::memstats::pass_scope(pass.name());
                let started = timing::enabled().then(std::time::Instant::now);
                let version_before = context.op_version(root.op.id);
                PassManager::walk_ops(context, root, &mut |op_ref| {
                    let target = pass.target();
                    if !target.matches(op_ref.op()) {
                        return Ok(());
                    }
                    if let Some(expected) = target.kind_mismatch(context, op_ref.op()) {
                        return Err(PassError::RegionKind {
                            pass: pass.name(),
                            op: describe_op(op_ref.op()),
                            expected,
                        });
                    }
                    let id = op_ref.op().id;
                    if pass.settles() && context.is_settled(pass.name(), id) {
                        return Ok(());
                    }
                    let version = context.op_version(id);
                    // What the pass builds is located at the op it runs on,
                    // unless the pass locates it more closely.
                    let origin = context.set_origin(context.op_location(id));
                    let ran = pass.run(&op_ref, context, analyses);
                    context.set_origin(origin);
                    ran?;
                    if pass.settles()
                        && context.has_operation(id)
                        && context.op_version(id) == version
                    {
                        context.settle(pass.name(), id);
                    }
                    Ok(())
                })?;
                // Any edit under `root` bumps its version, so this is the "did
                // the pass touch the IR" signal, taken from the IR itself rather
                // than from the pass's own report.
                let mutated = context.op_version(root.op.id) != version_before;
                let dirty = context.take_dirty_ops();
                if mutated && ir_verification_enabled() {
                    let result = if is_machine_ir(context, root.op.id) {
                        crate::backend::verify_machine_ir(context, root.op.id)
                    } else {
                        verify_dirty_subtrees(context, root.op.id, &dirty)
                    };
                    result.map_err(|error| PassError::InvalidIR {
                        pass: pass.name(),
                        error,
                    })?;
                }
                if let Some(started) = started {
                    timing::record(pass.name(), started.elapsed());
                }
                if let Some(scope) = scope {
                    scope.finish(context.census());
                }
                crate::memstats::analysis_census(pass.name(), analyses.cached_count());
                Ok(())
            }
            PassNode::Nested { op_name, manager } => {
                if mode == Mode::Shared {
                    return PassManager::walk_ops(context, root, &mut |op_ref| {
                        if matches_op_name(op_ref.op(), op_name) {
                            manager.run_with(context, op_ref.clone(), analyses, Mode::Shared)?;
                        }
                        Ok(())
                    });
                }
                let mut targets = Vec::new();
                PassManager::walk_ops(context, root, &mut |op_ref| {
                    if matches_op_name(op_ref.op(), op_name) {
                        targets.push(op_ref.op().id);
                    }
                    Ok(())
                })?;
                context.commit();
                let done = tasks::run(context, manager, &targets, workers)?;
                tasks::report(op_name, context, &done);
                context
                    .commit_batches(done.into_iter().map(|d| d.batch).collect())
                    .map_err(PassError::OverlappingEdits)
            }
            PassNode::Fixpoint { cap, manager } => {
                let mut current = root.clone();
                for _ in 0..*cap {
                    let version_before = context.op_version(current.op.id);
                    current = manager.run_with(context, current, analyses, mode)?;
                    if context.op_version(current.op.id) == version_before {
                        break;
                    }
                }
                Ok(())
            }
        }
    }

    fn walk_ops<F>(context: &Context, root: &OperationRef, f: &mut F) -> Result<(), PassError>
    where
        F: FnMut(OperationRef) -> Result<(), PassError>,
    {
        // Read before the visit: it may erase `root`, and the walk still has to
        // descend into the regions the replacement took over.
        let regions: Vec<_> = root
            .op
            .regions()
            .iter()
            .map(|id| context.get_region(*id))
            .collect();
        // What a pass builds under a function goes into that function's chunks.
        let _scope = context.partition_scope(root.op.id);
        f(root.clone())?;
        for region in regions {
            // The visit may have erased `root`, reclaiming the regions it owned;
            // a region the replacement took over is still live and still walked.
            if !region.is_live() {
                continue;
            }
            // A pass run earlier in this walk may have erased or replaced a
            // later op in the same region (isel rewrites the whole block at
            // once); the snapshot below still holds the erased op. Handles,
            // not ids: an erased op's id belongs to whatever took its slot.
            let ops: Vec<_> = region
                .op_ids()
                .iter()
                .map(|id| context.get_op(*id))
                .collect();
            for op in ops {
                if !op.is_live() {
                    continue;
                }
                PassManager::walk_ops(context, &OperationRef::new(op), f)?;
            }
        }
        Ok(())
    }
}

/// `dialect.name`, followed by the symbol an op defines: what an error names.
fn describe_op(op: &OpHandle) -> String {
    let mut text = format!("{}.{}", op.dialect().as_str(), op.name().as_str());
    if let Some(symbol) = op.clone().as_interface::<dyn crate::Symbol>() {
        text.push_str(&format!(" @{}", symbol.symbol_name()));
    }
    text
}

impl Default for PassManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Print wall time per pass as `tir-time:` lines on stderr, slowest first, when
/// `TIR_TIME_PASSES` is set. Totals accumulate over every pipeline run in the
/// process, so call this once at the end; `wall` is the whole run, so the gap
/// to the pass total is the time spent outside passes (frontend, emission).
pub fn report_pass_timing(wall: std::time::Duration) {
    timing::summary(wall);
}

/// One epoch's callable tasks: every target gets a context of its own over
/// the committed base, at most `workers` run at once, and the finished
/// batches come back in target order for the commit.
mod tasks {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{AnalysisManager, Mode, OperationRef, PassError, PassManager};
    use crate::context::{EditBatch, Snapshot};
    use crate::{Context, OpId};

    /// A finished task: its batch and the bytes of the chunks it wrote.
    pub(super) struct Done {
        pub batch: EditBatch,
        pub overlay_bytes: usize,
    }

    pub(super) fn run(
        context: &Context,
        manager: &mut PassManager,
        targets: &[OpId],
        workers: usize,
    ) -> Result<Vec<Done>, PassError> {
        let snapshot = context.snapshot();
        // A task whose partition no other target shares keeps filling it.
        let partitions = snapshot.partitions(targets);
        let mut sorted = partitions.clone();
        sorted.sort_unstable();
        let exclusive: Vec<bool> = partitions
            .iter()
            .map(|&part| {
                let sharing = sorted.partition_point(|other| *other <= part)
                    - sorted.partition_point(|other| *other < part);
                part != 0 && sharing == 1
            })
            .collect();
        let workers = workers.min(targets.len()).max(1);
        let replicas: Option<Vec<PassManager>> = (1..workers).map(|_| manager.replica()).collect();
        let mut results: Vec<Option<Result<Done, PassError>>> =
            (0..targets.len()).map(|_| None).collect();
        match replicas {
            Some(replicas) if !replicas.is_empty() => {
                let next = AtomicUsize::new(0);
                let slots = Mutex::new(std::mem::take(&mut results));
                let worker = |manager: &mut PassManager| {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&target) = targets.get(index) else {
                            return;
                        };
                        let done = one(
                            &snapshot,
                            manager,
                            index,
                            targets.len(),
                            target,
                            exclusive[index],
                        );
                        slots.lock().unwrap()[index] = Some(done);
                    }
                };
                std::thread::scope(|scope| {
                    for mut replica in replicas {
                        let worker = &worker;
                        scope.spawn(move || worker(&mut replica));
                    }
                    worker(manager);
                });
                results = slots.into_inner().unwrap();
            }
            _ => {
                for (index, (slot, &target)) in results.iter_mut().zip(targets).enumerate() {
                    *slot = Some(one(
                        &snapshot,
                        manager,
                        index,
                        targets.len(),
                        target,
                        exclusive[index],
                    ));
                }
            }
        }
        drop(snapshot);
        results
            .into_iter()
            .map(|done| done.expect("every target ran"))
            .collect()
    }

    fn one(
        snapshot: &Snapshot,
        manager: &mut PassManager,
        index: usize,
        count: usize,
        target: OpId,
        exclusive: bool,
    ) -> Result<Done, PassError> {
        let context = Context::open(snapshot, index, count, target, exclusive);
        let root = OperationRef::new(context.get_op(target));
        manager.run_with(&context, root, &AnalysisManager::new(), Mode::Shared)?;
        let overlay_bytes = context.census().touched_bytes;
        Ok(Done {
            batch: context.finish(),
            overlay_bytes,
        })
    }

    /// The epoch's memory, at the moment every task is done and nothing is
    /// committed: the base, the most any task wrote, and what the
    /// batches hold while they wait.
    pub(super) fn report(op_name: &str, context: &Context, done: &[Done]) {
        if !crate::memstats::enabled() {
            return;
        }
        crate::memstats::epoch_census(
            op_name,
            done.len(),
            context.census().bytes(),
            done.iter().map(|d| d.overlay_bytes).max().unwrap_or(0),
            done.iter().map(|d| d.batch.bytes()).sum(),
        );
    }
}

pub(crate) mod timing {
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    static TOTALS: Mutex<Vec<(&'static str, Duration, usize)>> = Mutex::new(Vec::new());

    pub fn enabled() -> bool {
        static FROM_ENV: OnceLock<bool> = OnceLock::new();
        *FROM_ENV
            .get_or_init(|| std::env::var_os("TIR_TIME_PASSES").is_some_and(|value| value != "0"))
    }

    pub fn record(name: &'static str, elapsed: Duration) {
        let mut totals = TOTALS.lock().unwrap();
        match totals.iter_mut().find(|(pass, ..)| *pass == name) {
            Some((_, total, runs)) => {
                *total += elapsed;
                *runs += 1;
            }
            None => totals.push((name, elapsed, 1)),
        }
    }

    pub fn summary(wall: Duration) {
        if !enabled() {
            return;
        }
        let mut totals = TOTALS.lock().unwrap().clone();
        totals.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let total: Duration = totals.iter().map(|(_, elapsed, _)| *elapsed).sum();
        eprintln!(
            "tir-time: summary wall_ms={:.3} passes_ms={:.3}",
            wall.as_secs_f64() * 1e3,
            total.as_secs_f64() * 1e3
        );
        for (name, elapsed, runs) in totals {
            eprintln!(
                "tir-time: pass name={name} total_ms={:.3} runs={runs}",
                elapsed.as_secs_f64() * 1e3
            );
        }
    }
}
