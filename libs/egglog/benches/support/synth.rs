// A seeded theory shaped like the largest upstream egglog inputs (eggcc-2mm,
// gemma, taylor51, herbie; plan claim 5.3): 20 to 200 constructors over one
// Eq sort, rules of 2 to 4 facts with 2 to 9 table atoms, and about 0.4 i64
// primitive calls per rule. Each rule is cut from the initial terms, so it
// matches them: a fact is a term with its subterms below a depth turned into
// variables, and a later fact joins an earlier one on a shared subterm.

use std::collections::HashSet;

use tir_adt::FxBuildHasher;
use tir_egglog::ast::{Action, Fact};
use tir_egglog::prelude::*;
use tir_egglog::{Database, Rules, SortKind, Value};

const SEED: u64 = 0x6567_676c_6f67_0001;
// Table 0 is the Num constructor over i64, which the primitive rules read.
const NUM: usize = 0;

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }

    fn range(&mut self, lo: usize, hi: usize) -> usize {
        lo + self.below(hi - lo + 1)
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }
}

// Percent of tables by arity: 0 30%, 1 25%, 2 30%, 3 10%, 4 to 8 5%.
fn arity(rng: &mut Rng) -> usize {
    match rng.below(100) {
        0..30 => 0,
        30..55 => 1,
        55..85 => 2,
        85..95 => 3,
        _ => rng.range(4, 8),
    }
}

// The initial terms as a DAG: node `i` is `table(children)`, and the child of
// a Num node is its i64.
struct Terms {
    nodes: Vec<(usize, Vec<u32>)>,
    parents: Vec<Vec<u32>>,
}

impl Terms {
    fn new(rng: &mut Rng, arities: &[usize], size: usize) -> Self {
        let mut nodes: Vec<(usize, Vec<u32>)> = Vec::with_capacity(size);
        let mut seen: HashSet<_, FxBuildHasher> = HashSet::default();
        while nodes.len() < size {
            let table = rng.below(arities.len());
            let children: Vec<u32> = match table {
                NUM => vec![rng.below(64) as u32],
                _ if nodes.is_empty() && arities[table] > 0 => continue,
                _ => (0..arities[table])
                    .map(|_| rng.below(nodes.len()) as u32)
                    .collect(),
            };
            if seen.insert((table, children.clone())) {
                nodes.push((table, children));
            }
        }
        let mut parents = vec![Vec::new(); size];
        for (id, (table, children)) in nodes.iter().enumerate() {
            if *table != NUM {
                children
                    .iter()
                    .for_each(|&c| parents[c as usize].push(id as u32));
            }
        }
        Self { nodes, parents }
    }

    fn is_num(&self, node: u32) -> bool {
        self.nodes[node as usize].0 == NUM
    }

    // `node` as a pattern of at most `budget` more atoms, taken level by level
    // with Num children first. Unexpanded children and `keep` stay variables.
    fn pattern(&self, node: u32, budget: &mut usize, keep: u32, vars: &mut Vars) -> Expr {
        let (table, children) = &self.nodes[node as usize];
        if *table == NUM {
            vars.ints.push(node);
            return exprs::call(&format!("T{NUM}"), vec![exprs::var(&format!("n{node}"))]);
        }
        let mut expand = vec![false; children.len()];
        for num_first in [true, false] {
            for (at, &child) in children.iter().enumerate() {
                if self.is_num(child) == num_first && child != keep && *budget > 0 {
                    *budget -= 1;
                    expand[at] = true;
                }
            }
        }
        let args = children.iter().zip(expand).map(|(&child, expand)| {
            if expand {
                self.pattern(child, budget, keep, vars)
            } else {
                vars.ids.push(child);
                exprs::var(&format!("v{child}"))
            }
        });
        exprs::call(&format!("T{table}"), args.collect())
    }
}

// The variables of a rule, by the node each stands for: e-classes, and the
// i64 of Num nodes.
#[derive(Default)]
struct Vars {
    ids: Vec<u32>,
    ints: Vec<u32>,
}

// A rule cut from the terms: the facts, and a union of the first fact's
// root with a new term over the rule's variables.
fn rule(rng: &mut Rng, terms: &Terms, arities: &[usize]) -> (Facts, Actions) {
    // Some roots cut no Num atom, so slightly more than 40% of the rules
    // try for a primitive call.
    let prim = rng.below(100) < 45;
    let roots: Vec<u32> = (0..terms.nodes.len() as u32)
        .filter(|&n| {
            let (table, children) = &terms.nodes[n as usize];
            *table != NUM
                && !children.is_empty()
                && (!prim || children.iter().any(|&c| terms.is_num(c)))
        })
        .collect();
    let facts = rng.range(2, 4);
    let mut atoms = vec![1; facts];
    for _ in facts..rng.range(facts, 9) {
        atoms[rng.below(facts)] += 1;
    }
    if prim
        && atoms[0] == 1
        && let Some(at) = atoms.iter().position(|&a| a > 1)
    {
        atoms[at] -= 1;
        atoms[0] += 1;
    }
    let root = rng.pick(&roots);
    let mut vars = Vars::default();
    let mut body = Vec::new();
    let (mut top, mut keep) = (root, u32::MAX);
    let mut tops = Vec::new();
    for budget in atoms {
        let mut budget = budget - 1;
        let pattern = terms.pattern(top, &mut budget, keep, &mut vars);
        body.push(Fact::Eq(span!(), exprs::var(&format!("v{top}")), pattern));
        vars.ids.push(top);
        tops.push(top);
        // The next fact joins on a variable: through a parent of it, or as
        // the term under it.
        let parents =
            (vars.ids.iter()).flat_map(|&v| terms.parents[v as usize].iter().map(move |&p| (p, v)));
        let under = (vars.ids.iter())
            .filter(|&&v| !terms.nodes[v as usize].1.is_empty() && !terms.is_num(v));
        let next: Vec<(u32, u32)> = parents
            .chain(under.map(|&v| (v, u32::MAX)))
            .filter(|(top, _)| !tops.contains(top))
            .collect();
        if next.is_empty() {
            break;
        }
        (top, keep) = rng.pick(&next);
    }
    let tables: Vec<usize> = (1..arities.len()).filter(|&t| arities[t] > 0).collect();
    let table = rng.pick(&tables);
    let mut args: Vec<Expr> = (0..arities[table])
        .map(|_| exprs::var(&format!("v{}", rng.pick(&vars.ids))))
        .collect();
    if let Some(num) = vars.ints.first().filter(|_| prim) {
        let sum = exprs::call("+", vec![exprs::var(&format!("n{num}")), exprs::int(1)]);
        args[0] = exprs::call(&format!("T{NUM}"), vec![sum]);
    }
    let new = exprs::call(&format!("T{table}"), args);
    let union = Action::Union(span!(), exprs::var(&format!("v{root}")), new);
    (Facts(body), GenericActions(vec![union]))
}

// The theory with one rule per table, and `size` distinct initial terms.
pub fn synth(size: usize) -> (Database, Rules) {
    let mut rng = Rng(SEED);
    let tables = rng.range(20, 200);
    let arities: Vec<usize> = (0..tables)
        .map(|t| if t == NUM { 1 } else { arity(&mut rng) })
        .collect();
    // An EGraph registers the primitives of the base sorts.
    let mut db = EGraph::default().database().clone();
    let sort = db.add_sort("T", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    for (t, &arity) in arities.iter().enumerate() {
        let keys = if t == NUM {
            vec![int]
        } else {
            vec![sort; arity]
        };
        db.add_constructor(&format!("T{t}"), &keys, sort);
    }
    let terms = Terms::new(&mut Rng(SEED ^ size as u64), &arities, size);
    let mut values: Vec<Value> = Vec::with_capacity(size);
    for (table, children) in &terms.nodes {
        let args: Vec<Value> = match *table {
            NUM => vec![db.base(i64::from(children[0]))],
            _ => children.iter().map(|&c| values[c as usize]).collect(),
        };
        let table = db.table(&format!("T{table}")).unwrap();
        values.push(db.entry(table, &args).unwrap());
    }
    let mut rules = Rules::new();
    let mut added = 0;
    while added < tables {
        // A cut that found no second fact to join is dropped.
        let (facts, actions) = rule(&mut rng, &terms, &arities);
        if facts.0.len() >= 2 {
            rules.rule(&db, "", facts, actions).unwrap();
            added += 1;
        }
    }
    (db, rules)
}

// `tables` one-column constructors over one Eq sort whose `ids` e-classes come
// from a Leaf table. Each constructor holds `rows` keys spread over the whole
// id space, as the small tables of a large e-graph do, and a rule moves its
// terms to the next constructor.
pub fn sparse(tables: usize, ids: usize, rows: usize) -> (Database, Rules) {
    let mut rng = Rng(SEED);
    let mut db = EGraph::default().database().clone();
    let sort = db.add_sort("T", SortKind::Eq);
    let int = db.sort("i64").unwrap();
    let leaf = db.add_constructor("Leaf", &[int], sort);
    let leaves: Vec<Value> = (0..ids as i64)
        .map(|i| {
            let i = db.base(i);
            db.entry(leaf, &[i]).unwrap()
        })
        .collect();
    let mut rules = Rules::new();
    for t in 0..tables {
        let table = db.add_constructor(&format!("U{t}"), &[sort], sort);
        for _ in 0..rows {
            db.entry(table, &[rng.pick(&leaves)]).unwrap();
        }
    }
    for t in 0..tables {
        let lhs = exprs::call(&format!("U{t}"), vec![exprs::var("x")]);
        let rhs = exprs::call(&format!("U{}", (t + 1) % tables), vec![exprs::var("x")]);
        rules.rewrite(&db, "", lhs, rhs).unwrap();
    }
    (db, rules)
}
