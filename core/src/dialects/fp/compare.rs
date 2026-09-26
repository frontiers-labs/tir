use tir_adt::{APInt, ComparisonKind, compare_float};

use super::arithmetic::{float_width, width_of_float};
use super::resource::{apply_flags, effect_records, effects_for, verify_ports};
use super::{ComparisonBehavior, Exceptions, SubnormalMode};
use crate::NodeId;
use crate::builtin::{IntegerType, StateResource};
use crate::sem::{SemGraph, SymKind, SymPayload};
use crate::{
    Context, Error, HasResourceSemantics, ResourceEffect, ResourceEffects, ResourceSemantics,
    operation,
};

use super::semantics::{interp_error, speculatable_if_ignore};
use crate as tir;

operation! {
    CmpOp {
        name: "cmp", dialect: "fp",
        operands: O { lhs: "crate::builtin::FloatType", rhs: "crate::builtin::FloatType" },
        attributes: A { predicate: "Predicate in FLOAT", semantics: "FpSemantics" },
        results: R { result: "crate::Integer<1>" },
        interfaces: [ResourceEffects, HasResourceSemantics, crate::Speculatable, crate::interp::Interp],
        sem: "(set result $cmp_expr)",
        state: "in_out", verifier: "true",
    }
}

impl CmpOp {
    fn cmp_expr(&self, graph: &mut tir::sem::SemGraph) -> Option<tir::NodeId> {
        tir_symbolic::sem::cmpf_semantics(graph, self.predicate())
    }
}

impl tir::Verifiable for CmpOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        let operands = self.0.value_operands();
        let lhs = context.get_value(operands[0]).ty();
        if lhs != context.get_value(operands[1]).ty() {
            return Err(Error::VerificationError(
                "fp comparison operands must have one format".into(),
            ));
        }
        float_width(context, lhs)?;
        let semantics = self.semantics();
        let semantics = semantics.comparison()?;
        if semantics.subnormals != SubnormalMode::Gradual {
            return Err(Error::VerificationError(
                "fp comparison supports gradual subnormals".into(),
            ));
        }
        let required = effects_for(None, semantics.exceptions);
        verify_ports(&self.0, &required)
    }
}

impl crate::Speculatable for CmpOp {
    fn is_speculatable(&self) -> bool {
        speculatable_if_ignore(self.semantics().comparison().map(|s| s.exceptions))
    }
}

impl ResourceEffects for CmpOp {
    fn resource_effects(&self) -> Vec<ResourceEffect> {
        let semantics = self.semantics();
        let semantics = semantics
            .comparison()
            .expect("verified comparison semantics");
        effect_records(&self.0, effects_for(None, semantics.exceptions))
    }
}

fn comparison_result(op: &CmpOp, graph: &mut SemGraph) -> NodeId {
    let mut expression = SemGraph::new();
    let root = tir_symbolic::sem::cmpf_semantics(&mut expression, op.predicate())
        .expect("verified floating predicate has symbolic semantics");
    let operands = op.0.value_operands();
    let mut memo = std::collections::HashMap::new();
    for node in expression.preorder(root) {
        let Some(SymPayload::SymbolId(symbol)) = expression.get_leaf_data(node) else {
            continue;
        };
        memo.insert(
            node.index(),
            super::resource::value(graph, &op.0, operands[*symbol as usize]),
        );
    }
    let result = tir_symbolic::sem::copy_subgraph(graph, &expression, root, &mut memo);
    graph.annotation_mut(result).actual_type = Some(IntegerType::new(&op.0.context, 1));
    result
}

fn nan_test(
    graph: &mut SemGraph,
    context: &Context,
    input: NodeId,
    width: u32,
    signaling_only: bool,
) -> NodeId {
    let bits = super::resource::operation(graph, SymKind::Bitcast, &[input]);
    graph.annotation_mut(bits).actual_type = Some(IntegerType::new(context, width));
    let (signless, infinity, quiet) = match width {
        32 => (0x7fff_ffff, 0x7f80_0000, 0x0040_0000),
        64 => (
            0x7fff_ffff_ffff_ffff,
            0x7ff0_0000_0000_0000,
            0x0008_0000_0000_0000,
        ),
        _ => unreachable!("verified comparison format"),
    };
    let mask = super::resource::constant(graph, width, signless);
    let magnitude = super::resource::operation(graph, SymKind::And, &[bits, mask]);
    let infinity = super::resource::constant(graph, width, infinity);
    let nan = super::resource::operation(graph, SymKind::UGt, &[magnitude, infinity]);
    graph.annotation_mut(nan).actual_type = Some(IntegerType::new(context, 1));
    if !signaling_only {
        return nan;
    }
    let quiet = super::resource::constant(graph, width, quiet);
    let quiet = super::resource::operation(graph, SymKind::And, &[bits, quiet]);
    let zero = super::resource::constant(graph, width, 0);
    let signaling = super::resource::operation(graph, SymKind::Eq, &[quiet, zero]);
    graph.annotation_mut(signaling).actual_type = Some(IntegerType::new(context, 1));
    let result = super::resource::operation(graph, SymKind::And, &[nan, signaling]);
    graph.annotation_mut(result).actual_type = Some(IntegerType::new(context, 1));
    result
}

impl HasResourceSemantics for CmpOp {
    fn resource_semantics(&self) -> ResourceSemantics {
        let semantics = self.semantics();
        let semantics = semantics
            .comparison()
            .expect("verified comparison semantics");
        let mut graph = SemGraph::new();
        let result = comparison_result(self, &mut graph);
        if semantics.exceptions == Exceptions::Ignore {
            return ResourceSemantics {
                graph,
                raised_flags: None,
                root: result,
                value_results: vec![result],
                state_results: Vec::new(),
            };
        }
        let operands = self.0.value_operands();
        let width = float_width(&self.0.context, self.0.context.get_value(operands[0]).ty())
            .expect("verified comparison format")
            .bit_width();
        let signaling_only = semantics.behavior == ComparisonBehavior::Quiet;
        let lhs = super::resource::value(&mut graph, &self.0, operands[0]);
        let rhs = super::resource::value(&mut graph, &self.0, operands[1]);
        let lhs_invalid = nan_test(&mut graph, &self.0.context, lhs, width, signaling_only);
        let rhs_invalid = nan_test(&mut graph, &self.0.context, rhs, width, signaling_only);
        let invalid =
            super::resource::operation(&mut graph, SymKind::Or, &[lhs_invalid, rhs_invalid]);
        graph.annotation_mut(invalid).actual_type = Some(IntegerType::new(&self.0.context, 1));
        let invalid_flag = super::resource::constant(&mut graph, 5, 0b1_0000);
        let no_flags = super::resource::constant(&mut graph, 5, 0);
        let raised =
            super::resource::operation(&mut graph, SymKind::If, &[invalid, invalid_flag, no_flags]);
        graph.annotation_mut(raised).actual_type = Some(IntegerType::new(&self.0.context, 5));
        let environment = super::resource::value(
            &mut graph,
            &self.0,
            super::resource::state_for(&self.0, StateResource::FpEnv),
        );
        super::resource::flagged_result(
            &self.0,
            graph,
            result,
            raised,
            semantics.exceptions,
            environment,
        )
    }
}

impl crate::interp::Interp for CmpOp {
    fn evaluate(
        &self,
        operands: &[crate::interp::Value],
        state: &mut crate::interp::ExecutionState,
    ) -> Result<Vec<crate::interp::Value>, crate::interp::InterpError> {
        let [
            crate::interp::Value::Float(lhs),
            crate::interp::Value::Float(rhs),
        ] = operands
        else {
            return Err(crate::interp::InterpError::Message(
                "fp comparison requires floating operands".into(),
            ));
        };
        let width = width_of_float(lhs)?;
        if width_of_float(rhs)? != width {
            return Err(crate::interp::InterpError::Message(
                "fp comparison operands have different formats".into(),
            ));
        }
        let semantics = self.semantics();
        let semantics = semantics.comparison().map_err(interp_error)?;
        let result = compare_float(
            width,
            lhs.to_bits() as u64,
            rhs.to_bits() as u64,
            self.predicate(),
            match semantics.behavior {
                ComparisonBehavior::Quiet => ComparisonKind::Quiet,
                ComparisonBehavior::Signaling => ComparisonKind::Signaling,
            },
        );
        let result = apply_flags(result, semantics.exceptions, state)?;
        Ok(vec![crate::interp::Value::Int(APInt::new(1, result.bits))])
    }
}
