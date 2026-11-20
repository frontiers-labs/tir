use super::*;

impl Checker {
    pub(super) fn covered(
        &mut self,
        mut inputs: Vec<Ty>,
        mut rows: Vec<Vec<Pattern>>,
    ) -> Result<bool, Error> {
        if rows.is_empty() {
            return Ok(false);
        }
        if inputs.is_empty() {
            return Ok(true);
        }
        if rows.iter().any(|row| row.iter().all(catch_all)) {
            return Ok(true);
        }
        let column = (0..inputs.len())
            .find(|index| rows.iter().any(|row| !catch_all(&row[*index])))
            .unwrap_or(0);
        inputs.swap(0, column);
        for row in &mut rows {
            row.swap(0, column);
        }
        let input = self.substitution.apply(&inputs[0]);
        let mut cases: Vec<(String, Vec<Ty>)> = Vec::new();
        match &input {
            Ty::Con(id, _) if *id == BOOL => {
                cases.push(("true".into(), Vec::new()));
                cases.push(("false".into(), Vec::new()));
            }
            Ty::Con(id, args) if *id == TUPLE => cases.push(("tuple".into(), list_values(args))),
            Ty::Con(id, _) if *id == UNIT => cases.push(("unit".into(), Vec::new())),
            Ty::Con(id, _) => {
                for path in self.constructors.clone() {
                    let Some(scheme) = self.environment.get(&path).cloned() else {
                        continue;
                    };
                    let ctor = self.instantiate(&scheme)?;
                    let (args, result) = match ctor {
                        Ty::Fun(args, result) => (list_values(&args), *result),
                        ty => (Vec::new(), ty),
                    };
                    if matches!(&result,Ty::Con(family,_) if family==id) {
                        let subst = hm::unify(&result, &input).map_err(kernel_error)?;
                        cases.push((
                            path.join("::"),
                            args.iter().map(|arg| subst.apply(arg)).collect(),
                        ));
                    }
                }
            }
            _ => {}
        }
        if cases.is_empty() {
            let defaults = rows
                .into_iter()
                .filter(|row| catch_all(&row[0]))
                .map(|row| row.into_iter().skip(1).collect())
                .collect();
            return self.covered(inputs.into_iter().skip(1).collect(), defaults);
        }
        for (constructor, args) in cases {
            let mut specialized = Vec::new();
            for row in &rows {
                let head = &row[0];
                let fields = if catch_all(head) {
                    Some(
                        (0..args.len())
                            .map(|_| Node {
                                value: PatternKind::Wildcard,
                                range: head.range.clone(),
                            })
                            .collect(),
                    )
                } else {
                    match &head.value {
                        PatternKind::Constructor { path, arguments }
                            if path.join("::") == constructor =>
                        {
                            Some(arguments.clone())
                        }
                        PatternKind::Path(path) if path.join("::") == constructor => {
                            Some(Vec::new())
                        }
                        PatternKind::Literal(Literal::Bool(value))
                            if value.to_string() == constructor =>
                        {
                            Some(Vec::new())
                        }
                        PatternKind::Tuple(items) if constructor == "tuple" => Some(items.clone()),
                        PatternKind::Unit if constructor == "unit" => Some(Vec::new()),
                        _ => None,
                    }
                };
                if let Some(mut fields) = fields {
                    fields.extend_from_slice(&row[1..]);
                    specialized.push(fields);
                }
            }
            let mut next = args;
            next.extend_from_slice(&inputs[1..]);
            if !self.covered(next, specialized)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    fn pattern_equation(&mut self, left: &Ty, right: &Ty) -> Result<(), Error> {
        self.unify(left,right).map_err(|error| {
            if any_rigid(left)||any_rigid(right)||matches!(right,Ty::Con(_,args) if list_values(args).iter().any(|ty|matches!(ty,Ty::Con(SUM,_)|Ty::Con(PRODUCT,_)))) {Error(format!("indexed pattern requires explicit equality evidence: {error}"))}else{error}
        })
    }
    pub(super) fn pattern(
        &mut self,
        definitions: &Definitions<'_>,
        pattern: &Pattern,
        expected: &Ty,
        environment: &mut Environment,
        bound: &mut Vec<String>,
    ) -> Result<(), Error> {
        match &pattern.value {
            PatternKind::Wildcard => Ok(()),
            PatternKind::Literal(value) => self.pattern_equation(expected, &literal_type(value)),
            PatternKind::Unit => self.pattern_equation(expected, &con(UNIT)),
            PatternKind::Path(path) if path.len() == 1 => {
                if bound.contains(&path[0]) {
                    return Err(Error("HM: duplicate pattern binder".into()));
                }
                bound.push(path[0].clone());
                environment.insert(path.clone(), BoundValue::mono(expected.clone()));
                Ok(())
            }
            PatternKind::Path(path) => {
                if !self.constructors.contains(path) {
                    return Err(Error("HM: pattern requires an enum constructor".into()));
                }
                let ty = self.lookup(definitions, path, environment)?;
                if matches!(ty, Ty::Fun(_, _)) {
                    return Err(Error("HM: constructor pattern requires arguments".into()));
                }
                self.pattern_equation(expected, &ty)
            }
            PatternKind::Constructor { path, arguments } => {
                if !self.constructors.contains(path) {
                    return Err(Error("HM: pattern requires an enum constructor".into()));
                }
                let constructor = self.lookup(definitions, path, environment)?;
                let args = self.fresh_types(arguments.len())?;
                self.pattern_equation(
                    &constructor,
                    &Ty::Fun(type_list(args.clone()), Box::new(expected.clone())),
                )?;
                let mut outcome = Ok(());
                for (pattern, ty) in arguments.iter().zip(args) {
                    outcome = self.pattern(definitions, pattern, &ty, environment, bound);
                    if outcome.is_err() {
                        break;
                    }
                }
                outcome?;
                Ok(())
            }
            PatternKind::Tuple(items) => {
                let args = self.fresh_types(items.len())?;
                self.pattern_equation(expected, &Ty::Con(TUPLE, type_list(args.clone())))?;
                let mut outcome = Ok(());
                for (pattern, ty) in items.iter().zip(args) {
                    outcome = self.pattern(definitions, pattern, &ty, environment, bound);
                    if outcome.is_err() {
                        break;
                    }
                }
                outcome?;
                Ok(())
            }
        }
    }
    pub(super) fn inductive_occurrences(&mut self, owner: ConId, ty: &Ty, positive: bool) {
        match ty {
            Ty::Fun(args, result) => {
                for argument in list_values(args) {
                    self.inductive_occurrences(owner, &argument, false);
                }
                self.inductive_occurrences(owner, result, positive);
            }
            Ty::Con(id, args) => {
                if self.inductive_ids.contains(&id.0) {
                    self.inductive_edges.push((owner.0, id.0, positive));
                }
                let covariant = [TUPLE, VECTOR].contains(id)
                    || self.records.iter().any(|(_, record)| record == id);
                for argument in list_values(args) {
                    self.inductive_occurrences(owner, &argument, positive && covariant);
                }
            }
            _ => {}
        }
    }
    pub(super) fn check_inductive_cycles(&self) -> Result<(), Error> {
        let mut reach = self
            .inductive_edges
            .iter()
            .map(|(from, to, _)| (*from, *to))
            .collect::<BTreeSet<_>>();
        loop {
            let mut next = reach.clone();
            for (from, middle) in &reach {
                for (start, to) in &reach {
                    if middle == start {
                        next.insert((*from, *to));
                    }
                }
            }
            if next == reach {
                break;
            }
            reach = next;
        }
        if self
            .inductive_edges
            .iter()
            .any(|(from, to, positive)| !positive && (from == to || reach.contains(&(*to, *from))))
        {
            return Err(Error(
                "mutually recursive inductive occurrence is not strictly positive".into(),
            ));
        }
        Ok(())
    }
}
