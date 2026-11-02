//! Colors against clones. Every live color of a colored database must hold
//! the e-graph that a fresh database reaches from the base facts and the
//! facts of every color at or below it, replayed in the same order and run
//! with the same program. Random theories, color orders and facts come from
//! a seed; the same check run against clones with one planted bug each shows
//! that it notices each kind of mistake.

use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

use tir_egglog::prelude::*;
use tir_egglog::{
    Color, Database, Merge, Program, ReportLevel, Rules, Schedule, Signature, SortId, SortKind,
    TableId, Value,
};

// xorshift64*.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())]
    }
}

#[derive(Clone)]
enum Rule {
    Rewrite(Expr, Expr),
    Rule(Facts, Actions),
}

// One Eq sort `E` with `Num(i64)`, `F(E)` and `G(E, E)`, an upper bound
// `ub(E) -> i64` that keeps the least value written, and a relation `R(E, E)`.
struct Theory {
    db: Database,
    program: Program,
    e: SortId,
    num: TableId,
    f: TableId,
    g: TableId,
    ub: TableId,
    r: TableId,
}

impl Theory {
    // `keep` merges two bounds of `ub`; the semantics is `i64::min`.
    fn new(rules: &[Rule], keep: fn(i64, i64) -> i64) -> Self {
        let mut db = Database::new();
        let e = db.add_sort("E", SortKind::Eq);
        let int = db.sort("i64").unwrap();
        let signature = Signature {
            inputs: vec![int, int],
            output: int,
        };
        let merge = db.add_primitive("keep", signature, move |ctx, args| {
            let (a, b) = (
                ctx.base_value::<i64>(args[0]),
                ctx.base_value::<i64>(args[1]),
            );
            Some(ctx.base(keep(a, b)))
        });
        let num = db.add_constructor("Num", &[int], e);
        let f = db.add_constructor("F", &[e], e);
        let g = db.add_constructor("G", &[e, e], e);
        let merge = Merge::Prim(merge, vec![Merge::Old, Merge::New]);
        let ub = db.add_function("ub", &[e], int, merge);
        let r = db.add_relation("R", &[e, e]);
        let mut set = Rules::new();
        for rule in rules.iter().cloned() {
            match rule {
                Rule::Rewrite(lhs, rhs) => set.rewrite(&db, "", lhs, rhs),
                Rule::Rule(facts, actions) => set.rule(&db, "", facts, actions),
            }
            .unwrap();
        }
        let program = set.compile(&db).unwrap();
        Self {
            db,
            program,
            e,
            num,
            f,
            g,
            ub,
            r,
        }
    }

    fn tables(&self) -> [TableId; 5] {
        [self.num, self.f, self.g, self.ub, self.r]
    }
}

const VARS: [&str; 3] = ["x", "y", "z"];

fn num(k: i64) -> Expr {
    exprs::call("Num", vec![exprs::int(k)])
}

fn var(name: &str, bound: &mut Vec<&'static str>) -> Expr {
    if let Some(&name) = VARS.iter().find(|&&v| v == name) {
        bound.push(name);
    }
    exprs::var(name)
}

fn pattern(rng: &mut Rng, depth: usize, bound: &mut Vec<&'static str>) -> Expr {
    match rng.below(if depth == 0 { 2 } else { 4 }) {
        0 => var(rng.pick(&VARS), bound),
        1 => num(rng.below(3) as i64),
        2 => exprs::call("F", vec![pattern(rng, depth - 1, bound)]),
        _ => {
            let (a, b) = (
                pattern(rng, depth - 1, bound),
                pattern(rng, depth - 1, bound),
            );
            exprs::call("G", vec![a, b])
        }
    }
}

fn leaf(rng: &mut Rng, bound: &[&str]) -> Expr {
    match bound {
        [] => num(rng.below(3) as i64),
        _ if rng.chance(25) => num(rng.below(3) as i64),
        _ => exprs::var(rng.pick(bound)),
    }
}

// At most one constructor over bound variables and constants. Each new node
// lands in the class it is unioned with, so the classes stay those of the
// terms, the constants and the fresh ids, and saturation ends.
fn node(rng: &mut Rng, bound: &[&str]) -> Expr {
    match rng.below(3) {
        0 => leaf(rng, bound),
        1 => exprs::call("F", vec![leaf(rng, bound)]),
        _ => exprs::call("G", vec![leaf(rng, bound), leaf(rng, bound)]),
    }
}

// The bound `n` flows only into `ub` and the only bound a rule compares
// against is 0, the least one: matches stay true as bounds fall, so the
// fixed point does not depend on the order facts arrive in.
fn rule(rng: &mut Rng) -> Rule {
    let (mut facts, mut bound, mut n) = (Vec::new(), Vec::new(), false);
    for i in 0..1 + rng.below(3) {
        let mut v = |rng: &mut Rng| var(rng.pick(&VARS), &mut bound);
        let fact = match if i == 0 { rng.below(2) } else { rng.below(4) } {
            0 => Fact::Fact(exprs::call("R", vec![v(rng), v(rng)])),
            1 => {
                let ub = exprs::call("ub", vec![v(rng)]);
                if !n && rng.chance(60) {
                    n = true;
                    Fact::Eq(span!(), ub, exprs::var("n"))
                } else {
                    Fact::Eq(span!(), ub, exprs::int(0))
                }
            }
            2 => Fact::Eq(span!(), v(rng), exprs::call("F", vec![v(rng)])),
            _ => {
                let (a, b, c) = (v(rng), v(rng), v(rng));
                Fact::Eq(span!(), a, exprs::call("G", vec![b, c]))
            }
        };
        facts.push(fact);
    }
    let mut actions = Vec::new();
    for _ in 0..1 + rng.below(2) {
        let v = |rng: &mut Rng| exprs::var(rng.pick(&bound));
        actions.push(match rng.below(4) {
            0 => Action::Union(span!(), v(rng), v(rng)),
            1 => Action::Expr(span!(), exprs::call("R", vec![v(rng), v(rng)])),
            2 => {
                let value = match n && rng.chance(70) {
                    true => exprs::var("n"),
                    false => exprs::int(rng.below(4) as i64),
                };
                Action::Set(span!(), "ub".to_owned(), vec![v(rng)], value)
            }
            _ => Action::Union(span!(), v(rng), node(rng, &bound)),
        });
    }
    Rule::Rule(Facts(facts), GenericActions(actions))
}

fn rewrite(rng: &mut Rng) -> Rule {
    let mut bound = Vec::new();
    let lhs = match rng.below(2) {
        0 => exprs::call("F", vec![pattern(rng, 1, &mut bound)]),
        _ => {
            let (a, b) = (pattern(rng, 1, &mut bound), pattern(rng, 1, &mut bound));
            exprs::call("G", vec![a, b])
        }
    };
    Rule::Rewrite(lhs, node(rng, &bound))
}

#[derive(Clone, Copy)]
enum Cell {
    Term(usize),
    Int(i64),
}

// Facts name colors by their index in the order they were made, 0 being
// the base, and terms by their index among the `Term`s and `Fresh`es.
#[derive(Clone)]
enum Op {
    Term(TableId, Vec<Cell>),
    Fresh,
    Union(usize, usize, usize),
    UnionAll(Vec<(usize, usize, usize)>),
    Insert(usize, TableId, Vec<Cell>),
    Color(Vec<usize>),
    Colors(Vec<usize>),
    Drop(usize),
    Run,
}

// The colors made so far: `below[c]` has a bit for c, the base and every
// color under c.
struct Order {
    below: Vec<u64>,
    live: Vec<bool>,
    made_at: Vec<usize>,
}

impl Order {
    fn new() -> Self {
        Self {
            below: vec![1],
            live: vec![true],
            made_at: vec![0],
        }
    }

    fn add(&mut self, parents: &[usize], at: usize) {
        let bit = 1 << self.below.len();
        let below = parents
            .iter()
            .fold(1 | bit, |bits, &p| bits | self.below[p]);
        self.below.push(below);
        self.live.push(true);
        self.made_at.push(at);
    }

    fn apply(&mut self, at: usize, op: &Op) {
        match op {
            Op::Color(parents) => self.add(parents, at),
            Op::Colors(parents) => parents.iter().for_each(|&p| self.add(&[p], at)),
            &Op::Drop(c) => {
                for (live, below) in self.live.iter_mut().zip(&self.below) {
                    *live &= below >> c & 1 == 0;
                }
            }
            _ => {}
        }
    }

    // Whether `c` sees the facts of `d`.
    fn sees(&self, c: usize, d: usize) -> bool {
        self.below[c] >> d & 1 == 1
    }

    fn live(&self) -> Vec<usize> {
        (0..self.live.len()).filter(|&c| self.live[c]).collect()
    }
}

struct Gen<'a> {
    rng: &'a mut Rng,
    th: &'a Theory,
    ops: Vec<Op>,
    terms: usize,
    order: Order,
}

impl Gen<'_> {
    fn push(&mut self, op: Op) {
        self.order.apply(self.ops.len(), &op);
        if matches!(op, Op::Term(..) | Op::Fresh) {
            self.terms += 1;
        }
        self.ops.push(op);
    }

    fn term(&mut self, depth: usize) -> usize {
        let (table, cells) = match self.rng.below(if depth == 0 { 3 } else { 6 }) {
            0 if self.terms > 0 => return self.rng.below(self.terms),
            0 | 1 => {
                self.push(Op::Fresh);
                return self.terms - 1;
            }
            2 => (self.th.num, vec![Cell::Int(self.rng.below(4) as i64)]),
            3 | 4 => (self.th.f, vec![Cell::Term(self.term(depth - 1))]),
            _ => {
                let (a, b) = (self.term(depth - 1), self.term(depth - 1));
                (self.th.g, vec![Cell::Term(a), Cell::Term(b)])
            }
        };
        self.push(Op::Term(table, cells));
        self.terms - 1
    }

    fn any(&mut self) -> usize {
        self.rng.below(self.terms)
    }

    fn color(&mut self) -> usize {
        let live = self.order.live();
        self.rng.pick(&live)
    }

    // Up to `n` distinct live colors, the base among them now and then.
    fn parents(&mut self, n: usize) -> Vec<usize> {
        let mut parents: Vec<usize> = (0..n).map(|_| self.color()).collect();
        parents.sort_unstable();
        parents.dedup();
        parents
    }

    // A fact in color `c`, the base when `c` is 0.
    fn fact(&mut self, c: usize) {
        let (a, b) = (self.any(), self.any());
        let op = match self.rng.below(7) {
            0..=2 => Op::Union(c, a, b),
            3 | 4 => {
                let bound = Cell::Int(self.rng.below(4) as i64);
                Op::Insert(c, self.th.ub, vec![Cell::Term(a), bound])
            }
            5 => Op::Insert(c, self.th.r, vec![Cell::Term(a), Cell::Term(b)]),
            _ => {
                self.push(Op::Fresh);
                let out = Cell::Term(self.terms - 1);
                Op::Insert(c, self.th.f, vec![Cell::Term(a), out])
            }
        };
        self.push(op);
    }

    fn step(&mut self) {
        let colored = self.order.live().len() > 1;
        match self.rng.below(12) {
            0 => {
                self.term(2);
            }
            1 | 2 => self.fact(0),
            3 if colored => {
                let n = 2 + self.rng.below(2);
                let unions = (0..n).map(|_| (self.color(), self.any(), self.any()));
                let op = Op::UnionAll(unions.collect());
                self.push(op);
            }
            4 if colored && self.rng.chance(30) => {
                let live = self.order.live();
                let c = self.rng.pick(&live[1..]);
                self.push(Op::Drop(c));
            }
            _ if colored => {
                let mut c = self.color();
                if c == 0 {
                    c = self.color();
                }
                self.fact(c);
            }
            _ => self.fact(0),
        }
    }

    // One color, or with `colors` up to `room` of them.
    fn make_color(&mut self, room: usize) {
        let n = self.rng.below(4);
        if n > 0 && n <= room && self.rng.chance(30) {
            let parents = self.parents(n);
            self.push(Op::Colors(parents));
        } else {
            let parents = self.parents(n);
            let keep_base = self.rng.chance(50);
            let parents = parents.into_iter().filter(|&p| p != 0 || keep_base);
            self.push(Op::Color(parents.collect()));
        }
    }

    fn ops(mut self) -> Vec<Op> {
        let colors = self.rng.below(33);
        let phases = 2 + self.rng.below(3);
        for _ in 0..2 + self.rng.below(4) {
            self.term(3);
        }
        for phase in 0..phases {
            let made = self.order.below.len() - 1;
            let quota = colors.saturating_sub(made) / (phases - phase);
            for _ in 0..4 + self.rng.below(10) + quota {
                let made = self.order.below.len() - 1;
                match made < colors && self.rng.chance(50) {
                    true => self.make_color(colors - made),
                    false => self.step(),
                }
            }
            self.push(Op::Run);
        }
        self.ops
    }
}

fn row(db: &Database, terms: &[Value], cells: &[Cell]) -> Vec<Value> {
    let cell = |cell: &Cell| match *cell {
        Cell::Term(t) => terms[t],
        Cell::Int(i) => db.base(i),
    };
    cells.iter().map(cell).collect()
}

// Each item's label is the index of the first item equal to it, so two
// e-graphs with different ids label equal partitions the same way.
fn labels<K: Clone + Eq + Hash>(keys: &[K]) -> Vec<usize> {
    let mut first = HashMap::new();
    (keys.iter().enumerate())
        .map(|(i, key)| *first.entry(key.clone()).or_insert(i))
        .collect()
}

// What a check compares of one e-graph, over the terms the test made: their
// classes, their bounds, the `ub` and `R` rows between them, the number of
// distinct rows of each table, and the id cells of rows that are not roots.
#[derive(Debug, PartialEq)]
struct View {
    classes: Vec<usize>,
    bounds: Vec<Option<i64>>,
    ub: BTreeSet<(usize, i64)>,
    r: BTreeSet<(usize, usize)>,
    sizes: Vec<usize>,
    stale: usize,
}

// The base of `db` for `None`, else the color.
fn view(th: &Theory, db: &Database, color: Option<Color>, terms: &[Value]) -> View {
    let find = |v: Value| color.map_or_else(|| db.find(v), |c| db.find_in(c, v));
    let roots: Vec<Value> = terms.iter().map(|&v| find(v)).collect();
    let classes = labels(&roots);
    let class: HashMap<Value, usize> = roots.iter().copied().zip(classes.clone()).collect();
    let int = |v| db.base_value::<i64>(v);
    let (mut ub, mut r, mut sizes, mut stale) = (BTreeSet::new(), BTreeSet::new(), vec![], 0);
    for table in th.tables() {
        let rows: BTreeSet<Vec<Value>> = match color {
            None => db.rows(table).map(|row| row.values().collect()).collect(),
            Some(c) => db
                .rows_in(c, table)
                .map(|row| row.values().collect())
                .collect(),
        };
        let columns = db.schema().table(table).columns();
        let ids: Vec<usize> = (0..columns.len()).filter(|&i| columns[i] == th.e).collect();
        for row in &rows {
            stale += ids.iter().filter(|&&i| find(row[i]) != row[i]).count();
            let (a, b) = (class.get(&row[0]), row.get(1).and_then(|v| class.get(v)));
            match (a, b) {
                (Some(&a), _) if table == th.ub => {
                    ub.insert((a, int(row[1])));
                }
                (Some(&a), Some(&b)) if table == th.r => {
                    r.insert((a, b));
                }
                _ => {}
            }
        }
        sizes.push(rows.len());
    }
    let bound = |&v: &Value| match color {
        None => db.lookup(th.ub, &[v]),
        Some(c) => db.lookup_in(c, th.ub, &[v]),
    };
    View {
        classes,
        bounds: terms.iter().map(|v| bound(v).map(int)).collect(),
        ub,
        r,
        sizes,
        stale,
    }
}

trait Subject {
    fn apply(&mut self, at: usize, op: &Op);
    fn view(&mut self, c: usize) -> View;
    // Labels of the terms by their classes in every color of `cs`.
    fn common(&mut self, cs: &[usize]) -> Result<Vec<usize>, String>;
}

// One ordinary database that replays the facts one color sees.
struct Replica {
    db: Database,
    program: Program,
    terms: Vec<Value>,
    done: usize,
}

impl Replica {
    fn new(th: &Theory) -> Self {
        Self {
            db: th.db.clone(),
            program: th.program.clone(),
            terms: Vec::new(),
            done: 0,
        }
    }

    // Applies the ops not replayed yet; `sees(at, d)` says whether the fact
    // of color `d` at op `at` holds here.
    fn replay(&mut self, th: &Theory, ops: &[Op], sees: impl Fn(usize, usize) -> bool) {
        let db = &mut self.db;
        for (at, op) in ops.iter().enumerate().skip(self.done) {
            match op {
                Op::Term(table, cells) => {
                    let key = row(db, &self.terms, cells);
                    self.terms.push(db.entry(*table, &key).unwrap());
                }
                Op::Fresh => self.terms.push(db.fresh(th.e)),
                &Op::Union(c, a, b) if sees(at, c) => db.union(self.terms[a], self.terms[b]),
                Op::UnionAll(unions) => {
                    for &(c, a, b) in unions {
                        if sees(at, c) {
                            db.union(self.terms[a], self.terms[b]);
                        }
                    }
                }
                Op::Insert(c, table, cells) if sees(at, *c) => {
                    db.insert(*table, &row(db, &self.terms, cells)).unwrap();
                }
                Op::Run => {
                    self.program.run(db, &Schedule::saturate("")).unwrap();
                }
                _ => {}
            }
        }
        self.done = ops.len();
    }
}

// One wrong colored database each, for the sensitivity test.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Bug {
    // A color sees only the base and its own facts.
    NoParents,
    // A color misses base facts that arrive after it is made.
    NoLateBase,
    // Dropping a color also forgets the facts of the colors below it.
    DropForgetsBelow,
    // Colors merge `ub` with max.
    MaxMerge,
    // The facts of the first color hold everywhere.
    Leak,
}

// The semantics: one replica per color, replaying the base and the facts of
// the colors at or below it. With a bug, a wrong colored database.
struct Clones<'t> {
    base: &'t Theory,
    // The theory of the replicas of colors; the base one but for MaxMerge.
    colored: &'t Theory,
    bug: Option<Bug>,
    ops: Vec<Op>,
    order: Order,
    forgotten: u64,
    replicas: Vec<Option<Replica>>,
}

impl<'t> Clones<'t> {
    fn new(base: &'t Theory, colored: &'t Theory, bug: Option<Bug>) -> Self {
        Self {
            base,
            colored,
            bug,
            ops: Vec::new(),
            order: Order::new(),
            forgotten: 0,
            replicas: vec![None],
        }
    }
}

impl Subject for Clones<'_> {
    fn apply(&mut self, at: usize, op: &Op) {
        self.order.apply(at, op);
        self.ops.push(op.clone());
        self.replicas.resize_with(self.order.below.len(), || None);
        if let (&Op::Drop(c), Some(Bug::DropForgetsBelow)) = (op, self.bug) {
            self.forgotten |= self.order.below[c] & !(1 | 1 << c);
            self.replicas.iter_mut().for_each(|r| *r = None);
        }
    }

    fn view(&mut self, c: usize) -> View {
        let (order, bug, forgotten) = (&self.order, self.bug, self.forgotten);
        let sees = |at: usize, d: usize| match bug {
            Some(Bug::NoParents) => d == 0 || d == c,
            Some(Bug::NoLateBase) if d == 0 && c != 0 => at < order.made_at[c],
            Some(Bug::Leak) if d == 1 => true,
            _ => order.sees(c, d) && forgotten >> d & 1 == 0,
        };
        let th = if c == 0 { self.base } else { self.colored };
        let replica = self.replicas[c].get_or_insert_with(|| Replica::new(th));
        replica.replay(th, &self.ops, sees);
        view(th, &replica.db, None, &replica.terms)
    }

    fn common(&mut self, cs: &[usize]) -> Result<Vec<usize>, String> {
        let views: Vec<View> = cs.iter().map(|&c| self.view(c)).collect();
        let n = views[0].classes.len();
        let keys: Vec<Vec<usize>> = (0..n)
            .map(|t| views.iter().map(|v| v.classes[t]).collect())
            .collect();
        Ok(labels(&keys))
    }
}

// The colored database under test.
struct Colored<'t> {
    th: &'t Theory,
    db: Database,
    program: Program,
    colors: Vec<Color>,
    terms: Vec<Value>,
}

impl<'t> Colored<'t> {
    fn new(th: &'t Theory) -> Self {
        Self {
            th,
            db: th.db.clone(),
            program: th.program.clone(),
            colors: vec![Color::BASE],
            terms: Vec::new(),
        }
    }
}

impl Subject for Colored<'_> {
    // Base facts alternate between the plain entry points and the `_in`
    // ones with `Color::BASE`.
    fn apply(&mut self, at: usize, op: &Op) {
        let (db, terms, plain) = (&mut self.db, &self.terms, at.is_multiple_of(2));
        match op {
            Op::Term(table, cells) => {
                let key = row(db, terms, cells);
                self.terms.push(db.entry(*table, &key).unwrap());
            }
            Op::Fresh => self.terms.push(db.fresh(self.th.e)),
            &Op::Union(0, a, b) if plain => db.union(terms[a], terms[b]),
            &Op::Union(c, a, b) => db.union_in(self.colors[c], terms[a], terms[b]),
            Op::UnionAll(unions) => {
                let cs: Vec<Color> = unions.iter().map(|u| self.colors[u.0]).collect();
                let a: Vec<Value> = unions.iter().map(|u| terms[u.1]).collect();
                let b: Vec<Value> = unions.iter().map(|u| terms[u.2]).collect();
                db.union_in_all(&cs, &a, &b);
            }
            Op::Insert(0, table, cells) if plain => {
                db.insert(*table, &row(db, terms, cells)).unwrap();
            }
            Op::Insert(c, table, cells) => {
                let row = row(db, terms, cells);
                db.insert_in(self.colors[*c], *table, &row).unwrap();
            }
            Op::Color(parents) => {
                let below: Vec<Color> = parents.iter().map(|&p| self.colors[p]).collect();
                let color = db.color(&below);
                self.colors.push(color);
            }
            Op::Colors(parents) => {
                let below: Vec<Color> = parents.iter().map(|&p| self.colors[p]).collect();
                let colors = db.colors(&below);
                self.colors.extend(colors);
            }
            &Op::Drop(c) => db.drop_color(self.colors[c]),
            Op::Run => {
                self.program.run(db, &Schedule::saturate("")).unwrap();
            }
        }
    }

    fn view(&mut self, c: usize) -> View {
        let color = (c != 0).then(|| self.colors[c]);
        view(self.th, &self.db, color, &self.terms)
    }

    fn common(&mut self, cs: &[usize]) -> Result<Vec<usize>, String> {
        let db = &self.db;
        let cs: Vec<Color> = cs.iter().map(|&c| self.colors[c]).collect();
        let pairs = db.common(&cs);
        if let Some((a, b, c)) = (pairs.iter())
            .flat_map(|&(a, b)| cs.iter().map(move |&c| (a, b, c)))
            .find(|&(a, b, c)| db.find_in(c, a) != db.find_in(c, b))
        {
            return Err(format!("common pair ({a:?}, {b:?}) differs in {c:?}"));
        }
        // The classes the pairs join over the base classes.
        let mut parent: HashMap<Value, Value> = HashMap::new();
        let root = |parent: &HashMap<Value, Value>, mut v: Value| {
            while let Some(&p) = parent.get(&v) {
                v = p;
            }
            v
        };
        for &(a, b) in &pairs {
            let (a, b) = (root(&parent, db.find(a)), root(&parent, db.find(b)));
            if a != b {
                parent.insert(a, b);
            }
        }
        let keys: Vec<Value> = (self.terms.iter())
            .map(|&v| root(&parent, db.find(v)))
            .collect();
        Ok(labels(&keys))
    }
}

// Replays the script of `seed` into the subject that `bug` names (the
// colored database for `None`) and the semantics, comparing every live color
// and a few `common`s after each run.
fn check(seed: u64, bug: Option<Bug>) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let mut rules: Vec<Rule> = (0..1 + rng.below(4)).map(|_| rewrite(&mut rng)).collect();
    rules.extend((0..1 + rng.below(3)).map(|_| rule(&mut rng)));
    let th = Theory::new(&rules, i64::min);
    let max = (bug == Some(Bug::MaxMerge)).then(|| Theory::new(&rules, i64::max));
    let ops = Gen {
        rng: &mut rng,
        th: &th,
        ops: Vec::new(),
        terms: 0,
        order: Order::new(),
    }
    .ops();
    let mut want = Clones::new(&th, &th, None);
    let mut got: Box<dyn Subject + '_> = match bug {
        None => Box::new(Colored::new(&th)),
        Some(_) => Box::new(Clones::new(&th, max.as_ref().unwrap_or(&th), bug)),
    };
    for (at, op) in ops.iter().enumerate() {
        want.apply(at, op);
        got.apply(at, op);
        if !matches!(op, Op::Run) {
            continue;
        }
        let live = want.order.live();
        for &c in &live {
            let (want, got) = (want.view(c), got.view(c));
            if want != got {
                return Err(format!(
                    "seed {seed}, op {at}, color {c}\nwant {want:?}\n got {got:?}"
                ));
            }
        }
        for _ in 0..4 {
            let cs = [rng.pick(&live), rng.pick(&live)];
            let (want, got) = (want.common(&cs)?, got.common(&cs)?);
            if want != got {
                return Err(format!(
                    "seed {seed}, op {at}, common {cs:?}\nwant {want:?}\n got {got:?}"
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn every_live_color_holds_the_egraph_of_the_facts_below_it() {
    for seed in 0..1000 {
        check(seed, None).unwrap_or_else(|e| panic!("{e}"));
    }
}

#[test]
fn the_check_catches_each_planted_bug() {
    for bug in [
        Bug::NoParents,
        Bug::NoLateBase,
        Bug::DropForgetsBelow,
        Bug::MaxMerge,
        Bug::Leak,
    ] {
        let caught = (0..200).any(|seed| check(seed, Some(bug)).is_err());
        assert!(caught, "{bug:?} passed 200 seeds");
    }
}

// Clones cannot see how much a color run matches: it must find only the
// matches that read a row of its color, never a base match again.
#[test]
fn a_color_run_repeats_no_base_match() {
    let mut db = Database::new();
    let math = db.add_sort("Math", SortKind::Eq);
    let [p, _, r] = ["p", "q", "r"].map(|name| db.add_relation(name, &[math]));
    let mut rules = Rules::new();
    rules.rule(&db, "", facts![(p x)], actions![(q x)]).unwrap();
    let mut program = rules.compile(&db).unwrap();
    program.set_report_level(ReportLevel::StageInfo);
    for _ in 0..50 {
        let x = db.fresh(math);
        db.insert(p, &[x]).unwrap();
    }
    let mut matches = |db: &mut Database| -> usize {
        let report = program.run(db, &Schedule::run("")).unwrap();
        report.num_matches_per_rule.values().sum()
    };
    assert_eq!(matches(&mut db), 50);
    assert_eq!(matches(&mut db), 0);

    let color = db.color(&[]);
    let unrelated = db.fresh(math);
    db.insert_in(color, r, &[unrelated]).unwrap();
    assert_eq!(matches(&mut db), 0);

    for _ in 0..2 {
        let x = db.fresh(math);
        db.insert_in(color, p, &[x]).unwrap();
    }
    assert_eq!(matches(&mut db), 2);
    assert_eq!(matches(&mut db), 0);
}
