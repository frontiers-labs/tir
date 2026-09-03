use std::collections::HashMap;

use crate::analysis::AnalysisManager;
use crate::builtin::{MakeTupleOp, ModuleOp, TupleGetOp};
use crate::func::{CallOp, FuncOp, ReturnOp};
use crate::passes::thread_state::unthread;
use crate::ptr::AllocaOp;
use crate::{
    BlockId, ConstantLike, Context, OpHandle, OpId, Operation, OperationRef, Pass, PassError,
    PassTarget, RegionId, Rewriter, Symbol, Terminator, Value, ValueId, Visibility,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlineBudget {
    pub max_ops: u32,
    pub per_constant_arg: u32,
}

impl InlineBudget {
    fn admits(&self, cost: u32, constants: u32) -> bool {
        cost <= self.max_ops + self.per_constant_arg * constants
    }
}

impl std::str::FromStr for InlineBudget {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        let (max_ops, per_constant_arg) = text
            .split_once(',')
            .ok_or_else(|| "inline takes '<max_ops,per_constant_arg>'".to_string())?;
        let number = |field: &str| {
            field
                .trim()
                .parse()
                .map_err(|_| format!("invalid inline budget '{field}'"))
        };
        Ok(Self {
            max_ops: number(max_ops)?,
            per_constant_arg: number(per_constant_arg)?,
        })
    }
}

pub struct InlinePass {
    budget: InlineBudget,
}

impl InlinePass {
    pub fn new(budget: InlineBudget) -> Self {
        Self { budget }
    }

    pub fn parse(args: &str) -> Result<Self, String> {
        args.parse().map(Self::new)
    }
}

crate::register_pass!(InlinePass, "inline", InlinePass::parse);

impl Pass for InlinePass {
    fn name(&self) -> &'static str {
        "inline"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation::<ModuleOp>()
    }

    fn run(
        &mut self,
        module: &OperationRef,
        context: &Context,
        rewriter: &mut Rewriter,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let mut graph = CallGraph::read(context, module.op());
        for index in graph.callees_first() {
            let caller = graph.nodes[index].func;
            let mut edited = false;
            for site in std::mem::take(&mut graph.nodes[index].sites) {
                if !self.admits(context, &graph, &site) {
                    continue;
                }
                if !edited {
                    // Before the first splice: erasing a call whose state
                    // result is still named leaves the split behind it holding
                    // a definition that is gone.
                    unthread(context, rewriter, &context.get_op(caller))?;
                    edited = true;
                }
                let copied = splice(context, rewriter, &graph, &site)?;
                graph.inlined(&site, &copied);
            }
            if edited {
                unthread(context, rewriter, &context.get_op(caller))?;
            }
        }
        graph.erase_dead_private(context, rewriter)
    }
}

impl InlinePass {
    fn admits(&self, context: &Context, graph: &CallGraph, site: &Site) -> bool {
        if site.recursive {
            return false;
        }
        let callee = &graph.nodes[site.callee];
        if callee.private && callee.users == 1 {
            return true;
        }
        self.budget
            .admits(cost_of(context, callee.func), site.constant_args)
    }
}

struct Site {
    call: OperationRef,
    caller: usize,
    callee: usize,
    constant_args: u32,
    recursive: bool,
}

struct CallGraph {
    nodes: Vec<Node>,
    by_op: HashMap<OpId, usize>,
    order: Vec<usize>,
}

struct Node {
    func: OpId,
    body: RegionId,
    entry: BlockId,
    value: ValueId,
    private: bool,
    users: u32,
    sites: Vec<Site>,
    component: usize,
}

impl CallGraph {
    fn read(context: &Context, module: &OpHandle) -> Self {
        let functions: Vec<OpId> = region_ops(context, module)
            .into_iter()
            .filter(|&op| context.get_op(op).is::<FuncOp>())
            .collect();
        let by_op: HashMap<OpId, usize> = functions
            .iter()
            .enumerate()
            .map(|(index, &op)| (op, index))
            .collect();
        let mut nodes: Vec<Node> = functions
            .iter()
            .map(|&op| {
                let function = context.get_op(op).as_op::<FuncOp>().expect("a λ");
                let body = *context
                    .get_op(op)
                    .regions()
                    .first()
                    .expect("a λ has a body");
                Node {
                    func: op,
                    body,
                    entry: context.get_region(body).block_ids()[0],
                    value: function.fn_value(),
                    private: function.symbol_visibility() == Visibility::Private,
                    users: 0,
                    sites: Vec::new(),
                    component: usize::MAX,
                }
            })
            .collect();

        for index in 0..nodes.len() {
            for call in calls_under(context, &context.get_op(nodes[index].func)) {
                let Some(callee) = callee_node(context, &call, &by_op) else {
                    continue;
                };
                let op = call.op().clone().as_op::<CallOp>().expect("a call");
                let args = op.args();
                if args.len() != context.get_block(nodes[callee].entry).arguments().len() {
                    continue;
                }
                let constant_args = args
                    .iter()
                    .filter(|&&arg| is_constant(context, arg))
                    .count() as u32;
                nodes[index].sites.push(Site {
                    call,
                    caller: index,
                    callee,
                    constant_args,
                    recursive: false,
                });
            }
        }
        for node in &mut nodes {
            node.users = uses_of(context, module, node.value);
        }

        let mut graph = Self {
            nodes,
            by_op,
            order: Vec::new(),
        };
        graph.find_components();
        graph
    }

    fn find_components(&mut self) {
        let mut state = Tarjan {
            index: vec![usize::MAX; self.nodes.len()],
            low: vec![0; self.nodes.len()],
            stack: Vec::new(),
            on_stack: vec![false; self.nodes.len()],
            next: 0,
            components: 0,
        };
        for node in 0..self.nodes.len() {
            if state.index[node] == usize::MAX {
                state.visit(node, self);
            }
        }
        let components: Vec<usize> = self.nodes.iter().map(|node| node.component).collect();
        for node in &mut self.nodes {
            let caller = node.component;
            for site in &mut node.sites {
                site.recursive = components[site.callee] == caller;
            }
        }
    }

    fn callees_first(&self) -> Vec<usize> {
        self.order.clone()
    }

    fn inlined(&mut self, site: &Site, copied: &[usize]) {
        self.nodes[site.callee].users -= 1;
        for &callee in copied {
            self.nodes[callee].users += 1;
        }
    }

    fn erase_dead_private(
        &mut self,
        context: &Context,
        rewriter: &mut Rewriter,
    ) -> Result<(), PassError> {
        loop {
            let Some(dead) = (0..self.nodes.len()).find(|&node| {
                self.nodes[node].private
                    && self.nodes[node].users == 0
                    && context.has_operation(self.nodes[node].func)
            }) else {
                return Ok(());
            };
            for call in calls_under(context, &context.get_op(self.nodes[dead].func)) {
                if let Some(callee) = callee_node(context, &call, &self.by_op) {
                    self.nodes[callee].users -= 1;
                }
            }
            self.nodes[dead].users = usize::MAX as u32;
            rewriter.erase_op(&OperationRef::new(context.get_op(self.nodes[dead].func)))?;
        }
    }
}

struct Tarjan {
    index: Vec<usize>,
    low: Vec<usize>,
    stack: Vec<usize>,
    on_stack: Vec<bool>,
    next: usize,
    components: usize,
}

impl Tarjan {
    fn visit(&mut self, node: usize, graph: &mut CallGraph) {
        self.index[node] = self.next;
        self.low[node] = self.next;
        self.next += 1;
        self.stack.push(node);
        self.on_stack[node] = true;

        let callees: Vec<usize> = graph.nodes[node].sites.iter().map(|s| s.callee).collect();
        for callee in callees {
            if self.index[callee] == usize::MAX {
                self.visit(callee, graph);
                self.low[node] = self.low[node].min(self.low[callee]);
            } else if self.on_stack[callee] {
                self.low[node] = self.low[node].min(self.index[callee]);
            }
        }

        if self.low[node] != self.index[node] {
            return;
        }
        let component = self.components;
        self.components += 1;
        loop {
            let member = self.stack.pop().expect("a component holds its root");
            self.on_stack[member] = false;
            graph.nodes[member].component = component;
            graph.order.push(member);
            if member == node {
                return;
            }
        }
    }
}

fn splice(
    context: &Context,
    rewriter: &mut Rewriter,
    graph: &CallGraph,
    site: &Site,
) -> Result<Vec<usize>, PassError> {
    let call = site.call.op().clone().as_op::<CallOp>().expect("a call");
    let callee = &graph.nodes[site.callee];
    let bindings: HashMap<ValueId, ValueId> = context
        .get_block(callee.entry)
        .arguments()
        .iter()
        .map(Value::id)
        .zip(call.args())
        .collect();

    let copy = crate::clone_region_with_mapping(context, callee.body, &bindings);
    let block = context.get_block(context.get_region(copy).block_ids()[0]);
    // The copy duplicates every call the callee held, so the λs they name gain
    // a user the graph read before any of this existed.
    let copied: Vec<usize> = block
        .op_ids()
        .iter()
        .flat_map(|&op| {
            let instance = context.get_op(op);
            let mut calls = calls_under(context, &instance);
            if instance.is::<CallOp>() {
                calls.push(OperationRef::new(instance));
            }
            calls
        })
        .filter_map(|call| callee_node(context, &call, &graph.by_op))
        .collect();
    let ops = block.op_ids();
    let (&last, _) = ops.split_last().expect("a body is terminated");
    let returned = context
        .get_op(last)
        .as_op::<ReturnOp>()
        .expect("a λ body ends in a return")
        .returned_value();
    rewriter.erase_op(&OperationRef::new(context.get_op(last)))?;

    let entry = graph.nodes[site.caller].entry;
    let destination = context
        .parent_block(site.call.op().id)
        .expect("the call sits in a block");
    for op in block.op_ids() {
        block.remove_op(op);
        if context.get_op(op).is::<AllocaOp>() {
            context.get_block(entry).insert(0, op);
            continue;
        }
        let position = context
            .get_block(destination)
            .op_ids()
            .iter()
            .position(|&other| other == site.call.op().id)
            .expect("the call sits in the block holding it");
        context.get_block(destination).insert(position, op);
    }

    if let Some(returned) = returned {
        // After splicing, not before: `replace_value_uses` skips blocks that
        // are not in the tree.
        context.replace_value_uses(call.result(), returned);
    }
    rewriter.erase_op(&site.call)?;
    rewriter.erase_block(block.id());

    let caller = context.get_op(graph.nodes[site.caller].func);
    let tuples = fold_tuple_gets(context, rewriter, &ops_under(context, &caller))?;
    erase_unused(context, rewriter, &caller, &tuples)?;
    Ok(copied)
}

/// Resolve `tuple_get(make_tuple(..), i)` in `ops` to the element it selects.
///
/// Substituting an argument for a parameter is what creates the pattern: a
/// struct passed by value is a `make_tuple` in the caller and a `tuple_get` in
/// the callee, and only inlining ever puts the two in one function. Nothing
/// downstream knows the shape, and a `make_tuple` reaching the backend in any
/// position but a call argument or a returned value cannot be encoded.
fn fold_tuple_gets(
    context: &Context,
    rewriter: &mut Rewriter,
    ops: &[OpId],
) -> Result<Vec<(OpId, ValueId)>, PassError> {
    let mut sources = Vec::new();
    for &op in ops {
        let instance = context.get_op(op);
        let Some(get) = instance.clone().as_op::<TupleGetOp>() else {
            continue;
        };
        let Some(source) = context.get_value(get.tuple()).defining_op() else {
            continue;
        };
        let Some(made) = context.get_op(source).as_op::<MakeTupleOp>() else {
            continue;
        };
        let Some(&element) = Operation::operands(&made).get(get.index()) else {
            continue;
        };
        context.replace_value_uses(get.result(), element);
        rewriter.erase_op(&OperationRef::new(instance))?;
        sources.push((source, made.result()));
    }
    Ok(sources)
}

fn erase_unused(
    context: &Context,
    rewriter: &mut Rewriter,
    caller: &OpHandle,
    candidates: &[(OpId, ValueId)],
) -> Result<(), PassError> {
    for &(op, value) in candidates {
        if context.has_operation(op) && uses_of(context, caller, value) == 0 {
            rewriter.erase_op(&OperationRef::new(context.get_op(op)))?;
        }
    }
    Ok(())
}

fn cost_of(context: &Context, function: OpId) -> u32 {
    fn count(context: &Context, root: &OpHandle, total: &mut u32) {
        for op in region_ops(context, root) {
            let instance = context.get_op(op);
            count(context, &instance, total);
            let free = instance
                .clone()
                .as_interface::<dyn ConstantLike>()
                .is_some()
                || instance.clone().as_interface::<dyn Terminator>().is_some();
            if !free {
                *total += 1;
            }
        }
    }
    let mut total = 0;
    count(context, &context.get_op(function), &mut total);
    total
}

fn callee_node(
    context: &Context,
    call: &OperationRef,
    by_op: &HashMap<OpId, usize>,
) -> Option<usize> {
    let call = call.op().clone().as_op::<CallOp>()?;
    let definition = context.get_value(call.callee()).defining_op()?;
    by_op.get(&definition).copied()
}

fn is_constant(context: &Context, value: ValueId) -> bool {
    context.get_value(value).defining_op().is_some_and(|op| {
        context
            .get_op(op)
            .as_interface::<dyn ConstantLike>()
            .is_some()
    })
}

fn ops_under(context: &Context, root: &OpHandle) -> Vec<OpId> {
    let mut ops = region_ops(context, root);
    let mut index = 0;
    while index < ops.len() {
        ops.extend(region_ops(context, &context.get_op(ops[index])));
        index += 1;
    }
    ops
}

fn region_ops(context: &Context, root: &OpHandle) -> Vec<OpId> {
    root.regions()
        .iter()
        .flat_map(|&region| context.get_region(region).iter(context.clone()))
        .flat_map(|block| block.op_ids())
        .collect()
}

fn calls_under(context: &Context, root: &OpHandle) -> Vec<OperationRef> {
    let mut calls = Vec::new();
    collect_calls(context, root, &mut calls);
    calls
}

fn collect_calls(context: &Context, root: &OpHandle, calls: &mut Vec<OperationRef>) {
    for op in region_ops(context, root) {
        let instance = context.get_op(op);
        collect_calls(context, &instance, calls);
        if instance.is::<CallOp>() {
            calls.push(OperationRef::new(instance));
        }
    }
}

fn uses_of(context: &Context, root: &OpHandle, value: ValueId) -> u32 {
    let mut total = 0;
    for op in region_ops(context, root) {
        let instance = context.get_op(op);
        total += uses_of(context, &instance, value);
        total += instance.operands().iter().filter(|&&v| v == value).count() as u32;
    }
    total
}
