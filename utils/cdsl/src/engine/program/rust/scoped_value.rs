//! Ordinary value expressions evaluated after a scoped schedule or in a query.
use super::*;
use crate::ast::{BinaryOp, UnaryOp};

pub(super) fn ident(name: &str) -> Ident {
    match name.strip_prefix("\0cdsl") {
        Some(index) => format_ident!("__cdsl_{index}"),
        None => format_ident!("__cdsl_bound_{name}"),
    }
}

pub(super) fn ty(
    program: &CheckedProgram,
    selected: &[String],
    schema: &Schema,
    signature: &mut Signature,
    checked: &Ty,
) -> Result<TokenStream, Error> {
    let Ty::Con(family, parts) = checked else {
        return Err(Error("scoped value requires a concrete type".into()));
    };
    if *family == crate::infer::TUPLE {
        let parts = type_parts(parts)
            .into_iter()
            .map(|part| ty(program, selected, schema, signature, part))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(quote!((#(#parts,)*)));
    }
    if *family == crate::infer::UNIT {
        return Ok(quote!(()));
    }
    let source = program
        .type_families
        .iter()
        .find_map(|(path, id)| (*id == *family).then_some(path))
        .ok_or_else(|| Error("scoped value has no executable type".into()))?;
    if source == &["Nat"] {
        return Ok(quote!(u64));
    }
    let expr = Expr {
        value: Expression::Path(source.clone()),
        range: 0..0,
    };
    Ok(Codec::read(&expr, checked, signature, schema, selected)?.argument_ty())
}

#[derive(Clone)]
pub(super) enum Shape {
    Other,
    Natural,
    Tuple(Vec<Self>),
    Projection { index: usize, element: Box<Self> },
}

impl Shape {
    pub(super) fn new(program: &CheckedProgram, ty: &Ty) -> Self {
        let Ty::Con(family, parts) = ty else {
            return Self::Other;
        };
        if *family == crate::infer::TUPLE {
            return Self::Tuple(
                type_parts(parts)
                    .into_iter()
                    .map(|ty| Self::new(program, ty))
                    .collect(),
            );
        }
        if program.type_families.get(&vec!["Nat".into()]) == Some(family) {
            return Self::Natural;
        }
        Self::Other
    }

    fn element(&self, index: usize) -> &Self {
        match self {
            Self::Tuple(items) => items.get(index).unwrap_or(&Self::Other),
            Self::Projection {
                index: selected,
                element,
            } if *selected == index => element,
            _ => &Self::Other,
        }
    }
}

pub(super) struct Values<'a> {
    pub(super) globals: &'a BTreeMap<Path, &'a Binding>,
    pub(super) constructors: &'a BTreeSet<Path>,
    pub(super) tables: Option<&'a BTreeMap<Path, &'a QueryTable>>,
}

impl Values<'_> {
    pub(super) fn emit(&self, expr: &Expr, shape: &Shape) -> Result<TokenStream, Error> {
        let argument = |expr| self.emit(expr, shape);
        let arithmetic_error = quote!(::tir_egglog::Error::Unsupported(
            "invalid arithmetic in CDSL value".into()
        ));
        Ok(match &expr.value {
            Expression::Unit => quote!(()),
            Expression::Literal(Literal::Bool(value)) => quote!(#value),
            Expression::Literal(Literal::String(value)) => {
                quote!(::std::string::String::from(#value))
            }
            Expression::Literal(Literal::Natural(value)) => {
                let digits = value.replace('_', "");
                if matches!(shape, Shape::Natural) {
                    let integer = digits.parse::<u64>().map_err(error)?;
                    quote!(#integer)
                } else {
                    let integer = digits.parse::<i64>().map_err(error)?;
                    quote!(#integer)
                }
            }
            Expression::Path(path) if self.globals.contains_key(path) => argument(
                &crate::normalize::head(expr, self.globals, self.constructors)?,
            )?,
            Expression::Path(path) if path.len() == 1 => {
                let name = ident(&path[0]);
                quote!(::std::borrow::ToOwned::to_owned(&#name))
            }
            Expression::Tuple(items) => {
                let items = items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| self.emit(item, shape.element(i)))
                    .collect::<Result<Vec<_>, _>>()?;
                quote!((#(#items,)*))
            }
            Expression::Block(block) => self.block(block, shape)?,
            Expression::If {
                condition,
                then,
                otherwise,
            } => {
                let condition = self.emit(condition, &Shape::Other)?;
                let then = self.block(then, shape)?;
                let otherwise = argument(otherwise)?;
                quote!(if #condition { #then } else { #otherwise })
            }
            Expression::Unary { op, value } => {
                let value = self.emit(value, &Shape::Other)?;
                match op {
                    UnaryOp::Not => quote!(!(#value)),
                    UnaryOp::Negate => {
                        quote!((#value).checked_neg().ok_or_else(|| #arithmetic_error)?)
                    }
                }
            }
            Expression::Binary { op, left, right } => {
                let left = argument(left)?;
                let right = argument(right)?;
                match op {
                    BinaryOp::Or => quote!((#left) || (#right)),
                    BinaryOp::And => quote!((#left) && (#right)),
                    BinaryOp::Equal => quote!((#left) == (#right)),
                    BinaryOp::NotEqual => quote!((#left) != (#right)),
                    op => {
                        let method = match op {
                            BinaryOp::Add => "checked_add",
                            BinaryOp::Subtract => "checked_sub",
                            BinaryOp::Multiply => "checked_mul",
                            BinaryOp::Divide => "checked_div",
                            BinaryOp::Remainder => "checked_rem",
                            _ => return Err(Error("unsupported CDSL value operator".into())),
                        };
                        let method = format_ident!("{method}");
                        quote!((#left).#method(#right).ok_or_else(|| #arithmetic_error)?)
                    }
                }
            }
            Expression::Member { value, name } => {
                if let Ok(index) = name.parse::<usize>() {
                    if let Expression::Tuple(items) = &value.value
                        && let Some(item) = items.get(index)
                    {
                        return argument(item);
                    }
                    let value = self.emit(
                        value,
                        &Shape::Projection {
                            index,
                            element: Box::new(shape.clone()),
                        },
                    )?;
                    let index = syn::Index::from(index);
                    quote!((#value).#index)
                } else {
                    let resolved = crate::normalize::head(expr, self.globals, self.constructors)?;
                    if matches!(&resolved.value, Expression::Member { .. }) {
                        return Err(Error("unsupported executable CDSL member".into()));
                    }
                    argument(&resolved)?
                }
            }
            Expression::Call { value, arguments } => self.call(expr, value, arguments, shape)?,
            _ => return Err(Error("unsupported executable CDSL value".into())),
        })
    }

    fn call(
        &self,
        expr: &Expr,
        value: &Expr,
        arguments: &[Expr],
        shape: &Shape,
    ) -> Result<TokenStream, Error> {
        if let Expression::Path(path) = &value.value
            && let Some(table) = self.tables.and_then(|tables| tables.get(path))
        {
            return table.lookup(self, arguments);
        }
        if let Expression::Path(path) = &value.value {
            match (path.as_slice(), arguments) {
                ([std, integer, op], [left, right])
                    if std == "std"
                        && integer == "int"
                        && matches!(op.as_str(), "less" | "equal") =>
                {
                    let a = self.emit(left, shape)?;
                    let b = self.emit(right, shape)?;
                    if op == "less" {
                        return Ok(quote!((#a) < (#b)));
                    }
                    return Ok(quote!((#a) == (#b)));
                }
                ([op], [left, right]) if matches!(op.as_str(), "lt" | "gt") => {
                    let a = self.emit(left, shape)?;
                    let b = self.emit(right, shape)?;
                    if op == "lt" {
                        return Ok(quote!((#a) < (#b)));
                    }
                    return Ok(quote!((#a) > (#b)));
                }
                _ => {}
            }
        }
        let resolved = crate::normalize::head(expr, self.globals, self.constructors)?;
        if matches!(&resolved.value, Expression::Call { value: callee, .. }
            if matches!((&callee.value, &value.value), (Expression::Path(a), Expression::Path(b)) if a == b))
        {
            return Err(Error("unsupported executable CDSL value call".into()));
        }
        self.emit(&resolved, shape)
    }

    fn block(&self, block: &Block, shape: &Shape) -> Result<TokenStream, Error> {
        let mut bindings = BTreeMap::new();
        for binding in &block.bindings {
            let value = crate::normalize::substitute(&binding.value, &bindings)?;
            bindings.insert(binding.path.clone(), value);
        }
        let result = crate::normalize::substitute(&block.value, &bindings)?;
        self.emit(&result, shape)
    }
}
