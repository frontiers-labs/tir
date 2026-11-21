//! RVSDG Control Tree Translation pass (Reissmann §5.1.2 Algorithm III).
//!
//! All analysis runs in CDSL via `AnalysisManager` (`core/defs/analysis/cfg.cdsl`).
//! All restructuring lowering, dataflow resolution, switch folding, synthetic op
//! ordering, and erasure sets run in CDSL/Egglog (`core/defs/passes/restructure.cdsl`).
//! This module executes the Egglog schedule and materializes the indexed `Out*`
//! tables directly into `Context`.

use crate::analysis::cfg;
use crate::attributes::{AttributeValue, Predicate};
use crate::builtin::{self, IntegerType, TupleType, UnitType, ops as b};
use crate::ptr::{self, PtrType};
use crate::scf::nodes::{ForOpBuilder, LoopOpBuilder, SwitchOpBuilder};
use crate::state::ops as state_ops;
use crate::{
    AnalysisManager, BlockId, CaseGuard, Context, OpId, Operation, OperationRef, Pass, PassError,
    RegionId, TypeId, Value, ValueId,
};
use tir_egglog::{Database, IndexId, Program, Row, Schedule, TableId, Value as EggVal};

#[derive(Clone, Default)]
pub struct RestructureNodesPass;

impl RestructureNodesPass {
    pub fn new() -> Self {
        Self
    }
}

crate::register_pass!(RestructureNodesPass, "restructure-nodes");

fn rule_err(error: impl std::fmt::Display) -> PassError {
    PassError::InvalidRuleSet(error.to_string())
}

impl Pass for RestructureNodesPass {
    fn name(&self) -> &'static str {
        "restructure-nodes"
    }

    fn run(
        &mut self,
        op: OpId,
        ctx: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        cfg::refresh(ctx, op)?;
        let mut db = ctx.eqsat_database().map_err(rule_err)?;
        let mut program = Program::from_bytes(
            include_bytes!(concat!(env!("OUT_DIR"), "/restructure-rules.bin")),
            &mut db,
        )
        .map_err(rule_err)?;
        let phases = "phase_targets! phase_active phase_ord_seed1 * phase_arms \
            phase_homes_inner phase_homes phase_home_derived! phase_reg_tree phase_reg_tree2 \
            phase_depths! phase_depth_max phase_vars! phase_static_writes! * phase_flow! \
            phase_fill phase_fold_prep phase_fold_match phase_fold_diff phase_fold_dup \
            phase_fold_keep * phase_fold_skip! phase_emit_raw phase_need_val! phase_emit_final";
        let schedule = Schedule::seq(phases.split_whitespace().map(|tok| match tok {
            "*" => Schedule::seq([
                Schedule::run("ord_next"),
                Schedule::run("ord_links"),
                Schedule::saturate("ord_index"),
                Schedule::run("ord_count"),
            ]),
            s if s.ends_with('!') => Schedule::saturate(&s[..s.len() - 1]),
            s => Schedule::run(s),
        }));
        program.run(&mut db, &schedule, &[]).map_err(rule_err)?;
        db.rebuild().map_err(rule_err)?;
        commit(ctx, &mut db)
    }
}

struct Indexes {
    reg_orig: TableId,
    entry_st: TableId,
    split_st: TableId,
    join_st: TableId,
    ty_i1: TableId,
    ty_i32: TableId,
    ty_state: TableId,
    ty_exact: IndexId,
    ty_ret: IndexId,
    chain_cnt: IndexId,
    join_in: IndexId,
    reg_port: IndexId,
    for_port: IndexId,
    for_body: IndexId,
    moved_by_reg: IndexId,
    loop_by_reg: IndexId,
    sw_by_reg: IndexId,
    for_by_reg: IndexId,
    synth_by_reg: IndexId,
    rw_op: IndexId,
    app_st: IndexId,
    loop_init: IndexId,
    loop_res: IndexId,
    sw_in: IndexId,
    sw_arm: IndexId,
    sw_res: IndexId,
    for_init: IndexId,
    for_res: IndexId,
    yield_by_reg: IndexId,
}

impl Indexes {
    fn build(db: &mut Database) -> Result<Self, PassError> {
        let idx = |db: &mut Database, name: &str, col: usize| -> Result<IndexId, PassError> {
            let t = db.table(name).map_err(rule_err)?;
            Ok(db.add_kept_index(t, &[col], &[]))
        };
        Ok(Self {
            reg_orig: db.table("Reg::Orig").map_err(rule_err)?,
            entry_st: db.table("Val::EntryState").map_err(rule_err)?,
            split_st: db.table("Val::SplitState").map_err(rule_err)?,
            join_st: db.table("Val::JoinState").map_err(rule_err)?,
            ty_i1: db.table("Ty::I1").map_err(rule_err)?,
            ty_i32: db.table("Ty::I32").map_err(rule_err)?,
            ty_state: db.table("Ty::State").map_err(rule_err)?,
            ty_exact: idx(db, "Ty::Exact", 1)?,
            ty_ret: idx(db, "Ty::RetType", 1)?,
            chain_cnt: idx(db, "OutChainCount", 0)?,
            join_in: idx(db, "OutJoinState", 0)?,
            reg_port: idx(db, "OutRegPort", 0)?,
            for_port: idx(db, "OutForPort", 0)?,
            for_body: idx(db, "OutForBody", 0)?,
            moved_by_reg: idx(db, "OutMovedOp", 1)?,
            loop_by_reg: idx(db, "OutLoop", 1)?,
            sw_by_reg: idx(db, "OutSwitch", 1)?,
            for_by_reg: idx(db, "OutFor", 1)?,
            synth_by_reg: idx(db, "OutSynth", 1)?,
            rw_op: idx(db, "OutRewriteOperand", 0)?,
            app_st: idx(db, "OutAppendState", 0)?,
            loop_init: idx(db, "OutLoopInit", 0)?,
            loop_res: idx(db, "OutLoopRes", 0)?,
            sw_in: idx(db, "OutSwitchIn", 0)?,
            sw_arm: idx(db, "OutSwitchArm", 0)?,
            sw_res: idx(db, "OutSwitchRes", 0)?,
            for_init: idx(db, "OutForInit", 0)?,
            for_res: idx(db, "OutForRes", 0)?,
            yield_by_reg: idx(db, "OutYield", 0)?,
        })
    }
}

#[derive(Clone, Copy)]
enum OpItem {
    Moved(OpId),
    Loop(EggVal),
    Switch(EggVal, i64, EggVal),
    For(EggVal, i64, EggVal, EggVal, EggVal),
    Synth(EggVal, EggVal, i64, i64, EggVal, EggVal, EggVal),
}

fn commit(ctx: &Context, db: &mut Database) -> Result<(), PassError> {
    let orig_tbl = db.table("OutOrigRegion").map_err(rule_err)?;
    if db.row_count(orig_tbl) == 0 {
        return Ok(());
    }
    let ix = Indexes::build(db)?;
    let mut vals: Vec<(EggVal, ValueId)> = db
        .rows(db.table("Val::Orig").map_err(rule_err)?)
        .map(|r| (r[1], ValueId::from_number(i(db, r, 0) as u32)))
        .collect();
    vals.sort_unstable_by_key(|e| e.0);

    let (mut regs, mut reg_order, mut orig_ports) = (Vec::new(), Vec::new(), Vec::new());

    for r in db.rows(orig_tbl) {
        let (r_key, r_id, entry, is_for, depth) = (
            r[0],
            i(db, r, 0),
            BlockId::from_number(i(db, r, 2) as u32),
            i(db, r, 3) != 0,
            i(db, r, 4),
        );
        let Some(rk) = db.lookup(ix.reg_orig, &[r_key]) else {
            continue;
        };
        let rid = RegionId::from_number(r_id as u32);
        let mut ports = ctx.block_arguments(entry);
        if is_for {
            for pv in listed_slots(db, ix.for_port, r_key) {
                let p = ctx.append_block_argument(entry, TypeId::STATE);
                set(&mut vals, pv, p.id());
                ports.push(p);
            }
        }
        set(&mut regs, rk, rid);
        reg_order.push((depth, rk, Some((r_key, orig_ports.len()))));
        orig_ports.push((rid, rk, ports));
    }
    for r in db.rows(db.table("OutChildRegion").map_err(rule_err)?) {
        let (rk, depth) = (r[0], i(db, r, 1));
        let rid = ctx.create_region().id();
        let ports = listed_pairs(db, ix.reg_port, rk)
            .into_iter()
            .map(|(tk, vk)| {
                let v = ctx.create_value(decode_ty(ctx, db, &ix, tk), None);
                set(&mut vals, vk, v.id());
                v
            })
            .collect();
        ctx.set_region_nodes(rid, ports, Vec::new(), Vec::new());
        set(&mut regs, rk, rid);
        reg_order.push((depth, rk, None));
    }
    reg_order.sort_unstable_by_key(|e| (e.0, e.1));

    let mut orig_bodies = Vec::new();
    for &(_, rk, orig_info) in &reg_order {
        let orig = orig_info.map(|(k, idx)| (k, orig_ports[idx].2.as_slice()));
        let (ops, yields) = populate_region(ctx, db, &ix, &regs, &mut vals, rk, orig);
        if orig_info.is_some() {
            orig_bodies.push((rk, ops, yields));
        } else {
            let rid = get(&regs, rk).expect("child reg");
            ctx.set_region_ops(rid, ops);
            ctx.set_region_results(rid, yields);
        }
    }
    orig_bodies.sort_unstable_by_key(|e| e.0);

    for r in db.rows(db.table("OutEraseOp").map_err(rule_err)?) {
        let op_id = OpId::from_number(i(db, r, 0) as u32);
        if ctx.has_operation(op_id) {
            ctx.erase_op(&OperationRef::new(ctx.get_op(op_id)))?;
        }
    }
    for (rid, rk, ports) in orig_ports {
        let idx = orig_bodies
            .binary_search_by_key(&rk, |e| e.0)
            .expect("orig body");
        let ops = std::mem::take(&mut orig_bodies[idx].1);
        let yields = std::mem::take(&mut orig_bodies[idx].2);
        ctx.set_region_nodes(rid, ports, ops, yields);
    }
    for r in db.rows(db.table("OutEraseBlock").map_err(rule_err)?) {
        ctx.erase_block(BlockId::from_number(i(db, r, 0) as u32));
    }
    Ok(())
}

fn populate_region(
    ctx: &Context,
    db: &Database,
    ix: &Indexes,
    regs: &[(EggVal, RegionId)],
    vals: &mut Vec<(EggVal, ValueId)>,
    rk: EggVal,
    orig: Option<(EggVal, &[Value])>,
) -> (Vec<OpId>, Vec<ValueId>) {
    let mut ops = Vec::new();
    if let Some((r_key, _)) = orig
        && let Some(row) = db.listed(ix.chain_cnt, &[r_key]).next()
    {
        let n = i(db, row, 1);
        let es = state_ops::entry_state(ctx)
            .state_result(TypeId::STATE)
            .build();
        ops.push(es.id());
        if let Some(ek) = db.lookup(ix.entry_st, &[r_key]) {
            set(vals, ek, es.result());
        }
        if n > 1 {
            let sp = state_ops::split(ctx)
                .state_operands(vec![es.result()])
                .states(vec![TypeId::STATE; n as usize])
                .build();
            ops.push(sp.id());
            for (idx, val) in sp.states().into_iter().enumerate() {
                if let Some(sk) = db.lookup(ix.split_st, &[r_key, db.base(idx as i64)]) {
                    set(vals, sk, val);
                }
            }
        }
    }

    let mut reg_items: Vec<(i64, OpItem)> = Vec::new();
    for r in db.listed(ix.synth_by_reg, &[rk]) {
        reg_items.push((
            i(db, r, 2),
            OpItem::Synth(r[0], r[3], i(db, r, 4), i(db, r, 5), r[6], r[7], r[8]),
        ));
    }
    for r in db.listed(ix.moved_by_reg, &[rk]) {
        reg_items.push((
            i(db, r, 2),
            OpItem::Moved(OpId::from_number(i(db, r, 0) as u32)),
        ));
    }
    for r in db.listed(ix.loop_by_reg, &[rk]) {
        reg_items.push((i(db, r, 2), OpItem::Loop(r[0])));
    }
    for r in db.listed(ix.sw_by_reg, &[rk]) {
        reg_items.push((i(db, r, 2), OpItem::Switch(r[0], i(db, r, 3), r[4])));
    }
    for r in db.listed(ix.for_by_reg, &[rk]) {
        reg_items.push((
            i(db, r, 2),
            OpItem::For(r[0], i(db, r, 3), r[4], r[5], r[6]),
        ));
    }
    reg_items.sort_unstable_by_key(|e| e.0);
    for (_, item) in reg_items {
        emit_item(ctx, db, ix, regs, vals, &mut ops, item);
    }

    if let Some((r_key, _)) = orig
        && let Some(jk) = db.lookup(ix.join_st, &[r_key])
    {
        let states = listed_vals(db, ix.join_in, r_key, vals);
        let join = state_ops::join(ctx)
            .states(states)
            .state_result(TypeId::STATE)
            .build();
        ops.push(join.id());
        set(vals, jk, join.result());
    }

    let mut yields = listed_vals(db, ix.yield_by_reg, rk, vals);
    if let Some((_, ports)) = orig
        && let Some(row) = db.listed(ix.for_body, &[rk]).next()
    {
        let (iv, ub, step) = (
            get(vals, row[1]).expect("iv"),
            get(vals, row[2]).expect("ub"),
            get(vals, row[3]).expect("step"),
        );
        let cmp = b::CmpIOpBuilder::new(ctx)
            .lhs(iv)
            .rhs(ub)
            .predicate(Predicate::Slt)
            .result_type(IntegerType::new(ctx, 1))
            .build();
        let adv = b::AddIOpBuilder::new(ctx)
            .lhs(iv)
            .rhs(step)
            .result_type(ctx.get_value(iv).ty())
            .build();
        ops.push(cmp.id());
        ops.push(adv.id());
        yields = [cmp.result(), adv.result()]
            .into_iter()
            .chain(yields)
            .chain(ports.iter().map(Value::id))
            .collect();
    }
    (ops, yields)
}

fn emit_item(
    ctx: &Context,
    db: &Database,
    ix: &Indexes,
    regs: &[(EggVal, RegionId)],
    vals: &mut Vec<(EggVal, ValueId)>,
    ops: &mut Vec<OpId>,
    item: OpItem,
) {
    match item {
        OpItem::Synth(out_k, kind_k, a, b_val, ty_k, lhs_k, rhs_k) => {
            if get(vals, out_k).is_some() {
                return;
            }
            let v = match db.base_value::<String>(kind_k).as_str() {
                "const" => {
                    let op = b::constant(ctx, b_val, IntegerType::new(ctx, a as u64)).build();
                    ops.push(op.id());
                    op.result()
                }
                "zero" => make_zero(ctx, ops, decode_ty(ctx, db, ix, ty_k)),
                "not" => {
                    let i1 = IntegerType::new(ctx, 1);
                    let one = b::constant(ctx, 1, i1).build();
                    let inv =
                        b::xori(ctx, get(vals, lhs_k).expect("not"), one.result(), i1).build();
                    ops.push(one.id());
                    ops.push(inv.id());
                    inv.result()
                }
                "sub" => {
                    let op = b::subi(
                        ctx,
                        get(vals, lhs_k).expect("sub l"),
                        get(vals, rhs_k).expect("sub r"),
                        IntegerType::new(ctx, 32),
                    )
                    .build();
                    ops.push(op.id());
                    op.result()
                }
                pred_s => {
                    let pred = if pred_s == "eq" {
                        Predicate::Eq
                    } else {
                        Predicate::Slt
                    };
                    let op = b::CmpIOpBuilder::new(ctx)
                        .lhs(get(vals, lhs_k).expect("cmp l"))
                        .rhs(get(vals, rhs_k).expect("cmp r"))
                        .predicate(pred)
                        .result_type(IntegerType::new(ctx, 1))
                        .build();
                    ops.push(op.id());
                    op.result()
                }
            };
            set(vals, out_k, v);
        }
        OpItem::Moved(op_id) => {
            let ok = db.base(i64::from(op_id.raw()));
            for r in db.listed(ix.rw_op, &[ok]) {
                ctx.set_op_operand(op_id, i(db, r, 1) as usize, get(vals, r[2]).expect("rw"));
            }
            for r in db.listed(ix.app_st, &[ok]) {
                ctx.append_operand(op_id, get(vals, r[1]).expect("state"));
                set(vals, r[2], ctx.append_result(op_id, TypeId::STATE));
            }
            ops.push(op_id);
        }
        OpItem::Loop(body_rk) => {
            let inits = listed_vals(db, ix.loop_init, body_rk, vals);
            let (tys, rvals) = res_spec(ctx, db, ix, ix.loop_res, body_rk);
            let op = LoopOpBuilder::new(ctx)
                .inits(inits)
                .body(get(regs, body_rk).expect("body"))
                .result_types(tys)
                .build();
            ops.push(op.id());
            bind_results(ctx, vals, op.id(), &rvals);
        }
        OpItem::Switch(sw, term_id, pred_k) => {
            let inputs = listed_vals(db, ix.sw_in, sw, vals);
            let arms = listed_slots(db, ix.sw_arm, sw)
                .into_iter()
                .map(|a| get(regs, a).expect("arm"))
                .collect();
            let (tys, rvals) = res_spec(ctx, db, ix, ix.sw_res, sw);
            let mut bld = SwitchOpBuilder::new(ctx)
                .predicate(get(vals, pred_k).expect("sw pred"))
                .inputs(inputs)
                .arms(arms)
                .result_types(tys);
            if term_id >= 0
                && let Some(g) = ctx
                    .get_op(OpId::from_number(term_id as u32))
                    .as_interface::<dyn CaseGuard>()
            {
                bld = bld.cases(&g.cases());
            }
            let op = bld.build();
            ops.push(op.id());
            bind_results(ctx, vals, op.id(), &rvals);
        }
        OpItem::For(for_k, body_r, lb_k, ub_k, step_k) => {
            let inits = listed_vals(db, ix.for_init, for_k, vals);
            let (tys, rvals) = res_spec(ctx, db, ix, ix.for_res, for_k);
            let op = ForOpBuilder::new(ctx)
                .lb(get(vals, lb_k).expect("lb"))
                .inits(inits)
                .ub(get(vals, ub_k).expect("ub"))
                .step(get(vals, step_k).expect("step"))
                .body(RegionId::from_number(body_r as u32))
                .result_types(tys)
                .build();
            ops.push(op.id());
            bind_results(ctx, vals, op.id(), &rvals);
        }
    }
}

fn decode_ty(ctx: &Context, db: &Database, ix: &Indexes, ty_k: EggVal) -> TypeId {
    if db.lookup(ix.ty_i1, &[]) == Some(ty_k) {
        IntegerType::new(ctx, 1)
    } else if db.lookup(ix.ty_i32, &[]) == Some(ty_k) {
        IntegerType::new(ctx, 32)
    } else if db.lookup(ix.ty_state, &[]) == Some(ty_k) {
        TypeId::STATE
    } else if let Some(r) = db.listed(ix.ty_exact, &[ty_k]).next() {
        TypeId::from_number(i(db, r, 0) as u32)
    } else if let Some(r) = db.listed(ix.ty_ret, &[ty_k]).next() {
        match ctx
            .get_op(OpId::from_number(i(db, r, 0) as u32))
            .attr("ret_type")
        {
            Some(AttributeValue::Type(t)) => t,
            _ => UnitType::new(ctx),
        }
    } else {
        unreachable!("unknown Ty e-class {ty_k:?}")
    }
}

fn make_zero(ctx: &Context, ops: &mut Vec<OpId>, ty: TypeId) -> ValueId {
    if PtrType::view(ctx, ty).is_some() {
        let op = ptr::NullOpBuilder::new(ctx).result_type(ty).build();
        ops.push(op.id());
        return op.result();
    }
    if let Some((e, m)) = ctx.terms().float_format(ty) {
        let bits = b::constant(ctx, 0, IntegerType::new(ctx, e + m + 1)).build();
        let cast = b::bitcast(ctx, bits.result(), ty).build();
        ops.push(bits.id());
        ops.push(cast.id());
        return cast.result();
    }
    if let Some(tuple) = TupleType::view(ctx, ty) {
        let elems = tuple
            .elements()
            .iter()
            .map(|&e| make_zero(ctx, ops, e))
            .collect();
        let op = builtin::MakeTupleOpBuilder::new(ctx)
            .elements(elems)
            .result_type(ty)
            .build();
        ops.push(op.id());
        return op.result();
    }
    let op = b::constant(ctx, 0, ty).build();
    ops.push(op.id());
    op.result()
}

fn res_spec(
    ctx: &Context,
    db: &Database,
    ix: &Indexes,
    idx: IndexId,
    key: EggVal,
) -> (Vec<TypeId>, Vec<EggVal>) {
    listed_pairs(db, idx, key)
        .into_iter()
        .map(|(tk, rv)| (decode_ty(ctx, db, ix, tk), rv))
        .unzip()
}

fn bind_results(ctx: &Context, vals: &mut Vec<(EggVal, ValueId)>, op: OpId, rvals: &[EggVal]) {
    rvals
        .iter()
        .zip(ctx.get_op(op).results())
        .for_each(|(&rv, res_v)| set(vals, rv, res_v));
}

fn listed_vals(
    db: &Database,
    idx: IndexId,
    key: EggVal,
    vals: &[(EggVal, ValueId)],
) -> Vec<ValueId> {
    listed_slots(db, idx, key)
        .into_iter()
        .map(|vk| get(vals, vk).expect("val"))
        .collect()
}

fn listed_slots(db: &Database, idx: IndexId, key: EggVal) -> Vec<EggVal> {
    let mut v: Vec<_> = db
        .listed(idx, &[key])
        .map(|r| (i(db, r, 1), r[2]))
        .collect();
    v.sort_unstable_by_key(|e| e.0);
    v.into_iter().map(|e| e.1).collect()
}

fn listed_pairs(db: &Database, idx: IndexId, key: EggVal) -> Vec<(EggVal, EggVal)> {
    let mut v: Vec<_> = db
        .listed(idx, &[key])
        .map(|r| (i(db, r, 1), r[2], r[3]))
        .collect();
    v.sort_unstable_by_key(|e| e.0);
    v.into_iter().map(|e| (e.1, e.2)).collect()
}

fn i(db: &Database, r: Row<'_>, c: usize) -> i64 {
    db.base_value::<i64>(r[c])
}

fn get<K: Ord, V: Copy>(map: &[(K, V)], key: K) -> Option<V> {
    map.binary_search_by(|e| e.0.cmp(&key))
        .ok()
        .map(|idx| map[idx].1)
}

fn set<K: Ord, V>(map: &mut Vec<(K, V)>, key: K, val: V) {
    match map.binary_search_by(|e| e.0.cmp(&key)) {
        Ok(idx) => map[idx].1 = val,
        Err(idx) => map.insert(idx, (key, val)),
    }
}
