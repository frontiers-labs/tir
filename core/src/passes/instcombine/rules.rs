use std::collections::HashMap;

use tir_symbolic::egraph::{EGraph, ENode, Id, Pattern, Rewrite, Rhs, Substitution, Var};

use super::seed::Seeded;
use super::state;
use crate::utils::APInt;
use crate::{
    Conditional, ConstantFold, Context, OperationRef, PassError, Rewriter, TypeId, ValueId,
    builtin::{IntegerType, ops},
    sem::{Prov, SemNode as Node, SymKind, Value},
};

pub(crate) type Sym = u32;
pub(crate) type Rule = Rewrite<Node, Sym>;

pub type EmitFn = Box<
    dyn Fn(&Context, &[ValueId], TypeId, &OperationRef, &mut Rewriter) -> Result<ValueId, PassError>
        + Send
        + Sync,
>;

pub struct Ruleset {
    pub rewrites: Vec<Rule>,
    pub emits: Vec<Option<EmitFn>>,
}

impl Ruleset {
    fn new() -> Self {
        Self {
            rewrites: Vec::new(),
            emits: Vec::new(),
        }
    }

    fn push(&mut self, rewrite: Rule, emit: Option<EmitFn>) {
        self.rewrites.push(rewrite);
        self.emits.push(emit);
    }
}

pub fn builtin_ruleset(context: &Context, seeded: &Seeded) -> Ruleset {
    let eg = &seeded.eg;
    let mut ruleset = generated_ruleset(context);
    for template in fold_templates(eg) {
        ruleset.push(const_fold(context.clone(), &template), None);
    }
    for arity in gamma_arities(eg) {
        ruleset.push(decided_gamma(context.clone(), arity), None);
    }
    ruleset.push(state::forward_load(context.clone()), None);
    ruleset.push(
        state::eliminate_dead_store(seeded.exported_states.clone()),
        None,
    );
    ruleset
}

pub(crate) fn operand(substitution: &Substitution<Sym>, index: u32) -> Id {
    substitution
        .get(&Var::Symbol(index))
        .expect("bound operand")
}

fn const_value(eg: &EGraph<Node>, class: Id) -> Option<APInt> {
    eg.nodes(eg.find(class))
        .iter()
        .find_map(|node| node.int().cloned())
}

fn class_type(eg: &EGraph<Node>, class: Id) -> Option<TypeId> {
    eg.nodes(eg.find(class)).iter().find_map(Node::op_type)
}

fn class_int_width(context: &Context, eg: &EGraph<Node>, class: Id) -> Option<u32> {
    class_value_type(context, eg, class).and_then(|ty| {
        (context.get_type_data(ty).as_ref() as &dyn std::any::Any)
            .downcast_ref::<IntegerType>()
            .map(IntegerType::width)
    })
}

pub(crate) fn class_value_type(context: &Context, eg: &EGraph<Node>, class: Id) -> Option<TypeId> {
    eg.nodes(eg.find(class)).iter().find_map(|node| {
        node.ty
            .or_else(|| node.value().map(|value| context.get_value(value).ty()))
    })
}

fn bind_width(widths: &mut HashMap<&'static str, u32>, name: &'static str, width: u32) -> bool {
    widths.get(name).is_none_or(|bound| *bound == width) && {
        widths.insert(name, width);
        true
    }
}

fn class_is_literal(eg: &EGraph<Node>, class: Id, literal: i64) -> bool {
    const_value(eg, class).is_some_and(|value| {
        let mask = if value.width() == 64 {
            u64::MAX
        } else {
            (1u64 << value.width()) - 1
        };
        value.to_u64() == literal as u64 & mask
    })
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
fn fold_templates(eg: &EGraph<Node>) -> Vec<Node> {
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

fn const_fold(context: Context, template: &Node) -> Rule {
    let mut lhs = Pattern::new();
    let args: Vec<Id> = (0..template.children().len())
        .map(|index| lhs.var(Var::Symbol(index as Sym)))
        .collect();
    let mut root = template.clone();
    root.children_mut().copy_from_slice(&args);
    lhs.add(root);
    Rewrite::new(
        "const-fold",
        lhs,
        Rhs::Apply(Box::new(move |eg, _substitution, root| {
            if let Some((value, ty)) = fold_class(&context, eg, eg.find(root)) {
                let folded = eg.add(konst(value).typed(ty));
                eg.union(root, folded);
            }
        })),
    )
}

fn fold_class(context: &Context, eg: &EGraph<Node>, class: Id) -> Option<(APInt, TypeId)> {
    eg.nodes(class).iter().find_map(|node| {
        let (Prov::Op(op), Some(ty)) = (node.prov, node.op_type()) else {
            return None;
        };
        let args = &node.children;
        if !context.has_operation(op) {
            return None;
        }
        // A folded class materializes as `builtin.constant`, which only holds an
        // integer, so an op computing anything else (an address, say) must keep
        // its own form however constant its operands are.
        if !produces_integer(context, op) {
            return None;
        }
        let operands: Vec<Value> = args
            .iter()
            .map(|&class| const_value(eg, class).map(Value::Int))
            .collect::<Option<_>>()?;
        match context
            .get_op(op)
            .as_interface::<dyn ConstantFold>()?
            .fold(&operands)
        {
            Some(Value::Int(value)) => Some((value, ty)),
            _ => None,
        }
    })
}

/// The γ arities the seeded graph holds, so a decided gate is searched at the
/// widths it actually occurs at. Only a gate the seeding built is ever decided and
/// rewrites introduce no new arity, so the seeded graph fixes the set.
fn gamma_arities(eg: &EGraph<Node>) -> Vec<usize> {
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
fn decided_gamma(context: Context, arity: usize) -> Rule {
    let mut lhs = Pattern::new();
    let args: Vec<Id> = (0..arity)
        .map(|index| lhs.var(Var::Symbol(index as Sym)))
        .collect();
    lhs.add(Node::gamma_pattern(args));
    Rewrite::new(
        "gamma-decided",
        lhs,
        Rhs::Apply(Box::new(move |eg, _substitution, root| {
            let arm = decided_arm(&context, eg, eg.find(root));
            if let Some(arm) = arm {
                eg.union(root, arm);
            }
        })),
    )
}

fn decided_arm(context: &Context, eg: &EGraph<Node>, class: Id) -> Option<Id> {
    eg.nodes(class).iter().find_map(|node| {
        let [decision, arms @ ..] = &node.children[..] else {
            return None;
        };
        if node.sym() != Some(SymKind::If) {
            return None;
        }
        const_value(eg, *decision)?;
        let cases = gate_cases(context, node)?;
        if cases.len() != arms.len() {
            return None;
        }
        let index = cases
            .iter()
            .position(|case| case.is_some_and(|case| class_is_literal(eg, *decision, case)))
            .or_else(|| cases.iter().position(Option::is_none))?;
        Some(arms[index])
    })
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

fn produces_integer(context: &Context, op: crate::OpId) -> bool {
    let instance = context.get_op(op);
    instance.results().first().is_some_and(|&result| {
        let ty = context.get_type_data(context.get_value(result).ty());
        (ty.as_ref() as &dyn std::any::Any)
            .downcast_ref::<IntegerType>()
            .is_some()
    })
}

fn konst(value: APInt) -> Node {
    Node::constant(value, Prov::None)
}

include!(concat!(env!("OUT_DIR"), "/instcombine_rules.rs"));
