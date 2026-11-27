//! Cost-guided `TreeExtractor` materialization and IR edit publishing for EqSat.

use tir_egglog::{
    ArcSort, Color, Database, Enode, IndexId, NativeIdentity, SortKind, TableId, TableSchema,
    Value,
    extract::{DagCostModel, MonoidCost, TreeCostModelFromDag, TreeExtractor},
    sort::BigInt,
};

use super::generated::{Expr, table_eq_sat_load_op, table_val_expr};
use crate::passes::common::{
    Alias, CheckedEdit, Local, Replacement, UseSite, apply_uses, check_removals, check_uses,
    erase_operations, literal, placed_op, rule_error,
};

use crate::{
    Context, NewOp, OpHandle, OpId, Operation, OperationRef, PassError, Speculatable, ValueId,
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
    db: &mut Database,
    custom_defs: Vec<DefinitionRow>,
    edits: Vec<Alias>,
    local: Vec<Local>,
    redirected: &[ValueId],
    erase: &[OpId],
    pinned_vals: &[ValueId],
    pinned_ops: &[OpId],
    live_ops: Vec<OpId>,
    lifted: &[ValueId],
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
    )
}

// ── Cost-Guided Extraction & Materialization ─────────────────────────────────

struct ExtractionState<'a> {
    context: &'a Context,
    extractor: TreeExtractor<'a, EqSatCost>,
    rows: ExtractionRows<'a>,
    decisions: PublicationDecisions<'a>,
}

struct PublicationDecisions<'a> {
    db: Database,
    candidates: TableId,
    by_class: IndexId,
    pending_erasures: TableId,
    new_redirects: TableId,
    prior_erasures: &'a [OpId],
    prior_redirects: &'a [ValueId],
}

impl<'a> PublicationDecisions<'a> {
    fn new(prior_erasures: &'a [OpId], prior_redirects: &'a [ValueId]) -> Self {
        let mut db = Database::new();
        let number = db.add_sort("u32", SortKind::base::<u32>());
        let candidates = db.add_relation("Candidate", &[number, number]);
        // The kept index uses row order to break ties within a class.
        let by_class = db.add_kept_index(candidates, &[0], &[]);
        let pending_erasures = db.add_relation("Erase", &[number]);
        let new_redirects = db.add_relation("Redirect", &[number]);
        Self {
            db,
            candidates,
            by_class,
            pending_erasures,
            new_redirects,
            prior_erasures,
            prior_redirects,
        }
    }

    fn add_candidate(&mut self, class: Value, value: ValueId) -> Result<(), PassError> {
        // Class IDs belong to the extraction snapshot. Only their numbers cross into this DB.
        let row = [self.db.base(class.raw()), self.db.base(value.number())];
        self.db.insert(self.candidates, &row).map_err(rule_error)
    }

    fn candidates(&self, class: Value) -> impl Iterator<Item = ValueId> + '_ {
        self.db
            .listed(self.by_class, &[self.db.base(class.raw())])
            .map(|row| ValueId::from_number(self.db.base_value(row[1])))
    }

    fn erase(&mut self, op: OpId) -> Result<(), PassError> {
        let key = [self.db.base(op.number())];
        self.db
            .insert(self.pending_erasures, &key)
            .map_err(rule_error)
    }

    fn is_erased(&self, op: OpId) -> bool {
        self.prior_erasures.binary_search(&op).is_ok()
            || self
                .db
                .lookup(self.pending_erasures, &[self.db.base(op.number())])
                .is_some()
    }

    fn erasures(&self) -> impl Iterator<Item = OpId> + '_ {
        self.db
            .rows(self.pending_erasures)
            .map(|row| OpId::from_number(self.db.base_value(row[0])))
    }

    fn redirect(&mut self, value: ValueId) -> Result<(), PassError> {
        let key = [self.db.base(value.number())];
        self.db.insert(self.new_redirects, &key).map_err(rule_error)
    }

    fn is_redirected(&self, value: ValueId) -> bool {
        self.prior_redirects.binary_search(&value).is_ok()
            || self
                .db
                .lookup(self.new_redirects, &[self.db.base(value.number())])
                .is_some()
    }
}

struct ExtractionRows<'a> {
    db: &'a Database,
    val_expr: TableId,
    by_value: IndexId,
    by_class: IndexId,
    by_load: IndexId,
}

impl<'a> ExtractionRows<'a> {
    fn new(db: &'a mut Database) -> Result<Self, PassError> {
        let val_expr = table_val_expr(db).map_err(rule_error)?;
        let load_op = table_eq_sat_load_op(db).map_err(rule_error)?;
        let by_value = db.add_kept_index(val_expr, &[0], &[]);
        let by_class = db.add_kept_index(val_expr, &[1], &[]);
        let by_load = db.add_kept_index(load_op, &[0], &[]);
        Ok(Self {
            db,
            val_expr,
            by_value,
            by_class,
            by_load,
        })
    }

    fn class_of(&self, value: ValueId) -> Option<Value> {
        let key = self.db.base(i64::from(value.number()));
        let row = self
            .db
            .listed(self.by_value, &[key])
            .find(|row| !row.is_subsumed())?;
        Some(self.db.find_in(Color::BASE, row[1]))
    }

    fn values(&self) -> impl Iterator<Item = (ValueId, Value)> + '_ {
        self.db
            .rows_in(Color::BASE, self.val_expr)
            .filter(|row| !row.is_subsumed())
            .filter_map(|row| {
                let value = self.value_id(row[0]).ok()?;
                Some((value, self.db.find_in(Color::BASE, row[1])))
            })
    }

    fn values_in_class(&self, class: Value) -> impl Iterator<Item = ValueId> + '_ {
        self.db
            .listed(self.by_class, &[class])
            .filter(|row| !row.is_subsumed())
            .filter_map(|row| self.value_id(row[0]).ok())
    }

    fn load_op(&self, op: OpId) -> Result<Option<(ValueId, ValueId)>, PassError> {
        let key = self.db.base(i64::from(op.number()));
        self.db
            .listed(self.by_load, &[key])
            .filter(|row| !row.is_subsumed())
            .last()
            .map(|row| {
                let state = self.value_id(row[1]).map_err(rule_error)?;
                let result = self.value_id(row[2]).map_err(rule_error)?;
                Ok((state, result))
            })
            .transpose()
    }

    fn value_id(&self, value: Value) -> Result<ValueId, std::num::TryFromIntError> {
        u32::try_from(self.db.base_value::<i64>(value)).map(ValueId::from_number)
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_and_apply(
    context: &Context,
    db: &mut Database,
    prior_edits: &[CheckedEdit],
    redirected: &[ValueId],
    erased: &[OpId],
    pinned_vals: &[ValueId],
    pinned_ops: &[OpId],
    mut live_ops: Vec<OpId>,
    lifted: &[ValueId],
) -> Result<(), PassError> {
    let rows = ExtractionRows::new(db)?;
    let mut values = rows.values().peekable();
    if values.peek().is_none() {
        return Ok(());
    }

    let extractor = TreeExtractor::with_cost_model_in(
        rows.db,
        Color::BASE,
        TreeCostModelFromDag(OpCostModel { context }),
    );
    live_ops.sort_by_cached_key(|&id| {
        if let Some((block, position)) = context.block_placement(id) {
            (0, block.number(), position, id)
        } else if let Some(region) = context.parent_nodes_region(id) {
            (1, region.number(), 0, id)
        } else {
            (2, 0, 0, id)
        }
    });
    let mut decisions = PublicationDecisions::new(erased, redirected);
    for (vid, class) in values {
        if context.has_value(vid)
            && context.get_value(vid).defining_op().is_none()
            && redirected.binary_search(&vid).is_err()
        {
            decisions.add_candidate(class, vid)?;
        }
    }

    let mut state = ExtractionState {
        context,
        extractor,
        rows,
        decisions,
    };

    for op_id in live_ops {
        if state.decisions.is_erased(op_id)
            || pinned_ops.binary_search(&op_id).is_ok()
            || !context.has_operation(op_id)
        {
            continue;
        }
        if let Some((state_in, val_res)) = state.rows.load_op(op_id)? {
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

    let removals = state
        .decisions
        .erasures()
        .filter(|&id| context.has_operation(id))
        .map(|id| context.get_op(id))
        .collect();
    erase_operations(context, removals)
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
    if pinned_vals.binary_search(&val_res).is_ok() || state.decisions.is_redirected(val_res) {
        return Ok(());
    }
    let Some(eclass) = state.rows.class_of(val_res) else {
        return Ok(());
    };
    let vis = VisibilityCtx {
        context,
        db: state.rows.db,
        decisions: &state.decisions,
    };
    let expected_ty = context.get_value(val_res).ty();
    let target = if let Some(prev) = vis.find_existing(eclass, val_res, op_id, expected_ty) {
        Some(Expression::Read(prev))
    } else if state
        .extractor
        .cost_of(eclass)
        .is_some_and(|c| c.compute < 100_000)
    {
        if let Some(existing_val) = state
            .rows
            .values_in_class(eclass)
            .filter(|&v| {
                v != val_res
                    && context.has_value(v)
                    && context.get_value(v).ty() == expected_ty
                    && vis.visible(v, op_id)
            })
            .min()
        {
            Some(Expression::Read(existing_val))
        } else {
            decode_extracted(&state.extractor, eclass, val_res, op_id, &vis, true)?
        }
    } else {
        None
    };

    let Some(expr) = target else {
        state.decisions.add_candidate(eclass, val_res)?;
        return Ok(());
    };
    let replacement = match expr {
        Expression::Read(v) if v != val_res => v,
        Expression::Read(_) => {
            state.decisions.add_candidate(eclass, val_res)?;
            return Ok(());
        }
        other => {
            let anchor = OperationRef::new(op.clone());
            materialize(context, &anchor, val_res, other, None)?
        }
    };
    replace_all_uses(context, val_res, replacement);
    state.decisions.redirect(val_res)?;
    if let Some(&state_out) = op.results().get(1) {
        replace_all_uses(context, state_out, state_in);
        state.decisions.redirect(state_out)?;
    }
    state.decisions.erase(op_id)?;
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
            || state.decisions.is_redirected(res)
        {
            continue;
        }
        let Some(eclass) = state.rows.class_of(res) else {
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
            db: state.rows.db,
            decisions: &state.decisions,
        };
        let Some(expr) = decode_extracted(&state.extractor, eclass, res, op_id, &vis, true)? else {
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
        state.decisions.redirect(res)?;
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
    let db = state.rows.db;
    let Some(op) = placed_op(context, op_id) else {
        return Ok(());
    };
    let results = op.results();
    if !op.regions().is_empty() || results.len() != 1 {
        return Ok(());
    }
    let res = results[0];
    if pinned_vals.binary_search(&res).is_ok() || state.decisions.is_redirected(res) {
        return Ok(());
    }
    let Some(eclass) = state.rows.class_of(res) else {
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
    let own_cost = compute_own_cost(context, &op, kind, is_lifted, &state.rows, &state.extractor);
    let vis = VisibilityCtx {
        context,
        db: state.rows.db,
        decisions: &state.decisions,
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
            state.decisions.redirect(res)?;
            if is_lifted {
                state.decisions.erase(op_id)?;
            }
            return Ok(());
        }
    }

    // Case 2: Another earlier dominating value already computes this e-class (CSE / fold reuse).
    if !has_scoped_operand
        && best_cost <= own_cost
        && (is_lifted || best_cost < own_cost)
        && let Some(prev) = vis.find_existing(eclass, res, op_id, context.get_value(res).ty())
    {
        replace_all_uses(context, res, prev);
        state.decisions.redirect(res)?;
        if is_lifted {
            state.decisions.erase(op_id)?;
        }
        return Ok(());
    }

    // Case 3: Extracted expression is strictly cheaper than `op`.
    let best_name = db.schema().table(best_table).name();
    if best_cost < own_cost
        && !(has_scoped_operand && matches!(best_name, "Expr::Unary" | "Expr::Bin"))
        && let Some(expr) = decode_extracted(&state.extractor, eclass, res, op_id, &vis, true)?
    {
        match expr {
            Expression::Read(target) if target != res => {
                replace_all_uses(context, res, target);
                state.decisions.redirect(res)?;
                if is_lifted {
                    state.decisions.erase(op_id)?;
                }
            }
            Expression::Read(_) => {
                state.decisions.add_candidate(eclass, res)?;
            }
            other => {
                let old = OperationRef::new(op);
                materialize(context, &old, res, other, Some(res))?;
                state.decisions.add_candidate(eclass, res)?;
            }
        }
        return Ok(());
    }

    // Case 4: `op` is already optimal — record for downstream CSE.
    state.decisions.add_candidate(eclass, res)?;
    Ok(())
}

fn compute_own_cost(
    context: &Context,
    op: &OpHandle,
    kind: NativeIdentity,
    is_lifted: bool,
    rows: &ExtractionRows<'_>,
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
        let Some(oc) = rows.class_of(operand) else {
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
    db: &'a Database,
    decisions: &'a PublicationDecisions<'a>,
}

impl VisibilityCtx<'_> {
    fn visible(&self, value: ValueId, anchor: OpId) -> bool {
        if !self.context.has_value(value) || self.decisions.is_redirected(value) {
            return false;
        }
        let Some(anchor_region) = self.context.region_of_op(anchor) else {
            return false;
        };
        let anchor_block = self.context.block_placement(anchor);
        if let Some(def_op) = self.context.get_value(value).defining_op() {
            if def_op == anchor
                || self.decisions.is_erased(def_op)
                || !self.context.has_operation(def_op)
            {
                return false;
            }
            if let Some((def_block, def_position)) = self.context.block_placement(def_op) {
                if let Some((block, anchor_position)) = anchor_block
                    && block == def_block
                {
                    return def_position < anchor_position;
                }
                let Some(def_region) = self.context.parent_region(def_block) else {
                    return false;
                };
                return self.context.get_region(def_region).entry_block() == def_block
                    && self.context.region_under(anchor_region, def_region);
            }
            if let Some(def_region) = self.context.parent_nodes_region(def_op) {
                if self.context.parent_nodes_region(anchor) == Some(def_region) {
                    let Ok(table) = self.db.table("DependsOn") else {
                        return true;
                    };
                    let key = [
                        self.db.base(i64::from(def_region.number())),
                        self.db.base(i64::from(def_op.number())),
                        self.db.base(i64::from(anchor.number())),
                    ];
                    return self.db.lookup(table, &key).is_none();
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
            if anchor_block.is_some_and(|(block, _)| block == def_block) {
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

    fn find_existing(
        &self,
        eclass: Value,
        exclude: ValueId,
        anchor: OpId,
        expected_ty: crate::TypeId,
    ) -> Option<ValueId> {
        self.decisions.candidates(eclass).find(|&value| {
            value != exclude
                && self.context.has_value(value)
                && self.context.get_value(value).ty() == expected_ty
                && self.visible(value, anchor)
        })
    }
}

fn decode_extracted(
    extractor: &TreeExtractor<'_, EqSatCost>,
    eclass: Value,
    expected: ValueId,
    anchor: OpId,
    vis: &VisibilityCtx<'_>,
    is_root: bool,
) -> Result<Option<Expression>, PassError> {
    let context = vis.context;
    let db = vis.db;
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
        && let Some(prev) = vis.find_existing(class, expected, anchor, expected_ty)
    {
        return Ok(Some(Expression::Read(prev)));
    }

    match name {
        "Expr::Pi" => decode_extracted(extractor, row[1], expected, anchor, vis, is_root),
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
            let Some(child) = decode_extracted(extractor, row[2], expected, anchor, vis, false)?
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
            let Some(left) = decode_extracted(extractor, row[2], expected, anchor, vis, false)?
            else {
                return Ok(None);
            };
            let Some(right) = decode_extracted(extractor, row[3], expected, anchor, vis, false)?
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
