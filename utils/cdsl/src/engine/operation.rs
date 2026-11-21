use std::collections::{BTreeMap, BTreeSet};

use tir_egglog::{
    Database, Rules,
    ast::{Action, Expr, Fact, Facts, GenericActions},
    prelude::exprs::{bool as boolean, call, int, string, var},
    span,
};

use super::{Schema, error, path};
use crate::{
    CheckedProgram, Error,
    ast::{
        BinaryOp, Binding, Block, Declaration, Expr as Source, Expression, Literal, OpField, Path,
        PatternKind,
    },
    normalize::substitute,
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

struct Term {
    value: Expr,
    width: Option<Expr>,
}

impl Term {
    fn plain(value: Expr) -> Self {
        Self { value, width: None }
    }

    fn bits(value: Expr, width: &Expr) -> Self {
        Self {
            value,
            width: Some(width.clone()),
        }
    }
}

fn width_preserving(name: &str) -> bool {
    matches!(
        name,
        "add_wrapping"
            | "sub_wrapping"
            | "mul_wrapping"
            | "and"
            | "or"
            | "xor"
            | "shl"
            | "lshr"
            | "ashr"
            | "udiv"
            | "urem"
            | "sdiv"
            | "srem"
    )
}

fn bits(
    name: &str,
    args: &[Term],
    generics: &[Expr],
    result_width: Option<&Expr>,
    bindings: &IntegerBindings,
) -> Option<Term> {
    if let [x, y] = args {
        // Checked binary bits primitives use one width for both operands.
        let width = generics
            .first()
            .or(x.width.as_ref())
            .or(y.width.as_ref())
            .or(result_width)?;
        let (x, y) = (&x.value, &y.value);
        let word = |value| Term::bits(value, width);
        let control = Term::plain;
        let binary = |op| apply(op, [x.clone(), y.clone()]);
        let signed_binary = |op| apply(op, [signed(x.clone(), width), signed(y.clone(), width)]);
        let signed_overflow = |op| {
            let result = signed_binary(op);
            not(apply(
                "equal",
                [result.clone(), signed(narrow(result, width), width)],
            ))
        };
        return Some(match name {
            "add_wrapping" => word(narrow(binary("+"), width)),
            "sub_wrapping" => word(narrow(binary("-"), width)),
            "mul_wrapping" => word(narrow(binary("*"), width)),
            "and" => word(binary("&")),
            "or" => word(binary("|")),
            "xor" => word(binary("^")),
            "eq" => control(binary("equal")),
            "ne" => control(not(binary("equal"))),
            "ult" => control(binary("bool-<")),
            "ugt" => control(binary("bool->")),
            "ule" => control(binary("le")),
            "uge" => control(binary("ge")),
            "slt" => control(signed_binary("bool-<")),
            "sgt" => control(signed_binary("bool->")),
            "sle" => control(signed_binary("le")),
            "sge" => control(signed_binary("ge")),
            "shl" | "lshr" | "ashr" => {
                let shift = apply(&bindings.shift_amount, [y.clone()]);
                let input = if name == "ashr" {
                    signed(x.clone(), width)
                } else {
                    x.clone()
                };
                let op = if name == "shl" { "<<" } else { ">>" };
                word(narrow(apply(op, [input, shift]), width))
            }
            "udiv" => word(binary("/")),
            "urem" => word(binary("%")),
            "sdiv" => word(narrow(signed_binary("/"), width)),
            "srem" => word(narrow(signed_binary("%"), width)),
            "signed_div_defined" => {
                let min = apply(">>", [modulus(width), int(1)]);
                let overflow = apply(
                    "and",
                    [
                        apply("equal", [x.clone(), min]),
                        apply("equal", [y.clone(), narrow(bigint(-1), width)]),
                    ],
                );
                control(apply(
                    "and",
                    [not(apply("equal", [y.clone(), bigint(0)])), not(overflow)],
                ))
            }
            "unsigned_add_overflows" => control(apply("ge", [binary("+"), modulus(width)])),
            "unsigned_sub_overflows" => control(binary("bool-<")),
            "signed_add_overflows" => control(signed_overflow("+")),
            "signed_sub_overflows" => control(signed_overflow("-")),
            _ => return None,
        });
    }
    Some(match (name, args, generics) {
        ("from_int", [x], [w]) => Term::bits(narrow(x.value.clone(), w), w),
        ("from_bool", [x], _) => Term::bits(apply(&bindings.bool_bit, [x.value.clone()]), &int(1)),
        ("zero", [], [w]) => Term::bits(bigint(0), w),
        ("zero", [], []) => Term::plain(bigint(0)),
        ("width_value", [], [w]) => Term::bits(apply("bigint", [w.clone()]), w),
        ("zext" | "trunc" | "bitcast", [x], [_, out]) => {
            Term::bits(narrow(x.value.clone(), out), out)
        }
        ("sext", [x], [input, out]) => Term::bits(narrow(signed(x.value.clone(), input), out), out),
        _ => return None,
    })
}

const OPERAND_BINDING: &str = "\0eqsat_operand";
const RECEIVER_BINDING: &str = "\0eqsat_receiver";

fn operand_bits(slot: usize) -> Expr {
    var(&format!("__bits_{slot}"))
}

#[derive(Clone, Copy)]
enum Receiver<'a> {
    None,
    AttributeParameter,
    Implicit(&'a str),
}

struct Attributes<'a> {
    widths: &'a [Expr],
    bindings: &'a IntegerBindings,
    projected: BTreeSet<(String, &'a str)>,
    flags: BTreeSet<(String, bool)>,
}

impl Attributes<'_> {
    fn body(
        &mut self,
        expr: &Source,
        receiver: Receiver<'_>,
        result_width: Option<&Expr>,
    ) -> Option<Term> {
        let Expression::Function(function) = &expr.value else {
            return None;
        };
        if !function.generics.is_empty() {
            return None;
        }
        let (receiver, operands) = match (receiver, function.parameters.as_slice()) {
            (Receiver::Implicit(name), []) => (Some(name), &[][..]),
            (Receiver::Implicit(_), _) => return None,
            (Receiver::AttributeParameter, [receiver, operands @ ..])
                if operands.len() == self.widths.len() =>
            {
                (Some(receiver.name.as_str()), operands)
            }
            (_, operands) if operands.len() == self.widths.len() => (None, operands),
            _ => return None,
        };
        let mut values = BTreeMap::new();
        let mut bind = |name: &str, path| {
            values.insert(
                vec![name.to_owned()],
                Source {
                    value: Expression::Path(path),
                    range: expr.range.clone(),
                },
            );
        };
        if let Some(receiver) = receiver {
            bind(receiver, vec![RECEIVER_BINDING.into()]);
        }
        for (slot, operand) in operands.iter().enumerate() {
            bind(
                &operand.name,
                vec![OPERAND_BINDING.into(), slot.to_string()],
            );
        }
        self.block(&function.body, values, result_width)
    }

    fn block(
        &mut self,
        block: &Block,
        mut values: BTreeMap<Path, Source>,
        result_width: Option<&Expr>,
    ) -> Option<Term> {
        if block.bindings.is_empty() && values.is_empty() {
            return self.term(&block.value, result_width);
        }
        for binding in &block.bindings {
            if !binding.generics.is_empty() {
                return None;
            }
            values.insert(
                binding.path.clone(),
                substitute(&binding.value, &values).ok()?,
            );
        }
        self.term(&substitute(&block.value, &values).ok()?, result_width)
    }

    fn term(&mut self, expr: &Source, result_width: Option<&Expr>) -> Option<Term> {
        Some(match &expr.value {
            Expression::Literal(Literal::Bool(value)) => Term::plain(boolean(*value)),
            Expression::Path(path) => {
                let [binding, slot] = path.as_slice() else {
                    return None;
                };
                if binding != OPERAND_BINDING {
                    return None;
                }
                let slot = slot.parse::<usize>().ok()?;
                Term::bits(operand_bits(slot), self.widths.get(slot)?)
            }
            Expression::Block(block) => self.block(block, BTreeMap::new(), result_width)?,
            Expression::Member { .. } => {
                let name = self_field(expr)?;
                self.projected
                    .insert((name.to_owned(), &self.bindings.attr_int));
                Term::plain(var(&format!("__attr_{name}")))
            }
            Expression::Binary { op, left, right } => Term::plain(apply(
                match op {
                    BinaryOp::And => "and",
                    BinaryOp::Or => "or",
                    _ => return None,
                },
                [self.term(left, None)?.value, self.term(right, None)?.value],
            )),
            Expression::Match { value, arms } => {
                let field = self_field(value)?;
                self.projected
                    .insert((field.to_owned(), &self.bindings.attr_case));
                let selected = var(&format!("__attr_{field}"));
                let mut answer = boolean(false);
                for (pattern, consequent) in arms.iter().rev() {
                    let PatternKind::Path(path) = &pattern.value else {
                        return None;
                    };
                    let name = path.last()?.to_ascii_lowercase();
                    answer = apply(
                        &self.bindings.pick_case,
                        [
                            selected.clone(),
                            string(&name),
                            self.term(consequent, None)?.value,
                            answer,
                        ],
                    );
                }
                Term::plain(answer)
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
                        let result_width = result_width.filter(|_| width_preserving(name));
                        let input_width = match name.as_str() {
                            "from_int" | "from_bool" => None,
                            _ => generics.first().or(result_width),
                        };
                        let args = arguments
                            .iter()
                            .map(|arg| self.term(arg, input_width))
                            .collect::<Option<Vec<_>>>()?;
                        bits(name, &args, &generics, result_width, self.bindings)?
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
                        Term::plain(var(&format!("__flag_{name}_{default}")))
                    }
                    _ => return None,
                }
            }
            _ => return None,
        })
    }
}

fn unsupported(kind: &str, field: &str) -> Error {
    Error(format!(
        "unsupported EqSat integer semantics in {kind}.{field}"
    ))
}

fn self_field(expr: &Source) -> Option<&str> {
    match &expr.value {
        Expression::Member { value, name } if matches!(&value.value, Expression::Path(path) if path == &[RECEIVER_BINDING]) => {
            Some(name)
        }
        _ => None,
    }
}

fn width_argument(expr: &Source) -> Option<Expr> {
    match &expr.value {
        Expression::Path(parts) if parts.len() == 1 => Some(width_variable(&parts[0])),
        Expression::Literal(Literal::Natural(value)) => Some(int(value.parse().ok()?)),
        _ => None,
    }
}

fn width_variable(name: &str) -> Expr {
    var(&format!("__width_{name}"))
}

fn type_argument<'a>(expr: &'a Source, name: &str) -> Option<&'a Source> {
    if let Expression::Generic { value, arguments } = &expr.value
        && matches!(&value.value, Expression::Path(path) if path == &["std", "ir", name])
        && let [argument] = arguments.as_slice()
    {
        return Some(argument);
    }
    None
}

fn integer_type(expr: &Source) -> Option<Expr> {
    width_argument(type_argument(
        type_argument(expr, "El").unwrap_or(expr),
        "Bits",
    )?)
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

fn attribute(table: &str, field: &str) -> Expr {
    apply(table, [string("operation"), var("__op"), string(field)])
}

fn row(name: &str, args: impl Into<Vec<Expr>>) -> Fact {
    Fact::Fact(apply(name, args))
}

struct IntegerBindings {
    binary: String,
    flag_field: String,
    result: String,
    operand: String,
    folded: String,
    constant_kind: String,
    flag_value: String,
    constant: String,
    attribute: String,
    attr_int: String,
    attr_case: String,
    pick_case: String,
    bool_bit: String,
    shift_amount: String,
}

pub(super) fn integer_rule_arguments(binding: &Binding) -> Option<&[Source]> {
    match &binding.value.value {
        Expression::Call { value, arguments }
            if path(value).is_ok_and(|path| path == &["std", "schedule", "integer_rules"]) =>
        {
            Some(arguments)
        }
        _ => None,
    }
}

pub(super) struct IntegerOperation<'a> {
    pub binding: &'a Binding,
    widths: Vec<Expr>,
    output: Expr,
    binary: bool,
    method: Option<(&'a str, &'a Source)>,
    exec: Option<&'a Source>,
}

pub(super) fn integer_operations<'a>(
    program: &'a CheckedProgram,
    arguments: &'a [Source],
) -> Result<
    (
        &'a [String],
        impl Iterator<Item = IntegerOperation<'a>>,
        &'a Source,
    ),
    Error,
> {
    let [operations, interface, bindings] = arguments else {
        return Err(Error(
            "integer_rules requires an operation module, interface and bindings".into(),
        ));
    };
    let operations = path(operations)?;
    let interface = match &interface.value {
        Expression::Generic { value, .. } => path(value)?,
        _ => path(interface)?,
    };
    let module = program
        .artifact()
        .all_modules()
        .find(|module| module.path == *operations)
        .ok_or_else(|| {
            Error(format!(
                "missing checked operation module {}",
                operations.join("::")
            ))
        })?;
    let operations = module.ast.declarations.iter().filter_map(move |declaration| {
        let Declaration::Let { binding, .. } = &declaration.value else {
            return None;
        };
        let fields = binding.operation_fields()?;
        let (parameters, output) = fields.iter().find_map(|field| match &field.value {
            OpField::Signature { parameters, result, .. } => Some((parameters, integer_type(result)?)),
            _ => None,
        })?;
        let widths = parameters.iter().map(|p| integer_type(&p.ty)).collect::<Option<Vec<_>>>()?;
        let binary = matches!(widths.as_slice(), [left, right] if left == &output && right == &output)
            && binding.generics.iter().any(|generic| width_variable(&generic.name) == output);
        let method = fields.iter().find_map(|field| {
            let OpField::Interfaces { receiver, entries } = &field.value else {
                return None;
            };
            entries
                .iter()
                .find_map(|(_, path, method)| (path == interface).then_some(method))
                .map(|method| (receiver.as_str(), method))
        });
        let exec = operation_field(binding, "exec");
        (binary || method.is_some() || exec.is_some()).then_some(IntegerOperation {
            binding, widths, output, binary, method, exec,
        })
    });
    Ok((&module.path, operations, bindings))
}

fn operation_field<'a>(binding: &'a Binding, label: &str) -> Option<&'a Source> {
    binding
        .operation_fields()?
        .iter()
        .find_map(|field| match &field.value {
            OpField::Field { name, value } if name == label => Some(value),
            _ => None,
        })
}

impl Schema {
    pub(super) fn add_integer_operations(
        &self,
        rule_source: &CheckedProgram,
        arguments: &[Source],
        rules: &mut Rules,
        flag_fields: &str,
        fold: &str,
    ) -> Result<(), Error> {
        let (module, operations, bindings) = integer_operations(rule_source, arguments)?;
        let Expression::Record(fields) = &bindings.value else {
            return Err(Error("integer_rules requires literal bindings".into()));
        };
        let binding = |name: &str| {
            let value = fields
                .iter()
                .find(|(field, _)| field == name)
                .ok_or_else(|| Error(format!("missing integer rule binding {name}")))?;
            Ok(self.name(path(&value.1)?))
        };
        let bindings = IntegerBindings {
            binary: binding("binary")?,
            flag_field: binding("flag_field")?,
            result: binding("result")?,
            operand: binding("operand")?,
            folded: binding("folded")?,
            constant_kind: binding("constant_kind")?,
            flag_value: binding("flag_value")?,
            constant: binding("constant")?,
            attribute: binding("attribute")?,
            attr_int: binding("attr_int")?,
            attr_case: binding("attr_case")?,
            pick_case: binding("pick_case")?,
            bool_bit: binding("bool_bit")?,
            shift_amount: binding("shift_amount")?,
        };
        let db = self.database();
        for set in [flag_fields, fold] {
            rules.add_ruleset(set);
        }
        let mut flags = BTreeSet::new();
        for IntegerOperation {
            binding,
            widths,
            output,
            binary,
            method,
            exec,
        } in operations
        {
            let import = binding
                .operation_import(module, rule_source.artifact().declaration_source(module))
                .ok_or_else(|| Error(format!("unnamed operation in {}", module.join("::"))))?;
            let kind = import.name;
            let declaration = || apply(&import.symbol, []);

            if binary {
                add(
                    rules,
                    db,
                    flag_fields,
                    Vec::new(),
                    vec![apply(&bindings.binary, [declaration()])],
                )?;
            }
            let field = |label| operation_field(binding, label);
            let receiver = if field("attributes").is_some() {
                Receiver::AttributeParameter
            } else {
                Receiver::None
            };
            let (label, body, body_receiver) = if let Some((receiver, method)) = method {
                let label = "ConstantLike.constant_value";
                let body = match &method.value {
                    Expression::Record(entries) => entries
                        .iter()
                        .find(|(name, _)| name == "constant_value")
                        .map(|(_, body)| body),
                    _ => None,
                }
                .ok_or_else(|| unsupported(&kind, label))?;
                (label, body, Receiver::Implicit(receiver))
            } else if let Some(body) = exec {
                ("exec", body, receiver)
            } else {
                continue;
            };
            let mut attrs = Attributes {
                widths: &widths,
                bindings: &bindings,
                projected: BTreeSet::new(),
                flags: BTreeSet::new(),
            };
            let mut lower = |source, label, receiver, result_width| {
                attrs
                    .body(source, receiver, result_width)
                    .ok_or_else(|| unsupported(&kind, label))
            };
            let result_width = method.is_none().then_some(&output);
            let value = lower(body, label, body_receiver, result_width)?;
            if method.is_some() {
                let width = match &body.value {
                    Expression::Function(function) => {
                        function.result.as_ref().and_then(integer_type)
                    }
                    _ => None,
                };
                if width.as_ref().or(value.width.as_ref()) != Some(&output) {
                    return Err(Error(format!(
                        "{kind}.{label} must return the declared integer result width"
                    )));
                }
            }
            let value = value.value;
            let guards = [("requires", true), ("poison", false)]
                .into_iter()
                .filter_map(|(label, expected)| Some((label, field(label)?, boolean(expected))))
                .map(|(label, source, expected)| {
                    Ok(eq(lower(source, label, receiver, None)?.value, expected))
                })
                .collect::<Result<Vec<_>, Error>>()?;
            let mut query = vec![row(
                &bindings.result,
                [var("__op"), declaration(), var("__value"), output],
            )];
            for (i, width) in widths.iter().enumerate() {
                let operand = var(&format!("__operand_{i}"));
                query.extend([
                    row(
                        &bindings.operand,
                        [var("__op"), int(i as i64), operand.clone(), width.clone()],
                    ),
                    row(&bindings.folded, [operand, operand_bits(i)]),
                ]);
            }
            if method.is_some() {
                query.push(row(&bindings.constant_kind, [declaration()]));
            }
            for (field, primitive) in attrs.projected {
                let handle = var(&format!("__handle_{field}"));
                query.extend([
                    eq(handle.clone(), attribute(&bindings.attribute, &field)),
                    eq(var(&format!("__attr_{field}")), apply(primitive, [handle])),
                ]);
            }
            for (field, default) in attrs.flags {
                query.push(row(
                    &bindings.flag_value,
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
            let mut heads = vec![apply(&bindings.folded, [var("__value"), value.clone()])];
            if method.is_none() {
                heads.push(apply(
                    &bindings.constant,
                    [var("__op"), var("__value"), value],
                ));
            }
            add(rules, db, fold, query, heads)?;
        }
        for (field, default) in flags {
            add(
                rules,
                db,
                flag_fields,
                Vec::new(),
                vec![apply(
                    &bindings.flag_field,
                    [string(&field), boolean(default)],
                )],
            )?;
        }
        Ok(())
    }
}
