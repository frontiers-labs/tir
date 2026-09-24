//! Cycle assignment for formed fusion groups.

use super::*;

struct Services<'p, 'm, 'h> {
    predictor: Option<&'p mut dyn BranchPredictor>,
    mem: Option<&'m mut MemorySystem>,
    handler: Option<&'h mut dyn EventHandler>,
}

struct Engine<'m, 'b> {
    model: &'m MachineModel,
    base: &'b [ScoreboardInstr],
    config: &'m TimingConfig,
    prf: Option<&'m Prf>,
    work: Vec<Work<'m>>,
    arch_to_work: Vec<(usize, usize)>,
    dependencies: Vec<HashMap<Reg, Vec<usize>>>,
    outputs: Vec<HashMap<Reg, OutputState>>,
    lanes: HashMap<&'static str, Vec<u64>>,
    usage: HashMap<&'static str, u64>,
    frontend: FrontendState,
    dispatch_ready: Vec<Option<Vec<u64>>>,
    rename_ready: HashMap<(usize, usize), u64>,
    rename_calendar: HashMap<u64, u16>,
    retire_calendar: HashMap<u64, u16>,
    retire_ready: HashMap<usize, u64>,
    prf_used: HashMap<String, u16>,
    rob_used: usize,
    width: u16,
    rob_capacity: usize,
    next_dispatch: usize,
    next_retire: usize,
    next_unissued: usize,
    active: VecDeque<usize>,
    redirect: u64,
    predicted_branches: HashMap<usize, bool>,
    pending_wrong_branch: Option<usize>,
    mispredicts: u64,
    cycle: u64,
    last_retire: u64,
}

impl<'m, 'b> Engine<'m, 'b> {
    fn new(
        model: &'m MachineModel,
        base: &'b [ScoreboardInstr],
        config: &'m TimingConfig,
        prf: Option<&'m Prf>,
        groups: &[Group<'m>],
    ) -> Self {
        let (work, arch_to_work) = build_work(groups, base, 1);
        let n = base.len();
        Self {
            model,
            base,
            config,
            prf,
            dispatch_ready: vec![None; work.len()],
            work,
            arch_to_work,
            dependencies: build_input_dependencies(base, 1),
            outputs: vec![HashMap::new(); n],
            lanes: model
                .resources
                .iter()
                .map(|resource| (resource.name, vec![0; usize::from(resource.units.max(1))]))
                .collect(),
            usage: HashMap::new(),
            frontend: FrontendState::default(),
            rename_ready: HashMap::new(),
            rename_calendar: HashMap::new(),
            retire_calendar: HashMap::new(),
            retire_ready: HashMap::new(),
            prf_used: HashMap::new(),
            rob_used: 0,
            width: model.issue_width.max(1),
            rob_capacity: if config.window == 0 {
                usize::MAX
            } else {
                config.window
            },
            next_dispatch: 0,
            next_retire: 0,
            next_unissued: 0,
            active: VecDeque::new(),
            redirect: 0,
            predicted_branches: HashMap::new(),
            pending_wrong_branch: None,
            mispredicts: 0,
            cycle: 0,
            last_retire: 0,
        }
    }

    fn run(mut self, services: &mut Services<'_, '_, '_>) -> TimingResult {
        while self.next_retire < self.base.len() {
            self.retire(services);
            self.dispatch(services);
            self.issue(services);
            self.cycle += 1;
        }
        let cycles = self.last_retire + 1;
        if let Some(handler) = services.handler.as_mut() {
            handler.finish(cycles);
        }
        TimingResult {
            cycles,
            instructions: self.base.len() as u64,
            mispredicts: self.mispredicts,
        }
    }
}

fn has_opcode_candidate(model: &MachineModel, base: &[ScoreboardInstr]) -> bool {
    base.iter().enumerate().any(|(start, _)| {
        model.fusions.iter().any(|rule| {
            !rule.steps.is_empty()
                && start + rule.steps.len() <= base.len()
                && base[start..start + rule.steps.len()]
                    .iter()
                    .enumerate()
                    .all(|(step, slot)| {
                        (step == 0 || !slot.fusion_boundary)
                            && rule.steps[step].ops.contains(&slot.op_name.as_str())
                    })
        })
    })
}

fn expanded_instances(
    base: &[ScoreboardInstr],
    iterations: usize,
    stride: u64,
) -> Vec<ScoreboardInstr> {
    let mut expanded = Vec::with_capacity(base.len().saturating_mul(iterations));
    for iteration in 0..iterations {
        for (index, slot) in base.iter().enumerate() {
            let mut instance = slot.clone();
            if instance.layout_known {
                instance.pc = instance
                    .pc
                    .saturating_add((iteration as u64).saturating_mul(stride));
            }
            if index == 0 {
                instance.fusion_boundary = true;
            }
            expanded.push(instance);
        }
    }
    expanded
}

fn report_fusions(
    work: &[Work<'_>],
    base: &[ScoreboardInstr],
    handler: &mut Option<&mut dyn EventHandler>,
) {
    let Some(handler) = handler.as_mut() else {
        return;
    };
    for item in work {
        let Some(rule) = item.group.rule else {
            continue;
        };
        let execution_uops = rule
            .schedule
            .uops
            .iter()
            .map(|uop| {
                uop.inherit_routes.map_or(1usize, |step| {
                    item.members(base)[step].class.uops.len().max(1)
                })
            })
            .sum::<usize>()
            .min(usize::from(u16::MAX)) as u16;
        let decoded_uops = if rule.schedule.decode_groups.is_empty() {
            rule.schedule.decode_uops
        } else {
            rule.schedule
                .decode_groups
                .iter()
                .map(|group| u32::from(group.slots))
                .sum::<u32>()
                .min(u32::from(u16::MAX)) as u16
        };
        handler.fused_group(
            item.arch_start,
            item.group.len,
            rule.name,
            decoded_uops,
            execution_uops,
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_fused(
    model: &MachineModel,
    base: &[ScoreboardInstr],
    iterations: usize,
    config: &TimingConfig,
    predictor: Option<&mut dyn BranchPredictor>,
    prf: Option<&Prf>,
    mem: Option<&mut MemorySystem>,
    mut handler: Option<&mut dyn EventHandler>,
) -> TimingResult {
    let mut services = Services {
        predictor,
        mem,
        handler: None,
    };
    if !has_opcode_candidate(model, base) {
        let plain = MachineModel {
            fusions: &[],
            ..*model
        };
        return super::super::run(
            &plain,
            base,
            iterations,
            config,
            services.predictor,
            prf,
            services.mem,
            handler,
        );
    }
    let expanded = expanded_instances(base, iterations, config.unroll_stride);
    let groups = select_groups(model, &expanded);
    if groups.iter().all(|group| group.rule.is_none()) {
        let plain = MachineModel {
            fusions: &[],
            ..*model
        };
        return super::super::run(
            &plain,
            base,
            iterations,
            config,
            services.predictor,
            prf,
            services.mem,
            handler,
        );
    }
    if let Some(h) = handler.as_mut() {
        h.start(&SimContext {
            model,
            iterations,
            base,
        });
    }
    let engine = Engine::new(model, &expanded, config, prf, &groups);
    report_fusions(&engine.work, &expanded, &mut handler);
    services.handler = handler;
    engine.run(&mut services)
}

impl Engine<'_, '_> {
    fn retire(&mut self, services: &mut Services<'_, '_, '_>) {
        while self.try_retire_group(services) {}
    }

    fn try_retire_group(&mut self, services: &mut Services<'_, '_, '_>) -> bool {
        if self.next_retire >= self.base.len() {
            return false;
        }
        let (work_index, local) = self.arch_to_work[self.next_retire];
        let item = &self.work[work_index];
        let retire_groups = item.stage(Stage::Retire);
        let Some((steps, cost)) = retire_groups
            .iter()
            .find(|(steps, _)| steps.first() == Some(&local))
        else {
            return false;
        };
        if !steps
            .iter()
            .all(|step| item.member_complete[*step].is_some_and(|done| done <= self.cycle))
        {
            return false;
        }
        let ready = if item.group.rule.is_some() {
            *self
                .retire_ready
                .entry(self.next_retire)
                .or_insert_with(|| {
                    reserve_capacity(&mut self.retire_calendar, self.cycle, *cost, self.width)
                })
        } else {
            self.cycle
        };
        if ready > self.cycle {
            return false;
        }
        let end = *steps.last().unwrap();
        for step in local..=end {
            self.retire_member(work_index, step, services);
        }
        self.last_retire = self.cycle;
        while self.active.front().is_some_and(|&id| {
            let item = &self.work[id];
            item.arch_start + item.group.len <= self.next_retire
        }) {
            self.active.pop_front();
        }
        true
    }

    fn retire_member(
        &mut self,
        work_index: usize,
        step: usize,
        services: &mut Services<'_, '_, '_>,
    ) {
        let item = &mut self.work[work_index];
        if let Some(handler) = services.handler.as_mut() {
            handler.retired(self.cycle, item.arch_start + step);
        }
        if let Some(prf) = self.prf.filter(|_| !self.config.in_order) {
            for (file, count) in
                super::super::register_file_counts(&item.members(self.base)[step].defs, prf)
            {
                *self.prf_used.entry(file).or_default() -= count;
            }
        }
        for (_, last, slots, allocated, released) in &mut item.rob_allocations {
            if *last == step && *allocated && !*released {
                self.rob_used -= usize::from(*slots);
                *released = true;
            }
        }
        self.next_retire += 1;
    }
}

impl Engine<'_, '_> {
    fn dispatch(&mut self, services: &mut Services<'_, '_, '_>) {
        while self.next_dispatch < self.base.len()
            && self.cycle >= self.redirect
            && self.pending_wrong_branch.is_none()
        {
            if !self.try_dispatch_one(services) {
                break;
            }
        }
    }

    fn try_dispatch_one(&mut self, services: &mut Services<'_, '_, '_>) -> bool {
        let (work_index, local) = self.arch_to_work[self.next_dispatch];
        self.plan_decode(work_index, services);
        let item = &self.work[work_index];
        let (rename_steps, rename_cost) = item
            .stage(Stage::Rename)
            .into_iter()
            .find(|(steps, _)| steps.contains(&local))
            .expect("member missing rename group");
        let key = (work_index, rename_steps[0]);
        let ready = if let Some(&ready) = self.rename_ready.get(&key) {
            ready
        } else {
            if !self.can_admit_rename(work_index, &rename_steps) {
                return false;
            }
            let decode_ready = rename_steps
                .iter()
                .map(|step| self.dispatch_ready[work_index].as_ref().unwrap()[*step])
                .max()
                .unwrap_or(self.cycle);
            if decode_ready > self.cycle {
                return false;
            }
            let ready = reserve_capacity(
                &mut self.rename_calendar,
                self.cycle,
                rename_cost,
                self.width,
            );
            *self.rename_ready.entry(key).or_insert(ready)
        };
        if ready > self.cycle || !self.can_admit_member(work_index, local) {
            return false;
        }
        self.commit_dispatch(work_index, local, services);
        true
    }

    fn plan_decode(&mut self, work_index: usize, services: &mut Services<'_, '_, '_>) {
        if self.dispatch_ready[work_index].is_some() {
            return;
        }
        let item = &self.work[work_index];
        let mut ready = self.cycle;
        let mut fetch_ready = Vec::with_capacity(item.group.len);
        for (local, slot) in item.members(self.base).iter().enumerate() {
            if let Some(mem) = services.mem.as_deref_mut() {
                let pc = unrolled_pc(
                    slot.pc,
                    item.arch_start + local,
                    self.base.len(),
                    self.config.unroll_stride,
                );
                ready += mem.fetch_stall(pc, ready);
            }
            fetch_ready.push(ready);
        }
        self.dispatch_ready[work_index] = Some(frontend_group_delivery(
            self.model,
            &mut self.frontend,
            item,
            self.base,
            self.config,
            &fetch_ready,
        ));
    }

    fn can_admit_rename(&self, work_index: usize, steps: &[usize]) -> bool {
        let item = &self.work[work_index];
        let rob_need: usize = item
            .rob_allocations
            .iter()
            .filter(|(first, _, _, allocated, _)| !allocated && steps.contains(first))
            .map(|(_, _, slots, _, _)| usize::from(*slots))
            .sum();
        assert!(
            rob_need <= self.rob_capacity,
            "fusion ROB allocation exceeds configured window"
        );
        if self.rob_used + rob_need > self.rob_capacity {
            return false;
        }
        let defs: Vec<Reg> = steps
            .iter()
            .flat_map(|step| item.members(self.base)[*step].defs.iter().cloned())
            .collect();
        if let Some(prf) = self.prf.filter(|_| !self.config.in_order) {
            for (file, count) in super::super::register_file_counts(&defs, prf) {
                assert!(
                    count <= prf.capacity.get(&file).copied().unwrap_or(u16::MAX),
                    "fusion rename group exceeds physical file capacity"
                );
            }
        }
        self.config.in_order || super::super::prf_can_allocate(&defs, self.prf, &self.prf_used)
    }

    fn can_admit_member(&self, work_index: usize, local: usize) -> bool {
        let item = &self.work[work_index];
        if let Some((_, _, slots, _, _)) = item
            .rob_allocations
            .iter()
            .find(|(first, _, _, _, _)| *first == local)
        {
            let cost = usize::from(*slots);
            assert!(
                cost <= self.rob_capacity,
                "fusion ROB allocation exceeds configured window"
            );
            if self.rob_used + cost > self.rob_capacity {
                return false;
            }
        }
        let defs = &item.members(self.base)[local].defs;
        if let Some(prf) = self.prf.filter(|_| !self.config.in_order) {
            for (file, count) in super::super::register_file_counts(defs, prf) {
                assert!(
                    count <= prf.capacity.get(&file).copied().unwrap_or(u16::MAX),
                    "fusion register allocation exceeds physical file capacity"
                );
            }
        }
        self.config.in_order || super::super::prf_can_allocate(defs, self.prf, &self.prf_used)
    }

    fn commit_dispatch(
        &mut self,
        work_index: usize,
        local: usize,
        services: &mut Services<'_, '_, '_>,
    ) {
        let item = &mut self.work[work_index];
        item.member_dispatched[local] = true;
        if let Some((_, _, slots, allocated, _)) = item
            .rob_allocations
            .iter_mut()
            .find(|(first, _, _, _, _)| *first == local)
        {
            *allocated = true;
            self.rob_used += usize::from(*slots);
        }
        if let Some(prf) = self.prf.filter(|_| !self.config.in_order) {
            for (file, count) in
                super::super::register_file_counts(&item.members(self.base)[local].defs, prf)
            {
                *self.prf_used.entry(file).or_default() += count;
            }
        }
        if let Some(handler) = services.handler.as_mut() {
            handler.dispatched(self.cycle, self.next_dispatch);
        }
        if let (Some(predictor), Some(branch)) = (
            services.predictor.as_deref_mut(),
            item.members(self.base)[local].branch.as_ref(),
        ) {
            let predicted = predictor.predict(branch.pc, branch.target);
            self.predicted_branches
                .insert(self.next_dispatch, predicted);
            if predicted != branch.taken {
                self.pending_wrong_branch = Some(self.next_dispatch);
            }
        }
        if local == 0 {
            self.active.push_back(work_index);
        }
        self.next_dispatch += 1;
    }
}

impl Engine<'_, '_> {
    fn issue(&mut self, services: &mut Services<'_, '_, '_>) {
        let mut issued_this_cycle = 0u16;
        let ids: Vec<_> = self.active.iter().copied().collect();
        for id in ids {
            if issued_this_cycle >= self.width {
                break;
            }
            if self.config.in_order && id > self.next_unissued {
                continue;
            }
            for uop_index in 0..self.work[id].uop_count() {
                if issued_this_cycle >= self.width {
                    break;
                }
                if self.try_issue_uop(id, uop_index, services) {
                    issued_this_cycle += 1;
                }
            }
            while self.next_unissued < self.work.len()
                && self.work[self.next_unissued]
                    .uop_issued
                    .iter()
                    .all(Option::is_some)
            {
                self.next_unissued += 1;
            }
        }
    }

    fn try_issue_uop(
        &mut self,
        id: usize,
        uop_index: usize,
        services: &mut Services<'_, '_, '_>,
    ) -> bool {
        let item = &self.work[id];
        if item.uop_issued[uop_index].is_some() || !self.members_dispatched(item, uop_index) {
            return false;
        }
        let members = item.members(self.base);
        if !dependencies_ready(
            self.model,
            item,
            uop_index,
            members,
            &self.dependencies,
            &self.outputs,
            self.cycle,
        ) {
            return false;
        }
        let chosen = if let Some(uop) = item.uop(uop_index) {
            reserve_uop_at(uop, members, &mut self.lanes, &mut self.usage, self.cycle)
        } else {
            super::super::reserve_lanes_at(
                &members[0],
                &mut self.lanes,
                &mut self.usage,
                self.cycle,
            )
        };
        let Some(chosen) = chosen else {
            return false;
        };
        let arch_start = item.arch_start;
        let reservation_owner = item
            .uop(uop_index)
            .and_then(|uop| uop.inherit_routes)
            .map_or(arch_start, |step| arch_start + step);
        self.work[id].uop_issued[uop_index] = Some(self.cycle);
        if let Some(handler) = services.handler.as_mut() {
            for (resource, cycles) in chosen {
                handler.reserved(self.cycle, reservation_owner, resource, cycles);
            }
        }
        if let Some(uop) = self.work[id].uop(uop_index).copied() {
            self.finish_fused_uop(id, uop_index, uop, services);
        } else {
            self.finish_plain_uop(id, services);
        }
        true
    }

    fn members_dispatched(&self, item: &Work<'_>, uop_index: usize) -> bool {
        let touched = item
            .uop(uop_index)
            .map(uop_steps)
            .unwrap_or_else(|| HashSet::from([0]));
        if touched.is_empty() {
            item.member_dispatched.iter().all(|ready| *ready)
        } else {
            touched.iter().all(|step| item.member_dispatched[*step])
        }
    }

    fn finish_fused_uop(
        &mut self,
        id: usize,
        uop_index: usize,
        uop: FusionMicroOp,
        services: &mut Services<'_, '_, '_>,
    ) {
        let item = &mut self.work[id];
        let members = item.members(self.base);
        let memory_done = uop_memory_complete(
            &uop,
            members,
            self.cycle + u64::from(uop.read_cycle),
            services.mem.as_deref_mut(),
        );
        item.uop_complete[uop_index] =
            Some((self.cycle + u64::from(uop.write_cycle)).max(memory_done));
        record_uop_outputs(
            &uop,
            members,
            item,
            self.cycle,
            memory_done,
            &mut self.outputs,
        );
        let branches: Vec<_> = uop
            .control_steps
            .iter()
            .filter_map(|&step| {
                members[step]
                    .branch
                    .map(|branch| (item.arch_start + step, branch))
            })
            .collect();
        for step in 0..item.group.len {
            if item.member_owners[step].contains(&uop_index) {
                update_member_events(item, step, self.cycle, &mut services.handler);
            }
        }
        let resolved = self.cycle + u64::from(uop.write_cycle);
        for (index, branch) in branches {
            self.resolve_branch(index, &branch, resolved, services);
        }
    }

    fn finish_plain_uop(&mut self, id: usize, services: &mut Services<'_, '_, '_>) {
        let item = &mut self.work[id];
        let slot = &item.members(self.base)[0];
        let complete =
            super::super::completion_cycle(slot, self.cycle, services.mem.as_deref_mut());
        item.uop_complete[0] = Some(complete);
        let state = OutputState {
            issued: self.cycle,
            ready: self.cycle + super::super::instr_latency(slot),
            memory_ready: complete,
            resource: slot.class.resources.first().copied(),
        };
        for reg in &slot.defs {
            self.outputs[item.arch_start].insert(reg.clone(), state);
        }
        let branch = slot.branch;
        let index = item.arch_start;
        update_member_events(item, 0, self.cycle, &mut services.handler);
        item.member_complete[0] = Some(complete);
        if let Some(branch) = branch {
            self.resolve_branch(index, &branch, complete, services);
        }
    }

    fn resolve_branch(
        &mut self,
        index: usize,
        branch: &super::super::BranchOutcome,
        resolved: u64,
        services: &mut Services<'_, '_, '_>,
    ) {
        let Some(predictor) = services.predictor.as_deref_mut() else {
            return;
        };
        let predicted = self
            .predicted_branches
            .remove(&index)
            .expect("branch was not predicted at dispatch");
        if predicted != branch.taken {
            self.mispredicts += 1;
            self.redirect = self.redirect.max(resolved + self.config.mispredict_penalty);
            if let Some(handler) = services.handler.as_mut() {
                handler.mispredicted(index, resolved, self.redirect);
            }
            self.pending_wrong_branch = None;
        }
        predictor.update(branch.pc, branch.target, branch.taken);
    }
}
