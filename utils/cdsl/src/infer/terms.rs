use super::*;
impl Checker {
    pub(super) fn binary(
        &mut self,
        definitions: &Definitions<'_>,
        op: BinaryOp,
        left: &Expr,
        right: &Expr,
        environment: &Environment,
        types: &Types,
    ) -> Result<Ty, Error> {
        let a = self.expression(definitions, left, environment, types)?;
        let b = self.expression(definitions, right, environment, types)?;
        match op {
            BinaryOp::Equal | BinaryOp::NotEqual => {
                let left_type = self.substitution.apply(&a);
                let right_type = self.substitution.apply(&b);
                if left_type == con(INT) || right_type == con(INT) {
                    self.check_expression(definitions, left, &con(INT), environment, types)?;
                    self.check_expression(definitions, right, &con(INT), environment, types)?;
                } else {
                    self.unify(&a, &b)?;
                }
                let resolved = self.substitution.apply(&a);
                if !self.equality_data(&resolved) {
                    return Err(Error("equality requires decidable data; functions and IR descriptions need an explicit equality interface".into()));
                }
                Ok(con(BOOL))
            }
            BinaryOp::And | BinaryOp::Or => {
                self.unify(&a, &con(BOOL))?;
                self.unify(&b, &con(BOOL))?;
                Ok(con(BOOL))
            }
            BinaryOp::Add
            | BinaryOp::Subtract
            | BinaryOp::Multiply
            | BinaryOp::Divide
            | BinaryOp::Remainder => {
                if self.substitution.apply(&a) == con(INT)
                    || self.substitution.apply(&b) == con(INT)
                {
                    self.check_expression(definitions, left, &con(INT), environment, types)?;
                    self.check_expression(definitions, right, &con(INT), environment, types)?;
                    return Ok(con(INT));
                }
                if self.bigint(&self.substitution.apply(&a))
                    || self.bigint(&self.substitution.apply(&b))
                {
                    self.unify(&a, &b)?;
                    return Ok(self.substitution.apply(&a));
                }
                self.check_expression(definitions, left, &con(NAT), environment, types)?;
                self.check_expression(definitions, right, &con(NAT), environment, types)?;
                Ok(con(NAT))
            }
            BinaryOp::Arrow => Err(Error("HM: unexpected arrow constraint".into())),
        }
    }
    fn equality_data(&self, ty: &Ty) -> bool {
        match ty {
            Ty::Con(id, _) if [NAT, INT, BOOL, STRING, TYPE, TYPE1, UNIT].contains(id) => true,
            Ty::Con(VECTOR, args) => list_values(args)
                .first()
                .is_some_and(|ty| self.equality_data(ty)),
            Ty::Con(TUPLE, args) => list_values(args).iter().all(|ty| self.equality_data(ty)),
            Ty::Con(id, args) => self
                .records
                .iter()
                .find(|(_, record)| record == id)
                .is_some_and(|(fields, _)| {
                    list_values(args)
                        .iter()
                        .take(fields.len())
                        .all(|ty| self.equality_data(ty))
                }),
            _ => false,
        }
    }
    pub(super) fn call(
        &mut self,
        definitions: &Definitions<'_>,
        value: &Expr,
        arguments: &[Expr],
        environment: &Environment,
        types: &Types,
        expected: Option<&Ty>,
    ) -> Result<Ty, Error> {
        let function = self.expression(definitions, value, environment, types)?;
        let function =
            self.resolve_computation(definitions, &self.substitution.apply(&function))?;
        let parameters = self.fresh_types(arguments.len())?;
        let result = match expected {
            Some(ty) => ty.clone(),
            None => self.fresh()?,
        };
        self.unify(
            &function,
            &Ty::Fun(type_list(parameters.clone()), Box::new(result.clone())),
        )?;
        let mut args = Vec::new();
        for (argument, expected) in arguments.iter().zip(parameters) {
            args.push(self.check_expression(
                definitions,
                argument,
                &expected,
                environment,
                types,
            )?);
        }
        let result = self.substitution.apply(&result);
        let function =
            self.resolve_computation(definitions, &self.substitution.apply(&function))?;
        self.unify(
            &function,
            &Ty::Fun(type_list(args), Box::new(result.clone())),
        )?;
        let result = self.substitution.apply(&result);
        Ok(result)
    }
}
