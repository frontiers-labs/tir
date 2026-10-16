use std::sync::Arc;

use smallvec::{SmallVec, smallvec};
use tir_egraph::{Atom, ColumnId, Externs, Guard, HeadOp, LabelFill, Plan, Query, Source};
use tir_egraph::{ClassId as Id, Engine, Label as ENode};
use tir_relational::{Cmp, Expr};

use super::state;
use crate::rewrite::{RuleSet, Seeded};
use crate::utils::APInt;
use crate::{
    ConstantFold, Context, Operation, TypeId,
    attributes::{AttributeValue, Predicate},
    builtin::{IntegerType, ops},
    sem::{IrOp, Kind, Prov, SemNode as Node, SymKind, Value, node::field},
};

/// The host functions instcombine's guards call. Both read the IR — an op's
/// constant folding, a gate's case values — which saturation never changes, so
/// a guard reading one filters the match rather than looking at the graph.
pub(crate) mod call {
    pub(crate) const FOLD: u32 = 0;
    pub(crate) const DECIDED_ARM: u32 = 1;
    /// The width of an integer type, for a rule that binds one by name.
    pub(crate) const INT_WIDTH_OF: u32 = 2;
    pub(crate) const FLOAT_WIDTH_OF: u32 = 3;
    pub(crate) const STATE_RESOURCE_OF: u32 = 4;
    /// The carrier a value of a type lives in, as one word; zero for none.
    pub(crate) const CARRIER_OF: u32 = 5;
    pub(crate) const INT_TYPE: u32 = 6;
    pub(crate) const POINTER_WIDTH_OF: u32 = 7;
    /// The offset in bits of a field, by type and field name, under the layout
    /// in scope.
    pub(crate) const OFFSET_OF: u32 = 8;
    /// The size in bits of a type under the layout in scope.
    pub(crate) const SIZE_OF: u32 = 9;
    /// Where the generated rules' own host functions start.
    pub(crate) const PDL: u32 = 16;
}

/// Fields of the scalars the two rules below pass around.
struct Scalars;

impl Scalars {
    const ROW: u32 = 0;
    const VALUE: u32 = 1;
    const WIDTH: u32 = 2;
    const TY: u32 = 3;
    const ARM: u32 = 2;
}

pub struct Interpretation {
    context: Context,
    /// The data layout's pointer width, which a pointer's carrier is.
    pointer_width: Option<u32>,
    pdl: fn(u32, &[u64], &mut [u64]) -> bool,
    /// The layout in scope, for a rule that asks one.
    layout: Option<crate::DataLayout>,
}

impl Interpretation {
    pub(crate) fn new(
        context: &Context,
        pointer_width: Option<u32>,
        pdl: fn(u32, &[u64], &mut [u64]) -> bool,
    ) -> Self {
        Self {
            context: context.clone(),
            pointer_width,
            pdl,
            layout: None,
        }
    }

    pub(crate) fn with_layout(self, layout: Option<crate::DataLayout>) -> Self {
        Self { layout, ..self }
    }
}

impl Externs<Node> for Interpretation {
    fn call(&self, id: u32, terms: &[&Node], args: &[u64], out: &mut [u64]) -> bool {
        match id {
            call::FOLD => fold_constant(&self.context, terms, args, out),
            call::DECIDED_ARM => self.decided_arm(terms, args, out),
            call::INT_WIDTH_OF => match class_int_width_of(&self.context, args[0] as u32) {
                Some(width) => {
                    out[0] = width as u64;
                    true
                }
                None => false,
            },
            call::FLOAT_WIDTH_OF => match class_float_width_of(&self.context, args[0] as u32) {
                Some(width) => {
                    out[0] = width as u64;
                    true
                }
                None => false,
            },
            call::STATE_RESOURCE_OF => match class_state_resource_of(args[0] as u32) {
                Some(resource) => {
                    out[0] = resource;
                    true
                }
                None => false,
            },
            call::CARRIER_OF => {
                out[0] = crate::sem::node::carrier_word(crate::sem::egraph::carrier_of(
                    &self.context,
                    self.pointer_width,
                    TypeId::from_number(args[0] as u32),
                ));
                true
            }
            call::INT_TYPE => {
                out[0] = u64::from(self.context.terms().int(args[0]).number());
                true
            }
            call::POINTER_WIDTH_OF => {
                let ty = TypeId::from_number(args[0] as u32);
                match (
                    crate::ptr::PtrType::view(&self.context, ty),
                    self.pointer_width,
                ) {
                    (Some(_), Some(width)) => {
                        out[0] = u64::from(width);
                        true
                    }
                    _ => false,
                }
            }
            call::OFFSET_OF | call::SIZE_OF => {
                let Some(layout) = &self.layout else {
                    return false;
                };
                let ty = TypeId::from_number(args[0] as u32);
                let bits = match id {
                    call::OFFSET_OF => self
                        .context
                        .names()
                        .name_at(args[1] as usize)
                        .and_then(|field| layout.offset_in_bits(ty, &field)),
                    _ => layout.layout_of(ty).map(|layout| layout.size),
                };
                bits.map(|bits| out[0] = bits).is_some()
            }
            generated if generated >= call::PDL => (self.pdl)(generated - call::PDL, args, out),
            _ => false,
        }
    }
}

pub(crate) fn fold_constant(
    context: &Context,
    terms: &[&Node],
    args: &[u64],
    out: &mut [u64],
) -> bool {
    let [folded] = terms else {
        return false;
    };
    let (Prov::Op(op), Some(ty)) = (folded.prov, folded.op_type()) else {
        return false;
    };
    // A folded class materializes as `builtin.constant`, which only holds an
    // integer, so an op computing anything else (an address, say) must keep
    // its own form however constant its operands are.
    if !context.has_operation(op) || !produces_integer(context, op) {
        return false;
    }
    let instance = context.get_op(op);
    let Some(values): Option<Vec<Value>> = instance
        .operands()
        .iter()
        .zip(args)
        .map(|(&operand, &value)| {
            let ty = context.get_value(operand).ty();
            context
                .terms()
                .bit_width(ty)
                .map(|width| Value::Int(APInt::new(width, value)))
        })
        .collect()
    else {
        return false;
    };
    let Some(Value::Int(value)) = instance
        .as_interface::<dyn ConstantFold>()
        .and_then(|folder| folder.fold(&values))
    else {
        return false;
    };
    // The value travels as one word: a wider result past it stays unfolded.
    if value.limbs()[1..].iter().any(|&limb| limb != 0) {
        return false;
    }
    out.copy_from_slice(&[value.to_u64(), value.width(), ty.number() as u64]);
    true
}

impl Interpretation {
    fn decided_arm(&self, terms: &[&Node], args: &[u64], out: &mut [u64]) -> bool {
        let [gate] = terms else { return false };
        let &[value, arms] = args else {
            return false;
        };
        let Some((cases, width)) = gate_cases(&self.context, gate) else {
            return false;
        };
        let width = u64::from(width);
        if cases.len() as u64 != arms {
            return false;
        }
        let mask = if width >= 64 {
            u64::MAX
        } else {
            (1u64 << width) - 1
        };
        let Some(index) = cases
            .iter()
            .position(|case| case.is_some_and(|case| value == case as u64 & mask))
            .or_else(|| cases.iter().position(Option::is_none))
        else {
            return false;
        };
        out[0] = index as u64;
        true
    }
}

/// The generated rules, compiled once per pass and shared by every function
/// the pass runs on.
pub struct Ruleset {
    pub(crate) rewrites: Vec<Arc<tir_egraph::Rule<Node>>>,
    // The rules stated `proof trusted`, which no solver checked.
    pub(crate) trusted: Vec<Arc<tir_egraph::Rule<Node>>>,
}

impl Ruleset {
    pub(crate) fn new() -> Self {
        Self {
            rewrites: Vec::new(),
            trusted: Vec::new(),
        }
    }

    pub(crate) fn push_query(&mut self, rule: tir_egraph::Rule<Node>) {
        self.rewrites.push(Arc::new(rule));
    }

    pub(crate) fn push_trusted(&mut self, rule: tir_egraph::Rule<Node>) {
        self.trusted.push(Arc::new(rule));
    }
}

/// What the pass compiles once and every function saturates with: the
/// generated rules, and each rule a function's graph called for, so a rule
/// keeps one plan, and the engine's cache of it, for the life of the pass.
#[derive(Clone, Default)]
pub struct Shared {
    generated: Option<Arc<Ruleset>>,
    folds: Vec<(Node, Arc<tir_egraph::Rule<Node>>)>,
    gammas: Vec<(usize, Arc<tir_egraph::Rule<Node>>)>,
    fixed: Vec<Arc<tir_egraph::Rule<Node>>>,
}

/// The rules one function saturates with: the shared generated ones first,
/// the trusted ones among them only when `trusted`, then the ones its own
/// graph calls for.
pub fn builtin_ruleset(
    context: &Context,
    seeded: &Seeded,
    shared: &mut Shared,
    trusted: bool,
) -> RuleSet {
    let eg = &seeded.eg;
    let generated = shared
        .generated
        .get_or_insert_with(|| context.compiled_rules(|| generated_ruleset(context)))
        .clone();
    let mut rules = generated.rewrites.clone();
    if trusted {
        rules.extend(generated.trusted.iter().cloned());
    }
    for template in fold_templates(eg) {
        let known = shared.folds.iter().position(|(seen, _)| {
            seen.matches(&template) && seen.children().len() == template.children().len()
        });
        let known = known.unwrap_or_else(|| {
            let rule = Arc::new(const_fold(&template));
            shared.folds.push((template, rule));
            shared.folds.len() - 1
        });
        rules.push(shared.folds[known].1.clone());
    }
    for arity in gamma_arities(eg) {
        let known = shared.gammas.iter().position(|&(seen, _)| seen == arity);
        let known = known.unwrap_or_else(|| {
            shared.gammas.push((arity, Arc::new(decided_gamma(arity))));
            shared.gammas.len() - 1
        });
        rules.push(shared.gammas[known].1.clone());
    }
    if shared.fixed.is_empty() {
        for (predicate, complement) in COMPLEMENTS {
            let rule = cmp_complement(context, predicate, complement);
            shared.fixed.push(Arc::new(rule));
        }
        shared.fixed.push(Arc::new(state::forward_load()));
    }
    rules.extend(shared.fixed.iter().cloned());
    let interpretation = Interpretation::new(context, seeded.pointer_width, pdl_extern);
    RuleSet {
        rules,
        tables: Vec::new(),
        externs: Box::new(interpretation),
    }
}

fn fold_templates(eg: &Engine<Node>) -> Vec<Node> {
    let mut templates: Vec<Node> = Vec::new();
    for class in eg.classes() {
        for node in class.nodes() {
            if !matches!(node.prov, Prov::Op(_))
                || node.op_type().is_none()
                || node.kind.ir().is_some_and(|op| op.law.is_some())
            {
                continue;
            }
            let template = node
                .op_template(node.children().to_vec())
                .expect("an op node has an op template");
            if !templates.iter().any(|seen| {
                seen.matches(&template) && seen.children().len() == template.children().len()
            }) {
                templates.push(template);
            }
        }
    }
    templates
}

pub(crate) fn const_fold(template: &Node) -> tir_egraph::Rule<Node> {
    let arity = template.children().len() as u32;
    // Variables: 0 the root, 1..=arity its operands, then the folded class.
    let operands: SmallVec<[u32; 4]> = (1..=arity).collect();
    let mut root = template.clone();
    root.children_mut()
        .iter_mut()
        .zip(&operands)
        .for_each(|(child, &var)| *child = Id::from_raw(var));
    let mut atoms = vec![Atom::Node {
        template: root,
        args: operands.clone(),
        class: 0,
        row: Some(Scalars::ROW),
    }];
    let mut guards = Vec::new();
    let mut values: SmallVec<[Expr; 4]> = SmallVec::new();
    // One scalar per operand, above the fixed slots.
    for (index, &var) in operands.iter().enumerate() {
        let slot = Scalars::TY + 1 + index as u32;
        atoms.push(Atom::Const {
            key: var,
            value: slot,
        });
        values.push(Expr::Scalar(slot));
    }
    guards.push(Guard::Extern {
        call: call::FOLD,
        terms: smallvec![Source::Row(Scalars::ROW)],
        args: values,
        out: smallvec![Scalars::VALUE, Scalars::WIDTH, Scalars::TY],
    });
    tir_egraph::Rule {
        name: "const-fold".into(),
        plan: Plan::compile(Query {
            vars: arity + 2,
            scalars: Scalars::TY + 1 + arity,
            root: 0,
            atoms,
            guards,
            nots: Vec::new(),
        }),
        head: vec![
            HeadOp::Insert {
                label: LabelFill {
                    template: konst(APInt::new(1, 0)),
                    fills: smallvec![
                        (field::INT_VALUE, Scalars::VALUE),
                        (field::INT_WIDTH, Scalars::WIDTH),
                        (field::TY, Scalars::TY),
                    ],
                },
                args: SmallVec::new(),
                into: arity + 1,
            },
            HeadOp::Union(0, arity + 1),
        ],
        head_vars: 0,
        post_saturation: false,
    }
}

fn gamma_arities(eg: &Engine<Node>) -> Vec<usize> {
    let mut arities: Vec<usize> = Vec::new();
    for class in eg.classes() {
        for node in class.nodes() {
            if node.sym() == Some(SymKind::If) && !arities.contains(&node.children().len()) {
                arities.push(node.children().len());
            }
        }
    }
    arities
}

fn decided_gamma(arity: usize) -> tir_egraph::Rule<Node> {
    let arity = arity as u32;
    // Variables: 0 the root, 1 the decision, 2.. the arms.
    let args: SmallVec<[u32; 4]> = (1..=arity).collect();
    tir_egraph::Rule {
        name: "gamma-decided".into(),
        plan: Plan::compile(Query {
            vars: arity + 1,
            scalars: 3,
            root: 0,
            atoms: vec![
                Atom::Node {
                    template: Node::gamma_pattern(
                        args.iter().map(|&var| Id::from_raw(var)).collect(),
                    ),
                    args: args.clone(),
                    class: 0,
                    row: Some(Scalars::ROW),
                },
                Atom::Const {
                    key: 1,
                    value: Scalars::VALUE,
                },
            ],
            guards: vec![Guard::Extern {
                call: call::DECIDED_ARM,
                terms: smallvec![Source::Row(Scalars::ROW)],
                args: smallvec![Expr::Scalar(Scalars::VALUE), Expr::Lit(arity as i64 - 1),],
                out: smallvec![Scalars::ARM],
            }],
            nots: Vec::new(),
        }),
        head: vec![HeadOp::UnionIndexed {
            class: 0,
            offset: 2,
            index: Scalars::ARM,
        }],
        head_vars: 0,
        post_saturation: false,
    }
}

fn gate_cases(context: &Context, node: &Node) -> Option<(Vec<Option<i64>>, u32)> {
    let gate = match node.prov {
        Prov::Op(gate) => gate,
        Prov::Value(value) => context.get_value(value).defining_op()?,
        Prov::None | Prov::Introduced => return None,
    };
    if !context.has_operation(gate) {
        return None;
    }
    let instance = context.get_op(gate);
    // A γ selects its arms by case, the last arm taking every other value. The
    // seeding spells a boolean one, which indexes its arms, as
    // `If(p, arm 1, arm 0)`, so its node lists the case-1 arm first.
    let gamma = instance.as_interface::<dyn crate::Gamma>()?;
    let arms = gamma.arms().len();
    let width = context
        .terms()
        .bit_width(context.get_value(gamma.predicate()).ty())? as u32;
    let cases = if width == 1 && arms == 2 {
        vec![Some(1), None]
    } else {
        gamma
            .cases()
            .into_iter()
            .map(|case| Some(case as i64))
            .chain([None])
            .collect()
    };
    Some((cases, width))
}

/// Each comparison predicate paired with its negation at the same operand order:
/// `!(a < b)` is `a >= b`. Both directions, so knowing either settles the other.
pub(crate) const COMPLEMENTS: [(Predicate, Predicate); 10] = [
    (Predicate::Eq, Predicate::Ne),
    (Predicate::Ne, Predicate::Eq),
    (Predicate::Slt, Predicate::Sge),
    (Predicate::Sge, Predicate::Slt),
    (Predicate::Sle, Predicate::Sgt),
    (Predicate::Sgt, Predicate::Sle),
    (Predicate::Ult, Predicate::Uge),
    (Predicate::Uge, Predicate::Ult),
    (Predicate::Ule, Predicate::Ugt),
    (Predicate::Ugt, Predicate::Ule),
];

fn cmp_complement(
    context: &Context,
    predicate: Predicate,
    complement: Predicate,
) -> tir_egraph::Rule<Node> {
    // Variables: 0 the settled comparison, 1 and 2 its operands, 3 the
    // complement, 4 the constant the head mints.
    let operands = vec![Id::from_raw(1), Id::from_raw(2)];
    tir_egraph::Rule {
        name: "cmp-complement".into(),
        plan: Plan::compile(Query {
            vars: 5,
            scalars: 6,
            root: 0,
            atoms: vec![
                Atom::Node {
                    template: cmpi(context, predicate, None, operands.clone()),
                    args: smallvec![1, 2],
                    class: 0,
                    row: Some(Complement::ROW),
                },
                Atom::Const {
                    key: 0,
                    value: Complement::VALUE,
                },
                Atom::Node {
                    template: cmpi(context, complement, None, operands),
                    args: smallvec![1, 2],
                    class: 3,
                    row: Some(Complement::OTHER_ROW),
                },
            ],
            guards: vec![
                Guard::Let {
                    out: Complement::NEGATED,
                    value: Expr::IsZero(Box::new(Expr::Scalar(Complement::VALUE))),
                },
                Guard::Read {
                    term: Source::Row(Complement::ROW),
                    field: field::TY,
                    out: Complement::TY,
                },
                Guard::Read {
                    term: Source::Row(Complement::OTHER_ROW),
                    field: field::TY,
                    out: Complement::OTHER_TY,
                },
                // Both spellings answer at the same width, or they are not the
                // same question.
                Guard::Cmp(
                    tir_relational::Cmp::Eq,
                    Expr::Scalar(Complement::TY),
                    Expr::Scalar(Complement::OTHER_TY),
                ),
            ],
            nots: Vec::new(),
        }),
        head: vec![
            HeadOp::Insert {
                label: LabelFill {
                    template: konst(APInt::new(1, 0)),
                    fills: smallvec![(field::INT_VALUE, Complement::NEGATED)],
                },
                args: SmallVec::new(),
                into: 4,
            },
            HeadOp::Union(3, 4),
        ],
        head_vars: 0,
        post_saturation: false,
    }
}

/// The scalar slots [`cmp_complement`] names.
struct Complement;

impl Complement {
    const ROW: u32 = 0;
    const VALUE: u32 = 1;
    const NEGATED: u32 = 2;
    const OTHER_ROW: u32 = 3;
    const TY: u32 = 4;
    const OTHER_TY: u32 = 5;
}

fn cmpi(context: &Context, predicate: Predicate, ty: Option<TypeId>, children: Vec<Id>) -> Node {
    Node {
        kind: Kind::Ir(IrOp::template(
            ops::CmpIOp::dialect(),
            ops::CmpIOp::name(),
            vec![context.named_attribute("predicate", AttributeValue::Predicate(predicate))],
        )),
        payload: None,
        ty,
        children,
        prov: Prov::None,
        carrier: None,
    }
}

fn produces_integer(context: &Context, op: crate::OpId) -> bool {
    let instance = context.get_op(op);
    instance
        .value_results()
        .first()
        .is_some_and(|&result| IntegerType::view(context, context.get_value(result).ty()).is_some())
}

fn class_int_width_of(context: &Context, ty: u32) -> Option<u32> {
    let ty = TypeId::from_number(ty);
    if context.is_state_type(ty) {
        return None;
    }
    IntegerType::view(context, ty).and_then(|int| u32::try_from(int.width()).ok())
}

fn class_float_width_of(context: &Context, ty: u32) -> Option<u32> {
    let ty = TypeId::from_number(ty);
    crate::builtin::FloatType::view(context, ty).and_then(|float| {
        match (float.exp_width(), float.mant_width()) {
            (8, 23) => Some(32),
            (11, 52) => Some(64),
            _ => None,
        }
    })
}

fn class_state_resource_of(ty: u32) -> Option<u64> {
    let ty = TypeId::from_number(ty);
    crate::builtin::StateType::view(ty).map(|state| state.resource().semantic_code())
}

pub(crate) fn konst(value: APInt) -> Node {
    Node::constant(value, Prov::None)
}

pub(crate) fn constant_operand_kept<O: Operation>(
    context: &Context,
    operand: usize,
    zero: bool,
) -> bool {
    let Some(law) = IrOp::declared(context, (O::dialect(), O::name()), Vec::new(), 0).law else {
        return true;
    };
    law.offset_coefficient(operand).is_none() || (zero && law.identity(operand).is_none())
}

include!(concat!(env!("OUT_DIR"), "/instcombine_rules.rs"));
