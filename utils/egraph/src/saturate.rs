//! Saturation of the e-graph: the database's semi-naive driver, with the
//! e-graph choosing where a rule is searched by the operator its root binds.

use tir_relational::query::Host;
use tir_relational::rule as rel;
use tir_relational::saturate::{Delta, Saturate, saturate};

use crate::engine::{CLASSES, Lists};
use crate::query::{Externs, Lookup, Plan, SharedLookup};
use crate::rule::{Rule, Run, compile};
use crate::{ClassId, Engine, Label};

/// Δ_h grouped by operator, as `(op, row, class)` sorted: for each operator,
/// the classes holding it paired with the row each one enters that
/// operator's bucket at, which is what orders the group. One list per height,
/// grown with the frontier.
type OpGroups = Vec<Vec<(u64, u32, u32)>>;

fn op_roots<'d, L: Label>(
    eg: &Engine<L>,
    delta: &'d mut Delta<OpGroups>,
    height: usize,
    op: u64,
) -> &'d [(u64, u32, u32)] {
    delta.at(&eg.db, CLASSES, height);
    let mut by_op = std::mem::take(&mut delta.client);
    while by_op.len() <= height {
        let mut groups: Vec<(u64, u32, u32)> = Vec::new();
        for &class in delta.level(by_op.len()) {
            let first = groups.len();
            for row in eg.rows(ClassId(class)) {
                let op = eg.row_op(row);
                match groups[first..].iter_mut().find(|(seen, _, _)| *seen == op) {
                    Some((_, at, _)) => *at = (*at).min(row.0),
                    None => groups.push((op, row.0, class)),
                }
            }
        }
        groups.sort_unstable();
        by_op.push(groups);
    }
    delta.client = by_op;
    let groups = &delta.client[height];
    let from = groups.partition_point(|&(other, _, _)| other < op);
    let to = groups.partition_point(|&(other, _, _)| other <= op);
    &groups[from..to]
}

fn round_roots<L: Label>(eg: &Engine<L>, plan: &Plan<L>, delta: &mut Delta<OpGroups>) -> Vec<u32> {
    match plan.root_op() {
        Some(op) => op_roots(eg, delta, plan.generic().height(), op)
            .iter()
            .map(|&(_, _, class)| class)
            .collect(),
        None => delta.at(&eg.db, CLASSES, plan.generic().height()).to_vec(),
    }
}

// A plan prepared ahead of any graph reads the labels the vocabulary has,
// whatever rows a graph holds.
struct Unheld<'a, L: Label>(Lookup<'a, L>);

impl<L: Label> tir_relational::query::Calls for Unheld<'_, L> {
    fn call(&self, id: u32, args: &[u64], out: &mut [u64]) -> bool {
        self.0.call(id, args, out)
    }
}

impl<L: Label> Host for Unheld<'_, L> {
    fn version(&self) -> u64 {
        self.0.version()
    }

    fn resolve(&self, param: u32, since: Option<u64>, out: &mut tir_relational::query::Resolution) {
        self.0.resolve(param, since, out)
    }

    fn empty(&self, _param: u32) -> bool {
        false
    }
}

impl<L: Label> Saturate<Lists, 1> for Run<'_, L> {
    type Frontier = OpGroups;

    fn version(&self) -> u64 {
        self.eg.labels_len() as u64
    }

    fn host(&self, rule: usize) -> impl Host + '_ {
        Lookup {
            eg: self.eg,
            plan: &self.rules[rule].plan,
            externs: self.externs,
        }
    }

    type Shared<'a>
        = SharedLookup<'a, L>
    where
        Self: 'a;

    fn shared(&self, rule: usize) -> Option<SharedLookup<'_, L>> {
        Some(SharedLookup {
            terms: self.eg.terms(),
            plan: &self.rules[rule].plan,
            externs: self.externs.shared()?,
        })
    }

    fn head(&self, rule: usize) -> Vec<rel::Head> {
        let mut head = Vec::new();
        compile(&self.rules[rule].head, &mut head);
        head
    }

    fn roots(&mut self, rule: usize, frontier: Option<&mut Delta<OpGroups>>) -> Vec<u32> {
        let plan = &self.rules[rule].plan;
        match frontier {
            Some(delta) => round_roots(self.eg, plan, delta),
            None => plan
                .roots(self.eg)
                .into_iter()
                .map(|class| class.0)
                .collect(),
        }
    }

    fn rebuild(&mut self) {
        self.eg.rebuild();
    }

    fn size(&self) -> usize {
        self.eg.total_size()
    }
}

impl<L: Label> Engine<L> {
    /// Build what the first saturation with `rules` builds that no row of
    /// a graph decides; see [`tir_relational::query::Plan::prepare`]. The
    /// vocabulary keeps it for every graph over it, so a client that makes
    /// its rules ahead of its graphs pays for it then.
    pub fn prepare_rules<R: std::borrow::Borrow<Rule<L>>>(&mut self, rules: &[R]) {
        let mut caches = std::mem::take(self.caches.get_mut().expect("the plan caches"));
        for rule in rules {
            let plan = &rule.borrow().plan;
            let host = Unheld(Lookup {
                eg: self,
                plan,
                externs: &crate::NoExterns,
            });
            if let Some(cache) = caches.plan(plan.generic(), &self.db, &host) {
                plan.generic().prepare(&self.db, &host, cache);
            }
        }
        *self.caches.get_mut().expect("the plan caches") = caches;
    }

    /// Saturate in place with `rules`, and the host functions their guards
    /// call; see [`saturate`]. The fixpoint test counts classes, e-nodes and
    /// raised facts, and the node limit counts e-nodes.
    pub fn saturate_rules<R: std::borrow::Borrow<Rule<L>>>(
        &mut self,
        rules: &[R],
        externs: &dyn Externs<L>,
        iter_limit: usize,
        node_limit: usize,
    ) {
        let rules: Vec<&Rule<L>> = rules.iter().map(std::borrow::Borrow::borrow).collect();
        let rules = &rules[..];
        let mut starts = Vec::with_capacity(rules.len());
        let mut ops = Vec::with_capacity(rules.iter().map(|rule| rule.head.len()).sum());
        for rule in rules {
            starts.push(ops.len());
            ops.extend(rule.head.iter().map(|op| (op, None)));
        }
        let generic: Vec<rel::Rule<'_>> = rules
            .iter()
            .map(|rule| rel::Rule {
                name: &rule.name,
                plan: rule.plan.generic(),
                head_vars: rule.head_vars,
                post_saturation: rule.post_saturation,
            })
            .collect();
        let mut caches = std::mem::take(self.caches.get_mut().expect("the plan caches"));
        let mut run = Run {
            eg: self,
            rules,
            externs,
            starts,
            ops,
        };
        saturate(
            &mut run,
            &mut caches,
            CLASSES,
            &generic,
            iter_limit,
            node_limit,
        );
        *self.caches.get_mut().expect("the plan caches") = caches;
    }
}
