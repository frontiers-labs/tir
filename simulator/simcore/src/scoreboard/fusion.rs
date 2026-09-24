//! Fusion formation and scheduling. All indices exposed to handlers remain
//! indices in the original architectural instruction stream.

use std::collections::{HashMap, HashSet, VecDeque};

use tir::backend::sched::{
    FusionExpr, FusionMicroOp, FusionOperandRef, FusionOperandSelector, FusionPattern,
    FusionSchedule, FusionStageGroup, FusionValue, InstrSchedClass, MachineModel, MicroOp,
};

use super::{
    EventHandler, FrontendState, Prf, ScoreboardInstr, SimContext, TimingConfig, TimingResult,
    unrolled_pc,
};
use crate::memsys::MemorySystem;
use crate::predictor::BranchPredictor;

type Reg = (String, u16);

#[derive(Clone)]
struct Group<'a> {
    start: usize,
    len: usize,
    rule: Option<&'a FusionPattern>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    Yes,
    No,
    Unknown,
}

#[derive(Clone, Copy)]
enum Fact<'a> {
    Integer(i128),
    Register(&'a str, u16, Option<(u64, u64)>),
}

fn value<'a>(expr: FusionValue, members: &'a [ScoreboardInstr]) -> Option<Fact<'a>> {
    match expr {
        FusionValue::Integer(n) => Some(Fact::Integer(n)),
        FusionValue::Regnum(reference) => {
            let fact = members
                .get(reference.step)?
                .fusion_operands
                .iter()
                .find(|fact| fact.name == reference.name)?;
            match &fact.value {
                super::FusionOperandValue::Register { index, .. } => {
                    Some(Fact::Integer(i128::from(*index)))
                }
                _ => None,
            }
        }
        FusionValue::OperandWidth(reference) => {
            let fact = members
                .get(reference.step)?
                .fusion_operands
                .iter()
                .find(|fact| fact.name == reference.name)?;
            fact.width_bits
                .map(|width| Fact::Integer(i128::from(width)))
        }
        FusionValue::EncodedByte { step, index } => members
            .get(step)?
            .encoded_bytes
            .as_ref()?
            .get(index)
            .map(|byte| Fact::Integer(i128::from(*byte))),
        FusionValue::Pc(step) => members
            .get(step)
            .filter(|slot| slot.layout_known)
            .map(|slot| Fact::Integer(i128::from(slot.pc))),
        FusionValue::Width(step) => members
            .get(step)
            .filter(|slot| slot.layout_known)
            .map(|slot| Fact::Integer(i128::from(slot.width_bytes))),
        FusionValue::Operand(reference) => {
            let fact = members
                .get(reference.step)?
                .fusion_operands
                .iter()
                .find(|fact| fact.name == reference.name)?;
            Some(match &fact.value {
                super::FusionOperandValue::Register {
                    file,
                    index,
                    bit_range,
                } => Fact::Register(file, *index, *bit_range),
                super::FusionOperandValue::Immediate(n) => Fact::Integer(*n),
            })
        }
        FusionValue::Add(a, b)
        | FusionValue::Sub(a, b)
        | FusionValue::Mul(a, b)
        | FusionValue::Div(a, b) => {
            let (Some(Fact::Integer(a_value)), Some(Fact::Integer(b_value))) =
                (value(*a, members), value(*b, members))
            else {
                return None;
            };
            let result = match expr {
                FusionValue::Add(..) => a_value.checked_add(b_value),
                FusionValue::Sub(..) => a_value.checked_sub(b_value),
                FusionValue::Mul(..) => a_value.checked_mul(b_value),
                FusionValue::Div(..) => a_value.checked_div(b_value),
                _ => unreachable!(),
            }?;
            Some(Fact::Integer(result))
        }
    }
}

fn compare(
    a: FusionValue,
    b: FusionValue,
    members: &[ScoreboardInstr],
    f: impl FnOnce(std::cmp::Ordering) -> bool,
) -> Truth {
    let (Some(a), Some(b)) = (value(a, members), value(b, members)) else {
        return Truth::Unknown;
    };
    let ordering = match (a, b) {
        (Fact::Integer(a), Fact::Integer(b)) => a.cmp(&b),
        (Fact::Register(af, ai, _), Fact::Register(bf, bi, _)) => (af, ai).cmp(&(bf, bi)),
        _ => return Truth::Unknown,
    };
    if f(ordering) { Truth::Yes } else { Truth::No }
}

fn register_equal(a: FusionValue, b: FusionValue, members: &[ScoreboardInstr]) -> Truth {
    match (value(a, members), value(b, members)) {
        (Some(Fact::Register(af, ai, ar)), Some(Fact::Register(bf, bi, br))) => {
            if af != bf {
                Truth::No
            } else if let (Some(a), Some(b)) = (ar, br) {
                if a == b { Truth::Yes } else { Truth::No }
            } else if ai != bi {
                Truth::No
            } else {
                Truth::Unknown
            }
        }
        _ => Truth::Unknown,
    }
}

fn register_overlap(a: FusionValue, b: FusionValue, members: &[ScoreboardInstr]) -> Truth {
    match (value(a, members), value(b, members)) {
        (Some(Fact::Register(af, _ai, ar)), Some(Fact::Register(bf, _bi, br))) => {
            if af != bf {
                return Truth::No;
            }
            match (ar, br) {
                (Some((as_, ae)), Some((bs, be))) => {
                    if as_ < be && bs < ae {
                        Truth::Yes
                    } else {
                        Truth::No
                    }
                }
                _ => Truth::Unknown,
            }
        }
        _ => Truth::Unknown,
    }
}

fn guard(expr: FusionExpr, members: &[ScoreboardInstr]) -> Truth {
    use std::cmp::Ordering;
    match expr {
        FusionExpr::True => Truth::Yes,
        FusionExpr::And(parts) => {
            let mut unknown = false;
            for part in parts {
                match guard(*part, members) {
                    Truth::No => return Truth::No,
                    Truth::Unknown => unknown = true,
                    Truth::Yes => {}
                }
            }
            if unknown { Truth::Unknown } else { Truth::Yes }
        }
        FusionExpr::Or(parts) => {
            let mut unknown = false;
            for part in parts {
                match guard(*part, members) {
                    Truth::Yes => return Truth::Yes,
                    Truth::Unknown => unknown = true,
                    Truth::No => {}
                }
            }
            if unknown { Truth::Unknown } else { Truth::No }
        }
        FusionExpr::Not(expr) => match guard(*expr, members) {
            Truth::Yes => Truth::No,
            Truth::No => Truth::Yes,
            Truth::Unknown => Truth::Unknown,
        },
        FusionExpr::Eq(a, b) => compare(a, b, members, |o| o == Ordering::Equal),
        FusionExpr::Ne(a, b) => compare(a, b, members, |o| o != Ordering::Equal),
        FusionExpr::Lt(a, b) => compare(a, b, members, |o| o == Ordering::Less),
        FusionExpr::Le(a, b) => compare(a, b, members, |o| o != Ordering::Greater),
        FusionExpr::Gt(a, b) => compare(a, b, members, |o| o == Ordering::Greater),
        FusionExpr::Ge(a, b) => compare(a, b, members, |o| o != Ordering::Less),
        FusionExpr::SameRegister(a, b) => register_equal(a, b, members),
        FusionExpr::OverlapRegister(a, b) => register_overlap(a, b, members),
        FusionExpr::SameBlock {
            first,
            second,
            bytes,
        } => {
            let (Some(a), Some(b)) = (members.get(first), members.get(second)) else {
                return Truth::Unknown;
            };
            if !a.layout_known || !b.layout_known || bytes == 0 {
                Truth::Unknown
            } else if a.pc / bytes == b.pc / bytes
                && a.pc
                    .saturating_add(u64::from(a.width_bytes.saturating_sub(1)))
                    / bytes
                    == a.pc / bytes
                && b.pc
                    .saturating_add(u64::from(b.width_bytes.saturating_sub(1)))
                    / bytes
                    == b.pc / bytes
            {
                Truth::Yes
            } else {
                Truth::No
            }
        }
        FusionExpr::Aligned { step, bytes } => {
            let Some(slot) = members.get(step) else {
                return Truth::Unknown;
            };
            if !slot.layout_known || bytes == 0 {
                Truth::Unknown
            } else if slot.pc % bytes == 0 {
                Truth::Yes
            } else {
                Truth::No
            }
        }
    }
}

fn select_groups<'a>(model: &'a MachineModel, base: &[ScoreboardInstr]) -> Vec<Group<'a>> {
    let mut groups = Vec::new();
    let mut i = 0;
    while i < base.len() {
        let mut selected = None;
        for rule in model.fusions {
            let len = rule.steps.len();
            if len == 0 || i + len > base.len() {
                continue;
            }
            let members = &base[i..i + len];
            if members.iter().skip(1).any(|slot| slot.fusion_boundary)
                || members[..len - 1].iter().any(|slot| slot.branch.is_some())
                || members
                    .iter()
                    .enumerate()
                    .any(|(step, slot)| !rule.steps[step].ops.contains(&slot.op_name.as_str()))
                || members.windows(2).any(|pair| {
                    pair[0].layout_known
                        && pair[1].layout_known
                        && pair[0].pc.saturating_add(u64::from(pair[0].width_bytes)) != pair[1].pc
                })
            {
                continue;
            }
            match guard(rule.guard, members) {
                Truth::Yes => {
                    if validate_recipe(rule, members) {
                        selected = Some(rule);
                    }
                    break;
                }
                Truth::Unknown => break,
                Truth::No => {}
            }
        }
        let len = selected.map_or(1, |rule| rule.steps.len());
        groups.push(Group {
            start: i,
            len,
            rule: selected,
        });
        i += len;
    }
    groups
}

fn selected_registers(
    selector: FusionOperandSelector,
    members: &[ScoreboardInstr],
    is_output: bool,
) -> Option<Vec<(usize, Reg)>> {
    match selector {
        FusionOperandSelector::AllInputs(step) if !is_output => Some(
            members
                .get(step)?
                .uses
                .iter()
                .cloned()
                .map(|reg| (step, reg))
                .collect(),
        ),
        FusionOperandSelector::AllOutputs(step) if is_output => Some(
            members
                .get(step)?
                .defs
                .iter()
                .cloned()
                .map(|reg| (step, reg))
                .collect(),
        ),
        FusionOperandSelector::Operand(FusionOperandRef { step, name }) => {
            let slot = members.get(step)?;
            let fact = slot.fusion_operands.iter().find(|fact| fact.name == name)?;
            let super::FusionOperandValue::Register { file, index, .. } = &fact.value else {
                return None;
            };
            let reg = (file.clone(), *index);
            let list = if is_output { &slot.defs } else { &slot.uses };
            list.contains(&reg).then_some(vec![(step, reg)])
        }
        _ => None,
    }
}

fn stage_groups(
    groups: &'static [FusionStageGroup],
    scalar: u16,
    len: usize,
) -> Vec<(Vec<usize>, u16)> {
    if groups.is_empty() {
        vec![((0..len).collect(), scalar)]
    } else {
        groups
            .iter()
            .map(|group| (group.steps.to_vec(), group.slots))
            .collect()
    }
}

fn validate_stage(groups: &'static [FusionStageGroup], len: usize) {
    if groups.is_empty() {
        return;
    }
    let mut expected = 0;
    for group in groups {
        assert!(
            !group.steps.is_empty() && group.slots > 0,
            "invalid fusion stage group"
        );
        for &step in group.steps {
            assert_eq!(
                step, expected,
                "fusion stage groups must partition members in order"
            );
            expected += 1;
        }
    }
    assert_eq!(expected, len, "fusion stage groups must cover all members");
}

fn validate_recipe(rule: &FusionPattern, members: &[ScoreboardInstr]) -> bool {
    let schedule = &rule.schedule;
    let len = members.len();
    assert!(
        !schedule.uops.is_empty(),
        "fusion {} has no execution recipe",
        rule.name
    );
    for groups in [
        schedule.decode_groups,
        schedule.rename_groups,
        schedule.rob_groups,
        schedule.retire_groups,
    ] {
        validate_stage(groups, len);
    }
    let mut inputs = HashSet::new();
    let mut outputs = HashSet::new();
    let mut memory: Vec<Vec<usize>> = members.iter().map(|slot| vec![0; slot.mem.len()]).collect();
    let mut control = vec![0usize; len];
    let mut names = HashSet::new();
    for uop in schedule.uops {
        assert!(names.insert(uop.name), "duplicate fusion micro-op name");
        assert!(
            uop.read_cycle <= uop.write_cycle,
            "fusion output precedes input"
        );
        assert!(
            uop.inherit_routes.is_none_or(|step| step < len),
            "invalid inherited route step"
        );
        for selector in uop.inputs {
            let Some(registers) = selected_registers(*selector, members, false) else {
                return false;
            };
            inputs.extend(registers);
        }
        let mut uop_outputs = HashSet::new();
        for selector in uop.outputs {
            let Some(registers) = selected_registers(*selector, members, true) else {
                return false;
            };
            uop_outputs.extend(registers);
        }
        for result in uop_outputs {
            if !outputs.insert(result) {
                return false;
            }
        }
        for reference in uop.memory {
            let records = memory
                .get_mut(reference.step)
                .expect("invalid fusion memory step");
            match reference.index {
                Some(_index) if records.is_empty() => {}
                Some(index) => *records.get_mut(index).expect("invalid fusion memory index") += 1,
                None => records.iter_mut().for_each(|count| *count += 1),
            }
        }
        for &step in uop.control_steps {
            *control.get_mut(step).expect("invalid fusion control step") += 1;
        }
    }
    validate_coverage(members, &inputs, &outputs, &memory, &control);
    for uop in schedule.uops {
        for dependency in uop.depends_on {
            assert!(
                names.contains(dependency),
                "fusion micro-op depends on unknown name"
            );
            assert_ne!(*dependency, uop.name, "fusion micro-op depends on itself");
        }
    }
    true
}

fn validate_coverage(
    members: &[ScoreboardInstr],
    inputs: &HashSet<(usize, Reg)>,
    outputs: &HashSet<(usize, Reg)>,
    memory: &[Vec<usize>],
    control: &[usize],
) {
    for (step, slot) in members.iter().enumerate() {
        for reg in &slot.uses {
            let internal = members[..step].iter().any(|prior| prior.defs.contains(reg));
            assert!(
                internal || inputs.contains(&(step, reg.clone())),
                "fusion recipe omits an architectural input"
            );
        }
        for reg in &slot.defs {
            assert!(
                outputs.contains(&(step, reg.clone())),
                "fusion recipe omits an architectural result"
            );
        }
        assert!(
            memory[step].iter().all(|count| *count == 1),
            "fusion memory event must be mapped exactly once"
        );
        if slot.branch.is_some() {
            assert_eq!(
                control[step], 1,
                "fusion control event must be mapped exactly once"
            );
        }
    }
}

#[derive(Clone, Copy)]
struct OutputState {
    issued: u64,
    ready: u64,
    memory_ready: u64,
    resource: Option<&'static str>,
}

impl OutputState {
    fn ready_for(self, model: &MachineModel, consumer: Option<&str>) -> u64 {
        let forwarded = self
            .resource
            .zip(consumer)
            .and_then(|(from, to)| model.forward_latency(from, to))
            .map(|latency| self.issued + u64::from(latency));
        forwarded.unwrap_or(self.ready).max(self.memory_ready)
    }
}

struct Work<'a> {
    group: Group<'a>,
    arch_start: usize,
    member_dispatched: Vec<bool>,
    uop_issued: Vec<Option<u64>>,
    uop_complete: Vec<Option<u64>>,
    member_issued: Vec<bool>,
    member_complete: Vec<Option<u64>>,
    member_owners: Vec<Vec<usize>>,
    /// First member, last member, slots, allocated, released.
    rob_allocations: Vec<(usize, usize, u16, bool, bool)>,
}

impl<'a> Work<'a> {
    fn members<'b>(&self, base: &'b [ScoreboardInstr]) -> &'b [ScoreboardInstr] {
        &base[self.group.start..self.group.start + self.group.len]
    }

    fn schedule(&self) -> Option<&FusionSchedule> {
        self.group.rule.map(|rule| &rule.schedule)
    }

    fn uop_count(&self) -> usize {
        self.schedule().map_or(1, |schedule| schedule.uops.len())
    }

    fn uop(&self, index: usize) -> Option<&FusionMicroOp> {
        self.schedule().map(|schedule| &schedule.uops[index])
    }

    fn stage(&self, kind: Stage) -> Vec<(Vec<usize>, u16)> {
        if let Some(schedule) = self.schedule() {
            let (groups, scalar) = match kind {
                Stage::Decode => (schedule.decode_groups, schedule.decode_uops),
                Stage::Rename => (schedule.rename_groups, schedule.rename_slots),
                Stage::Retire => (schedule.retire_groups, schedule.retire_slots),
            };
            stage_groups(groups, scalar, self.group.len)
        } else {
            let cost = match kind {
                Stage::Decode => 0,
                Stage::Rename | Stage::Retire => 1,
            };
            vec![(vec![0], cost)]
        }
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Decode,
    Rename,
    Retire,
}

fn member_owners(schedule: &FusionSchedule, members: &[ScoreboardInstr]) -> Vec<Vec<usize>> {
    let mut owners = vec![Vec::new(); members.len()];
    for (uop_index, uop) in schedule.uops.iter().enumerate() {
        let mut steps = HashSet::new();
        for selector in uop.inputs.iter().chain(uop.outputs) {
            let step = match selector {
                FusionOperandSelector::Operand(reference) => reference.step,
                FusionOperandSelector::AllInputs(step)
                | FusionOperandSelector::AllOutputs(step) => *step,
            };
            steps.insert(step);
        }
        for reference in uop.memory {
            steps.insert(reference.step);
        }
        for &step in uop.control_steps {
            steps.insert(step);
        }
        for step in steps {
            owners[step].push(uop_index);
        }
    }
    // A pure instruction may contribute only to the group match. Its issue and
    // completion are still represented by the final micro-op.
    for owner in &mut owners {
        if owner.is_empty() {
            owner.push(schedule.uops.len() - 1);
        }
    }
    owners
}

fn build_work<'a>(
    groups: &[Group<'a>],
    base: &[ScoreboardInstr],
    iterations: usize,
) -> (Vec<Work<'a>>, Vec<(usize, usize)>) {
    let mut work = Vec::with_capacity(groups.len().saturating_mul(iterations));
    let mut arch_to_work = Vec::with_capacity(base.len().saturating_mul(iterations));
    for iteration in 0..iterations {
        for group in groups {
            let id = work.len();
            let arch_start = iteration * base.len() + group.start;
            for local in 0..group.len {
                arch_to_work.push((id, local));
            }
            let owners = group.rule.map_or_else(
                || vec![vec![0]],
                |rule| member_owners(&rule.schedule, &base[group.start..group.start + group.len]),
            );
            let rob_allocations = if let Some(rule) = group.rule {
                stage_groups(rule.schedule.rob_groups, rule.schedule.rob_slots, group.len)
                    .into_iter()
                    .map(|(steps, slots)| {
                        (
                            *steps.first().unwrap(),
                            *steps.last().unwrap(),
                            slots,
                            false,
                            false,
                        )
                    })
                    .collect()
            } else {
                vec![(0, 0, 1, false, false)]
            };
            work.push(Work {
                group: group.clone(),
                arch_start,
                member_dispatched: vec![false; group.len],
                uop_issued: vec![None; group.rule.map_or(1, |rule| rule.schedule.uops.len())],
                uop_complete: vec![None; group.rule.map_or(1, |rule| rule.schedule.uops.len())],
                member_issued: vec![false; group.len],
                member_complete: vec![None; group.len],
                member_owners: owners,
                rob_allocations,
            });
        }
    }
    (work, arch_to_work)
}

/// For each architectural input, capture the preceding normal writer and all
/// still-live OR contributors. These edges are stable even if younger groups
/// issue before older groups on an out-of-order machine.
fn build_input_dependencies(
    base: &[ScoreboardInstr],
    iterations: usize,
) -> Vec<HashMap<Reg, Vec<usize>>> {
    let n = base.len().saturating_mul(iterations);
    let mut dependencies = vec![HashMap::new(); n];
    let mut writers: HashMap<Reg, usize> = HashMap::new();
    let mut frontiers: HashMap<Reg, Vec<usize>> = HashMap::new();
    for index in 0..n {
        let slot = &base[index % base.len()];
        if !super::is_zero_idiom(slot) {
            for reg in &slot.uses {
                if slot.or_updates.contains(reg) {
                    continue;
                }
                let mut deps = Vec::new();
                if let Some(&writer) = writers.get(reg) {
                    deps.push(writer);
                }
                if let Some(contributors) = frontiers.get(reg) {
                    deps.extend(contributors);
                }
                dependencies[index].insert(reg.clone(), deps);
            }
        }
        for reg in &slot.defs {
            if !slot.or_updates.contains(reg) {
                writers.insert(reg.clone(), index);
                frontiers.remove(reg);
            }
        }
        for reg in &slot.or_updates {
            frontiers.entry(reg.clone()).or_default().push(index);
        }
    }
    dependencies
}

fn decode_group(
    model: &MachineModel,
    frontend: &mut FrontendState,
    class: &InstrSchedClass,
    cost: u16,
    earliest: u64,
) -> u64 {
    let Some(f) = &model.frontend else {
        return earliest;
    };
    if cost == 0 {
        return earliest;
    }
    let mut remaining = cost;
    let mut ready = earliest;
    while remaining > 0 {
        let max_decoder = f
            .decode
            .slots
            .iter()
            .filter_map(|slot| {
                f.decode
                    .decoders
                    .iter()
                    .find(|decoder| decoder.name == *slot)
                    .filter(|_| class.decoder.is_none_or(|needed| needed == *slot))
                    .map(|decoder| decoder.max_uops_per_instruction)
            })
            .max()
            .expect("fusion decoder has no eligible slot");
        let chunk = remaining.min(max_decoder).min(f.decode.uops_per_cycle);
        assert!(chunk > 0, "fusion decoder has zero capacity");
        let part = InstrSchedClass {
            decode_uops: chunk,
            ..*class
        };
        ready = frontend.reserve_decode(f, &part, ready);
        remaining -= chunk;
    }
    ready
}

fn frontend_group_delivery(
    model: &MachineModel,
    state: &mut FrontendState,
    work: &Work<'_>,
    base: &[ScoreboardInstr],
    config: &TimingConfig,
    earliest: &[u64],
) -> Vec<u64> {
    let Some(frontend) = &model.frontend else {
        return earliest.to_vec();
    };
    if work.schedule().is_none() {
        let slot = &base[work.group.start];
        let pc = unrolled_pc(slot.pc, work.arch_start, base.len(), config.unroll_stride);
        return vec![super::frontend_delivery_cycle(
            model,
            state,
            slot,
            pc,
            earliest[0],
        )];
    }
    let schedule = work.schedule().unwrap();
    let members = work.members(base);
    let first_pc = unrolled_pc(
        members[0].pc,
        work.arch_start,
        base.len(),
        config.unroll_stride,
    );
    let width = members
        .iter()
        .map(|slot| u32::from(slot.width_bytes))
        .sum::<u32>()
        .min(u32::from(u16::MAX)) as u16;
    let mut class = InstrSchedClass {
        decode_uops: schedule.decoded_cache_uops,
        decoder: schedule.decoder,
        decode_cycles: schedule.decode_cycles,
        ..InstrSchedClass::DEFAULT
    };
    if let Some(cache) = &frontend.decoded_cache {
        let mut remaining = schedule.decoded_cache_uops;
        if remaining > 0 && cache.deliver_uops_per_cycle > 0 {
            class.decode_uops = remaining.min(cache.deliver_uops_per_cycle);
            if let Some(mut hit) = state.reserve_decoded_cache(
                cache,
                &class,
                first_pc,
                width,
                *earliest.iter().max().unwrap_or(&0),
            ) {
                remaining -= class.decode_uops;
                while remaining > 0 {
                    class.decode_uops = remaining.min(cache.deliver_uops_per_cycle);
                    hit = state
                        .reserve_decoded_cache(cache, &class, first_pc, width, hit)
                        .expect("decoded fusion line disappeared during delivery");
                    remaining -= class.decode_uops;
                }
                return vec![hit; work.group.len];
            }
        }
    }
    let mut fetched = Vec::with_capacity(work.group.len);
    for (local, slot) in members.iter().enumerate() {
        let pc = unrolled_pc(
            slot.pc,
            work.arch_start + local,
            base.len(),
            config.unroll_stride,
        );
        fetched.push(state.reserve_fetch(frontend, pc, slot.width_bytes, earliest[local]));
    }
    let mut member_ready = fetched.clone();
    for (steps, cost) in work.stage(Stage::Decode) {
        class.decode_uops = cost;
        let earliest = steps.iter().map(|step| fetched[*step]).max().unwrap_or(0);
        let ready = decode_group(model, state, &class, cost, earliest);
        for step in steps {
            member_ready[step] = ready;
        }
    }
    if let Some(cache) = &frontend.decoded_cache {
        class.decode_uops = schedule.decoded_cache_uops;
        state.fill_decoded_cache(cache, &class, first_pc, width);
    }
    member_ready
}

fn reserve_capacity(calendar: &mut HashMap<u64, u16>, earliest: u64, cost: u16, width: u16) -> u64 {
    if cost == 0 {
        return earliest;
    }
    let width = width.max(1);
    let mut remaining = cost;
    let mut cycle = earliest;
    loop {
        let used = calendar.entry(cycle).or_default();
        let available = width.saturating_sub(*used);
        let assigned = available.min(remaining);
        *used += assigned;
        remaining -= assigned;
        if remaining == 0 {
            return cycle;
        }
        cycle += 1;
    }
}

fn uop_resource(uop: &FusionMicroOp, members: &[ScoreboardInstr]) -> Option<&'static str> {
    if let Some(step) = uop.inherit_routes {
        return members[step].class.resources.first().copied().or_else(|| {
            members[step]
                .class
                .uops
                .first()
                .and_then(|op| op.routes.first())
                .and_then(|route| route.resources.first())
                .map(|resource| resource.resource)
        });
    }
    uop.routes
        .first()
        .and_then(|route| route.resources.first())
        .map(|resource| resource.resource)
}

fn reserve_uop_at(
    uop: &FusionMicroOp,
    members: &[ScoreboardInstr],
    lanes: &mut HashMap<&'static str, Vec<u64>>,
    usage: &mut HashMap<&'static str, u64>,
    cycle: u64,
) -> Option<Vec<(&'static str, u16)>> {
    let mut candidate = lanes.clone();
    let mut chosen = Vec::new();
    let issue = if let Some(step) = uop.inherit_routes {
        super::reserve_class_resources(
            &members[step].class,
            &mut candidate,
            cycle,
            &mut chosen,
            usage,
        )
    } else {
        let micro_ops = [MicroOp { routes: uop.routes }];
        super::reserve_micro_ops(&micro_ops, &mut candidate, cycle, &mut chosen, usage)
    };
    if issue != cycle {
        return None;
    }
    *lanes = candidate;
    for (resource, cycles) in &chosen {
        *usage.entry(resource).or_default() += u64::from(*cycles);
    }
    Some(chosen)
}

fn uop_steps(uop: &FusionMicroOp) -> HashSet<usize> {
    let mut steps = HashSet::new();
    for selector in uop.inputs.iter().chain(uop.outputs) {
        steps.insert(match selector {
            FusionOperandSelector::Operand(reference) => reference.step,
            FusionOperandSelector::AllInputs(step) | FusionOperandSelector::AllOutputs(step) => {
                *step
            }
        });
    }
    for reference in uop.memory {
        steps.insert(reference.step);
    }
    for &step in uop.control_steps {
        steps.insert(step);
    }
    steps
}

fn dependencies_ready(
    model: &MachineModel,
    work: &Work<'_>,
    uop_index: usize,
    members: &[ScoreboardInstr],
    dependencies: &[HashMap<Reg, Vec<usize>>],
    outputs: &[HashMap<Reg, OutputState>],
    cycle: u64,
) -> bool {
    let Some(uop) = work.uop(uop_index) else {
        let slot = &members[0];
        if super::is_zero_idiom(slot) {
            return true;
        }
        return slot.uses.iter().all(|reg| {
            dependencies[work.arch_start]
                .get(reg)
                .is_none_or(|producers| {
                    producers.iter().all(|producer| {
                        outputs[*producer].get(reg).is_some_and(|state| {
                            state.ready_for(model, slot.class.resources.first().copied()) <= cycle
                        })
                    })
                })
        });
    };
    let consumer_resource = uop_resource(uop, members);
    for dependency in uop.depends_on {
        let Some((index, _prior)) = work
            .schedule()
            .unwrap()
            .uops
            .iter()
            .enumerate()
            .find(|(_, prior)| prior.name == *dependency)
        else {
            return false;
        };
        if index >= uop_index
            || work.uop_complete[index]
                .is_none_or(|complete| complete > cycle + u64::from(uop.read_cycle))
        {
            return false;
        }
    }
    let mut required = HashSet::new();
    for selector in uop.inputs {
        for (step, reg) in selected_registers(*selector, members, false).unwrap_or_default() {
            required.insert((step, reg));
        }
    }
    // A later member may consume an earlier member's result without listing
    // that internal input in the recipe. Preserve that architectural edge.
    for step in uop_steps(uop) {
        for reg in &members[step].uses {
            if members[..step].iter().any(|prior| prior.defs.contains(reg)) {
                required.insert((step, reg.clone()));
            }
        }
    }
    required.into_iter().all(|(step, reg)| {
        if members[step].or_updates.contains(&reg) {
            return true;
        }
        dependencies[work.arch_start + step]
            .get(&reg)
            .is_none_or(|producers| {
                producers.iter().all(|producer| {
                    // An input and result carried by the same fused micro-op can
                    // pass internally without an architectural output wakeup.
                    if *producer >= work.arch_start && *producer < work.arch_start + work.group.len
                    {
                        let local = producer - work.arch_start;
                        if uop.outputs.iter().any(|selector| {
                            selected_registers(*selector, members, true)
                                .is_some_and(|regs| regs.contains(&(local, reg.clone())))
                        }) {
                            return true;
                        }
                    }
                    outputs[*producer].get(&reg).is_some_and(|state| {
                        state.ready_for(model, consumer_resource)
                            <= cycle + u64::from(uop.read_cycle)
                    })
                })
            })
    })
}

fn uop_memory_complete(
    uop: &FusionMicroOp,
    members: &[ScoreboardInstr],
    cycle: u64,
    mem: Option<&mut MemorySystem>,
) -> u64 {
    let Some(mem) = mem else {
        return cycle;
    };
    let mut complete = cycle;
    for reference in uop.memory {
        let slot = &members[reference.step];
        let indices: Box<dyn Iterator<Item = usize>> = match reference.index {
            Some(index) => Box::new(std::iter::once(index)),
            None => Box::new(0..slot.mem.len()),
        };
        for index in indices {
            let Some(access) = slot.mem.get(index) else {
                continue;
            };
            let done = mem.access_data(slot.pc, access.addr, access.is_write, cycle);
            if !access.is_write {
                complete = complete.max(done);
            }
        }
    }
    complete
}

fn record_uop_outputs(
    uop: &FusionMicroOp,
    members: &[ScoreboardInstr],
    work: &Work<'_>,
    cycle: u64,
    memory_complete: u64,
    outputs: &mut [HashMap<Reg, OutputState>],
) {
    let state = OutputState {
        issued: cycle,
        ready: cycle + u64::from(uop.write_cycle),
        memory_ready: memory_complete,
        resource: uop_resource(uop, members),
    };
    for selector in uop.outputs {
        for (step, reg) in selected_registers(*selector, members, true).unwrap_or_default() {
            outputs[work.arch_start + step].insert(reg, state);
        }
    }
}

fn update_member_events(
    work: &mut Work<'_>,
    member: usize,
    cycle: u64,
    handler: &mut Option<&mut dyn EventHandler>,
) {
    if work.member_issued[member] {
        return;
    }
    if !work.member_owners[member]
        .iter()
        .all(|&uop| work.uop_issued[uop].is_some())
    {
        return;
    }
    work.member_issued[member] = true;
    work.member_complete[member] = Some(
        work.member_owners[member]
            .iter()
            .filter_map(|&uop| work.uop_complete[uop])
            .max()
            .unwrap_or(cycle),
    );
    if let Some(h) = handler.as_mut() {
        h.issued(cycle, work.arch_start + member);
    }
}

mod engine;

pub(super) use engine::run_fused;
