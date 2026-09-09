use std::collections::HashMap;

use crate::analysis::{AnalysisManager, regions::subtree_ops};
use crate::builtin::{MakeTupleOp, ModuleOp, TupleGetOp};
use crate::func::{CallOp, FuncOp};
use crate::ptr::AllocaOp;
use crate::{
    ConstantLike, Context, OpHandle, OpId, Operation, OperationRef, Pass, PassError, PassTarget,
    RegionId, Symbol, Terminator, Value, ValueId, Visibility,
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

/// How large a caller may already be before it stops taking copies.
///
/// Not a growth bound: the simplifier's saturation is superlinear in the size of
/// the function it runs on, so what a copy costs is set by the caller it lands
/// in rather than by how much it adds. Measured against `gcc.c-torture`
/// `memset-2.c`, where one 36-op callee inlined into an 1839-op caller moves
/// instcombine from 70 ms to 2.3 s. The largest function in CoreMark is 354 ops, which stays eligible.
const MAX_CALLER_OPS: u32 = 400;

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
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let mut graph = CallGraph::read(context, module.op());
        for index in graph.callees_first() {
            let caller = graph.nodes[index].func;
            let mut caller_ops = cost_of(context, caller);
            for site in std::mem::take(&mut graph.nodes[index].sites) {
                if caller_ops >= MAX_CALLER_OPS {
                    break;
                }
                if !self.admits(context, &graph, &site) {
                    continue;
                }
                caller_ops += cost_of(context, graph.nodes[site.callee].func);
                let copied = splice_nodes(context, &graph, &site)?;
                graph.inlined(&site, &copied);
            }
        }
        graph.erase_dead_private(context)
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
    value: ValueId,
    private: bool,
    users: u32,
    sites: Vec<Site>,
    component: usize,
}

impl CallGraph {
    fn read(context: &Context, module: &OpHandle) -> Self {
        let functions: Vec<OpId> = module
            .regions()
            .iter()
            .flat_map(|&region| context.get_region(region).op_ids())
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
                if args.len()
                    != context
                        .get_region(nodes[callee].body)
                        .value_arguments()
                        .len()
                {
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
            node.users = context.use_count(node.value) as u32;
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
        let mut members = vec![0usize; self.nodes.len()];
        for &component in &components {
            members[component] += 1;
        }
        // A component of one is still non-trivial when its member calls itself,
        // and a callee that can reach itself is never inlined into anyone: a
        // copy of it holds the same call, so there is no depth to bound.
        let mut recursive: Vec<bool> = components
            .iter()
            .map(|&component| members[component] > 1)
            .collect();
        for node in &self.nodes {
            for site in &node.sites {
                if site.callee == site.caller {
                    recursive[site.callee] = true;
                }
            }
        }
        for node in &mut self.nodes {
            for site in &mut node.sites {
                site.recursive = recursive[site.callee];
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

    fn erase_dead_private(&mut self, context: &Context) -> Result<(), PassError> {
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
            context.erase_op(&OperationRef::new(context.get_op(self.nodes[dead].func)))?;
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

/// [`splice`] for an unordered callee body: its operations join the region
/// holding the call, reading the call's arguments through the ports they were
/// declared on. The callee's memory order is wired into the caller's: the
/// state its body was entered on is the one the call observed, and the state
/// the body leaves behind is what the call left. Nothing is unthreaded.
fn splice_nodes(
    context: &Context,
    graph: &CallGraph,
    site: &Site,
) -> Result<Vec<usize>, PassError> {
    let call = site.call.op();
    let callee = &graph.nodes[site.callee];
    let source = context.get_region(callee.body);
    let args = call.clone().as_op::<CallOp>().expect("a call").args();
    let bindings: HashMap<ValueId, ValueId> = source
        .value_arguments()
        .iter()
        .map(Value::id)
        .zip(args)
        .chain(
            source
                .state_arguments()
                .iter()
                .map(Value::id)
                .zip(call.state_operands()),
        )
        .collect();
    let destination = context
        .parent_nodes_region(call.id)
        .expect("a call in an unordered body sits in a region");
    let (ops, mut produced) =
        crate::clone::clone_nodes_ops_into(context, callee.body, &bindings, destination);

    let body = graph.nodes[site.caller].body;
    let copied: Vec<usize> = ops
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
    let entered = call.state_operands().first().copied();
    let roots: Vec<OpId> = ops
        .iter()
        .copied()
        .filter(|&op| context.get_op(op).is::<crate::state::EntryStateOp>())
        .collect();
    // The callee runs on a chain per object it names; the caller hands the
    // call one state, so it is split into the memory each of those chains is
    // entered on. Two chains rooted at one state would be two futures for it.
    let mut chains = match (roots.len() > 1, entered) {
        (true, Some(entered)) => {
            let split = crate::state::SplitOpBuilder::new(context)
                .state(entered)
                .states(roots.len())
                .build();
            context.add(destination, split.id());
            split.states().into_iter()
        }
        _ => Vec::new().into_iter(),
    };
    for &op in &ops {
        let instance = context.get_op(op);
        if instance.is::<AllocaOp>() && destination != body {
            context.remove_from_region(destination, op);
            context.add(body, op);
        }
        if instance.is::<crate::state::EntryStateOp>() {
            let Some(entered) = chains.next().or(entered) else {
                return Err(PassError::RewriteFailed(call.id));
            };
            let root = instance.state_results()[0];
            rename(context, destination, root, entered);
            // A callee touching no memory hands back the state it was entered
            // on, which the copy's result list names by the root just erased.
            for value in &mut produced {
                if *value == root {
                    *value = entered;
                }
            }
            context.erase_op(&OperationRef::new(instance))?;
        }
    }

    let produced_states = context.states_among(&produced);
    for (&old, &new) in call
        .value_results()
        .iter()
        .zip(&context.values_among(&produced))
    {
        rename(context, destination, old, new);
    }
    // A callee touching no memory leaves no dependency behind: the call passed
    // the state it observed through unchanged.
    for (index, &old) in call.state_results().iter().enumerate() {
        let new = produced_states
            .get(index)
            .copied()
            .or(entered)
            .ok_or(PassError::RewriteFailed(call.id))?;
        rename(context, destination, old, new);
    }
    context.erase_op(&site.call)?;

    let caller = context.get_op(graph.nodes[site.caller].func);
    let tuples = fold_tuple_gets(context, body, &subtree_ops(context, &caller))?;
    erase_unused(context, &tuples)?;
    Ok(copied)
}

/// Hand every reader of `old` under `region` the value `new`, the result
/// lists of the regions there included.
fn rename(context: &Context, region: RegionId, old: ValueId, new: ValueId) {
    context.replace_value_uses(old, new);
    context.rename_region_results(region, old, new, &[]);
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
    body: RegionId,
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
        let Some(&element) = Operation::operands(&made).get(get.index() as usize) else {
            continue;
        };
        rename(context, body, get.result(), element);
        context.erase_op(&OperationRef::new(instance))?;
        sources.push((source, made.result()));
    }
    Ok(sources)
}

fn erase_unused(context: &Context, candidates: &[(OpId, ValueId)]) -> Result<(), PassError> {
    for &(op, value) in candidates {
        if context.has_operation(op) && !context.is_used(value) {
            context.erase_op(&OperationRef::new(context.get_op(op)))?;
        }
    }
    Ok(())
}

fn cost_of(context: &Context, function: OpId) -> u32 {
    subtree_ops(context, &context.get_op(function))
        .into_iter()
        .filter(|&op| {
            let instance = context.get_op(op);
            // Dependency bookkeeping computes nothing, so it costs nothing.
            let free = instance
                .clone()
                .as_interface::<dyn ConstantLike>()
                .is_some()
                || instance.clone().as_interface::<dyn Terminator>().is_some()
                || instance.is::<crate::state::EntryStateOp>()
                || instance.is::<crate::state::JoinOp>();
            !free
        })
        .count() as u32
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

fn calls_under(context: &Context, root: &OpHandle) -> Vec<OperationRef> {
    let mut calls = Vec::new();
    collect_calls(context, root, &mut calls);
    calls
}

fn collect_calls(context: &Context, root: &OpHandle, calls: &mut Vec<OperationRef>) {
    for op in root
        .regions()
        .iter()
        .flat_map(|&region| context.get_region(region).op_ids())
    {
        let instance = context.get_op(op);
        collect_calls(context, &instance, calls);
        if instance.is::<CallOp>() {
            calls.push(OperationRef::new(instance));
        }
    }
}
