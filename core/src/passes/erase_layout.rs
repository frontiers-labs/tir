//! Layout erasure: the rules of `core/defs/layout.pdl` on the shared rewrite
//! driver. A `ptr.field` becomes a `ptr.ptradd` of the field's offset and a
//! `ptr.copy` a `ptr.memcpy` of the copied type's size, the offset and size
//! asked of `layout.offsetof` and `layout.sizeof`, which fold under the spec in
//! scope. A `builtin.refine` becomes the value it refines: selection has no
//! instruction for a proved fact.

use std::sync::Arc;

use tir_egraph::{ClassId as Id, Engine, Ref, Vocabulary};

use super::instcombine::rules::{Interpretation, Ruleset};
use crate::analysis::AnalysisManager;
use crate::func::FuncOp;
use crate::operation::OpNameId;
use crate::rewrite::{Driver, Kinds, Rewrite, RuleSet, root_ops, rule_roots, seed_ops};
use crate::sem::SemNode as Node;
use crate::sem::node::cost;
use crate::{Context, OperationRef, Pass, PassError, PassTarget, RegionKind};

mod generated {
    use smallvec::{SmallVec, smallvec};
    use tir_egraph::ClassId as Id;
    use tir_egraph::{Atom, ColumnId, Guard, HeadOp, LabelFill, Plan, Query, Source};
    use tir_relational::Expr;

    use crate::Context;
    use crate::passes::instcombine::rules::{Ruleset, call, konst};
    use crate::sem::{SemNode as Node, node::field};
    use crate::utils::APInt;

    include!(concat!(env!("OUT_DIR"), "/layout_rules.rs"));
}

#[derive(Clone, Default)]
pub struct EraseLayoutPass {
    vocabulary: Option<Vocabulary<Node>>,
    kinds: Kinds,
    // The generated rules and the ops their roots name, built on the first
    // run and shared by every function the pass runs on.
    generated: Option<Arc<(Ruleset, Vec<OpNameId>)>>,
}

impl EraseLayoutPass {
    pub fn new() -> Self {
        Self::default()
    }
}

crate::register_pass!(EraseLayoutPass, "erase-layout");

impl Pass for EraseLayoutPass {
    fn name(&self) -> &'static str {
        "erase-layout"
    }

    fn target(&self) -> PassTarget {
        PassTarget::operation_on::<FuncOp>(RegionKind::Nodes)
    }

    fn run(
        &mut self,
        op: &OperationRef,
        context: &Context,
        _analyses: &AnalysisManager,
    ) -> Result<(), PassError> {
        let root = op.op().id;
        let generated = self.generated.get_or_insert_with(|| {
            context.compiled_rules(|| {
                let ruleset = generated::generated_ruleset(context);
                let roots = root_ops(context, &rule_roots(&ruleset.rewrites));
                (ruleset, roots)
            })
        });
        let (ruleset, roots) = &**generated;
        // Each rule rewrites one op over its operands: the graph holds only
        // the ops a root names, and a function holding none has nothing to
        // erase.
        let lowered = context.ops_named(root, roots);
        if lowered.is_empty() {
            return Ok(());
        }
        let vocabulary = self.vocabulary.take();
        let vocabulary = vocabulary.unwrap_or_else(|| Vocabulary::new(context.names()));
        let seeded = seed_ops(context, root, &lowered, vocabulary, &mut self.kinds);
        let rewrite = EraseLayout {
            rules: RuleSet {
                rules: ruleset.rewrites.clone(),
                tables: Vec::new(),
                externs: Box::new(
                    Interpretation::new(context, seeded.pointer_width, generated::pdl_extern)
                        .with_layout(crate::DataLayout::for_op(context, root)),
                ),
            },
        };
        let mut driver = Driver::new(context, seeded, &rewrite, self.name());
        driver.saturate();
        driver.index_values();
        let extraction = driver.extract();
        let lowered = driver.lower(&lowered, &extraction);
        self.vocabulary = Some(driver.eg.release());
        lowered
    }
}

struct EraseLayout {
    rules: RuleSet,
}

impl Rewrite for EraseLayout {
    fn rules(&self) -> &RuleSet {
        &self.rules
    }

    fn cost(&self, _: &Engine<Node>, _: Id, node: &Node, _: &[Ref], _: u64) -> u64 {
        cost(node)
    }

    fn lowers(&self) -> bool {
        true
    }

    fn arm_facts(&self) -> bool {
        false
    }
}
