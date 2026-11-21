use super::*;

pub(in crate::engine) struct Dependencies<'a> {
    pub declarations: BTreeMap<Path, &'a Declaration>,
    pub ordered: Vec<Path>,
    pub needed: BTreeSet<Path>,
    pub active: BTreeSet<Path>,
}

pub(in crate::engine) enum Selection {
    Program,
    Catalog,
    FullSchema,
}

impl<'a> Dependencies<'a> {
    pub fn collect(program: &'a CheckedProgram, selected: &[String], mode: Selection) -> Self {
        let catalog_only = matches!(mode, Selection::Catalog);
        let artifact = program.artifact();
        let mut ordered = Vec::new();
        let roots = artifact
            .modules
            .iter()
            .map(|module| module.path.as_slice())
            .collect::<BTreeSet<_>>();
        let mut declarations = BTreeMap::new();
        let mut references = BTreeMap::new();
        let mut active = BTreeSet::new();
        let mut pending = Vec::new();
        for module in artifact.all_modules() {
            for (node, refs) in module
                .ast
                .declarations
                .iter()
                .zip(&artifact.references[&module.path])
            {
                let (name, executable) = match &node.value {
                    Declaration::Axiom { name, ty, .. } => (std::slice::from_ref(name), clause(ty)),
                    Declaration::Enum { name, .. } => (std::slice::from_ref(name), false),
                    Declaration::Let { binding, .. }
                        if binding.operation_name(&module.path).is_none() =>
                    {
                        (binding.path.as_slice(), module.package != "<embedded std>")
                    }
                    _ => continue,
                };
                let path = [module.path.as_slice(), name].concat();
                if module.package != "<embedded std>"
                    && module.path.starts_with(selected)
                    && roots.contains(module.path.as_slice())
                    && (!catalog_only || !executable)
                {
                    active.insert(path.clone());
                }
                if matches!(mode, Selection::FullSchema)
                    && !executable
                    && module.package != "<embedded std>"
                {
                    pending.push(path.clone());
                }
                ordered.push(path.clone());
                references.insert(path.clone(), refs);
                declarations.insert(path, &node.value);
            }
        }

        pending.extend(active.iter().cloned());
        if roots.contains(selected) {
            pending.extend(rust::exported_members(program, selected).into_values());
        }
        let mut expanded_modules = BTreeSet::new();
        let mut needed = BTreeSet::new();
        while let Some(path) = pending.pop() {
            if artifact.module_exports.contains_key(&path) {
                if !expanded_modules.insert(path.clone()) {
                    continue;
                }
                pending.extend(declarations.range(path.clone()..)
                    .take_while(|(member, _)| member.starts_with(&path))
                    .filter(|(_, declaration)| matches!(declaration, Declaration::Axiom { ty, .. } if clause(ty)))
                    .map(|(member, _)| member.clone()));
                continue;
            }
            let owner = if declarations.contains_key(&path) {
                path
            } else if path.len() > 1
                && matches!(
                    declarations.get(&path[..path.len() - 1]),
                    Some(Declaration::Enum { .. })
                )
            {
                path[..path.len() - 1].to_vec()
            } else {
                continue;
            };
            if catalog_only && is_executable(declarations[&owner]) {
                continue;
            }
            if !needed.insert(owner.clone()) {
                continue;
            }
            pending.extend(references[&owner].iter().cloned());
            if let Some(matches) = program.operation_matches.get(&owner) {
                for checked in matches.values() {
                    let mut names = vec!["Operation"];
                    let mut projections = BTreeSet::new();
                    if !checked.operands.is_empty() {
                        names.push("Operand");
                    }
                    if !checked.operands.is_empty() || !checked.results.is_empty() {
                        names.push("Value");
                    }
                    for ty in checked.operands.iter().chain(&checked.results) {
                        match_dependencies(ty, &mut names, &mut projections);
                    }
                    pending.extend(projections.into_iter().map(ir_host_path));
                    pending.extend(declarations.iter().filter_map(
                        |(declaration, node)| match node {
                            Declaration::Axiom {
                                engine: Some(EngineBinding::Name(name)),
                                ..
                            } if names.contains(&name.as_str()) => Some(declaration.clone()),
                            _ => None,
                        },
                    ));
                }
            }
        }
        ordered.retain(|path| needed.contains(path));
        Self {
            declarations,
            ordered,
            needed,
            active,
        }
    }
}

fn is_executable(declaration: &Declaration) -> bool {
    match declaration {
        Declaration::Let { .. } => true,
        Declaration::Axiom { ty, .. } => clause(ty),
        _ => false,
    }
}

fn match_dependencies(
    ty: &PortType,
    tables: &mut Vec<&'static str>,
    projections: &mut BTreeSet<&'static str>,
) {
    let (projection, has_symbol) = match ty {
        PortType::Integer(width) => ("integer_width", matches!(width, PortIndex::Symbol(_))),
        PortType::Float(format) => ("fp_format", matches!(format, PortIndex::Symbol(_))),
        PortType::Pointer(pointee) => {
            match_dependencies(pointee, tables, projections);
            ("type_argument", false)
        }
        PortType::Computation {
            format,
            rounding,
            contraction,
        } => {
            match_dependencies(&PortType::Float(*format), tables, projections);
            (
                "type_argument",
                [rounding, contraction]
                    .iter()
                    .any(|index| matches!(index, PortIndex::Symbol(_))),
            )
        }
        PortType::Variable(_) | PortType::Unsupported => return,
    };
    projections.insert(projection);
    tables.extend(["TypeTerm", "TypeCtor"]);
    if has_symbol {
        tables.push("Symbol");
    }
}
