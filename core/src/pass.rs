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

/// Validate an MLIR-style pipeline while pass execution is unavailable.
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
/// one reserved name: `fixpoint<3>(func.func(instcombine))` names a bounded
/// repeated pipeline.
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
        pm.add_boxed_pass(pass);
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
        }
    }
}

impl std::error::Error for PassError {}

#[derive(Debug, Clone, Copy)]
/// Declares a pass's operation target.
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
pub struct OperationTarget;

impl PassTarget {
    pub fn operation<T: Operation>() -> Self {
        Self::Operation(OperationTarget)
    }
    pub fn operation_on<T: Operation>(_kind: RegionKind) -> Self {
        Self::operation::<T>()
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

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        analyses: &AnalysisManager,
    ) -> Result<(), PassError>;
}

impl Context {
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

#[derive(Default)]
pub struct PassManager;

impl PassManager {
    pub fn new() -> Self {
        Self
    }
    pub fn set_workers(&mut self, _workers: usize) {}
    pub fn add_pass<P: Pass + 'static>(&mut self, _pass: P) -> &mut Self {
        self
    }
    pub fn add_boxed_pass(&mut self, _pass: Box<dyn Pass>) -> &mut Self {
        self
    }
    pub fn nest<T: Operation>(&mut self) -> &mut PassManager {
        self
    }
    pub fn fixpoint(&mut self, _cap: u8) -> &mut PassManager {
        self
    }
    fn nest_parsed(&mut self, _op_name: impl Into<String>) -> &mut PassManager {
        self
    }
    pub fn run(&mut self, _context: &Context, _op: OpHandle) -> Result<(), PassError> {
        Ok(())
    }
    pub fn run_on_op_ref(
        &mut self,
        _context: &Context,
        root: OperationRef,
        _analyses: &AnalysisManager,
    ) -> Result<OperationRef, PassError> {
        Ok(root)
    }
}

pub fn report_pass_timing(_wall: std::time::Duration) {}
