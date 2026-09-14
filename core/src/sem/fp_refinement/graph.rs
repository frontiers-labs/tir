use std::collections::{HashMap, HashSet};

use crate::attributes::AttributeValue;
use crate::builtin::{FloatType, StateResource};
use crate::fp::{
    AccuracyRequirement, ArithmeticSemantics, EvaluationContract, ScopedFacts, Semantics,
    TransformPermissions,
};
use crate::{Context, OpId, Operation, ValueId};

use super::{ContractionCandidate, ContractionProposal, FusedForm, RefinementError, RoundOp};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ReferenceSnapshot(GraphSnapshot);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CandidateSnapshot(GraphSnapshot);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContractionSite {
    mul: usize,
    consumer: usize,
    product_position: usize,
}

#[derive(Clone, Copy)]
struct MatchedContraction {
    mul: usize,
    consumer: usize,
    product_position: usize,
    form: FusedForm,
}

impl MatchedContraction {
    fn site(self) -> ContractionSite {
        ContractionSite {
            mul: self.mul,
            consumer: self.consumer,
            product_position: self.product_position,
        }
    }
}

pub(super) fn contraction_proposals(
    context: &Context,
    round: &RoundOp,
) -> Result<Vec<ContractionProposal>, RefinementError> {
    let reference = round.reference_region();
    let captures = round.operands();
    let aliases = capture_aliases(&captures);
    let boundaries = reference
        .ports()
        .iter()
        .enumerate()
        .map(|(index, value)| (value.id(), aliases[index]))
        .collect();
    let (graph, operation_indices) =
        GraphBuilder::new(context, captures.to_vec(), boundaries, Some(reference.id()))
            .capture_indexed(&reference.results())?;
    let live_operations: HashMap<_, _> = operation_indices
        .into_iter()
        .map(|(operation, index)| (index, operation))
        .collect();
    let mut proposals = graph
        .contraction_sites()
        .into_iter()
        .map(|site| {
            let mul = live_operations[&site.mul];
            let add_or_sub = live_operations[&site.consumer];
            let mul_handle = context.get_op(mul);
            let consumer = context.get_op(add_or_sub);
            ContractionProposal {
                mul,
                add_or_sub,
                fused_operands: [
                    mul_handle.value_operands()[0],
                    mul_handle.value_operands()[1],
                    consumer.value_operands()[1 - site.product_position],
                ],
                form: site.form,
                retain_mul: graph.mul_is_used_elsewhere(
                    site.mul,
                    site.consumer,
                    site.product_position,
                ),
            }
        })
        .collect::<Vec<_>>();
    proposals.sort_by_key(|proposal| (proposal.add_or_sub, proposal.mul, proposal.form));
    proposals.dedup();
    Ok(proposals)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ContractSnapshot {
    arithmetic: ArithmeticSemantics,
    permissions: TransformPermissions,
    assumptions: ScopedFacts,
    accuracy: AccuracyRequirement,
    result_formats: Vec<CanonicalType>,
    environment_epoch: u64,
    intermediate_exceptions: crate::fp::IntermediateExceptions,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct GraphSnapshot {
    captures: Vec<CanonicalType>,
    operations: Vec<CanonicalOperation>,
    outputs: Vec<CanonicalValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CanonicalOperation {
    dialect: String,
    name: String,
    attributes: Vec<(String, CanonicalAttribute)>,
    operands: Vec<CanonicalValue>,
    results: Vec<CanonicalType>,
    nested_reference: Option<Box<ReferenceSnapshot>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CanonicalValue {
    Capture(usize),
    Result { operation: usize, result: usize },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CanonicalType {
    Float {
        exponent: u32,
        mantissa: u32,
    },
    ShapedFloat {
        exponent: u32,
        mantissa: u32,
        shape: Vec<CanonicalDimension>,
    },
    State(StateResource),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CanonicalDimension {
    Constant(i64),
    Named(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum CanonicalAttribute {
    String(String),
    Integer(i64),
    Unsigned(u64),
    F32(u32),
    F64(u64),
    Bool(bool),
    Array(Vec<Self>),
    Dict(Vec<(String, Self)>),
    Semantics(Semantics),
    Contract(ContractSnapshot),
    Predicate(String),
    Type(CanonicalType),
    Binder(String),
}

pub(super) fn check_contraction_rule(rule: &tir_pdl::Rule) -> Result<(), RefinementError> {
    let mut lhs = PdlGraphBuilder::default();
    let lhs_output = lhs.value(&rule.lhs)?;
    let captures = lhs.captures_by_name.clone();
    let mut lhs_graph = lhs.finish(lhs_output);
    let CanonicalValue::Result {
        operation: consumer,
        result: 0,
    } = lhs_graph.outputs[0]
    else {
        return Err(RefinementError::UnsupportedForm);
    };
    let form = match (
        lhs_graph.operations[consumer].dialect.as_str(),
        lhs_graph.operations[consumer].name.as_str(),
    ) {
        ("fp", "add") => super::FusedForm::MulAdd,
        ("fp", "sub") => super::FusedForm::MulSub,
        _ => return Err(RefinementError::UnsupportedForm),
    };
    let site = lhs_graph
        .contraction_sites()
        .into_iter()
        .find(|site| site.consumer == consumer && site.form == form)
        .ok_or(RefinementError::DisconnectedGroup)?;
    lhs_graph.normalize_pdl_state_chain(site.mul, consumer)?;
    let mut rhs = PdlGraphBuilder::with_captures(captures);
    let rhs_output = rhs.value(&rule.rhs)?;
    let rhs_graph = rhs.finish(rhs_output);
    lhs_graph
        .contract(site.site())
        .filter(|expected| expected == &rhs_graph)
        .map(|_| ())
        .ok_or(RefinementError::DisconnectedGroup)
}

#[derive(Default)]
struct PdlGraphBuilder {
    captures: Vec<CanonicalType>,
    captures_by_name: HashMap<String, (usize, CanonicalType)>,
    operations: Vec<CanonicalOperation>,
}

impl PdlGraphBuilder {
    fn with_captures(captures_by_name: HashMap<String, (usize, CanonicalType)>) -> Self {
        let mut captures = vec![None; captures_by_name.len()];
        for (index, ty) in captures_by_name.values() {
            captures[*index] = Some(ty.clone());
        }
        Self {
            captures: captures
                .into_iter()
                .collect::<Option<_>>()
                .unwrap_or_default(),
            captures_by_name,
            operations: Vec::new(),
        }
    }

    fn finish(self, output: CanonicalValue) -> GraphSnapshot {
        let mut outputs = vec![output.clone()];
        if let CanonicalValue::Result { operation, .. } = output {
            outputs.extend(
                (1..self.operations[operation].results.len())
                    .map(|result| CanonicalValue::Result { operation, result }),
            );
        }
        GraphSnapshot {
            captures: self.captures,
            operations: self.operations,
            outputs,
        }
    }

    fn value(&mut self, term: &tir_pdl::Term) -> Result<CanonicalValue, RefinementError> {
        match &term.kind {
            tir_pdl::TermKind::Binder { name, ty } => self.binder(name, ty.as_ref()),
            tir_pdl::TermKind::Operation {
                operator: tir_pdl::Operator::Dialect { dialect, name },
                attributes,
                operands,
                dependencies,
            } => {
                if dialect == "fp" {
                    let expected = match name.as_str() {
                        "mul" | "add" | "sub" => 2,
                        "fma" => 3,
                        "neg" => 1,
                        _ => return Err(RefinementError::UnsupportedForm),
                    };
                    if operands.len() != expected {
                        return Err(RefinementError::UnsupportedForm);
                    }
                }
                let mut operands = operands
                    .iter()
                    .map(|operand| self.value(operand))
                    .collect::<Result<Vec<_>, _>>()?;
                let dependency_values = dependencies
                    .iter()
                    .map(|operand| self.value(operand))
                    .collect::<Result<Vec<_>, _>>()?;
                let result_type = term
                    .ty
                    .as_ref()
                    .map(pdl_type)
                    .transpose()?
                    .or_else(|| {
                        operands
                            .first()
                            .and_then(|value| self.value_type(value).cloned())
                    })
                    .ok_or(RefinementError::IncompatibleSemantics)?;
                let state_results = dependency_values
                    .iter()
                    .map(|value| self.value_type(value).cloned())
                    .collect::<Option<Vec<_>>>()
                    .ok_or(RefinementError::IncompatibleSemantics)?;
                if state_results
                    .iter()
                    .any(|ty| !matches!(ty, CanonicalType::State(_)))
                {
                    return Err(RefinementError::IncompatibleSemantics);
                }
                operands.extend(dependency_values);
                let mut attributes = attributes
                    .iter()
                    .map(|attribute| {
                        let value = match &attribute.value {
                            tir_pdl::AttributeValue::Integer(value) => {
                                CanonicalAttribute::Integer(*value)
                            }
                            tir_pdl::AttributeValue::String(value) => {
                                CanonicalAttribute::String(value.clone())
                            }
                            tir_pdl::AttributeValue::Binder(value) => {
                                CanonicalAttribute::Binder(value.clone())
                            }
                        };
                        (attribute.name.clone(), value)
                    })
                    .collect::<Vec<_>>();
                attributes.sort_by(|left, right| left.0.cmp(&right.0));
                let operation = self.operations.len();
                self.operations.push(CanonicalOperation {
                    dialect: dialect.clone(),
                    name: name.clone(),
                    attributes,
                    operands,
                    results: std::iter::once(result_type).chain(state_results).collect(),
                    nested_reference: None,
                });
                Ok(CanonicalValue::Result {
                    operation,
                    result: 0,
                })
            }
            _ => Err(RefinementError::UnsupportedForm),
        }
    }

    fn binder(
        &mut self,
        name: &str,
        binding: Option<&tir_pdl::BindingType>,
    ) -> Result<CanonicalValue, RefinementError> {
        if let Some((index, expected)) = self.captures_by_name.get(name) {
            if let Some(tir_pdl::BindingType::Type(ty)) = binding {
                if &pdl_type(ty)? != expected {
                    return Err(RefinementError::IncompatibleSemantics);
                }
            } else if binding.is_some() {
                return Err(RefinementError::UnsupportedForm);
            }
            return Ok(CanonicalValue::Capture(*index));
        }
        let Some(tir_pdl::BindingType::Type(ty)) = binding else {
            return Err(RefinementError::UnsupportedForm);
        };
        let ty = pdl_type(ty)?;
        let index = self.captures.len();
        self.captures.push(ty.clone());
        self.captures_by_name.insert(name.to_owned(), (index, ty));
        Ok(CanonicalValue::Capture(index))
    }

    fn value_type(&self, value: &CanonicalValue) -> Option<&CanonicalType> {
        match value {
            CanonicalValue::Capture(index) => self.captures.get(*index),
            CanonicalValue::Result { operation, result } => {
                self.operations.get(*operation)?.results.get(*result)
            }
        }
    }
}

fn pdl_type(ty: &tir_pdl::Type) -> Result<CanonicalType, RefinementError> {
    use tir_pdl::{FloatFormat, Resource, Type};
    let format_parts = |format| match format {
        FloatFormat::Binary32 => (8, 23),
        FloatFormat::Binary64 => (11, 52),
    };
    Ok(match ty {
        Type::Float(format) => {
            let (exponent, mantissa) = format_parts(*format);
            CanonicalType::Float { exponent, mantissa }
        }
        Type::ShapedFloat {
            format: value,
            shape,
        } => {
            let (exponent, mantissa) = format_parts(*value);
            let shape = shape
                .iter()
                .map(|dimension| match &dimension.kind {
                    tir_pdl::ExprKind::Integer(value) => Ok(CanonicalDimension::Constant(*value)),
                    tir_pdl::ExprKind::Name(value) => Ok(CanonicalDimension::Named(value.clone())),
                    _ => Err(RefinementError::UnsupportedForm),
                })
                .collect::<Result<_, _>>()?;
            CanonicalType::ShapedFloat {
                exponent,
                mantissa,
                shape,
            }
        }
        Type::State(Resource::FpEnv) => CanonicalType::State(StateResource::FpEnv),
        Type::State(Resource::Memory) => CanonicalType::State(StateResource::Memory),
        Type::State(Resource::Named(_)) | Type::Integer(_) | Type::Named(_) => {
            return Err(RefinementError::UnsupportedForm);
        }
    })
}

impl ReferenceSnapshot {
    pub fn capture(context: &Context, round: &RoundOp) -> Result<Self, RefinementError> {
        let reference = round.reference_region();
        let captures = round.operands();
        let aliases = capture_aliases(&captures);
        let boundaries = reference
            .ports()
            .iter()
            .enumerate()
            .map(|(index, value)| (value.id(), aliases[index]))
            .collect();
        GraphBuilder::new(context, captures.to_vec(), boundaries, Some(reference.id()))
            .capture(&reference.results())
            .map(Self)
    }

    pub(crate) fn capture_contraction(
        context: &Context,
        round: &RoundOp,
        candidate: &ContractionCandidate,
    ) -> Result<(Self, ContractionSite), RefinementError> {
        let reference = round.reference_region();
        let captures = round.operands();
        let aliases = capture_aliases(&captures);
        let boundaries = reference
            .ports()
            .iter()
            .enumerate()
            .map(|(index, value)| (value.id(), aliases[index]))
            .collect();
        let (graph, [mul, consumer]) =
            GraphBuilder::new(context, captures.to_vec(), boundaries, Some(reference.id()))
                .capture_with_operations(
                    &reference.results(),
                    [candidate.mul, candidate.add_or_sub],
                )?;
        let product_position = graph
            .contraction_sites()
            .into_iter()
            .find(|site| {
                site.mul == mul && site.consumer == consumer && site.form == candidate.form
            })
            .ok_or(RefinementError::DisconnectedGroup)?
            .product_position;
        Ok((
            Self(graph),
            ContractionSite {
                mul,
                consumer,
                product_position,
            },
        ))
    }
}

impl CandidateSnapshot {
    pub fn capture(
        context: &Context,
        round: &RoundOp,
        candidate: &ContractionCandidate,
    ) -> Result<Self, RefinementError> {
        let yielded = round.reference_region().results();
        if candidate.result_mapping.len() != yielded.len()
            || candidate
                .result_mapping
                .iter()
                .zip(&yielded)
                .any(|((source, _), yielded)| source != yielded)
        {
            return Err(RefinementError::IncompleteResultMapping);
        }
        let captures = round.operands();
        let boundaries = captures.iter().copied().enumerate().fold(
            HashMap::new(),
            |mut boundaries, (index, value)| {
                boundaries.entry(value).or_insert(index);
                boundaries
            },
        );
        let outputs: Vec<_> = candidate
            .result_mapping
            .iter()
            .map(|(_, output)| *output)
            .collect();
        GraphBuilder::new(context, captures.to_vec(), boundaries, None)
            .capture(&outputs)
            .map(Self)
    }

    pub(super) fn is_contraction_of(
        &self,
        reference: &ReferenceSnapshot,
        site: ContractionSite,
    ) -> bool {
        reference
            .0
            .contract(site)
            .is_some_and(|expected| expected == self.0)
    }
}

impl GraphSnapshot {
    fn is_op(&self, operation: usize, dialect: &str, name: &str) -> bool {
        self.operations
            .get(operation)
            .is_some_and(|op| op.dialect == dialect && op.name == name)
    }

    fn contract(&self, site: ContractionSite) -> Option<Self> {
        let ContractionSite {
            mul,
            consumer,
            product_position,
        } = site;
        let form = if self.is_op(consumer, "fp", "add") {
            FusedForm::MulAdd
        } else if self.is_op(consumer, "fp", "sub") {
            FusedForm::MulSub
        } else {
            return None;
        };
        let consumer_name = match form {
            super::FusedForm::MulAdd => "add",
            super::FusedForm::MulSub => "sub",
        };
        if !self.is_op(mul, "fp", "mul") || !self.is_op(consumer, "fp", consumer_name) {
            return None;
        }
        let product = CanonicalValue::Result {
            operation: mul,
            result: 0,
        };
        if self.operations[consumer].operands.get(product_position) != Some(&product) {
            return None;
        }
        if form == super::FusedForm::MulSub && product_position != 0 {
            return None;
        }
        let mul_op = &self.operations[mul];
        let consumer_op = &self.operations[consumer];
        if !self.compatible_group(mul_op, consumer_op, product_position) {
            return None;
        }
        let retained_mul = self.mul_is_used_elsewhere(mul, consumer, product_position);
        let mut graph = self.clone();
        let mut addend = consumer_op.operands[1 - product_position].clone();
        if form == super::FusedForm::MulSub {
            let ty = graph.value_type(&addend)?.clone();
            let neg = graph.operations.len();
            graph.operations.push(CanonicalOperation {
                dialect: "fp".into(),
                name: "neg".into(),
                attributes: Vec::new(),
                operands: vec![addend],
                results: vec![ty],
                nested_reference: None,
            });
            addend = CanonicalValue::Result {
                operation: neg,
                result: 0,
            };
        }
        let state_operands = if retained_mul {
            &consumer_op.operands[2..]
        } else {
            &mul_op.operands[2..]
        };
        let mut attributes = consumer_op.attributes.clone();
        if let Some((_, segments)) = attributes
            .iter_mut()
            .find(|(name, _)| name == "operand_segment_sizes")
        {
            *segments = CanonicalAttribute::Array(vec![
                CanonicalAttribute::Unsigned(1),
                CanonicalAttribute::Unsigned(1),
                CanonicalAttribute::Unsigned(1),
                CanonicalAttribute::Unsigned(state_operands.len() as u64),
            ]);
        }
        graph.operations[consumer] = CanonicalOperation {
            dialect: "fp".into(),
            name: "fma".into(),
            attributes,
            operands: vec![
                mul_op.operands[0].clone(),
                mul_op.operands[1].clone(),
                addend,
            ]
            .into_iter()
            .chain(state_operands.iter().cloned())
            .collect(),
            results: consumer_op.results.clone(),
            nested_reference: None,
        };
        graph.recanonicalize()
    }

    fn contraction_sites(&self) -> Vec<MatchedContraction> {
        let mut sites = Vec::new();
        for (consumer, operation) in self.operations.iter().enumerate() {
            let form = match (operation.dialect.as_str(), operation.name.as_str()) {
                ("fp", "add") => FusedForm::MulAdd,
                ("fp", "sub") => FusedForm::MulSub,
                _ => continue,
            };
            for (product_position, operand) in operation.operands.iter().take(2).enumerate() {
                let CanonicalValue::Result {
                    operation: mul,
                    result: 0,
                } = operand
                else {
                    continue;
                };
                if (form != FusedForm::MulSub || product_position == 0)
                    && self.is_op(*mul, "fp", "mul")
                    && self.compatible_group(&self.operations[*mul], operation, product_position)
                {
                    sites.push(MatchedContraction {
                        mul: *mul,
                        consumer,
                        product_position,
                        form,
                    });
                }
            }
        }
        sites
    }

    fn compatible_group(
        &self,
        mul: &CanonicalOperation,
        consumer: &CanonicalOperation,
        product_position: usize,
    ) -> bool {
        let Some(numeric_type) = mul.results.first() else {
            return false;
        };
        if !matches!(numeric_type, CanonicalType::Float { .. })
            || mul.operands.len() < 2
            || consumer.operands.len() < 2
            || consumer.results.first() != Some(numeric_type)
            || self.value_type(&mul.operands[0]) != Some(numeric_type)
            || self.value_type(&mul.operands[1]) != Some(numeric_type)
            || self.value_type(&consumer.operands[1 - product_position]) != Some(numeric_type)
            || semantic_attributes(&mul.attributes) != semantic_attributes(&consumer.attributes)
        {
            return false;
        }
        let mul_states = mul.operands[2..]
            .iter()
            .map(|value| self.value_type(value))
            .collect::<Option<Vec<_>>>();
        let consumer_states = consumer.operands[2..]
            .iter()
            .map(|value| self.value_type(value))
            .collect::<Option<Vec<_>>>();
        let Some((mul_states, consumer_states)) = mul_states.zip(consumer_states) else {
            return false;
        };
        mul_states == consumer_states
            && mul_states
                .iter()
                .all(|ty| matches!(ty, CanonicalType::State(StateResource::FpEnv)))
            && mul.results[1..] == consumer.results[1..]
            && mul.results.len() == mul_states.len() + 1
            && consumer.results.len() == consumer_states.len() + 1
            && mul.results[1..]
                .iter()
                .all(|ty| matches!(ty, CanonicalType::State(_)))
    }

    fn normalize_pdl_state_chain(
        &mut self,
        mul: usize,
        consumer: usize,
    ) -> Result<(), RefinementError> {
        let mul_operands = self.operations[mul].operands[2..].to_vec();
        let consumer_operands = self.operations[consumer].operands[2..].to_vec();
        if mul_operands != consumer_operands
            || self.operations[mul].results.len() != mul_operands.len() + 1
        {
            return Err(RefinementError::DisconnectedGroup);
        }
        for index in 0..mul_operands.len() {
            self.operations[consumer].operands[index + 2] = CanonicalValue::Result {
                operation: mul,
                result: index + 1,
            };
        }
        Ok(())
    }

    fn mul_is_used_elsewhere(
        &self,
        operation: usize,
        consumer: usize,
        product_position: usize,
    ) -> bool {
        self.operations[operation]
            .results
            .iter()
            .enumerate()
            .any(|(result, _)| {
                let value = CanonicalValue::Result { operation, result };
                self.outputs.contains(&value)
                    || self.operations.iter().enumerate().any(|(index, op)| {
                        op.operands.iter().enumerate().any(|(position, operand)| {
                            operand == &value
                                && (index != consumer
                                    || (result == 0 && position != product_position))
                        })
                    })
            })
    }

    fn value_type(&self, value: &CanonicalValue) -> Option<&CanonicalType> {
        match value {
            CanonicalValue::Capture(index) => self.captures.get(*index),
            CanonicalValue::Result { operation, result } => {
                self.operations.get(*operation)?.results.get(*result)
            }
        }
    }

    fn recanonicalize(&self) -> Option<Self> {
        let mut operations = Vec::new();
        let mut indices = HashMap::new();
        let outputs = self
            .outputs
            .iter()
            .map(|value| self.recanonicalize_value(value, &mut operations, &mut indices))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            captures: self.captures.clone(),
            operations,
            outputs,
        })
    }

    fn recanonicalize_value(
        &self,
        value: &CanonicalValue,
        operations: &mut Vec<CanonicalOperation>,
        indices: &mut HashMap<usize, usize>,
    ) -> Option<CanonicalValue> {
        let CanonicalValue::Result { operation, result } = value else {
            return Some(value.clone());
        };
        if let Some(index) = indices.get(operation) {
            return Some(CanonicalValue::Result {
                operation: *index,
                result: *result,
            });
        }
        let source = self.operations.get(*operation)?;
        let operands = source
            .operands
            .iter()
            .map(|operand| self.recanonicalize_value(operand, operations, indices))
            .collect::<Option<_>>()?;
        let index = operations.len();
        operations.push(CanonicalOperation {
            operands,
            ..source.clone()
        });
        indices.insert(*operation, index);
        Some(CanonicalValue::Result {
            operation: index,
            result: *result,
        })
    }
}

fn semantic_attributes(
    attributes: &[(String, CanonicalAttribute)],
) -> Vec<&(String, CanonicalAttribute)> {
    attributes
        .iter()
        .filter(|(name, _)| name != "operand_segment_sizes")
        .collect()
}

impl ContractSnapshot {
    pub(crate) fn capture(
        context: &Context,
        contract: &EvaluationContract,
    ) -> Result<Self, RefinementError> {
        Ok(Self {
            arithmetic: contract.arithmetic,
            permissions: contract.permissions,
            assumptions: contract.assumptions,
            accuracy: contract.accuracy.clone(),
            result_formats: contract
                .result_formats
                .iter()
                .map(|ty| canonical_type(context, *ty))
                .collect::<Result<_, _>>()?,
            environment_epoch: contract.environment_epoch,
            intermediate_exceptions: contract.intermediate_exceptions,
        })
    }
}

struct GraphBuilder<'a> {
    context: &'a Context,
    captures: Vec<ValueId>,
    boundaries: HashMap<ValueId, usize>,
    region: Option<crate::RegionId>,
    operations: Vec<CanonicalOperation>,
    operation_indices: HashMap<OpId, usize>,
    active: HashSet<OpId>,
}

impl<'a> GraphBuilder<'a> {
    fn new(
        context: &'a Context,
        captures: Vec<ValueId>,
        boundaries: HashMap<ValueId, usize>,
        region: Option<crate::RegionId>,
    ) -> Self {
        Self {
            context,
            captures,
            boundaries,
            region,
            operations: Vec::new(),
            operation_indices: HashMap::new(),
            active: HashSet::new(),
        }
    }

    fn capture(mut self, outputs: &[ValueId]) -> Result<GraphSnapshot, RefinementError> {
        let captures = self
            .captures
            .iter()
            .map(|value| canonical_type(self.context, self.context.get_value(*value).ty()))
            .collect::<Result<_, _>>()?;
        let outputs = outputs
            .iter()
            .map(|output| self.value(*output))
            .collect::<Result<_, _>>()?;
        Ok(GraphSnapshot {
            captures,
            operations: self.operations,
            outputs,
        })
    }

    fn capture_with_operations<const N: usize>(
        mut self,
        outputs: &[ValueId],
        selected: [OpId; N],
    ) -> Result<(GraphSnapshot, [usize; N]), RefinementError> {
        let captures = self
            .captures
            .iter()
            .map(|value| canonical_type(self.context, self.context.get_value(*value).ty()))
            .collect::<Result<_, _>>()?;
        let outputs = outputs
            .iter()
            .map(|output| self.value(*output))
            .collect::<Result<_, _>>()?;
        let selected = selected.map(|operation| {
            self.operation_indices
                .get(&operation)
                .copied()
                .ok_or(RefinementError::DisconnectedGroup)
        });
        let selected = selected
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .try_into()
            .map_err(|_| RefinementError::UnsupportedForm)?;
        Ok((
            GraphSnapshot {
                captures,
                operations: self.operations,
                outputs,
            },
            selected,
        ))
    }

    fn capture_indexed(
        mut self,
        outputs: &[ValueId],
    ) -> Result<(GraphSnapshot, HashMap<OpId, usize>), RefinementError> {
        let captures = self
            .captures
            .iter()
            .map(|value| canonical_type(self.context, self.context.get_value(*value).ty()))
            .collect::<Result<_, _>>()?;
        let outputs = outputs
            .iter()
            .map(|output| self.value(*output))
            .collect::<Result<_, _>>()?;
        Ok((
            GraphSnapshot {
                captures,
                operations: self.operations,
                outputs,
            },
            self.operation_indices,
        ))
    }

    fn value(&mut self, value: ValueId) -> Result<CanonicalValue, RefinementError> {
        if let Some(index) = self.boundaries.get(&value) {
            return Ok(CanonicalValue::Capture(*index));
        }
        if !self.context.has_value(value) {
            return Err(RefinementError::StaleOperation);
        }
        let definition = self
            .context
            .get_value(value)
            .defining_op()
            .ok_or(RefinementError::OutsideReference)?;
        if self
            .region
            .is_some_and(|region| self.context.parent_nodes_region(definition) != Some(region))
        {
            return Err(RefinementError::OutsideReference);
        }
        let result = self
            .context
            .get_op(definition)
            .results()
            .iter()
            .position(|candidate| *candidate == value)
            .ok_or(RefinementError::StaleOperation)?;
        let operation = self.operation(definition)?;
        Ok(CanonicalValue::Result { operation, result })
    }

    fn operation(&mut self, operation: OpId) -> Result<usize, RefinementError> {
        if let Some(index) = self.operation_indices.get(&operation) {
            return Ok(*index);
        }
        if !self.context.has_operation(operation) || !self.active.insert(operation) {
            return Err(RefinementError::StaleOperation);
        }
        let handle = self.context.get_op(operation);
        let nested_round = handle.clone().as_op::<RoundOp>();
        if !handle.regions().is_empty() && nested_round.is_none() {
            return Err(RefinementError::UnsupportedForm);
        }
        let operands = handle
            .operands()
            .iter()
            .map(|operand| self.value(*operand))
            .collect::<Result<_, _>>()?;
        let attributes = canonical_attributes(self.context, &handle.attributes())?;
        let results = handle
            .results()
            .iter()
            .map(|result| canonical_type(self.context, self.context.get_value(*result).ty()))
            .collect::<Result<_, _>>()?;
        let nested_reference = nested_round
            .map(|round| ReferenceSnapshot::capture(self.context, &round).map(Box::new))
            .transpose()?;
        self.active.remove(&operation);
        let index = self.operations.len();
        self.operations.push(CanonicalOperation {
            dialect: handle.dialect().as_str().to_owned(),
            name: handle.name().as_str().to_owned(),
            attributes,
            operands,
            results,
            nested_reference,
        });
        self.operation_indices.insert(operation, index);
        Ok(index)
    }
}

fn capture_aliases(captures: &[ValueId]) -> Vec<usize> {
    let mut first = HashMap::new();
    captures
        .iter()
        .enumerate()
        .map(|(index, value)| *first.entry(*value).or_insert(index))
        .collect()
}

fn canonical_attributes(
    context: &Context,
    attributes: &[crate::attributes::NamedAttribute],
) -> Result<Vec<(String, CanonicalAttribute)>, RefinementError> {
    let mut attributes = attributes
        .iter()
        .map(|attribute| {
            Ok((
                context.resolve(attribute.name),
                canonical_attribute(context, &attribute.value)?,
            ))
        })
        .collect::<Result<Vec<_>, RefinementError>>()?;
    attributes.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(attributes)
}

fn canonical_attribute(
    context: &Context,
    attribute: &AttributeValue,
) -> Result<CanonicalAttribute, RefinementError> {
    Ok(match attribute {
        AttributeValue::Str(value) => CanonicalAttribute::String(value.to_string()),
        AttributeValue::Int(value) => CanonicalAttribute::Integer(*value),
        AttributeValue::UInt(value) => CanonicalAttribute::Unsigned(*value),
        AttributeValue::F32(value) => CanonicalAttribute::F32(value.to_bits()),
        AttributeValue::F64(value) => CanonicalAttribute::F64(value.to_bits()),
        AttributeValue::Bool(value) => CanonicalAttribute::Bool(*value),
        AttributeValue::Array(values) => CanonicalAttribute::Array(
            values
                .iter()
                .map(|value| canonical_attribute(context, value))
                .collect::<Result<_, _>>()?,
        ),
        AttributeValue::Dict(values) => CanonicalAttribute::Dict(
            values
                .iter()
                .map(|(name, value)| Ok((name.clone(), canonical_attribute(context, value)?)))
                .collect::<Result<_, RefinementError>>()?,
        ),
        AttributeValue::FpSemantics(semantics) => CanonicalAttribute::Semantics(**semantics),
        AttributeValue::EvaluationContract(contract) => {
            CanonicalAttribute::Contract(ContractSnapshot::capture(context, contract)?)
        }
        AttributeValue::Predicate(predicate) => {
            CanonicalAttribute::Predicate(predicate.name().to_owned())
        }
        AttributeValue::Type(ty) => CanonicalAttribute::Type(canonical_type(context, *ty)?),
        AttributeValue::Register(_) | AttributeValue::Value(_) | AttributeValue::Block(_) => {
            return Err(RefinementError::UnsupportedForm);
        }
    })
}

fn canonical_type(context: &Context, ty: crate::TypeId) -> Result<CanonicalType, RefinementError> {
    if let Some(resource) = context.state_resource(ty) {
        return Ok(CanonicalType::State(resource));
    }
    let data = context.get_type_data(ty);
    let Some(float) = (data.as_ref() as &dyn std::any::Any).downcast_ref::<FloatType>() else {
        return Err(RefinementError::UnsupportedForm);
    };
    Ok(CanonicalType::Float {
        exponent: float.exp_width(),
        mantissa: float.mant_width(),
    })
}
