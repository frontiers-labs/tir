use super::*;
use crate::lower::{CheckedMatch, PortIndex, PortType};

fn port_variable(ty: &Ty, variables: &mut Vec<Ty>) -> usize {
    if let Some(index) = variables.iter().position(|candidate| candidate == ty) {
        return index;
    }
    let index = variables.len();
    variables.push(ty.clone());
    index
}

impl Checker {
    pub(super) fn interface_type(
        &mut self,
        definitions: &Definitions<'_>,
        contract: &Binding,
        types: &Types,
    ) -> Result<Ty, Error> {
        let Expression::Record(methods) = &contract.value.value else {
            return Err(Error("interface contract must be a record".into()));
        };
        let mut defaults = Vec::new();
        let signatures = methods
            .iter()
            .map(|(name, method)| {
                let ty = if let Expression::Function(function) = &method.value {
                    if !function.generics.is_empty() {
                        return Err(Error("interface method cannot declare generics".into()));
                    }
                    let inputs = function
                        .parameters
                        .iter()
                        .map(|parameter| self.annotation(definitions, &parameter.ty, types))
                        .collect::<Result<Vec<_>, _>>()?;
                    let output = function.result.as_ref().ok_or_else(|| {
                        Error(format!("interface method {name} needs a result type"))
                    })?;
                    let output = self.annotation(definitions, output, types)?;
                    let ty = Ty::Fun(type_list(inputs), Box::new(output));
                    defaults.push((function, ty.clone()));
                    ty
                } else {
                    self.annotation(definitions, method, types)?
                };
                if !matches!(ty, Ty::Fun(..)) {
                    return Err(Error(format!("interface method {name} needs a signature")));
                }
                Ok((name.clone(), ty))
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let contract = self.record_type(signatures, Vec::new())?;
        if !defaults.is_empty() {
            let receiver = vec!["self".into()];
            let environment =
                Environment::from([(receiver.clone(), BoundValue::mono(contract.clone()))]);
            let mut types = types.clone();
            types.insert(receiver, None);
            for (function, signature) in defaults {
                self.function(
                    definitions,
                    function,
                    &environment,
                    &types,
                    FunctionMode::Check(&signature),
                )?;
            }
        }
        Ok(contract)
    }

    pub(super) fn operation_member(
        &mut self,
        definitions: &Definitions<'_>,
        value: &Expr,
        member: &str,
        environment: &Environment,
        types: &Types,
    ) -> Result<Option<Ty>, Error> {
        if !matches!(member, "kind" | "typed_kind" | "matches") {
            return Ok(None);
        }
        let Expression::Path(path) = &value.value else {
            return Ok(None);
        };
        if environment.contains_key(path) || types.get(path) == Some(&None) {
            return Ok(None);
        }
        let Some(binding) = definitions.get(path) else {
            return Ok(None);
        };
        if matches!(&binding.value.value, Expression::Record(fields)
            if fields.iter().any(|(name, _)| name == member))
        {
            return Ok(None);
        }
        if binding
            .operation_name(&path[..path.len() - binding.path.len()])
            .is_none()
        {
            return Ok(None);
        }
        if member == "kind" {
            return Ok(Some(con(
                self.operation_type_family(definitions, "OperationKind")?
            )));
        }
        let kind = self.operation_kind(definitions, path, binding)?;
        if member == "typed_kind" {
            return Ok(Some(kind));
        }
        let Ty::Con(_, groups) = kind else {
            return Err(Error("missing operation ports".into()));
        };
        let mut parameters = vec![con(self.operation_type_family(definitions, "OperationId")?)];
        parameters.extend(list_values(&groups));
        Ok(Some(Ty::Fun(type_list(parameters), Box::new(con(FACT)))))
    }

    pub(super) fn checked_matches(
        &mut self,
        matches: MatchArguments,
    ) -> Result<BTreeMap<usize, CheckedMatch>, Error> {
        let mut variables = Vec::new();
        let mut group = |ty| {
            let ty = self.normalize_indices(&self.substitution.apply(&ty))?;
            match ty {
                Ty::Con(UNIT, _) => Ok(Vec::new()),
                Ty::Con(TUPLE, ports) => Ok(list_values(&ports)
                    .iter()
                    .map(|ty| {
                        if let Ty::Con(id, arguments) = ty
                            && self.port_family(*id, &["std", "ir", "ValueRef"])
                            && let [port] = list_values(arguments).as_slice()
                        {
                            return self.port_type(port, &mut variables);
                        }
                        PortType::Unsupported
                    })
                    .collect()),
                _ => Err(Error("operation match ports must be tuples".into())),
            }
        };
        matches
            .into_iter()
            .map(|(position, [operands, results])| {
                Ok((
                    position,
                    CheckedMatch {
                        operands: group(operands)?,
                        results: group(results)?,
                    },
                ))
            })
            .collect()
    }

    pub(super) fn port_family(&self, id: ConId, path: &[&str]) -> bool {
        matches!(
            self.types.get(&path.iter().map(|s| (*s).to_owned()).collect::<Path>()),
            Some(Some(Ty::Con(candidate, _))) if *candidate == id
        )
    }

    fn port_type(&self, ty: &Ty, variables: &mut Vec<Ty>) -> PortType {
        let Ty::Con(id, arguments) = ty else {
            return match ty {
                Ty::Var(_) | Ty::Rigid(_) => PortType::Variable(port_variable(ty, variables)),
                _ => PortType::Unsupported,
            };
        };
        match list_values(arguments).as_slice() {
            [inner] if self.port_family(*id, &["std", "ir", "El"]) => {
                self.port_type(inner, variables)
            }
            [width] if self.port_family(*id, &["std", "ir", "Bits"]) => {
                if let Some(width) = self
                    .natural_value(width)
                    .and_then(|width| i64::try_from(width).ok())
                {
                    PortType::Integer(PortIndex::Constant(width))
                } else if matches!(width, Ty::Var(_) | Ty::Rigid(_))
                    || matches!(width, Ty::Con(id, args) if matches!(args.as_ref(), TyList::Nil)
                        && self.port_family(*id, &["std", "ir", "RuleWidth"]))
                {
                    PortType::Integer(PortIndex::Variable(port_variable(width, variables)))
                } else {
                    PortType::Unsupported
                }
            }
            [pointee] if self.port_family(*id, &["std", "ir", "Ptr"]) => {
                PortType::Pointer(Box::new(self.port_type(pointee, variables)))
            }
            [format] if self.port_family(*id, &["dialects", "fp", "model", "Float"]) => self
                .fp_port_index(format, variables)
                .map_or(PortType::Unsupported, PortType::Float),
            [format, rounding, contraction]
                if self.port_family(*id, &["dialects", "fp", "model", "Computation"]) =>
            {
                match [format, rounding, contraction].map(|ty| self.fp_port_index(ty, variables)) {
                    [Some(format), Some(rounding), Some(contraction)] => PortType::Computation {
                        format,
                        rounding,
                        contraction,
                    },
                    _ => PortType::Unsupported,
                }
            }
            _ => PortType::Unsupported,
        }
    }

    fn fp_port_index(&self, ty: &Ty, variables: &mut Vec<Ty>) -> Option<PortIndex> {
        if matches!(ty, Ty::Var(_) | Ty::Rigid(_)) {
            return Some(PortIndex::Variable(port_variable(ty, variables)));
        }
        let Ty::Con(id, args) = ty else { return None };
        if !matches!(args.as_ref(), TyList::Nil) {
            return None;
        }
        let (_, expression, _) = self
            .indexed_terms
            .iter()
            .find(|(candidate, _, _)| candidate == id)?;
        let Expression::Path(path) = &expression.value else {
            return None;
        };
        let [dialects, fp, model, family, case] = path.as_slice() else {
            return None;
        };
        if dialects != "dialects" || fp != "fp" || model != "model" {
            return None;
        }
        Some(match (family.as_str(), case.as_str()) {
            ("Format", "Binary16") => PortIndex::Constant(16),
            ("Format", "Binary32") => PortIndex::Constant(32),
            ("Format", "Binary64") => PortIndex::Constant(64),
            ("Rounding", "NearestEven") => PortIndex::Symbol("nearest_even"),
            ("Rounding", "NearestAway") => PortIndex::Symbol("nearest_away"),
            ("Rounding", "TowardZero") => PortIndex::Symbol("toward_zero"),
            ("Rounding", "TowardPositive") => PortIndex::Symbol("toward_positive"),
            ("Rounding", "TowardNegative") => PortIndex::Symbol("toward_negative"),
            ("Contraction", "Separate") => PortIndex::Symbol("separate"),
            ("Contraction", "Permitted") => PortIndex::Symbol("permitted"),
            _ => return None,
        })
    }

    fn operation_type_family(
        &mut self,
        definitions: &Definitions<'_>,
        name: &str,
    ) -> Result<ConId, Error> {
        let path = vec!["std".into(), "ir".into(), name.into()];
        if let Some((generics, annotation)) = self.axioms.get(&path).cloned() {
            self.axiom(definitions, &path, &generics, &annotation)?;
        }
        let Some(Some(Ty::Con(id, _))) = self.types.get(&path) else {
            return Err(Error(format!("missing {name} declaration")));
        };
        Ok(*id)
    }

    fn operation_kind(
        &mut self,
        definitions: &Definitions<'_>,
        path: &Path,
        binding: &Binding,
    ) -> Result<Ty, Error> {
        if let Some(scheme) = self.operation_kinds.get(path).cloned() {
            return self.instantiate(&scheme);
        }
        let fields = binding.operation_fields().ok_or_else(|| {
            Error(format!(
                "{} has no checked operation signature",
                path.join("::")
            ))
        })?;
        let named_results = fields.iter().any(|field| {
            matches!(&field.value, OpField::Signature { results, .. } if !results.is_empty())
        });
        let record = operation_record(fields, binding.value.range.clone())?;
        let Expression::Record(fields) = &record.value else {
            return Err(Error("operation must elaborate to a record".into()));
        };
        let signature = fields
            .iter()
            .find_map(|(name, field)| (name == "signature").then_some(field))
            .ok_or_else(|| Error("operation has no signature".into()))?;
        let globals = self.types.clone();
        let (locals, generics) = self.generics(definitions, &binding.generics, &globals)?;
        let signature = self.operation_signature(definitions, signature, &locals)?;
        let id = self.operation_type_family(definitions, "OperationKindOf")?;
        let Ty::Fun(operands, result) = signature else {
            return Err(Error("operation signature must be a function type".into()));
        };
        let reference = self.operation_type_family(definitions, "ValueRef")?;
        let refs = |ports: Vec<Ty>| {
            if ports.is_empty() {
                return con(UNIT);
            }
            Ty::Con(
                TUPLE,
                type_list(
                    ports
                        .into_iter()
                        .map(|port| Ty::Con(reference, type_list(vec![port])))
                        .collect(),
                ),
            )
        };
        let inputs = refs(list_values(&operands));
        let outputs = if *result == con(UNIT) {
            con(UNIT)
        } else if named_results {
            let Ty::Con(TUPLE, ports) = *result else {
                return Err(Error("named results require a result product".into()));
            };
            refs(list_values(&ports))
        } else {
            refs(vec![*result])
        };
        let kind = Ty::Con(id, type_list(vec![inputs, outputs]));
        let kind = self.discharge(&kind, &generics, &Environment::new(), None)?;
        let scheme = self.generalize(&Environment::new(), &kind);
        let instantiated = self.instantiate(&scheme)?;
        self.operation_kinds.insert(path.clone(), scheme);
        Ok(instantiated)
    }

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
        let (receiver_name, entries) = fields
            .iter()
            .find_map(|field| match &field.value {
                OpField::Interfaces { receiver, entries } => {
                    Some((receiver.as_str(), entries.as_slice()))
                }
                _ => None,
            })
            .unwrap_or(("self", &[]));
        let Expression::Record(fields) = &mut record.value else {
            return Err(Error("operation must elaborate to a record".into()));
        };
        let annotation = expected.filter(|ty| !matches!(ty, Ty::Var(_)));
        let signature = fields
            .iter()
            .find_map(|(name, value)| (name == "signature").then_some(value))
            .ok_or_else(|| Error("operation descriptor requires a signature".into()))?;
        let Ty::Fun(operands, result) = self.operation_signature(definitions, signature, types)?
        else {
            return Err(Error("operation signature must be a function type".into()));
        };
        let operands = list_values(&operands);
        let attributes = fields.iter().find(|(name, _)| name == "attributes");
        let receiver = match attributes {
            Some((_, attributes)) => self.annotation(definitions, attributes, types)?,
            None => self.record_type(Vec::new(), Vec::new())?,
        };
        if !matches!(receiver, Ty::Con(id, _) if self.records.iter().any(|(_, record)| *record == id))
        {
            return Err(Error("operation attributes require a record type".into()));
        }
        let has_attributes = attributes.is_some();
        let mut methods_env = environment.clone();
        let mut methods_types = types.clone();
        methods_env.insert(
            vec![receiver_name.into()],
            BoundValue::mono(receiver.clone()),
        );
        methods_types.insert(vec![receiver_name.into()], None);
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
            let ty = self.interface_type(definitions, contract, &contract_types)?;
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
                "signature" => con(TYPE),
                "attributes" if annotation.is_none() => con(TYPE),
                "exec" | "requires" | "poison" => {
                    let mut parameters = operands.clone();
                    if has_attributes
                        && matches!(&value.value, Expression::Function(function)
                            if function.parameters.len() == operands.len() + 1)
                    {
                        parameters.insert(0, receiver.clone());
                    }
                    let output = if name == "exec" {
                        result.clone()
                    } else {
                        Box::new(con(BOOL))
                    };
                    let expected = Ty::Fun(type_list(parameters), output);
                    self.check_expression(definitions, value, &expected, environment, types)
                        .map_err(|error| {
                            Error(format!(
                                "operation {name} disagrees with signature: {error}"
                            ))
                        })?
                }
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
    ) -> Result<Ty, Error> {
        let Expression::Binary {
            op: BinaryOp::Arrow,
            left,
            right,
        } = &signature.value
        else {
            return Err(Error("operation signature must be a function type".into()));
        };
        let operands = match &left.value {
            Expression::Tuple(items) => items
                .iter()
                .map(|item| self.operation_slot(definitions, item, types))
                .collect::<Result<Vec<_>, _>>()?,
            Expression::Unit => Vec::new(),
            _ => vec![self.operation_slot(definitions, left, types)?],
        };
        Ok(Ty::Fun(
            type_list(operands),
            Box::new(self.operation_slot(definitions, right, types)?),
        ))
    }

    fn operation_slot(
        &mut self,
        definitions: &Definitions<'_>,
        expression: &Expr,
        types: &Types,
    ) -> Result<Ty, Error> {
        if let Expression::Tuple(items) = &expression.value {
            let slots = items
                .iter()
                .map(|item| self.operation_slot(definitions, item, types))
                .collect::<Result<Vec<_>, _>>()?;
            return Ok(Ty::Con(TUPLE, type_list(slots)));
        }
        let value = self.denotation(definitions, expression, types)?;
        let sort = self.value_sort(&value);
        let ir = |field: &str| vec!["std".into(), "ir".into(), field.into()];
        if sort == con(TYPE) || sort == con(TYPE1) {
            return Ok(value);
        }
        if self.types.get(&ir("Ty")) == Some(&Some(sort))
            && let Some(Some(Ty::Con(id, _))) = self.types.get(&ir("El"))
        {
            return Ok(Ty::Con(*id, type_list(vec![value])));
        }
        Err(Error("operation signature requires a type".into()))
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
