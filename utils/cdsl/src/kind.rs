use std::hash::{DefaultHasher, Hash, Hasher};

use crate::ast::{Binding, Expr, Expression, Literal, OpField};

/// The serialized link and the fixed-port arity of its owning declaration.
#[derive(Clone)]
pub(crate) struct OperationImport {
    pub name: String,
    pub symbol: String,
    pub ports: Option<(usize, usize)>,
}

fn hash_contract(ty: &Expr, binders: &[crate::ast::Binder], hash: &mut impl Hasher) {
    match &ty.value {
        Expression::Path(path) => {
            0u8.hash(hash);
            if let [name] = path.as_slice()
                && let Some(index) = binders.iter().position(|binder| binder.name == *name)
            {
                index.hash(hash);
            } else {
                path.hash(hash);
            }
        }
        Expression::Literal(literal) => {
            1u8.hash(hash);
            match literal {
                Literal::Natural(n) => (0u8, n).hash(hash),
                Literal::String(s) => (1u8, s).hash(hash),
                Literal::Bool(b) => (2u8, b).hash(hash),
            }
        }
        Expression::Unit => 2u8.hash(hash),
        Expression::Tuple(args) => {
            3u8.hash(hash);
            args.len().hash(hash);
            for arg in args {
                hash_contract(arg, binders, hash);
            }
        }
        Expression::Generic { value, arguments } => {
            4u8.hash(hash);
            hash_contract(value, binders, hash);
            arguments.len().hash(hash);
            for arg in arguments {
                hash_contract(arg, binders, hash);
            }
        }
        Expression::Record(fields) => {
            5u8.hash(hash);
            fields.len().hash(hash);
            for (field, value) in fields {
                field.hash(hash);
                hash_contract(value, binders, hash);
            }
        }
        _ => 6u8.hash(hash),
    }
}

impl Binding {
    pub(crate) fn operation_import(
        &self,
        module: &[String],
        source: &str,
    ) -> Option<OperationImport> {
        let name = self.operation_name(module)?;
        let signature = self.operation_fields().and_then(|fields| {
            fields.iter().find_map(|field| match &field.value {
                OpField::Signature {
                    parameters,
                    result,
                    results,
                } => Some((parameters, result, results)),
                _ => None,
            })
        });
        let mut contract = DefaultHasher::new();
        for binder in &self.generics {
            hash_contract(&binder.ty, &self.generics, &mut contract);
        }
        let ports = if let Some((parameters, result, results)) = signature {
            for parameter in parameters {
                hash_contract(&parameter.ty, &self.generics, &mut contract);
            }
            hash_contract(result, &self.generics, &mut contract);
            let outputs = if matches!(result.value, Expression::Unit) {
                0
            } else {
                results.len().max(1)
            };
            Some((parameters.len(), outputs))
        } else {
            hash_contract(&self.value, &self.generics, &mut contract);
            self.native_ports()
                .map(|(inputs, outputs)| (inputs.len(), outputs.len()))
        };
        ports.hash(&mut contract);
        let owner = [module, self.path.as_slice()].concat().join("::");
        Some(OperationImport {
            symbol: format!("@cdsl:decl:{source}:{owner}:{:016x}", contract.finish()),
            name,
            ports,
        })
    }
}
