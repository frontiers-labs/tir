//! Cost-guided `TreeExtractor` materialization and IR edit publishing for EqSat.

use std::collections::{HashMap, HashSet};

use tir_egglog::{
    ArcSort, Color, Database, Enode, NativeIdentity, TableSchema, Value,
    extract::{DagCostModel, MonoidCost, TreeCostModelFromDag, TreeExtractor},
    sort::BigInt,
};

use super::generated::Expr;
use crate::passes::common::{
    Alias, CheckedEdit, Local, Replacement, UseSite, apply_uses, check_removals, check_uses,
    erase_operations, literal, placed_op,
};

use crate::{
    BlockId, Context, NewOp, OpHandle, OpId, Operation, OperationRef, PassError, RegionId,
    Speculatable, ValueId,
    attributes::{AttributeValue, NamedAttribute},
    builtin::ops,
    schema::Argument,
};

type DefinitionRow = (OpId, ValueId, Expr);
type Definition = (OpHandle, ValueId, Expression);

enum Expression {
    Read(ValueId),
    Constant(NativeIdentity, &'static str, AttributeValue),
    Unary(&'static crate::schema::OpSchema, Box<Expression>),
    Binary(&'static crate::schema::OpSchema, Box<[Expression; 2]>),
}

// ── Cost Model (`OpCost`-Driven, Zero Base-Value Cost) ───────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct EqSatCost {
    compute: u32,
    ops: u16,
}

impl MonoidCost for EqSatCost {
    fn identity() -> Self {
        Self { compute: 0, ops: 0 }
    }

    fn combine(self, other: &Self) -> Self {
        Self {
            compute: self.compute.saturating_add(other.compute),
            ops: self.ops.saturating_add(other.ops),
        }
    }
}

struct OpCostModel<'ctx> {
    context: &'ctx Context,
}

impl DagCostModel<EqSatCost> for OpCostModel<'_> {
    fn base_value_cost(&self, _: &Database, _: &ArcSort, _: Value) -> EqSatCost {
        EqSatCost::identity()
    }

    fn enode_cost(&self, db: &Database, func: &TableSchema, enode: &Enode<'_>) -> EqSatCost {
        match func.name() {
            "Expr::Const" | "Expr::CustomConst" | "Expr::Pi" => EqSatCost { compute: 0, ops: 0 },
            "Expr::Var" => EqSatCost { compute: 1, ops: 0 },
            "Expr::Unary" | "Expr::Bin" => {
                let kind = db.base_value::<NativeIdentity>(enode.children[0]);
                let cost = self
                    .context
                    .schema_for_kind(kind)
                    .map_or(1, |schema| schema.cost);
                EqSatCost {
                    compute: cost.saturating_mul(10),
                    ops: 1,
                }
            }
            "Expr::Compare" => EqSatCost {
                compute: 10,
                ops: 1,
            },
            "Expr::Store" | "Expr::LoadVal" | "Expr::LoadState" | "Expr::Gamma2"
            | "Expr::StateGamma2" | "Expr::LoadGamma" => EqSatCost {
                compute: 100_000,
                ops: 100,
            },
            _ => EqSatCost {
                compute: 10,
                ops: 1,
            },
        }
    }
}

// ── Publish Entrypoint ───────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub(super) fn publish(
    context: &Context,
    db: &Database,
    custom_defs: Vec<DefinitionRow>,
    edits: Vec<Alias>,
    local: Vec<Local>,
    redirected: &[ValueId],
    erase: &[OpId],
    pinned_vals: &[ValueId],
    pinned_ops: &[OpId],
    live_ops: &[OpId],
    lifted: &[ValueId],
    load_ops: &HashMap<OpId, (ValueId, ValueId)>,
) -> Result<(), PassError> {
    let checked_edits = check_uses(context, edits, local, erase)?;
    let removals = check_removals(context, redirected, erase, &checked_edits)?;
    let definitions = check_definitions(context, custom_defs, redirected, &checked_edits)?;
    apply_uses(context, &checked_edits)?;
    for (op, result, expression) in definitions {
        let old = OperationRef::new(op);
        materialize(context, &old, result, expression, Some(result))?;
    }
    erase_operations(context, removals)?;

    // Run single flat `Color::BASE` `TreeExtractor` over `ValExpr` e-classes.
    extract_and_apply(
        context,
        db,
        &checked_edits,
        redirected,
        erase,
        pinned_vals,
        pinned_ops,
        live_ops,
        lifted,
        load_ops,
    )
}

// ── Cost-Guided Extraction & Materialization ─────────────────────────────────

struct ExtractionState<'a> {
    context: &'a Context,
    db: &'a Database,
    extractor: TreeExtractor<'a, EqSatCost>,
    val_to_class: HashMap<ValueId, Value>,
    block_pos: HashMap<OpId, (BlockId, usize)>,
    graph_deps: HashSet<(RegionId, OpId, OpId)>,
    existing: HashMap<Value, Vec<ValueId>>,
    erased: HashSet<OpId>,
    redirected: HashSet<ValueId>,
    to_erase: Vec<OpHandle>,
}

fn load_val_expr_map(db: &Database) -> HashMap<ValueId, Value> {
    let mut map = HashMap::new();
    let Ok(table) = db.table("ValExpr") else {
        return map;
    };
    for row in db.rows_in(Color::BASE, table) {
        if row.is_subsumed() {
            continue;
        }
        if let Ok(raw_v) = u32::try_from(db.base_value::<i64>(row[0])) {
            map.insert(ValueId::from_number(raw_v), db.find_in(Color::BASE, row[1]));
        }
    }
    map
}

fn load_graph_deps(db: &Database) -> HashSet<(RegionId, OpId, OpId)> {
    let mut deps = HashSet::new();
    let Ok(table) = db.table("DependsOn") else {
        return deps;
    };
    for row in db.rows_in(Color::BASE, table) {
        if let (Ok(r), Ok(a), Ok(b)) = (
            u32::try_from(db.base_value::<i64>(row[0])),
            u32::try_from(db.base_value::<i64>(row[1])),
            u32::try_from(db.base_value::<i64>(row[2])),
        ) {
            deps.insert((
                RegionId::from_number(r),
                OpId::from_number(a),
                OpId::from_number(b),
            ));
        }
    }
    deps
}

fn collect_extraction_order(
    context: &Context,
    live_ops: &[OpId],
) -> (Vec<OpId>, HashMap<OpId, (BlockId, usize)>) {
    let mut block_pos: HashMap<OpId, (BlockId, usize)> = HashMap::new();
    let mut ops_by_block: HashMap<BlockId, Vec<(usize, OpId)>> = HashMap::new();
    let mut ops_by_graph: HashMap<RegionId, Vec<OpId>> = HashMap::new();
    let mut seen_blocks: HashSet<BlockId> = HashSet::new();

    for &id in live_ops {
        if !context.has_operation(id) {
            continue;
        }
        if let Some(block) = context.parent_block(id) {
            if seen_blocks.insert(block) {
                for (pos, bid) in context.get_block(block).op_ids().into_iter().enumerate() {
                    block_pos.insert(bid, (block, pos));
                }
            }
            if let Some(&(_, pos)) = block_pos.get(&id) {
                ops_by_block.entry(block).or_default().push((pos, id));
            }
        } else if let Some(region) = context.parent_nodes_region(id) {
            ops_by_graph.entry(region).or_default().push(id);
        }
    }

    let mut ordered_ops = Vec::new();
    let mut blocks: Vec<BlockId> = ops_by_block.keys().copied().collect();
    blocks.sort_unstable();
    for block in blocks {
        let list = ops_by_block.get_mut(&block).unwrap();
        list.sort_unstable();
        ordered_ops.extend(list.iter().map(|&(_, id)| id));
    }
    let mut regions: Vec<RegionId> = ops_by_graph.keys().copied().collect();
    regions.sort_unstable();
    for region in regions {
        let list = ops_by_graph.get_mut(&region).unwrap();
        list.sort_unstable();
        ordered_ops.extend(list.iter().copied());
    }
    (ordered_ops, block_pos)
}

#[allow(clippy::too_many_arguments)]
fn extract_and_apply(
    context: &Context,
    db: &Database,
    prior_edits: &[CheckedEdit],
    redirected: &[ValueId],
    erased: &[OpId],
    pinned_vals: &[ValueId],
    pinned_ops: &[OpId],
    live_ops: &[OpId],
    lifted: &[ValueId],
    load_ops: &HashMap<OpId, (ValueId, ValueId)>,
) -> Result<(), PassError> {
    let val_to_class = load_val_expr_map(db);
    if val_to_class.is_empty() {
        return Ok(());
    }

    let extractor = TreeExtractor::with_cost_model_in(
        db,
        Color::BASE,
        TreeCostModelFromDag(OpCostModel { context }),
    );
    let (ordered_ops, block_pos) = collect_extraction_order(context, live_ops);
    let mut existing: HashMap<Value, Vec<ValueId>> = HashMap::new();
    for (&vid, &class) in &val_to_class {
        if context.has_value(vid)
            && context.get_value(vid).defining_op().is_none()
            && redirected.binary_search(&vid).is_err()
        {
            existing.entry(class).or_default().push(vid);
        }
    }

    let mut state = ExtractionState {
        context,
        db,
        extractor,
        val_to_class,
        block_pos,
        graph_deps: load_graph_deps(db),
        existing,
        erased: erased.iter().copied().collect(),
        redirected: redirected.iter().copied().collect(),
        to_erase: Vec::new(),
    };

    for op_id in ordered_ops {
        if state.erased.contains(&op_id)
            || pinned_ops.binary_search(&op_id).is_ok()
            || !context.has_operation(op_id)
        {
            continue;
        }
        if let Some(&(state_in, val_res)) = load_ops.get(&op_id) {
            extract_load_op(&mut state, op_id, state_in, val_res, pinned_vals)?;
            continue;
        }
        if let Some(op) = placed_op(context, op_id)
            && !op.regions().is_empty()
        {
            extract_region_op(&mut state, &op, pinned_vals, lifted)?;
            continue;
        }
        extract_single_op(&mut state, op_id, prior_edits, pinned_vals, lifted)?;
    }

    erase_operations(context, state.to_erase)
}

fn extract_load_op(
    state: &mut ExtractionState<'_>,
    op_id: OpId,
    state_in: ValueId,
    val_res: ValueId,
    pinned_vals: &[ValueId],
) -> Result<(), PassError> {
    let context = state.context;
    let Some(op) = placed_op(context, op_id) else {
        return Ok(());
    };
    if pinned_vals.binary_search(&val_res).is_ok() || state.redirected.contains(&val_res) {
        return Ok(());
    }
    let Some(&eclass) = state.val_to_class.get(&val_res) else {
        return Ok(());
    };
    let vis = VisibilityCtx {
        context,
        block_pos: &state.block_pos,
        graph_deps: &state.graph_deps,
        erased: &state.erased,
        redirected: &state.redirected,
    };
    let expected_ty = context.get_value(val_res).ty();
    let target = if let Some(prev) =
        find_visible_existing(&state.existing, eclass, val_res, op_id, expected_ty, &vis)
    {
        Some(Expression::Read(prev))
    } else if state
        .extractor
        .cost_of(eclass)
        .is_some_and(|c| c.compute < 100_000)
    {
        if let Some(existing_val) = state
            .val_to_class
            .iter()
            .filter_map(|(&v, &c)| {
                (c == eclass
                    && v != val_res
                    && context.has_value(v)
                    && context.get_value(v).ty() == expected_ty
                    && vis.visible(v, op_id))
                .then_some(v)
            })
            .min()
        {
            Some(Expression::Read(existing_val))
        } else {
            decode_extracted(
                context,
                state.db,
                &state.extractor,
                eclass,
                val_res,
                op_id,
                &state.existing,
                &vis,
                true,
            )?
        }
    } else {
        None
    };

    let Some(expr) = target else {
        state.existing.entry(eclass).or_default().push(val_res);
        return Ok(());
    };
    let replacement = match expr {
        Expression::Read(v) if v != val_res => v,
        Expression::Read(_) => {
            state.existing.entry(eclass).or_default().push(val_res);
            return Ok(());
        }
        other => {
            let anchor = OperationRef::new(op.clone());
            materialize(context, &anchor, val_res, other, None)?
        }
    };
    replace_all_uses(context, val_res, replacement);
    state.redirected.insert(val_res);
    if let Some(&state_out) = op.results().get(1) {
        replace_all_uses(context, state_out, state_in);
        state.redirected.insert(state_out);
    }
    state.erased.insert(op_id);
    state.to_erase.push(op);
    Ok(())
}

fn extract_region_op(
    state: &mut ExtractionState<'_>,
    op: &OpHandle,
    pinned_vals: &[ValueId],
    lifted: &[ValueId],
) -> Result<(), PassError> {
    let context = state.context;
    let op_id = op.id;
    for res in op.results() {
        if lifted.binary_search(&res).is_err()
            || pinned_vals.binary_search(&res).is_ok()
            || state.redirected.contains(&res)
        {
            continue;
        }
        let Some(&eclass) = state.val_to_class.get(&res) else {
            continue;
        };
        let Some(&best_cost) = state.extractor.cost_of(eclass) else {
            continue;
        };
        if best_cost.compute >= 100_000 {
            continue;
        }
        let vis = VisibilityCtx {
            context,
            block_pos: &state.block_pos,
            graph_deps: &state.graph_deps,
            erased: &state.erased,
            redirected: &state.redirected,
        };
        let Some(expr) = decode_extracted(
            context,
            state.db,
            &state.extractor,
            eclass,
            res,
            op_id,
            &state.existing,
            &vis,
            true,
        )?
        else {
            continue;
        };
        let replacement = match expr {
            Expression::Read(v) if v != res => v,
            Expression::Read(_) => continue,
            Expression::Constant(kind, attr, value) => {
                let anchor = OperationRef::new(op.clone());
                materialize(
                    context,
                    &anchor,
                    res,
                    Expression::Constant(kind, attr, value),
                    None,
                )?
            }
            _ => continue,
        };
        replace_all_uses(context, res, replacement);
        state.redirected.insert(res);
    }
    Ok(())
}

fn extract_single_op(
    state: &mut ExtractionState<'_>,
    op_id: OpId,
    prior_edits: &[CheckedEdit],
    pinned_vals: &[ValueId],
    lifted: &[ValueId],
) -> Result<(), PassError> {
    let context = state.context;
    let db = state.db;
    let Some(op) = placed_op(context, op_id) else {
        return Ok(());
    };
    let results = op.results();
    if !op.regions().is_empty() || results.len() != 1 {
        return Ok(());
    }
    let res = results[0];
    if pinned_vals.binary_search(&res).is_ok() || state.redirected.contains(&res) {
        return Ok(());
    }
    let Some(&eclass) = state.val_to_class.get(&res) else {
        return Ok(());
    };
    let Some(kind) = context.operation_kind(op_id) else {
        return Ok(());
    };
    // Constants are already at cost 0; leave untouched so non-canonical spellings or
    // outer-region constants aren't reused in place of freshly folded canonical literals.
    if context.kind_has_interface::<dyn crate::ConstantLike>(&kind) {
        return Ok(());
    }
    let Some(&best_cost) = state.extractor.cost_of(eclass) else {
        return Ok(());
    };
    let Some((best_table, best_row)) = state.extractor.best_row(eclass) else {
        return Ok(());
    };

    let use_offset = prior_edits.partition_point(|edit| edit.0 < UseSite::Operand(op_id, 0));
    let has_scoped_operand = prior_edits[use_offset..]
        .iter()
        .take_while(|edit| matches!(edit.0, UseSite::Operand(user, _) if user == op_id))
        .any(|edit| matches!(edit.1, Replacement::Integer(..)));

    let is_lifted = lifted.binary_search(&res).is_ok();
    let own_cost = compute_own_cost(
        context,
        &op,
        kind,
        is_lifted,
        &state.val_to_class,
        &state.extractor,
    );
    let vis = VisibilityCtx {
        context,
        block_pos: &state.block_pos,
        graph_deps: &state.graph_deps,
        erased: &state.erased,
        redirected: &state.redirected,
    };

    // Case 1: Extracted root is an existing opaque `Expr::Var(v)` — direct identity!
    if db.schema().table(best_table).name() == "Expr::Var"
        && let Ok(raw_v) = u32::try_from(db.base_value::<i64>(best_row[0]))
    {
        let target = ValueId::from_number(raw_v);
        if target != res
            && context.has_value(target)
            && context.get_value(target).ty() == context.get_value(res).ty()
            && vis.visible(target, op_id)
        {
            replace_all_uses(context, res, target);
            state.redirected.insert(res);
            if is_lifted {
                state.erased.insert(op_id);
                state.to_erase.push(op);
            }
            return Ok(());
        }
    }

    // Case 2: Another earlier dominating value already computes this e-class (CSE / fold reuse).
    if !has_scoped_operand
        && best_cost <= own_cost
        && (is_lifted || best_cost < own_cost)
        && let Some(prev) = find_visible_existing(
            &state.existing,
            eclass,
            res,
            op_id,
            context.get_value(res).ty(),
            &vis,
        )
    {
        replace_all_uses(context, res, prev);
        state.redirected.insert(res);
        if is_lifted {
            state.erased.insert(op_id);
            state.to_erase.push(op);
        }
        return Ok(());
    }

    // Case 3: Extracted expression is strictly cheaper than `op`.
    let best_name = db.schema().table(best_table).name();
    if best_cost < own_cost
        && !(has_scoped_operand && matches!(best_name, "Expr::Unary" | "Expr::Bin"))
        && let Some(expr) = decode_extracted(
            context,
            db,
            &state.extractor,
            eclass,
            res,
            op_id,
            &state.existing,
            &vis,
            true,
        )?
    {
        match expr {
            Expression::Read(target) if target != res => {
                replace_all_uses(context, res, target);
                state.redirected.insert(res);
                if is_lifted {
                    state.erased.insert(op_id);
                    state.to_erase.push(op);
                }
            }
            Expression::Read(_) => {
                state.existing.entry(eclass).or_default().push(res);
            }
            other => {
                let old_pos = state.block_pos.get(&op_id).copied();
                let old = OperationRef::new(op);
                let new_op = materialize(context, &old, res, other, Some(res))?;
                if let (Some(pos), Some(def_op)) =
                    (old_pos, context.get_value(new_op).defining_op())
                {
                    state.block_pos.insert(def_op, pos);
                }
                state.existing.entry(eclass).or_default().push(res);
            }
        }
        return Ok(());
    }

    // Case 4: `op` is already optimal — record for downstream CSE.
    state.existing.entry(eclass).or_default().push(res);
    Ok(())
}

fn compute_own_cost(
    context: &Context,
    op: &OpHandle,
    kind: NativeIdentity,
    is_lifted: bool,
    val_to_class: &HashMap<ValueId, Value>,
    extractor: &TreeExtractor<'_, EqSatCost>,
) -> EqSatCost {
    if !is_lifted {
        return EqSatCost { compute: 1, ops: 0 };
    }
    let op_cost = context
        .schema_for_kind(kind)
        .map_or(1, |schema| schema.cost);
    let mut total = EqSatCost {
        compute: op_cost.saturating_mul(10),
        ops: 1,
    };
    for operand in op.operands() {
        let Some(&oc) = val_to_class.get(&operand) else {
            return EqSatCost {
                compute: u32::MAX,
                ops: u16::MAX,
            };
        };
        let Some(&child_cost) = extractor.cost_of(oc) else {
            return EqSatCost {
                compute: u32::MAX,
                ops: u16::MAX,
            };
        };
        total = total.combine(&child_cost);
    }
    total
}

struct VisibilityCtx<'a> {
    context: &'a Context,
    block_pos: &'a HashMap<OpId, (BlockId, usize)>,
    graph_deps: &'a HashSet<(RegionId, OpId, OpId)>,
    erased: &'a HashSet<OpId>,
    redirected: &'a HashSet<ValueId>,
}

impl VisibilityCtx<'_> {
    fn visible(&self, value: ValueId, anchor: OpId) -> bool {
        if !self.context.has_value(value) || self.redirected.contains(&value) {
            return false;
        }
        let Some(anchor_region) = self.context.region_of_op(anchor) else {
            return false;
        };
        let anchor_block = self.context.parent_block(anchor);
        if let Some(def_op) = self.context.get_value(value).defining_op() {
            if def_op == anchor
                || self.erased.contains(&def_op)
                || !self.context.has_operation(def_op)
            {
                return false;
            }
            if let Some(def_block) = self.context.parent_block(def_op) {
                if anchor_block == Some(def_block) {
                    let Some(&(_, def_p)) = self.block_pos.get(&def_op) else {
                        return false;
                    };
                    let Some(&(_, anc_p)) = self.block_pos.get(&anchor) else {
                        return false;
                    };
                    return def_p < anc_p;
                }
                let Some(def_region) = self.context.parent_region(def_block) else {
                    return false;
                };
                return self.context.get_region(def_region).entry_block() == def_block
                    && self.context.region_under(anchor_region, def_region);
            }
            if let Some(def_region) = self.context.parent_nodes_region(def_op) {
                if self.context.parent_nodes_region(anchor) == Some(def_region) {
                    return !self.graph_deps.contains(&(def_region, def_op, anchor));
                }
                return self.context.region_under(anchor_region, def_region);
            }
            return false;
        }
        // Block argument or region port.
        if let Some(def_region) = self.context.region_of_port(value) {
            return self.context.region_under(anchor_region, def_region);
        }
        if let Some(def_block) = self.context.block_of_argument(value) {
            if anchor_block == Some(def_block) {
                return true;
            }
            let Some(def_region) = self.context.parent_region(def_block) else {
                return false;
            };
            return self.context.get_region(def_region).entry_block() == def_block
                && self.context.region_under(anchor_region, def_region);
        }
        false
    }
}

fn find_visible_existing(
    existing: &HashMap<Value, Vec<ValueId>>,
    eclass: Value,
    exclude: ValueId,
    anchor: OpId,
    expected_ty: crate::TypeId,
    vis: &VisibilityCtx<'_>,
) -> Option<ValueId> {
    let list = existing.get(&eclass)?;
    list.iter().copied().find(|&v| {
        v != exclude
            && vis.context.has_value(v)
            && vis.context.get_value(v).ty() == expected_ty
            && vis.visible(v, anchor)
    })
}

#[allow(clippy::too_many_arguments)]
fn decode_extracted(
    context: &Context,
    db: &Database,
    extractor: &TreeExtractor<'_, EqSatCost>,
    eclass: Value,
    expected: ValueId,
    anchor: OpId,
    existing: &HashMap<Value, Vec<ValueId>>,
    vis: &VisibilityCtx<'_>,
    is_root: bool,
) -> Result<Option<Expression>, PassError> {
    let class = db.find_in(Color::BASE, eclass);
    let expected_ty = context.get_value(expected).ty();
    let Some((table, row)) = extractor.best_row(class) else {
        return Ok(None);
    };
    let name = db.schema().table(table).name();

    // For non-root subterms, reuse any existing visible value in the same e-class
    // (unless it's a Const, which we materialize as a fresh literal at the anchor).
    if !is_root
        && name != "Expr::Const"
        && name != "Expr::CustomConst"
        && let Some(prev) =
            find_visible_existing(existing, class, expected, anchor, expected_ty, vis)
    {
        return Ok(Some(Expression::Read(prev)));
    }

    match name {
        "Expr::Pi" => decode_extracted(
            context, db, extractor, row[1], expected, anchor, existing, vis, is_root,
        ),
        "Expr::Var" => {
            let Ok(raw_v) = u32::try_from(db.base_value::<i64>(row[0])) else {
                return Ok(None);
            };
            let v = ValueId::from_number(raw_v);
            Ok((v != expected
                && context.has_value(v)
                && context.get_value(v).ty() == expected_ty
                && vis.visible(v, anchor))
            .then_some(Expression::Read(v)))
        }
        "Expr::Const" => {
            let bits = db.base_value::<BigInt>(row[1]);
            Ok(literal(context, expected, bits).ok().map(|value| {
                Expression::Constant(ops::ConstantOp::native_identity(), "value", value)
            }))
        }
        "Expr::CustomConst" => {
            let kind = db.base_value::<NativeIdentity>(row[0]);
            let attr = db.base_value::<String>(row[1]);
            let bits = db.base_value::<BigInt>(row[2]);
            Ok(Some(custom_constant(context, expected, kind, &attr, bits)?))
        }
        "Expr::Unary" => {
            let kind = db.base_value::<NativeIdentity>(row[0]);
            if !context.kind_has_interface::<dyn Speculatable>(&kind) {
                return Ok(None);
            }
            let Some(schema) = context.schema_for_kind(kind) else {
                return Ok(None);
            };
            let Some(child) = decode_extracted(
                context, db, extractor, row[2], expected, anchor, existing, vis, false,
            )?
            else {
                return Ok(None);
            };
            Ok(Some(Expression::Unary(schema, Box::new(child))))
        }
        "Expr::Bin" => {
            let kind = db.base_value::<NativeIdentity>(row[0]);
            if !context.kind_has_interface::<dyn Speculatable>(&kind) {
                return Ok(None);
            }
            let Some(schema) = context.schema_for_kind(kind) else {
                return Ok(None);
            };
            let Some(left) = decode_extracted(
                context, db, extractor, row[2], expected, anchor, existing, vis, false,
            )?
            else {
                return Ok(None);
            };
            let Some(right) = decode_extracted(
                context, db, extractor, row[3], expected, anchor, existing, vis, false,
            )?
            else {
                return Ok(None);
            };
            Ok(Some(Expression::Binary(schema, Box::new([left, right]))))
        }
        _ => Ok(None),
    }
}

fn replace_all_uses(context: &Context, old: ValueId, new: ValueId) {
    let uses = context.uses_of(old);
    for usage in uses {
        context.set_op_operand(usage.op, usage.index, new);
    }
    let result_uses = context.result_uses(old);
    for (region, slot) in result_uses {
        let mut results = context.get_region(region).results();
        if let Some(entry) = results.get_mut(slot) {
            *entry = new;
        }
        context.set_region_results(region, results);
    }
}

// ── Custom Rule Collection Publishing ────────────────────────────────────────

fn check_definitions(
    context: &Context,
    mut forms: Vec<DefinitionRow>,
    redirected: &[ValueId],
    edits: &[CheckedEdit],
) -> Result<Vec<Definition>, PassError> {
    forms.sort_unstable_by_key(|(id, _, expr)| (*id, !matches!(expr, Expr::CustomConst(..))));
    let mut definitions: Vec<Definition> = Vec::new();
    let mut custom_definition = None;
    for (id, result, expr) in forms {
        let custom = matches!(expr, Expr::CustomConst(..));
        if redirected.binary_search(&result).is_ok() || (custom_definition == Some(id) && !custom) {
            continue;
        }
        let op = definition(context, id, result)?;
        let Some(expression) = decode_custom(context, result, expr, redirected)? else {
            continue;
        };
        let use_offset = edits.partition_point(|edit| edit.0 < UseSite::Operand(id, 0));
        if matches!(expression, Expression::Binary(..))
            && edits[use_offset..]
                .iter()
                .take_while(|edit| matches!(edit.0, UseSite::Operand(user, _) if id == user))
                .any(|edit| matches!(edit.1, Replacement::Integer(..)))
        {
            continue;
        }
        if definitions.last().is_some_and(|row| row.0.id == id) {
            return Err(PassError::RewriteFailed(id));
        }
        if custom {
            custom_definition = Some(id);
        }
        definitions.push((op, result, expression));
    }
    Ok(definitions)
}

fn decode_custom(
    context: &Context,
    expected: ValueId,
    expr: Expr,
    redirected: &[ValueId],
) -> Result<Option<Expression>, PassError> {
    Ok(match expr {
        Expr::Var(id) => {
            let value = ValueId::from(id);
            (context.has_value(value)
                && redirected.binary_search(&value).is_err()
                && context.get_value(value).ty() == context.get_value(expected).ty())
            .then_some(Expression::Read(value))
        }
        Expr::Const(_, bits) => literal(context, expected, bits)
            .ok()
            .map(|value| Expression::Constant(ops::ConstantOp::native_identity(), "value", value)),
        Expr::Unary(kind, _, child) => {
            let Some(schema) = context.schema_for_kind(kind.declaration()) else {
                return Ok(None);
            };
            decode_custom(context, expected, *child, redirected)?
                .map(|c| Expression::Unary(schema, Box::new(c)))
        }
        Expr::Bin(kind, _, left, right) => {
            let Some(schema) = context.schema_for_kind(kind.declaration()) else {
                return Ok(None);
            };
            let left = decode_custom(context, expected, *left, redirected)?;
            let right = decode_custom(context, expected, *right, redirected)?;
            match (left, right) {
                (Some(left), Some(right)) => {
                    Some(Expression::Binary(schema, Box::new([left, right])))
                }
                _ => None,
            }
        }
        Expr::CustomConst(kind, attr, bits) => Some(custom_constant(
            context,
            expected,
            kind.declaration(),
            &attr,
            bits,
        )?),
        Expr::Compare(..)
        | Expr::Pi(..)
        | Expr::Store(..)
        | Expr::LoadVal(..)
        | Expr::LoadState(..)
        | Expr::Gamma2(..)
        | Expr::StateGamma2(..)
        | Expr::LoadGamma(..) => None,
    })
}

fn custom_constant(
    context: &Context,
    expected: ValueId,
    kind: NativeIdentity,
    attr: &str,
    bits: BigInt,
) -> Result<Expression, PassError> {
    let schema = context.schema_for_kind(kind).ok_or_else(|| {
        PassError::InvalidRuleSet(format!(
            "unregistered constant declaration '{}'",
            kind.as_str()
        ))
    })?;
    let ty = context.get_value(expected).ty();
    if !context.kind_has_interface::<dyn crate::ConstantLike>(&kind)
        || !schema.regions.is_empty()
        || schema
            .signature
            .substitution(context.terms(), &[], &[Argument::Fixed(ty)])
            .is_none()
    {
        return Err(PassError::InvalidRuleSet(format!(
            "'{}' cannot construct a constant with this result type",
            kind.as_str()
        )));
    }
    let field = schema
        .attributes
        .iter()
        .find(|field| field.name == attr && matches!(field.ty, "Int" | "?Int"))
        .ok_or_else(|| {
            PassError::InvalidRuleSet(format!(
                "'{}' has no integer attribute '{attr}'",
                kind.as_str()
            ))
        })?;
    if schema
        .attributes
        .iter()
        .any(|other| other.name != attr && !other.ty.starts_with('?'))
    {
        return Err(PassError::InvalidRuleSet(format!(
            "'{}' requires additional constant attributes",
            kind.as_str()
        )));
    }
    Ok(Expression::Constant(
        kind,
        field.name,
        literal(context, expected, bits)?,
    ))
}

fn materialize(
    context: &Context,
    anchor: &OperationRef,
    expected: ValueId,
    expression: Expression,
    root_result: Option<ValueId>,
) -> Result<ValueId, PassError> {
    let (identity, operands, attributes) = match expression {
        Expression::Read(value) if root_result.is_none() => return Ok(value),
        Expression::Read(_) => return Err(PassError::RewriteFailed(anchor.op().id)),
        Expression::Constant(kind, attr, value) => (
            kind,
            Vec::new(),
            vec![NamedAttribute::new(context.intern(attr), value)],
        ),
        Expression::Unary(schema, child) => {
            let operand = materialize(context, anchor, expected, *child, None)?;
            ((schema.declaration)(), vec![operand], Vec::new())
        }
        Expression::Binary(schema, children) => {
            let mut operands = Vec::with_capacity(2);
            for child in *children {
                operands.push(materialize(context, anchor, expected, child, None)?);
            }
            ((schema.declaration)(), operands, Vec::new())
        }
    };
    let result = root_result.unwrap_or_else(|| {
        context
            .create_value(context.get_value(expected).ty(), None)
            .id()
    });
    let op = context.add_operation(
        crate::InsertionPoint::Root,
        NewOp::from_declaration(identity, operands, vec![result], Vec::new(), attributes),
    );
    let op = op.as_dyn_op();
    if root_result.is_some() {
        context.replace_op_keeping_results(anchor, op.as_ref())?;
    } else {
        context.insert(context.insertion_point(anchor.op().id), op.id());
    }
    Ok(result)
}

fn definition(context: &Context, id: OpId, result: ValueId) -> Result<OpHandle, PassError> {
    let op = placed_op(context, id).ok_or(PassError::RewriteFailed(id))?;
    if !context.has_value(result) || op.results().as_slice() != [result] || !op.regions().is_empty()
    {
        return Err(PassError::RewriteFailed(id));
    }
    Ok(op)
}
