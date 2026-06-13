use std::collections::{HashMap, HashSet};

use chumsky::error::Rich;

use crate::utils::{
    eval_bits_width, resolve_effective_asm_for_instruction,
    resolve_effective_encoding_for_instruction, resolve_isa_param_values,
    resolve_params_for_instruction, resolve_template_chain,
};
use crate::{Span, Type, ast};

type Diag = Rich<'static, String, Span>;

// TODO path strings must be interned
pub fn analyze(files: &[ast::File]) -> Vec<(String, Diag)> {
    let mut diags = vec![];

    let cache = build_item_cache(files);

    // TODO check item names are unique
    diags.extend(check_isas(files, &cache));
    diags.extend(check_templates(files, &cache));
    diags.extend(check_instructions(files, &cache));
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
            }

            // Overrides target a real instruction (at most once), use this
            // machine's resources, and reference real pipeline phases.
            let mut overridden: HashSet<&str> = HashSet::new();
            for ov in &machine.overrides {
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
) -> Vec<(String, Diag)> {
    let mut diags: Vec<(String, Diag)> = files
        .iter()
        .flat_map(|f| {
            f.instructions()
                .flat_map(|i| check_instruction_consistent(i, item_cache, &f.file_name).into_iter())
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

    // Encoding must exist somewhere in the chain or instruction.
    let effective_encoding = resolve_effective_encoding_for_instruction(instruction, item_cache);
    if effective_encoding.is_empty() {
        diags.push((
            file_name.to_string(),
            Rich::custom(
                instruction.span,
                format!("Instruction '{}' has no encoding defined", instruction.name),
            ),
        ));
    } else {
        diags.extend(check_encoding(
            instruction,
            effective_encoding,
            &params_cache,
            &operands_cache,
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
            ast::Expr::For(f) => {
                walk_paths(&f.start, out);
                walk_paths(&f.end, out);
                walk_paths(&f.body, out);
            }
            ast::Expr::IndexAccess(i) => walk_paths(&i.base, out),
            ast::Expr::Slice(s) => walk_paths(&s.base, out),
            ast::Expr::Try(t) => {
                walk_paths(&t.body, out);
                for handler in &t.handlers {
                    walk_paths(&handler.body, out);
                }
            }
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
            ast::Expr::For(f) => walk_excepts(&f.body, out),
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

    diags
}

fn check_encoding(
    instruction: &ast::Instruction,
    encoding: &[ast::EncodingArm],
    params_cache: &HashMap<&str, (Type, Option<ast::Expr>)>,
    operands_cache: &HashMap<&str, Type>,
    file_name: &str,
) -> Vec<(String, Diag)> {
    let mut diags = vec![];

    let known = |name: &str| params_cache.contains_key(name) || operands_cache.contains_key(name);
    let invalid_value = |span: Span| {
        (
            file_name.to_string(),
            Rich::custom(
                span,
                format!(
                    "Encoding value in instruction '{}' must be a literal, \
                     parameter, or operand reference",
                    instruction.name
                ),
            ),
        )
    };
    let unknown_value = |name: &str, span: Span| {
        (
            file_name.to_string(),
            Rich::custom(
                span,
                format!(
                    "Unknown '{}' in encoding of instruction '{}': \
                     not a parameter or operand",
                    name, instruction.name
                ),
            ),
        )
    };

    let value_width = |name: &str| -> Option<u16> {
        let ty = params_cache
            .get(name)
            .map(|(ty, _)| ty)
            .or_else(|| operands_cache.get(name))?;
        match ty {
            Type::Bits(width) => Some(*width),
            _ => None,
        }
    };
    let width_error = |span: Span, message: String| {
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

    for arm in encoding {
        if let ast::Expr::Lit(_) = arm.value {
            continue;
        }

        match encoding_value_name(&arm.value) {
            Some(name) if !known(name) => diags.push(unknown_value(name, arm.span)),
            Some(name) => {
                let arm_width = arm.end.unwrap_or(arm.start) - arm.start + 1;
                match &arm.value {
                    ast::Expr::Slice(slc) => {
                        let slice_width = slc.end - slc.start + 1;
                        if slice_width != arm_width {
                            diags.push(width_error(
                                arm.span,
                                format!(
                                    "slice '{name}[{}..{}]' is {slice_width} bits but the arm covers {arm_width}",
                                    slc.start, slc.end
                                ),
                            ));
                        }
                        if let Some(width) = value_width(name)
                            && slc.end >= width
                        {
                            diags.push(width_error(
                                arm.span,
                                format!(
                                    "slice '{name}[{}..{}]' exceeds bits<{width}>",
                                    slc.start, slc.end
                                ),
                            ));
                        }
                    }
                    ast::Expr::IndexAccess(idx) => {
                        if arm_width != 1 {
                            diags.push(width_error(
                                arm.span,
                                format!(
                                    "single bit '{name}[{}]' assigned to a {arm_width}-bit arm",
                                    idx.index
                                ),
                            ));
                        }
                        if let Some(width) = value_width(name)
                            && idx.index >= width
                        {
                            diags.push(width_error(
                                arm.span,
                                format!("bit '{name}[{}]' exceeds bits<{width}>", idx.index),
                            ));
                        }
                    }
                    ast::Expr::Ident(_) => {
                        if let Some(width) = value_width(name)
                            && arm_width != width
                        {
                            diags.push(width_error(
                                arm.span,
                                format!(
                                    "'{name}' is bits<{width}> but the arm covers {arm_width} bits"
                                ),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            None => {
                diags.push(invalid_value(arm.span));
            }
        }
    }

    diags
}

#[cfg(test)]
mod perf_model_tests {
    use super::analyze;
    use crate::{lex, parse};

    /// Lex + parse `src` into a one-file program and run semantic analysis,
    /// returning the diagnostic messages.
    pub(super) fn diagnose(src: &str) -> Vec<String> {
        let (tokens, lex_errs) = lex(src);
        assert!(lex_errs.is_empty(), "lex errors: {lex_errs:?}");
        let (file, parse_errs) = parse(src, &tokens, "test.tmdl");
        assert!(parse_errs.is_empty(), "parse errors: {parse_errs:?}");
        analyze(&[file.unwrap()])
            .into_iter()
            .map(|(_, d)| d.to_string())
            .collect()
    }

    const PRELUDE: &str = "
        sched_class WriteIALU;
        sched_class WriteIMul { latency = 3; }
        machine RocketCore for [RV64I] {
            unit ALU { count = 2; }
            unit MUL { count = 1; }
            bind WriteIALU { latency = 1; uses = [ALU]; }
            bind WriteIMul { latency = 3; uses = [MUL]; }
        }
    ";

    #[test]
    fn well_formed_model_has_no_perf_diagnostics() {
        let src = format!(
            "{PRELUDE}
            instruction Mul {{ behavior {{ rd = rs1; }} schedule {{ units = [WriteIMul]; }} }}"
        );
        // The minimal instruction trips unrelated checks (mnemonic/encoding/asm);
        // assert only that the performance model itself is clean.
        let perf: Vec<_> = diagnose(&src)
            .into_iter()
            .filter(|d| d.contains("unit") || d.contains("resource") || d.contains("bind"))
            .collect();
        assert!(perf.is_empty(), "unexpected perf diags: {perf:?}");
    }

    #[test]
    fn unknown_unit_in_schedule_is_reported() {
        let src = format!(
            "{PRELUDE}
            instruction Mul {{ behavior {{ rd = rs1; }} schedule {{ units = [WirteIMul]; }} }}"
        );
        let diags = diagnose(&src);
        assert!(
            diags.iter().any(|d| d.contains("unknown unit 'WirteIMul'")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn bind_to_unknown_unit_is_reported() {
        let src = "
            machine M for [RV64I] {
                unit ALU { count = 1; }
                bind NotAUnit { latency = 1; uses = [ALU]; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("binds unknown unit 'NotAUnit'")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn use_of_unknown_resource_is_reported() {
        let src = "
            sched_class W;
            machine M for [RV64I] {
                unit ALU { count = 1; }
                bind W { latency = 1; uses = [FPU]; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("uses unknown resource 'FPU'")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn phase_not_in_pipeline_is_reported() {
        let src = "
            sched_class W;
            machine M for [RV64I] {
                pipeline { IF; ID; }
                unit L { count = 1; }
                bind W { reads = ID; writes = NOPE; uses = [L]; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("references phase 'NOPE' not in machine 'M' pipeline")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn phase_bind_without_pipeline_is_reported() {
        let src = "
            sched_class W;
            machine M for [RV64I] {
                unit L { count = 1; }
                bind W { reads = ID; uses = [L]; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("references phase 'ID' not in machine 'M' pipeline")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn override_of_unknown_instruction_is_reported() {
        let src = "
            machine M for [RV64I] {
                unit ALU { count = 1; }
                override Nope { latency = 1; uses = [ALU]; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("overrides unknown instruction 'Nope'")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn forward_unknown_resource_is_reported() {
        let src = "
            machine M for [RV64I] {
                unit ALU { count = 1; }
                forward ALU => FPU { latency = 0; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("forward target 'FPU' is not a resource")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn duplicate_unit_and_bind_and_resource_are_reported() {
        let src = "
            sched_class W;
            sched_class W;
            machine M for [RV64I] {
                unit ALU { count = 1; }
                unit ALU { count = 2; }
                bind W { latency = 1; uses = [ALU]; }
                bind W { latency = 2; uses = [ALU]; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("duplicate unit declaration 'W'")),
            "diags: {diags:?}"
        );
        assert!(
            diags.iter().any(|d| d.contains("duplicate resource 'ALU'")),
            "diags: {diags:?}"
        );
        assert!(
            diags
                .iter()
                .any(|d| d.contains("duplicate bind for unit 'W'")),
            "diags: {diags:?}"
        );
    }
}

#[cfg(test)]
mod encoding_tests {
    use super::perf_model_tests::diagnose;

    const INST: &str = "
        instruction Foo {
            param MNEMONIC: String = \"foo\";
            param OPCODE: bits<7> = 0b0010011;
            operands { imm: bits<13> }
            behavior { imm = imm; }
    ";

    #[test]
    fn well_formed_encoding_has_no_diagnostics() {
        let src = format!(
            "{INST}
            encoding {{ 0..6 => OPCODE, 7 => imm[12], 8..11 => imm[1..4], 12..19 => imm[5..12] }}
        }}"
        );
        let diags: Vec<_> = diagnose(&src)
            .into_iter()
            .filter(|d| d.contains("encoding"))
            .collect();
        assert!(diags.is_empty(), "unexpected encoding diags: {diags:?}");
    }

    #[test]
    fn slice_out_of_operand_range_is_reported() {
        let src = format!(
            "{INST}
            encoding {{ 0..6 => OPCODE, 8..11 => imm[10..13] }}
        }}"
        );
        let diags = diagnose(&src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("slice 'imm[10..13]' exceeds bits<13>")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn slice_width_mismatch_is_reported() {
        let src = format!(
            "{INST}
            encoding {{ 0..6 => OPCODE, 8..11 => imm[1..5] }}
        }}"
        );
        let diags = diagnose(&src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("slice 'imm[1..5]' is 5 bits but the arm covers 4")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn single_bit_out_of_range_is_reported() {
        let src = format!(
            "{INST}
            encoding {{ 0..6 => OPCODE, 7 => imm[13] }}
        }}"
        );
        let diags = diagnose(&src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("bit 'imm[13]' exceeds bits<13>")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn whole_operand_width_mismatch_is_reported() {
        let src = format!(
            "{INST}
            encoding {{ 0..6 => OPCODE, 12..31 => imm }}
        }}"
        );
        let diags = diagnose(&src);
        assert!(
            diags
                .iter()
                .any(|d| d.contains("'imm' is bits<13> but the arm covers 20 bits")),
            "diags: {diags:?}"
        );
    }
}

#[cfg(test)]
mod operand_width_tests {
    use super::perf_model_tests::diagnose;

    #[test]
    fn isa_dependent_operand_width_resolves() {
        let src = "
            isa TestIsa { param XLEN: Integer = 32; }
            instruction Foo for [TestIsa] {
                operands { imm: bits<log2Ceil(self.XLEN)>, }
                behavior { rd = imm; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            !diags.iter().any(|d| d.contains("does not evaluate")),
            "diags: {diags:?}"
        );
    }

    #[test]
    fn non_constant_operand_width_is_reported() {
        let src = "
            isa TestIsa { param XLEN: Integer = 32; }
            instruction Foo for [TestIsa] {
                operands { imm: bits<log2Ceil(self.UNDEFINED)>, }
                behavior { rd = imm; }
            }
        ";
        let diags = diagnose(src);
        assert!(
            diags.iter().any(|d| d.contains(
                "width of operand 'imm' in instruction 'Foo' does not evaluate to a constant"
            )),
            "diags: {diags:?}"
        );
    }
}
