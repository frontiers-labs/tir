//! Transactional submission of sealed local Vulkan recordings.

use super::*;

const MAX_BATCH_COMMANDS: usize = 256;
const MAX_SHARED_POINTS: u32 = 4096;

fn validate_wait(point: Point, owner: *mut Device) -> i32 {
    if point.event.is_null() {
        return INVALID_ARGUMENT;
    }
    let event = unsafe { &*point.event };
    if event.owner != owner {
        return INVALID_ARGUMENT;
    }
    let failure = event_failure(event);
    if failure != 0 {
        return failure;
    }
    if event.requires_host_progress {
        match event.timeline.query() {
            Ok(current) if current < point.value => return UNSUPPORTED,
            Err(error) => return timeline_error(event, error),
            _ => {}
        }
    }
    SUCCESS
}

fn validate_signal(point: Point, owner: *mut Device) -> i32 {
    if point.event.is_null() || point.value == 0 {
        return INVALID_ARGUMENT;
    }
    let event = unsafe { &*point.event };
    if event.owner != owner
        || event.queue_owned
        || !event.source_event.is_null()
        || event.signal_domain != 1
    {
        return INVALID_ARGUMENT;
    }
    event_failure(event)
}

fn bindings_for(
    state: &CommandState,
    submit: *const SubmitDesc,
) -> Result<BTreeMap<u32, Point>, i32> {
    let mut used_slots = BTreeSet::new();
    for plan in &state.plans {
        for reference in plan.waits.iter().chain(&plan.signals) {
            if let EventRef::Slot(slot) = reference {
                used_slots.insert(*slot);
            }
        }
    }
    let mut bindings = BTreeMap::new();
    if !submit.is_null() {
        let desc = unsafe { &*submit };
        let status = validate_header(&desc.header, std::mem::size_of::<SubmitDesc>());
        if status != SUCCESS {
            return Err(status);
        }
        if desc.binding_count as usize != used_slots.len()
            || (desc.binding_count != 0 && desc.bindings.is_null())
        {
            return Err(INVALID_ARGUMENT);
        }
        let supplied = if desc.binding_count == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(desc.bindings, desc.binding_count as usize) }
        };
        for binding in supplied {
            if binding.reserved != 0
                || binding.point.event.is_null()
                || !used_slots.contains(&binding.slot)
                || bindings.insert(binding.slot, binding.point).is_some()
            {
                return Err(INVALID_ARGUMENT);
            }
        }
    }
    if bindings.len() != used_slots.len() {
        return Err(INVALID_ARGUMENT);
    }
    Ok(bindings)
}

fn submit_recordings(
    queue: *mut Queue,
    entries: &[BatchEntry],
    shared_waits: &[Point],
    shared_signals: &[Point],
    out: *mut Point,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = Point {
            event: ptr::null_mut(),
            value: 0,
        }
    };
    if queue.is_null() || entries.is_empty() || entries.len() > MAX_BATCH_COMMANDS {
        return INVALID_ARGUMENT;
    }
    let queue_ref = unsafe { &*queue };
    let mut distinct = BTreeSet::new();
    let mut commands = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.cmd.is_null() || !distinct.insert(entry.cmd as usize) {
            return INVALID_ARGUMENT;
        }
        let command = unsafe { &*entry.cmd };
        if command.device != queue_ref.parent || command.family != queue_ref.family {
            return INVALID_ARGUMENT;
        }
        commands.push(command);
    }
    let device = unsafe { &*queue_ref.parent };
    let Ok(_submission) = device.submission_lock.lock() else {
        return DEVICE_LOST;
    };
    if device.lost.load(Ordering::Acquire) || device.quiescent.load(Ordering::Acquire) {
        return DEVICE_LOST;
    }
    let mut guards = Vec::with_capacity(commands.len());
    for command in &commands {
        let Ok(mut state) = command.inner.lock() else {
            return DEVICE_LOST;
        };
        let status = refresh_command(&mut state);
        if status != SUCCESS {
            return status;
        }
        if state.state != RecordingState::Executable {
            return INVALID_ARGUMENT;
        }
        guards.push(state);
    }

    let mut resolved = Vec::new();
    if !shared_waits.is_empty() {
        for &point in shared_waits {
            let status = validate_wait(point, queue_ref.parent);
            if status != SUCCESS {
                return status;
            }
        }
        resolved.push(ResolvedSubmitSegment {
            command: None,
            waits: shared_waits.to_vec(),
            signals: Vec::new(),
        });
    }
    for (index, state) in guards.iter().enumerate() {
        if state.native.len() != state.plans.len() {
            return DEVICE_LOST;
        }
        let bindings = match bindings_for(state, entries[index].submit) {
            Ok(bindings) => bindings,
            Err(status) => return status,
        };
        let resolve = |reference: EventRef| match reference {
            EventRef::Point(point) => point,
            EventRef::Slot(slot) => bindings[&slot],
        };
        for (segment, plan) in state.plans.iter().enumerate() {
            let waits: Vec<Point> = plan.waits.iter().copied().map(&resolve).collect();
            let signals: Vec<Point> = plan.signals.iter().copied().map(&resolve).collect();
            for &point in &waits {
                let status = validate_wait(point, queue_ref.parent);
                if status != SUCCESS {
                    return status;
                }
            }
            for &point in &signals {
                let status = validate_signal(point, queue_ref.parent);
                if status != SUCCESS {
                    return status;
                }
            }
            resolved.push(ResolvedSubmitSegment {
                command: Some((index, segment)),
                waits,
                signals,
            });
        }
    }
    if !shared_signals.is_empty() {
        for &point in shared_signals {
            let status = validate_signal(point, queue_ref.parent);
            if status != SUCCESS {
                return status;
            }
        }
        resolved.push(ResolvedSubmitSegment {
            command: None,
            waits: Vec::new(),
            signals: shared_signals.to_vec(),
        });
    }

    let Ok(mut queue_state) = queue_ref.state.lock() else {
        return DEVICE_LOST;
    };
    if queue_state.poisoned {
        return SUBMISSION_FAILED;
    }
    let Some(value) = queue_state.next_value.checked_add(1) else {
        return INVALID_ARGUMENT;
    };
    let previous_queue_value = queue_state.next_value;
    let mut expected_signals: BTreeMap<usize, (u64, *mut Queue)> = BTreeMap::new();
    let mut original_signals: BTreeMap<usize, (u64, *mut Queue)> = BTreeMap::new();
    let mut prior_waits: BTreeMap<usize, u64> = BTreeMap::new();
    for segment in &resolved {
        for point in &segment.waits {
            let entry = prior_waits.entry(point.event as usize).or_default();
            *entry = (*entry).max(point.value);
        }
        for point in &segment.signals {
            let key = point.event as usize;
            let previous = if let Some(&state) = expected_signals.get(&key) {
                state
            } else {
                let event = unsafe { &*point.event };
                let state = event
                    .signal_state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let previous = (state.last_reserved, state.last_queue);
                original_signals.insert(key, previous);
                previous
            };
            if point.value <= previous.0
                || (previous.0 != 0
                    && !ptr::eq(previous.1, queue_ref)
                    && prior_waits
                        .get(&key)
                        .is_none_or(|&waited| waited < previous.0))
            {
                return INVALID_ARGUMENT;
            }
            expected_signals.insert(key, (point.value, queue));
        }
    }

    let mut segments = Vec::new();
    for spec in &resolved {
        let mut waits_by_event = BTreeMap::new();
        for point in &spec.waits {
            let value = waits_by_event.entry(point.event as usize).or_insert(0);
            *value = (*value).max(point.value);
        }
        let waits = waits_by_event
            .into_iter()
            .map(|(address, value)| (unsafe { &(*(address as *const Event)).timeline }, value))
            .collect();
        let mut segment = vulkan::SubmitSegment {
            command: spec
                .command
                .map(|(command, native)| &guards[command].native[native]),
            waits,
            signals: Vec::new(),
        };
        let mut signaled = BTreeSet::new();
        for point in &spec.signals {
            if !signaled.insert(point.event as usize) {
                segments.push(segment);
                segment = vulkan::SubmitSegment {
                    command: None,
                    waits: Vec::new(),
                    signals: Vec::new(),
                };
                signaled.clear();
                signaled.insert(point.event as usize);
            }
            segment
                .signals
                .push((unsafe { &(*point.event).timeline }, point.value));
        }
        segments.push(segment);
    }
    let Some(last) = segments.last_mut() else {
        return DEVICE_LOST;
    };
    last.signals.push((&queue_ref.completion.timeline, value));
    let point = Point {
        event: (&raw const queue_ref.completion).cast_mut(),
        value,
    };

    for (&event_address, &(reserved, last_queue)) in &expected_signals {
        let event = unsafe { &*(event_address as *const Event) };
        let mut state = event
            .signal_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.last_reserved = reserved;
        state.last_queue = last_queue;
    }
    queue_state.next_value = value;
    let mut retained = BTreeSet::new();
    for spec in &resolved {
        for point_to_retain in spec.waits.iter().chain(&spec.signals) {
            if retained.insert(point_to_retain.event as usize) {
                let event = unsafe { &*point_to_retain.event };
                if !event.queue_owned {
                    let mut inflight = event
                        .inflight
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    inflight.retain(|prior| match unsafe { &*prior.event }.timeline.query() {
                        Ok(completed) => completed < prior.value,
                        Err(_) => true,
                    });
                    inflight.push(point);
                }
            }
        }
    }
    if let Err(error) = device
        .backend
        .submit_segments(queue_ref.backend_index, &segments)
    {
        if error == -1 || error == -2 {
            set_backend_diagnostic(
                OUT_OF_MEMORY,
                error,
                "Vulkan queue submission ran out of memory",
            );
            for (&event_address, &(reserved, last_queue)) in &original_signals {
                let event = unsafe { &*(event_address as *const Event) };
                let mut state = event
                    .signal_state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.last_reserved = reserved;
                state.last_queue = last_queue;
            }
            queue_state.next_value = previous_queue_value;
            for event_address in retained {
                let event = unsafe { &*(event_address as *const Event) };
                if !event.queue_owned {
                    let mut inflight = event
                        .inflight
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    inflight
                        .retain(|prior| prior.event != point.event || prior.value != point.value);
                }
            }
            return OUT_OF_MEMORY;
        }
        if error == -4 {
            device.lost.store(true, Ordering::Release);
        }
        set_backend_diagnostic(SUBMISSION_FAILED, error, "Vulkan queue submission failed");
        queue_state.poisoned = true;
        queue_ref
            .completion
            .failure
            .store(SUBMISSION_FAILED, Ordering::Release);
        for &event_address in expected_signals.keys() {
            unsafe {
                (*(event_address as *mut Event))
                    .failure
                    .store(SUBMISSION_FAILED, Ordering::Release)
            };
        }
        for state in &mut guards {
            state.state = RecordingState::Pending;
            state.pending = Some(point);
        }
        return SUBMISSION_FAILED;
    }
    for state in &mut guards {
        state.state = RecordingState::Pending;
        state.pending = Some(point);
    }
    unsafe { *out = point };
    SUCCESS
}

/// Submits one sealed recording asynchronously on the selected queue.
///
/// # Safety
/// `queue` and `cmd` must be live, `out` writable, and a nonnull descriptor
/// must point to a complete binding array.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_submit(
    queue: *mut Queue,
    cmd: *mut Command,
    submit_desc: *const SubmitDesc,
    out: *mut Point,
) -> i32 {
    submit_recordings(
        queue,
        &[BatchEntry {
            cmd,
            submit: submit_desc,
        }],
        &[],
        &[],
        out,
    )
}

/// Submits a validated ordered batch as one queue transaction.
///
/// # Safety
/// `queue` must be live, `desc` and its arrays readable, and `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn nisyros_submit_batch(
    queue: *mut Queue,
    desc: *const BatchSubmitDesc,
    out: *mut Point,
) -> i32 {
    if out.is_null() {
        return INVALID_ARGUMENT;
    }
    unsafe {
        *out = Point {
            event: ptr::null_mut(),
            value: 0,
        }
    };
    if desc.is_null() {
        return INVALID_ARGUMENT;
    }
    let desc = unsafe { &*desc };
    let status = validate_header(&desc.header, std::mem::size_of::<BatchSubmitDesc>());
    if status != SUCCESS {
        return status;
    }
    if desc.command_count == 0
        || desc.command_count as usize > MAX_BATCH_COMMANDS
        || desc.commands.is_null()
        || desc.wait_count > MAX_SHARED_POINTS
        || (desc.wait_count != 0 && desc.waits.is_null())
        || desc.signal_count > MAX_SHARED_POINTS
        || (desc.signal_count != 0 && desc.signals.is_null())
    {
        return INVALID_ARGUMENT;
    }
    let commands =
        unsafe { std::slice::from_raw_parts(desc.commands, desc.command_count as usize) };
    let waits = if desc.wait_count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(desc.waits, desc.wait_count as usize) }
    };
    let signals = if desc.signal_count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(desc.signals, desc.signal_count as usize) }
    };
    submit_recordings(queue, commands, waits, signals, out)
}
