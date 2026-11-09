use std::collections::BTreeMap;

use syn::Expr;
use tir_cdsl::core::{Pattern, Term, TermKind};
use tir_cdsl::output::{CheckedOperation, Sem};
use tir_cdsl::types::Type;
use tir_cdsl::{CheckedModule, Value};
use tir_symbolic::lang::SymKind;

use crate::sem_recipe::Recipe;

#[derive(Clone)]
enum Argument {
    Node(u32),
    Attributes,
    Capture(Value),
}
type Locals = BTreeMap<String, Argument>;
type Types = BTreeMap<String, Type>;
type Rows = BTreeMap<u32, Type>;

pub fn lower(
    module: &CheckedModule,
    value: &Value,
    operation: &CheckedOperation<'_>,
    defaults: &BTreeMap<String, bool>,
    hosts: &BTreeMap<String, Expr>,
) -> Result<Sem, String> {
    let mut lower = Lower {
        module,
        operation,
        defaults,
        hosts,
        recipe: Recipe::new(),
        splices: Vec::new(),
        depth: 0,
    };
    if let Value::Foreign { identity, .. } = value {
        let root = lower.host(identity)?;
        return Ok(Sem {
            recipe: lower.recipe.finish(root),
            splices: lower.splices,
        });
    }
    let mut arguments = Vec::new();
    if !operation.environment.is_empty() {
        return Err("semantic environment lowering is not available".into());
    }
    if !operation.attributes.is_empty() {
        arguments.push(Argument::Attributes);
    }
    for index in 0..operation.operands.len() {
        let node = lower.recipe.operand(index as u32);
        arguments.push(Argument::Node(node));
    }
    let root = lower.function(value, arguments)?;
    Ok(Sem {
        recipe: lower.recipe.finish(root),
        splices: lower.splices,
    })
}
struct Lower<'a> {
    module: &'a CheckedModule,
    operation: &'a CheckedOperation<'a>,
    defaults: &'a BTreeMap<String, bool>,
    hosts: &'a BTreeMap<String, Expr>,
    recipe: Recipe,
    splices: Vec<String>,
    depth: usize,
}
impl Lower<'_> {
    fn function(&mut self, function: &Value, arguments: Vec<Argument>) -> Result<u32, String> {
        match function {
            Value::Function(function) => {
                if function.parameters().len() != arguments.len() {
                    return Err("checked semantic function has an unexpected arity".into());
                }
                let mut locals = function
                    .captures()
                    .iter()
                    .map(|(name, value)| (name.clone(), Argument::Capture(value.clone())))
                    .collect::<Locals>();
                locals.extend(function.parameters().iter().cloned().zip(arguments));
                self.node(
                    function.code(),
                    &locals,
                    function.type_arguments(),
                    &Rows::new(),
                )
            }
            Value::Foreign { .. } => Err(
                "opaque semantic models must be selected at the compiled-function boundary".into(),
            ),
            _ => Err("semantic body must be a checked function or typed host model".into()),
        }
    }
    fn host(&mut self, identity: &str) -> Result<u32, String> {
        let name = identity.strip_prefix("main::").unwrap_or(identity);
        if !self.hosts.contains_key(name) {
            return Err(format!("missing typed Rust semantic host binding `{name}`"));
        }
        if !self.splices.iter().any(|existing| existing == name) {
            self.splices.push(name.into());
        }
        Ok(self.recipe.splice(name))
    }
    fn ty(&self, term: &Term, types: &Types, rows: &Rows) -> Type {
        term.ty.substitute(types).substitute_variables(rows)
    }
    fn node(
        &mut self,
        term: &Term,
        locals: &Locals,
        types: &Types,
        rows: &Rows,
    ) -> Result<u32, String> {
        match self.value(term, locals, types, rows)? {
            Argument::Node(node) => Ok(node),
            Argument::Capture(value) => self.literal(&value),
            Argument::Attributes => Err("attribute receiver cannot be a semantic result".into()),
        }
    }
    fn literal(&mut self, value: &Value) -> Result<u32, String> {
        match value {
            Value::Bool(value) => Ok(self.recipe.integer(1, i64::from(*value), false)),
            Value::Integer(value) => Ok(self.recipe.integer(
                64,
                i64::try_from(*value)
                    .map_err(|_| "semantic integer exceeds the supported model".to_string())?,
                true,
            )),
            _ => Err("captured semantic value has no supported compiled representation".into()),
        }
    }
    fn value(
        &mut self,
        term: &Term,
        locals: &Locals,
        types: &Types,
        rows: &Rows,
    ) -> Result<Argument, String> {
        if self.depth >= 128 {
            return Err("semantic output nesting exceeds backend capacity".into());
        }
        self.depth += 1;
        let result = self.value_inner(term, locals, types, rows);
        self.depth -= 1;
        result
    }
    fn value_inner(
        &mut self,
        term: &Term,
        locals: &Locals,
        types: &Types,
        rows: &Rows,
    ) -> Result<Argument, String> {
        let node = match &term.kind {
            TermKind::Literal(literal) => return self.literal_term(literal),
            TermKind::Local(name) => {
                return locals
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("unbound checked semantic local `{name}`"));
            }
            TermKind::Project(record, field) => match self.value(record, locals, types, rows)? {
                Argument::Attributes => {
                    if self.operation.attributes.get(field) != Some(&Type::Bool) {
                        return Err(format!(
                            "attribute `{field}` needs a supported semantic codec"
                        ));
                    }
                    self.recipe
                        .bool_attribute(field, *self.defaults.get(field).unwrap_or(&false))
                }
                _ => return Err("semantic projection needs the checked attribute receiver".into()),
            },
            TermKind::Call(function, arguments) => {
                let values = arguments
                    .iter()
                    .map(|argument| self.value(argument, locals, types, rows))
                    .collect::<Result<Vec<_>, _>>()?;
                self.call(function, values, locals, types, rows)?
            }
            TermKind::Let(Pattern::Bind(name), value, tail) => {
                let mut locals = locals.clone();
                let value = self.value(value, &locals, types, rows)?;
                locals.insert(name.clone(), value);
                return self.value(tail, &locals, types, rows);
            }
            TermKind::If(condition, yes, no) => {
                let condition = self.node(condition, locals, types, rows)?;
                let yes = self.node(yes, locals, types, rows)?;
                let no = self.node(no, locals, types, rows)?;
                self.recipe.apply(SymKind::If, &[condition, yes, no])
            }
            TermKind::Unary(op, value)
                if op == "!" && self.ty(value, types, rows) == Type::Bool =>
            {
                let value = self.node(value, locals, types, rows)?;
                self.recipe.apply(SymKind::Not, &[value])
            }
            TermKind::Binary(op, left, right, _) if self.ty(left, types, rows) == Type::Bool => {
                let left = self.node(left, locals, types, rows)?;
                let right = self.node(right, locals, types, rows)?;
                let kind = match op.as_str() {
                    "&&" | "&" => SymKind::And,
                    "||" | "|" => SymKind::Or,
                    "^" => SymKind::Xor,
                    "==" => SymKind::Eq,
                    "!=" => SymKind::Ne,
                    _ => return Err(format!("unsupported checked Boolean operator `{op}`")),
                };
                self.recipe.apply(kind, &[left, right])
            }
            _ => {
                return Err(format!(
                    "checked semantic construct at {:?} is unsupported by this output backend",
                    term.span
                ));
            }
        };
        Ok(Argument::Node(node))
    }
    fn literal_term(&mut self, literal: &tir_cdsl::ast::Literal) -> Result<Argument, String> {
        let value = match literal {
            tir_cdsl::ast::Literal::Bool(value) => Value::Bool(*value),
            tir_cdsl::ast::Literal::Integer(value) => Value::Integer(*value),
            _ => return Err("semantic strings are not executable models".into()),
        };
        self.literal(&value).map(Argument::Node)
    }
    fn call(
        &mut self,
        function: &Term,
        arguments: Vec<Argument>,
        locals: &Locals,
        types: &Types,
        rows: &Rows,
    ) -> Result<u32, String> {
        match &function.kind {
            TermKind::Global(name, parameters, variables) => {
                let definition = self
                    .module
                    .definition(name)
                    .ok_or_else(|| format!("checked semantic reference `{name}` missing"))?;
                let parameters = parameters
                    .iter()
                    .map(|(name, ty)| {
                        (
                            name.clone(),
                            ty.substitute(types).substitute_variables(rows),
                        )
                    })
                    .collect::<Types>();
                let variables = variables
                    .iter()
                    .map(|(name, ty)| (*name, ty.substitute(types).substitute_variables(rows)))
                    .collect::<Rows>();
                if let TermKind::Foreign(identity) = &definition.term.kind {
                    let signature = self.ty(function, types, rows);
                    return self.model(identity, &signature, arguments);
                }
                let TermKind::Function(names, body) = &definition.term.kind else {
                    return Err("semantic callable is not a checked function".into());
                };
                if names.len() != arguments.len() {
                    return Err("checked helper arity mismatch".into());
                }
                let locals = names.iter().cloned().zip(arguments).collect();
                self.node(body, &locals, &parameters, &variables)
            }
            TermKind::Local(name) => match locals.get(name) {
                Some(Argument::Capture(value)) => self.function(value, arguments),
                _ => Err("unsupported symbolic function argument".into()),
            },
            TermKind::Function(names, body) => {
                let mut locals = locals.clone();
                locals.extend(names.iter().cloned().zip(arguments));
                self.node(body, &locals, types, rows)
            }
            TermKind::Foreign(identity) => {
                self.model(identity, &self.ty(function, types, rows), arguments)
            }
            _ => Err("semantic call needs checked code or an explicit typed host model".into()),
        }
    }
    fn model(
        &mut self,
        identity: &str,
        signature: &Type,
        arguments: Vec<Argument>,
    ) -> Result<u32, String> {
        let name = identity.strip_prefix("main::").unwrap_or(identity);
        let Type::Function(parameters, result) = signature else {
            return Err("semantic host model must have a function type".into());
        };
        let integer =
            |ty: &Type| matches!(ty,Type::Nominal(name,args) if name=="ops::Int"&&args.len()==1);
        let ints = parameters.len() == 2
            && integer(&parameters[0].1)
            && parameters[0].1 == parameters[1].1;
        let boolean = parameters.len() == 2
            && parameters.iter().all(|(_, ty)| *ty == Type::Bool)
            && result.as_ref() == &Type::Bool;
        let kind = match name {
            "ops::add" if ints && result.as_ref() == &parameters[0].1 => SymKind::Add,
            "ops::bit_and" if ints && result.as_ref() == &parameters[0].1 => SymKind::And,
            "ops::bit_xor" if ints && result.as_ref() == &parameters[0].1 => SymKind::Xor,
            "ops::udiv" if ints && result.as_ref() == &parameters[0].1 => SymKind::UDiv,
            "ops::slt" if ints && result.as_ref() == &Type::Bool => SymKind::Lt,
            "ops::ult" if ints && result.as_ref() == &Type::Bool => SymKind::ULt,
            "ops::ne" if ints && result.as_ref() == &Type::Bool => SymKind::Ne,
            "ops::bool_and" if boolean => SymKind::And,
            "ops::bool_or" if boolean => SymKind::Or,
            "ops::zero" if parameters.is_empty() && integer(result) => return self.host(identity),
            _ => {
                return Err(format!(
                    "unsupported or ill-typed semantic model `{name}`: {signature}"
                ));
            }
        };
        let arguments = arguments
            .into_iter()
            .map(|argument| match argument {
                Argument::Node(node) => Ok(node),
                _ => Err("semantic model expects value operands".to_string()),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if arguments.len() != parameters.len() {
            return Err("semantic model argument count mismatch".into());
        }
        Ok(self.recipe.apply(kind, &arguments))
    }
}
