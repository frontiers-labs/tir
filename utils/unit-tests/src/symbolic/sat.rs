use tir_symbolic::sat::{Lit, SatResult, Solver, Var};

use super::support::Rng;

/// Mint `n` fresh variables; `Var`s are plain indices, so they are valid in any
/// solver that has allocated at least as many.
fn vars(n: usize) -> Vec<Var> {
    let mut s = Solver::new();
    (0..n).map(|_| s.new_var()).collect()
}

fn lit_true(l: Lit, model: &[bool]) -> bool {
    model[l.var().index()] ^ l.is_negated()
}

fn satisfies(clauses: &[Vec<Lit>], model: &[bool]) -> bool {
    clauses
        .iter()
        .all(|c| c.iter().any(|&l| lit_true(l, model)))
}

/// Exhaustively decide satisfiability — the oracle for the random tests.
fn brute(n: usize, clauses: &[Vec<Lit>]) -> bool {
    (0..(1u64 << n)).any(|mask| {
        let model: Vec<bool> = (0..n).map(|i| (mask >> i) & 1 == 1).collect();
        satisfies(clauses, &model)
    })
}

fn solve_clauses(n: usize, clauses: &[Vec<Lit>]) -> SatResult {
    // `new_var` hands out Var(0), Var(1), ... matching the clause literals' indices.
    let mut s = Solver::new();
    for _ in 0..n {
        s.new_var();
    }
    for c in clauses {
        s.add_clause(c);
    }
    s.solve()
}

#[test]
fn lit_packing_roundtrips() {
    let v = vars(6)[5];
    let p = Lit::positive(v);
    let q = Lit::negative(v);
    assert_eq!(p.var(), v);
    assert!(!p.is_negated());
    assert!(q.is_negated());
    assert_eq!(p.negate(), q);
    assert_eq!(q.negate(), p);
}

#[test]
fn trivial_sat() {
    let mut s = Solver::new();
    let a = s.new_var();
    let b = s.new_var();
    s.add_clause(&[Lit::positive(a), Lit::positive(b)]);
    match s.solve() {
        SatResult::Sat(m) => assert!(m[a.index()] || m[b.index()]),
        other => panic!("expected sat, got {other:?}"),
    }
}

#[test]
fn trivial_unsat() {
    let mut s = Solver::new();
    let a = s.new_var();
    s.add_clause(&[Lit::positive(a)]);
    s.add_clause(&[Lit::negative(a)]);
    assert_eq!(s.solve(), SatResult::Unsat);
}

#[test]
fn empty_clause_is_unsat() {
    let mut s = Solver::new();
    s.new_var();
    s.add_clause(&[]);
    assert_eq!(s.solve(), SatResult::Unsat);
}

#[test]
fn unit_propagation_chain() {
    // a, ¬a∨b, ¬b∨c  ⇒  a,b,c all true.
    let mut s = Solver::new();
    let a = s.new_var();
    let b = s.new_var();
    let c = s.new_var();
    s.add_clause(&[Lit::positive(a)]);
    s.add_clause(&[Lit::negative(a), Lit::positive(b)]);
    s.add_clause(&[Lit::negative(b), Lit::positive(c)]);
    match s.solve() {
        SatResult::Sat(_) => {
            assert!(s.value(a));
            assert!(s.value(b));
            assert!(s.value(c));
        }
        other => panic!("expected sat, got {other:?}"),
    }
}

#[test]
fn pigeonhole_3_into_2_is_unsat() {
    let (pigeons, holes) = (3usize, 2usize);
    let mut s = Solver::new();
    let x: Vec<Vec<Var>> = (0..pigeons)
        .map(|_| (0..holes).map(|_| s.new_var()).collect())
        .collect();
    // Each pigeon occupies at least one hole.
    for row in &x {
        let clause: Vec<Lit> = row.iter().map(|&v| Lit::positive(v)).collect();
        s.add_clause(&clause);
    }
    // No hole holds two pigeons.
    #[allow(clippy::needless_range_loop)]
    for h in 0..holes {
        for i in 0..pigeons {
            for j in (i + 1)..pigeons {
                s.add_clause(&[Lit::negative(x[i][h]), Lit::negative(x[j][h])]);
            }
        }
    }
    assert_eq!(s.solve(), SatResult::Unsat);
}

#[test]
fn random_3sat_matches_brute_force() {
    let mut rng = Rng(0x1234_5678);
    let n = 6usize;
    let vars = vars(n);
    for _ in 0..400 {
        let m = 5 + rng.below(20) as usize;
        let mut clauses: Vec<Vec<Lit>> = Vec::with_capacity(m);
        for _ in 0..m {
            let mut c = Vec::with_capacity(3);
            while c.len() < 3 {
                let v = vars[rng.below(n as u64) as usize];
                let l = Lit::new(v, rng.below(2) == 1);
                if !c.contains(&l) && !c.contains(&l.negate()) {
                    c.push(l);
                }
            }
            clauses.push(c);
        }
        let expected = brute(n, &clauses);
        match solve_clauses(n, &clauses) {
            SatResult::Sat(model) => {
                assert!(expected, "solver said sat but instance is unsat");
                assert!(satisfies(&clauses, &model), "returned model is not a model");
            }
            SatResult::Unsat => assert!(!expected, "solver said unsat but instance is sat"),
            SatResult::Unknown => panic!("no budget was set; unknown is impossible"),
        }
    }
}
