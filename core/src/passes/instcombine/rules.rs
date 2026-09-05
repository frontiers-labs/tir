use smallvec::{SmallVec, smallvec};
use tir_relational::{
    Atom, Cmp, ColumnId, Expr, Externs, Guard, HeadOp, LabelFill, Plan, Query, Source,
};
use tir_relational::{ClassId as Id, Engine, Label as ENode};

use super::seed::Seeded;
use super::state;
use crate::utils::APInt;
use crate::{
    Conditional, ConstantFold, Context, Operation, OperationRef, PassError, Rewriter, TypeId,
    ValueId,
    attributes::{AttributeValue, Predicate},
    builtin::{IntegerType, ops},
    sem::{IrOp, Kind, Prov, SemNode as Node, SymKind, Value, node::field},
};

/// The host functions instcombine's guards call. Both read the IR — an op's
/// constant folding, a gate's case values — which saturation never changes, so
/// a guard reading one filters the match rather than looking at the graph.
mod call {
    pub(super) const FOLD: u32 = 0;
    pub(super) const DECIDED_ARM: u32 = 1;
    /// The width of an integer type, for a rule that binds one by name.
    pub(super) const INT_WIDTH_OF: u32 = 2;
    /// Where the generated rules' own host functions start.
    pub(super) const PDL: u32 = 16;
}

/// Fields of the scalars the two rules below pass around.
struct Scalars;

impl Scalars {
    const ROW: u32 = 0;
    const CONST: u32 = 1;
    const VALUE: u32 = 2;
    const WIDTH: u32 = 3;
    const TY: u32 = 4;
    const ARM: u32 = 2;
}

pub struct Interpretation {
    context: Context,
}

impl Externs<Node> for Interpretation {
    fn call(&self, id: u32, terms: &[&Node], args: &[u64], out: &mut [u64]) -> bool {
        match id {
            call::FOLD => self.fold(terms, out),
            call::DECIDED_ARM => self.decided_arm(terms, args, out),
            call::INT_WIDTH_OF => match class_int_width_of(&self.context, args[0] as u32) {
                Some(width) => {
                    out[0] = width as u64;
                    true
                }
                None => false,
            },
            generated if generated >= call::PDL => pdl_extern(generated - call::PDL, args, out),
            _ => false,
        }
    }
}

impl Interpretation {
    /// The value the op `terms[0]` computes from the constants `terms[1..]`, as
    /// value, width and result type.
    fn fold(&self, terms: &[&Node], out: &mut [u64]) -> bool {
        let [folded, operands @ ..] = terms else {
            return false;
        };
        let (Prov::Op(op), Some(ty)) = (folded.prov, folded.op_type()) else {
            return false;
        };
        // A folded class materializes as `builtin.constant`, which only holds an
        // integer, so an op computing anything else (an address, say) must keep
        // its own form however constant its operands are.
        if !self.context.has_operation(op) || !produces_integer(&self.context, op) {
            return false;
        }
        let Some(values): Option<Vec<Value>> = operands
            .iter()
            .map(|node| node.int().cloned().map(Value::Int))
            .collect()
        else {
            return false;
        };
        let instance = self.context.get_op(op);
        let Some(Value::Int(value)) = instance
            .as_interface::<dyn ConstantFold>()
            .and_then(|folder| folder.fold(&values))
        else {
            return false;
        };
        out.copy_from_slice(&[value.to_u64(), value.width() as u64, ty.number() as u64]);
        true
    }

    /// Which arm of the gate `terms[0]` the decision `args[0]` at `args[1]` bits
    /// selects: the arm whose case value it equals, or the default when none
    /// does. `args[2]` is how many arms the rule's shape has.
    fn decided_arm(&self, terms: &[&Node], args: &[u64], out: &mut [u64]) -> bool {
        let [gate] = terms else { return false };
        let &[value, width, arms] = args else {
            return false;
        };
        let Some(cases) = gate_cases(&self.context, gate) else {
            return false;
        };
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

pub type EmitFn = Box<
    dyn Fn(&Context, &[ValueId], TypeId, &OperationRef, &mut Rewriter) -> Result<ValueId, PassError>
        + Send
        + Sync,
>;

pub struct Ruleset {
    pub rewrites: Vec<tir_relational::Rule<Node>>,
    pub emits: Vec<Option<EmitFn>>,
    /// What the rules' guards ask the IR.
    pub interpretation: Interpretation,
}

impl Ruleset {
    fn new(context: &Context) -> Self {
        Self {
            rewrites: Vec::new(),
            emits: Vec::new(),
            interpretation: Interpretation {
                context: context.clone(),
            },
        }
    }

    fn push_query(&mut self, rule: tir_relational::Rule<Node>, emit: Option<EmitFn>) {
        self.rewrites.push(rule);
        self.emits.push(emit);
    }
}

pub fn builtin_ruleset(context: &Context, seeded: &Seeded) -> Ruleset {
    let eg = &seeded.eg;
    let mut ruleset = generated_ruleset(context);
    for template in fold_templates(eg) {
        ruleset.push_query(const_fold(&template), None);
    }
    for arity in gamma_arities(eg) {
        ruleset.push_query(decided_gamma(arity), None);
    }
    for (predicate, complement) in COMPLEMENTS {
        ruleset.push_query(cmp_complement(context, predicate, complement), None);
    }
    ruleset.push_query(state::pointer_derivation(), None);
    ruleset.push_query(state::forward_load(), None);
    ruleset
}

fn emit_shl() -> EmitFn {
    Box::new(|context, operands, ty, target, rewriter| {
        let op = ops::shli(context, operands[0], operands[1], ty).build();
        rewriter.insert_op_before(target, &op)?;
        Ok(op.result())
    })
}

/// One LHS template per distinct seeded-op signature in `eg`, so const folding
/// searches the classes holding such an op instead of every class. Only a seeded
/// op ever folds and rewrites introduce none, so the seeded graph fixes the set.
fn fold_templates(eg: &Engine<Node>) -> Vec<Node> {
    let mut templates: Vec<Node> = Vec::new();
    for class in eg.classes() {
        for node in class.nodes() {
            if !matches!(node.prov, Prov::Op(_)) || node.op_type().is_none() {
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

/// An op every operand of which is a known constant is that constant. What the
/// value is comes from the op's own [`ConstantFold`]; what the rule reads is the
/// matched row and its operands' constant facts, and nothing else.
fn const_fold(template: &Node) -> tir_relational::Rule<Node> {
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
    let mut folded: SmallVec<[Source; 2]> = smallvec![Source::Row(Scalars::ROW)];
    // One scalar pair per operand, above the fixed slots.
    for (index, &var) in operands.iter().enumerate() {
        let slot = Scalars::TY + 1 + index as u32;
        atoms.push(Atom::Fact {
            column: ColumnId::Const,
            key: var,
            value: slot,
        });
        folded.push(Source::Label(slot));
    }
    guards.push(Guard::Extern {
        call: call::FOLD,
        terms: folded,
        args: SmallVec::new(),
        out: smallvec![Scalars::VALUE, Scalars::WIDTH, Scalars::TY],
    });
    tir_relational::Rule {
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

/// The γ arities the seeded graph holds, so a decided gate is searched at the
/// widths it actually occurs at. Only a gate the seeding built is ever decided and
/// rewrites introduce no new arity, so the seeded graph fixes the set.
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

/// A gate whose decision is a known constant publishes what the arm that constant
/// selects yields: the arm whose *case value* the decision equals, or the default
/// when none does. Reading the decision as a boolean holds only for a two-armed
/// `if`, whose case values are exactly `1` and the default; a switch names its own,
/// and `case 0` is its first arm rather than the one no case matched.
fn decided_gamma(arity: usize) -> tir_relational::Rule<Node> {
    let arity = arity as u32;
    // Variables: 0 the root, 1 the decision, 2.. the arms.
    let args: SmallVec<[u32; 4]> = (1..=arity).collect();
    tir_relational::Rule {
        name: "gamma-decided".into(),
        plan: Plan::compile(Query {
            vars: arity + 1,
            scalars: 4,
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
                Atom::Fact {
                    column: ColumnId::Const,
                    key: 1,
                    value: Scalars::CONST,
                },
            ],
            guards: vec![
                Guard::Read {
                    term: Source::Label(Scalars::CONST),
                    field: field::INT_VALUE,
                    out: Scalars::VALUE,
                },
                Guard::Read {
                    term: Source::Label(Scalars::CONST),
                    field: field::INT_WIDTH,
                    out: Scalars::WIDTH,
                },
                Guard::Extern {
                    call: call::DECIDED_ARM,
                    terms: smallvec![Source::Row(Scalars::ROW)],
                    args: smallvec![
                        Expr::Scalar(Scalars::VALUE),
                        Expr::Scalar(Scalars::WIDTH),
                        Expr::Lit(arity as i64 - 1),
                    ],
                    out: smallvec![Scalars::ARM],
                },
            ],
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

/// The case value selecting each arm of the gate `node` stands for, in arm order.
fn gate_cases(context: &Context, node: &Node) -> Option<Vec<Option<i64>>> {
    let gate = match node.prov {
        Prov::Op(gate) => gate,
        Prov::Value(value) => context.get_value(value).defining_op()?,
        Prov::None | Prov::Introduced(_) => return None,
    };
    if !context.has_operation(gate) {
        return None;
    }
    Some(
        context
            .get_op(gate)
            .as_interface::<dyn Conditional>()?
            .case_values()
            .into_iter()
            .map(|(_, case)| case)
            .collect(),
    )
}

/// Each comparison predicate paired with its negation at the same operand order:
/// `!(a < b)` is `a >= b`. Both directions, so knowing either settles the other.
const COMPLEMENTS: [(Predicate, Predicate); 10] = [
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

/// A comparison and its complement over the same operands are one fact: whichever
/// of the two a scope or a constant settles, the other is its negation. A
/// comparison's identity is its predicate attribute, and the PDL backend generates
/// neither attribute matching nor attribute emission, so this family is written
/// here instead of in `rules.pdl`.
/// A settled comparison decides its complement. The complement is reached
/// sideways — through the back-edges of an operand this match already bound —
/// rather than by a scan of every class holding that predicate.
fn cmp_complement(
    context: &Context,
    predicate: Predicate,
    complement: Predicate,
) -> tir_relational::Rule<Node> {
    // Variables: 0 the settled comparison, 1 and 2 its operands, 3 the
    // complement, 4 the constant the head mints.
    let operands = vec![Id::from_raw(1), Id::from_raw(2)];
    tir_relational::Rule {
        name: "cmp-complement".into(),
        plan: Plan::compile(Query {
            vars: 5,
            scalars: 8,
            root: 0,
            atoms: vec![
                Atom::Node {
                    template: cmpi(context, predicate, None, operands.clone()),
                    args: smallvec![1, 2],
                    class: 0,
                    row: Some(Complement::ROW),
                },
                Atom::Fact {
                    column: ColumnId::Const,
                    key: 0,
                    value: Complement::CONST,
                },
                Atom::Node {
                    template: cmpi(context, complement, None, operands),
                    args: smallvec![1, 2],
                    class: 3,
                    row: Some(Complement::OTHER_ROW),
                },
            ],
            guards: vec![
                Guard::Read {
                    term: Source::Label(Complement::CONST),
                    field: field::INT_VALUE,
                    out: Complement::VALUE,
                },
                Guard::Read {
                    term: Source::Label(Complement::CONST),
                    field: field::INT_WIDTH,
                    out: Complement::WIDTH,
                },
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
                    fills: smallvec![
                        (field::INT_VALUE, Complement::NEGATED),
                        (field::INT_WIDTH, Complement::WIDTH),
                    ],
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
    const CONST: u32 = 1;
    const VALUE: u32 = 2;
    const WIDTH: u32 = 3;
    const NEGATED: u32 = 4;
    const OTHER_ROW: u32 = 5;
    const TY: u32 = 6;
    const OTHER_TY: u32 = 7;
}

/// The `builtin.cmpi` node of `predicate` over `children`, at `ty` — `None` for a
/// pattern template, which matches the comparison at any result type.
fn cmpi(context: &Context, predicate: Predicate, ty: Option<TypeId>, children: Vec<Id>) -> Node {
    Node {
        kind: Kind::Ir(IrOp {
            dialect: ops::CmpIOp::dialect(),
            name: ops::CmpIOp::name(),
            attrs: vec![context.named_attribute("predicate", AttributeValue::Predicate(predicate))],
            commutative: false,
            cost: 0,
        }),
        payload: None,
        ty,
        children,
        prov: Prov::None,
    }
}

fn produces_integer(context: &Context, op: crate::OpId) -> bool {
    let instance = context.get_op(op);
    instance.results().first().is_some_and(|&result| {
        let ty = context.get_type_data(context.get_value(result).ty());
        (ty.as_ref() as &dyn std::any::Any)
            .downcast_ref::<IntegerType>()
            .is_some()
    })
}

/// The width of an integer type, by number; `None` for anything else.
fn class_int_width_of(context: &Context, ty: u32) -> Option<u32> {
    let ty = TypeId::from_number(ty);
    (context.get_type_data(ty).as_ref() as &dyn std::any::Any)
        .downcast_ref::<IntegerType>()
        .map(IntegerType::width)
}

fn konst(value: APInt) -> Node {
    Node::constant(value, Prov::None)
}

include!(concat!(env!("OUT_DIR"), "/instcombine_rules.rs"));
