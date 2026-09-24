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

fn abi_kind_name(kind: ast::AbiValueKind) -> &'static str {
    match kind {
        ast::AbiValueKind::Int => "int",
        ast::AbiValueKind::Float => "float",
        ast::AbiValueKind::Vector => "vector",
    }
}

fn check_abi_arg_overflow(file_name: &str, abi: &ast::Abi, diags: &mut Vec<(String, Diag)>) {
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
                file_name.to_string(),
                Rich::custom(
                    sequence.span,
                    format!(
                        "ABI '{}' {} argument overflow references undeclared {} sequence",
                        abi.name,
                        abi_kind_name(sequence.kind),
                        abi_kind_name(target)
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
                    .map(|kind| abi_kind_name(*kind))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                diags.push((
                    file_name.to_string(),
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
                                    abi_kind_name(pass.kind)
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
                                    abi_kind_name(pass.kind),
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

            check_abi_arg_overflow(&file.file_name, abi, &mut diags);
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
            let file_name = file.file_name.as_str();
            let (resources, modeled) = check_machine_resources(file_name, machine, &mut diags);
            let names = MachineNames {
                resources,
                modeled,
                frontend_decoders: machine
                    .frontend
                    .iter()
                    .flat_map(|frontend| frontend.decode.decoders.iter())
                    .map(|decoder| decoder.name.as_str())
                    .collect(),
                phases: machine.pipeline.iter().map(|p| p.name.as_str()).collect(),
            };
            check_machine_frontend(file_name, machine, &mut diags);
            check_machine_reg_files(file_name, machine, files, &mut diags);
            check_machine_binds(file_name, machine, item_cache, &names, &mut diags);
            check_machine_overrides(file_name, machine, item_cache, &names, &mut diags);
            check_machine_fusions(file_name, machine, files, item_cache, &names, &mut diags);
            check_machine_forwards(file_name, machine, &names, &mut diags);
        }
    }

    diags
}

struct MachineNames<'a> {
    resources: HashSet<&'a str>,
    modeled: HashSet<&'a str>,
    frontend_decoders: HashSet<&'a str>,
    phases: HashSet<&'a str>,
}

fn check_machine_resources<'a>(
    file_name: &str,
    machine: &'a ast::Machine,
    diags: &mut Vec<(String, Diag)>,
) -> (HashSet<&'a str>, HashSet<&'a str>) {
    let mut resource_names: HashSet<&str> = HashSet::new();
    for res in &machine.resources {
        if !resource_names.insert(res.name.as_str()) {
            diags.push((
                file_name.to_string(),
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
                file_name.to_string(),
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
        if resource_group_is_cyclic(group.name.as_str(), &resource_groups, &mut HashSet::new()) {
            diags.push((
                file_name.to_string(),
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
                file_name.to_string(),
                Rich::custom(group.span, "resource occupancy must be positive"),
            ));
        }
        for referenced in resource_references(&group.resources) {
            if !modeled_resource_names.contains(referenced) {
                diags.push((
                    file_name.to_string(),
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
    (resource_names, modeled_resource_names)
}

fn check_machine_frontend(
    file_name: &str,
    machine: &ast::Machine,
    diags: &mut Vec<(String, Diag)>,
) {
    if let Some(frontend) = &machine.frontend {
        for (name, value) in [
            ("bytes_per_cycle", frontend.fetch.bytes_per_cycle),
            ("window_bytes", frontend.fetch.window_bytes),
            ("alignment", frontend.fetch.alignment),
            ("queue_bytes", frontend.fetch.queue_bytes),
        ] {
            if value <= 0 {
                diags.push((
                    file_name.to_string(),
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
                    file_name.to_string(),
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
                    file_name.to_string(),
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
                    file_name.to_string(),
                    Rich::custom(
                        decoder.span,
                        format!("duplicate frontend decoder '{}'", decoder.name),
                    ),
                ));
            }
        }
        if frontend.decode.slots.is_empty() {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    frontend.decode.span,
                    "frontend decode must declare at least one slot",
                ),
            ));
        }
        for slot in &frontend.decode.slots {
            if !decoder_names.contains(slot.as_str()) {
                diags.push((
                    file_name.to_string(),
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
                        file_name.to_string(),
                        Rich::custom(
                            cache.span,
                            format!("frontend decoded_cache {name} must be positive"),
                        ),
                    ));
                }
            }
        }
    }
}

fn check_machine_reg_files(
    file_name: &str,
    machine: &ast::Machine,
    files: &[ast::File],
    diags: &mut Vec<(String, Diag)>,
) {
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
                file_name.to_string(),
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
                file_name.to_string(),
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
}

fn check_machine_binds(
    file_name: &str,
    machine: &ast::Machine,
    item_cache: &HashMap<&str, &ast::Item>,
    names: &MachineNames<'_>,
    diags: &mut Vec<(String, Diag)>,
) {
    let mut bound_units: HashSet<&str> = HashSet::new();
    for bind in &machine.binds {
        if bind.decode_uops.is_some_and(|count| count <= 0) {
            diags.push((
                file_name.to_string(),
                Rich::custom(bind.span, "decode_uops must be positive"),
            ));
        }
        if let Some(message) =
            eliminated_conflict(bind.eliminated, bind.latency, &bind.uses, &bind.uops)
        {
            diags.push((
                file_name.to_string(),
                Rich::custom(bind.span, message.to_string()),
            ));
        }
        if bind.decode_cycles.is_some_and(|cycles| cycles <= 0) {
            diags.push((
                file_name.to_string(),
                Rich::custom(bind.span, "decode_cycles must be positive"),
            ));
        }
        if let Some(decoder) = &bind.decoder {
            if !names.frontend_decoders.contains(decoder.as_str()) {
                diags.push((
                    file_name.to_string(),
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
                    file_name.to_string(),
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
                file_name.to_string(),
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
            if !names.phases.contains(phase.as_str()) {
                diags.push((
                    file_name.to_string(),
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
                file_name.to_string(),
                Rich::custom(
                    bind.span,
                    format!(
                        "'{}' bound in machine '{}' is not a unit",
                        bind.unit, machine.name
                    ),
                ),
            )),
            None => diags.push((
                file_name.to_string(),
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
                file_name.to_string(),
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
            if !names.resources.contains(used.as_str()) {
                diags.push((
                    file_name.to_string(),
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
                    file_name.to_string(),
                    Rich::custom(uop.span, "micro-op count must be positive"),
                ));
            }
            if has_non_positive_occupancy(&uop.resources) {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(uop.span, "resource occupancy must be positive"),
                ));
            }
            for referenced in resource_references(&uop.resources) {
                if !names.modeled.contains(referenced) {
                    diags.push((
                        file_name.to_string(),
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
}

fn check_machine_overrides(
    file_name: &str,
    machine: &ast::Machine,
    item_cache: &HashMap<&str, &ast::Item>,
    names: &MachineNames<'_>,
    diags: &mut Vec<(String, Diag)>,
) {
    // Overrides target a real instruction (at most once), use this
    // machine's resources, and reference real pipeline phases.
    let mut overridden: HashSet<&str> = HashSet::new();
    for ov in &machine.overrides {
        if !ov.latency_cases.is_empty()
            && !ov
                .latency
                .is_some_and(|latency| (1..=65535).contains(&latency))
        {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    ov.span,
                    "conditional latency requires a fallback latency in 1..65535",
                ),
            ));
        }
        if !ov.latency_cases.is_empty() {
            if ov.eliminated == Some(true) {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        ov.span,
                        "conditional latency cannot be used with eliminated = true",
                    ),
                ));
            }
            if ov.zero_idiom == Some(true) {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        ov.span,
                        "conditional latency cannot be used with zero_idiom = true",
                    ),
                ));
            }
            if ov.reads.is_some() || ov.writes.is_some() {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        ov.span,
                        "conditional latency cannot be used with reads or writes phases",
                    ),
                ));
            }
        }
        for case in &ov.latency_cases {
            if !(1..=65535).contains(&case.latency) {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(case.span, "conditional latency case must be in 1..65535"),
                ));
            }
            if let Some(message) = latency_condition_error(&case.condition) {
                diags.push((file_name.to_string(), Rich::custom(case.span, message)));
            }
            check_conditional_uops(file_name, machine, &case.uops, names, diags);
        }
        if ov.decode_uops.is_some_and(|count| count <= 0) {
            diags.push((
                file_name.to_string(),
                Rich::custom(ov.span, "decode_uops must be positive"),
            ));
        }
        if let Some(message) = eliminated_conflict(ov.eliminated, ov.latency, &ov.uses, &ov.uops) {
            diags.push((
                file_name.to_string(),
                Rich::custom(ov.span, message.to_string()),
            ));
        }
        if ov.decode_cycles.is_some_and(|cycles| cycles <= 0) {
            diags.push((
                file_name.to_string(),
                Rich::custom(ov.span, "decode_cycles must be positive"),
            ));
        }
        if let Some(decoder) = &ov.decoder
            && !names.frontend_decoders.contains(decoder.as_str())
        {
            diags.push((
                file_name.to_string(),
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
                file_name.to_string(),
                Rich::custom(
                    ov.span,
                    format!(
                        "override target '{}' in machine '{}' is not an instruction",
                        ov.instruction, machine.name
                    ),
                ),
            )),
            None => diags.push((
                file_name.to_string(),
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
                file_name.to_string(),
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
            if !names.resources.contains(used.as_str()) {
                diags.push((
                    file_name.to_string(),
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
                    file_name.to_string(),
                    Rich::custom(uop.span, "micro-op count must be positive"),
                ));
            }
            if has_non_positive_occupancy(&uop.resources) {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(uop.span, "resource occupancy must be positive"),
                ));
            }
            for referenced in resource_references(&uop.resources) {
                if !names.modeled.contains(referenced) {
                    diags.push((
                        file_name.to_string(),
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
            if !names.phases.contains(phase.as_str()) {
                diags.push((
                    file_name.to_string(),
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
}

fn check_conditional_uops(
    file_name: &str,
    machine: &ast::Machine,
    uops: &[ast::MicroOp],
    names: &MachineNames<'_>,
    diags: &mut Vec<(String, Diag)>,
) {
    for uop in uops {
        if uop.count <= 0 {
            diags.push((
                file_name.to_string(),
                Rich::custom(uop.span, "micro-op count must be positive"),
            ));
        }
        if has_non_positive_occupancy(&uop.resources) {
            diags.push((
                file_name.to_string(),
                Rich::custom(uop.span, "resource occupancy must be positive"),
            ));
        }
        for referenced in resource_references(&uop.resources) {
            if !names.modeled.contains(referenced) {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        uop.span,
                        format!(
                            "conditional micro-op references unknown resource '{}' in machine '{}'",
                            referenced, machine.name
                        ),
                    ),
                ));
            }
        }
    }
}

fn latency_condition_error(expr: &ast::Expr) -> Option<&'static str> {
    use ast::BuiltinFunction::*;
    let mut error = None;
    crate::utils::visit_exprs(expr, &mut |expr| {
        if error.is_some() {
            return;
        }
        match expr {
            ast::Expr::Assign(_) | ast::Expr::Try(_) => {
                error = Some("conditional latency predicate cannot have side effects");
            }
            ast::Expr::Call(call) => {
                let count = call.arguments.len();
                let valid_arity = match call.callee.as_ref() {
                    ast::Expr::BuiltinFunction(Clamp | Extract) => count == 3,
                    ast::Expr::BuiltinFunction(Bitcast | Log2Ceil | Regnum | Width) => count == 1,
                    ast::Expr::BuiltinFunction(SExt | ZExt) => count == 2,
                    ast::Expr::BuiltinFunction(
                        Load | Store | LoadReserved | StoreConditional | AtomicRmw | Fence | FenceI
                        | Trap | Todo,
                    ) => {
                        error = Some(
                            "conditional latency predicate cannot access memory or have side effects",
                        );
                        return;
                    }
                    _ => {
                        error = Some("unsupported builtin in latency predicate");
                        return;
                    }
                };
                if !valid_arity {
                    error = Some("invalid argument count in latency predicate");
                }
            }
            _ => {}
        }
    });
    error
}

fn check_machine_fusions(
    file_name: &str,
    machine: &ast::Machine,
    files: &[ast::File],
    item_cache: &HashMap<&str, &ast::Item>,
    names: &MachineNames<'_>,
    diags: &mut Vec<(String, Diag)>,
) {
    let instructions: Vec<_> = files.iter().flat_map(|f| f.instructions()).collect();
    let pc_classes: HashSet<String> = files
        .iter()
        .flat_map(|file| file.register_classes())
        .filter(|class| class.is_program_counter())
        .map(|class| class.name.clone())
        .collect();
    let mut rule_names = HashSet::new();
    for fusion in &machine.fusions {
        let mut report = |span, message: String| {
            diags.push((file_name.to_string(), Rich::custom(span, message)));
        };
        if !rule_names.insert(&fusion.name) {
            report(
                fusion.span,
                format!("duplicate fusion rule '{}'", fusion.name),
            );
        }
        let mut steps = HashMap::new();
        for (index, step) in fusion.steps.iter().enumerate() {
            if steps.insert(step.name.as_str(), index).is_some() {
                report(step.span, format!("duplicate fusion step '{}'", step.name));
            }
            for name in &step.instruction_names {
                if !instructions.iter().any(|inst| inst.name == *name) {
                    report(
                        step.span,
                        format!("fusion instruction '{}' matches no instruction", name),
                    );
                }
            }
            for mnemonic in &step.mnemonics {
                if !instructions.iter().any(|inst| {
                    resolve_params_for_instruction(inst, item_cache)
                        .get("MNEMONIC")
                        .and_then(|(_, value)| value.as_ref())
                        .and_then(as_string_literal)
                        .is_some_and(|value| value == *mnemonic)
                }) {
                    report(
                        step.span,
                        format!("fusion mnemonic '{}' matches no instruction", mnemonic),
                    );
                }
            }
            for inst in &instructions {
                let params = resolve_params_for_instruction(inst, item_cache);
                let selected = step.instruction_names.contains(&inst.name)
                    || params
                        .get("MNEMONIC")
                        .and_then(|(_, value)| value.as_ref())
                        .and_then(as_string_literal)
                        .is_some_and(|value| step.mnemonics.contains(&value));
                if selected
                    && params
                        .get("OPNAME")
                        .and_then(|(_, value)| value.as_ref())
                        .and_then(as_string_literal)
                        .or_else(|| {
                            params
                                .get("MNEMONIC")
                                .and_then(|(_, value)| value.as_ref())
                                .and_then(as_string_literal)
                        })
                        .is_none()
                {
                    report(
                        step.span,
                        format!("fusion instruction '{}' has no operation name", inst.name),
                    );
                }
            }
        }
        let validate_ref = |reference: &ast::FusionOperandRef| -> Result<Type, String> {
            let Some(step) = fusion.steps.iter().find(|step| step.name == reference.step) else {
                return Err(format!("unknown fusion step '{}'", reference.step));
            };
            let selected: Vec<_> = instructions
                .iter()
                .filter(|inst| {
                    step.instruction_names.contains(&inst.name)
                        || resolve_params_for_instruction(inst, item_cache)
                            .get("MNEMONIC")
                            .and_then(|(_, value)| value.as_ref())
                            .and_then(as_string_literal)
                            .is_some_and(|value| step.mnemonics.contains(&value))
                })
                .collect();
            let mut expected = None;
            for inst in selected {
                let operand = crate::utils::resolve_operands_for_instruction(inst, item_cache)
                    .into_iter()
                    .find(|(name, _)| *name == reference.operand)
                    .ok_or_else(|| {
                        format!(
                            "fusion operand '{}.{}' is absent from instruction '{}'",
                            reference.step, reference.operand, inst.name
                        )
                    })?;
                if let Some(ty) = &expected {
                    if ty != &operand.1 {
                        return Err(format!(
                            "fusion operand '{}.{}' has incompatible types across its instruction set",
                            reference.step, reference.operand
                        ));
                    }
                } else {
                    expected = Some(operand.1);
                }
            }
            expected.ok_or_else(|| {
                format!(
                    "fusion operand '{}.{}' matches no instruction",
                    reference.step, reference.operand
                )
            })
        };
        if let Some(condition) = &fusion.condition
            && let Err(message) = validate_fusion_guard(condition, &steps, &validate_ref)
        {
            report(expr_span(condition), message);
        }
        let assignments = check_fusion_schedule(fusion, names, &steps, &validate_ref, &mut report);
        check_fusion_effect_coverage(
            fusion,
            &instructions,
            item_cache,
            &pc_classes,
            &assignments,
            &mut report,
        );
    }
}

fn check_fusion_schedule<'a>(
    fusion: &'a ast::FusionDecl,
    names: &MachineNames<'_>,
    steps: &HashMap<&str, usize>,
    validate_ref: &impl Fn(&ast::FusionOperandRef) -> Result<Type, String>,
    report: &mut impl FnMut(Span, String),
) -> FusionAssignments<'a> {
    let schedule = &fusion.schedule;
    check_fusion_stage_costs(fusion, names, report);
    if schedule.uops.is_empty() {
        report(
            schedule.span,
            "fusion schedule needs at least one uop".to_string(),
        );
    }
    let mut uop_names = HashSet::new();
    let mut all_inputs = HashSet::new();
    let mut all_outputs = HashSet::new();
    let mut named_inputs = HashSet::new();
    let mut named_outputs = HashSet::new();
    let mut memory: HashSet<(&String, Option<i64>)> = HashSet::new();
    let mut control = HashSet::new();
    for uop in &schedule.uops {
        if !uop_names.insert(uop.name.as_str()) {
            report(uop.span, format!("duplicate fusion uop '{}'", uop.name));
        }
        for dependency in &uop.depends_on {
            if !uop_names.contains(dependency.as_str()) || dependency == &uop.name {
                report(
                    uop.span,
                    format!(
                        "fusion uop '{}' depends on unknown or later uop '{dependency}'",
                        uop.name
                    ),
                );
            }
        }
        check_fusion_uop_resources(uop, names, steps, report);
        for selector in &uop.inputs {
            match selector {
                ast::FusionOperandSelector::Operand(reference) => {
                    if let Err(message) = validate_ref(reference) {
                        report(reference.span, message);
                    }
                    named_inputs.insert((reference.step.as_str(), reference.operand.as_str()));
                }
                ast::FusionOperandSelector::AllInputs(step) => {
                    if !steps.contains_key(step.as_str()) {
                        report(uop.span, format!("unknown fusion step '{step}'"));
                    }
                    all_inputs.insert(step.as_str());
                }
                ast::FusionOperandSelector::AllOutputs(_) => {
                    report(uop.span, "all_outputs is invalid in inputs".to_string())
                }
            }
        }
        for selector in &uop.outputs {
            match selector {
                ast::FusionOperandSelector::Operand(reference) => {
                    if let Err(message) = validate_ref(reference) {
                        report(reference.span, message);
                    }
                    if !named_outputs.insert((reference.step.as_str(), reference.operand.as_str()))
                    {
                        report(
                            reference.span,
                            format!(
                                "fusion output '{}.{}' is assigned more than once",
                                reference.step, reference.operand
                            ),
                        );
                    }
                }
                ast::FusionOperandSelector::AllOutputs(step) => {
                    if !steps.contains_key(step.as_str()) {
                        report(uop.span, format!("unknown fusion step '{step}'"));
                    }
                    if !all_outputs.insert(step.as_str()) {
                        report(
                            uop.span,
                            format!("fusion outputs of step '{step}' are assigned more than once"),
                        );
                    }
                }
                ast::FusionOperandSelector::AllInputs(_) => {
                    report(uop.span, "all_inputs is invalid in outputs".to_string())
                }
            }
        }
        for reference in &uop.memory {
            if !steps.contains_key(reference.step.as_str())
                || reference.index.is_some_and(|index| index < 0)
            {
                report(
                    reference.span,
                    format!(
                        "invalid fusion memory reference '{}.memory'",
                        reference.step
                    ),
                );
            }
            let overlaps = memory.iter().any(|(step, index)| {
                *step == &reference.step
                    && (reference.index.is_none() || index.is_none() || *index == reference.index)
            });
            if overlaps || !memory.insert((&reference.step, reference.index)) {
                report(
                    reference.span,
                    format!(
                        "fusion memory reference '{}' is assigned more than once",
                        reference.step
                    ),
                );
            }
        }
        for step in &uop.control_steps {
            if !steps.contains_key(step.as_str()) {
                report(uop.span, format!("unknown fusion control step '{step}'"));
            }
            if !control.insert(step.as_str()) {
                report(
                    uop.span,
                    format!("fusion control step '{step}' is assigned more than once"),
                );
            }
        }
    }
    for (step, _) in &named_outputs {
        if all_outputs.contains(step) {
            report(
                schedule.span,
                format!(
                    "fusion outputs of step '{step}' have both named and whole-step assignments"
                ),
            );
        }
    }
    FusionAssignments {
        all_inputs,
        all_outputs,
        named_inputs,
        named_outputs,
        memory,
        control,
    }
}

fn check_fusion_uop_resources(
    uop: &ast::FusionMicroOp,
    names: &MachineNames<'_>,
    steps: &HashMap<&str, usize>,
    report: &mut impl FnMut(Span, String),
) {
    if let Some(inherit) = &uop.inherit_routes
        && !steps.contains_key(inherit.as_str())
    {
        report(uop.span, format!("unknown fusion route source '{inherit}'"));
    }
    if let Some(resources) = &uop.resources {
        if has_non_positive_occupancy(resources) {
            report(
                uop.span,
                "fusion resource occupancy must be positive".to_string(),
            );
        }
        for resource in resource_references(resources) {
            if !names.modeled.contains(resource) {
                report(
                    uop.span,
                    format!("fusion uop references unknown resource '{resource}'"),
                );
            }
        }
    }
    if uop.read_cycle < 0 || uop.write_cycle < uop.read_cycle || uop.write_cycle > u16::MAX as i64 {
        report(
            uop.span,
            "fusion uop cycles must be ordered and in 0..=65535".to_string(),
        );
    }
}

struct FusionAssignments<'a> {
    all_inputs: HashSet<&'a str>,
    all_outputs: HashSet<&'a str>,
    named_inputs: HashSet<(&'a str, &'a str)>,
    named_outputs: HashSet<(&'a str, &'a str)>,
    memory: HashSet<(&'a String, Option<i64>)>,
    control: HashSet<&'a str>,
}

fn check_fusion_effect_coverage<'a>(
    fusion: &ast::FusionDecl,
    instructions: &[&'a ast::Instruction],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    pc_classes: &HashSet<String>,
    assignments: &FusionAssignments<'_>,
    report: &mut impl FnMut(Span, String),
) {
    let schedule = &fusion.schedule;
    let FusionAssignments {
        all_inputs,
        all_outputs,
        named_inputs,
        named_outputs,
        memory,
        control,
    } = assignments;
    for step in &fusion.steps {
        let selected = instructions.iter().filter(|inst| {
            step.instruction_names.contains(&inst.name)
                || resolve_params_for_instruction(inst, item_cache)
                    .get("MNEMONIC")
                    .and_then(|(_, value)| value.as_ref())
                    .and_then(as_string_literal)
                    .is_some_and(|value| step.mnemonics.contains(&value))
        });
        for inst in selected {
            let operands = crate::utils::resolve_operands_for_instruction(inst, item_cache);
            let effects = fusion_required_effects(inst, &operands, pc_classes);
            for (mapped_step, index) in memory {
                if *mapped_step == &step.name
                    && index
                        .is_some_and(|index| index < 0 || index as usize >= effects.memory_count)
                {
                    report(
                        schedule.span,
                        format!(
                            "fusion memory index for step '{}' exceeds effects of '{}'",
                            step.name, inst.name
                        ),
                    );
                }
            }
            if effects.reads.iter().any(|name| {
                !all_inputs.contains(step.name.as_str())
                    && !named_inputs.contains(&(step.name.as_str(), name.as_str()))
            }) || effects.implicit_reads && !all_inputs.contains(step.name.as_str())
            {
                report(
                    schedule.span,
                    format!(
                        "fusion step '{}' leaves an input of '{}' unassigned",
                        step.name, inst.name
                    ),
                );
            }
            if effects.writes.iter().any(|name| {
                !all_outputs.contains(step.name.as_str())
                    && !named_outputs.contains(&(step.name.as_str(), name.as_str()))
            }) || effects.implicit_writes && !all_outputs.contains(step.name.as_str())
            {
                report(
                    schedule.span,
                    format!(
                        "fusion step '{}' leaves an output of '{}' unassigned",
                        step.name, inst.name
                    ),
                );
            }
            if effects.memory_count > 0
                && !memory.contains(&(&step.name, None))
                && (0..effects.memory_count)
                    .any(|index| !memory.contains(&(&step.name, Some(index as i64))))
            {
                report(
                    schedule.span,
                    format!(
                        "fusion step '{}' leaves memory effects of '{}' unassigned",
                        step.name, inst.name
                    ),
                );
            }
            if effects.control && !control.contains(step.name.as_str()) {
                report(
                    schedule.span,
                    format!(
                        "fusion step '{}' leaves control effect of '{}' unassigned",
                        step.name, inst.name
                    ),
                );
            }
            if effects.unknown
                && (!all_inputs.contains(step.name.as_str())
                    || !all_outputs.contains(step.name.as_str())
                    || !memory.contains(&(&step.name, None))
                    || !control.contains(step.name.as_str()))
            {
                report(
                    schedule.span,
                    format!(
                        "fusion step '{}' has unknown behavior and needs whole-step effect selectors",
                        step.name
                    ),
                );
            }
        }
    }
}

fn check_fusion_stage_costs(
    fusion: &ast::FusionDecl,
    names: &MachineNames<'_>,
    report: &mut impl FnMut(Span, String),
) {
    let schedule = &fusion.schedule;
    for (label, count) in [
        ("decode_uops", schedule.decode_uops),
        ("decode_cycles", schedule.decode_cycles),
        ("rename_slots", schedule.rename_slots),
        ("rob_entries", schedule.rob_entries),
        ("retire_slots", schedule.retire_slots),
    ] {
        if !(1..=u16::MAX as i64).contains(&count) {
            report(
                schedule.span,
                format!("fusion {label} must be in 1..=65535"),
            );
        }
    }
    if schedule
        .decoded_cache_uops
        .is_some_and(|count| !(1..=u16::MAX as i64).contains(&count))
    {
        report(
            schedule.span,
            "fusion decoded_cache_uops must be in 1..=65535".to_string(),
        );
    }
    if let Some(decoder) = &schedule.decoder
        && !names.frontend_decoders.contains(decoder.as_str())
    {
        report(
            schedule.span,
            format!("fusion references unknown decoder '{decoder}'"),
        );
    }
    for (label, groups) in [
        ("decode", &schedule.decode_groups),
        ("rename", &schedule.rename_groups),
        ("rob", &schedule.rob_groups),
        ("retire", &schedule.retire_groups),
    ] {
        if !groups.is_empty() {
            let mut position = 0;
            for group in groups {
                if !(1..=u16::MAX as i64).contains(&group.slots) || group.steps.is_empty() {
                    report(
                        group.span,
                        format!("fusion {label} group needs positive slots and steps"),
                    );
                }
                for step in &group.steps {
                    if fusion
                        .steps
                        .get(position)
                        .is_none_or(|expected| expected.name != *step)
                    {
                        report(
                            group.span,
                            format!("fusion {label} groups must partition steps in order"),
                        );
                    }
                    position += 1;
                }
            }
            if position != fusion.steps.len() {
                report(
                    schedule.span,
                    format!("fusion {label} groups must cover every step"),
                );
            }
        }
    }
}

struct FusionRequiredEffects {
    reads: HashSet<String>,
    writes: HashSet<String>,
    implicit_reads: bool,
    implicit_writes: bool,
    memory_count: usize,
    control: bool,
    unknown: bool,
}

fn fusion_required_effects(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    pc_classes: &HashSet<String>,
) -> FusionRequiredEffects {
    let reads = crate::rustgen::infer_read_register_operands(&inst.behavior, operands);
    let writes = crate::rustgen::infer_defined_register_operands(&inst.behavior, operands)
        .into_iter()
        .collect();
    let mut implicit_reads = HashSet::new();
    crate::rustgen::collect_register_path_reads(&inst.behavior, &mut implicit_reads);
    let mut implicit_writes = Vec::new();
    crate::rustgen::collect_register_path_writes(&inst.behavior, &mut implicit_writes);
    let control = implicit_writes
        .iter()
        .any(|((class, _), _)| pc_classes.contains(class));
    let mut memory_count = 0;
    crate::utils::visit_exprs(&inst.behavior, &mut |expr| {
        if let ast::Expr::Call(call) = expr
            && matches!(
                call.callee.as_ref(),
                ast::Expr::BuiltinFunction(
                    ast::BuiltinFunction::Load
                        | ast::BuiltinFunction::Store
                        | ast::BuiltinFunction::LoadReserved
                        | ast::BuiltinFunction::StoreConditional
                        | ast::BuiltinFunction::AtomicRmw
                )
            )
        {
            memory_count += 1;
        }
    });
    FusionRequiredEffects {
        reads,
        writes,
        implicit_reads: implicit_reads
            .iter()
            .any(|(class, _)| !pc_classes.contains(class)),
        implicit_writes: implicit_writes
            .iter()
            .any(|((class, _), _)| !pc_classes.contains(class)),
        memory_count,
        control,
        unknown: crate::utils::behavior_uses_todo(&inst.behavior),
    }
}

fn validate_fusion_guard(
    expr: &ast::Expr,
    steps: &HashMap<&str, usize>,
    reference: &impl Fn(&ast::FusionOperandRef) -> Result<Type, String>,
) -> Result<(), String> {
    match expr {
        ast::Expr::Block(block) if block.last_expr_return && block.stmts.len() == 1 => {
            validate_fusion_guard(&block.stmts[0], steps, reference)
        }
        ast::Expr::Ident(id) if id.name == "true" || id.name == "false" => Ok(()),
        ast::Expr::Binary(binary)
            if matches!(binary.op, ast::BinOp::BitwiseAnd | ast::BinOp::BitwiseOr) =>
        {
            validate_fusion_guard(&binary.lhs, steps, reference)?;
            validate_fusion_guard(&binary.rhs, steps, reference)
        }
        ast::Expr::Unary(unary) if unary.op == ast::UnOp::BitwiseNot => {
            validate_fusion_guard(&unary.x, steps, reference)
        }
        ast::Expr::Binary(binary)
            if matches!(
                binary.op,
                ast::BinOp::Equal
                    | ast::BinOp::NotEqual
                    | ast::BinOp::LessThan
                    | ast::BinOp::LessThenEqual
                    | ast::BinOp::GreaterThan
                    | ast::BinOp::GreaterThanEqual
            ) =>
        {
            if validate_fusion_value(&binary.lhs, steps, reference)? != FusionValueKind::Integer
                || validate_fusion_value(&binary.rhs, steps, reference)? != FusionValueKind::Integer
            {
                return Err("fusion comparison requires numeric values".to_string());
            }
            Ok(())
        }
        ast::Expr::Call(call) => {
            let ast::Expr::Ident(callee) = call.callee.as_ref() else {
                return Err("unsupported fusion guard call".to_string());
            };
            match (callee.name.as_str(), call.arguments.as_slice()) {
                ("same_register" | "overlap_register", [a, b]) => {
                    if validate_fusion_value(a, steps, reference)? != FusionValueKind::Register
                        || validate_fusion_value(b, steps, reference)? != FusionValueKind::Register
                    {
                        return Err(format!("{} requires register operands", callee.name));
                    }
                    Ok(())
                }
                ("same_block", [a, b, bytes]) => {
                    validate_fusion_step(a, steps)?;
                    validate_fusion_step(b, steps)?;
                    validate_fusion_positive_literal(bytes)
                }
                ("aligned", [step, bytes]) => {
                    validate_fusion_step(step, steps)?;
                    validate_fusion_positive_literal(bytes)
                }
                _ => Err(format!("unsupported fusion guard call '{}'", callee.name)),
            }
        }
        _ => Err("unsupported fusion guard expression".to_string()),
    }
}

fn validate_fusion_step(expr: &ast::Expr, steps: &HashMap<&str, usize>) -> Result<(), String> {
    match expr {
        ast::Expr::Ident(id) if steps.contains_key(id.name.as_str()) => Ok(()),
        _ => Err("fusion layout guard requires a named pattern step".to_string()),
    }
}

fn validate_fusion_positive_literal(expr: &ast::Expr) -> Result<(), String> {
    match expr {
        ast::Expr::Lit(ast::Lit::Int(value))
            if fusion_literal_i128(value)
                .is_some_and(|value| value > 0 && value <= u64::MAX as i128) =>
        {
            Ok(())
        }
        _ => Err("fusion layout byte count must be a positive integer literal".to_string()),
    }
}

fn fusion_literal_i128(value: &ast::LitInt) -> Option<i128> {
    let spelling = value.value();
    let (radix, digits) = if let Some(digits) = spelling
        .strip_prefix("0x")
        .or_else(|| spelling.strip_prefix("0X"))
    {
        (16, digits)
    } else if let Some(digits) = spelling
        .strip_prefix("0b")
        .or_else(|| spelling.strip_prefix("0B"))
    {
        (2, digits)
    } else {
        (10, spelling)
    };
    i128::from_str_radix(digits, radix).ok()
}

fn validate_fusion_value(
    expr: &ast::Expr,
    steps: &HashMap<&str, usize>,
    reference: &impl Fn(&ast::FusionOperandRef) -> Result<Type, String>,
) -> Result<FusionValueKind, String> {
    match expr {
        ast::Expr::Block(block) if block.last_expr_return && block.stmts.len() == 1 => {
            validate_fusion_value(&block.stmts[0], steps, reference)
        }
        ast::Expr::Lit(ast::Lit::Int(_)) => Ok(FusionValueKind::Integer),
        ast::Expr::Field(field) => {
            let ast::Expr::Ident(step) = field.base.as_ref() else {
                return Err("fusion operand must be step.operand".to_string());
            };
            let ty = reference(&ast::FusionOperandRef {
                step: step.name.clone(),
                operand: field.member.clone(),
                span: field.span,
            })?;
            match ty {
                Type::Struct(_) => Ok(FusionValueKind::Register),
                Type::Bits(_) | Type::BitsExpr(_) | Type::Integer => Ok(FusionValueKind::Integer),
                _ => Err("fusion guard operand has unsupported type".to_string()),
            }
        }
        ast::Expr::Call(call) => {
            let name = match call.callee.as_ref() {
                ast::Expr::Ident(id) => id.name.as_str(),
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Width) => "width",
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Regnum) => "regnum",
                _ => return Err("unsupported fusion value call".to_string()),
            };
            match (name, call.arguments.as_slice()) {
                ("pc" | "width", [step]) => {
                    validate_fusion_step(step, steps)?;
                    Ok(FusionValueKind::Integer)
                }
                ("regnum" | "operand_width", [operand]) => {
                    let ast::Expr::Field(field) = operand else {
                        return Err(format!("fusion {name} requires step.operand"));
                    };
                    let ast::Expr::Ident(step) = field.base.as_ref() else {
                        return Err(format!("fusion {name} requires step.operand"));
                    };
                    let ty = reference(&ast::FusionOperandRef {
                        step: step.name.clone(),
                        operand: field.member.clone(),
                        span: field.span,
                    })?;
                    if name == "regnum" && !matches!(ty, Type::Struct(_)) {
                        return Err("fusion regnum requires a register operand".to_string());
                    }
                    Ok(FusionValueKind::Integer)
                }
                ("encoded_byte", [step, index]) => {
                    validate_fusion_step(step, steps)?;
                    match index {
                        ast::Expr::Lit(ast::Lit::Int(value))
                            if fusion_literal_i128(value)
                                .is_some_and(|value| usize::try_from(value).is_ok()) =>
                        {
                            Ok(FusionValueKind::Integer)
                        }
                        _ => Err(
                            "fusion encoded_byte index must be a nonnegative integer literal"
                                .to_string(),
                        ),
                    }
                }
                _ => Err(format!("unsupported fusion value call '{name}'")),
            }
        }
        ast::Expr::Binary(binary)
            if matches!(
                binary.op,
                ast::BinOp::Add | ast::BinOp::Sub | ast::BinOp::Mul | ast::BinOp::Div
            ) =>
        {
            if validate_fusion_value(&binary.lhs, steps, reference)? != FusionValueKind::Integer
                || validate_fusion_value(&binary.rhs, steps, reference)? != FusionValueKind::Integer
            {
                return Err("fusion arithmetic requires numeric values".to_string());
            }
            Ok(FusionValueKind::Integer)
        }
        _ => Err("unsupported fusion value expression".to_string()),
    }
}

#[derive(PartialEq, Eq)]
enum FusionValueKind {
    Register,
    Integer,
}

fn check_machine_forwards(
    file_name: &str,
    machine: &ast::Machine,
    names: &MachineNames<'_>,
    diags: &mut Vec<(String, Diag)>,
) {
    // Forwards run between this machine's resources, each pair at most once.
    let mut fwd_pairs: HashSet<(&str, &str)> = HashSet::new();
    for fw in &machine.forwards {
        for (which, res) in [("source", &fw.from), ("target", &fw.to)] {
            if !names.resources.contains(res.as_str()) {
                diags.push((
                    file_name.to_string(),
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
                file_name.to_string(),
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

pub(crate) fn build_item_cache(files: &[ast::File]) -> HashMap<&str, &ast::Item> {
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
        ast::Expr::Cast(cast) => match &*cast.x {
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

    // Operands in declaration order, each name resolving to its closest
    // declaration: the instruction's overrides the template's.
    let mut operands: Vec<&ast::Operand> = Vec::new();
    for operand in chain
        .iter()
        .flat_map(|t| &t.operands)
        .chain(&instruction.operands)
    {
        match operands.iter_mut().find(|held| held.name == operand.name) {
            Some(held) => *held = operand,
            None => operands.push(operand),
        }
    }

    // `bits<expr>` widths must constant-fold against the ISA parameters.
    let isa_params = resolve_isa_param_values(instruction, item_cache);
    diags.extend(check_operand_constraints(
        instruction,
        &operands,
        &isa_params,
        file_name,
    ));
    for operand in &operands {
        let name = &operand.name;
        if let Type::BitsExpr(expr) = &operand.ty
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
    match resolve_effective_encoding_for_instruction(instruction, item_cache) {
        Some(encoding) => diags.extend(check_encoding(
            instruction,
            encoding,
            &params_cache,
            &operands,
            &isa_params,
            item_cache,
            file_name,
        )),
        None if !text_only => diags.push((
            file_name.to_string(),
            Rich::custom(
                instruction.span,
                format!("Instruction '{}' has no encoding defined", instruction.name),
            ),
        )),
        None => {}
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

    let reserved: HashSet<String> = operands
        .iter()
        .map(|operand| operand.name.clone())
        .chain(params_cache.keys().map(|name| name.to_string()))
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
    if crate::rustgen::resolve_asm_templates(asm_).is_some() {
        vec![]
    } else {
        vec![(
            file_name.to_string(),
            Rich::custom(
                instruction.span,
                format!(
                    "Asm block must be a literal string or a nonempty tuple of literal strings for instruction '{}'",
                    instruction.name
                ),
            ),
        )]
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
            | ast::Expr::Tuple(_)
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
    crate::utils::visit_exprs(behavior, &mut |node| {
        if let ast::Expr::Tuple(tuple) = node {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    tuple.span,
                    format!(
                        "concatenation in the behavior of '{owner}'; a concatenation \
                         spells the bits of an encoding, not a value"
                    ),
                ),
            ));
        }
    });

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
    let mut uses_flags = false;
    crate::utils::visit_exprs(behavior, &mut |e| {
        if let ast::Expr::Let(l) = e {
            bound.insert(l.name.clone());
        }
        uses_flags |= matches!(e, ast::Expr::BuiltinFunction(ast::BuiltinFunction::FPFlags));
    });
    if bound.is_empty() && !uses_flags {
        return;
    }

    struct Walker<'a> {
        owner: &'a str,
        bound: HashSet<String>,
        reserved: &'a HashSet<String>,
        file_name: &'a str,
    }

    impl Walker<'_> {
        fn rounded(expr: &ast::Expr, scope: &[(String, bool)]) -> bool {
            match expr {
                ast::Expr::Call(call) => crate::typeck::float_call_rounding(call) == Ok(true),
                ast::Expr::Ident(id) => scope
                    .iter()
                    .rev()
                    .find(|(name, _)| name == &id.name)
                    .is_some_and(|(_, rounded)| *rounded),
                ast::Expr::Block(block) if block.last_expr_return => {
                    let mut nested = scope.to_vec();
                    for stmt in &block.stmts {
                        if let ast::Expr::Let(binding) = stmt {
                            nested.push((
                                binding.name.clone(),
                                Self::rounded(&binding.value, &nested),
                            ));
                        }
                    }
                    block
                        .stmts
                        .last()
                        .is_some_and(|expr| Self::rounded(expr, &nested))
                }
                _ => false,
            }
        }

        fn err(&self, diags: &mut Vec<(String, Diag)>, span: Span, message: String) {
            diags.push((self.file_name.to_string(), Rich::custom(span, message)));
        }

        /// Walk `expr` in source order; `scope` holds the bindings visible at
        /// this point, and is truncated back on leaving a nested scope.
        fn walk(
            &self,
            expr: &ast::Expr,
            scope: &mut Vec<(String, bool)>,
            diags: &mut Vec<(String, Diag)>,
        ) {
            let nested =
                |walker: &Self, e: &ast::Expr, scope: &mut Vec<(String, bool)>, diags: &mut _| {
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
                    if scope.iter().any(|(name, _)| name == &l.name)
                        || self.reserved.contains(&l.name)
                    {
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
                    scope.push((l.name.clone(), Self::rounded(&l.value, scope)));
                }
                ast::Expr::Ident(id) => {
                    if self.bound.contains(&id.name)
                        && !scope.iter().any(|(name, _)| name == &id.name)
                    {
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
                    if matches!(
                        &*c.callee,
                        ast::Expr::BuiltinFunction(ast::BuiltinFunction::FPFlags)
                    ) && c.arguments.len() == 1
                        && !Self::rounded(&c.arguments[0], scope)
                    {
                        self.err(
                            diags,
                            c.span,
                            "fp_flags requires an explicit-rounding operation".to_string(),
                        );
                    }
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
                ast::Expr::Tuple(tuple) => {
                    for element in &tuple.elements {
                        self.walk(element, scope, diags);
                    }
                }
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

fn count_matching(e: &ast::Expr, pred: fn(&ast::Expr) -> bool) -> usize {
    let mut n = 0;
    crate::utils::visit_exprs(e, &mut |x| {
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
pub(crate) fn expr_span(e: &ast::Expr) -> Span {
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
        ast::Expr::Tuple(t) => t.span,
        ast::Expr::Lit(ast::Lit::Int(li)) => li.span,
        ast::Expr::Lit(ast::Lit::Str(ls)) => ls.span,
        ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => (0..0).into(),
    }
}

/// Check an instruction's encoding expression and every shape it expands to.
/// Bit ranges are checked once on the expression, since no condition moves
/// them; each shape is then checked as the fixed bit map it is.
fn check_encoding(
    instruction: &ast::Instruction,
    encoding: &ast::Expr,
    params_cache: &HashMap<&str, (Type, Option<ast::Expr>)>,
    operands: &[&ast::Operand],
    isa_params: &HashMap<String, i64>,
    item_cache: &HashMap<&str, &ast::Item>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    let mut diags = vec![];
    let unit = crate::encoding::encoding_unit(isa_params);
    let ctx = crate::utils::encoding_context(instruction, item_cache);
    let (shapes, errors) = crate::shapes::expand(encoding, &ctx);
    diags.extend(
        errors
            .into_iter()
            .map(|(span, message)| (file_name.to_string(), Rich::custom(span, message))),
    );

    let declared_width = |name: &str| -> Option<u16> {
        let ty = params_cache.get(name).map(|(ty, _)| ty).or_else(|| {
            operands
                .iter()
                .find(|operand| operand.name == name)
                .map(|operand| &operand.ty)
        })?;
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

    crate::utils::visit_exprs(encoding, &mut |node| {
        let Some(name) = encoding_value_name(node) else {
            return;
        };
        let Some(width) = declared_width(name) else {
            return;
        };
        match node {
            ast::Expr::Slice(slc) if slc.hi >= width => diags.push(out_of_range(
                slc.span,
                format!(
                    "slice '{name}[{}..{}]' exceeds bits<{width}>",
                    slc.hi, slc.lo
                ),
            )),
            ast::Expr::IndexAccess(idx) if idx.index >= width => diags.push(out_of_range(
                idx.span,
                format!("bit '{name}[{}]' exceeds bits<{width}>", idx.index),
            )),
            _ => {}
        }
    });

    diags.extend(check_shapes_distinguishable(
        instruction,
        &shapes,
        params_cache,
        unit,
        file_name,
    ));
    diags.extend(check_pc_single_shape(
        instruction,
        &shapes,
        item_cache,
        file_name,
    ));

    // One mistake in an encoding is reached once per shape.
    let mut seen = HashSet::new();
    for shape in &shapes {
        diags.extend(
            check_shape(
                instruction,
                shape,
                &ctx,
                operands,
                isa_params,
                unit,
                file_name,
            )
            .into_iter()
            .filter(|(_, diag)| seen.insert((*diag.span(), diag.to_string()))),
        );
    }

    diags
}

/// Bit-level checks of one fixed bit map: which operand bits it drops, and
/// whether it fills whole encoding units.
fn check_shape(
    instruction: &ast::Instruction,
    shape: &crate::shapes::Shape,
    ctx: &crate::shapes::Context,
    operands: &[&ast::Operand],
    isa_params: &HashMap<String, i64>,
    unit: Option<u16>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    let encoding = &shape.fields;
    let mut diags = vec![];
    for operand in operands {
        let width = match &operand.ty {
            Type::Bits(width) => *width,
            Type::BitsExpr(expr) => match eval_bits_width(expr, isa_params) {
                Some(width) => width,
                None => continue,
            },
            _ => continue,
        };
        let Some(covered) = covered_bits(encoding, &operand.name, width) else {
            continue;
        };
        let dropped = covered.iter().take_while(|bit| !**bit).count() as u32;

        // A gap between spelled bits is truncation nothing can justify.
        let top = covered.iter().rposition(|bit| *bit).unwrap_or_default();
        let skipped: Vec<String> = (dropped as usize..top)
            .filter(|bit| !covered[*bit])
            .map(|bit| bit.to_string())
            .collect();
        if !skipped.is_empty() {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "encoding of instruction '{}' skips bits {} of operand '{}'; \
                         only low bits may be dropped, and only with #[align]",
                        instruction.name,
                        skipped.join(", "),
                        operand.name
                    ),
                ),
            ));
        }

        // Dropping the high bits is sound exactly when the guard that selects
        // this shape has already asked whether the operand survives the narrow
        // field, which is what makes a short immediate form the same
        // instruction as the long one.
        let spelled = top as u16 + 1;
        if spelled < width && !guard_proves_fit(&shape.guard, ctx, &operand.name, spelled) {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "encoding of instruction '{}' spells only bits {top}..{dropped} of \
                         operand '{}'; a shape that drops the high bits needs a condition \
                         proving they are an extension, e.g. sext({} as bits<{spelled}>, \
                         {width}) == {}",
                        instruction.name, operand.name, operand.name, operand.name
                    ),
                ),
            ));
        }

        // Dropping the low `dropped` bits is sound exactly when the operand is
        // declared a multiple of `2^dropped`. An alignment is a `u32`, so a
        // wider drop can never be justified.
        let justified = if dropped < u32::BITS {
            format!("{}", 1u32 << dropped)
        } else {
            format!("2^{dropped}")
        };
        let bits = if dropped == 1 { "bit" } else { "bits" };
        match operand.align {
            // Already reported by `check_operand_constraints`.
            Some(align) if !align.is_power_of_two() => {}
            Some(align) if align.trailing_zeros() > dropped => diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "alignment {align} of operand '{}' in '{}' exceeds the {justified} \
                         justified by the {dropped} low {bits} its encoding drops",
                        operand.name, instruction.name
                    ),
                ),
            )),
            Some(align) if align.trailing_zeros() == dropped => {}
            _ if dropped > 0 => diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "encoding of instruction '{}' drops the low {dropped} {bits} of \
                         operand '{}'; declare #[align({justified})] on it",
                        instruction.name, operand.name
                    ),
                ),
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

/// Decoding stays a function of the instruction word: two shapes of one
/// instruction must differ in width or in some bit both of them fix.
fn check_shapes_distinguishable(
    instruction: &ast::Instruction,
    shapes: &[crate::shapes::Shape],
    params_cache: &HashMap<&str, (Type, Option<ast::Expr>)>,
    unit: Option<u16>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    // A shape whose fields do not lay out has no bit map to compare; the layout
    // itself is already reported.
    let maps: Vec<(u16, HashMap<u16, bool>)> = shapes
        .iter()
        .filter_map(|shape| {
            let width = shape.fields.iter().map(|field| field.width).sum();
            let arms = crate::encoding::encoding_arms(&shape.fields, unit)?;
            Some((width, fixed_bits(&arms, params_cache)))
        })
        .collect();

    let mut diags = vec![];
    for (index, (width, bits)) in maps.iter().enumerate() {
        for (other_width, other_bits) in &maps[index + 1..] {
            let distinguishable = width != other_width
                || bits
                    .iter()
                    .any(|(bit, value)| other_bits.get(bit).is_some_and(|other| other != value));
            if !distinguishable {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        instruction.span,
                        format!(
                            "two encoding shapes of instruction '{}' are the same \
                             {width} bits wide and fix no bit differently, so the \
                             word does not decide between them",
                            instruction.name
                        ),
                    ),
                ));
            }
        }
    }
    diags
}

/// The literal bits a shape pins, by absolute bit position.
fn fixed_bits(
    arms: &[ast::EncodingArm],
    params_cache: &HashMap<&str, (Type, Option<ast::Expr>)>,
) -> HashMap<u16, bool> {
    let mut bits = HashMap::new();
    for arm in arms {
        let literal = match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => li,
            ast::Expr::Ident(id) => match params_cache.get(id.name.as_str()) {
                Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => li,
                _ => continue,
            },
            _ => continue,
        };
        let value = crate::utils::parse_literal_value(literal);
        for bit in arm.start..=arm.end.unwrap_or(arm.start) {
            bits.insert(bit, (value >> (bit - arm.start)) & 1 == 1);
        }
    }
    bits
}

/// An instruction whose behavior reads the program counter cannot have its
/// length decided by a guard: the behavior would need to know which shape was
/// chosen. No branch relaxation exists, so one shape is the rule.
fn check_pc_single_shape(
    instruction: &ast::Instruction,
    shapes: &[crate::shapes::Shape],
    item_cache: &HashMap<&str, &ast::Item>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    if shapes.len() < 2 {
        return vec![];
    }
    let is_pc = |expr: &ast::Expr| {
        matches!(expr, ast::Expr::Path(path)
            if matches!(item_cache.get(path.base.as_str()),
                        Some(ast::Item::RegisterClass(class)) if class.is_program_counter()))
    };
    // Writing the program counter is fine — `jmp *reg` sets it from a register
    // whatever the encoding looks like. Reading it is not: the value an
    // instruction computes from it counts its own bytes, and the shape decides
    // how many those are. A read is any mention outside an assignment's
    // destination, including one buried in a call argument (`store(sp, 8,
    // PC::pc + 3)`).
    let mut written: Vec<Span> = Vec::new();
    crate::utils::visit_exprs(&instruction.behavior, &mut |node| {
        if let ast::Expr::Assign(assign) = node {
            written.push(expr_span(&assign.dest));
        }
    });
    let reads_pc = {
        let mut found = false;
        crate::utils::visit_exprs(&instruction.behavior, &mut |node| {
            let span = expr_span(node);
            let destination = written
                .iter()
                .any(|dest| dest.start <= span.start && span.end <= dest.end);
            found |= is_pc(node) && !destination;
        });
        found
    };
    if !reads_pc {
        return vec![];
    }
    vec![(
        file_name.to_string(),
        Rich::custom(
            instruction.span,
            format!(
                "instruction '{}' reads the program counter and has {} encoding \
                 shapes; its behavior cannot see which one the encoder picked",
                instruction.name,
                shapes.len()
            ),
        ),
    )]
}

/// Which bits of `name` the encoding spells, low bit first, or `None` when the
/// encoding does not carry the operand at all.
/// Whether every way of reaching this shape asks that `operand` survives the
/// `spelled` bits it spells of it. Each clause of the guard is one way, so each
/// has to make the test itself.
fn guard_proves_fit(
    guard: &crate::shapes::Guard,
    ctx: &crate::shapes::Context,
    operand: &str,
    spelled: u16,
) -> bool {
    fn proves(predicate: &crate::shapes::Predicate, operand: &str, spelled: u16) -> bool {
        use crate::shapes::Predicate;
        match predicate {
            Predicate::Fits {
                op, bits, signed, ..
            } => op == operand && (*bits == spelled || (!signed && *bits <= spelled)),
            // Every conjunct holds, so any one of them proving it is enough.
            Predicate::And(parts) => parts.iter().any(|part| proves(part, operand, spelled)),
            // `if ~fits { wide } else { narrow }` reaches the narrow shape with
            // the negation taken.
            Predicate::Not(inner) => match &**inner {
                Predicate::Not(inner) => proves(inner, operand, spelled),
                _ => false,
            },
            _ => false,
        }
    }
    guard.0.iter().all(|clause| {
        let clause = crate::shapes::Guard(vec![clause.clone()]);
        crate::shapes::lower_guard(&clause, ctx)
            .is_ok_and(|predicate| proves(&predicate, operand, spelled))
    })
}

fn covered_bits(encoding: &[ast::EncodingField], name: &str, width: u16) -> Option<Vec<bool>> {
    let mut covered = vec![false; usize::from(width)];
    let mut carried = false;
    for field in encoding {
        if encoding_value_name(&field.value) != Some(name) {
            continue;
        }
        carried = true;
        let (lo, hi) = match &field.value {
            ast::Expr::Slice(slc) => (slc.lo, slc.hi),
            ast::Expr::IndexAccess(idx) => (idx.index, idx.index),
            ast::Expr::Cast(_) => (0, field.width.saturating_sub(1)),
            _ => (0, width.saturating_sub(1)),
        };
        for bit in lo..=hi {
            // A slice reaching past the operand is reported on its own.
            if let Some(bit) = covered.get_mut(usize::from(bit)) {
                *bit = true;
            }
        }
    }
    carried.then_some(covered)
}

/// `#[align(N)]` is a power of two and constrains an immediate: a register
/// operand takes whatever values its class holds, not a multiple of anything.
fn check_operand_constraints(
    instruction: &ast::Instruction,
    operands: &[&ast::Operand],
    isa_params: &HashMap<String, i64>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    let mut diags = vec![];
    for operand in operands {
        let Some(align) = operand.align else {
            continue;
        };
        if !align.is_power_of_two() {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "alignment {align} of operand '{}' in '{}' is not a power of two",
                        operand.name, instruction.name
                    ),
                ),
            ));
        }
        if matches!(operand.ty, Type::Struct(_)) {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "operand '{}' of '{}' is a register; an alignment constrains an immediate",
                        operand.name, instruction.name
                    ),
                ),
            ));
        }

        // The smallest nonzero multiple of the alignment is the alignment
        // itself, so an operand too narrow to hold it can take no value at all.
        let width = match &operand.ty {
            Type::Bits(width) => Some(*width),
            Type::BitsExpr(expr) => eval_bits_width(expr, isa_params),
            _ => None,
        };
        if let Some(width) = width
            && operand.nonzero
            && align.is_power_of_two()
            && u32::from(width) <= align.trailing_zeros()
        {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    instruction.span,
                    format!(
                        "operand '{}' of '{}' is #[align({align})] and #[nonzero], \
                         which no bits<{width}> value satisfies",
                        operand.name, instruction.name
                    ),
                ),
            ));
        }
    }
    diags
}
