use super::*;

impl Checker {
    pub(super) fn operation(
        &mut self,
        definitions: &Definitions<'_>,
        expr: &Expr,
        fields: &[Node<OpField>],
        expected: Option<&Ty>,
        environment: &Environment,
        types: &Types,
    ) -> Result<Ty, Error> {
        let mut record = operation_record(fields, expr.range.clone())?;
        let Some((receiver_name, entries)) = fields.iter().find_map(|field| match &field.value {
            OpField::Interfaces { receiver, entries } => Some((receiver, entries)),
            _ => None,
        }) else {
            return self.check_expression(
                definitions,
                &record,
                expected.ok_or_else(|| {
                    Error(
                        "operation descriptor requires a source-defined contract annotation".into(),
                    )
                })?,
                environment,
                types,
            );
        };
        let Expression::Record(fields) = &mut record.value else {
            return Err(Error("operation must elaborate to a record".into()));
        };
        let annotation = expected.filter(|ty| !matches!(ty, Ty::Var(_)));
        if annotation.is_none() {
            if !fields.iter().any(|(name, _)| name == "signature") {
                return Err(Error("operation descriptor requires a signature".into()));
            }
            if fields.iter().any(|(name, _)| {
                !matches!(name.as_str(), "signature" | "attributes" | "interfaces")
            }) {
                return Err(Error(
                    "operation descriptor requires a source-defined contract annotation".into(),
                ));
            }
        }
        let attributes = fields.iter().find(|(name, _)| name == "attributes");
        let receiver = match attributes {
            Some((_, attributes)) => self.annotation(definitions, attributes, types)?,
            None => self.record_type(Vec::new(), Vec::new())?,
        };
        if !matches!(receiver, Ty::Con(id, _) if self.records.iter().any(|(_, record)| *record == id))
        {
            return Err(Error("operation attributes require a record type".into()));
        }
        let mut methods_env = environment.clone();
        let mut methods_types = types.clone();
        methods_env.insert(vec![receiver_name.clone()], BoundValue::mono(receiver));
        methods_types.insert(vec![receiver_name.clone()], None);
        let mut checked = Vec::new();
        let owner = self
            .frames
            .last()
            .filter(|path| {
                definitions.get(*path).is_some_and(|binding| {
                    let mut value = &binding.value;
                    while let Expression::Block(block) = &value.value {
                        value = &block.value;
                    }
                    std::ptr::eq(value, expr)
                })
            })
            .cloned();
        let mut implementations = BTreeMap::new();
        for (name, path, methods) in entries {
            let contract = definitions
                .get(path)
                .ok_or_else(|| Error(format!("unresolved operation interface {name}")))?;
            let declaration_types = self.types.clone();
            let (contract_types, generics) =
                self.generics(definitions, &contract.generics, &declaration_types)?;
            let ty = self.annotation(definitions, &contract.value, &contract_types)?;
            let mut components = vec![ty];
            components.extend(generics.iter().copied().map(Ty::Rigid));
            let packed = Ty::Con(TUPLE, type_list(components));
            let Ty::Con(_, components) = self.discharge(&packed, &generics, environment, None)?
            else {
                return Err(Error("invalid interface contract".into()));
            };
            let components = list_values(&components);
            let ty = self.check_expression(
                definitions,
                methods,
                &components[0],
                &methods_env,
                &methods_types,
            )?;
            let arguments = components[1..]
                .iter()
                .map(|arg| self.substitution.apply(arg))
                .collect::<Vec<_>>();
            if arguments
                .iter()
                .any(|arg| !hm::free_variables(arg).is_empty())
            {
                return Err(Error(format!(
                    "cannot infer generic arguments for interface {name}"
                )));
            }
            let inferred = generics
                .iter()
                .copied()
                .zip(arguments.iter().cloned())
                .collect::<Vec<_>>();
            for (parameter, argument) in &inferred {
                let kind = self
                    .rigid_types
                    .get(&parameter.0)
                    .ok_or_else(|| Error("missing interface parameter kind".into()))?;
                let kind = replace_rigid(kind, &inferred);
                self.unify(&self.value_sort(argument), &kind).map_err(|_| {
                    Error(format!(
                        "inferred argument for interface {name} has the wrong kind or universe"
                    ))
                })?;
            }
            if owner.is_some() {
                implementations.insert(
                    path.clone(),
                    CheckedMethods::new(methods.clone(), ty.clone(), arguments),
                );
            }
            checked.push((name.clone(), ty));
        }
        let mut operation_env = environment.clone();
        let interfaces_ty = self.record_type(checked, Vec::new())?;
        for (name, value) in fields {
            let ty = match name.as_str() {
                "interfaces" => interfaces_ty.clone(),
                "signature" if annotation.is_none() => {
                    self.operation_signature(definitions, value, types)?;
                    con(TYPE)
                }
                "attributes" if annotation.is_none() => con(TYPE),
                _ => continue,
            };
            let path = vec![format!("\0operation_{name}")];
            operation_env.insert(path.clone(), BoundValue::mono(ty));
            value.value = Expression::Path(path);
        }
        let result = match annotation {
            Some(expected) => {
                self.check_expression(definitions, &record, expected, &operation_env, types)
            }
            None => {
                let inferred = self.expression(definitions, &record, &operation_env, types)?;
                if let Some(expected) = expected {
                    self.unify(&inferred, expected)?;
                }
                Ok(inferred)
            }
        }?;
        if let Some(owner) = owner {
            self.checked_interfaces.insert(owner, implementations);
        }
        Ok(result)
    }

    fn operation_signature(
        &mut self,
        definitions: &Definitions<'_>,
        signature: &Expr,
        types: &Types,
    ) -> Result<(), Error> {
        let Expression::Binary {
            op: BinaryOp::Arrow,
            left,
            right,
        } = &signature.value
        else {
            return Err(Error("operation signature must be a function type".into()));
        };
        let ir_type = vec!["std".into(), "ir".into(), "Ty".into()];
        for side in [left.as_ref(), right.as_ref()] {
            let mut pending = vec![side];
            while let Some(item) = pending.pop() {
                if let Expression::Tuple(items) = &item.value {
                    pending.extend(items.iter().rev());
                    continue;
                }
                let ty = self.denotation(definitions, item, types)?;
                let sort = self.value_sort(&ty);
                if sort != con(TYPE) && Some(&Some(sort)) != self.types.get(&ir_type) {
                    return Err(Error("operation signature requires a type".into()));
                }
            }
        }
        Ok(())
    }
}

pub(crate) fn operation_record(
    fields: &[Node<OpField>],
    range: std::ops::Range<usize>,
) -> Result<Expr, Error> {
    let mut entries = Vec::new();
    for field in fields {
        let (name, value) = match &field.value {
            OpField::Field { name, value } => (name.clone(), value.clone()),
            OpField::Interfaces { entries, .. } => (
                "interfaces".into(),
                Node {
                    value: Expression::Record(
                        entries
                            .iter()
                            .map(|(name, _, methods)| (name.clone(), methods.clone()))
                            .collect(),
                    ),
                    range: field.range.clone(),
                },
            ),
            OpField::Signature {
                parameters,
                result,
                results,
            } => {
                let mut names = BTreeSet::new();
                if parameters
                    .iter()
                    .any(|parameter| !names.insert(&parameter.name))
                {
                    return Err(Error("duplicate operand name".into()));
                }
                let unique = results.iter().collect::<BTreeSet<_>>();
                if unique.len() != results.len() {
                    return Err(Error("duplicate result name".into()));
                }
                if !results.is_empty()
                    && !matches!(&result.value, Expression::Tuple(items) if items.len() == results.len())
                {
                    return Err(Error(
                        "named result count disagrees with result product".into(),
                    ));
                }
                let left = Node {
                    value: Expression::Tuple(
                        parameters
                            .iter()
                            .map(|parameter| parameter.ty.clone())
                            .collect(),
                    ),
                    range: field.range.clone(),
                };
                (
                    "signature".into(),
                    Node {
                        value: Expression::Binary {
                            op: BinaryOp::Arrow,
                            left: Box::new(left),
                            right: Box::new(result.clone()),
                        },
                        range: field.range.clone(),
                    },
                )
            }
        };
        if entries.iter().any(|(previous, _)| previous == &name) {
            return Err(Error(format!("duplicate operation field {name}")));
        }
        entries.push((name, value));
    }
    Ok(Node {
        value: Expression::Record(entries),
        range,
    })
}
