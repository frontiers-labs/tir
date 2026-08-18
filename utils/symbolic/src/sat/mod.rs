//! Textbook MiniSat-style CDCL SAT solver backing bit-blasted QF_BV queries:
//! two-watched-literal propagation, 1-UIP learning, VSIDS, Luby restarts.

/// A boolean variable, indexing the solver's per-variable tables.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Var(u32);

impl Var {
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A literal: a variable with a sign, packed as `var << 1 | negated`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Lit(u32);

impl Lit {
    pub fn new(var: Var, negated: bool) -> Self {
        Lit(var.0 << 1 | negated as u32)
    }

    pub fn positive(var: Var) -> Self {
        Lit::new(var, false)
    }

    pub fn negative(var: Var) -> Self {
        Lit::new(var, true)
    }

    pub fn var(self) -> Var {
        Var(self.0 >> 1)
    }

    pub fn is_negated(self) -> bool {
        self.0 & 1 == 1
    }

    pub fn negate(self) -> Self {
        Lit(self.0 ^ 1)
    }

    fn index(self) -> usize {
        self.0 as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SatResult {
    /// Satisfiable; the vector holds the value of each variable by index.
    Sat(Vec<bool>),
    Unsat,
    /// The conflict budget was exhausted before a verdict was reached.
    Unknown,
}

struct Clause {
    /// `lits[0]` and `lits[1]` are the two watched literals.
    lits: Vec<Lit>,
}

/// CDCL SAT solver: build with [`Solver::new_var`]/[`Solver::add_clause`], then [`Solver::solve`].
pub struct Solver {
    clauses: Vec<Clause>,
    /// `watches[l]`: clauses watching literal `l`, processed when `l` becomes false.
    watches: Vec<Vec<usize>>,
    /// Current truth value of each variable, `None` if unassigned.
    assign: Vec<Option<bool>>,
    /// Decision level at which each variable was assigned.
    level: Vec<usize>,
    /// The clause that forced each variable's assignment, `None` for decisions.
    reason: Vec<Option<usize>>,
    /// Saved last polarity per variable, for phase saving.
    phase: Vec<bool>,
    activity: Vec<f64>,
    var_inc: f64,
    /// Assignment stack in chronological order.
    trail: Vec<Lit>,
    /// Index into `trail` of the first not-yet-propagated literal.
    qhead: usize,
    /// `trail_lim[d]` is the trail length when decision level `d+1` began.
    trail_lim: Vec<usize>,
    /// Set once an empty clause (or top-level conflict) makes the problem unsat.
    unsat: bool,
}

impl Default for Solver {
    fn default() -> Self {
        Self::new()
    }
}

impl Solver {
    pub fn new() -> Self {
        Solver {
            clauses: Vec::new(),
            watches: Vec::new(),
            assign: Vec::new(),
            level: Vec::new(),
            reason: Vec::new(),
            phase: Vec::new(),
            activity: Vec::new(),
            var_inc: 1.0,
            trail: Vec::new(),
            qhead: 0,
            trail_lim: Vec::new(),
            unsat: false,
        }
    }

    pub fn num_vars(&self) -> usize {
        self.assign.len()
    }

    /// Allocate a fresh variable.
    pub fn new_var(&mut self) -> Var {
        let v = Var(self.assign.len() as u32);
        self.assign.push(None);
        self.level.push(0);
        self.reason.push(None);
        self.phase.push(false);
        self.activity.push(0.0);
        self.watches.push(Vec::new());
        self.watches.push(Vec::new());
        v
    }

    /// Add a clause; literals dedup, tautologies drop, the empty clause is unsat.
    pub fn add_clause(&mut self, lits: &[Lit]) {
        if self.unsat {
            return;
        }
        let mut cl: Vec<Lit> = Vec::with_capacity(lits.len());
        for &l in lits {
            if cl.contains(&l.negate()) {
                return; // tautology
            }
            if !cl.contains(&l) {
                cl.push(l);
            }
        }
        match cl.len() {
            0 => self.unsat = true,
            1 => {
                if !self.enqueue(cl[0], None) {
                    self.unsat = true;
                }
            }
            _ => {
                self.add_watched_clause(cl);
            }
        }
    }

    /// Solve to a verdict, with no conflict budget.
    pub fn solve(&mut self) -> SatResult {
        self.solve_with_budget(None)
    }

    /// Solve, giving up with [`SatResult::Unknown`] after `max_conflicts` conflicts.
    pub fn solve_with_budget(&mut self, max_conflicts: Option<u64>) -> SatResult {
        if self.unsat {
            return SatResult::Unsat;
        }
        let mut conflicts: u64 = 0;
        let mut restart_no: u64 = 0;
        let mut limit = luby(restart_no) * 100;
        loop {
            match self.propagate() {
                Some(confl) => {
                    conflicts += 1;
                    if self.decision_level() == 0 {
                        self.unsat = true;
                        return SatResult::Unsat;
                    }
                    let (learnt, bt_level) = self.analyze(confl);
                    self.cancel_until(bt_level);
                    self.learn(&learnt);
                    self.decay();
                    if max_conflicts.is_some_and(|m| conflicts >= m) {
                        return SatResult::Unknown;
                    }
                    if conflicts >= limit {
                        restart_no += 1;
                        limit = conflicts + luby(restart_no) * 100;
                        self.cancel_until(0);
                    }
                }
                None => match self.pick_branch() {
                    None => return SatResult::Sat(self.model()),
                    Some(lit) => {
                        self.trail_lim.push(self.trail.len());
                        self.enqueue(lit, None);
                    }
                },
            }
        }
    }

    /// The value of a variable in the last satisfying assignment.
    pub fn value(&self, var: Var) -> bool {
        self.assign[var.index()].unwrap_or(false)
    }

    fn model(&self) -> Vec<bool> {
        self.assign.iter().map(|a| a.unwrap_or(false)).collect()
    }

    fn decision_level(&self) -> usize {
        self.trail_lim.len()
    }

    fn value_lit(&self, l: Lit) -> Option<bool> {
        self.assign[l.var().index()].map(|b| b ^ l.is_negated())
    }

    /// Record that `l` is true, returning `false` on an immediate contradiction.
    fn enqueue(&mut self, l: Lit, reason: Option<usize>) -> bool {
        match self.value_lit(l) {
            Some(true) => true,
            Some(false) => false,
            None => {
                let v = l.var().index();
                self.assign[v] = Some(!l.is_negated());
                self.level[v] = self.decision_level();
                self.reason[v] = reason;
                self.trail.push(l);
                true
            }
        }
    }

    /// Propagate unit consequences, returning a conflicting clause if one arises.
    fn propagate(&mut self) -> Option<usize> {
        while self.qhead < self.trail.len() {
            let p = self.trail[self.qhead];
            self.qhead += 1;
            let false_lit = p.negate();

            let mut ws = std::mem::take(&mut self.watches[false_lit.index()]);
            let mut new_ws: Vec<usize> = Vec::with_capacity(ws.len());
            let mut conflict = None;
            let mut i = 0;
            while i < ws.len() {
                let cref = ws[i];
                i += 1;

                // Keep the falsified literal in slot 1.
                if self.clauses[cref].lits[0] == false_lit {
                    self.clauses[cref].lits.swap(0, 1);
                }
                let w0 = self.clauses[cref].lits[0];
                if self.value_lit(w0) == Some(true) {
                    new_ws.push(cref);
                    continue;
                }

                // Hunt for a non-false replacement watch.
                let mut found = false;
                let len = self.clauses[cref].lits.len();
                for k in 2..len {
                    let lk = self.clauses[cref].lits[k];
                    if self.value_lit(lk) != Some(false) {
                        self.clauses[cref].lits.swap(1, k);
                        let nw = self.clauses[cref].lits[1];
                        self.watches[nw.index()].push(cref);
                        found = true;
                        break;
                    }
                }
                if found {
                    continue;
                }

                // No replacement: the clause is unit or conflicting on w0.
                new_ws.push(cref);
                match self.value_lit(w0) {
                    Some(false) => {
                        conflict = Some(cref);
                        while i < ws.len() {
                            new_ws.push(ws[i]);
                            i += 1;
                        }
                        break;
                    }
                    _ => {
                        self.enqueue(w0, Some(cref));
                    }
                }
            }
            ws.clear();
            self.watches[false_lit.index()] = new_ws;

            if let Some(c) = conflict {
                self.qhead = self.trail.len();
                return Some(c);
            }
        }
        None
    }

    /// 1-UIP analysis: returns the learned clause (asserting literal at index 0) and backjump level.
    fn analyze(&mut self, conflict: usize) -> (Vec<Lit>, usize) {
        let mut seen = vec![false; self.num_vars()];
        let mut learnt: Vec<Lit> = vec![Lit(0)]; // slot 0: asserting literal
        let mut path_c = 0usize;
        let mut p: Option<Lit> = None;
        let mut confl = conflict;
        let mut index = self.trail.len();

        loop {
            let start = if p.is_some() { 1 } else { 0 };
            let len = self.clauses[confl].lits.len();
            for j in start..len {
                let q = self.clauses[confl].lits[j];
                let v = q.var().index();
                if !seen[v] && self.level[v] > 0 {
                    self.bump(q.var());
                    seen[v] = true;
                    if self.level[v] >= self.decision_level() {
                        path_c += 1;
                    } else {
                        learnt.push(q);
                    }
                }
            }

            // Next clause to resolve: the most recent seen literal on the trail.
            loop {
                index -= 1;
                if seen[self.trail[index].var().index()] {
                    break;
                }
            }
            let pl = self.trail[index];
            seen[pl.var().index()] = false;
            path_c -= 1;
            p = Some(pl);
            if path_c == 0 {
                break;
            }
            confl = self.reason[pl.var().index()].expect("implied literal has a reason");
        }
        learnt[0] = p.unwrap().negate();

        // Backjump to the second-highest level; move that literal to slot 1 for watching.
        let bt_level = if learnt.len() == 1 {
            0
        } else {
            let mut max_i = 1;
            for k in 2..learnt.len() {
                if self.level[learnt[k].var().index()] > self.level[learnt[max_i].var().index()] {
                    max_i = k;
                }
            }
            learnt.swap(1, max_i);
            self.level[learnt[1].var().index()]
        };
        (learnt, bt_level)
    }

    /// Add a just-learned clause and assert its unit literal.
    fn learn(&mut self, learnt: &[Lit]) {
        if learnt.len() == 1 {
            self.enqueue(learnt[0], None);
            return;
        }
        let cref = self.add_watched_clause(learnt.to_vec());
        self.enqueue(learnt[0], Some(cref));
    }

    /// Store a clause with two or more literals, watching `lits[0]` and `lits[1]`.
    fn add_watched_clause(&mut self, lits: Vec<Lit>) -> usize {
        let cref = self.clauses.len();
        self.watches[lits[0].index()].push(cref);
        self.watches[lits[1].index()].push(cref);
        self.clauses.push(Clause { lits });
        cref
    }

    /// Undo assignments made above decision level `level`.
    fn cancel_until(&mut self, level: usize) {
        if self.decision_level() <= level {
            return;
        }
        let target = self.trail_lim[level];
        while self.trail.len() > target {
            let l = self.trail.pop().unwrap();
            let v = l.var().index();
            self.phase[v] = self.assign[v].unwrap();
            self.assign[v] = None;
            self.reason[v] = None;
        }
        self.trail_lim.truncate(level);
        self.qhead = self.trail.len();
    }

    /// Pick the highest-activity unassigned variable, or `None` if all assigned.
    fn pick_branch(&self) -> Option<Lit> {
        let mut best: Option<Var> = None;
        let mut best_act = f64::NEG_INFINITY;
        for i in 0..self.num_vars() {
            if self.assign[i].is_none() && self.activity[i] >= best_act {
                best_act = self.activity[i];
                best = Some(Var(i as u32));
            }
        }
        best.map(|v| Lit::new(v, !self.phase[v.index()]))
    }

    fn bump(&mut self, var: Var) {
        let a = &mut self.activity[var.index()];
        *a += self.var_inc;
        if *a > 1e100 {
            for act in &mut self.activity {
                *act *= 1e-100;
            }
            self.var_inc *= 1e-100;
        }
    }

    fn decay(&mut self) {
        self.var_inc /= 0.95;
    }
}

/// The Luby restart sequence (0-based): 1,1,2,1,1,2,4,1,1,2,1,1,2,4,8,...
fn luby(x: u64) -> u64 {
    let mut size = 1u64;
    let mut seq = 0u32;
    while size < x + 1 {
        seq += 1;
        size = 2 * size + 1;
    }
    let mut x = x;
    while size - 1 != x {
        size = (size - 1) >> 1;
        seq -= 1;
        x %= size;
    }
    1u64 << seq
}
