use super::*;

pub(in crate::engine) struct Dependencies<'a> {
    pub declarations: BTreeMap<Path, &'a Declaration>,
    pub ordered: Vec<Path>,
    pub needed: BTreeSet<Path>,
    pub active: BTreeSet<Path>,
    pub operations: BTreeMap<Path, &'a Binding>,
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
        let owned_packages = artifact
            .modules
            .iter()
            .map(|module| module.package.as_str())
            .collect::<BTreeSet<_>>();
        let mut declarations = BTreeMap::new();
        let mut operations = BTreeMap::new();
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
                let (name, executable, operation) = match &node.value {
                    Declaration::Axiom { name, ty, .. } => {
                        (std::slice::from_ref(name), clause(ty), None)
                    }
                    Declaration::Enum { name, .. } => (std::slice::from_ref(name), false, None),
                    Declaration::Let { binding, .. } => {
                        let operation = binding.operation_name(&module.path).map(|_| binding);
                        (
                            binding.path.as_slice(),
                            operation.is_none() && module.package != "<embedded std>",
                            operation,
                        )
                    }
                    _ => continue,
                };
                let path = [module.path.as_slice(), name].concat();
                if module.package != "<embedded std>"
                    && module.path.starts_with(selected)
                    && roots.contains(module.path.as_slice())
                    && (!catalog_only || !executable)
                    && operation.is_none()
                {
                    active.insert(path.clone());
                }
                if matches!(mode, Selection::FullSchema)
                    && owned_packages.contains(module.package.as_str())
                    && !executable
                    && module.package != "<embedded std>"
                    && operation.is_none()
                {
                    pending.push(path.clone());
                }
                ordered.push(path.clone());
                references.insert(path.clone(), refs);
                if let Some(binding) = operation {
                    operations.insert(path.clone(), binding);
                }
                declarations.insert(path, &node.value);
            }
        }

        pending.extend(active.iter().cloned());
        if roots.contains(selected) {
            pending.extend(rust::exported_members(program, selected).into_values());
        }
        let mut needed = BTreeSet::new();
        while let Some(path) = pending.pop() {
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
            let is_operation = operations.contains_key(&owner);
            if catalog_only && !is_operation && is_executable(declarations[&owner]) {
                continue;
            }
            if !needed.insert(owner.clone()) {
                continue;
            }
            if is_operation {
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
                    pending.extend(declarations.iter().filter_map(|(path, declaration)| {
                        storage_name(declaration)
                            .filter(|name| names.contains(name))
                            .map(|_| path.clone())
                    }));
                }
            }
        }
        ordered.retain(|path| needed.contains(path));
        operations.retain(|path, _| needed.contains(path));
        Self {
            declarations,
            ordered,
            needed,
            active,
            operations,
        }
    }

    pub fn operation_imports(&self, program: &CheckedProgram) -> BTreeMap<Path, OperationImport> {
        self.operations
            .iter()
            .filter_map(|(path, binding)| {
                let module = &path[..path.len() - binding.path.len()];
                let import = binding
                    .operation_import(module, program.artifact().declaration_source(module))?;
                Some((path.clone(), import))
            })
            .collect()
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
