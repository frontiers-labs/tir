use super::*;

impl Checker {
    fn indexed_term(&mut self, expression: Expr, sort: Ty, types: &Types) -> Result<Ty, Error> {
        let mut parameters = Vec::new();
        let mut arguments = Vec::new();
        for path in crate::totality::free_paths(&expression)? {
            if types.get(&path) != self.types.get(&path)
                && let Some(Some(value)) = types.get(&path)
            {
                parameters.push(path);
                arguments.push(value.clone());
            }
        }
        let key = format!("{:?}", expression.value);
        if let Some((id, _, _)) = self.indexed_terms.iter().find(|(id, term, ty)| {
            *ty == sort
                && format!("{:?}", term.value) == key
                && self.term_parameters.get(&id.0) == Some(&parameters)
        }) {
            return Ok(Ty::Con(*id, type_list(arguments)));
        }
        let id = self.constructor()?;
        self.term_parameters.insert(id.0, parameters);
        self.indexed_terms.push((id, expression, sort));
        Ok(Ty::Con(id, type_list(arguments)))
    }

    fn term_expression(&self, value: &Ty) -> Option<Expr> {
        let value = self.substitution.apply(value);
        let node = |value| Node { value, range: 0..0 };
        if let Some(value) = self.natural_value(&value) {
            return Some(node(Expression::Literal(Literal::Natural(
                value.to_string(),
            ))));
        }
        match &value {
            Ty::Fun(arguments, result) => Some(node(Expression::Binary {
                op: BinaryOp::Arrow,
                left: Box::new(node(Expression::Tuple(
                    list_values(arguments)
                        .iter()
                        .map(|argument| self.term_expression(argument))
                        .collect::<Option<_>>()?,
                ))),
                right: Box::new(self.term_expression(result)?),
            })),
            Ty::Con(id, arguments) => {
                let arguments = list_values(arguments)
                    .iter()
                    .map(|argument| self.term_expression(argument))
                    .collect::<Option<Vec<_>>>()?;
                if let Some((_, expression, _)) = self
                    .indexed_terms
                    .iter()
                    .find(|(candidate, _, _)| candidate == id)
                {
                    let parameters = self.term_parameters.get(&id.0)?;
                    let values = parameters.iter().cloned().zip(arguments).collect();
                    return crate::normalize::substitute(expression, &values).ok();
                }
                if *id == UNIT {
                    return Some(node(Expression::Unit));
                }
                if *id == TUPLE {
                    return Some(node(Expression::Tuple(arguments)));
                }
                let path = self
                    .declared
                    .iter()
                    .find(|(_, (candidate, _, _))| candidate == id)
                    .map(|(path, _)| path)
                    .or_else(|| {
                        self.types.iter().find_map(|(path, ty)| {
                            if matches!(ty, Some(Ty::Con(candidate, _)) if candidate == id) {
                                Some(path)
                            } else {
                                None
                            }
                        })
                    })?;
                let callee = node(Expression::Path(path.clone()));
                Some(if arguments.is_empty() {
                    callee
                } else {
                    node(Expression::Generic {
                        value: Box::new(callee),
                        arguments,
                    })
                })
            }
            _ => None,
        }
    }

    pub(super) fn resolve_computation(
        &mut self,
        definitions: &Definitions<'_>,
        ty: &Ty,
    ) -> Result<Ty, Error> {
        match ty {
            Ty::Con(id, arguments) => {
                let arguments = list_values(arguments)
                    .iter()
                    .map(|argument| self.resolve_computation(definitions, argument))
                    .collect::<Result<Vec<_>, _>>()?;
                let result = Ty::Con(*id, type_list(arguments.clone()));
                let Some(parameters) = self.term_parameters.get(&id.0) else {
                    return Ok(result);
                };
                if parameters.is_empty() {
                    return Ok(result);
                }
                let Some(values) = arguments
                    .iter()
                    .map(|argument| self.term_expression(argument))
                    .collect::<Option<Vec<_>>>()
                else {
                    return Ok(result);
                };
                let Some((_, expression, sort)) = self
                    .indexed_terms
                    .iter()
                    .find(|(candidate, _, _)| candidate == id)
                    .cloned()
                else {
                    return Ok(result);
                };
                let values = parameters.iter().cloned().zip(values).collect();
                let expression = crate::normalize::substitute(&expression, &values)?;
                let expression =
                    crate::normalize::head(&expression, definitions, &self.constructors)?;
                self.parameter_argument(definitions, &expression, &self.types.clone(), sort)
            }
            Ty::Fun(arguments, result) => Ok(Ty::Fun(
                type_list(
                    list_values(arguments)
                        .iter()
                        .map(|argument| self.resolve_computation(definitions, argument))
                        .collect::<Result<_, _>>()?,
                ),
                Box::new(self.resolve_computation(definitions, result)?),
            )),
            _ => Ok(ty.clone()),
        }
    }
    pub(super) fn normalize_indices(&mut self, ty: &Ty) -> Result<Ty, Error> {
        match ty {
            Ty::Con(id, args) => {
                let args = list_values(args)
                    .iter()
                    .map(|t| self.normalize_indices(t))
                    .collect::<Result<Vec<_>, _>>()?;
                if *id == SUM || *id == PRODUCT {
                    let mut constant = if *id == SUM { 0u128 } else { 1u128 };
                    let mut terms = Vec::new();
                    for term in args {
                        if let Some(value) = self.natural_value(&term) {
                            constant = if *id == SUM {
                                constant.checked_add(value)
                            } else {
                                constant.checked_mul(value)
                            }
                            .ok_or_else(|| Error("normalized index exceeds u128".into()))?;
                        } else if let Ty::Con(inner, children) = &term {
                            if inner == id {
                                terms.extend(list_values(children));
                            } else {
                                terms.push(term);
                            }
                        } else {
                            terms.push(term);
                        }
                    }
                    if *id == PRODUCT && constant == 0 {
                        return self.natural(0);
                    }
                    if terms.is_empty() {
                        return self.natural(constant);
                    }
                    if constant != if *id == SUM { 0 } else { 1 } {
                        terms.push(self.natural(constant)?);
                    }
                    terms.sort_by_key(|t| format!("{t:?}"));
                    if terms.len() == 1 {
                        return Ok(terms.remove(0));
                    }
                    return Ok(Ty::Con(*id, type_list(terms)));
                }
                Ok(Ty::Con(*id, type_list(args)))
            }
            Ty::Fun(args, result) => Ok(Ty::Fun(
                type_list(
                    list_values(args)
                        .iter()
                        .map(|t| self.normalize_indices(t))
                        .collect::<Result<_, _>>()?,
                ),
                Box::new(self.normalize_indices(result)?),
            )),
            _ => Ok(ty.clone()),
        }
    }
    fn index_term(&self, ty: &Ty) -> bool {
        if self.natural_value(ty).is_some() {
            return true;
        }
        match ty {
            Ty::Rigid(id) => matches!(self.rigid_types.get(&id.0), Some(Ty::Con(NAT, _))),
            Ty::Var(id) => matches!(self.index_types.get(&id.0), Some(Ty::Con(NAT, _))),
            Ty::Con(SUM, args) | Ty::Con(PRODUCT, args) => {
                list_values(args).iter().all(|t| self.index_term(t))
            }
            Ty::Con(id, args) if list_values(args).is_empty() => {
                self.declared.values().any(|(declared, parameters, sort)| {
                    id == declared && parameters.is_empty() && *sort == con(NAT)
                })
            }
            _ => false,
        }
    }
    pub(super) fn type_denotation(&self, ty: &Ty) -> bool {
        let sort = self.value_sort(ty);
        sort == con(TYPE) || sort == con(TYPE1)
    }
    pub(super) fn value_sort(&self, ty: &Ty) -> Ty {
        if let Ty::Con(id, _) = ty
            && let Some((_, _, sort)) = self
                .indexed_terms
                .iter()
                .find(|(candidate, _, _)| candidate == id)
        {
            return sort.clone();
        }
        if let Ty::Con(id, _) = ty
            && let Some((_, _, result)) = self
                .declared
                .values()
                .find(|(constructor, _, _)| constructor == id)
        {
            return result.clone();
        }
        let kind = match ty {
            Ty::Rigid(id) => self.rigid_types.get(&id.0).cloned(),
            Ty::Var(id) => self.index_types.get(&id.0).cloned(),
            _ => None,
        };
        if let Some(kind) = kind {
            return kind;
        }
        if self.index_term(ty) {
            return con(NAT);
        }
        if self.universe_level(ty) == 0 {
            con(TYPE)
        } else {
            con(TYPE1)
        }
    }
    fn value_environment(&self, types: &Types) -> Environment {
        types
            .iter()
            .filter_map(|(path, ty)| {
                ty.as_ref().map(|value| {
                    let scheme = if self.types.get(path) == Some(ty) {
                        self.environment.get(path).cloned()
                    } else {
                        None
                    };
                    (
                        path.clone(),
                        scheme.unwrap_or_else(|| BoundValue::mono(self.value_sort(value))),
                    )
                })
            })
            .collect()
    }
    fn require_known_values(&self, expression: &Expr, types: &Types) -> Result<(), Error> {
        for path in crate::totality::free_paths(expression)? {
            if types.get(&path) == Some(&None) && self.types.get(&path) != Some(&None) {
                return Err(Error("type computation depends on a neutral value; explicit equality evidence is required".into()));
            }
        }
        Ok(())
    }
    pub(super) fn normalize_expression(
        &self,
        definitions: &Definitions<'_>,
        expr: &Expr,
        types: &Types,
        environment: Option<&Environment>,
    ) -> Result<Expr, Error> {
        let mut definitions = definitions.clone();
        let mut values = BTreeMap::new();
        for (path, value) in types {
            if self.types.get(path) != Some(value) {
                definitions.remove(path);
                if let Some(value) = value
                    && let Some(expression) = self.term_expression(value)
                {
                    values.insert(path.clone(), expression);
                }
            }
        }
        if let Some(environment) = environment {
            for path in environment.keys() {
                definitions.remove(path);
            }
        }
        let expr = crate::normalize::substitute(expr, &values)?;
        crate::normalize::head(&expr, &definitions, &self.constructors)
    }
    pub(super) fn annotation(
        &mut self,
        definitions: &Definitions<'_>,
        expression: &Expr,
        types: &Types,
    ) -> Result<Ty, Error> {
        let value = self.denotation(definitions, expression, types)?;
        let sort = self.value_sort(&value);
        if sort != con(TYPE) && sort != con(TYPE1) {
            return Err(Error(
                "annotation requires Type; use El<T> for a concrete IR value type".into(),
            ));
        }
        Ok(value)
    }
    pub(super) fn denotation(
        &mut self,
        definitions: &Definitions<'_>,
        expression: &Expr,
        types: &Types,
    ) -> Result<Ty, Error> {
        match &expression.value {
            Expression::Path(path) => {
                if !self.types.contains_key(path)
                    && let Some((generics, annotation)) = self.axioms.get(path).cloned()
                {
                    self.axiom(definitions, path, &generics, &annotation)?;
                }

                let ty =
                    if let Some(ty) = lookup_type(types, &self.types, path) {
                        ty.ok_or_else(|| {
                            Error(format!("HM: {} is a value, not a type", path.join("::")))
                        })?
                    } else {
                        if self.environment.contains_key(path) && definitions.contains_key(path) {
                            return Err(Error(format!(
                                "HM: recursive or non-type alias {}",
                                path.join("::")
                            )));
                        }
                        self.definition(definitions, path)?;
                        self.types.get(path).cloned().flatten().ok_or_else(|| {
                            Error(format!("HM: {} is not a type", path.join("::")))
                        })?
                    };
                if self.types.get(path) == Some(&Some(ty.clone()))
                    && self.arities.get(path).is_some_and(|arity| *arity > 0)
                {
                    return Err(Error("HM: type constructor requires arguments".into()));
                }
                if types.contains_key(path) && types.get(path) != self.types.get(path) {
                    return Ok(ty);
                }
                let quantified = hm::free_variables(&ty)
                    .into_iter()
                    .filter(|id| self.dependent_variables.contains(id))
                    .collect::<Vec<_>>();
                if quantified.is_empty() {
                    Ok(ty)
                } else {
                    self.instantiate(&Scheme { quantified, ty })
                }
            }
            Expression::Generic { value, arguments } => {
                let Expression::Path(path) = &value.value else {
                    return Err(Error("HM: unsupported type application".into()));
                };
                self.constructor_annotation(definitions, path, arguments, types)
            }
            Expression::Unit => Ok(con(UNIT)),
            Expression::Tuple(items) => {
                let args = self.annotations(definitions, items, types)?;
                Ok(Ty::Con(TUPLE, type_list(args)))
            }
            Expression::Record(fields) => {
                let mut local = types.clone();
                let mut compiled = Vec::new();
                let mut dependencies = Vec::new();
                let mut seen = BTreeSet::new();
                for (index, (name, value)) in fields.iter().enumerate() {
                    if !seen.insert(name.clone()) {
                        return Err(Error("duplicate record field".into()));
                    }
                    let ty = self.annotation(definitions, value, &local)?;
                    let mut used = false;
                    for (_, later) in &fields[index + 1..] {
                        crate::totality::visit(later, &mut |e| {
                            if matches!(&e.value,Expression::Path(p) if p==&vec![name.clone()]) {
                                used = true;
                            }
                            Ok(())
                        })?;
                    }
                    if used {
                        let kind = ty.clone();
                        let variable = self.fresh()?;
                        if let Ty::Var(id) = variable {
                            self.index_types.insert(id.0, kind.clone());
                            self.dependent_variables.insert(id);
                        }
                        local.insert(vec![name.clone()], Some(variable.clone()));
                        dependencies.push((name.clone(), kind, variable));
                    }
                    compiled.push((name.clone(), ty));
                }
                self.record_type(compiled, dependencies)
            }
            Expression::Binary {
                op: BinaryOp::Arrow,
                left,
                right,
            } => {
                let arguments = match &left.value {
                    Expression::Tuple(items) => self.annotations(definitions, items, types)?,
                    Expression::Unit => Vec::new(),
                    _ => vec![self.annotation(definitions, left, types)?],
                };
                Ok(Ty::Fun(
                    type_list(arguments),
                    Box::new(self.annotation(definitions, right, types)?),
                ))
            }
            Expression::Call { .. }
            | Expression::Block(_)
            | Expression::If { .. }
            | Expression::Match { .. } => {
                self.require_known_values(expression, types)?;
                let environment = self.value_environment(types);
                let result = self.expression(definitions, expression, &environment, types)?;
                self.unify(&result, &con(TYPE))?;
                let reduced = self.normalize_expression(definitions, expression, types, None)?;
                if format!("{:?}", reduced.value) == format!("{:?}", expression.value) {
                    return self.indexed_term(expression.clone(), con(TYPE), types);
                }
                if matches!(&reduced.value, Expression::If { condition, .. } if !matches!(condition.value, Expression::Literal(Literal::Bool(_))))
                {
                    return self.indexed_term(expression.clone(), con(TYPE), types);
                }
                self.annotation(definitions, &reduced, types)
            }
            _ => Err(Error(
                "type computation is neutral; supply explicit equality evidence".into(),
            )),
        }
    }
    pub(super) fn natural(&mut self, value: u128) -> Result<Ty, Error> {
        let id = if let Some(id) = self.naturals.get(&value) {
            *id
        } else {
            let id = self.constructor()?;
            self.naturals.insert(value, id);
            id
        };
        Ok(con(id))
    }
    pub(super) fn natural_value(&self, ty: &Ty) -> Option<u128> {
        self.naturals
            .iter()
            .find_map(|(n, id)| if con(*id) == *ty { Some(*n) } else { None })
    }
    pub(super) fn parameter_argument(
        &mut self,
        definitions: &Definitions<'_>,
        argument: &Expr,
        types: &Types,
        kind: Ty,
    ) -> Result<Ty, Error> {
        match kind {
            Ty::Con(NAT, _) => {
                let value = self.index(definitions, argument, types)?;
                Ok(value)
            }
            Ty::Con(TYPE, _) => {
                let ty = self.annotation(definitions, argument, types)?;
                if self.universe_level(&ty) > 0 {
                    return Err(Error("universe mismatch in small Type parameter".into()));
                }
                Ok(ty)
            }
            sort => {
                if let Ok(value) = self.denotation(definitions, argument, types)
                    && self.unify(&self.value_sort(&value), &sort).is_ok()
                {
                    return Ok(value);
                }
                let environment = self.value_environment(types);
                self.check_expression(definitions, argument, &sort, &environment, types)?;
                let argument = self.normalize_expression(definitions, argument, types, None)?;
                self.indexed_term(argument, sort, types)
            }
        }
    }
    pub(super) fn index(
        &mut self,
        definitions: &Definitions<'_>,
        expr: &Expr,
        types: &Types,
    ) -> Result<Ty, Error> {
        match &expr.value {
            Expression::Literal(Literal::Natural(n))=>self.natural(n.replace('_',"").parse().map_err(|_|Error("natural exceeds supported u128 representation".into()))?),
            Expression::Path(path)=> {
                if !types.contains_key(path)
                    && !self.types.contains_key(path)
                    && let Some((generics, annotation)) = self.axioms.get(path).cloned()
                {
                    self.axiom(definitions, path, &generics, &annotation)?;
                }
                if let Some(binding)=lookup_type(types,&self.types,path) {
                    if let Some(ty)=binding && self.index_term(&ty) {return Ok(ty);}
                    return Err(Error("neutral value needs explicit index/normalization evidence".into()));
                }
                if let Some(binding)=definitions.get(path) && binding.generics.is_empty() {return self.index(definitions,&binding.value,types);}
                Err(Error("expected a natural-number index".into()))
            }
            Expression::Binary {op,left,right}=> {
                let a=self.index(definitions,left,types)?;let b=self.index(definitions,right,types)?;
                if let (Some(a),Some(b))=(self.natural_value(&a),self.natural_value(&b)) {
                    let value=match op {BinaryOp::Add=>a.checked_add(b),BinaryOp::Subtract=>Some(a.saturating_sub(b)),BinaryOp::Multiply=>a.checked_mul(b),BinaryOp::Divide=>Some(a.checked_div(b).unwrap_or(0)),BinaryOp::Remainder=>Some(a.checked_rem(b).unwrap_or(a)),_=>None}.ok_or_else(||Error("index arithmetic needs a defined, representable result".into()))?;
                    return self.natural(value);
                }
                let operator=match op {BinaryOp::Add=>SUM,BinaryOp::Multiply=>PRODUCT,_=>return Err(Error("symbolic index equation requires explicit evidence".into()))};
                let identity=if operator==SUM {0}else{1};
                if self.natural_value(&a)==Some(identity) {return Ok(b);}
                if self.natural_value(&b)==Some(identity) {return Ok(a);}
                let mut terms=Vec::new();
                for term in [a,b] {if let Ty::Con(id,children)=&term&& *id==operator {terms.extend(list_values(children));continue;}terms.push(term);}
                terms.sort_by_key(|t|format!("{t:?}"));
                Ok(Ty::Con(operator,type_list(terms)))
            }
            Expression::Call {..}|Expression::Block(_)|Expression::If {..}|Expression::Match {..}=> {
                self.require_known_values(expr,types)?;let environment=self.value_environment(types);
                let ty=self.expression(definitions,expr,&environment,types)?;
                self.unify(&ty,&con(NAT))?;
                let reduced=self.normalize_expression(definitions,expr,types,None)?;
                if format!("{:?}",reduced.value)==format!("{:?}",expr.value) {return Err(Error("index computation is neutral; explicit equality evidence is required".into()));}
                self.index(definitions,&reduced,types)
            }
            _=>Err(Error("index computation requires a normalizable natural expression; unresolved equations need evidence".into())),
        }
    }
    pub(super) fn constructor_annotation(
        &mut self,
        definitions: &Definitions<'_>,
        path: &Path,
        arguments: &[Expr],
        types: &Types,
    ) -> Result<Ty, Error> {
        if !self.declared.contains_key(path)
            && let Some((generics, annotation)) = self.axioms.get(path).cloned()
        {
            self.axiom(definitions, path, &generics, &annotation)?;
        }
        if let Some((id, parameters, _)) = self.declared.get(path).cloned() {
            if parameters.len() != arguments.len() {
                return Err(Error("type constructor arity mismatch".into()));
            }
            let mut substitution = Substitution::default();
            let indices = self.explicit.get(path).cloned().unwrap_or_default();
            let mut args = Vec::new();
            for (position, (sort, argument)) in parameters.iter().zip(arguments).enumerate() {
                let sort = substitution.apply(sort);
                let arg = self.parameter_argument(definitions, argument, types, sort)?;
                if let Some(Ty::Var(id)) = indices.get(position) {
                    substitution.bindings.push((*id, arg.clone()));
                }
                args.push(arg);
            }
            return Ok(Ty::Con(id, type_list(args)));
        }
        if let Some(binding) = definitions.get(path)
            && !binding.generics.is_empty()
        {
            self.definition(definitions, path)?;
            if binding.generics.len() != arguments.len() {
                return Err(Error("generic alias arity mismatch".into()));
            }
            let mut values = BTreeMap::new();
            for (binder, argument) in binding.generics.iter().zip(arguments) {
                let kind = self.parameter_type(definitions, binder, types)?;
                self.parameter_argument(definitions, argument, types, kind)?;
                values.insert(vec![binder.name.clone()], argument.clone());
            }
            let body = crate::normalize::substitute(&binding.value, &values)?;
            return self.denotation(definitions, &body, types);
        }
        let constructor = lookup_type(types, &self.types, path);
        let Some(Some(Ty::Con(id, ref parameters))) = constructor else {
            return Err(Error("HM: expected a type constructor".into()));
        };
        if constructor.as_ref() != self.types.get(path)
            || self.arities.get(path) != Some(&arguments.len())
            || list_values(parameters).len() != arguments.len()
        {
            return Err(Error("HM: type constructor arity mismatch".into()));
        }
        let mut args = Vec::new();
        let mut supplied = Substitution::default();
        for (parameter, argument) in list_values(parameters).iter().zip(arguments) {
            let kind = match parameter {
                Ty::Var(v) => self.index_types.get(&v.0).cloned().unwrap_or(con(TYPE)),
                _ => con(TYPE),
            };
            let argument =
                self.parameter_argument(definitions, argument, types, supplied.apply(&kind))?;
            if let Ty::Var(id) = parameter {
                supplied.bindings.push((*id, argument.clone()));
            }
            args.push(argument);
        }
        Ok(Ty::Con(id, type_list(args)))
    }
    pub(super) fn record_type(
        &mut self,
        mut fields: Vec<(String, Ty)>,
        mut dependencies: Vec<(String, Ty, Ty)>,
    ) -> Result<Ty, Error> {
        fields.sort_by(|a, b| a.0.cmp(&b.0));
        dependencies.sort_by(|a, b| a.0.cmp(&b.0));
        if fields.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Error("duplicate schema record field".into()));
        }
        let names = fields.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        let mut shape = Vec::new();
        for (name, kind, _) in &dependencies {
            let index = names
                .iter()
                .position(|n| n == name)
                .ok_or_else(|| Error("dependent field is absent from its record".into()))?;
            shape.push((index, kind.clone()));
        }
        let id = if shape.is_empty() {
            self.record_id(names.clone())?
        } else {
            let existing = self
                .records
                .iter()
                .find(|(other, id)| {
                    *other == names && self.dependent_records.get(&id.0) == Some(&shape)
                })
                .map(|(_, id)| *id);
            if let Some(id) = existing {
                id
            } else {
                let id = self.constructor()?;
                self.records.push((names, id));
                self.dependent_records.insert(id.0, shape);
                id
            }
        };
        let mut args = fields.into_iter().map(|(_, ty)| ty).collect::<Vec<_>>();
        args.extend(dependencies.into_iter().map(|(_, _, index)| index));
        Ok(Ty::Con(id, type_list(args)))
    }
    pub(super) fn record_id(&mut self, names: Vec<String>) -> Result<ConId, Error> {
        let mut index = 0;
        let found = loop {
            if index == self.records.len() {
                break None;
            }
            let (fields, id) = &self.records[index];
            if fields == &names && !self.dependent_records.contains_key(&id.0) {
                break Some(*id);
            }
            index += 1;
        };
        if let Some(id) = found {
            return Ok(id);
        }
        let id = self.constructor()?;
        self.records.push((names, id));
        Ok(id)
    }
    pub(super) fn universe_level(&self, ty: &Ty) -> u32 {
        match ty {
            Ty::Con(TYPE, _) => 1,
            Ty::Con(TYPE1, _) => 2,
            Ty::Con(_, args) => list_values(args)
                .iter()
                .map(|t| self.universe_level(t))
                .max()
                .unwrap_or(0),
            Ty::Fun(args, result) => list_values(args)
                .iter()
                .map(|t| self.universe_level(t))
                .chain([self.universe_level(result)])
                .max()
                .unwrap_or(0),
            _ => 0,
        }
    }
    pub(super) fn syntactic_type(
        &self,
        expr: &Expr,
        definitions: &Definitions<'_>,
        types: &Types,
    ) -> bool {
        match &expr.value {
            Expression::Path(path) => {
                lookup_type(types, &self.types, path)
                    .flatten()
                    .is_some_and(|ty| self.type_denotation(&ty))
                    || self.interfaces.contains(path)
                    || definitions.get(path).is_some_and(|b| {
                        b.ty.as_ref().is_some_and(
                        |t| matches!(&t.value,Expression::Path(p) if p==&vec!["Type".to_owned()]),
                    )
                    })
            }
            Expression::Generic { value, .. } => {
                matches!(&value.value,Expression::Path(path) if
                    (self.arities.contains_key(path) && !self.explicit.contains_key(path))
                    || self.interfaces.contains(path)
                    || self.axioms.get(path).is_some_and(|(_, annotation)| type_family(annotation))
                    || definitions.get(path).is_some_and(|binding| matches!(binding.ty.as_ref().map(|e| &e.value), Some(Expression::Path(name)) if name == &vec!["Type".to_owned()])))
            }
            Expression::Record(fields) => {
                !fields.is_empty()
                    && fields
                        .iter()
                        .all(|(_, e)| self.syntactic_type(e, definitions, types))
            }
            Expression::Tuple(items) => {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|e| self.syntactic_type(e, definitions, types))
            }
            Expression::Binary {
                op: BinaryOp::Arrow,
                ..
            } => true,
            _ => false,
        }
    }
}
