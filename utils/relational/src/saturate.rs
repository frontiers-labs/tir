//! The semi-naive saturation driver: rounds over a frozen snapshot, each one
//! searching only what the round before changed, plus the change frontier a
//! round narrows itself by.

use std::collections::HashMap;

use tir_adt::FxBuildHasher;

use crate::query::{Matches, PlanCache};
use crate::telemetry::{RoundStats, Timer, apply_rule, register_rules};
use crate::{ClassId, Engine, Externs, Label, LabelId, Match, Plan, Rule, trace_enabled};

/// Which rules a round has to visit, by what the round before wrote.
pub(crate) struct RuleIndex {
    /// Identifies the rule set the index was built for: its rules' plan ids,
    /// folded.
    rules: u64,
    /// Operator bucket -> the anchorable rules with a row atom in it.
    by_op: HashMap<u64, Vec<u32>, FxBuildHasher>,
    /// The anchorable rules a risen fact can make match, with the columns they
    /// read.
    factual: Vec<(u32, u8)>,
    /// The rules that read no row, with the columns they read: a fact alone
    /// makes them match, a class minted with it included.
    rowless: Vec<(u32, u8)>,
    /// The rules the change log cannot narrow: searched every round.
    always: Vec<u32>,
    /// Every rule of the saturation proper, for the round that searches all.
    all: Vec<usize>,
}

impl RuleIndex {
    /// The plan ids of `rules`, folded into one word. A plan id is never
    /// reused, so two rule sets that agree here are the same plans in the
    /// same order.
    fn fingerprint<L: Label>(rules: &[Rule<L>]) -> u64 {
        rules.iter().fold(rules.len() as u64, |hash, rule| {
            let id = rule.plan.id() ^ u64::from(rule.post_saturation);
            (hash.rotate_left(5) ^ id).wrapping_mul(0x9e37_79b9_7f4a_7c15)
        })
    }

    fn new<L: Label>(rules: &[Rule<L>]) -> Self {
        let mut index = Self {
            rules: Self::fingerprint(rules),
            by_op: HashMap::default(),
            factual: Vec::new(),
            rowless: Vec::new(),
            always: Vec::new(),
            all: Vec::new(),
        };
        for (at, rule) in rules.iter().enumerate() {
            if rule.post_saturation {
                continue;
            }
            index.all.push(at);
            if !rule.plan.anchorable() {
                // A plan of facts alone matches anew only where a fact rose.
                if rule.plan.rowless() && rule.plan.fact_anchorable() {
                    index.rowless.push((at as u32, rule.plan.fact_columns()));
                } else {
                    index.always.push(at as u32);
                }
                continue;
            }
            for op in rule.plan.row_ops() {
                let rules = index.by_op.entry(op).or_default();
                if rules.last() != Some(&(at as u32)) {
                    rules.push(at as u32);
                }
            }
            let columns = rule.plan.fact_columns();
            if columns != 0 {
                index.factual.push((at as u32, columns));
            }
        }
        index
    }

    /// The rules this round visits, ascending, into `visit`.
    fn round<L: Label>(&self, eg: &Engine<L>, narrowed: bool, visit: &mut Vec<usize>) {
        visit.clear();
        if !narrowed {
            visit.extend_from_slice(&self.all);
            return;
        }
        for op in eg.new_ops() {
            if let Some(rules) = self.by_op.get(&op) {
                visit.extend(rules.iter().map(|&rule| rule as usize));
            }
        }
        let rose = eg.facts_rose(false);
        if rose != 0 {
            let reading = self
                .factual
                .iter()
                .filter(|(_, columns)| columns & rose != 0);
            visit.extend(reading.map(|&(rule, _)| rule as usize));
        }
        let rose = rose | eg.facts_rose(true);
        if rose != 0 {
            let reading = self
                .rowless
                .iter()
                .filter(|(_, columns)| columns & rose != 0);
            visit.extend(reading.map(|&(rule, _)| rule as usize));
        }
        visit.extend(self.always.iter().map(|&rule| rule as usize));
        visit.sort_unstable();
        visit.dedup();
    }
}

/// What an engine keeps for one rule between searches and saturations.
#[derive(Default)]
pub(crate) struct RuleCache {
    pub(crate) plan: PlanCache,
    /// The label each insert of the head builds, once it has been interned.
    heads: Vec<Option<LabelId>>,
}

/// Δ_h grouped by operator: for each, the classes holding it paired with the row
/// each one enters that operator's bucket at, which is what orders the group.
type OpGroups = HashMap<u64, Vec<(u32, ClassId)>, FxBuildHasher>;

/// Per-round frontier of semi-naive saturation: the change log of the previous
/// round closed upward, cached by the pattern heights a rule set asks for.
pub struct Delta {
    /// `levels[h]` is Δ_h; grown on demand, each from the one below it.
    levels: Vec<Vec<ClassId>>,
    /// `by_op[h]` groups Δ_h by the operators its classes hold, so a round scans
    /// each depth once instead of once per rule.
    /// Each group is ordered by the row a class enters the bucket at.
    by_op: Vec<OpGroups>,
}

impl Delta {
    /// Seed from a [`Engine::take_changed`] drain.
    pub fn new(changed: Vec<ClassId>) -> Self {
        Self {
            levels: vec![changed],
            by_op: Vec::new(),
        }
    }

    /// Nothing changed, so no rule can match anywhere new.
    pub fn is_empty(&self) -> bool {
        self.levels[0].is_empty()
    }

    /// Size of the change log this frontier grew from.
    pub fn len(&self) -> usize {
        self.levels[0].len()
    }

    /// Size of the deepest frontier asked for so far — levels only grow, so this
    /// is the widest set any rule searched.
    pub fn frontier(&self) -> usize {
        self.levels.last().expect("seeded level").len()
    }

    /// Δ_height, ascending.
    pub fn at<L: Label>(&mut self, eg: &Engine<L>, height: usize) -> &[ClassId] {
        while self.levels.len() <= height {
            let below = self.levels.last().expect("seeded level");
            self.levels.push(eg.delta(below, 1));
        }
        &self.levels[height]
    }

    /// The classes of Δ_height holding `op`, in the order the operator's row
    /// bucket holds them: where a rule of that height rooted on that operator can
    /// match anew.
    ///
    /// Scanning the frontier once per depth beats materializing each rule's whole
    /// bucket and filtering it, because a settled round's frontier is a handful of
    /// classes while the bucket holds every term of that shape in the function.
    /// The bucket is appended to as rows are minted, and a class enters it at its
    /// first row of that operator, so ordering each group by that row reproduces
    /// the bucket's order exactly — which is the root order a match's position in
    /// the round is defined by, and so the order class ids are assigned in.
    pub fn roots<L: Label>(&mut self, eg: &Engine<L>, height: usize, op: u64) -> &[(u32, ClassId)] {
        self.at(eg, height);
        while self.by_op.len() <= height {
            let mut by_op: OpGroups = HashMap::default();
            let mut ops: Vec<(u64, u32)> = Vec::new();
            for &class in &self.levels[self.by_op.len()] {
                ops.clear();
                for row in eg.rows(class) {
                    let op = eg.node(row).op_key();
                    match ops.iter_mut().find(|(seen, _)| *seen == op) {
                        Some((_, first)) => *first = (*first).min(row.0),
                        None => ops.push((op, row.0)),
                    }
                }
                for &(op, row) in &ops {
                    by_op.entry(op).or_default().push((row, class));
                }
            }
            for group in by_op.values_mut() {
                group.sort_unstable();
            }
            self.by_op.push(by_op);
        }
        self.by_op[height].get(&op).map_or(&[], Vec::as_slice)
    }
}

/// The classes a semi-naive round searches `plan` at: the change frontier at the
/// plan's height, restricted to the operator its root binds.
pub fn round_roots<L: Label>(eg: &Engine<L>, plan: &Plan<L>, delta: &mut Delta) -> Vec<ClassId> {
    match plan.root_op() {
        Some(op) => delta
            .roots(eg, plan.height(), op)
            .iter()
            .map(|&(_, class)| class)
            .collect(),
        None => delta.at(eg, plan.height()).to_vec(),
    }
}

impl<L: Label> Engine<L> {
    /// The index of `rules`, built once per rule set the engine saturates with.
    fn rule_index(&mut self, rules: &[Rule<L>]) -> std::sync::Arc<RuleIndex> {
        let wanted = RuleIndex::fingerprint(rules);
        match &self.rule_index {
            Some(index) if index.rules == wanted => index.clone(),
            _ => {
                let index = std::sync::Arc::new(RuleIndex::new(rules));
                self.rule_index = Some(index.clone());
                index
            }
        }
    }

    /// Saturate in place with `rules`, and the host functions their guards call.
    /// Each iteration searches every rule against one snapshot, then applies and
    /// rebuilds — a node born this iteration is visible only to the next. Stops
    /// at a fixpoint (nothing the class and node counts or the fact columns see
    /// changed, or an empty change log) or at a limit.
    ///
    /// A [`Rule::post_saturation`] rule sits out those rounds and fires once
    /// against the fixpoint, over the classes the saturation touched: the rest of
    /// the graph was already at that phase's fixpoint when the caller handed it
    /// over, so only those can hold a match it does not have. The phase is
    /// terminal — nothing feeds its results back — so a saturation that reached a
    /// fixpoint drains the change log on the way out, which leaves the next
    /// assumption scope's entry log holding that scope's own assertion rather
    /// than this fixpoint's tail. A stop on a limit is not a fixpoint: it marks
    /// everything changed instead, since the matches it never reached are not
    /// named by the log.
    pub fn saturate_rules(
        &mut self,
        rules: &[Rule<L>],
        externs: &dyn Externs<L>,
        iter_limit: usize,
        node_limit: usize,
    ) {
        let timer = Timer::start();
        register_rules(rules);
        // The first round searches before anything else rebuilds.
        self.rebuild();
        let mut log = self.take_changed();
        let mut touched = log.clone();
        let mut delta = log.take().map(Delta::new);
        let mut caches = self.take_caches();
        let index = self.rule_index(rules);
        let mut visit: Vec<usize> = Vec::new();
        let mut iters = 0;
        let mut on_a_limit = true;
        loop {
            let size = self.total_size();
            if iters >= iter_limit || size >= node_limit {
                break;
            }
            let before = (self.num_classes(), size, self.stats().raises);

            let mut stats = RoundStats::start(self, delta.as_ref());
            let mut found: Vec<(usize, Matches)> = Vec::new();
            // Most rounds write a handful of rows, and most rules read none of
            // their operators: those rules are not visited at all.
            index.round(self, delta.is_some(), &mut visit);
            for &index in &visit {
                let rule = &rules[index];
                // A rule one of whose operators the graph does not hold has no
                // match, and is settled before its cache is looked up.
                if rule
                    .plan
                    .row_ops()
                    .any(|op| self.labels_with_op(op).is_empty())
                {
                    continue;
                }
                let cache = &mut caches.entry(rule.plan.id()).or_default().plan;
                if rule.plan.anchorable() {
                    // The round's new matches are the ones holding a row the
                    // round before wrote, found from those rows, plus the ones
                    // a fact alone made new, found the way the change log names
                    // them: at the roots near a change, among old rows.
                    let matches = match delta.as_mut() {
                        None => rule.plan.search_all(self, externs, cache),
                        Some(delta) => {
                            let mut matches = rule.plan.search_new(self, externs, cache);
                            if rule.plan.fact_columns() & self.facts_rose(false) != 0 {
                                if rule.plan.fact_anchorable() {
                                    let risen = rule.plan.search_risen(self, false, externs, cache);
                                    matches.extend(risen);
                                } else {
                                    let roots = round_roots(self, &rule.plan, delta);
                                    stats.searched(roots.len(), Some(delta));
                                    let found = rule.plan.search_facts(self, roots, externs, cache);
                                    matches.extend(found);
                                }
                            }
                            matches
                        }
                    };
                    found.push((index, matches));
                    continue;
                }
                if delta.is_some() && rule.plan.rowless() && rule.plan.fact_anchorable() {
                    let matches = rule.plan.search_risen(self, true, externs, cache);
                    found.push((index, matches));
                    continue;
                }
                // Everything a rule reads is an atom or a guard over what an atom
                // bound, so both narrowings come free of any hand-asserted
                // licence — for a rule the change log can speak for. It cannot
                // speak for one whose match depends on rows outside the root's
                // cone.
                let bounded = !rule.plan.unbounded();
                let roots = match delta.as_mut().filter(|_| bounded) {
                    Some(delta) => round_roots(self, &rule.plan, delta),
                    None => rule.plan.roots(self),
                };
                stats.searched(roots.len(), delta.as_ref());
                if roots.is_empty() {
                    continue;
                }
                let fresh = (delta.is_some() && bounded, 0);
                let matches = rule
                    .plan
                    .search_roots(self, roots, None, fresh, externs, cache);
                found.push((index, matches));
            }
            let tracing = trace_enabled();
            for (index, matches) in found {
                let rule = &rules[index];
                let heads = &mut caches.entry(rule.plan.id()).or_default().heads;
                for at in 0..matches.len() {
                    let (root, bindings, scalars) = matches.get(at);
                    if tracing {
                        eprintln!("M {} {}", rule.name, self.find(root).index());
                    }
                    stats.apply(self, |eg| {
                        apply_rule(&rule.name, eg, |eg| {
                            eg.apply_head_cached(
                                &rule.head,
                                rule.head_vars,
                                bindings,
                                scalars,
                                heads,
                            )
                        })
                    });
                }
            }
            self.rebuild();
            stats.finish(self);

            iters += 1;
            let log = self.take_changed();
            match (&mut touched, &log) {
                (Some(all), Some(changed)) => all.extend(changed.iter().copied()),
                _ => touched = None,
            }
            delta = log.map(Delta::new);
            if delta.as_ref().is_some_and(Delta::is_empty) {
                on_a_limit = false;
                break;
            }
            if (self.num_classes(), self.total_size(), self.stats().raises) == before {
                // The counts held, but a round that changed only facts changed
                // nothing they count, and is not a fixpoint. `None` is the widest
                // such log there is, so it counts too.
                on_a_limit = delta.as_ref().is_none_or(|delta| !delta.is_empty());
                break;
            }
        }
        self.put_caches(caches);
        if on_a_limit {
            self.mark_all_changed();
            touched = None;
        }
        self.rebuild();
        self.post_saturate(rules, externs, touched);
        if !on_a_limit {
            self.take_changed();
        }
        timer.finish();
    }

    /// Fire every [`Rule::post_saturation`] rule once over `touched`, or over the
    /// whole graph where the saturation could not name what it changed.
    fn post_saturate(
        &mut self,
        rules: &[Rule<L>],
        externs: &dyn Externs<L>,
        touched: Option<Vec<ClassId>>,
    ) {
        let mut touched = touched.map(|mut all| {
            for id in &mut all {
                *id = self.find(*id);
            }
            all.sort_unstable();
            all.dedup();
            Delta::new(all)
        });
        let mut found: Vec<(&Rule<L>, Vec<Match>)> = Vec::new();
        for rule in rules.iter().filter(|rule| rule.post_saturation) {
            let roots = match touched.as_mut().filter(|_| !rule.plan.unbounded()) {
                Some(touched) => round_roots(self, &rule.plan, touched),
                None => rule.plan.roots(self),
            };
            if roots.is_empty() {
                continue;
            }
            found.push((
                rule,
                rule.plan.search(self, roots, &|_, _| true, false, externs),
            ));
        }
        for (rule, matches) in &found {
            for m in matches {
                apply_rule(&rule.name, self, |eg| {
                    eg.apply_head(&rule.head, rule.head_vars, m)
                });
            }
        }
        self.rebuild();
    }
}
