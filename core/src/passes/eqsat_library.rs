//! Library-owned checked rewrite policy.

use std::sync::Arc;
use tir_egglog::Read;

use crate::{Context, OpId, ValueId};

/// A generated request capture, decoded using its compiler descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    Operation(OpId),
    Data(ValueId),
    Dependency(ValueId),
}

/// A source-bound law permitting removal of one covered root and redirection
/// of its complete results. Only library proof adapters can construct it.
pub struct CheckedRewrite {
    root: OpId,
    witness: Box<dyn Proof>,
}

trait Proof: Send + Sync {
    fn validate(&self, context: &Context) -> bool;
    fn mappings(&self) -> &[(ValueId, ValueId)];
}

impl CheckedRewrite {
    pub(super) fn validate(&self, context: &Context) -> bool {
        self.witness.validate(context)
    }
    pub(super) fn covered_root(&self) -> OpId {
        self.root
    }
    pub(super) fn mappings(&self) -> &[(ValueId, ValueId)] {
        self.witness.mappings()
    }
}

type Checker =
    dyn Fn(&Context, &tir_pdl::ProofObligation, &[Capture]) -> Option<CheckedRewrite> + Send + Sync;

/// Typed PDL together with explicit library-owned proof bindings.
#[derive(Clone)]
pub struct EqSatLibrary {
    pub(super) file: tir_pdl::File,
    pub(super) bindings: Vec<(String, Arc<Checker>)>,
}

impl EqSatLibrary {
    /// An unbound library. Checked obligations require an explicit binding.
    pub fn new(file: tir_pdl::File) -> Self {
        Self {
            file,
            bindings: Vec::new(),
        }
    }

    /// Bind a rule to a source-bound checker. Returning `None` preserves source.
    pub fn bind(
        mut self,
        rule: impl Into<String>,
        checker: impl Fn(&Context, &tir_pdl::ProofObligation, &[Capture]) -> Option<CheckedRewrite>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.bindings.push((rule.into(), Arc::new(checker)));
        self
    }
}

impl From<tir_pdl::File> for EqSatLibrary {
    fn from(file: tir_pdl::File) -> Self {
        Self::new(file)
    }
}

struct Forward {
    root: OpId,
    witness: crate::sem::memory::ForwardWitness,
}

impl Proof for Forward {
    fn validate(&self, context: &Context) -> bool {
        self.witness.validate(context).is_ok()
            && demanded_state(
                context,
                self.root,
                self.witness.retained_store(),
                self.witness.replacements(),
            )
    }
    fn mappings(&self) -> &[(ValueId, ValueId)] {
        self.witness.replacements()
    }
}

/// Bind the generated ordinary scalar forwarding pattern to the real memory law.
/// Candidate discovery remains entirely in generated PDL rules.
pub fn forward_memory(
    context: &Context,
    obligation: &tir_pdl::ProofObligation,
    captures: &[Capture],
) -> Option<CheckedRewrite> {
    let Capture::Operation(load) = *captures.get(obligation.root)? else {
        return None;
    };
    let mut operations = captures.iter().filter_map(|capture| match capture {
        Capture::Operation(op) if *op != load => Some(*op),
        _ => None,
    });
    let store = operations.next()?;
    if operations.next().is_some() {
        return None;
    }
    let scope = context.parent_op(load)?;
    let witness = crate::sem::memory::check_forward(context, scope, store, load).ok()?;
    if !demanded_state(
        context,
        load,
        witness.retained_store(),
        witness.replacements(),
    ) {
        return None;
    }
    Some(CheckedRewrite {
        root: load,
        witness: Box::new(Forward {
            root: load,
            witness,
        }),
    })
}

// Removing the data demand of an unordered load must not remove the last
// demand for its retained store. Ordered blocks execute their operations.
fn demanded_state(
    context: &Context,
    load: OpId,
    store: OpId,
    replacements: &[(ValueId, ValueId)],
) -> bool {
    let state = replacements
        .get(1)
        .map_or(context.get_op(store).state_results()[0], |&(state, _)| {
            state
        });
    let check = || -> Result<bool, tir_egglog::Error> {
        let mut engine = context.egraph_with_rules()?;
        if engine
            .read(|s| s.table_subtype("MemoryForwardDemandRoot"))
            .is_none()
        {
            let source = r#"
(relation MemoryForwardDemandRoot (i64 i64 i64))
(relation MemoryForwardLegal (i64))
(relation MemoryForwardDemand (i64 i64))
(ruleset memory-forward-demand)
(rule ((MemoryForwardDemandRoot load store state) (= (OpPlacement store) (InBlock block a)) (= (OpPlacement load) (InBlock block b)) (< a b))
      ((MemoryForwardLegal load)) :ruleset memory-forward-demand)
(rule ((MemoryForwardDemandRoot load store state) (= (OpPlacement store) (InGraph region)) (= (OpPlacement load) (InGraph region)) (MemoryForwardDemand load state))
      ((MemoryForwardLegal load)) :ruleset memory-forward-demand)
(rule ((MemoryForwardDemandRoot root store state) (= (OpPlacement root) (InGraph region)) (= value (Yield region slot)))
      ((MemoryForwardDemand root value)) :ruleset memory-forward-demand)
(rule ((MemoryForwardDemandRoot root store state) (Result root 1 output) (= (OpPlacement root) (InGraph region)) (MemoryForwardDemand root value) (Result op slot value) (!= op root) (= (OpPlacement op) (InGraph region)) (= child (Operand op operand)))
      ((MemoryForwardDemand root child)) :ruleset memory-forward-demand)
;; Without a load-state output, another load may disappear in the same batch.
(rule ((MemoryForwardDemandRoot root store state) (not (Result root 1 output)) (= (OpPlacement root) (InGraph region)) (MemoryForwardDemand root value) (Result op slot value) (!= (OpKind op) "ptr.load") (= (OpPlacement op) (InGraph region)) (= child (Operand op operand)))
      ((MemoryForwardDemand root child)) :ruleset memory-forward-demand)
"#;
            let commands = engine.parse_program("memory forwarding demand", source)?;
            engine.run_program(commands)?;
            engine.set_ruleset_colored("memory-forward-demand");
        }
        let db = engine.database_mut();
        let color = db.color(&[]);
        let result = (|| {
            let db = engine.database_mut();
            db.insert_in(
                color,
                db.table("MemoryForwardDemandRoot")?,
                &[
                    db.base(i64::from(load.number())),
                    db.base(i64::from(store.number())),
                    db.base(i64::from(state.number())),
                ],
            )?;
            engine.run_program(vec![tir_egglog::ast::Command::RunSchedule(
                tir_egglog::Schedule::saturate(tir_egglog::Schedule::run("memory-forward-demand")),
            )])?;
            let db = engine.database();
            Ok(db
                .own_rows_in(color, db.table("MemoryForwardLegal")?)
                .any(|row| row[0] == db.base(i64::from(load.number()))))
        })();
        engine.database_mut().drop_color(color);
        result
    };
    check().unwrap_or(false)
}
