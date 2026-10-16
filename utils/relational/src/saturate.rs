//! The semi-naive saturation driver: rounds over a frozen snapshot, each one
//! searching only what the round before changed, plus the change frontier a
//! round narrows itself by.

use std::collections::hash_map::Entry;
use std::sync::{Mutex, OnceLock};

use smallvec::SmallVec;

use crate::FxHashMap;
use crate::db::{Core, Groups, SpaceId};
use crate::query::{Atom, Calls, Host, Matches, Param, Plan, PlanCache, Prepared, Unchecked};
use crate::rule::{Batch, Client, Head, Rule, apply};
use crate::telemetry::{RoundStats, Timer, count_rule, register_rules};
use crate::trace_enabled;

/// What a driver asks its client on top of a head's [`Client`].
pub trait Saturate<C, const S: usize>: Client<C, S> {
    /// What the client keeps per frontier, dropped with it.
    type Frontier: Default;
    /// See [`Host::version`]; one version covers every rule's host.
    fn version(&self) -> u64;
    /// The host of rule `rule`'s plan.
    fn host(&self, rule: usize) -> impl Host + '_;
    /// What a search calls of [`Self::host`] while it runs, as several
    /// threads may at once.
    type Shared<'a>: Calls + Send + Sync
    where
        Self: 'a;
    /// The host of rule `rule`'s plan for a search another thread runs.
    /// `None`, for a host that reads what is bound to one thread, keeps the
    /// rule's searches on the driver's.
    fn shared(&self, rule: usize) -> Option<Self::Shared<'_>>;
    /// Rule `rule`'s head. Asked for once per saturation, at the rule's first
    /// match: most rules of a set never match.
    fn head(&self, rule: usize) -> Vec<Head>;
    /// The ids rule `rule` is searched at: where it can match anew in
    /// `frontier`, or with `None` everywhere it can match.
    fn roots(&mut self, rule: usize, frontier: Option<&mut Delta<Self::Frontier>>) -> Vec<u32>;
    /// Make the database canonical again after a batch of writes, restoring
    /// whatever the client's tables promise.
    fn rebuild(&mut self);
    /// What the size limit of a saturation counts.
    fn size(&self) -> usize;
}

const RULE_SETS: usize = 4;

/// What a client keeps for its saturations: per plan, its search cache, and
/// which rules a round visits for the rule sets last saturated with.
#[derive(Default)]
pub struct Caches {
    plans: FxHashMap<u64, PlanCache>,
    indexes: Vec<RuleIndex>,
}

impl Caches {
    fn refreshed<C, const S: usize, H: Host + ?Sized>(
        &mut self,
        plan: &Plan,
        db: &Core<C, S>,
        host: &H,
        blocked: &mut Option<Param>,
    ) -> Option<&mut PlanCache> {
        if blocked.is_some_and(|param| host.empty(param)) {
            return None;
        }
        *blocked = None;
        let cache = match self.plans.entry(plan.id()) {
            Entry::Occupied(cache) => cache.into_mut(),
            Entry::Vacant(cache) => match plan.blocked(host) {
                Some(param) => {
                    *blocked = Some(param);
                    return None;
                }
                None => cache.insert(PlanCache::default()),
            },
        };
        plan.refresh(db, host, cache).then_some(cache)
    }

    /// The cache of `plan`, brought up to `host`'s version, or `None` when the
    /// plan has no match.
    pub fn plan<C, const S: usize, H: Host + ?Sized>(
        &mut self,
        plan: &Plan,
        db: &Core<C, S>,
        host: &H,
    ) -> Option<&mut PlanCache> {
        self.refreshed(plan, db, host, &mut None)
    }
}

/// Which rules a round has to visit, by what the round before wrote.
struct RuleIndex {
    /// Identifies the rule set the index was built for.
    rules: u64,
    /// The host version `by_cell` was extended to.
    version: Option<u64>,
    /// The row atoms of anchorable rules, in rule order.
    selects: Vec<Selection>,
    /// `(table, selected cell)` -> the anchorable rules with a row atom
    /// selecting it.
    by_cell: FxHashMap<(u32, u32), SmallVec<[u32; 2]>>,
    /// Per rule, the selection that last told it has no match.
    blocked: Vec<Option<Param>>,
    /// The `(table, column)` pairs `by_cell` names cells of.
    keyed: Vec<(u32, u8)>,
    /// The anchorable rules a risen value can make match, with the lattice
    /// columns they read as a mask.
    factual: Vec<(u32, u64)>,
    /// The rules that read no row, with the columns they read: a value alone
    /// makes them match, an id minted with it included.
    rowless: Vec<(u32, u64)>,
    /// The rules the change log cannot narrow: searched every round.
    always: Vec<u32>,
    /// Every rule of the saturation proper, for the round that searches all.
    all: Vec<usize>,
}

/// A row atom's selection, and per table how many of its cells `by_cell`
/// holds: resolutions only grow, a table's cells at the end.
struct Selection {
    rule: u32,
    param: Param,
    column: u8,
    seen: SmallVec<[(u32, usize); 2]>,
}

fn fingerprint(rules: &[Rule<'_>]) -> u64 {
    rules.iter().fold(rules.len() as u64, |hash, rule| {
        let id = rule.plan.id() ^ u64::from(rule.post_saturation);
        (hash.rotate_left(5) ^ id).wrapping_mul(0x9e37_79b9_7f4a_7c15)
    })
}

impl RuleIndex {
    fn new(rules: &[Rule<'_>]) -> Self {
        let atoms = rules.iter().map(|rule| rule.plan.query().atoms.len()).sum();
        let mut index = Self {
            rules: fingerprint(rules),
            version: None,
            selects: Vec::with_capacity(atoms),
            by_cell: FxHashMap::default(),
            blocked: vec![None; rules.len()],
            keyed: Vec::new(),
            factual: Vec::new(),
            rowless: Vec::new(),
            always: Vec::new(),
            all: Vec::with_capacity(rules.len()),
        };
        for (at, rule) in rules.iter().enumerate() {
            if rule.post_saturation {
                continue;
            }
            index.all.push(at);
            let plan = rule.plan;
            if !plan.anchorable() {
                // A plan of lattice atoms alone matches anew only where a
                // value rose.
                if plan.rowless() && plan.fact_anchorable() {
                    index.rowless.push((at as u32, plan.fact_columns()));
                } else {
                    index.always.push(at as u32);
                }
                continue;
            }
            for atom in &plan.query().atoms {
                if let Atom::Rows(rows) = atom {
                    index.selects.push(Selection {
                        rule: at as u32,
                        param: rows.select.param,
                        column: rows.select.column,
                        seen: SmallVec::new(),
                    });
                }
            }
            let columns = plan.fact_columns();
            if columns != 0 {
                index.factual.push((at as u32, columns));
            }
        }
        index
    }

    fn round<C, const S: usize, X: Saturate<C, S>>(
        &mut self,
        client: &X,
        caches: &mut Caches,
        rules: &[Rule<'_>],
        narrowed: bool,
        visit: &mut Vec<usize>,
    ) {
        visit.clear();
        if !narrowed {
            // A plan cached for another database is asked afresh of this one.
            for &at in &self.all {
                let blocked = &mut self.blocked[at];
                if blocked.is_none() {
                    *blocked = rules[at].plan.blocked(&client.host(at));
                }
            }
            visit.extend_from_slice(&self.all);
            return;
        }
        let version = client.version();
        if self.version != Some(version) {
            // A rule's cells are indexed once it can match. Its selections
            // sit together, in rule order.
            let (by_cell, keyed) = (&mut self.by_cell, &mut self.keyed);
            for selects in self.selects.chunk_by_mut(|a, b| a.rule == b.rule) {
                let rule = selects[0].rule;
                let plan = rules[rule as usize].plan;
                let host = client.host(rule as usize);
                let blocked = &mut self.blocked[rule as usize];
                let Some(cache) = caches.refreshed(plan, client.db(), &host, blocked) else {
                    continue;
                };
                for select in selects {
                    let tables = &cache.resolution(select.param).tables;
                    for (table, cells) in tables {
                        let known = select.seen.iter().position(|(seen, _)| seen == table);
                        let at = known.unwrap_or_else(|| {
                            select.seen.push((*table, 0));
                            if !keyed.contains(&(*table, select.column)) {
                                keyed.push((*table, select.column));
                            }
                            select.seen.len() - 1
                        });
                        for &cell in &cells[select.seen[at].1..] {
                            let rules = by_cell.entry((*table, cell)).or_default();
                            if rules.last() != Some(&rule) {
                                rules.push(rule);
                            }
                        }
                        select.seen[at].1 = cells.len();
                    }
                }
            }
            self.version = Some(version);
        }
        let db = client.db();
        for &(table, column) in &self.keyed {
            if !db.wrote(table) {
                continue;
            }
            let mut last = None;
            // The rows are sorted by cell, so each cell shows up as a run.
            db.each_new_cell(table, column as usize, |cell| {
                if last.replace(cell) != Some(cell)
                    && let Some(rules) = self.by_cell.get(&(table, cell))
                {
                    visit.extend(rules.iter().map(|&rule| rule as usize));
                }
            });
        }
        let rose = db.rose(false);
        if rose != 0 {
            let reading = self
                .factual
                .iter()
                .filter(|(_, columns)| columns & rose != 0);
            visit.extend(reading.map(|&(rule, _)| rule as usize));
        }
        let rose = rose | db.rose(true);
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

/// Per-round frontier of semi-naive saturation: the change log of the previous
/// round closed upward, cached by the heights a rule set asks for, and
/// whatever the client keeps beside it.
pub struct Delta<G = ()> {
    /// `levels[h]` is Δ_h; grown on demand, each from the one below it.
    levels: Vec<Vec<u32>>,
    pub client: G,
}

impl<G: Default> Delta<G> {
    /// Seed from a drained change log.
    pub fn new(changed: Vec<u32>) -> Self {
        Self {
            levels: vec![changed],
            client: G::default(),
        }
    }
}

impl<G> Delta<G> {
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

    /// Δ_height of `space`, ascending.
    pub fn at<C, const S: usize>(
        &mut self,
        db: &crate::Database<C, S>,
        space: SpaceId,
        height: usize,
    ) -> &[u32] {
        while self.levels.len() <= height {
            let below = self.levels.last().expect("seeded level");
            let next = db.close_upward(space, below.iter().copied(), Some(1));
            self.levels.push(next);
        }
        &self.levels[height]
    }

    /// Δ_height, once [`Self::at`] has grown the frontier that far.
    pub fn level(&self, height: usize) -> &[u32] {
        &self.levels[height]
    }
}

const MAX_THREADS: usize = 8;

// Below that, starting the thread costs more than its share of the searches.
const ROWS_PER_THREAD: usize = 2048;

// One thread is left to whatever runs beside the saturation.
fn threads() -> usize {
    static THREADS: OnceLock<usize> = OnceLock::new();
    *THREADS.get_or_init(|| {
        let asked = std::env::var("TIR_SATURATE_THREADS").ok();
        let asked = asked.and_then(|threads| threads.parse::<usize>().ok());
        let found = || std::thread::available_parallelism().map_or(1, usize::from);
        asked
            .unwrap_or_else(|| (found() - 1).min(MAX_THREADS))
            .max(1)
    })
}

// A search reads the database's core and its own plan's cache, so which thread
// ran it shows nowhere.
fn search_shared<C: Sync, const S: usize, X: Saturate<C, S>>(
    client: &X,
    caches: &mut Caches,
    rules: &[Rule<'_>],
    jobs: Vec<(usize, Prepared)>,
    found: &mut [(usize, Matches)],
) {
    let rows: usize = jobs.iter().map(|(_, prepared)| prepared.starts()).sum();
    let workers = threads().min(rows / ROWS_PER_THREAD).max(1);
    let groups = client.db().groups();
    let (core, groups): (&Core<C, S>, &Groups) = (client.db(), &groups);
    let work: Vec<_> = jobs
        .into_iter()
        .filter_map(|(slot, prepared)| {
            let rule = found[slot].0;
            let cache = caches.plans.remove(&rules[rule].plan.id())?;
            Some((slot, rule, prepared, cache, client.shared(rule)?))
        })
        .collect();
    let queue = Mutex::new(work.into_iter());
    let search = || {
        let mut done = Vec::new();
        loop {
            let next = queue.lock().expect("a queue of searches").next();
            let Some((slot, rule, prepared, mut cache, host)) = next else {
                return done;
            };
            let plan = rules[rule].plan;
            let matches = plan.run(core, groups, &host, &prepared, &mut cache);
            done.push((slot, rule, matches, cache));
        }
    };
    let done = std::thread::scope(|scope| {
        let others: Vec<_> = (1..workers).map(|_| scope.spawn(search)).collect();
        let mut done = search();
        for other in others {
            done.extend(other.join().expect("a search thread"));
        }
        done
    });
    for (slot, rule, matches, cache) in done {
        let later = std::mem::replace(&mut found[slot].1, matches);
        found[slot].1.extend(later);
        caches.plans.insert(rules[rule].plan.id(), cache);
    }
}

/// Saturate `rules` over `space` in place. Each round searches every rule
/// against one snapshot, then applies the matches a rule at a time, rebuilding
/// after each — a row born this round is visible only to the next round's
/// searches. Stops at a fixpoint (nothing the live ids, the
/// client's size or the raised values count changed, or an empty change log)
/// or at a limit.
///
/// A [`Rule::post_saturation`] rule sits out those rounds and fires once
/// against the fixpoint, over the ids the saturation touched: the rest was
/// already at that phase's fixpoint when the caller handed it over, so only
/// those can hold a match it does not have. The phase is terminal, so a
/// saturation that reached a fixpoint drains the change log on the way out,
/// which leaves the next assumption scope's log holding that scope's own
/// assertion rather than this fixpoint's tail. A stop on a limit is not a
/// fixpoint: it marks everything changed instead, since the matches it never
/// reached are not named by the log.
pub fn saturate<C: Sync, const S: usize, X: Saturate<C, S>>(
    client: &mut X,
    caches: &mut Caches,
    space: SpaceId,
    rules: &[Rule<'_>],
    iter_limit: usize,
    size_limit: usize,
) {
    let timer = Timer::start();
    register_rules(rules.iter().map(|rule| rule.name));
    // The first round searches before anything else rebuilds.
    client.rebuild();
    let mut log = take_changed(client, space);
    let mut touched = log.clone();
    let mut delta: Option<Delta<X::Frontier>> = log.take().map(Delta::new);
    let known = fingerprint(rules);
    let known = caches.indexes.iter().position(|index| index.rules == known);
    let mut index = match known {
        Some(at) => caches.indexes.swap_remove(at),
        None => RuleIndex::new(rules),
    };
    let mut visit: Vec<usize> = Vec::with_capacity(rules.len());
    let mut heads: Vec<Option<Vec<Head>>> = vec![None; rules.len()];
    let mut batch = Batch::default();
    let mut iters = 0;
    let mut on_a_limit = true;
    loop {
        let size = client.size();
        if iters >= iter_limit || size >= size_limit {
            break;
        }
        let progress = |client: &X| {
            let db = client.db();
            (db.live(space), client.size(), db.stats().raises)
        };
        let before = progress(client);

        let mut stats = RoundStats::start(client.db().stats(), delta.as_ref());
        let mut found: Vec<(usize, Matches)> = Vec::with_capacity(rules.len());
        let mut shared: Vec<(usize, Prepared)> = Vec::new();
        // Most rounds write a handful of rows, and most rules select none of
        // their cells: those rules are not visited at all.
        index.round(client, caches, rules, delta.is_some(), &mut visit);
        for &at in &visit {
            let plan = rules[at].plan;
            // A rule one of whose row atoms reads no table has no match, and is
            // settled before any root is worked out.
            let blocked = &mut index.blocked[at];
            let Some(cache) = caches.refreshed(plan, client.db(), &client.host(at), blocked) else {
                continue;
            };
            if plan.anchorable() {
                // The round's new matches are the ones holding a row the round
                // before wrote, found from those rows, plus the ones a value
                // alone made new, found the way the change log names them: at
                // the roots near a change, among old rows.
                // A host other threads may read has the search from rows
                // prepared here and run with the round's other such.
                let (db, host) = (client.db(), client.host(at));
                let rows = match (delta.is_some(), client.shared(at).is_some()) {
                    (false, false) => Ok(plan.search_all(db, &host, cache)),
                    (true, false) => Ok(plan.search_new(db, &host, cache)),
                    (false, true) => Err(plan.prepare_all(db, &host, cache)),
                    (true, true) => Err(plan.prepare_new(db, &host, cache)),
                };
                drop(host);
                let (mut matches, prepared) = match rows {
                    Ok(matches) => (matches, None),
                    Err(prepared) => (Matches::default(), prepared),
                };
                shared.extend(prepared.map(|prepared| (found.len(), prepared)));
                let matches = match delta.as_mut() {
                    None => matches,
                    Some(delta) => {
                        if plan.fact_columns() & client.db().rose(false) != 0 {
                            if plan.fact_anchorable() {
                                let host = client.host(at);
                                matches.extend(plan.search_risen(client.db(), &host, false, cache));
                            } else {
                                let roots = client.roots(at, Some(delta));
                                stats.searched(roots.len(), Some(delta));
                                let host = client.host(at);
                                let found = plan.search_facts(client.db(), &host, roots, cache);
                                matches.extend(found);
                            }
                        }
                        matches
                    }
                };
                found.push((at, matches));
                continue;
            }
            if delta.is_some() && plan.rowless() && plan.fact_anchorable() {
                let matches = plan.search_risen(client.db(), &client.host(at), true, cache);
                found.push((at, matches));
                continue;
            }
            // Everything a rule reads is an atom or a guard over what an atom
            // bound, so both narrowings come free of any hand-asserted licence
            // — for a rule the change log can speak for. It cannot speak for
            // one whose match depends on rows outside the root's cone.
            let bounded = !plan.unbounded();
            let roots = client.roots(at, delta.as_mut().filter(|_| bounded));
            stats.searched(roots.len(), delta.as_ref());
            if roots.is_empty() {
                continue;
            }
            let fresh = (delta.is_some() && bounded, 0);
            let host = client.host(at);
            let matches =
                plan.search_roots(client.db(), &host, roots, None::<&Unchecked>, fresh, cache);
            found.push((at, matches));
        }
        if !shared.is_empty() {
            search_shared(client, caches, rules, shared, &mut found);
        }
        let tracing = trace_enabled();
        for (at, matches) in found {
            if matches.is_empty() {
                continue;
            }
            let rule = &rules[at];
            let head = heads[at].get_or_insert_with(|| client.head(at));
            if tracing {
                for index in 0..matches.len() {
                    let root = client.db().root(space, matches.get(index).0);
                    eprintln!("M {} {root}", rule.name);
                }
            }
            apply(
                client,
                space,
                at,
                head,
                rule.head_vars,
                &matches,
                &mut batch,
            );
            stats.applied(matches.len(), batch.idle());
            count_rule(rule.name, matches.len(), batch.idle());
            // The next rule's inserts are looked up in tables that name what
            // this one equated.
            client.rebuild();
            if let Some(cache) = caches.plans.get_mut(&rule.plan.id()) {
                cache.recycle(matches);
            }
        }
        client.rebuild();
        stats.finish(client.db().stats());

        iters += 1;
        let log = take_changed(client, space);
        match (&mut touched, &log) {
            (Some(all), Some(changed)) => all.extend(changed.iter().copied()),
            _ => touched = None,
        }
        delta = log.map(Delta::new);
        if delta.as_ref().is_some_and(Delta::is_empty) {
            on_a_limit = false;
            break;
        }
        if progress(client) == before {
            // The counts held, but a round that changed only values changed
            // nothing they count, and is not a fixpoint. `None` is the widest
            // such log there is, so it counts too.
            on_a_limit = delta.as_ref().is_none_or(|delta| !delta.is_empty());
            break;
        }
    }
    caches.indexes.truncate(RULE_SETS - 1);
    caches.indexes.insert(0, index);
    if on_a_limit {
        client.db_mut().mark_all_changed(space);
        touched = None;
    }
    client.rebuild();
    post_saturate(client, caches, space, rules, &mut heads, touched);
    if !on_a_limit {
        take_changed(client, space);
    }
    timer.finish();
}

fn take_changed<C, const S: usize>(
    client: &mut impl Client<C, S>,
    space: SpaceId,
) -> Option<Vec<u32>> {
    let db = client.db_mut();
    db.advance();
    db.take_changed(space)
}

fn post_saturate<C, const S: usize, X: Saturate<C, S>>(
    client: &mut X,
    caches: &mut Caches,
    space: SpaceId,
    rules: &[Rule<'_>],
    heads: &mut [Option<Vec<Head>>],
    touched: Option<Vec<u32>>,
) {
    let mut touched: Option<Delta<X::Frontier>> = touched.map(|mut all| {
        for id in &mut all {
            *id = client.db().root(space, *id);
        }
        all.sort_unstable();
        all.dedup();
        Delta::new(all)
    });
    let mut found: SmallVec<[(usize, Matches); 4]> = SmallVec::new();
    for (at, rule) in rules.iter().enumerate() {
        if !rule.post_saturation {
            continue;
        }
        let roots = client.roots(at, touched.as_mut().filter(|_| !rule.plan.unbounded()));
        if roots.is_empty() {
            continue;
        }
        let host = client.host(at);
        let Some(cache) = caches.plan(rule.plan, client.db(), &host) else {
            continue;
        };
        let matches = rule.plan.search_roots(
            client.db(),
            &host,
            roots,
            None::<&Unchecked>,
            (false, 0),
            cache,
        );
        found.push((at, matches));
    }
    let mut batch = Batch::default();
    for (at, matches) in &found {
        if matches.is_empty() {
            continue;
        }
        let rule = &rules[*at];
        let head = heads[*at].get_or_insert_with(|| client.head(*at));
        apply(
            client,
            space,
            *at,
            head,
            rule.head_vars,
            matches,
            &mut batch,
        );
        count_rule(rule.name, matches.len(), batch.idle());
        client.rebuild();
    }
}
