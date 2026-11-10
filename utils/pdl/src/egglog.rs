use std::collections::{BTreeMap, BTreeSet};

use crate::{
    BinaryOp, BindingType, Direction, Expr, ExprKind, File, Item, Operator, Proof, RuleKind, Term,
    TermKind, Type, Width,
};

/// An emitted library and its proof requests. Requests do not authorize rewriting.
#[derive(Debug)]
pub struct CompiledEgglog {
    pub source: String,
    pub obligations: Vec<ProofObligation>,
}

/// The entity category stored in a request column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureRole {
    Operation,
    Data,
    Dependency,
}

/// One positional column of a generated request relation.
#[derive(Debug)]
pub struct RequestCapture {
    pub name: String,
    pub role: CaptureRole,
}

/// A checked rule's candidate relation and requested data mapping.
#[derive(Debug)]
pub struct ProofObligation {
    pub rule: String,
    pub relation: String,
    pub captures: Vec<RequestCapture>,
    pub root: usize,
    pub requested_mapping: (usize, usize),
}

fn integer_literal(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Integer(value) => Some(*value),
        ExprKind::Unary {
            op: crate::UnaryOp::Negate,
            value,
        } => match &value.kind {
            ExprKind::Integer(value) => value.checked_neg(),
            _ => None,
        },
        _ => None,
    }
}

struct CheckedPattern {
    facts: Vec<String>,
    captures: Vec<RequestCapture>,
    variables: Vec<String>,
    binders: BTreeMap<String, (String, CaptureRole)>,
    next: usize,
    pointer_widths: BTreeMap<String, String>,
    integer_widths: BTreeSet<String>,
}

impl CheckedPattern {
    fn capture(&mut self, name: String, role: CaptureRole) -> String {
        let variable = format!("c{}", self.captures.len());
        self.captures.push(RequestCapture { name, role });
        self.variables.push(variable.clone());
        variable
    }

    fn fresh(&mut self) -> String {
        let value = format!("t{}", self.next);
        self.next += 1;
        value
    }

    fn term(&mut self, term: &Term, dependency: bool, prefix: &str) -> Result<String, String> {
        if term.ty.is_some() {
            return Err("checked term annotations are unsupported".into());
        }
        let role = if dependency {
            CaptureRole::Dependency
        } else {
            CaptureRole::Data
        };
        let value = match &term.kind {
            TermKind::Binder { name, ty } => {
                let value = if let Some((value, previous)) = self.binders.get(name) {
                    if *previous != role {
                        return Err("binder used as both data and dependency".into());
                    }
                    value.clone()
                } else {
                    let value = self.capture(name.clone(), role.clone());
                    self.binders
                        .insert(name.clone(), (value.clone(), role.clone()));
                    value
                };
                if let Some(ty) = ty {
                    let constraint = match ty {
                        BindingType::Type(Type::Integer(width)) if !dependency => {
                            let width = match width {
                                Width::Concrete(w) if *w > 0 => format!("(from-string \"{w}\")"),
                                Width::Named(w) => {
                                    if self.pointer_widths.contains_key(w) {
                                        return Err(
                                            "checked pointer-width constraints are unsupported"
                                                .into(),
                                        );
                                    }
                                    self.integer_widths.insert(w.clone());
                                    format!("width_{w}")
                                }
                                Width::Any => self.fresh(),
                                _ => return Err("invalid integer width".into()),
                            };
                            format!("(Apply (Ctor \"builtin.i\") (TCons (NatLit {width}) (TNil)))")
                        }
                        BindingType::Type(Type::Pointer(Width::Named(width))) if !dependency => {
                            // The schema exposes pointer shape, but no resolved layout width.
                            // An unused width is existential; shared widths need a layout query.
                            if self.integer_widths.contains(width)
                                || self
                                    .pointer_widths
                                    .get(width)
                                    .is_some_and(|other| other != name)
                            {
                                return Err(
                                    "checked pointer-width constraints are unsupported".into()
                                );
                            }
                            self.pointer_widths.insert(width.clone(), name.clone());
                            let parameters = self.fresh();
                            format!("(Apply (Ctor \"ptr.p\") {parameters})")
                        }
                        BindingType::Type(Type::State(resource)) if dependency => {
                            let resource = match resource {
                                crate::Resource::Memory => "memory",
                                crate::Resource::FpEnv => "fp.env",
                                crate::Resource::Named(_) => {
                                    return Err("unsupported state resource".into());
                                }
                            };
                            let symbol = self.fresh();
                            self.facts
                                .push(format!("(= (SymbolName {symbol}) \"{resource}\")"));
                            format!(
                                "(Apply (Ctor \"builtin.state\") (TCons (SymbolParam {symbol}) (TNil)))"
                            )
                        }
                        _ => return Err("unsupported checked binder type or operand role".into()),
                    };
                    self.facts
                        .push(format!("(= (ValueType {value}) {constraint})"));
                }
                value
            }
            TermKind::Operation {
                operator: Operator::Dialect { dialect, name },
                attributes,
                operands,
                dependencies,
            } if attributes.is_empty() => {
                let op = self.capture(
                    format!("{dialect}.{name}@{}", self.captures.len()),
                    CaptureRole::Operation,
                );
                self.facts
                    .push(format!("(= (OpKind {op}) \"{dialect}.{name}\")"));
                self.facts
                    .push(format!("(not ({prefix}UnsupportedAttribute {op}))"));
                for (slot, child) in operands.iter().chain(dependencies).enumerate() {
                    let child = self.term(child, slot >= operands.len(), prefix)?;
                    self.facts
                        .push(format!("(= (Operand {op} {slot}) {child})"));
                }
                self.facts.push(format!(
                    "(not (Operand {op} {}))",
                    operands.len() + dependencies.len()
                ));
                let result = self.capture(
                    format!("{dialect}.{name}.result@{}", self.captures.len()),
                    role.clone(),
                );
                let slot = self.fresh();
                self.facts.push(format!("(Result {op} {slot} {result})"));
                result
            }
            _ => return Err("unsupported checked operation pattern".into()),
        };
        self.facts.push(if dependency {
            format!("({prefix}StateValue {value})")
        } else {
            format!("(not ({prefix}StateValue {value}))")
        });
        Ok(value)
    }
}

fn checked_rule(
    rule: &crate::Rule,
    prefix: &str,
    index: usize,
) -> Result<(String, ProofObligation), String> {
    if !rule.guards.is_empty() {
        return Err("checked guards are unsupported".into());
    }
    let TermKind::Binder { name, ty: None } = &rule.rhs.kind else {
        return Err("checked RHS must request a captured data value".into());
    };
    if rule.rhs.ty.is_some() || !matches!(rule.lhs.kind, TermKind::Operation { .. }) {
        return Err("unsupported checked root or RHS annotation".into());
    }
    let mut pattern = CheckedPattern {
        facts: Vec::new(),
        captures: Vec::new(),
        variables: Vec::new(),
        binders: BTreeMap::new(),
        next: 0,
        pointer_widths: BTreeMap::new(),
        integer_widths: BTreeSet::new(),
    };
    let old = pattern.term(&rule.lhs, false, prefix)?;
    let (new, CaptureRole::Data) = pattern.binders.get(name).ok_or("unbound checked RHS")? else {
        return Err("checked RHS must be data".into());
    };
    pattern
        .facts
        .push(format!("(= (ValueType {old}) (ValueType {new}))"));
    let position = |variable: &str| {
        pattern
            .variables
            .iter()
            .position(|v| v == variable)
            .ok_or_else(|| "missing request capture".to_owned())
    };
    let mapping = (position(&old)?, position(new)?);
    let relation = format!("{prefix}Request{index}");
    let source = format!(
        "(relation {relation} ({}))\n(rule ({}) (({relation} {})) :ruleset {prefix}requests)\n",
        vec!["i64"; pattern.captures.len()].join(" "),
        pattern.facts.join("\n"),
        pattern.variables.join(" ")
    );
    let source = format!(
        "{source}\n(rule (({prefix}RequestScope request_root) (Within request_root c0) {}) (({relation} {})) :ruleset {prefix}bound-requests)\n",
        pattern.facts.join("\n"),
        pattern.variables.join(" ")
    );
    Ok((
        source,
        ProofObligation {
            rule: rule.name.clone(),
            relation,
            captures: pattern.captures,
            root: 0,
            requested_mapping: mapping,
        },
    ))
}

struct ConstantBinding {
    bits: String,
    width: String,
}

struct Pattern<'a> {
    prefix: &'a str,
    operators: &'a mut BTreeMap<(String, usize), usize>,
    binders: BTreeMap<String, String>,
    facts: Vec<String>,
    next: usize,
    constants: BTreeMap<String, ConstantBinding>,
    homogeneous: bool,
    admissions: &'a mut BTreeSet<String>,
}

impl Pattern<'_> {
    fn fresh(&mut self) -> String {
        let name = format!("v{}", self.next);
        self.next += 1;
        name
    }

    fn guard(&self, guard: &Expr) -> Result<String, String> {
        match &guard.kind {
            ExprKind::Call { name, args } if name == "defined" && args.len() == 1 => {
                let ExprKind::Name(name) = &args[0].kind else {
                    return Err("defined requires a binder".into());
                };
                let value = self.binders.get(name).ok_or("undefined guard binder")?;
                Ok(format!("({}Defined {value})", self.prefix))
            }
            ExprKind::Binary { op, lhs, rhs }
                if matches!(op, BinaryOp::Equal | BinaryOp::NotEqual) =>
            {
                let width = self.bits_width(lhs).or_else(|| self.bits_width(rhs));
                let comparison = if *op == BinaryOp::Equal { "=" } else { "!=" };
                Ok(format!(
                    "({comparison} {} {})",
                    self.bits(lhs, width)?,
                    self.bits(rhs, width)?
                ))
            }
            _ => Err("unsupported eqsat guard".into()),
        }
    }

    fn bits_width(&self, expr: &Expr) -> Option<&str> {
        match &expr.kind {
            ExprKind::Name(name) => self
                .constants
                .get(name)
                .map(|binding| binding.width.as_str()),
            ExprKind::Call { args, .. } if args.len() == 1 => self.bits_width(&args[0]),
            ExprKind::Binary { lhs, rhs, .. } => {
                self.bits_width(lhs).or_else(|| self.bits_width(rhs))
            }
            _ => None,
        }
    }

    fn bits(&self, expr: &Expr, literal_width: Option<&str>) -> Result<String, String> {
        if let Some(value) = integer_literal(expr) {
            let width = literal_width.ok_or("numeric literals require a constant binder width")?;
            return Ok(format!(
                "(eqsat-int-bits {width} \"$attr.signed\" \"{:016x}\")",
                value as u64
            ));
        }
        match &expr.kind {
            ExprKind::Name(name) => self
                .constants
                .get(name)
                .map(|binding| binding.bits.clone())
                .ok_or_else(|| "numeric expressions require constant binders".into()),
            ExprKind::Call { name, args }
                if args.len() == 1 && matches!(name.as_str(), "ctz" | "popcount") =>
            {
                let ExprKind::Name(binder) = &args[0].kind else {
                    return Err("constant function requires a const binder".into());
                };
                let binding = self
                    .constants
                    .get(binder)
                    .ok_or("constant function requires a const binder")?;
                Ok(format!(
                    "(eqsat-int-eval \"{name}\" {} {})",
                    binding.width, binding.bits
                ))
            }
            ExprKind::Call { name, args }
                if args.len() == 1 && matches!(name.as_str(), "known_ones" | "known_zeros") =>
            {
                let ExprKind::Name(binder) = &args[0].kind else {
                    return Err("known bits require a binder".into());
                };
                let value = self
                    .binders
                    .get(binder)
                    .ok_or("undefined known-bits binder")?;
                let fact = if name == "known_ones" {
                    "KnownOnes"
                } else {
                    "KnownZeros"
                };
                Ok(format!("({}{fact} {value})", self.prefix))
            }
            ExprKind::Binary { op, lhs, rhs }
                if matches!(op, BinaryOp::BitAnd | BinaryOp::BitOr) =>
            {
                let primitive = if *op == BinaryOp::BitAnd {
                    "eqsat-bits-and"
                } else {
                    "eqsat-bits-or"
                };
                Ok(format!(
                    "({primitive} {} {})",
                    self.bits(lhs, literal_width)?,
                    self.bits(rhs, literal_width)?
                ))
            }
            _ => Err("unsupported known-bits expression".into()),
        }
    }

    fn rhs_bits(&self, expr: &Expr) -> Result<String, String> {
        if let Some(value) = integer_literal(expr) {
            return Ok(format!(
                "(eqsat-int-bits result_width \"$attr.signed\" \"{:016x}\")",
                value as u64
            ));
        }
        if let ExprKind::Binary { op, lhs, rhs } = &expr.kind {
            let name = match op {
                BinaryOp::Add => "add",
                BinaryOp::Subtract => "sub",
                BinaryOp::Multiply => "mul",
                _ => return self.bits(expr, Some("result_width")),
            };
            return Ok(format!(
                "(eqsat-int-eval \"{name}\" result_width {} {})",
                self.rhs_bits(lhs)?,
                self.rhs_bits(rhs)?
            ));
        }
        self.bits(expr, Some("result_width"))
    }

    fn rhs(&mut self, term: &Term) -> Result<String, String> {
        if term.ty.is_some() {
            return Err("right-hand type annotations are unsupported".into());
        }
        match &term.kind {
            TermKind::Binder { name, ty: None } => {
                let value = self
                    .binders
                    .get(name)
                    .ok_or_else(|| format!("unbound right-hand value '{name}'"))?
                    .clone();
                self.facts
                    .push(format!("(= ({}Type {value}) result_type)", self.prefix));
                Ok(value)
            }
            TermKind::Value(expr) => {
                self.facts.push(
                    "(= result_type (Apply (Ctor \"builtin.i\") (TCons (NatLit result_width) (TNil))))"
                        .into(),
                );
                Ok(format!(
                    "({}Constant result_type {})",
                    self.prefix,
                    self.rhs_bits(expr)?
                ))
            }
            TermKind::Operation {
                operator: Operator::Dialect { dialect, name },
                attributes,
                operands,
                dependencies,
            } if attributes.is_empty()
                && dependencies.is_empty()
                && matches!(operands.len(), 1 | 2) =>
            {
                let kind = format!("{dialect}.{name}");
                self.admissions.insert(kind.clone());
                let index = self.operators.len();
                let index = *self
                    .operators
                    .entry((kind, operands.len()))
                    .or_insert(index);
                let children = operands
                    .iter()
                    .map(|operand| self.rhs(operand))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "({}Op{index} result_type {})",
                    self.prefix,
                    children.join(" ")
                ))
            }
            _ => Err("unsupported eqsat right-hand expression".into()),
        }
    }

    fn term(&mut self, term: &Term) -> Result<String, String> {
        if term.ty.is_some() {
            return Err("term type annotations are unsupported".into());
        }
        match &term.kind {
            TermKind::Operation {
                operator: Operator::Dialect { dialect, name },
                attributes,
                operands,
                dependencies,
            } if attributes.is_empty() && dependencies.is_empty() => {
                let kind = format!("{dialect}.{name}");
                let index = self.operators.len();
                let index = *self
                    .operators
                    .entry((kind, operands.len()))
                    .or_insert(index);
                let children = operands
                    .iter()
                    .map(|operand| self.term(operand))
                    .collect::<Result<Vec<_>, _>>()?;
                let ty = if self.homogeneous {
                    "result_type".into()
                } else {
                    self.fresh()
                };
                Ok(format!(
                    "({}Op{index} {ty} {})",
                    self.prefix,
                    children.join(" ")
                ))
            }
            TermKind::Binder { name, ty } => {
                let value = if let Some(value) = self.binders.get(name) {
                    value.clone()
                } else {
                    let value = self.fresh();
                    self.binders.insert(name.clone(), value.clone());
                    value
                };
                if let Some(ty) = ty {
                    let width = match ty {
                        BindingType::Type(Type::Integer(width)) => width.clone(),
                        BindingType::Constant(Some(expr)) => match &expr.kind {
                            ExprKind::Name(name) => Width::Named(name.clone()),
                            ExprKind::Integer(value) => Width::Concrete(
                                u32::try_from(*value).map_err(|_| "invalid constant width")?,
                            ),
                            _ => return Err("unsupported constant width".into()),
                        },
                        BindingType::Constant(None) => Width::Any,
                        _ => return Err("only integer binders are supported".into()),
                    };
                    let width = match &width {
                        Width::Concrete(width) if *width > 0 => {
                            format!("(from-string \"{width}\")")
                        }
                        Width::Named(name) => format!("width_{name}"),
                        Width::Any => self.fresh(),
                        Width::Concrete(_) => {
                            return Err("zero-width integers are unsupported".into());
                        }
                    };
                    if matches!(ty, BindingType::Constant(_)) {
                        let bits = self.fresh();
                        let constant_ty = self.fresh();
                        self.facts.push(format!(
                            "(= {value} ({}Constant {constant_ty} {bits}))",
                            self.prefix
                        ));
                        self.constants.insert(
                            name.clone(),
                            ConstantBinding {
                                bits,
                                width: width.clone(),
                            },
                        );
                    }
                    self.facts.push(format!(
                        r#"(= ({}Type {value}) (Apply (Ctor "builtin.i") (TCons (NatLit {width}) (TNil))))"#,
                        self.prefix
                    ));
                }
                if self.homogeneous {
                    self.facts
                        .push(format!("(= ({}Type {value}) result_type)", self.prefix));
                }
                Ok(value)
            }
            TermKind::Value(expr) => {
                let value =
                    integer_literal(expr).ok_or("only literal integer constants are supported")?;
                let ty = if self.homogeneous {
                    "result_type".into()
                } else {
                    self.fresh()
                };
                let width = self.fresh();
                self.facts.push(format!(
                    "(= {ty} (Apply (Ctor \"builtin.i\") (TCons (NatLit {width}) (TNil))))"
                ));
                Ok(format!(
                    "({}Constant {ty} (eqsat-int-bits {width} \"$attr.signed\" \"{:016x}\"))",
                    self.prefix, value as u64
                ))
            }
            _ => Err("unsupported PDL pattern".into()),
        }
    }
}

/// Emit engine rules over TIR's IR relations for supported typed PDL equalities.
/// Unsupported pattern forms and proof contracts return an error.
pub fn emit_egglog(file: &File, prefix: &str) -> Result<CompiledEgglog, String> {
    let mut obligations = Vec::new();
    let mut requests = String::new();
    let mut operators = BTreeMap::new();
    let mut admissions = BTreeSet::new();
    let mut rewrites = String::new();
    for item in &file.items {
        let Item::Rule(rule) = item else {
            return Err("eqsat does not support type groups".into());
        };
        if !matches!(rule.kind, RuleKind::Equality(Direction::Forward))
            || !rule.requirements.is_empty()
            || rule.post_saturation
        {
            return Err(format!(
                "eqsat does not support the contract of '{}'",
                rule.name
            ));
        }
        if rule.proof() == Proof::Smt {
            let (source, obligation) = checked_rule(rule, prefix, obligations.len())?;
            requests.push_str(&source);
            obligations.push(obligation);
            continue;
        }
        if rule.proof() != Proof::Trusted {
            return Err(format!(
                "eqsat does not support the contract of '{}'",
                rule.name
            ));
        }
        let mut pattern = Pattern {
            prefix,
            operators: &mut operators,
            binders: BTreeMap::new(),
            facts: Vec::new(),
            next: 0,
            constants: BTreeMap::new(),
            homogeneous: !matches!(rule.rhs.kind, TermKind::Binder { .. }),
            admissions: &mut admissions,
        };
        let lhs = pattern.term(&rule.lhs)?;
        let rhs = pattern.rhs(&rule.rhs)?;
        let guards = rule
            .guards
            .iter()
            .map(|guard| pattern.guard(guard))
            .collect::<Result<Vec<_>, _>>()?
            .join("\n");
        let facts = pattern.facts.join("\n");
        rewrites.push_str(&format!(
            r#"
(rule (({prefix}Seed scope) ({prefix}Use scope op) (Result op slot value)
       (= term {lhs}) {facts} {guards}
       (= result_type (ValueType value)) (= result_type ({prefix}Type term)))
      ((union term {rhs})) :ruleset {prefix}rewrite)
"#
        ));
    }
    if !obligations.is_empty() {
        requests = format!(
            r#"
(ruleset {prefix}request-types)
(ruleset {prefix}requests)
(ruleset {prefix}bound-requests)
(ruleset {prefix}bound-request-types)
(relation {prefix}RequestScope (i64))
(relation {prefix}StateValue (i64))
(relation {prefix}UnsupportedAttribute (i64))
(rule (({prefix}RequestScope root) (= attr (Attribute op key)) (!= key "operand_segment_sizes"))
      (({prefix}UnsupportedAttribute op)) :ruleset {prefix}bound-request-types)
(rule (({prefix}RequestScope root) (= (ValueType value) (Apply (Ctor "builtin.state") (TCons resource (TNil)))))
      (({prefix}StateValue value)) :ruleset {prefix}bound-request-types)
(rule ((= attr (Attribute op key)) (!= key "operand_segment_sizes"))
      (({prefix}UnsupportedAttribute op)) :ruleset {prefix}request-types)
(rule ((= (ValueType value) (Apply (Ctor "builtin.state") (TCons resource (TNil)))))
      (({prefix}StateValue value)) :ruleset {prefix}request-types)
{requests}"#
        );
    }
    if rewrites.is_empty() && !obligations.is_empty() {
        return Ok(CompiledEgglog {
            source: requests,
            obligations,
        });
    }
    let admissions = admissions
        .iter()
        .flat_map(|kind| {
            ["SameOperandAndResultType", "Speculatable"].map(|interface| {
                format!("(check (Implements \"{kind}\" \"dyn tir::interfaces::{interface}\"))\n")
            })
        })
        .collect::<String>();
    let mut constructors = String::new();
    let mut origins = String::new();
    let mut choices = String::new();
    let mut projections = String::new();
    let mut opaque = String::new();
    let mut metadata = String::new();
    let mut typing = String::new();
    for ((kind, arity), index) in operators {
        metadata.push_str(&format!(
            "({prefix}Constructor \"{prefix}Op{index}\" \"{kind}\" {arity})\n"
        ));
        constructors.push_str(&format!(
            "({prefix}Op{index} TypeExpr {} :cost 1)\n",
            vec![format!("{prefix}Term"); arity].join(" ")
        ));
        let children = (0..arity)
            .map(|slot| format!("({prefix}Input operand{slot})"))
            .collect::<Vec<_>>()
            .join(" ");
        let operands = (0..arity)
            .map(|slot| format!("(= operand{slot} (Operand op {slot}))"))
            .collect::<Vec<_>>()
            .join("\n");
        let types = vec![format!("{prefix}Term"); arity].join(" ");
        let terms = (0..arity)
            .map(|slot| format!("child{slot}"))
            .collect::<Vec<_>>()
            .join(" ");
        typing.push_str(&format!("(rule ((= term ({prefix}Op{index} ty {terms}))) ((set ({prefix}Type term) ty)) :ruleset {prefix}rewrite)\n"));
        origins.push_str(&format!("(relation {prefix}OriginOp{index} (TypeExpr {types} i64))\n(relation {prefix}PickOp{index} (i64 TypeExpr {types}))\n"));
        choices.push_str(&format!(
            r#"
(rule (({prefix}PickOp{index} old ty {terms}) ({prefix}OriginOp{index} ty {terms} new)
       ({prefix}Available old new)) ((set ({prefix}Selected old) new)) :ruleset {prefix}choose)
"#
        ));
        projections.push_str(&format!(
            r#"
(rule (({prefix}Needed value) (Result op 0 value) (= (OpKind op) "{kind}")
       (not (Attribute op key))
       {operands} (not (Operand op {arity})) (= ty (ValueType value)))
      ((union ({prefix}Input value) ({prefix}Op{index} ty {children}))
       ({prefix}OriginOp{index} ty {children} value)) :ruleset {prefix}project)
"#
        ));
        opaque.push_str(&format!("(!= kind \"{kind}\") "));
        // An attributed operation is opaque even when the library names its kind.
        projections.push_str(&format!(
            r#"
(rule (({prefix}Needed value) (Result op slot value) (= (OpKind op) "{kind}")
       (= attr (Attribute op key)) (= ty (ValueType value)))
      ((union ({prefix}Input value) ({prefix}Opaque ty value))) :ruleset {prefix}project)
"#
        ));
    }
    let source = format!(
        r#"
{requests}
{admissions}
(datatype {prefix}Term ({prefix}Input i64 :unextractable)
  ({prefix}Arg TypeExpr i64 :cost 0) ({prefix}Opaque TypeExpr i64 :cost 1)
  ({prefix}Constant TypeExpr String :cost 1) {constructors})
(relation {prefix}Constructor (String String i64))
{metadata}
(function {prefix}Type ({prefix}Term) TypeExpr :no-merge)
(relation {prefix}ScopeRecord (i64))
(relation {prefix}ParentRecord (i64 i64))
(relation {prefix}Seed (i64))
(relation {prefix}Use (i64 i64))
(relation {prefix}YieldUse (i64 i64))
(relation {prefix}Bind (i64 i64 i64))
(relation {prefix}Assume (i64 i64 i64))
(relation {prefix}Defined ({prefix}Term))
(function {prefix}KnownOnes ({prefix}Term) String :merge (eqsat-bits-or old new))
(function {prefix}KnownZeros ({prefix}Term) String :merge (eqsat-bits-or old new))
(relation {prefix}Root (i64))
(relation {prefix}Output (i64 {prefix}Term))
(relation {prefix}Needed (i64))
(function {prefix}Selected (i64) i64 :merge (min old new))
(relation {prefix}Available (i64 i64))
(relation {prefix}PickArg (i64 TypeExpr i64))
(relation {prefix}PickOpaque (i64 TypeExpr i64))
(relation {prefix}PickConstant (i64 TypeExpr String))
(relation {prefix}OriginConstant (TypeExpr String i64))
{origins}
(ruleset {prefix}project)
(ruleset {prefix}rewrite)
(ruleset {prefix}output)
(ruleset {prefix}publish)
(ruleset {prefix}choose)
{typing}
(rule ((= term ({prefix}Input value)) (= ty (ValueType value))) ((set ({prefix}Type term) ty)) :ruleset {prefix}rewrite)
(rule ((= term ({prefix}Arg ty value))) ((set ({prefix}Type term) ty)) :ruleset {prefix}rewrite)
(rule ((= term ({prefix}Opaque ty value))) ((set ({prefix}Type term) ty)) :ruleset {prefix}rewrite)
(rule ((= term ({prefix}Constant ty bits))) ((set ({prefix}Type term) ty)) :ruleset {prefix}rewrite)
(rule (({prefix}Output old term)) (({prefix}Available old old)) :ruleset {prefix}output)
(rule (({prefix}Available old parent) (Result op slot parent) (= child (Operand op input)))
      (({prefix}Available old child)) :ruleset {prefix}output)
(rule (({prefix}PickArg old ty picked) ({prefix}Available old new)
       (= (ValueType new) ty) (= ({prefix}Input picked) ({prefix}Input new)))
      ((set ({prefix}Selected old) new)) :ruleset {prefix}choose)
(rule (({prefix}PickOpaque old ty new) ({prefix}Available old new))
      ((set ({prefix}Selected old) new)) :ruleset {prefix}choose)
(rule (({prefix}PickConstant old ty bits) ({prefix}OriginConstant ty bits new)
       ({prefix}Available old new)) ((set ({prefix}Selected old) new)) :ruleset {prefix}choose)
{choices}
(rule (({prefix}Seed scope) ({prefix}Use scope op) (Result op slot value))
      (({prefix}Needed value)) :ruleset {prefix}project)
(rule (({prefix}Needed value) (Result op slot value) (= child (Operand op input)))
      (({prefix}Needed child)) :ruleset {prefix}project)
(rule (({prefix}Needed value) (BlockParam block slot value) (= ty (ValueType value)))
      ((union ({prefix}Input value) ({prefix}Arg ty value))) :ruleset {prefix}project)
(rule (({prefix}Needed value) (RegionParam region slot value) (= ty (ValueType value)))
      ((union ({prefix}Input value) ({prefix}Arg ty value))) :ruleset {prefix}project)
(rule (({prefix}Needed value) (Result op slot value) (= kind (OpKind op)) {opaque} (= ty (ValueType value)))
      ((union ({prefix}Input value) ({prefix}Opaque ty value))) :ruleset {prefix}project)
(rule (({prefix}Needed value) (Result op 0 value) (= (OpKind op) "builtin.constant")
       (= (Attribute op "value") (IntegerAttr (Ctor meaning) storage_width bits))
       (= ty (ValueType value))
       (= ty (Apply (Ctor "builtin.i") (TCons (NatLit width) (TNil))))
       (= normalized (eqsat-int-bits width meaning bits)))
      ((union ({prefix}Input value) ({prefix}Constant ty normalized))
       ({prefix}OriginConstant ty normalized value)) :ruleset {prefix}project)
{projections}
{rewrites}
(rule (({prefix}Seed scope) ({prefix}Use scope op) (Result op slot value) (= term ({prefix}Input value)))
      (({prefix}Output value term)) :ruleset {prefix}output)
(rule (({prefix}Seed scope) ({prefix}Use scope owner) (= new ({prefix}Selected old)) (= old (Operand owner slot)) (!= old new))
      ((delete (Operand owner slot)) (set (Operand owner slot) new)) :ruleset {prefix}publish)
(rule (({prefix}Seed scope) ({prefix}YieldUse scope owner) (= new ({prefix}Selected old)) (= old (Yield owner slot)) (!= old new))
      ((delete (Yield owner slot)) (set (Yield owner slot) new)) :ruleset {prefix}publish)
"#
    );
    Ok(CompiledEgglog {
        source,
        obligations,
    })
}
