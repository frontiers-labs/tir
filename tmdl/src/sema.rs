use std::collections::{HashMap, HashSet};

use chumsky::error::Rich;

use crate::utils::{
    eval_bits_width, isa_param_values, parse_literal_value, resolve_effective_asm_for_instruction,
    resolve_effective_encoding_for_instruction, resolve_isa_param_values,
    resolve_params_for_instruction, resolve_template_chain,
};
use crate::{Span, Type, ast};

type Diag = Rich<'static, String, Span>;

fn isa_includes(
    isa_name: &str,
    required: &str,
    item_cache: &HashMap<&str, &ast::Item>,
    visiting: &mut HashSet<String>,
) -> bool {
    if isa_name == required {
        return true;
    }
    if !visiting.insert(isa_name.to_string()) {
        return false;
    }
    let includes = match item_cache.get(isa_name) {
        Some(ast::Item::Isa(isa)) => match &isa.requires {
            None => false,
            Some(ast::IsaRequirement::Single(parent)) => {
                isa_includes(parent, required, item_cache, visiting)
            }
            Some(ast::IsaRequirement::Any(parents)) | Some(ast::IsaRequirement::All(parents)) => {
                parents
                    .iter()
                    .any(|parent| isa_includes(parent, required, item_cache, visiting))
            }
        },
        _ => false,
    };
    visiting.remove(isa_name);
    includes
}

// TODO path strings must be interned
pub fn analyze(files: &[ast::File], text_only: bool) -> Vec<(String, Diag)> {
    let mut diags = vec![];

    let cache = build_item_cache(files);

    // TODO check item names are unique
    diags.extend(check_isas(files, &cache));
    diags.extend(check_templates(files, &cache));
    diags.extend(check_instructions(files, &cache, text_only));
    diags.extend(check_register_classes(files, &cache));
    diags.extend(check_abis(files, &cache));
    diags.extend(check_performance_model(files, &cache));
    for file in files {
        for item in &file.items {
            if let ast::Item::Isa(isa) = item
                && let Some(trap) = &isa.trap_handler
            {
                diags.extend(check_behavior(
                    &isa.name,
                    &trap.body,
                    &cache,
                    &file.file_name,
                ));
            }
        }
    }

    diags
}

fn check_abis(files: &[ast::File], item_cache: &HashMap<&str, &ast::Item>) -> Vec<(String, Diag)> {
    let classes: HashMap<&str, &ast::RegisterClass> = files
        .iter()
        .flat_map(|file| file.register_classes())
        .map(|class| (class.name.as_str(), class))
        .collect();
    let class_files: HashMap<String, &ast::RegisterClass> = classes
        .values()
        .map(|class| (class.name.clone(), *class))
        .collect();
    let mut diags = Vec::new();

    for file in files {
        for abi in file.abis() {
            if let Some(classifier) = &abi.classifier
                && !matches!(classifier.as_str(), "riscv" | "aapcs64" | "sysv")
            {
                diags.push((
                    file.file_name.clone(),
                    Rich::custom(
                        abi.span,
                        format!(
                            "ABI '{}' has unknown classifier '{}'; expected riscv, aapcs64, or sysv",
                            abi.name, classifier
                        ),
                    ),
                ));
            }
            if !abi.roles.iter().any(|role| role.name == "sp") {
                diags.push((
                    file.file_name.clone(),
                    Rich::custom(
                        abi.span,
                        format!("ABI '{}' does not declare the required 'sp' role", abi.name),
                    ),
                ));
            }
            let check_register = |register: &ast::AbiRegister, diags: &mut Vec<(String, Diag)>| {
                let Some(class) = classes.get(register.class.as_str()) else {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            register.span,
                            format!(
                                "ABI '{}' references unknown register '{}::{}'",
                                abi.name, register.class, register.name
                            ),
                        ),
                    ));
                    return;
                };
                let available = abi.for_isas.iter().any(|abi_isa| {
                    class.for_isas.iter().any(|class_isa| {
                        isa_includes(abi_isa, class_isa, item_cache, &mut HashSet::new())
                    })
                });
                let exists = class
                    .resolve_registers()
                    .any(|candidate| candidate.name == register.name);
                if !available || !exists {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            register.span,
                            format!(
                                "ABI '{}' references unknown register '{}::{}'",
                                abi.name, register.class, register.name
                            ),
                        ),
                    ));
                }
            };
            let check_sequence =
                |sequence: &ast::AbiRegisterSequence, diags: &mut Vec<(String, Diag)>| {
                    check_register(&sequence.start, diags);
                    if let Some(end) = &sequence.end {
                        check_register(end, diags);
                        if end.class != sequence.start.class {
                            diags.push((
                                file.file_name.clone(),
                                Rich::custom(
                                    sequence.span,
                                    format!(
                                        "ABI '{}' register range must stay within one class",
                                        abi.name
                                    ),
                                ),
                            ));
                        }
                    }
                };
            let expand_sequences = |sequences: &[ast::AbiRegisterSequence]| {
                let mut registers = Vec::new();
                for sequence in sequences {
                    let Some(class) = classes.get(sequence.start.class.as_str()) else {
                        continue;
                    };
                    let resolved: Vec<_> = class.resolve_registers().collect();
                    let Some(start) = resolved
                        .iter()
                        .find(|candidate| candidate.name == sequence.start.name)
                    else {
                        continue;
                    };
                    let Some(start_index) = start.encoding_index() else {
                        registers.push((
                            format!("{}:{}", sequence.start.class, sequence.start.name),
                            format!("{}::{}", sequence.start.class, sequence.start.name),
                            sequence.span,
                        ));
                        continue;
                    };
                    let end_index = sequence
                        .end
                        .as_ref()
                        .filter(|end| end.class == sequence.start.class)
                        .and_then(|end| {
                            resolved
                                .iter()
                                .find(|candidate| candidate.name == end.name)
                                .and_then(|register| register.encoding_index())
                        })
                        .unwrap_or(start_index);
                    let file_name = class.register_file(&class_files);
                    for register in &resolved {
                        let Some(index) = register.encoding_index() else {
                            continue;
                        };
                        if (start_index..=end_index).contains(&index) {
                            registers.push((
                                format!("{file_name}:{index}"),
                                format!("{}::{}", sequence.start.class, register.name),
                                sequence.span,
                            ));
                        }
                    }
                }
                registers
            };
            let kind_name = |kind| match kind {
                ast::AbiValueKind::Int => "int",
                ast::AbiValueKind::Float => "float",
                ast::AbiValueKind::Vector => "vector",
            };

            for (passes, direction) in [(&abi.args, "argument"), (&abi.rets, "return")] {
                let mut seen = HashSet::new();
                for pass in passes {
                    if !seen.insert(pass.kind) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                pass.span,
                                format!(
                                    "ABI '{}' declares more than one {} {direction} sequence",
                                    abi.name,
                                    kind_name(pass.kind)
                                ),
                            ),
                        ));
                    }
                }
            }

            for role in &abi.roles {
                check_register(&role.register, &mut diags);
            }
            let mut role_registers: HashMap<String, &str> = HashMap::new();
            for role in &abi.roles {
                let Some(class) = classes.get(role.register.class.as_str()) else {
                    continue;
                };
                let Some(register) = class
                    .resolve_registers()
                    .find(|candidate| candidate.name == role.register.name)
                else {
                    continue;
                };
                let identity = match register.encoding_index() {
                    Some(index) => format!("{}:{index}", class.register_file(&class_files)),
                    None => format!("{}:{}", role.register.class, role.register.name),
                };
                if let Some(previous) = role_registers.insert(identity, &role.name) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            role.span,
                            format!(
                                "ABI '{}' assigns register '{}::{}' to both '{}' and '{}'",
                                abi.name,
                                role.register.class,
                                role.register.name,
                                previous,
                                role.name
                            ),
                        ),
                    ));
                }
            }
            for (pass, direction) in abi
                .args
                .iter()
                .map(|pass| (pass, "argument"))
                .chain(abi.rets.iter().map(|pass| (pass, "return")))
            {
                for sequence in &pass.registers {
                    check_sequence(sequence, &mut diags);
                }
                let mut seen = HashSet::new();
                for (identity, display, span) in expand_sequences(&pass.registers) {
                    if !seen.insert(identity) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                span,
                                format!(
                                    "ABI '{}' {} {direction} sequence contains duplicate register '{}'",
                                    abi.name,
                                    kind_name(pass.kind),
                                    display
                                ),
                            ),
                        ));
                    }
                }
            }
            for sequence in abi
                .callee_saved
                .iter()
                .flatten()
                .chain(abi.reserved.iter().flatten())
            {
                check_sequence(sequence, &mut diags);
            }

            let reserved: HashMap<_, _> =
                expand_sequences(abi.reserved.as_deref().unwrap_or_default())
                    .into_iter()
                    .map(|(identity, display, _)| (identity, display))
                    .collect();
            for pass in &abi.args {
                for (identity, display, span) in expand_sequences(&pass.registers) {
                    if reserved.contains_key(&identity) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                span,
                                format!(
                                    "ABI '{}' uses reserved register '{}' for arguments",
                                    abi.name, display
                                ),
                            ),
                        ));
                    }
                }
            }
            for (identity, display, span) in
                expand_sequences(abi.callee_saved.as_deref().unwrap_or_default())
            {
                if reserved.contains_key(&identity) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            span,
                            format!(
                                "ABI '{}' lists register '{}' as both callee-saved and reserved",
                                abi.name, display
                            ),
                        ),
                    ));
                }
            }

            let args_by_kind: HashMap<_, _> = abi
                .args
                .iter()
                .map(|sequence| (sequence.kind, sequence))
                .collect();
            for sequence in &abi.args {
                if let Some(ast::AbiOverflow::Kind(target)) = sequence.overflow
                    && !args_by_kind.contains_key(&target)
                {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            sequence.span,
                            format!(
                                "ABI '{}' {} argument overflow references undeclared {} sequence",
                                abi.name,
                                kind_name(sequence.kind),
                                kind_name(target)
                            ),
                        ),
                    ));
                }
            }
            let mut reported_cycle_kinds = HashSet::new();
            for sequence in &abi.args {
                if reported_cycle_kinds.contains(&sequence.kind) {
                    continue;
                }
                let mut path = Vec::new();
                let mut positions = HashMap::new();
                let mut kind = sequence.kind;
                loop {
                    if let Some(&position) = positions.get(&kind) {
                        let mut cycle = path[position..].to_vec();
                        cycle.push(kind);
                        reported_cycle_kinds.extend(cycle.iter().copied());
                        let display = cycle
                            .iter()
                            .map(|kind| kind_name(*kind))
                            .collect::<Vec<_>>()
                            .join(" -> ");
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                sequence.span,
                                format!(
                                    "ABI '{}' argument overflow chain contains a cycle: {display}",
                                    abi.name
                                ),
                            ),
                        ));
                        break;
                    }
                    positions.insert(kind, path.len());
                    path.push(kind);
                    let Some(next) = args_by_kind.get(&kind).and_then(|sequence| {
                        if let Some(ast::AbiOverflow::Kind(next)) = sequence.overflow {
                            Some(next)
                        } else {
                            None
                        }
                    }) else {
                        break;
                    };
                    if !args_by_kind.contains_key(&next) {
                        break;
                    }
                    kind = next;
                }
            }
        }
    }

    diags
}

/// Validate sub-register-view params: `WRITE_POLICY` must be one of the two known
/// policies, a nonzero `BIT_OFFSET` requires the merge policy, and the view
/// (`BIT_OFFSET + WIDTH`) must fit within its storage class's `WIDTH`.
fn check_register_classes(
    files: &[ast::File],
    item_cache: &HashMap<&str, &ast::Item>,
) -> Vec<(String, Diag)> {
    let mut diags: Vec<(String, Diag)> = Vec::new();
    let classes: HashMap<String, &ast::RegisterClass> = files
        .iter()
        .flat_map(|f| f.register_classes())
        .map(|rc| (rc.name.clone(), rc))
        .collect();

    let eval = |rc: &ast::RegisterClass,
                name: &str,
                params: &HashMap<String, i64>|
     -> Option<i64> {
        match rc.parameters.get(name)? {
            (_, Some(ast::Expr::Lit(ast::Lit::Int(li)))) => Some(parse_literal_value(li) as i64),
            (_, Some(ast::Expr::Field(f))) if matches!(&*f.base, ast::Expr::Ident(id) if id.name == "self") => {
                params.get(f.member.as_str()).copied()
            }
            _ => None,
        }
    };

    for file in files {
        for rc in file.register_classes() {
            let policy = rc.parameters.get("WRITE_POLICY");
            let merge = match policy {
                None => false,
                Some((_, Some(ast::Expr::Lit(ast::Lit::Str(s))))) => match s.value() {
                    "merge" => true,
                    "zero_extend" => false,
                    other => {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                rc.span,
                                format!(
                                    "register class '{}' has invalid WRITE_POLICY '{other}'; \
                                     expected \"zero_extend\" or \"merge\"",
                                    rc.name
                                ),
                            ),
                        ));
                        continue;
                    }
                },
                Some(_) => {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            rc.span,
                            format!(
                                "register class '{}' WRITE_POLICY must be a string literal",
                                rc.name
                            ),
                        ),
                    ));
                    continue;
                }
            };

            if !rc.parameters.contains_key("BIT_OFFSET") && policy.is_none() {
                continue;
            }
            let isa = rc.for_isas.first().map(String::as_str).unwrap_or("");
            let params = isa_param_values(isa, item_cache);
            let offset = eval(rc, "BIT_OFFSET", &params).unwrap_or(0);
            if offset != 0 && !merge {
                diags.push((
                    file.file_name.clone(),
                    Rich::custom(
                        rc.span,
                        format!(
                            "register class '{}' has a nonzero BIT_OFFSET but WRITE_POLICY is \
                             not \"merge\"",
                            rc.name
                        ),
                    ),
                ));
            }
            let storage = rc.register_file(&classes);
            if let (Some(width), Some(sc)) = (eval(rc, "WIDTH", &params), classes.get(storage))
                && let Some(storage_width) = eval(sc, "WIDTH", &params)
                && offset + width > storage_width
            {
                diags.push((
                    file.file_name.clone(),
                    Rich::custom(
                        rc.span,
                        format!(
                            "register class '{}' view (BIT_OFFSET {offset} + WIDTH {width}) \
                             exceeds storage class '{storage}' WIDTH {storage_width}",
                            rc.name
                        ),
                    ),
                ));
            }
        }
    }
    diags
}

/// Validate the performance model: instruction `schedule` blocks, `unit`
/// declarations, and `machine` resource/bind references must all resolve. This is
/// the payoff of declaring units up front — a mistyped class name is an error
/// here rather than a silent fall-through to the default cost at runtime.
fn check_performance_model(
    files: &[ast::File],
    item_cache: &HashMap<&str, &ast::Item>,
) -> Vec<(String, Diag)> {
    let mut diags: Vec<(String, Diag)> = Vec::new();

    // Duplicate `unit` declarations: units form a namespace consumed by name, so a
    // silent collapse in the item cache would be confusing.
    let mut seen_units: HashSet<&str> = HashSet::new();
    for file in files {
        for unit in file.count() {
            if !seen_units.insert(unit.name.as_str()) {
                diags.push((
                    file.file_name.clone(),
                    Rich::custom(
                        unit.span,
                        format!("duplicate unit declaration '{}'", unit.name),
                    ),
                ));
            }
        }
    }

    // `schedule { units = [..] }` names — on instructions and on templates (which
    // derived instructions inherit) — must resolve to a `unit`.
    let schedule_owners = files.iter().flat_map(|file| {
        let insts = file.instructions().filter_map(|i| {
            i.schedule
                .as_ref()
                .map(|s| (&file.file_name, "instruction", &i.name, s))
        });
        let tmpls = file.templates().filter_map(|t| {
            t.schedule
                .as_ref()
                .map(|s| (&file.file_name, "template", &t.name, s))
        });
        insts.chain(tmpls)
    });
    for (file_name, kind, owner, schedule) in schedule_owners {
        for unit in &schedule.classes {
            match item_cache.get(unit.as_str()) {
                Some(ast::Item::Unit(_)) => {}
                Some(_) => diags.push((
                    file_name.clone(),
                    Rich::custom(
                        schedule.span,
                        format!("'{unit}' referenced by {kind} '{owner}' is not a unit"),
                    ),
                )),
                None => diags.push((
                    file_name.clone(),
                    Rich::custom(
                        schedule.span,
                        format!("unknown unit '{unit}' referenced by {kind} '{owner}'"),
                    ),
                )),
            }
        }
    }

    // Machine `resource` names must be unique; each `bind` must target a declared
    // `unit` (at most once) and may only `use` resources declared in that machine.
    for file in files {
        for machine in file.machines() {
            let mut resource_names: HashSet<&str> = HashSet::new();
            for res in &machine.resources {
                if !resource_names.insert(res.name.as_str()) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            res.span,
                            format!(
                                "duplicate resource '{}' in machine '{}'",
                                res.name, machine.name
                            ),
                        ),
                    ));
                }
            }

            let mut modeled_resource_names = resource_names.clone();
            let mut group_names: HashSet<&str> = HashSet::new();
            for group in &machine.resource_groups {
                if !group_names.insert(group.name.as_str())
                    || !modeled_resource_names.insert(group.name.as_str())
                {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            group.span,
                            format!(
                                "duplicate resource group '{}' in machine '{}'",
                                group.name, machine.name
                            ),
                        ),
                    ));
                }
            }
            let resource_groups: HashMap<&str, &ast::ResourceExpr> = machine
                .resource_groups
                .iter()
                .map(|group| (group.name.as_str(), &group.resources))
                .collect();
            for group in &machine.resource_groups {
                if resource_group_is_cyclic(
                    group.name.as_str(),
                    &resource_groups,
                    &mut HashSet::new(),
                ) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            group.span,
                            format!("cyclic resource group '{}'", group.name),
                        ),
                    ));
                }
            }
            for group in &machine.resource_groups {
                if has_non_positive_occupancy(&group.resources) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(group.span, "resource occupancy must be positive"),
                    ));
                }
                for referenced in resource_references(&group.resources) {
                    if !modeled_resource_names.contains(referenced) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                group.span,
                                format!(
                                    "group '{}' references unknown resource '{}' in machine '{}'",
                                    group.name, referenced, machine.name
                                ),
                            ),
                        ));
                    }
                }
            }

            let frontend_decoder_names: HashSet<&str> = machine
                .frontend
                .iter()
                .flat_map(|frontend| frontend.decode.decoders.iter())
                .map(|decoder| decoder.name.as_str())
                .collect();
            if let Some(frontend) = &machine.frontend {
                for (name, value) in [
                    ("bytes_per_cycle", frontend.fetch.bytes_per_cycle),
                    ("window_bytes", frontend.fetch.window_bytes),
                    ("alignment", frontend.fetch.alignment),
                    ("queue_bytes", frontend.fetch.queue_bytes),
                ] {
                    if value <= 0 {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                frontend.fetch.span,
                                format!("frontend fetch {name} must be positive"),
                            ),
                        ));
                    }
                }
                for (name, value) in [
                    ("uops_per_cycle", frontend.decode.uops_per_cycle),
                    ("queue_uops", frontend.decode.queue_uops),
                ] {
                    if value <= 0 {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                frontend.decode.span,
                                format!("frontend decode {name} must be positive"),
                            ),
                        ));
                    }
                }
                for decoder in &frontend.decode.decoders {
                    if decoder.max_uops_per_instruction <= 0 {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                decoder.span,
                                format!(
                                    "frontend decoder '{}' max_uops_per_instruction must be positive",
                                    decoder.name
                                ),
                            ),
                        ));
                    }
                }
                let mut decoder_names = HashSet::new();
                for decoder in &frontend.decode.decoders {
                    if !decoder_names.insert(decoder.name.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                decoder.span,
                                format!("duplicate frontend decoder '{}'", decoder.name),
                            ),
                        ));
                    }
                }
                if frontend.decode.slots.is_empty() {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            frontend.decode.span,
                            "frontend decode must declare at least one slot",
                        ),
                    ));
                }
                for slot in &frontend.decode.slots {
                    if !decoder_names.contains(slot.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                frontend.decode.span,
                                format!("frontend decode slot references unknown decoder '{slot}'"),
                            ),
                        ));
                    }
                }
                if let Some(cache) = &frontend.decoded_cache {
                    for (name, value) in [
                        ("sets", cache.sets),
                        ("ways", cache.ways),
                        ("line_bytes", cache.line_bytes),
                        ("line_uops", cache.line_uops),
                        ("deliver_uops_per_cycle", cache.deliver_uops_per_cycle),
                    ] {
                        if value <= 0 {
                            diags.push((
                                file.file_name.clone(),
                                Rich::custom(
                                    cache.span,
                                    format!("frontend decoded_cache {name} must be positive"),
                                ),
                            ));
                        }
                    }
                }
            }

            // `reg_file` names must be unique and resolve to a physical register
            // file (the root of a register class's inheritance chain) of a class
            // available to one of this machine's ISAs.
            let class_map: HashMap<String, &ast::RegisterClass> = files
                .iter()
                .flat_map(|f| f.register_classes())
                .map(|rc| (rc.name.clone(), rc))
                .collect();
            let machine_isas: HashSet<&str> = machine.for_isas.iter().map(String::as_str).collect();
            let valid_files: HashSet<&str> = class_map
                .values()
                .filter(|rc| {
                    rc.for_isas
                        .iter()
                        .any(|i| machine_isas.contains(i.as_str()))
                })
                .map(|rc| rc.register_file(&class_map))
                .collect();
            let mut reg_file_names: HashSet<&str> = HashSet::new();
            for (name, _) in &machine.reg_files {
                if !reg_file_names.insert(name.as_str()) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            machine.span,
                            format!(
                                "duplicate reg_file '{}' in machine '{}'",
                                name, machine.name
                            ),
                        ),
                    ));
                }
                if !valid_files.contains(name.as_str()) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            machine.span,
                            format!(
                                "machine '{}' declares reg_file '{}' which is not a physical register file of its ISA(s)",
                                machine.name, name
                            ),
                        ),
                    ));
                }
            }

            let phase_names: HashSet<&str> =
                machine.pipeline.iter().map(|p| p.name.as_str()).collect();

            let mut bound_units: HashSet<&str> = HashSet::new();
            for bind in &machine.binds {
                if bind.decode_uops.is_some_and(|count| count <= 0) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(bind.span, "decode_uops must be positive"),
                    ));
                }
                if let Some(message) =
                    eliminated_conflict(bind.eliminated, bind.latency, &bind.uses, &bind.uops)
                {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(bind.span, message.to_string()),
                    ));
                }
                if bind.decode_cycles.is_some_and(|cycles| cycles <= 0) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(bind.span, "decode_cycles must be positive"),
                    ));
                }
                if let Some(decoder) = &bind.decoder {
                    if !frontend_decoder_names.contains(decoder.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                bind.span,
                                format!(
                                    "bind for unit '{}' references unknown frontend decoder '{}'",
                                    bind.unit, decoder
                                ),
                            ),
                        ));
                    } else if machine.frontend.as_ref().is_some_and(|frontend| {
                        !frontend_has_capable_decoder(
                            frontend,
                            Some(decoder),
                            effective_decode_uops(bind.decode_uops, &bind.uops),
                        )
                    }) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                bind.span,
                                format!(
                                    "bind for unit '{}' frontend has no capable '{}' decoder slot",
                                    bind.unit, decoder
                                ),
                            ),
                        ));
                    }
                } else if machine.frontend.as_ref().is_some_and(|frontend| {
                    !frontend_has_capable_decoder(
                        frontend,
                        None,
                        effective_decode_uops(bind.decode_uops, &bind.uops),
                    )
                }) {
                    let uops = effective_decode_uops(bind.decode_uops, &bind.uops);
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            bind.span,
                            format!(
                                "bind for unit '{}' frontend has no decoder slot capable of {uops} micro-ops",
                                bind.unit
                            ),
                        ),
                    ));
                }
                // Phase-based `reads`/`writes` must name a stage in this machine's
                // pipeline (and so require a `pipeline` block to exist at all).
                for phase in bind.reads.iter().chain(bind.writes.iter()) {
                    if !phase_names.contains(phase.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                bind.span,
                                format!(
                                    "bind for unit '{}' references phase '{}' not in machine '{}' pipeline",
                                    bind.unit, phase, machine.name
                                ),
                            ),
                        ));
                    }
                }

                match item_cache.get(bind.unit.as_str()) {
                    Some(ast::Item::Unit(_)) => {}
                    Some(_) => diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            bind.span,
                            format!(
                                "'{}' bound in machine '{}' is not a unit",
                                bind.unit, machine.name
                            ),
                        ),
                    )),
                    None => diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            bind.span,
                            format!(
                                "machine '{}' binds unknown unit '{}'",
                                machine.name, bind.unit
                            ),
                        ),
                    )),
                }

                if !bound_units.insert(bind.unit.as_str()) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            bind.span,
                            format!(
                                "duplicate bind for unit '{}' in machine '{}'",
                                bind.unit, machine.name
                            ),
                        ),
                    ));
                }

                for used in &bind.uses {
                    if !resource_names.contains(used.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                bind.span,
                                format!(
                                    "bind for unit '{}' uses unknown resource '{}' in machine '{}'",
                                    bind.unit, used, machine.name
                                ),
                            ),
                        ));
                    }
                }
                for uop in &bind.uops {
                    if uop.count <= 0 {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(uop.span, "micro-op count must be positive"),
                        ));
                    }
                    if has_non_positive_occupancy(&uop.resources) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(uop.span, "resource occupancy must be positive"),
                        ));
                    }
                    for referenced in resource_references(&uop.resources) {
                        if !modeled_resource_names.contains(referenced) {
                            diags.push((
                                file.file_name.clone(),
                                Rich::custom(
                                    uop.span,
                                    format!(
                                        "bind for unit '{}' micro-op references unknown resource '{}' in machine '{}'",
                                        bind.unit, referenced, machine.name
                                    ),
                                ),
                            ));
                        }
                    }
                }
            }

            // Overrides target a real instruction (at most once), use this
            // machine's resources, and reference real pipeline phases.
            let mut overridden: HashSet<&str> = HashSet::new();
            for ov in &machine.overrides {
                if ov.decode_uops.is_some_and(|count| count <= 0) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(ov.span, "decode_uops must be positive"),
                    ));
                }
                if let Some(message) =
                    eliminated_conflict(ov.eliminated, ov.latency, &ov.uses, &ov.uops)
                {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(ov.span, message.to_string()),
                    ));
                }
                if ov.decode_cycles.is_some_and(|cycles| cycles <= 0) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(ov.span, "decode_cycles must be positive"),
                    ));
                }
                if let Some(decoder) = &ov.decoder
                    && !frontend_decoder_names.contains(decoder.as_str())
                {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            ov.span,
                            format!(
                                "override for '{}' references unknown frontend decoder '{}'",
                                ov.instruction, decoder
                            ),
                        ),
                    ));
                }
                match item_cache.get(ov.instruction.as_str()) {
                    Some(ast::Item::Instruction(_)) => {}
                    Some(_) => diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            ov.span,
                            format!(
                                "override target '{}' in machine '{}' is not an instruction",
                                ov.instruction, machine.name
                            ),
                        ),
                    )),
                    None => diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            ov.span,
                            format!(
                                "machine '{}' overrides unknown instruction '{}'",
                                machine.name, ov.instruction
                            ),
                        ),
                    )),
                }
                if !overridden.insert(ov.instruction.as_str()) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            ov.span,
                            format!(
                                "duplicate override for instruction '{}' in machine '{}'",
                                ov.instruction, machine.name
                            ),
                        ),
                    ));
                }
                for used in &ov.uses {
                    if !resource_names.contains(used.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                ov.span,
                                format!(
                                    "override for '{}' uses unknown resource '{}' in machine '{}'",
                                    ov.instruction, used, machine.name
                                ),
                            ),
                        ));
                    }
                }
                for uop in &ov.uops {
                    if uop.count <= 0 {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(uop.span, "micro-op count must be positive"),
                        ));
                    }
                    if has_non_positive_occupancy(&uop.resources) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(uop.span, "resource occupancy must be positive"),
                        ));
                    }
                    for referenced in resource_references(&uop.resources) {
                        if !modeled_resource_names.contains(referenced) {
                            diags.push((
                                file.file_name.clone(),
                                Rich::custom(
                                    uop.span,
                                    format!(
                                        "override for '{}' micro-op references unknown resource '{}' in machine '{}'",
                                        ov.instruction, referenced, machine.name
                                    ),
                                ),
                            ));
                        }
                    }
                }
                for phase in ov.reads.iter().chain(ov.writes.iter()) {
                    if !phase_names.contains(phase.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                ov.span,
                                format!(
                                    "override for '{}' references phase '{}' not in machine '{}' pipeline",
                                    ov.instruction, phase, machine.name
                                ),
                            ),
                        ));
                    }
                }
            }

            // Fusion rules name instruction mnemonics on both sides.
            if !machine.fusions.is_empty() {
                let known_mnemonics: HashSet<String> = files
                    .iter()
                    .flat_map(|f| f.instructions())
                    .filter_map(|inst| {
                        let params = resolve_params_for_instruction(inst, item_cache);
                        params
                            .get("MNEMONIC")
                            .and_then(|(_, value)| value.as_ref())
                            .and_then(as_string_literal)
                    })
                    .collect();
                for fusion in &machine.fusions {
                    for mnemonic in fusion.first.iter().chain(fusion.second.iter()) {
                        if !known_mnemonics.contains(mnemonic) {
                            diags.push((
                                file.file_name.clone(),
                                Rich::custom(
                                    fusion.span,
                                    format!(
                                        "fusion mnemonic '{}' matches no instruction",
                                        mnemonic
                                    ),
                                ),
                            ));
                        }
                    }
                }
            }

            // Forwards run between this machine's resources, each pair at most once.
            let mut fwd_pairs: HashSet<(&str, &str)> = HashSet::new();
            for fw in &machine.forwards {
                for (which, res) in [("source", &fw.from), ("target", &fw.to)] {
                    if !resource_names.contains(res.as_str()) {
                        diags.push((
                            file.file_name.clone(),
                            Rich::custom(
                                fw.span,
                                format!(
                                    "forward {} '{}' is not a resource of machine '{}'",
                                    which, res, machine.name
                                ),
                            ),
                        ));
                    }
                }
                if !fwd_pairs.insert((fw.from.as_str(), fw.to.as_str())) {
                    diags.push((
                        file.file_name.clone(),
                        Rich::custom(
                            fw.span,
                            format!(
                                "duplicate forward '{}' => '{}' in machine '{}'",
                                fw.from, fw.to, machine.name
                            ),
                        ),
                    ));
                }
            }
        }
    }

    diags
}

fn resource_references(expr: &ast::ResourceExpr) -> Vec<&str> {
    match expr {
        ast::ResourceExpr::Resource(name) => vec![name],
        ast::ResourceExpr::Any(resources) | ast::ResourceExpr::All(resources) => {
            resources.iter().flat_map(resource_references).collect()
        }
        ast::ResourceExpr::Occupied { resource, .. } => resource_references(resource),
    }
}

fn has_non_positive_occupancy(expr: &ast::ResourceExpr) -> bool {
    match expr {
        ast::ResourceExpr::Resource(_) => false,
        ast::ResourceExpr::Any(resources) | ast::ResourceExpr::All(resources) => {
            resources.iter().any(has_non_positive_occupancy)
        }
        ast::ResourceExpr::Occupied { resource, cycles } => {
            *cycles <= 0 || has_non_positive_occupancy(resource)
        }
    }
}

fn resource_group_is_cyclic<'a>(
    name: &'a str,
    groups: &HashMap<&'a str, &'a ast::ResourceExpr>,
    visiting: &mut HashSet<&'a str>,
) -> bool {
    if !visiting.insert(name) {
        return true;
    }
    let cyclic = groups.get(name).is_some_and(|expr| {
        resource_references(expr)
            .into_iter()
            .filter(|referenced| groups.contains_key(referenced))
            .any(|referenced| resource_group_is_cyclic(referenced, groups, visiting))
    });
    visiting.remove(name);
    cyclic
}

/// An `eliminated` instruction completes in the rename stage, so it can neither
/// occupy an execution resource nor carry latency.
fn eliminated_conflict(
    eliminated: Option<bool>,
    latency: Option<i64>,
    uses: &[String],
    uops: &[ast::MicroOp],
) -> Option<&'static str> {
    if eliminated != Some(true) {
        return None;
    }
    if !uses.is_empty() || !uops.is_empty() {
        return Some("eliminated instruction cannot reserve resources");
    }
    if latency.is_some_and(|latency| latency != 0) {
        return Some("eliminated instruction must have latency = 0");
    }
    None
}

fn effective_decode_uops(explicit: Option<i64>, uops: &[ast::MicroOp]) -> i64 {
    explicit.unwrap_or_else(|| uops.iter().map(|uop| uop.count).sum::<i64>().max(1))
}

fn frontend_has_capable_decoder(
    frontend: &ast::Frontend,
    required: Option<&String>,
    uops: i64,
) -> bool {
    frontend.decode.slots.iter().any(|slot| {
        required.is_none_or(|required| required == slot)
            && frontend
                .decode
                .decoders
                .iter()
                .any(|decoder| decoder.name == *slot && decoder.max_uops_per_instruction >= uops)
    })
}

fn build_item_cache(files: &[ast::File]) -> HashMap<&str, &ast::Item> {
    files
        .iter()
        .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
        .collect::<HashMap<_, _>>()
}

fn isa_parents(requirement: &ast::IsaRequirement) -> Vec<&str> {
    match requirement {
        ast::IsaRequirement::Single(parent) => vec![parent.as_str()],
        ast::IsaRequirement::All(parents) | ast::IsaRequirement::Any(parents) => {
            parents.iter().map(String::as_str).collect()
        }
    }
}

fn encoding_value_name(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Ident(id) => Some(id.name.as_str()),
        ast::Expr::Slice(slc) => match &*slc.base {
            ast::Expr::Ident(id) => Some(id.name.as_str()),
            _ => None,
        },
        ast::Expr::IndexAccess(idx) => match &*idx.base {
            ast::Expr::Ident(id) => Some(id.name.as_str()),
            _ => None,
        },
        _ => None,
    }
}

// Checks that all ISA parents are defined and are also ISAs.
fn check_isas(files: &[ast::File], item_cache: &HashMap<&str, &ast::Item>) -> Vec<(String, Diag)> {
    files
        .iter()
        .flat_map(|file| {
            file.isas().flat_map(|isa| {
                isa.requires
                    .as_ref()
                    .map(isa_parents)
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|parent| match item_cache.get(parent) {
                        None => Some((
                            file.file_name.clone(),
                            Rich::custom(
                                isa.span,
                                format!("Unknown parent '{}' for ISA '{}'", parent, isa.name),
                            ),
                        )),
                        Some(item) if !matches!(item, ast::Item::Isa(_)) => Some((
                            file.file_name.clone(),
                            Rich::custom(
                                isa.span,
                                format!(
                                    "Parent '{}' for ISA '{}' must also be an ISA",
                                    parent, isa.name
                                ),
                            ),
                        )),
                        _ => None,
                    })
            })
        })
        .collect()
}

fn check_templates(
    files: &[ast::File],
    item_cache: &HashMap<&str, &ast::Item>,
) -> Vec<(String, Diag)> {
    files
        .iter()
        .flat_map(|f| {
            f.templates()
                .flat_map(|t| check_template_parents(t, item_cache, &f.file_name).into_iter())
        })
        .collect()
}

fn check_instructions(
    files: &[ast::File],
    item_cache: &HashMap<&str, &ast::Item>,
    text_only: bool,
) -> Vec<(String, Diag)> {
    let mut diags: Vec<(String, Diag)> = files
        .iter()
        .flat_map(|f| {
            f.instructions().flat_map(|i| {
                check_instruction_consistent(i, item_cache, &f.file_name, text_only).into_iter()
            })
        })
        .collect();

    let mut first_by_opname: HashMap<String, (&str, Span, &str)> = HashMap::new();
    for file in files {
        for instruction in file.instructions() {
            let params = resolve_params_for_instruction(instruction, item_cache);
            let opname = params
                .get("OPNAME")
                .and_then(|(_, value)| value.as_ref())
                .and_then(as_string_literal)
                .or_else(|| {
                    params
                        .get("MNEMONIC")
                        .and_then(|(_, value)| value.as_ref())
                        .and_then(as_string_literal)
                });

            let Some(opname) = opname else {
                continue;
            };

            if let Some((first_file, _first_span, first_inst_name)) = first_by_opname.get(&opname) {
                diags.push((
                    file.file_name.clone(),
                    Rich::custom(
                        instruction.span,
                        format!(
                            "Instruction '{}' resolves operation name '{}' that duplicates instruction '{}' in file '{}'",
                            instruction.name, opname, first_inst_name, first_file
                        ),
                    ),
                ));
            } else {
                first_by_opname.insert(
                    opname,
                    (
                        file.file_name.as_str(),
                        instruction.span,
                        instruction.name.as_str(),
                    ),
                );
            }
        }
    }

    diags
}

fn as_string_literal(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Lit(ast::Lit::Str(s)) => Some(s.value().to_string()),
        ast::Expr::Block(b) if b.last_expr_return => b.stmts.last().and_then(as_string_literal),
        _ => None,
    }
}

// Checks that all parent templates exist and are also templates.
fn check_template_parents(
    template: &ast::Template,
    item_cache: &HashMap<&str, &ast::Item>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    let mut diags = vec![];
    let mut visited: HashSet<&str> = HashSet::new();
    visited.insert(template.name.as_str());
    let mut ancestor_params: HashSet<&str> = HashSet::new();

    let mut current = template;

    while let Some(parent_name) = current.parent_template.as_deref() {
        match item_cache.get(parent_name).copied() {
            None => {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        current.span,
                        format!(
                            "Unknown parent template '{}' for template '{}'",
                            parent_name, current.name
                        ),
                    ),
                ));
                break;
            }
            Some(ast::Item::Template(parent_tmpl)) => {
                if !visited.insert(parent_name) {
                    diags.push((
                        file_name.to_string(),
                        Rich::custom(
                            current.span,
                            format!("Cyclic template inheritance involving '{}'", parent_name),
                        ),
                    ));
                    break;
                }
                ancestor_params.extend(parent_tmpl.params.keys().map(String::as_str));
                current = parent_tmpl;
            }
            Some(_) => {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        current.span,
                        format!(
                            "Parent '{}' of template '{}' must also be a template",
                            parent_name, current.name
                        ),
                    ),
                ));
                break;
            }
        }
    }

    for (param_name, (_ty, value)) in &template.params {
        if ancestor_params.contains(param_name.as_str()) && value.is_none() {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    template.span,
                    format!(
                        "Parameter '{}' in template '{}' is already defined by an ancestor; \
                         provide a value to override it",
                        param_name, template.name
                    ),
                ),
            ));
        }
    }

    diags
}

fn check_instruction_consistent(
    instruction: &ast::Instruction,
    item_cache: &HashMap<&str, &ast::Item>,
    file_name: &str,
    text_only: bool,
) -> Vec<(String, Diag)> {
    let mut diags = vec![];

    // Check parent template exists and is a template.
    if let Some(parent_name) = instruction.parent_template.as_deref() {
        match item_cache.get(parent_name).copied() {
            None => diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "Unknown parent template '{}' for instruction '{}'",
                        parent_name, instruction.name
                    ),
                ),
            )),
            Some(item) if !matches!(item, ast::Item::Template(_)) => diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "Parent '{}' for instruction '{}' must be a template",
                        parent_name, instruction.name
                    ),
                ),
            )),
            _ => {}
        }
    }

    // Check ISAs exist and are ISAs.
    for isa_name in &instruction.for_isas {
        match item_cache.get(isa_name.as_str()).copied() {
            None => {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        instruction.span,
                        format!(
                            "Unknown ISA '{}' in instruction '{}'",
                            isa_name, instruction.name
                        ),
                    ),
                ));
            }
            Some(item) if !matches!(item, ast::Item::Isa(_)) => {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        instruction.span,
                        format!(
                            "'{}' referenced in instruction '{}' is not an ISA",
                            isa_name, instruction.name
                        ),
                    ),
                ));
            }
            _ => {}
        }
    }

    let chain = resolve_template_chain(instruction, item_cache);

    // Build params_cache: root-first insertion means later (closer) definitions win.
    let mut params_cache: HashMap<&str, (Type, Option<ast::Expr>)> = HashMap::new();
    for tmpl in &chain {
        for (name, (ty, value)) in &tmpl.params {
            params_cache.insert(name.as_str(), (ty.clone(), value.clone()));
        }
    }
    for (name, (ty, value)) in &instruction.params {
        params_cache.insert(name.as_str(), (ty.clone(), value.clone()));
    }

    // Build operands_cache from chain + instruction.
    let mut operands_cache: HashMap<&str, Type> = HashMap::new();
    for tmpl in &chain {
        for (name, ty) in &tmpl.operands {
            operands_cache.insert(name.as_str(), ty.clone());
        }
    }
    for (name, ty) in &instruction.operands {
        operands_cache.insert(name.as_str(), ty.clone());
    }

    // `bits<expr>` widths must constant-fold against the ISA parameters.
    let isa_params = resolve_isa_param_values(instruction, item_cache);
    for (name, ty) in &operands_cache {
        if let Type::BitsExpr(expr) = ty
            && eval_bits_width(expr, &isa_params).is_none()
        {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "width of operand '{}' in instruction '{}' does not evaluate to a constant",
                        name, instruction.name
                    ),
                ),
            ));
        }
    }

    for (name, (_ty, value)) in &params_cache {
        if value.is_none() {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "Parameter '{}' in instruction '{}' has no bound value",
                        name, instruction.name
                    ),
                ),
            ));
        }
    }

    if !params_cache.contains_key("OPNAME") && !params_cache.contains_key("MNEMONIC") {
        diags.push((
            file_name.to_string(),
            Rich::custom(
                instruction.span,
                format!(
                    "Instruction '{}' must define OPNAME or MNEMONIC",
                    instruction.name
                ),
            ),
        ));
    }

    // Encoding must exist somewhere in the chain or instruction. Text-only
    // targets (pseudo-ISAs like PTX) have no binary representation, so an empty
    // encoding is allowed there and simply produces no encoder.
    let effective_encoding = resolve_effective_encoding_for_instruction(instruction, item_cache);
    if effective_encoding.is_empty() {
        if !text_only {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!("Instruction '{}' has no encoding defined", instruction.name),
                ),
            ));
        }
    } else {
        diags.extend(check_encoding(
            instruction,
            effective_encoding,
            &params_cache,
            &operands_cache,
            crate::encoding::encoding_unit(&isa_params),
            file_name,
        ));
    }

    // Asm must exist somewhere in the chain or instruction.
    let effective_asm = resolve_effective_asm_for_instruction(instruction, item_cache);
    if let Some(effective_asm) = effective_asm {
        diags.extend(check_asm(
            instruction,
            effective_asm,
            &params_cache,
            file_name,
        ));
    } else {
        diags.push((
            file_name.to_string(),
            Rich::custom(
                instruction.span,
                format!(
                    "Instruction '{}' has no asm block defined",
                    instruction.name
                ),
            ),
        ));
    }

    diags.extend(check_behavior(
        &instruction.name,
        &instruction.behavior,
        item_cache,
        file_name,
    ));

    let reserved: HashSet<String> = operands_cache
        .keys()
        .chain(params_cache.keys())
        .map(|name| name.to_string())
        .chain(
            item_cache
                .iter()
                .filter(|(_, item)| matches!(item, ast::Item::RegisterClass(_)))
                .map(|(name, _)| name.to_string()),
        )
        .collect();
    check_let_bindings(
        &instruction.name,
        &instruction.behavior,
        &reserved,
        file_name,
        &mut diags,
    );

    diags
}

fn check_asm(
    instruction: &ast::Instruction,
    asm_: &ast::Expr,
    _params_cache: &HashMap<&str, (Type, Option<ast::Expr>)>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    // Asm may be wrapped in a block (`asm { "..." }`); unwrap a single-expression block.
    let inner = match asm_ {
        ast::Expr::Block(b) if b.stmts.len() == 1 => &b.stmts[0],
        other => other,
    };
    match inner {
        ast::Expr::Lit(ast::Lit::Str(_)) => vec![],
        _ => vec![(
            file_name.to_string(),
            Rich::custom(
                instruction.span,
                format!(
                    "Asm block must be a single literal string for instruction '{}'",
                    instruction.name
                ),
            ),
        )],
    }
}

/// Validate register paths and exception kinds in a behavior or trap-handler
/// body; `owner` names it in diagnostics.
fn check_behavior(
    owner: &str,
    behavior: &ast::Expr,
    item_cache: &HashMap<&str, &ast::Item>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    fn walk_paths<'a>(expr: &'a ast::Expr, out: &mut Vec<&'a ast::Path>) {
        match expr {
            ast::Expr::Path(p) => out.push(p),
            ast::Expr::Assign(a) => {
                walk_paths(&a.dest, out);
                walk_paths(&a.value, out);
            }
            ast::Expr::Let(l) => {
                if let Some(width) = &l.width {
                    walk_paths(width, out);
                }
                walk_paths(&l.value, out);
            }
            ast::Expr::Binary(b) => {
                walk_paths(&b.lhs, out);
                walk_paths(&b.rhs, out);
            }
            ast::Expr::Block(b) => {
                for stmt in &b.stmts {
                    walk_paths(stmt, out);
                }
            }
            ast::Expr::Call(c) => {
                walk_paths(&c.callee, out);
                for arg in &c.arguments {
                    walk_paths(arg, out);
                }
            }
            ast::Expr::Field(f) => walk_paths(&f.base, out),
            ast::Expr::Unary(u) => walk_paths(&u.x, out),
            ast::Expr::If(i) => {
                walk_paths(&i.cond, out);
                walk_paths(&i.then, out);
                if let Some(e) = &i.else_ {
                    walk_paths(e, out);
                }
            }
            ast::Expr::IndexAccess(i) => walk_paths(&i.base, out),
            ast::Expr::Slice(s) => walk_paths(&s.base, out),
            ast::Expr::Cast(c) => {
                walk_paths(&c.x, out);
                walk_paths(&c.width, out);
            }
            ast::Expr::Try(t) => {
                walk_paths(&t.body, out);
                for handler in &t.handlers {
                    walk_paths(&handler.body, out);
                }
            }
            ast::Expr::Lambda(l) => walk_paths(&l.body, out),
            ast::Expr::Ident(_)
            | ast::Expr::Lit(_)
            | ast::Expr::BuiltinFunction(_)
            | ast::Expr::Invalid => {}
        }
    }

    fn walk_excepts<'a>(expr: &'a ast::Expr, out: &mut Vec<&'a ast::ExceptClause>) {
        match expr {
            ast::Expr::Try(t) => {
                walk_excepts(&t.body, out);
                for handler in &t.handlers {
                    out.push(handler);
                    walk_excepts(&handler.body, out);
                }
            }
            ast::Expr::Block(b) => {
                for stmt in &b.stmts {
                    walk_excepts(stmt, out);
                }
            }
            ast::Expr::If(i) => {
                walk_excepts(&i.then, out);
                if let Some(e) = &i.else_ {
                    walk_excepts(e, out);
                }
            }
            _ => {}
        }
    }

    let mut diags = Vec::new();
    let mut excepts = Vec::new();
    walk_excepts(behavior, &mut excepts);
    for clause in excepts {
        if !ast::EXCEPTION_KINDS.contains(&clause.kind.as_str()) {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    clause.span,
                    format!(
                        "unknown exception kind '{}' in instruction '{}'; known kinds: {}",
                        clause.kind,
                        owner,
                        ast::EXCEPTION_KINDS.join(", ")
                    ),
                ),
            ));
        }
    }

    let mut paths = Vec::new();
    walk_paths(behavior, &mut paths);

    for path in paths {
        // `Ordering::<member>` is a memory-ordering constant, not a register path.
        if path.base == "Ordering" {
            let member_ok =
                path.remainder.len() == 1 && ast::ordering_code(&path.remainder[0]).is_some();
            if !member_ok {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        path.span,
                        format!(
                            "unknown ordering '{}::{}' in instruction '{}'; valid orderings: {}",
                            path.base,
                            path.remainder.join("::"),
                            owner,
                            ast::ORDERING_NAMES.join(", ")
                        ),
                    ),
                ));
            }
            continue;
        }

        let reg_class = match item_cache.get(path.base.as_str()) {
            Some(ast::Item::RegisterClass(rc)) => rc,
            Some(_) | None => {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        path.span,
                        format!(
                            "unknown register class '{}' in behavior for instruction '{}'",
                            path.base, owner
                        ),
                    ),
                ));
                continue;
            }
        };

        if path.remainder.len() != 1 {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    path.span,
                    format!(
                        "path '{}::{}' must have exactly one register component",
                        path.base,
                        path.remainder.join("::")
                    ),
                ),
            ));
            continue;
        }

        let reg_name = &path.remainder[0];
        let exists = reg_class.resolve_registers().any(|r| {
            r.name == *reg_name || r.alias.as_ref().is_some_and(|alias| alias == reg_name)
        });
        if !exists {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    path.span,
                    format!(
                        "unknown register '{}' in path '{}::{}' for instruction '{}'",
                        reg_name, path.base, reg_name, owner
                    ),
                ),
            ));
        }
    }

    check_atomic_structure(owner, behavior, file_name, &mut diags);

    diags
}

/// `let` scoping: a binding is visible only to the statements that follow it in
/// its own block, and its name must be fresh — an operand, parameter, register
/// class or another binding of that name is a redefinition.
fn check_let_bindings(
    owner: &str,
    behavior: &ast::Expr,
    reserved: &HashSet<String>,
    file_name: &str,
    diags: &mut Vec<(String, Diag)>,
) {
    let mut bound = HashSet::new();
    visit_exprs(behavior, &mut |e| {
        if let ast::Expr::Let(l) = e {
            bound.insert(l.name.clone());
        }
    });
    if bound.is_empty() {
        return;
    }

    struct Walker<'a> {
        owner: &'a str,
        bound: HashSet<String>,
        reserved: &'a HashSet<String>,
        file_name: &'a str,
    }

    impl Walker<'_> {
        fn err(&self, diags: &mut Vec<(String, Diag)>, span: Span, message: String) {
            diags.push((self.file_name.to_string(), Rich::custom(span, message)));
        }

        /// Walk `expr` in source order; `scope` holds the bindings visible at
        /// this point, and is truncated back on leaving a nested scope.
        fn walk(&self, expr: &ast::Expr, scope: &mut Vec<String>, diags: &mut Vec<(String, Diag)>) {
            let nested = |walker: &Self, e: &ast::Expr, scope: &mut Vec<String>, diags: &mut _| {
                let depth = scope.len();
                walker.walk(e, scope, diags);
                scope.truncate(depth);
            };
            match expr {
                ast::Expr::Let(l) => {
                    if let Some(width) = &l.width {
                        self.walk(width, scope, diags);
                    }
                    self.walk(&l.value, scope, diags);
                    let owner = self.owner;
                    if scope.contains(&l.name) || self.reserved.contains(&l.name) {
                        self.err(
                            diags,
                            l.span,
                            format!(
                                "binding '{}' redefines an existing name in '{owner}'",
                                l.name
                            ),
                        );
                    }
                    // Bind regardless, so a rejected redefinition does not also
                    // report every later use as undefined.
                    scope.push(l.name.clone());
                }
                ast::Expr::Ident(id) => {
                    if self.bound.contains(&id.name) && !scope.contains(&id.name) {
                        let owner = self.owner;
                        self.err(
                            diags,
                            id.span,
                            format!(
                                "binding '{}' is used before its definition in '{owner}'",
                                id.name
                            ),
                        );
                    }
                }
                ast::Expr::Block(b) => {
                    let depth = scope.len();
                    for stmt in &b.stmts {
                        self.walk(stmt, scope, diags);
                    }
                    scope.truncate(depth);
                }
                ast::Expr::Assign(a) => {
                    self.walk(&a.dest, scope, diags);
                    self.walk(&a.value, scope, diags);
                }
                ast::Expr::Binary(b) => {
                    self.walk(&b.lhs, scope, diags);
                    self.walk(&b.rhs, scope, diags);
                }
                ast::Expr::Unary(u) => self.walk(&u.x, scope, diags),
                ast::Expr::Call(c) => {
                    for argument in &c.arguments {
                        self.walk(argument, scope, diags);
                    }
                }
                ast::Expr::Field(f) => self.walk(&f.base, scope, diags),
                ast::Expr::If(i) => {
                    self.walk(&i.cond, scope, diags);
                    nested(self, &i.then, scope, diags);
                    if let Some(e) = &i.else_ {
                        nested(self, e, scope, diags);
                    }
                }
                ast::Expr::IndexAccess(ix) => self.walk(&ix.base, scope, diags),
                ast::Expr::Slice(s) => self.walk(&s.base, scope, diags),
                ast::Expr::Cast(c) => {
                    self.walk(&c.x, scope, diags);
                    self.walk(&c.width, scope, diags);
                }
                ast::Expr::Try(t) => {
                    nested(self, &t.body, scope, diags);
                    for handler in &t.handlers {
                        nested(self, &handler.body, scope, diags);
                    }
                }
                ast::Expr::Lambda(l) => nested(self, &l.body, scope, diags),
                ast::Expr::Lit(_) | ast::Expr::Path(_) | ast::Expr::BuiltinFunction(_) => {}
                ast::Expr::Invalid => {}
            }
        }
    }

    let walker = Walker {
        owner,
        bound,
        reserved,
        file_name,
    };
    walker.walk(behavior, &mut Vec::new(), diags);
}

/// A `load_reserved`/`store_conditional`/`atomic_rmw` call.
fn is_atomic_call(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::Call(c) if matches!(
        &*c.callee,
        ast::Expr::BuiltinFunction(
            ast::BuiltinFunction::LoadReserved
                | ast::BuiltinFunction::StoreConditional
                | ast::BuiltinFunction::AtomicRmw
        )
    ))
}

/// An atomic operation whose result may be discarded in statement position.
fn is_discardable_atomic_call(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::Call(c) if matches!(
        &*c.callee,
        ast::Expr::BuiltinFunction(
            ast::BuiltinFunction::StoreConditional | ast::BuiltinFunction::AtomicRmw
        )
    ))
}

/// A `fence`/`fence_i` call.
fn is_fence_call(e: &ast::Expr) -> bool {
    matches!(e, ast::Expr::Call(c) if matches!(
        &*c.callee,
        ast::Expr::BuiltinFunction(ast::BuiltinFunction::Fence | ast::BuiltinFunction::FenceI)
    ))
}

/// Visit `e` and every sub-expression.
fn visit_exprs<'a>(e: &'a ast::Expr, f: &mut dyn FnMut(&'a ast::Expr)) {
    f(e);
    match e {
        ast::Expr::Assign(a) => {
            visit_exprs(&a.dest, f);
            visit_exprs(&a.value, f);
        }
        ast::Expr::Let(l) => {
            if let Some(width) = &l.width {
                visit_exprs(width, f);
            }
            visit_exprs(&l.value, f);
        }
        ast::Expr::Binary(b) => {
            visit_exprs(&b.lhs, f);
            visit_exprs(&b.rhs, f);
        }
        ast::Expr::Unary(u) => visit_exprs(&u.x, f),
        ast::Expr::Block(b) => b.stmts.iter().for_each(|s| visit_exprs(s, f)),
        ast::Expr::Call(c) => {
            visit_exprs(&c.callee, f);
            c.arguments.iter().for_each(|a| visit_exprs(a, f));
        }
        ast::Expr::Field(fld) => visit_exprs(&fld.base, f),
        ast::Expr::If(i) => {
            visit_exprs(&i.cond, f);
            visit_exprs(&i.then, f);
            if let Some(e) = &i.else_ {
                visit_exprs(e, f);
            }
        }
        ast::Expr::IndexAccess(ix) => visit_exprs(&ix.base, f),
        ast::Expr::Slice(s) => visit_exprs(&s.base, f),
        ast::Expr::Cast(c) => {
            visit_exprs(&c.x, f);
            visit_exprs(&c.width, f);
        }
        ast::Expr::Try(t) => {
            visit_exprs(&t.body, f);
            t.handlers.iter().for_each(|h| visit_exprs(&h.body, f));
        }
        ast::Expr::Lambda(l) => visit_exprs(&l.body, f),
        ast::Expr::Ident(_)
        | ast::Expr::Lit(_)
        | ast::Expr::Path(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Invalid => {}
    }
}

fn count_matching(e: &ast::Expr, pred: fn(&ast::Expr) -> bool) -> usize {
    let mut n = 0;
    visit_exprs(e, &mut |x| {
        if pred(x) {
            n += 1;
        }
    });
    n
}

/// Enforce the atomics/fence structural rules: at most one atomic per statement,
/// atomics only within an assignment RHS (or a bare `store_conditional`), and
/// `fence`/`fence_i` only in statement position.
fn check_atomic_structure(
    owner: &str,
    stmt: &ast::Expr,
    file_name: &str,
    diags: &mut Vec<(String, Diag)>,
) {
    let mut err = |span: Span, msg: String| {
        diags.push((file_name.to_string(), Rich::custom(span, msg)));
    };
    match stmt {
        ast::Expr::Block(b) => {
            for s in &b.stmts {
                check_atomic_structure(owner, s, file_name, diags);
            }
        }
        ast::Expr::Assign(a) => {
            if count_matching(&a.dest, is_atomic_call) > 0 {
                err(
                    a.span,
                    format!("atomic access is not allowed in an assignment target in '{owner}'"),
                );
            }
            if count_matching(&a.value, is_atomic_call) > 1 {
                err(
                    a.span,
                    format!("at most one atomic access is allowed per statement in '{owner}'"),
                );
            }
            if count_matching(stmt, is_fence_call) > 0 {
                err(
                    a.span,
                    format!("fence is only valid in statement position in '{owner}'"),
                );
            }
        }
        // A binding is an assignment-RHS position: the atomic runs once, at the
        // `let`, and its uses share that single access.
        ast::Expr::Let(l) => {
            if count_matching(&l.value, is_atomic_call) > 1 {
                err(
                    l.span,
                    format!("at most one atomic access is allowed per statement in '{owner}'"),
                );
            }
            if count_matching(stmt, is_fence_call) > 0 {
                err(
                    l.span,
                    format!("fence is only valid in statement position in '{owner}'"),
                );
            }
        }
        // A statement-level `if`/`try` guard: recurse into the bodies, but the
        // condition/body must not hold an atomic or fence in a value position.
        ast::Expr::If(i) => {
            if count_matching(&i.cond, is_atomic_call) > 0
                || count_matching(&i.cond, is_fence_call) > 0
            {
                err(
                    i.span,
                    format!("atomic or fence is not allowed in a condition in '{owner}'"),
                );
            }
            check_atomic_structure(owner, &i.then, file_name, diags);
            if let Some(e) = &i.else_ {
                check_atomic_structure(owner, e, file_name, diags);
            }
        }
        ast::Expr::Try(t) => {
            check_atomic_structure(owner, &t.body, file_name, diags);
            for h in &t.handlers {
                check_atomic_structure(owner, &h.body, file_name, diags);
            }
        }
        // A bare statement may discard the result of a store-conditional or
        // atomic RMW. Loads must feed an assignment; fences are statement-only.
        _ => {
            if is_discardable_atomic_call(stmt) || is_fence_call(stmt) {
                return;
            }
            if count_matching(stmt, is_atomic_call) > 0 {
                err(
                    expr_span(stmt),
                    format!(
                        "atomic access must appear within an assignment right-hand side in '{owner}'"
                    ),
                );
            }
            if count_matching(stmt, is_fence_call) > 0 {
                err(
                    expr_span(stmt),
                    format!("fence is only valid in statement position in '{owner}'"),
                );
            }
        }
    }
}

/// Best-effort span of an arbitrary expression, for diagnostics.
fn expr_span(e: &ast::Expr) -> Span {
    match e {
        ast::Expr::Assign(a) => a.span,
        ast::Expr::Let(l) => l.span,
        ast::Expr::Binary(b) => b.span,
        ast::Expr::Unary(u) => u.span,
        ast::Expr::Block(b) => b.span,
        ast::Expr::Call(c) => c.span,
        ast::Expr::Field(f) => f.span,
        ast::Expr::If(i) => i.span,
        ast::Expr::IndexAccess(ix) => ix.span,
        ast::Expr::Slice(s) => s.span,
        ast::Expr::Cast(c) => c.span,
        ast::Expr::Try(t) => t.span,
        ast::Expr::Path(p) => p.span,
        ast::Expr::Ident(id) => id.span,
        ast::Expr::Lambda(l) => l.span,
        ast::Expr::Lit(ast::Lit::Int(li)) => li.span,
        ast::Expr::Lit(ast::Lit::Str(ls)) => ls.span,
        ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => (0..0).into(),
    }
}

fn check_encoding(
    instruction: &ast::Instruction,
    encoding: &[ast::EncodingField],
    params_cache: &HashMap<&str, (Type, Option<ast::Expr>)>,
    operands_cache: &HashMap<&str, Type>,
    unit: Option<u16>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    let mut diags = vec![];

    let declared_width = |name: &str| -> Option<u16> {
        let ty = params_cache
            .get(name)
            .map(|(ty, _)| ty)
            .or_else(|| operands_cache.get(name))?;
        match ty {
            Type::Bits(width) => Some(*width),
            _ => None,
        }
    };
    let out_of_range = |span: Span, message: String| {
        (
            file_name.to_string(),
            Rich::custom(
                span,
                format!(
                    "{message} in encoding of instruction '{}'",
                    instruction.name
                ),
            ),
        )
    };

    for field in encoding {
        let Some(name) = encoding_value_name(&field.value) else {
            continue;
        };
        let Some(width) = declared_width(name) else {
            continue;
        };
        match &field.value {
            ast::Expr::Slice(slc) if slc.hi >= width => diags.push(out_of_range(
                field.span,
                format!(
                    "slice '{name}[{}..{}]' exceeds bits<{width}>",
                    slc.hi, slc.lo
                ),
            )),
            ast::Expr::IndexAccess(idx) if idx.index >= width => diags.push(out_of_range(
                field.span,
                format!("bit '{name}[{}]' exceeds bits<{width}>", idx.index),
            )),
            _ => {}
        }
    }

    diags.extend(crate::encoding::check_encoding_units(
        encoding,
        unit,
        &instruction.name,
        instruction.span,
        file_name,
    ));

    diags
}
