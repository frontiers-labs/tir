use crate::NodeId;
use crate::utils::APInt;

use super::{SemBuilderHooks, SemGraph, SymKind, SymPayload};

/// A compiled semantic program over the existing symbolic operations.
pub struct SemRecipe {
    pub steps: &'static [SemRecipeStep],
    pub root: u32,
}

/// One graph-construction step. References name preceding steps.
pub enum SemRecipeStep {
    Operand(u32),
    Argument(u32),
    Splice(&'static str),
    BoolAttribute {
        name: &'static str,
        default: bool,
    },
    Integer {
        width: u64,
        value: i64,
        signed: bool,
    },
    ResultWidth {
        significant_bits: bool,
        subtract: u64,
    },
    Apply {
        kind: SymKind,
        arguments: &'static [u32],
    },
}

/// Replay checked semantic data without parsing source or allocating a syntax tree.
pub fn build_recipe(
    graph: &mut SemGraph,
    recipe: &SemRecipe,
    operation: &crate::OpHandle,
    hooks: &dyn SemBuilderHooks<SemGraph>,
) -> Option<NodeId> {
    let mut nodes = Vec::with_capacity(recipe.steps.len());
    for step in recipe.steps {
        let node = match step {
            SemRecipeStep::Operand(index) => {
                let node = graph.add_node(SymKind::Symbol);
                graph.set_leaf_data(node, SymPayload::SymbolId(*index));
                node
            }
            SemRecipeStep::Argument(index) => {
                let node = graph.add_node(SymKind::Arg);
                graph.set_leaf_data(node, SymPayload::Int(APInt::new(32, u64::from(*index))));
                node
            }
            SemRecipeStep::Splice(name) => hooks.splice(name, graph)?,
            SemRecipeStep::BoolAttribute { name, default } => {
                let value = match operation.attr(name) {
                    Some(crate::attributes::AttributeValue::Bool(value)) => value,
                    None => *default,
                    _ => return None,
                };
                let node = graph.add_node(SymKind::Constant);
                graph.set_leaf_data(node, SymPayload::Int(APInt::new(1, u64::from(value))));
                node
            }
            SemRecipeStep::Integer {
                width,
                value,
                signed,
            } => {
                let node = graph.add_node(SymKind::Constant);
                let value = if *signed {
                    APInt::new_signed(*width, *value)
                } else {
                    APInt::new(*width, *value as u64)
                };
                graph.set_leaf_data(node, SymPayload::Int(value));
                node
            }
            SemRecipeStep::ResultWidth {
                significant_bits,
                subtract,
            } => {
                let width = hooks.result_width()?;
                let bits = if *significant_bits {
                    u64::from(u64::BITS - width.leading_zeros()).max(1)
                } else {
                    16
                };
                let node = graph.add_node(SymKind::Constant);
                graph.set_leaf_data(
                    node,
                    SymPayload::Int(APInt::new(bits, width.saturating_sub(*subtract))),
                );
                node
            }
            SemRecipeStep::Apply { kind, arguments } => {
                let node = graph.add_node(*kind);
                for argument in *arguments {
                    graph.add_edge(node, nodes[*argument as usize]);
                }
                node
            }
        };
        nodes.push(node);
    }
    Some(nodes[recipe.root as usize])
}

struct DefinedHooks<'a> {
    operation: &'a crate::OpHandle,
    splice: Option<crate::operation::OpSpliceFn>,
}
impl SemBuilderHooks<SemGraph> for DefinedHooks<'_> {
    fn splice(&self, name: &str, graph: &mut SemGraph) -> Option<NodeId> {
        if let Some(splice) = self.splice {
            splice(self.operation, name, graph)
        } else {
            (self.operation.definition()?.splice?)(self.operation, name, graph)
        }
    }
    fn result_width(&self) -> Option<u64> {
        let value = *self.operation.results().first()?;
        Some(
            crate::builtin::IntegerType::view(
                &self.operation.context,
                self.operation.context.get_value(value).ty(),
            )
            .map_or(0, |integer| integer.width()),
        )
    }
}
/// Replay a definition's semantic recipe through its named host bindings.
pub fn build_defined_recipe(
    operation: &crate::OpHandle,
    graph: &mut SemGraph,
    recipe: &SemRecipe,
) -> Option<NodeId> {
    build_recipe(
        graph,
        recipe,
        operation,
        &DefinedHooks {
            operation,
            splice: None,
        },
    )
}
/// Replay a typed host model with its explicit compiled splice bindings.
pub fn build_recipe_with(
    operation: &crate::OpHandle,
    graph: &mut SemGraph,
    recipe: &SemRecipe,
    splice: fn(&crate::OpHandle, &str, &mut SemGraph) -> Option<NodeId>,
) -> Option<NodeId> {
    build_recipe(
        graph,
        recipe,
        operation,
        &DefinedHooks {
            operation,
            splice: Some(splice),
        },
    )
}
