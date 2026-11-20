use std::collections::BTreeSet;

use tir_egglog::{
    Database, Rules, Signature,
    ast::{Action, Expr, Fact, Facts, GenericActions},
    prelude::exprs::{bool as boolean, call, int, string, var},
    span,
};

use super::{Schema, error};
use crate::{
    CheckedProgram, Error,
    ast::{BinaryOp, Declaration, Expr as Source, Expression, Literal, OpField, PatternKind},
};

fn apply(name: &str, args: impl Into<Vec<Expr>>) -> Expr {
    call(name, args.into())
}

fn bigint(n: i64) -> Expr {
    apply("bigint", [int(n)])
}

fn modulus(width: &Expr) -> Expr {
    apply("<<", [bigint(1), width.clone()])
}

fn narrow(x: Expr, width: &Expr) -> Expr {
    apply("&", [x, apply("-", [modulus(width), bigint(1)])])
}

fn signed(x: Expr, width: &Expr) -> Expr {
    let sign = apply(">>", [modulus(width), int(1)]);
    apply("-", [apply("^", [x, sign.clone()]), sign])
}

fn not(x: Expr) -> Expr {
    apply("not", [x])
}

fn bits(name: &str, args: &[Expr], width: &Expr, generics: &[Expr]) -> Option<Expr> {
    if let [x, y] = args {
        let binary = |op| apply(op, [x.clone(), y.clone()]);
        let signed_binary = |op| apply(op, [signed(x.clone(), width), signed(y.clone(), width)]);
        let signed_overflow = |op| {
            let result = signed_binary(op);
            not(apply(
                "bool-=",
                [result.clone(), signed(narrow(result, width), width)],
            ))
        };
        return Some(match name {
            "add_wrapping" => narrow(binary("+"), width),
            "sub_wrapping" => narrow(binary("-"), width),
            "mul_wrapping" => narrow(binary("*"), width),
            "and" => binary("&"),
            "or" => binary("|"),
            "xor" => binary("^"),
            "eq" => binary("bool-="),
            "ne" => not(binary("bool-=")),
            "ult" => binary("bool-<"),
            "ugt" => binary("bool->"),
            "ule" => binary("bool-<="),
            "uge" => binary("bool->="),
            "slt" => signed_binary("bool-<"),
            "sgt" => signed_binary("bool->"),
            "sle" => signed_binary("bool-<="),
            "sge" => signed_binary("bool->="),
            "shl" | "lshr" | "ashr" => {
                let shift = apply("eqsat-amount", [y.clone()]);
                let input = if name == "ashr" {
                    signed(x.clone(), width)
                } else {
                    x.clone()
                };
                let op = if name == "shl" { "<<" } else { ">>" };
                narrow(apply(op, [input, shift]), width)
            }
            "udiv" => binary("/"),
            "urem" => binary("%"),
            "sdiv" => narrow(signed_binary("/"), width),
            "srem" => narrow(signed_binary("%"), width),
            "signed_div_defined" => {
                let min = apply(">>", [modulus(width), int(1)]);
                let overflow = apply(
                    "and",
                    [
                        apply("bool-=", [x.clone(), min]),
                        apply("bool-=", [y.clone(), narrow(bigint(-1), width)]),
                    ],
                );
                apply(
                    "and",
                    [not(apply("bool-=", [y.clone(), bigint(0)])), not(overflow)],
                )
            }
            "unsigned_add_overflows" => apply("bool->=", [binary("+"), modulus(width)]),
            "unsigned_sub_overflows" => binary("bool-<"),
            "signed_add_overflows" => signed_overflow("+"),
            "signed_sub_overflows" => signed_overflow("-"),
            _ => return None,
        });
    }
    Some(match (name, args, generics) {
        ("from_int", [x], [w]) => narrow(x.clone(), w),
        ("from_bool", [x], _) => apply("eqsat-bool-bit", [x.clone()]),
        ("zero", [], _) => bigint(0),
        ("width_value", [], [w]) => apply("bigint", [w.clone()]),
        ("zext" | "trunc" | "bitcast", [x], [_, out]) => narrow(x.clone(), out),
        ("sext", [x], [input, out]) => narrow(signed(x.clone(), input), out),
        _ => return None,
    })
}

struct Attributes {
    word: Expr,
    projected: BTreeSet<(String, &'static str)>,
    flags: BTreeSet<(String, bool)>,
}

impl Attributes {
    fn body(&mut self, expr: &Source) -> Option<Expr> {
        let Expression::Function(function) = &expr.value else {
            return None;
        };
        if !function.body.bindings.is_empty() {
            return None;
        }
        self.term(&function.body.value)
    }

    fn term(&mut self, expr: &Source) -> Option<Expr> {
        Some(match &expr.value {
            Expression::Literal(Literal::Bool(value)) => boolean(*value),
            Expression::Path(path) if path.len() == 1 => var(&path[0]),
            Expression::Member { .. } => {
                let name = self_field(expr)?;
                self.projected.insert((name.to_owned(), "eqsat-attr-int"));
                var(&format!("__attr_{name}"))
            }
            Expression::Binary { op, left, right } => apply(
                match op {
                    BinaryOp::And => "and",
                    BinaryOp::Or => "or",
                    _ => return None,
                },
                [self.term(left)?, self.term(right)?],
            ),
            Expression::Match { value, arms } => {
                let field = self_field(value)?;
                self.projected.insert((field.to_owned(), "eqsat-attr-case"));
                let selected = var(&format!("__attr_{field}"));
                let mut answer = boolean(false);
                for (pattern, consequent) in arms.iter().rev() {
                    let PatternKind::Path(path) = &pattern.value else {
                        return None;
                    };
                    let name = path.last()?.to_ascii_lowercase();
                    answer = apply(
                        "eqsat-pick-case",
                        [
                            selected.clone(),
                            string(&name),
                            self.term(consequent)?,
                            answer,
                        ],
                    );
                }
                answer
            }
            Expression::Call { value, arguments } => {
                let (head, generics) = match &value.value {
                    Expression::Generic { value, arguments } => (
                        value,
                        arguments
                            .iter()
                            .map(width_argument)
                            .collect::<Option<Vec<_>>>()?,
                    ),
                    Expression::Path(_) => (value, Vec::new()),
                    _ => return None,
                };
                let Expression::Path(path) = &head.value else {
                    return None;
                };
                match path.as_slice() {
                    [std, module, name] if std == "std" && module == "bits" => {
                        let args = arguments
                            .iter()
                            .map(|arg| self.term(arg))
                            .collect::<Option<Vec<_>>>()?;
                        bits(name, &args, &self.word, &generics)?
                    }
                    [std, module, name]
                        if std == "std" && module == "option" && name == "get_or" =>
                    {
                        let [field, default] = arguments.as_slice() else {
                            return None;
                        };
                        let name = self_field(field)?;
                        let Expression::Literal(Literal::Bool(default)) = default.value else {
                            return None;
                        };
                        self.flags.insert((name.to_owned(), default));
                        var(&format!("__flag_{name}_{default}"))
                    }
                    _ => return None,
                }
            }
            _ => return None,
        })
    }
}

fn named(expr: &Source, name: &str) -> bool {
    matches!(&expr.value, Expression::Path(parts) if parts.last().is_some_and(|part| part == name))
}

fn self_field(expr: &Source) -> Option<&str> {
    match &expr.value {
        Expression::Member { value, name } if named(value, "self") => Some(name),
        _ => None,
    }
}

fn width_argument(expr: &Source) -> Option<Expr> {
    match &expr.value {
        Expression::Path(parts) if parts.len() == 1 => Some(var(&format!("__width_{}", parts[0]))),
        Expression::Literal(Literal::Natural(value)) => Some(int(value.parse().ok()?)),
        _ => None,
    }
}

fn type_argument<'a>(expr: &'a Source, name: &str) -> Option<&'a Source> {
    if let Expression::Generic { value, arguments } = &expr.value
        && named(value, name)
        && let [argument] = arguments.as_slice()
    {
        return Some(argument);
    }
    None
}

fn integer_type(expr: &Source) -> Option<Expr> {
    width_argument(type_argument(type_argument(expr, "El")?, "Bits")?)
}

fn eq(left: Expr, right: Expr) -> Fact {
    Fact::Eq(span!(), left, right)
}

fn add(
    rules: &mut Rules,
    db: &Database,
    set: &str,
    body: Vec<Fact>,
    head: Vec<Expr>,
) -> Result<(), Error> {
    let actions = head
        .into_iter()
        .map(|term| Action::Expr(span!(), term))
        .collect();
    rules
        .rule(db, set, Facts(body), GenericActions(actions))
        .map_err(error)
}

fn attribute(field: &str) -> Expr {
    apply(
        "Attribute",
        [string("operation"), var("__op"), string(field)],
    )
}

fn row(name: &str, args: impl Into<Vec<Expr>>) -> Fact {
    Fact::Fact(apply(name, args))
}

impl Schema {
    /// Register a typed external signature for planning a program loaded with host bindings.
    pub fn bind_external(
        &mut self,
        name: &str,
        inputs: &[&str],
        output: &str,
    ) -> Result<(), Error> {
        let db = self.engine.database_mut();
        let inputs = inputs
            .iter()
            .map(|name| db.sort(name).map_err(error))
            .collect::<Result<_, _>>()?;
        let output = db.sort(output).map_err(error)?;
        db.add_primitive(name, Signature { inputs, output }, |_, _| None);
        Ok(())
    }

    /// Compile constant evaluation from the checked integer operation signatures and methods.
    pub fn compile_integer_operations(
        &self,
        rule_source: &CheckedProgram,
        program: &CheckedProgram,
        dialect: &str,
    ) -> Result<Vec<u8>, Error> {
        let db = self.database();
        let mut rules = self.lower(rule_source)?.rules;
        rules.set_colored("branch_local");
        for set in ["eqsat-flag-fields", "eqsat-fold"] {
            rules.add_ruleset(set);
        }
        let mut flags = BTreeSet::new();
        let declarations = program
            .artifact()
            .modules
            .iter()
            .flat_map(|module| &module.ast.declarations);
        for declaration in declarations {
            let Declaration::Let { binding, .. } = &declaration.value else {
                continue;
            };
            let Expression::Op(fields) = &binding.value.value else {
                continue;
            };
            let [name] = binding.path.as_slice() else {
                continue;
            };
            let Some((parameters, output)) = fields.iter().find_map(|field| match &field.value {
                OpField::Signature {
                    parameters, result, ..
                } => Some((parameters, integer_type(result)?)),
                _ => None,
            }) else {
                continue;
            };
            let Some(widths) = parameters
                .iter()
                .map(|p| integer_type(&p.ty))
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let method = program
                .operation_interfaces()
                .iter()
                .find(|op| op.operation().path() == binding.path.as_slice())
                .and_then(|op| {
                    op.implementations().find_map(|(iface, method)| {
                        (iface.path().last().map(String::as_str) == Some("ConstantLike"))
                            .then_some(method)
                    })
                });
            let field = |label| {
                fields.iter().find_map(|field| match &field.value {
                    OpField::Field { name, value } if name == label => Some(value),
                    _ => None,
                })
            };
            let body = if let Some(method) = method {
                let Expression::Record(entries) = &method.expression().value else {
                    continue;
                };
                entries
                    .iter()
                    .find(|(name, _)| name == "constant_value")
                    .map(|(_, body)| body)
            } else {
                if parameters.is_empty() {
                    continue;
                }
                field("exec")
            };
            let mut attrs = Attributes {
                word: widths.first().cloned().unwrap_or_else(|| output.clone()),
                projected: BTreeSet::new(),
                flags: BTreeSet::new(),
            };
            let Some(value) = body.and_then(|body| attrs.body(body)) else {
                continue;
            };
            let guards = [("requires", true), ("poison", false)]
                .into_iter()
                .filter_map(|(label, expected)| Some((field(label)?, boolean(expected))))
                .map(|(source, expected)| Some(eq(attrs.body(source)?, expected)))
                .collect::<Option<Vec<_>>>();
            let Some(guards) = guards else { continue };
            let kind = format!("{dialect}.{name}");
            let mut query = vec![row(
                "EqSatIntegerResult",
                [var("__op"), string(&kind), var("__value"), output],
            )];
            for (i, (parameter, width)) in parameters.iter().zip(widths).enumerate() {
                let operand = var(&format!("__operand_{i}"));
                query.extend([
                    row(
                        "EqSatIntegerOperand",
                        [var("__op"), int(i as i64), operand.clone(), width.clone()],
                    ),
                    row("EqSatFolded", [operand, var(&parameter.name)]),
                ]);
            }
            if method.is_some() {
                query.push(row("EqSatConstantImplementation", [string(&kind)]));
            }
            for (field, primitive) in attrs.projected {
                let handle = var(&format!("__handle_{field}"));
                query.extend([
                    eq(handle.clone(), attribute(&field)),
                    eq(var(&format!("__attr_{field}")), apply(primitive, [handle])),
                ]);
            }
            for (field, default) in attrs.flags {
                query.push(row(
                    "EqSatFlag",
                    [
                        var("__op"),
                        string(&field),
                        boolean(default),
                        var(&format!("__flag_{field}_{default}")),
                    ],
                ));
                flags.insert((field, default));
            }
            query.extend(guards);
            let mut heads = vec![apply("EqSatFolded", [var("__value"), value.clone()])];
            if method.is_none() {
                heads.push(apply("EqSatConstant", [var("__op"), var("__value"), value]));
            }
            add(&mut rules, db, "eqsat-fold", query, heads)?;
        }
        for (field, default) in flags {
            add(
                &mut rules,
                db,
                "eqsat-flag-fields",
                Vec::new(),
                vec![apply("EqSatFlagField", [string(&field), boolean(default)])],
            )?;
        }
        rules
            .add_combined_ruleset("eqsat-global", &["analyze".into(), "eqsat-fold".into()])
            .map_err(error)?;
        Ok(rules.compile(db).map_err(error)?.to_bytes())
    }
}
