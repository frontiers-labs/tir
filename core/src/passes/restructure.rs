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

#[allow(dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/restructure.rs"));
}

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
        let mut program = generated::Program::load(&mut db).map_err(rule_err)?;
        program.transform().map_err(rule_err)?;
        program.rebuild().map_err(rule_err)?;
        commit(ctx, &mut program)
    }
}

use generated::{Class, Index, Program};

type Reg = Class<generated::Reg>;
type Sw = Class<generated::Sw>;
type Ty = Class<generated::Ty>;
type Val = Class<generated::Val>;
type Slots<K, V> = Index<K, (K, i64, V)>;
type Results<K> = Index<K, (K, i64, Ty, Val)>;

struct Indexes {
    ty_i1: Option<Ty>,
    ty_i32: Option<Ty>,
    ty_state: Option<Ty>,
    ty_exact: Index<Ty, (i64, Ty)>,
    ty_ret: Index<Ty, (i64, Ty)>,
    chain_cnt: Index<i64, generated::OutChainCountRow>,
    join_in: Slots<i64, Val>,
    reg_port: Results<Reg>,
    for_port: Slots<i64, Val>,
    for_body: Index<Reg, generated::OutForBodyClassRow>,
    moved_by_reg: Index<Reg, generated::OutMovedOpClassRow>,
    loop_by_reg: Index<Reg, generated::OutLoopClassRow>,
    sw_by_reg: Index<Reg, generated::OutSwitchClassRow>,
    for_by_reg: Index<Reg, generated::OutForClassRow>,
    synth_by_reg: Index<Reg, generated::OutSynthClassRow>,
    rw_op: Slots<i64, Val>,
    app_st: Index<i64, generated::OutAppendStateClassRow>,
    loop_init: Slots<Reg, Val>,
    loop_res: Results<Reg>,
    sw_in: Slots<Sw, Val>,
    sw_arm: Slots<Sw, Reg>,
    sw_res: Results<Sw>,
    for_init: Slots<i64, Val>,
    for_res: Results<i64>,
    yield_by_reg: Slots<Reg, Val>,
}

impl Indexes {
    fn build(program: &mut Program<'_>) -> Result<Self, tir_egglog::Error> {
        Ok(Self {
            ty_i1: program.lookup_ty_i1()?,
            ty_i32: program.lookup_ty_i32()?,
            ty_state: program.lookup_ty_state()?,
            ty_exact: program.index_ty_exact_by_result()?,
            ty_ret: program.index_ty_ret_type_by_result()?,
            chain_cnt: program.index_out_chain_count_by_region(),
            join_in: program.index_out_join_state_by_region(),
            reg_port: program.index_out_reg_port_by_region(),
            for_port: program.index_out_for_port_by_region(),
            for_body: program.index_out_for_body_by_region(),
            moved_by_reg: program.index_out_moved_op_by_region(),
            loop_by_reg: program.index_out_loop_by_region(),
            sw_by_reg: program.index_out_switch_by_region(),
            for_by_reg: program.index_out_for_by_region(),
            synth_by_reg: program.index_out_synth_by_region(),
            rw_op: program.index_out_rewrite_operand_by_operation(),
            app_st: program.index_out_append_state_by_operation(),
            loop_init: program.index_out_loop_init_by_body(),
            loop_res: program.index_out_loop_res_by_body(),
            sw_in: program.index_out_switch_input_by_switch(),
            sw_arm: program.index_out_switch_arm_by_switch(),
            sw_res: program.index_out_switch_res_by_switch(),
            for_init: program.index_out_for_init_by_loop_op(),
            for_res: program.index_out_for_res_by_loop_op(),
            yield_by_reg: program.index_out_yield_by_region(),
        })
    }
}

enum OpItem {
    Moved(OpId),
    Loop(Reg),
    Switch(Sw, i64, Val),
    For(i64, i64, Val, Val, Val),
    Synth(Val, String, i64, i64, Ty, Val, Val),
}

fn commit(ctx: &Context, program: &mut Program<'_>) -> Result<(), PassError> {
    if program.out_orig_region().next().is_none() {
        return Ok(());
    }
    let ix = Indexes::build(program).map_err(rule_err)?;
    let mut vals: Vec<(Val, ValueId)> = program
        .rows_val_orig()
        .map_err(rule_err)?
        .map(|row| row.map(|(id, class)| (class, ValueId::from_number(id as u32))))
        .collect::<Result<_, _>>()
        .map_err(rule_err)?;
    vals.sort_unstable_by_key(|e| e.0);

    let (mut regs, mut reg_order, mut orig_ports) = (Vec::new(), Vec::new(), Vec::new());

    for row in program.out_orig_region() {
        let (r_id, _, entry, is_for, depth) = row.map_err(rule_err)?;
        let entry = BlockId::from_number(entry as u32);
        let Some(rk) = program.lookup_reg_orig(r_id).map_err(rule_err)? else {
            continue;
        };
        let rid = RegionId::from_number(r_id as u32);
        let mut ports = ctx.block_arguments(entry);
        if is_for != 0 {
            for pv in listed_slots(program, &ix.for_port, r_id) {
                let p = ctx.append_block_argument(entry, TypeId::STATE);
                set(&mut vals, pv, p.id());
                ports.push(p);
            }
        }
        set(&mut regs, rk, rid);
        reg_order.push((depth, rk, Some((r_id, orig_ports.len()))));
        orig_ports.push((rid, rk, ports));
    }
    for row in program.out_child_region_classes(tir_egglog::Color::BASE) {
        let (rk, depth) = row.map_err(rule_err)?;
        let rid = ctx.create_region().id();
        let ports = listed_pairs(program, &ix.reg_port, rk)
            .into_iter()
            .map(|(tk, vk)| {
                let v = ctx.create_value(decode_ty(ctx, program, &ix, tk), None);
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
        let (ops, yields) = populate_region(ctx, program, &ix, &regs, &mut vals, rk, orig);
        if orig_info.is_some() {
            orig_bodies.push((rk, ops, yields));
        } else {
            let rid = get(&regs, rk).expect("child reg");
            ctx.set_region_ops(rid, ops);
            ctx.set_region_results(rid, yields);
        }
    }
    orig_bodies.sort_unstable_by_key(|e| e.0);

    for row in program.out_erase_op() {
        let (id,) = row.map_err(rule_err)?;
        let op_id = OpId::from_number(id as u32);
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
    for row in program.out_erase_block() {
        let (id,) = row.map_err(rule_err)?;
        ctx.erase_block(BlockId::from_number(id as u32));
    }
    Ok(())
}

fn populate_region(
    ctx: &Context,
    program: &Program<'_>,
    ix: &Indexes,
    regs: &[(Reg, RegionId)],
    vals: &mut Vec<(Val, ValueId)>,
    rk: Reg,
    orig: Option<(i64, &[Value])>,
) -> (Vec<OpId>, Vec<ValueId>) {
    let mut ops = Vec::new();
    if let Some((r_key, _)) = orig
        && let Some((_, n)) = indexed(program, &ix.chain_cnt, r_key).next()
    {
        let es = state_ops::entry_state(ctx)
            .state_result(TypeId::STATE)
            .build();
        ops.push(es.id());
        if let Some(ek) = program
            .lookup_val_entry_state(r_key)
            .expect("EntryState constructor")
        {
            set(vals, ek, es.result());
        }
        if n > 1 {
            let sp = state_ops::split(ctx)
                .state_operands(vec![es.result()])
                .states(vec![TypeId::STATE; n as usize])
                .build();
            ops.push(sp.id());
            for (idx, val) in sp.states().into_iter().enumerate() {
                if let Some(sk) = program
                    .lookup_val_split_state(r_key, idx as i64)
                    .expect("SplitState constructor")
                {
                    set(vals, sk, val);
                }
            }
        }
    }

    let mut reg_items: Vec<(i64, OpItem)> = Vec::new();
    for (out, _, order, kind, width, literal, ty, lhs, rhs) in
        indexed(program, &ix.synth_by_reg, rk)
    {
        reg_items.push((
            order,
            OpItem::Synth(out, kind, width, literal, ty, lhs, rhs),
        ));
    }
    for (id, _, order) in indexed(program, &ix.moved_by_reg, rk) {
        reg_items.push((order, OpItem::Moved(OpId::from_number(id as u32))));
    }
    for (body, _, order) in indexed(program, &ix.loop_by_reg, rk) {
        reg_items.push((order, OpItem::Loop(body)));
    }
    for (switch, _, order, terminator, selector) in indexed(program, &ix.sw_by_reg, rk) {
        reg_items.push((order, OpItem::Switch(switch, terminator, selector)));
    }
    for (for_op, _, order, body, lb, ub, step) in indexed(program, &ix.for_by_reg, rk) {
        reg_items.push((order, OpItem::For(for_op, body, lb, ub, step)));
    }
    reg_items.sort_unstable_by_key(|e| e.0);
    for (_, item) in reg_items {
        emit_item(ctx, program, ix, regs, vals, &mut ops, item);
    }

    if let Some((r_key, _)) = orig
        && let Some(jk) = program
            .lookup_val_join_state(r_key)
            .expect("JoinState constructor")
    {
        let states = listed_vals(program, &ix.join_in, r_key, vals);
        let join = state_ops::join(ctx)
            .states(states)
            .state_result(TypeId::STATE)
            .build();
        ops.push(join.id());
        set(vals, jk, join.result());
    }

    let mut yields = listed_vals(program, &ix.yield_by_reg, rk, vals);
    if let Some((_, ports)) = orig
        && let Some((_, iv, ub, step)) = indexed(program, &ix.for_body, rk).next()
    {
        let (iv, ub, step) = (
            get(vals, iv).expect("iv"),
            get(vals, ub).expect("ub"),
            get(vals, step).expect("step"),
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
    program: &Program<'_>,
    ix: &Indexes,
    regs: &[(Reg, RegionId)],
    vals: &mut Vec<(Val, ValueId)>,
    ops: &mut Vec<OpId>,
    item: OpItem,
) {
    match item {
        OpItem::Synth(out_k, kind_k, a, b_val, ty_k, lhs_k, rhs_k) => {
            if get(vals, out_k).is_some() {
                return;
            }
            let v = match kind_k.as_str() {
                "const" => {
                    let op = b::constant(ctx, b_val, IntegerType::new(ctx, a as u64)).build();
                    ops.push(op.id());
                    op.result()
                }
                "zero" => make_zero(ctx, ops, decode_ty(ctx, program, ix, ty_k)),
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
            let key = i64::from(op_id.raw());
            for (_, slot, value) in indexed(program, &ix.rw_op, key) {
                ctx.set_op_operand(op_id, slot as usize, get(vals, value).expect("rw"));
            }
            for (_, input, output) in indexed(program, &ix.app_st, key) {
                ctx.append_operand(op_id, get(vals, input).expect("state"));
                set(vals, output, ctx.append_result(op_id, TypeId::STATE));
            }
            ops.push(op_id);
        }
        OpItem::Loop(body_rk) => {
            let inits = listed_vals(program, &ix.loop_init, body_rk, vals);
            let (tys, rvals) = res_spec(ctx, program, ix, &ix.loop_res, body_rk);
            let op = LoopOpBuilder::new(ctx)
                .inits(inits)
                .body(get(regs, body_rk).expect("body"))
                .result_types(tys)
                .build();
            ops.push(op.id());
            bind_results(ctx, vals, op.id(), &rvals);
        }
        OpItem::Switch(sw, term_id, pred_k) => {
            let inputs = listed_vals(program, &ix.sw_in, sw, vals);
            let arms = listed_slots(program, &ix.sw_arm, sw)
                .into_iter()
                .map(|a| get(regs, a).expect("arm"))
                .collect();
            let (tys, rvals) = res_spec(ctx, program, ix, &ix.sw_res, sw);
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
            let inits = listed_vals(program, &ix.for_init, for_k, vals);
            let (tys, rvals) = res_spec(ctx, program, ix, &ix.for_res, for_k);
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

fn decode_ty(ctx: &Context, program: &Program<'_>, ix: &Indexes, ty: Ty) -> TypeId {
    if ix.ty_i1 == Some(ty) {
        IntegerType::new(ctx, 1)
    } else if ix.ty_i32 == Some(ty) {
        IntegerType::new(ctx, 32)
    } else if ix.ty_state == Some(ty) {
        TypeId::STATE
    } else if let Some((id, _)) = indexed(program, &ix.ty_exact, ty).next() {
        TypeId::from_number(id as u32)
    } else if let Some((id, _)) = indexed(program, &ix.ty_ret, ty).next() {
        match ctx.get_op(OpId::from_number(id as u32)).attr("ret_type") {
            Some(AttributeValue::Type(ty)) => ty,
            _ => UnitType::new(ctx),
        }
    } else {
        unreachable!("missing constructor for generated restructuring type")
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

fn res_spec<K>(
    ctx: &Context,
    program: &Program<'_>,
    ix: &Indexes,
    index: &Results<K>,
    key: K,
) -> (Vec<TypeId>, Vec<Val>) {
    listed_pairs(program, index, key)
        .into_iter()
        .map(|(ty, result)| (decode_ty(ctx, program, ix, ty), result))
        .unzip()
}

fn bind_results(ctx: &Context, vals: &mut Vec<(Val, ValueId)>, op: OpId, rvals: &[Val]) {
    rvals
        .iter()
        .zip(ctx.get_op(op).results())
        .for_each(|(&rv, res_v)| set(vals, rv, res_v));
}

fn listed_vals<K>(
    program: &Program<'_>,
    index: &Slots<K, Val>,
    key: K,
    vals: &[(Val, ValueId)],
) -> Vec<ValueId> {
    listed_slots(program, index, key)
        .into_iter()
        .map(|vk| get(vals, vk).expect("val"))
        .collect()
}

fn listed_slots<K, V>(program: &Program<'_>, index: &Slots<K, V>, key: K) -> Vec<V> {
    let mut slots: Vec<_> = indexed(program, index, key)
        .map(|(_, slot, value)| (slot, value))
        .collect();
    slots.sort_unstable_by_key(|e| e.0);
    slots.into_iter().map(|(_, value)| value).collect()
}

fn listed_pairs<K>(program: &Program<'_>, index: &Results<K>, key: K) -> Vec<(Ty, Val)> {
    let mut pairs: Vec<_> = indexed(program, index, key)
        .map(|(_, slot, ty, value)| (slot, ty, value))
        .collect();
    pairs.sort_unstable_by_key(|e| e.0);
    pairs
        .into_iter()
        .map(|(_, ty, value)| (ty, value))
        .collect()
}

fn indexed<'a, K, R>(
    program: &'a Program<'_>,
    index: &Index<K, R>,
    key: K,
) -> impl Iterator<Item = R> + use<'a, K, R> {
    program
        .listed(index, key)
        .expect("restructuring index")
        .map(Result::unwrap)
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
