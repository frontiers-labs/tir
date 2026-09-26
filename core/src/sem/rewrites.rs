//! The proved algebraic rewrites the semantic e-graph saturates with.
//!
//! Instruction selection saturates a whole function's e-graph with them before
//! covering.

use std::sync::OnceLock;

use tir_relational::Rule;

use super::SymKind;
use super::axioms::pdl::axioms_from_pdl;
use super::axioms::{Axiom, Folding, Interpretation, Materializable};
use super::egraph::SemEGraph;
use super::node::SemNode;
use crate::Context;

/// The axioms a target selects with and the rules they compile to. The axioms
/// stay because a proof obligation names the one it belongs to: `TIR_VERIFY_AXIOMS`
/// discharges each width instantiation as it fires.
#[derive(Default)]
pub struct Theory {
    pub rules: Vec<Rule<SemNode>>,
    axioms: Vec<Axiom>,
    /// The pure ops the heads fold over constants; an extern id names one.
    folds: Vec<SymKind>,
    /// The target's answer to a `materializable(v)` guard.
    materializable: Option<Box<Materializable>>,
}

impl Theory {
    /// Add an axiom's rules: the reading that folds an operand the graph turns
    /// out to have made constant, where that is a different rule at all, and the
    /// one that does not.
    pub fn push(&mut self, axiom: Axiom) {
        let index = self.axioms.len();
        if let Some((folding, true)) = axiom.compile(index, &mut self.folds, Folding::Assume) {
            self.rules.push(folding);
        }
        if let Some((plain, _)) = axiom.compile(index, &mut self.folds, Folding::Never) {
            self.rules.push(plain);
        }
        self.axioms.push(axiom);
    }

    /// Add a rule that is not an axiom — a target's own bridge, or a test's.
    pub fn push_rule(&mut self, rule: Rule<SemNode>) {
        self.rules.push(rule);
    }

    /// Add the read-zero and ignored-write laws declared by target register traits.
    pub(crate) fn with_hardwired_zero_fields(
        mut self,
        fields: &[(super::StateResourceKind, super::StateFieldKind)],
    ) -> Self {
        use super::{SymPayload, template_node};
        use smallvec::smallvec;
        use tir_relational::{Atom, HeadOp, LabelFill, Plan, Query};

        let constant = |width, value| {
            template_node(
                SymKind::Constant,
                Some(SymPayload::Int(tir_adt::APInt::new(width, value))),
                None,
            )
        };
        for &(resource, field) in fields {
            let width = resource
                .field_schema(field)
                .and_then(|schema| schema.bit_width)
                .expect("a hardwired register field has a scalar width");
            for write in [false, true] {
                let mut atoms = vec![
                    Atom::Node {
                        template: template_node(
                            if write {
                                SymKind::StateAssign
                            } else {
                                SymKind::StateRead
                            },
                            None,
                            None,
                        ),
                        args: if write {
                            smallvec![1, 2, 3, 4, 5]
                        } else {
                            smallvec![1, 2, 3]
                        },
                        class: 0,
                        row: None,
                    },
                    Atom::Literal {
                        value: constant(2, resource as u64),
                        class: 2,
                    },
                    Atom::Literal {
                        value: constant(2, field as u64),
                        class: 3,
                    },
                ];
                if write {
                    atoms.push(Atom::Literal {
                        value: constant(1, super::StateAccessKind::Change as u64),
                        class: 4,
                    });
                }
                self.push_rule(Rule {
                    name: format!(
                        "hardwired-zero-{resource:?}-{field:?}-{}",
                        if write { "write" } else { "read" }
                    ),
                    plan: Plan::compile(Query::tree(if write { 6 } else { 4 }, 0, atoms)),
                    head: if write {
                        vec![HeadOp::Union(0, 1)]
                    } else {
                        vec![
                            HeadOp::Insert {
                                label: LabelFill::plain(constant(width, 0)),
                                args: smallvec![],
                                into: 4,
                            },
                            HeadOp::Union(0, 4),
                        ]
                    },
                    head_vars: u32::from(!write),
                    post_saturation: false,
                });
            }
        }
        self
    }

    /// Answer `materializable(v)` guards with the target's constant
    /// materializers.
    pub fn with_materializable(
        mut self,
        materializable: impl Fn(&tir_adt::APInt) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.materializable = Some(Box::new(materializable));
        self
    }

    /// Whether any axiom decomposes a wide constant in place.
    pub fn materializes_constants(&self) -> bool {
        self.axioms.iter().any(Axiom::materializes_constants)
    }

    fn interpretation<'a>(&'a self, context: &'a Context) -> Interpretation<'a> {
        Interpretation::new(
            context,
            &self.axioms,
            &self.folds,
            self.materializable.as_deref(),
        )
    }
}

/// Saturation budget: a cap on iterations and on e-node count.
#[derive(Clone, Copy, Debug)]
pub struct SaturationLimits {
    pub max_iterations: usize,
    pub max_nodes: usize,
}

impl Default for SaturationLimits {
    fn default() -> Self {
        Self {
            max_iterations: 30,
            max_nodes: 10_000,
        }
    }
}

/// Saturate `eg` with `theory` under `limits`, then run the theory's
/// post-saturation rules once over the fixpoint. See
/// [`tir_relational::Engine::saturate_rules`] for the round loop and for what a
/// stop on a limit leaves behind.
///
/// Round 0 starts from the change log rather than from the whole graph, so an
/// assumption scope pays for what it assumed instead of for the graph it assumed
/// it over. That is sound because the log is drained at the end of every
/// saturation that reaches a fixpoint, which leaves a scope's entry log holding
/// its own assertion alone, and because a scope saturates exactly once — the log
/// is a single consumable stream, so a second saturation under the same
/// assumption would find the assertion already gone.
pub fn saturate(ctx: &Context, eg: &mut SemEGraph, theory: &Theory, limits: SaturationLimits) {
    let externs = theory.interpretation(ctx);
    eg.saturate_rules(
        &theory.rules,
        &externs,
        limits.max_iterations,
        limits.max_nodes,
    );
}

/// The target-independent semantic invariants every rule set gets.
pub fn discover_rewrites() -> Theory {
    // The rule file is one per process; parsing it again for every pass built
    // would be the only work here that does not depend on the graph.
    static AXIOMS: OnceLock<Vec<Axiom>> = OnceLock::new();
    let axioms = AXIOMS.get_or_init(|| {
        axioms_from_pdl(include_str!("../../defs/isel.pdl"))
            .expect("core/defs/isel.pdl must be a valid rule set")
    });
    let mut theory = Theory::default();
    for axiom in axioms.iter().cloned() {
        theory.push(axiom);
    }
    theory
}
