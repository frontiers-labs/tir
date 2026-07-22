use std::sync::Arc;

use linkme::distributed_slice;

use crate::{
    Block, Context, OpId, OpInstance, Operation,
    analysis::{AnalysisManager, PreservedAnalyses},
};

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
/// name may be dialect-qualified (`builtin.func`) or bare (`func`). Example:
/// `builtin.func(mem2reg)` runs `mem2reg` nested inside every function.
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
}

impl std::fmt::Display for PassError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PassError::MissingBlock(name) => {
                write!(f, "operation '{name}' does not have a parent block")
            }
            PassError::InvalidRuleSet(message) => write!(f, "invalid rule set: {message}"),
            PassError::RewriteFailed(op) => write!(f, "failed to rewrite op {op:?}"),
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
/// let _ = tir::PassTarget::Operation("builtin.func");
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

    fn matches(&self, op: &OpInstance) -> bool {
        match self {
            PassTarget::Any => true,
            PassTarget::Operation(target) => op.is_name(target.dialect, target.name),
        }
    }
}

#[derive(Clone)]
pub struct OperationRef {
    op: Arc<OpInstance>,
    block: Option<Arc<Block>>,
    position: Option<usize>,
}

impl OperationRef {
    pub fn new(op: Arc<OpInstance>, block: Option<Arc<Block>>, position: Option<usize>) -> Self {
        Self {
            op,
            block,
            position,
        }
    }

    pub fn op(&self) -> &Arc<OpInstance> {
        &self.op
    }

    pub fn block(&self) -> Option<&Arc<Block>> {
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

    /// Run on `op`, reporting which cached analyses stayed valid. Return
    /// [`PreservedAnalyses::none`] after mutating the IR unless the pass
    /// provably kept an analysis intact; [`PreservedAnalyses::all`] when
    /// nothing changed.
    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<PreservedAnalyses, PassError>;
}

pub struct Rewriter {
    context: Context,
}

impl Rewriter {
    pub fn new(context: Context) -> Self {
        Self { context }
    }

    pub fn context(&self) -> &Context {
        &self.context
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
            let new_results = self.context.get_op(new_op.id()).results.clone();
            if new_results.len() == target.op.results.len() {
                for (old, new) in target.op.results.iter().zip(new_results.iter()) {
                    self.context.replace_value_uses(*old, *new);
                }
            }
            // The replaced-out op no longer references its operands; the new op
            // registered its own uses when it was added to the context. Drop the old
            // op from the arena so it doesn't linger as a phantom.
            self.context.detach_op_uses(&target.op);
            self.context.remove_operation(target.op.id);
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
            self.context.detach_op_uses(&target.op);
            self.context.remove_operation(target.op.id);
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
        &mut self,
        target: &OperationRef,
        new_op: &dyn Operation,
    ) -> Result<(), PassError> {
        let block = target
            .block
            .as_ref()
            .ok_or(PassError::MissingBlock(target.name().as_str()))?;
        let position = block
            .op_ids()
            .iter()
            .position(|id| *id == target.op.id)
            .ok_or(PassError::RewriteFailed(target.op.id))?;
        block.insert(position, new_op.id());
        Ok(())
    }
}

/// Match an op against a nesting spec that is either a bare op name (`func`)
/// or a dialect-qualified name (`builtin.func`).
fn matches_op_name(op: &OpInstance, spec: &str) -> bool {
    match spec.split_once('.') {
        Some((dialect, name)) => op.is_name(dialect, name),
        None => op.name().as_str() == spec,
    }
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
}

impl PassManager {
    pub fn new() -> Self {
        Self { passes: vec![] }
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

    pub fn run(&mut self, context: &Context, op: Arc<OpInstance>) -> Result<(), PassError> {
        let root = OperationRef {
            op,
            block: None,
            position: None,
        };
        self.run_on_op_ref(context, root, &AnalysisManager::new())
    }

    pub fn run_on_op_ref(
        &mut self,
        context: &Context,
        root: OperationRef,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let mut rewriter = Rewriter::new(context.clone());
        for entry in &mut self.passes {
            PassManager::run_entry(entry, context, &root, &mut rewriter, analyses)?;
        }
        Ok(())
    }

    fn run_entry(
        entry: &mut PassNode,
        context: &Context,
        root: &OperationRef,
        rewriter: &mut Rewriter,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        match entry {
            PassNode::Pass(pass) => PassManager::walk_ops(context, root, &mut |op_ref| {
                if pass.target().matches(op_ref.op()) {
                    let preserved = pass.run(&op_ref, context, rewriter, analyses)?;
                    analyses.invalidate(&preserved);
                }
                Ok(())
            }),
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
        f(root.clone())?;
        for region_id in &root.op.regions {
            let region = context.get_region(*region_id);
            for block in region.iter(context.clone()) {
                let op_ids = block.op_ids();
                for (index, op_id) in op_ids.into_iter().enumerate() {
                    // A pass run earlier in this walk may have erased or replaced a
                    // later op in the same block (isel rewrites the whole block at
                    // once); the snapshot still holds the old id. Skip ops that are no
                    // longer live — a replacement carries a new id and isn't revisited.
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
        Context, IRBuilder, Operation,
        builtin::{AddIOp, FuncOp, IntegerType, ops},
    };

    use super::{AnalysisManager, Pass, PassError, PassManager, PassTarget, PreservedAnalyses};

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
        ) -> Result<PreservedAnalyses, PassError> {
            let add = op.as_op::<AddIOp>().expect("target guarantees AddIOp");
            let operands = add.operands();
            let result_ty = context.get_value(add.result()).ty();
            let new_op = ops::subi(context, operands[0], operands[1], result_ty).build();
            rewriter.replace_op(op, &new_op)?;
            Ok(PreservedAnalyses::none())
        }
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

        let func = ops::func(
            &context,
            "demo",
            IntegerType::new(&context, 32),
            Some(region.id()),
        )
        .build();
        let func_body = func.body();

        let mut func_builder = IRBuilder::new(func_body.clone());
        let add = ops::addi(
            &context,
            func_body.arguments()[0].id(),
            func_body.arguments()[1].id(),
            IntegerType::new(&context, 32),
        )
        .build();
        let add_result = add.result();
        let add_id = add.id();
        func_builder.insert(add);
        func_builder.insert(ops::r#return(&context, add_result).build());

        let mut module_builder = IRBuilder::new(module.body());
        module_builder.insert(func);

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

        // The use-list followed the rewrite: param0 is now used by the subi (the
        // replacement), not the erased addi.
        let subi_id = func_body.op_ids()[0];
        let uses = context.value_uses(func_body.arguments()[0].id());
        assert_eq!(uses.len(), 1, "param0 should have exactly one use");
        assert_eq!(uses[0].op(), subi_id);

        // The replaced-out addi is gone from the arena, not just the block.
        assert!(
            !context.has_operation(add_id),
            "replaced op should leave the arena"
        );
    }

    #[test]
    fn erasing_an_op_detaches_its_operand_uses() {
        let context = Context::with_default_dialects();
        let i32 = IntegerType::new(&context, 32);

        let region = context.create_region();
        let arg = context.create_value(i32, None);
        let block = context.create_block(vec![arg.clone()]);
        region.add_block(block.id());
        let func = ops::func(&context, "demo", i32, Some(region.id())).build();
        let body = func.body();

        let mut b = IRBuilder::new(body.clone());
        let neg = ops::subi(
            &context,
            body.arguments()[0].id(),
            body.arguments()[0].id(),
            i32,
        )
        .build();
        let neg_id = neg.id();
        let neg_ref = super::OperationRef::new(context.get_op(neg_id), Some(body.clone()), None);
        b.insert(neg);
        assert!(context.is_value_used(body.arguments()[0].id()));

        let mut rewriter = super::Rewriter::new(context.clone());
        rewriter.erase_op(&neg_ref).expect("erase should succeed");

        assert!(
            !context.is_value_used(body.arguments()[0].id()),
            "erasing the only consumer must clear the value's uses"
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

        let pm = parse_pipeline("mem2reg").expect("bare pass should parse");
        assert!(matches!(pm.passes.as_slice(), [PassNode::Pass(_)]));

        let pm = parse_pipeline(" builtin.func( mem2reg ) ").expect("nested should parse");
        match pm.passes.as_slice() {
            [PassNode::Nested { op_name, manager }] => {
                assert_eq!(op_name, "builtin.func");
                assert!(matches!(manager.passes.as_slice(), [PassNode::Pass(_)]));
            }
            _ => panic!("expected a single nested node"),
        }
    }

    #[test]
    fn pipeline_reports_errors() {
        assert!(super::parse_pipeline("definitely-not-a-pass").is_err());
        assert!(super::parse_pipeline("builtin.func(mem2reg").is_err());
        assert!(super::parse_pipeline("mem2reg)").is_err());
    }

    #[test]
    fn matches_bare_and_qualified_op_names() {
        let context = Context::with_default_dialects();
        let func = ops::func(&context, "demo", IntegerType::new(&context, 32), None).build();
        let op = context.get_op(func.id());
        assert!(super::matches_op_name(&op, "func"));
        assert!(super::matches_op_name(&op, "builtin.func"));
        assert!(!super::matches_op_name(&op, "scf.func"));
        assert!(!super::matches_op_name(&op, "module"));
    }
}
