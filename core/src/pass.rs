use crate::BlockHandle;

use linkme::distributed_slice;

use crate::{Context, OpHandle, OpId, Operation, Value, analysis::AnalysisManager};

/// A pass made available to the pipeline parser by name.
///
/// Backends and libraries contribute entries with [`register_pass!`]; the opt
/// tool builds pipelines purely from this registry, so adding a pass never
/// requires touching the tool.
pub struct PassInfo {
    pub name: &'static str,
    pub ctor: fn() -> Box<dyn Pass>,
}

/// Link-time registry of every pass reachable in the final binary.
#[distributed_slice]
pub static PASSES: [PassInfo];

/// Construct a registered pass by name, or `None` if no pass owns that name.
pub fn build_pass(name: &str) -> Option<Box<dyn Pass>> {
    PASSES.iter().find(|p| p.name == name).map(|p| (p.ctor)())
}

/// Names of all registered passes, for help text and diagnostics.
pub fn registered_passes() -> Vec<&'static str> {
    let mut names: Vec<_> = PASSES.iter().map(|p| p.name).collect();
    names.sort_unstable();
    names
}

/// Register a pass under `name` so the pipeline parser can build it.
///
/// `ty` must implement [`Pass`] and expose a `new() -> Self` constructor.
#[macro_export]
macro_rules! register_pass {
    ($ty:ty, $name:expr) => {
        const _: () = {
            #[$crate::linkme::distributed_slice($crate::PASSES)]
            #[linkme(crate = $crate::linkme)]
            static REGISTRATION: $crate::PassInfo = $crate::PassInfo {
                name: $name,
                ctor: || ::std::boxed::Box::new(<$ty>::new()),
            };
        };
    };
}

/// Parse an MLIR-style pass pipeline into a [`PassManager`].
///
/// The grammar is a comma-separated list of elements, where each element is
/// either a registered pass name or an op-nesting `op(inner-pipeline)`. The op
/// name may be dialect-qualified (`func.func`) or bare (`func`). Example:
/// `func.func(instcombine)` runs `instcombine` nested inside every function.
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

    fn parse_element(&mut self, pm: &mut PassManager) -> Result<(), String> {
        self.skip_ws();
        let name = self.parse_ident()?;
        self.skip_ws();
        if self.pos < self.bytes.len() && self.bytes[self.pos] == b'(' {
            self.pos += 1;
            let nested = pm.nest_parsed(name);
            self.parse_list(nested)?;
            self.skip_ws();
            if self.pos >= self.bytes.len() || self.bytes[self.pos] != b')' {
                return Err("missing ')' in pass pipeline".to_string());
            }
            self.pos += 1;
            Ok(())
        } else {
            let pass = build_pass(&name).ok_or_else(|| format!("unknown pass '{name}'"))?;
            pm.add_boxed_pass(pass);
            Ok(())
        }
    }
}

#[derive(Debug)]
pub enum PassError {
    MissingBlock(&'static str),
    InvalidRuleSet(String),
    RewriteFailed(OpId),
    InvalidIR {
        pass: &'static str,
        error: crate::Error,
    },
}

impl std::fmt::Display for PassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PassError::MissingBlock(name) => {
                write!(f, "operation '{name}' does not have a parent block")
            }
            PassError::InvalidRuleSet(message) => write!(f, "invalid rule set: {message}"),
            PassError::RewriteFailed(op) => write!(f, "failed to rewrite op {op:?}"),
            PassError::InvalidIR { pass, error } => {
                write!(f, "pass '{pass}' produced invalid IR: {error:?}")
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
/// A type-safe operation target stored by [`PassTarget`].
pub struct OperationTarget {
    dialect: &'static str,
    name: &'static str,
}

impl PassTarget {
    /// Targets operations of type `T`.
    pub fn operation<T: Operation>() -> Self {
        Self::Operation(OperationTarget {
            dialect: T::dialect(),
            name: T::name(),
        })
    }

    fn matches(&self, op: &OpHandle) -> bool {
        match self {
            PassTarget::Any => true,
            PassTarget::Operation(target) => op.is_name(target.dialect, target.name),
        }
    }
}

#[derive(Clone)]
pub struct OperationRef {
    op: OpHandle,
    block: Option<BlockHandle>,
    position: Option<usize>,
}

impl OperationRef {
    pub fn new(op: OpHandle, block: Option<BlockHandle>, position: Option<usize>) -> Self {
        Self {
            op,
            block,
            position,
        }
    }

    pub fn op(&self) -> &OpHandle {
        &self.op
    }

    pub fn block(&self) -> Option<&BlockHandle> {
        self.block.as_ref()
    }

    pub fn position(&self) -> Option<usize> {
        self.position
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

    /// The IR contract this pass leaves behind. Instruction selection rewrites
    /// SSA operations into machine instructions, which record their destination
    /// as a register attribute instead of an SSA result (see
    /// [`Rewriter::replace_op`]); [`crate::verify_op_tree`] checks the SSA
    /// contract, so it does not describe that output.
    fn emits_machine_ir(&self) -> bool {
        false
    }

    /// Run on `op`. A pass reports nothing about what it changed: every edit
    /// bumps the version stamps of the ops it touched (see
    /// [`Context::op_version`]), which is what invalidates cached analyses and
    /// triggers post-pass verification.
    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError>;
}

pub struct Rewriter {
    context: Context,
    /// Set while a machine-emitting pass runs. Such a pass replaces a source op
    /// with machine ops that declare no SSA results and instead claim the
    /// original result's def-site through a Def-role register attribute, so the
    /// erased op's result values must outlive it.
    results_claimed: bool,
}

impl Rewriter {
    pub fn new(context: Context) -> Self {
        Self {
            context,
            results_claimed: false,
        }
    }

    pub(crate) fn set_results_claimed(&mut self, claimed: bool) {
        self.results_claimed = claimed;
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    /// Give `block` one more entry argument. The block keeps its id, so the
    /// branches naming it as a destination stay valid.
    pub fn append_block_argument(&mut self, block: crate::BlockId, ty: crate::TypeId) -> Value {
        self.context.append_block_argument(block, ty)
    }

    /// Move everything `block` holds from `at` onward into a fresh block, which
    /// is returned detached: the caller decides where in a region it belongs.
    pub fn split_block(&mut self, block: crate::BlockId, at: usize) -> BlockHandle {
        let source = self.context.get_block(block);
        let tail = source.op_ids().split_off(at);
        let split = self.context.create_block(vec![]);
        for op in tail {
            source.remove_op(op);
            split.append(op);
        }
        split
    }

    /// Copy `op` and everything under it, returning the copy. The copy is
    /// detached: the caller decides which block it joins.
    pub fn clone_op(&mut self, op: OpId) -> OpId {
        crate::clone::clone_op(&self.context, op)
    }

    /// Copy `region` and everything under it, returning the copy. The copy is
    /// detached: the caller decides which operation owns it.
    pub fn clone_region(&mut self, region: crate::RegionId) -> crate::RegionId {
        crate::clone::clone_region(&self.context, region)
    }

    /// Move every block of `source` to the end of `destination`, emptying
    /// `source`.
    pub fn splice_region(&mut self, source: crate::RegionId, destination: crate::RegionId) {
        let source = self.context.get_region(source);
        let destination = self.context.get_region(destination);
        for block in source
            .iter(self.context.clone())
            .map(|block| block.id())
            .collect::<Vec<_>>()
        {
            source.remove_block(block);
            destination.add_block(block);
        }
    }

    /// Move every operation of `source` to the end of `destination`, emptying
    /// `source`.
    pub fn splice_block(&mut self, source: crate::BlockId, destination: crate::BlockId) {
        let source = self.context.get_block(source);
        let destination = self.context.get_block(destination);
        for op in source.op_ids() {
            source.remove_op(op);
            destination.append(op);
        }
    }

    pub fn replace_op(
        &mut self,
        target: &OperationRef,
        new_op: &dyn Operation,
    ) -> Result<(), PassError> {
        let block = target
            .block
            .as_ref()
            .ok_or(PassError::MissingBlock(target.name().as_str()))?;
        if block.replace_op(target.op.id, new_op.id()) {
            // Rewrite SSA uses of the old results to the new op's results when the
            // shapes line up, so consumers don't dangle on the erased op's values.
            // Machine ops declare no SSA results — they instead claim the original
            // result's def-site through a Def-role register attribute (the emitter
            // destination convention) — so they skip this entirely and the original
            // values stay live.
            let new_results = self.context.get_op(new_op.id()).results().to_vec();
            let results_forwarded = new_results.len() == target.op.results().len();
            if results_forwarded {
                for (old, new) in target.op.results().iter().zip(new_results.iter()) {
                    self.context.replace_value_uses(*old, *new);
                }
            }
            // Drop the old op and what it owns so nothing lingers as a phantom.
            self.remove(target.op.id, results_forwarded);
            Ok(())
        } else {
            Err(PassError::RewriteFailed(target.op.id))
        }
    }

    pub fn erase_op(&mut self, target: &OperationRef) -> Result<(), PassError> {
        let block = target
            .block
            .as_ref()
            .ok_or(PassError::MissingBlock(target.name().as_str()))?;
        if block.remove_op(target.op.id) {
            self.remove(target.op.id, true);
            Ok(())
        } else {
            Err(PassError::RewriteFailed(target.op.id))
        }
    }

    /// Erase `op` and the entities it owns. Its result values go with it unless
    /// they were forwarded to a replacement or are claimed by machine ops.
    fn remove(&self, op: OpId, results_dead: bool) {
        if results_dead && !self.results_claimed {
            self.context.remove_operation(op);
        } else {
            self.context.remove_operation_keeping_results(op);
        }
    }

    /// Insert `new_op` immediately before `target` in its block. Used when one
    /// source op lowers to several machine instructions (e.g. a sub-word sign
    /// extension becoming `slli` then `srai`): the feeding instructions are inserted
    /// ahead of the op that consumes them. Repeated calls before the same target
    /// preserve insertion order.
    pub fn insert_op_before(
        &mut self,
        target: &OperationRef,
        new_op: &dyn Operation,
    ) -> Result<(), PassError> {
        let block = target
            .block
            .as_ref()
            .ok_or(PassError::MissingBlock(target.name().as_str()))?;
        // `target` may carry a block handle taken before earlier rewrites, so the
        // position comes from the live block.
        let position = self
            .context
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
/// erased root is looked up by position, because [`Rewriter::replace_op`] keeps the replacement at the
/// erased op's index — that is where selection leaves the machine symbol it
/// made out of a function.
fn refreshed(context: &Context, root: &OperationRef) -> Option<OperationRef> {
    if context.has_operation(root.op.id) {
        return Some(OperationRef::new(
            context.get_op(root.op.id),
            root.block.clone(),
            root.position,
        ));
    }
    let block = root.block.as_ref()?.id();
    let position = root.position?;
    if !context.has_block(block) {
        return None;
    }
    let block = context.get_block(block);
    let id = *block.op_ids().get(position)?;
    context
        .has_operation(id)
        .then(|| OperationRef::new(context.get_op(id), Some(block), Some(position)))
}

/// Whether `op`'s tree has entered the machine layer — it holds a target
/// instruction, or one of the `asm` dialect's containers and pseudos. A machine
/// instruction declares no SSA results; it claims its destination through a
/// register attribute (see [`Rewriter::replace_op`]). The SSA contract
/// [`crate::verify_op_tree`] checks therefore does not describe such a tree, and
/// no verifier expresses the machine contract yet.
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
    for op in dirty {
        if context.has_operation(*op) && encloses(context, root, *op) {
            crate::verify_op_tree(context, *op)?;
        }
    }
    Ok(())
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

enum PassNode {
    Pass(Box<dyn Pass>),
    Nested {
        op_name: String,
        manager: PassManager,
    },
}

pub struct PassManager {
    passes: Vec<PassNode>,
    verify_ir: Option<bool>,
}

impl PassManager {
    pub fn new() -> Self {
        Self {
            passes: vec![],
            verify_ir: None,
        }
    }

    /// Force post-pass IR verification on or off for this pipeline, overriding
    /// the `TIR_VERIFY_IR` environment default.
    pub fn verify_ir(&mut self, enabled: bool) -> &mut Self {
        self.verify_ir = Some(enabled);
        self
    }

    pub fn add_pass<P: Pass + 'static>(&mut self, pass: P) -> &mut Self {
        self.add_boxed_pass(Box::new(pass))
    }

    pub fn add_boxed_pass(&mut self, pass: Box<dyn Pass>) -> &mut Self {
        self.passes.push(PassNode::Pass(pass));
        self
    }

    /// Nest a sub-pipeline under every operation of type `T`.
    pub fn nest<T: Operation>(&mut self) -> &mut PassManager {
        self.nest_parsed(format!("{}.{}", T::dialect(), T::name()))
    }

    fn nest_parsed(&mut self, op_name: impl Into<String>) -> &mut PassManager {
        self.passes.push(PassNode::Nested {
            op_name: op_name.into(),
            manager: PassManager::new(),
        });
        match self.passes.last_mut() {
            Some(PassNode::Nested { manager, .. }) => manager,
            _ => unreachable!("nested pass manager entry just added"),
        }
    }

    pub fn run(&mut self, context: &Context, op: OpHandle) -> Result<(), PassError> {
        let root = OperationRef {
            op,
            block: None,
            position: None,
        };
        let result = self.run_on_op_ref(context, root, &AnalysisManager::new());
        crate::memstats::summary();
        result.map(|_| ())
    }

    /// Run the pipeline over the subtree rooted at `root` and return the root as
    /// it stands afterwards: a pass may replace the root in place — selection
    /// turns a function into a machine symbol — and both the remaining passes
    /// and the caller follow the replacement.
    pub fn run_on_op_ref(
        &mut self,
        context: &Context,
        mut root: OperationRef,
        analyses: &AnalysisManager,
    ) -> Result<OperationRef, PassError> {
        let mut rewriter = Rewriter::new(context.clone());
        for entry in &mut self.passes {
            Self::run_entry(
                entry,
                self.verify_ir,
                context,
                &root,
                &mut rewriter,
                analyses,
            )?;
            if let Some(current) = refreshed(context, &root) {
                root = current;
            }
        }
        Ok(root)
    }

    fn run_entry(
        entry: &mut PassNode,
        verify_ir: Option<bool>,
        context: &Context,
        root: &OperationRef,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        match entry {
            PassNode::Pass(pass) => {
                let scope = crate::memstats::pass_scope(pass.name());
                rewriter.set_results_claimed(pass.emits_machine_ir());
                let version_before = context.op_version(root.op.id);
                PassManager::walk_ops(context, root, &mut |op_ref| {
                    if pass.target().matches(op_ref.op()) {
                        pass.run(&op_ref, context, rewriter, analyses)?;
                    }
                    Ok(())
                })?;
                // Any edit under `root` bumps its version, so this is the "did
                // the pass touch the IR" signal, taken from the IR itself rather
                // than from the pass's own report.
                let mutated = context.op_version(root.op.id) != version_before;
                let dirty = context.take_dirty_ops();
                if mutated
                    && verify_ir.unwrap_or_else(ir_verification_enabled)
                    && !pass.emits_machine_ir()
                    && !is_machine_ir(context, root.op.id)
                {
                    verify_dirty_subtrees(context, root.op.id, &dirty).map_err(|error| {
                        PassError::InvalidIR {
                            pass: pass.name(),
                            error,
                        }
                    })?;
                }
                if let Some(scope) = scope {
                    scope.finish(context.slab_census());
                }
                crate::memstats::analysis_census(pass.name(), analyses.cached_count());
                Ok(())
            }
            PassNode::Nested { op_name, manager } => {
                PassManager::walk_ops(context, root, &mut |op_ref| {
                    if matches_op_name(op_ref.op(), op_name) {
                        manager.run_on_op_ref(context, op_ref.clone(), analyses)?;
                    }
                    Ok(())
                })
            }
        }
    }

    fn walk_ops<F>(context: &Context, root: &OperationRef, f: &mut F) -> Result<(), PassError>
    where
        F: FnMut(OperationRef) -> Result<(), PassError>,
    {
        // Read before the visit: it may erase `root`, and the walk still has to
        // descend into the regions the replacement took over.
        let regions = root.op.regions();
        f(root.clone())?;
        for region_id in regions {
            // The visit may have erased `root`, reclaiming the regions it owned;
            // a region the replacement took over is still live and still walked.
            if !context.has_region(region_id) {
                continue;
            }
            let region = context.get_region(region_id);
            for block in region.iter(context.clone()) {
                let op_ids = block.op_ids();
                for (index, op_id) in op_ids.into_iter().enumerate() {
                    // A pass run earlier in this walk may have erased or replaced a
                    // later op in the same block (isel rewrites the whole block at
                    // once); the id list read before the loop still holds the old id.
                    // Skip ops that are no longer live — a replacement carries a new
                    // id and isn't revisited.
                    if !context.has_operation(op_id) {
                        continue;
                    }
                    let op = context.get_op(op_id);
                    let child = OperationRef {
                        op,
                        block: Some(block.clone()),
                        position: Some(index),
                    };
                    PassManager::walk_ops(context, &child, f)?;
                }
            }
        }
        Ok(())
    }
}

impl Default for PassManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Context, Operation,
        builtin::{AddIOp, IntegerType, ops},
        cfg::ops as cfg_ops,
        func::{FuncOp, ops as func_ops},
    };

    use super::{AnalysisManager, Pass, PassError, PassManager, PassTarget};

    struct AddToSubPass;

    impl Pass for AddToSubPass {
        fn name(&self) -> &'static str {
            "add-to-sub"
        }

        fn target(&self) -> PassTarget {
            PassTarget::operation::<AddIOp>()
        }

        fn run(
            &mut self,
            op: &super::OperationRef,
            context: &Context,
            rewriter: &mut super::Rewriter,
            _analyses: &AnalysisManager,
        ) -> Result<(), PassError> {
            let add = op.as_op::<AddIOp>().expect("target guarantees AddIOp");
            let operands = add.operands();
            let result_ty = context.get_value(add.result()).ty();
            let new_op = ops::subi(context, operands[0], operands[1], result_ty).build();
            rewriter.replace_op(op, &new_op)
        }
    }

    /// Erases the addi while the `return` still reads its result, leaving a
    /// dangling operand.
    struct BreakIRPass;

    impl Pass for BreakIRPass {
        fn name(&self) -> &'static str {
            "break-ir"
        }

        fn target(&self) -> PassTarget {
            PassTarget::operation::<AddIOp>()
        }

        fn run(
            &mut self,
            op: &super::OperationRef,
            _context: &Context,
            rewriter: &mut super::Rewriter,
            _analyses: &AnalysisManager,
        ) -> Result<(), PassError> {
            rewriter.erase_op(op)
        }
    }

    /// Reads the IR and leaves it exactly as it found it.
    struct ReadOnlyPass;

    impl Pass for ReadOnlyPass {
        fn name(&self) -> &'static str {
            "read-only"
        }

        fn run(
            &mut self,
            _op: &super::OperationRef,
            _context: &Context,
            _rewriter: &mut super::Rewriter,
            _analyses: &AnalysisManager,
        ) -> Result<(), PassError> {
            Ok(())
        }
    }

    /// Mutates on every run, without changing what the IR means.
    struct TouchPass;

    impl Pass for TouchPass {
        fn name(&self) -> &'static str {
            "touch"
        }

        fn target(&self) -> PassTarget {
            PassTarget::operation::<FuncOp>()
        }

        fn run(
            &mut self,
            op: &super::OperationRef,
            _context: &Context,
            _rewriter: &mut super::Rewriter,
            _analyses: &AnalysisManager,
        ) -> Result<(), PassError> {
            let func = op.as_op::<FuncOp>().expect("target guarantees FuncOp");
            func.body()
                .set_attr("touched", crate::attributes::AttributeValue::Bool(true));
            Ok(())
        }
    }

    /// `func.func demo(%0) { %1 = addi %0, %0; func.return %1 }`, with `pass` run over it.
    fn run_on_broken_candidate(pass: Box<dyn Pass>) -> Result<(), PassError> {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);
        let region = context.create_region();
        let arg = context.create_value(i32, None);
        let block = context.create_block(vec![arg]);
        region.add_block(block.id());
        let func = func_ops::func(&context, "demo", i32, Some(region.id())).build();
        let body = func.body();

        let add = ops::addi(
            &context,
            body.arguments()[0].id(),
            body.arguments()[0].id(),
            i32,
        )
        .build();
        let add_result = add.result();
        body.append_op(add);
        body.append_op(func_ops::r#return(&context, add_result).build());

        let mut pm = PassManager::new();
        pm.verify_ir(true);
        pm.add_boxed_pass(pass);
        pm.run(&context, context.get_op(func.id()))
    }

    #[test]
    fn invalid_ir_after_a_pass_names_that_pass() {
        let error = run_on_broken_candidate(Box::new(BreakIRPass))
            .expect_err("erasing a still-used op must be caught");
        assert!(
            error.to_string().contains("break-ir"),
            "error should name the offending pass, got: {error}"
        );
    }

    #[test]
    fn appending_a_block_argument_keeps_the_block_id() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);
        let block = context.create_block(vec![]);
        let mut rewriter = super::Rewriter::new(context.clone());

        let argument = rewriter.append_block_argument(block.id(), i32);

        let block = context.get_block(block.id());
        assert_eq!(
            block.arguments().iter().map(|a| a.id()).collect::<Vec<_>>(),
            vec![argument.id()]
        );
    }

    #[test]
    fn splitting_a_block_moves_its_tail_into_a_new_block() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);
        let value = context.create_value(i32, None);
        let block = context.create_block(vec![]);
        let head = block.append_op(ops::addi(&context, value.id(), value.id(), i32).build());
        let tail = block.append_op(ops::subi(&context, value.id(), value.id(), i32).build());
        let mut rewriter = super::Rewriter::new(context.clone());

        let split = rewriter.split_block(block.id(), 1);

        assert_eq!(context.get_block(block.id()).op_ids(), vec![head.id()]);
        assert_eq!(split.op_ids(), vec![tail.id()]);
        assert_eq!(context.parent_block(tail.id()), Some(split.id()));
    }

    #[test]
    fn splicing_a_block_appends_its_operations_to_another() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);
        let value = context.create_value(i32, None);
        let destination = context.create_block(vec![]);
        let source = context.create_block(vec![]);
        let head = destination
            .clone()
            .append_op(ops::addi(&context, value.id(), value.id(), i32).build());
        let moved = source
            .clone()
            .append_op(ops::subi(&context, value.id(), value.id(), i32).build());
        let mut rewriter = super::Rewriter::new(context.clone());

        rewriter.splice_block(source.id(), destination.id());

        assert_eq!(
            context.get_block(destination.id()).op_ids(),
            vec![head.id(), moved.id()]
        );
        assert!(context.get_block(source.id()).is_empty());
        assert_eq!(context.parent_block(moved.id()), Some(destination.id()));
    }

    /// A function whose body block takes one argument, adds it to itself and
    /// returns the sum.
    fn function_with_one_argument(context: &Context) -> FuncOp {
        let i32 = IntegerType::new(context, 32);
        let region = context.create_region();
        let argument = context.create_value(i32, None);
        let block = context.create_block(vec![argument.clone()]);
        region.add_block(block.id());
        let add = block.append_op(ops::addi(context, argument.id(), argument.id(), i32).build());
        block.append_op(func_ops::r#return(context, add.result()).build());
        func_ops::func(context, "demo", i32, Some(region.id())).build()
    }

    #[test]
    fn cloning_an_op_remaps_values_defined_inside_it() {
        let context = Context::with_default_dialects();
        let source = function_with_one_argument(&context);
        let mut rewriter = super::Rewriter::new(context.clone());

        let clone = rewriter.clone_op(source.id());

        let clone = context.get_op(clone);
        assert_ne!(clone.id, source.id());
        let body = context
            .get_region(clone.regions()[0])
            .iter(context.clone())
            .next()
            .expect("the clone keeps the body block");
        let argument = body.arguments()[0].id();
        assert_ne!(argument, source.body().arguments()[0].id());
        let add = context.get_op(body.op_ids()[0]);
        assert_eq!(add.operands().as_slice(), vec![argument, argument]);
        let r#return = context.get_op(body.op_ids()[1]);
        assert_eq!(r#return.operands().as_slice(), vec![add.results()[0]]);
    }

    #[test]
    fn cloning_a_region_remaps_branch_destinations() {
        let context = Context::with_default_dialects();
        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let target = context.create_block(vec![]);
        region.add_block(entry.id());
        region.add_block(target.id());
        entry.append_op(cfg_ops::br(&context, vec![], target.id()).build());
        target
            .clone()
            .append_op(func_ops::r#return(&context, crate::Operand::none()).build());
        let mut rewriter = super::Rewriter::new(context.clone());

        let clone = rewriter.clone_region(region.id());

        let blocks: Vec<_> = context
            .get_region(clone)
            .iter(context.clone())
            .map(|block| block.id())
            .collect();
        assert_eq!(blocks.len(), 2);
        assert!(!blocks.contains(&target.id()));
        let branch = context
            .get_op(context.get_block(blocks[0]).op_ids()[0])
            .as_op::<crate::cfg::BranchOp>()
            .expect("the clone keeps the branch");
        assert_eq!(branch.dest(), blocks[1]);
    }

    #[test]
    fn splicing_a_region_moves_its_blocks() {
        let context = Context::with_default_dialects();
        let source = context.create_region();
        let destination = context.create_region();
        let moved = context.create_block(vec![]);
        source.add_block(moved.id());
        let mut rewriter = super::Rewriter::new(context.clone());

        rewriter.splice_region(source.id(), destination.id());

        assert_eq!(source.iter(context.clone()).count(), 0);
        let blocks: Vec<_> = destination
            .iter(context.clone())
            .map(|block| block.id())
            .collect();
        assert_eq!(blocks, vec![moved.id()]);
        assert_eq!(context.parent_region(moved.id()), Some(destination.id()));
    }

    #[test]
    fn a_pass_that_changes_nothing_is_not_verified() {
        run_on_broken_candidate(Box::new(ReadOnlyPass))
            .expect("a pass that left no version behind skips verification");
    }

    #[test]
    fn an_analysis_survives_a_pass_that_changes_nothing() {
        use crate::analysis::DominatorTree;

        let context = Context::with_default_dialects();
        let func = function_with_one_argument(&context);
        let analyses = AnalysisManager::new();
        let before = analyses.get::<DominatorTree>(&context, func.id());

        let mut pm = PassManager::new();
        pm.add_pass(ReadOnlyPass);
        let root = super::OperationRef::new(context.get_op(func.id()), None, None);
        pm.run_on_op_ref(&context, root, &analyses)
            .expect("the pass changes nothing");

        assert!(std::rc::Rc::ptr_eq(
            &before,
            &analyses.get::<DominatorTree>(&context, func.id())
        ));
    }

    #[test]
    fn repeated_pass_runs_do_not_grow_the_analysis_cache() {
        use crate::analysis::DominatorTree;

        let context = Context::with_default_dialects();
        let func = function_with_one_argument(&context);
        let analyses = AnalysisManager::new();
        let mut pm = PassManager::new();
        pm.add_pass(TouchPass);

        let mut counts = Vec::new();
        for _ in 0..8 {
            let root = super::OperationRef::new(context.get_op(func.id()), None, None);
            pm.run_on_op_ref(&context, root, &analyses)
                .expect("touching a block attribute keeps the IR valid");
            analyses.get::<DominatorTree>(&context, func.id());
            counts.push(analyses.cached_count());
        }

        assert!(
            counts.iter().all(|count| *count == counts[0]),
            "each rebuild must replace the stale result, got {counts:?}"
        );
    }

    #[test]
    fn an_analysis_is_rebuilt_after_a_pass_mutates() {
        use crate::analysis::DominatorTree;

        let context = Context::with_default_dialects();
        let func = function_with_one_argument(&context);
        let analyses = AnalysisManager::new();
        let before = analyses.get::<DominatorTree>(&context, func.id());

        let mut pm = PassManager::new();
        pm.add_pass(AddToSubPass);
        let root = super::OperationRef::new(context.get_op(func.id()), None, None);
        pm.run_on_op_ref(&context, root, &analyses)
            .expect("rewriting addi to subi keeps the IR valid");

        assert!(!std::rc::Rc::ptr_eq(
            &before,
            &analyses.get::<DominatorTree>(&context, func.id())
        ));
    }

    #[test]
    fn nested_pass_manager_rewrites_ops() {
        let context = Context::with_default_dialects();
        let module = ops::module(&context, None).build();

        let param0 = context.create_value(IntegerType::new(&context, 32), None);
        let param1 = context.create_value(IntegerType::new(&context, 32), None);

        let region = context.create_region();
        let block = context.create_block(vec![param0, param1]);
        region.add_block(block.id());

        let func = func_ops::func(
            &context,
            "demo",
            IntegerType::new(&context, 32),
            Some(region.id()),
        )
        .build();
        let func_body = func.body();
        let func_id = func.id();

        let func_builder = func_body.clone();
        let add = ops::addi(
            &context,
            func_body.arguments()[0].id(),
            func_body.arguments()[1].id(),
            IntegerType::new(&context, 32),
        )
        .build();
        let add_result = add.result();
        let add_id = add.id();
        func_builder.append_op(add);
        func_builder.append_op(func_ops::r#return(&context, add_result).build());

        module.body().append_op(func);

        let mut pm = PassManager::new();
        pm.nest::<FuncOp>().add_pass(AddToSubPass);

        pm.run(&context, context.get_op(module.id()))
            .expect("pass pipeline should succeed");

        let op_names: Vec<_> = func_body
            .op_ids()
            .into_iter()
            .map(|op_id| context.get_op(op_id).name().as_str())
            .collect();

        assert_eq!(op_names, vec!["subi", "return"]);

        // The def-use chain followed the rewrite: param0 is now read by the subi
        // (the replacement), not by the erased addi.
        let subi_id = func_body.op_ids()[0];
        assert_eq!(
            crate::analysis::DefUse::new(&context, func_id)
                .users_of(func_body.arguments()[0].id().number()),
            [subi_id]
        );

        // The replaced-out addi is gone from the arena, not just the block.
        assert!(
            !context.has_operation(add_id),
            "replaced op should leave the arena"
        );
    }

    #[test]
    fn erasing_an_op_drops_its_operand_uses() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);

        let region = context.create_region();
        let arg = context.create_value(i32, None);
        let block = context.create_block(vec![arg.clone()]);
        region.add_block(block.id());
        let func = func_ops::func(&context, "demo", i32, Some(region.id())).build();
        let body = func.body();

        let neg = ops::subi(
            &context,
            body.arguments()[0].id(),
            body.arguments()[0].id(),
            i32,
        )
        .build();
        let neg_id = neg.id();
        let neg_ref = super::OperationRef::new(context.get_op(neg_id), Some(body.clone()), None);
        body.append_op(neg);
        let argument = body.arguments()[0].id().number();
        assert!(crate::analysis::DefUse::new(&context, func.id()).is_used(argument));

        let mut rewriter = super::Rewriter::new(context.clone());
        rewriter.erase_op(&neg_ref).expect("erase should succeed");

        assert!(
            !crate::analysis::DefUse::new(&context, func.id()).is_used(argument),
            "erasing the only consumer must leave the value unused"
        );
        // The erased op is gone from the arena, not just the block.
        assert!(
            !context.has_operation(neg_id),
            "erased op should leave the arena"
        );
    }

    #[test]
    fn pipeline_parses_bare_and_nested() {
        use super::{PassNode, parse_pipeline};

        let pm = parse_pipeline("instcombine").expect("bare pass should parse");
        assert!(matches!(pm.passes.as_slice(), [PassNode::Pass(_)]));

        let pm = parse_pipeline(" func.func( instcombine ) ").expect("nested should parse");
        match pm.passes.as_slice() {
            [PassNode::Nested { op_name, manager }] => {
                assert_eq!(op_name, "func.func");
                assert!(matches!(manager.passes.as_slice(), [PassNode::Pass(_)]));
            }
            _ => panic!("expected a single nested node"),
        }
    }

    #[test]
    fn pipeline_reports_errors() {
        assert!(super::parse_pipeline("definitely-not-a-pass").is_err());
        assert!(super::parse_pipeline("func.func(instcombine").is_err());
        assert!(super::parse_pipeline("instcombine)").is_err());
    }

    #[test]
    fn matches_bare_and_qualified_op_names() {
        let context = Context::with_default_dialects();
        let func = func_ops::func(&context, "demo", IntegerType::new(&context, 32), None).build();
        let op = context.get_op(func.id());
        assert!(super::matches_op_name(&op, "func"));
        assert!(super::matches_op_name(&op, "func.func"));
        assert!(!super::matches_op_name(&op, "builtin.func"));
        assert!(!super::matches_op_name(&op, "scf.func"));
        assert!(!super::matches_op_name(&op, "module"));
    }
}
