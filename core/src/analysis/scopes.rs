//! The scopes an RVSDG value lives in: the edges a structured loop is left
//! through, and where a definition is in scope.
//!
//! A loop body binds a token naming its iteration, and an `scf.break`/`scf.continue`
//! names that token to leave through it. A transform growing a carried port has to
//! feed every such edge, so it tracks the scopes it is walking inside.
//!
//! Region nesting is the other half: which definitions reach into a nested
//! region, and which the module holds for everything below it.

use crate::BlockHandle;

use crate::builtin::TokenType;
use crate::{BlockId, Context, OpHandle, OpId, RegionId, ValueId, ValueIds, scf};

/// The loop body's token scope: the value its `scf.break`/`scf.continue` name.
pub fn loop_scope(context: &Context, body: RegionId) -> Option<ValueId> {
    let token = TokenType::new(context);
    let mut blocks = context.get_region(body).iter(context.clone());
    let block = blocks.next()?;
    blocks.next().is_none().then_some(())?;
    block
        .arguments()
        .iter()
        .find(|argument| argument.ty() == token)
        .map(|argument| argument.id())
}

/// The scope `op` leaves through, if it is an exit at all.
pub fn exit_scope(op: &OpHandle) -> Option<ValueId> {
    (op.is::<scf::BreakOp>() || op.is::<scf::ContinueOp>()).then(|| op.operands()[0])
}

/// The scope `region`'s terminator leaves through, if it ends in an exit at all.
pub fn region_exit(context: &Context, region: RegionId) -> Option<ValueId> {
    let block = context.get_region(region).iter(context.clone()).next()?;
    exit_scope(&context.get_op(*block.op_ids().last()?))
}

/// What a structured region's terminator does with control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionExit {
    /// Falls out of the region, carrying its operands to whatever follows it.
    Yield,
    /// Leaves the loop naming its scope.
    Break,
    /// Starts the next iteration of the loop naming its scope.
    Continue,
}

/// How `op` leaves the region it terminates, or `None` where it does not leave one
/// — a `return`, or a terminator emission has already lowered.
pub fn region_exit_kind(op: &OpHandle) -> Option<RegionExit> {
    if op.is::<scf::BreakOp>() {
        Some(RegionExit::Break)
    } else if op.is::<scf::ContinueOp>() {
        Some(RegionExit::Continue)
    } else if op.is::<scf::YieldOp>() {
        Some(RegionExit::Yield)
    } else {
        None
    }
}

/// Every edge feeding a loop's carried ports from inside `body`: the exits leaving
/// its scope, in the order the body tree holds them, and the body's own terminator
/// when it is not one of them. What a transform growing a port has to carry a value
/// on, and the order it reads them in.
pub fn port_edges(context: &Context, body: RegionId) -> Vec<OpId> {
    let Some(block) = context.get_region(body).iter(context.clone()).next() else {
        return Vec::new();
    };
    let mut edges = match loop_scope(context, body) {
        Some(scope) => scope_exits(context, &block, scope),
        None => Vec::new(),
    };
    let Some(&terminator) = block.op_ids().last() else {
        return edges;
    };
    if !edges.contains(&terminator) {
        edges.push(terminator);
    }
    edges
}

/// The exits leaving `scope` from `block`'s region tree, outermost first.
fn scope_exits(context: &Context, block: &BlockHandle, scope: ValueId) -> Vec<OpId> {
    let mut exits = Vec::new();
    for op_id in block.op_ids() {
        let op = context.get_op(op_id);
        if exit_scope(&op) == Some(scope) {
            exits.push(op_id);
        }
        for region in op.regions() {
            for nested in context.get_region(region).iter(context.clone()) {
                exits.extend(scope_exits(context, &nested, scope));
            }
        }
    }
    exits
}

/// The values an edge carries into the ports it feeds: an exit's operands past its
/// scope token, any other terminator's operands.
pub fn carried_operands(op: &OpHandle) -> ValueIds {
    let operands = op.operands();
    match exit_scope(op) {
        Some(_) => operands[1..].into(),
        None => operands,
    }
}

/// The scopes every exit inside `op`'s regions leaves through.
pub fn nested_exit_scopes(context: &Context, op: &OpHandle) -> Vec<ValueId> {
    let mut scopes = Vec::new();
    for region in op.regions() {
        for block in context.get_region(region).iter(context.clone()) {
            for op_id in block.op_ids() {
                let nested = context.get_op(op_id);
                scopes.extend(exit_scope(&nested));
                scopes.extend(nested_exit_scopes(context, &nested));
            }
        }
    }
    scopes
}

/// The region a loop evaluates before each iteration, the arguments it reads the
/// carried values as, and the values it forwards into the body — its terminator's
/// trailing operands, one per port. `None` for a loop that tests nothing it carries,
/// whose body reads the carried values directly.
pub fn tested_ports(
    context: &Context,
    op: &OpHandle,
    ports: usize,
) -> Option<(RegionId, Vec<ValueId>, Vec<ValueId>)> {
    let guard = op.clone().as_interface::<dyn crate::GuardedLoop>()?;
    let crate::EntryGuard::Region {
        region, arguments, ..
    } = guard.entry_guard()
    else {
        return None;
    };
    if arguments.len() != ports {
        return None;
    }
    let block = context.get_region(region).iter(context.clone()).next()?;
    let terminator = context.get_op(*block.op_ids().last()?);
    let operands = terminator.operands();
    let first = operands.len().checked_sub(ports)?;
    Some((region, arguments, operands[first..].to_vec()))
}

/// Whether `value` is a λ or δ node of the module. The module is one region:
/// what it defines is in scope everywhere inside it, so it dominates every use
/// without appearing in any function's dominator tree.
pub fn is_module_level(context: &Context, value: ValueId) -> bool {
    context
        .get_value(value)
        .defining_op()
        .and_then(|op| context.parent_block(op))
        .and_then(|block| context.parent_region(block))
        .and_then(|region| context.get_region(region).parent_op())
        .is_some_and(|parent| context.get_op(parent).is::<crate::builtin::ModuleOp>())
}

/// Whether a definition in `block` — `def`, or a block argument where there is
/// none — is in scope at `op`.
///
/// The region tree is the dominance: what a block defines is in scope in that
/// block after the definition, and inside the regions the operations after it
/// hold. Nothing here computes a dominator tree, because a structured region has
/// one block and a block reaches only what it encloses.
pub fn precedes(context: &Context, block: BlockId, def: Option<OpId>, op: OpId) -> bool {
    let Some(ob) = context.get_op(op).parent_block() else {
        return false;
    };
    if block == ob {
        // A block argument precedes every op in its block.
        return def.is_none_or(|def| context.get_block(block).is_before(def, op));
    }
    let Some(holder) = holder_in(context, block, ob) else {
        return false;
    };
    def.is_none_or(|def| context.get_block(block).is_before(def, holder))
}

/// The operation of `block` whose regions transitively contain `inner`.
pub fn holder_in(context: &Context, block: BlockId, inner: BlockId) -> Option<OpId> {
    let mut current = inner;
    loop {
        let region = context.parent_region(current)?;
        let holder = context.get_region(region).parent_op()?;
        let parent = context.get_op(holder).parent_block()?;
        if parent == block {
            return Some(holder);
        }
        current = parent;
    }
}
