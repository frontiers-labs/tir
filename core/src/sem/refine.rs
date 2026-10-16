//! The obligation of a `builtin.refine`: the solver proves its predicate.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::{
    Counterexample, CounterexampleBinding, ProofOutcome, RuleProofResult, SemGraph, SmtOracle,
    SymKind, SymPayload, UnsupportedReason, con, op,
};
use crate::builtin::{IntegerType, RefineOp};
use crate::{
    ConstantLike, Context, DataLayout, Error, Gamma, NodeId, OpHandle, OpId, Operation, ValueId,
};

// A refine is proved when its op is written: when it is made, and when its
// operands, attributes or place change. A rewrite of the ops computing its
// predicate is itself a proved refinement, so it keeps the predicate true.

/// Prove `forall inputs. path /\ defined(x) -> p` for `%r = refine %x, %p`,
/// where `p` must itself be defined. `x` and `p` are read through the
/// semantics of the ops computing them, down to the values no such op
/// defines; an op is undefined where its `requires:` fails or its `poison:`
/// holds. `path` is that every gate arm the refine sits in is the one its
/// predicate selects, the arm's ports read as the gate's operands. Same
/// outcomes as a rule proof; an obligation the solver cannot decide is
/// reported, never assumed.
pub fn prove_refinement(context: &Context, refine: &RefineOp) -> RuleProofResult {
    let instance = context.get_op(refine.id());
    let mut obligation = Obligation {
        context,
        graph: SemGraph::new(),
        nodes: HashMap::new(),
        symbols: Vec::new(),
        pointer_width: DataLayout::for_instance(context, &instance)
            .and_then(|layout| layout.pointer_size()),
    };
    let mut path = Vec::new();
    for (gate, arm) in enclosing_arms(context, refine.id()) {
        match obligation.selects(&gate, arm) {
            Some(selects) => path.push(selects),
            None => return unsupported("a gate's predicate is no bit vector"),
        }
    }
    let Some(predicate) = obligation.value(refine.predicate()) else {
        return unsupported(
            "the predicate reads a value of no bit-vector type, or a pointer no layout sizes",
        );
    };
    // An input the obligation cannot read is taken as defined everywhere.
    let input = obligation.value(refine.input());
    let graph = &mut obligation.graph;
    // Poison wherever the input is, and true elsewhere.
    let holds = match input {
        Some(input) => op(graph, SymKind::Eq, &[input, input]),
        None => con(graph, 1, 1),
    };
    if let Some(taken) = path
        .into_iter()
        .reduce(|a, b| op(graph, SymKind::And, &[a, b]))
    {
        let elsewhere = op(graph, SymKind::Not, &[taken]);
        op(graph, SymKind::Poison, &[elsewhere, holds]);
    }
    // Each side is rooted at its graph's last node.
    let mut claim = graph.clone();
    op(&mut claim, SymKind::Bitcast, &[predicate]);
    let widths: Vec<u32> = obligation.symbols.iter().map(|&(_, width)| width).collect();
    match SmtOracle.prove_refinement_outcome(&obligation.graph, &claim, &widths) {
        ProofOutcome::Proven => RuleProofResult::Proven,
        ProofOutcome::Disproven { model, values } => RuleProofResult::Disproven {
            counterexample: Some(counterexample(&obligation.symbols, &model, values)),
        },
        ProofOutcome::Unsupported(reason) => RuleProofResult::Unsupported { reason },
    }
}

fn enclosing_arms(context: &Context, mut op: OpId) -> Vec<(OpHandle, usize)> {
    let mut arms = Vec::new();
    while let Some(region) = context.region_of_op(op) {
        let Some(parent) = context.get_region(region).parent_op() else {
            break;
        };
        let gate = context.get_op(parent);
        if let Some(gamma) = gate.clone().as_interface::<dyn Gamma>()
            && let Some(arm) = gamma.arms().iter().position(|&arm| arm == region)
        {
            arms.push((gate, arm));
        }
        op = parent;
    }
    arms.reverse();
    arms
}

pub(crate) fn check_refinements(context: &Context, ops: &[OpId]) -> Result<(), Error> {
    context.forget_erased_refines();
    for &id in ops {
        let Some(refine) = context.get_op(id).as_op::<RefineOp>() else {
            continue;
        };
        if context.refine_proved(id) {
            continue;
        }
        let failure = match prove_refinement(context, &refine) {
            RuleProofResult::Proven | RuleProofResult::Admitted => {
                context.note_refine_proved(id);
                continue;
            }
            RuleProofResult::Disproven { counterexample } => {
                let inputs = counterexample
                    .map(|example| {
                        example
                            .bindings
                            .iter()
                            .map(|binding| format!("{} = {}", binding.name, binding.bits))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                format!("fails for {inputs}")
            }
            RuleProofResult::Unsupported { reason } => {
                format!("cannot be decided ({reason:?}), and is not assumed")
            }
        };
        let located = context
            .op_location(id)
            .map(|span| format!(" at byte {}", span.0))
            .unwrap_or_default();
        return Err(Error::VerificationError(format!(
            "refine %{}{located}: predicate %{} {failure}",
            refine.result().number(),
            refine.predicate().number()
        )));
    }
    Ok(())
}

fn unsupported(why: &str) -> RuleProofResult {
    RuleProofResult::Unsupported {
        reason: UnsupportedReason::UnsupportedObligation(why.into()),
    }
}

struct Obligation<'a> {
    context: &'a Context,
    graph: SemGraph,
    // `None` for a value the obligation cannot read.
    nodes: HashMap<ValueId, Option<NodeId>>,
    symbols: Vec<(ValueId, u32)>,
    pointer_width: Option<u32>,
}

impl Obligation<'_> {
    fn selects(&mut self, gate: &OpHandle, arm: usize) -> Option<NodeId> {
        let gamma = gate.clone().as_interface::<dyn Gamma>()?;
        let binding = gamma.binding();
        let ports = self.context.get_region(gamma.arms()[arm]).ports();
        let forwarded = gate.operands()[binding.operands].to_vec();
        for (operand, port) in forwarded.into_iter().zip(&ports[binding.ports]) {
            if let Some(node) = self.value(operand) {
                self.nodes.insert(port.id(), Some(node));
            }
        }
        let predicate = gamma.predicate();
        let width = self
            .context
            .terms()
            .bit_width(self.context.get_value(predicate).ty())?;
        let predicate = self.value(predicate)?;
        let width = u32::try_from(width).ok().filter(|&width| width <= 64)?;
        let cases = gamma.cases();
        let mut case = |kind, bits| {
            let bits = con(&mut self.graph, bits, width);
            op(&mut self.graph, kind, &[predicate, bits])
        };
        if let Some(&bits) = cases.get(arm) {
            return Some(case(SymKind::Eq, bits));
        }
        let others: Vec<NodeId> = cases.iter().map(|&bits| case(SymKind::Ne, bits)).collect();
        let every = others
            .into_iter()
            .reduce(|a, b| op(&mut self.graph, SymKind::And, &[a, b]));
        Some(every.unwrap_or_else(|| con(&mut self.graph, 1, 1)))
    }

    // Each value's operands are read before it, from a stack: a value is
    // pushed once to read its operands and once to be read.
    fn value(&mut self, root: ValueId) -> Option<NodeId> {
        let mut entered = HashSet::new();
        let mut pending = vec![(root, false)];
        while let Some((value, ready)) = pending.pop() {
            if self.nodes.contains_key(&value) {
                continue;
            }
            if !ready && entered.insert(value) {
                pending.push((value, true));
                let inputs = self.inputs(value).into_iter();
                pending.extend(inputs.map(|input| (input, false)));
                continue;
            }
            let node = self.defined(value).or_else(|| self.symbol(value));
            self.nodes.insert(value, node);
        }
        self.nodes[&root]
    }

    // The operands `defined` reads `value` through.
    fn inputs(&self, value: ValueId) -> Vec<ValueId> {
        let Some(op) = self.context.get_value(value).defining_op() else {
            return Vec::new();
        };
        let instance = self.context.get_op(op);
        let read = instance.results().len() == 1
            && instance.state_operands().is_empty()
            && !instance.has_interface::<dyn ConstantLike>();
        match read {
            true => instance.operands().to_vec(),
            false => Vec::new(),
        }
    }

    fn symbol(&mut self, value: ValueId) -> Option<NodeId> {
        let ty = self.context.get_value(value).ty();
        let width = IntegerType::view(&self.context, ty)
            .and_then(|int| u32::try_from(int.width()).ok())
            .or_else(|| {
                crate::ptr::PtrType::view(&self.context, ty)?;
                self.pointer_width
            })?;
        let node = self.graph.add_node(SymKind::Symbol);
        let id = self.symbols.len() as u32;
        self.graph.set_leaf_data(node, SymPayload::SymbolId(id));
        self.symbols.push((value, width));
        Some(node)
    }

    // Undefined where the defining op's `requires:` fails or its `poison:`
    // holds.
    fn defined(&mut self, value: ValueId) -> Option<NodeId> {
        let instance = self
            .context
            .get_op(self.context.get_value(value).defining_op()?);
        if instance.results().len() != 1 || !instance.state_operands().is_empty() {
            return None;
        }
        if let Some(constant) = instance.clone().as_interface::<dyn ConstantLike>() {
            let node = self.graph.add_node(SymKind::Constant);
            let bits = constant.constant_value().with_signed(false);
            self.graph.set_leaf_data(node, SymPayload::Int(bits));
            return Some(node);
        }
        let definition = instance.clone().as_dyn_op();
        let mut scratch = SemGraph::new();
        let result = definition.semantic_expr(&mut scratch)?;
        let poison = definition.poison_expr(&mut scratch);
        let requires = definition.requires_expr(&mut scratch);
        let operands = instance.operands();
        let mut copied = Vec::with_capacity(scratch.len());
        for node in (0..scratch.len()).map(NodeId::from_index) {
            let kind = *scratch.get_node(node);
            if let (SymKind::Symbol, Some(SymPayload::SymbolId(index))) =
                (kind, scratch.get_leaf_data(node))
            {
                let operand = operands.get(*index as usize)?;
                copied.push(self.nodes.get(operand).copied().flatten()?);
                continue;
            }
            let new = self.graph.add_node(kind);
            if let Some(data) = scratch.get_leaf_data(node) {
                self.graph.set_leaf_data(new, data.clone());
            }
            for child in scratch.children(node) {
                self.graph.add_edge(new, copied[child.index()]);
            }
            copied.push(new);
        }
        let mut node = copied[result.index()];
        if let Some(poison) = poison {
            node = op(
                &mut self.graph,
                SymKind::Poison,
                &[copied[poison.index()], node],
            );
        }
        if let Some(requires) = requires {
            let fails = op(&mut self.graph, SymKind::Not, &[copied[requires.index()]]);
            node = op(&mut self.graph, SymKind::Poison, &[fails, node]);
        }
        Some(node)
    }
}

fn counterexample(
    symbols: &[(ValueId, u32)],
    model: &BTreeMap<u32, Vec<bool>>,
    values: Option<(String, String)>,
) -> Counterexample {
    let bindings = symbols
        .iter()
        .enumerate()
        .filter_map(|(index, (value, _))| {
            let bits = model.get(&(index as u32))?;
            let digits: String = bits
                .chunks(4)
                .rev()
                .map(|nibble| {
                    let digit = nibble
                        .iter()
                        .enumerate()
                        .fold(0, |digit, (bit, &set)| digit | u32::from(set) << bit);
                    char::from_digit(digit, 16).expect("a hex digit")
                })
                .collect();
            Some(CounterexampleBinding {
                name: format!("%{}", value.number()),
                bits: format!("0x{digits}"),
            })
        })
        .collect();
    let (lhs_bits, rhs_bits) = values.unzip();
    Counterexample {
        bindings,
        lhs_bits,
        rhs_bits,
    }
}
