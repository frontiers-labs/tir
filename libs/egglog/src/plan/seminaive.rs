// Semi-naive variants (thesis §2.3.3, §3.3.3) and domination (§3.3.4).

use super::hir::{Act, Rule, Target, Var};
use super::permute::Symmetries;
use crate::Schema;
use crate::vm::code::Slice;

// One way to match a rule: the slice each premise atom reads.
pub(crate) struct Variant {
    pub(crate) rule: usize,
    pub(crate) slices: Vec<Slice>,
}

// The variant that reads every row of every atom.
pub(crate) fn naive(index: usize, rule: &Rule) -> Variant {
    Variant {
        rule: index,
        slices: vec![Slice::All; rule.premise.len()],
    }
}

// Variant i: the table atoms before i read Old, atom i reads New, the atoms
// after i read All. A rule naming its delta tables has variants for those
// atoms only; the others read All. Old is All with `ts < last_run`, so a table keeps
// indexes on All only. A naive rule has one variant that reads All.
pub(crate) fn variants(
    index: usize,
    rule: &Rule,
    syms: &Symmetries,
    schema: Schema<'_>,
) -> Vec<Variant> {
    let tables: Vec<usize> = (0..rule.premise.len())
        .filter(|&a| match rule.premise[a].target {
            Target::Table(table) => {
                (rule.deltas.as_ref()).is_none_or(|deltas| deltas.contains(&table))
            }
            Target::Prim(_) => false,
        })
        .collect();
    let all = vec![Slice::All; rule.premise.len()];
    if rule.naive || tables.is_empty() {
        return vec![Variant {
            rule: index,
            slices: all,
        }];
    }
    let mut variants: Vec<Vec<Slice>> = Vec::new();
    for (i, &new) in tables.iter().enumerate() {
        let mut slices = all.clone();
        tables[..i].iter().for_each(|&old| slices[old] = Slice::Old);
        slices[new] = Slice::New;
        variants.push(slices);
    }
    // Drop each variant whose matches a kept variant also finds.
    let duplicate_safe = rule.actions.iter().all(|act| act.duplicate_safe(schema));
    let mut kept = vec![true; variants.len()];
    for b in 0..variants.len() {
        kept[b] = !(0..variants.len()).any(|a| {
            duplicate_safe && a != b && kept[a] && dominates(rule, &variants[a], &variants[b], syms)
        });
    }
    let kept = variants.into_iter().zip(kept).filter(|(_, kept)| *kept);
    kept.map(|(slices, _)| Variant {
        rule: index,
        slices,
    })
    .collect()
}

// Whether a renaming of the variables sends each atom of variant `a` to an
// atom of `b` that reads no row `a` would not, and the actions of `a` to
// those of `b`: then every match of `b` is a match of `a` with the same
// effect.
fn dominates(rule: &Rule, a: &[Slice], b: &[Slice], syms: &Symmetries) -> bool {
    let mut map: Vec<Option<Var>> = (0..rule.constants.len() as Var)
        .map(|v| rule.constant(v).then_some(v))
        .collect();
    let mut used = vec![false; rule.premise.len()];
    assign(rule, a, b, syms, 0, &mut map, &mut used)
}

fn assign(
    rule: &Rule,
    a: &[Slice],
    b: &[Slice],
    syms: &Symmetries,
    k: usize,
    map: &mut Vec<Option<Var>>,
    used: &mut [bool],
) -> bool {
    let Some(atom) = rule.premise.get(k) else {
        return actions_match(rule, map, syms);
    };
    for (j, other) in rule.premise.iter().enumerate() {
        if used[j] || other.target != atom.target || !(a[k] == Slice::All || a[k] == b[j]) {
            continue;
        }
        for form in syms.forms(other.target, &other.args) {
            let saved = map.clone();
            if extend(map, &atom.args, &form) {
                used[j] = true;
                if assign(rule, a, b, syms, k + 1, map, used) {
                    return true;
                }
                used[j] = false;
            }
            *map = saved;
        }
    }
    false
}

fn extend(map: &mut [Option<Var>], from: &[Var], to: &[Var]) -> bool {
    from.iter().zip(to).all(|(&f, &t)| {
        let image = map[f as usize].get_or_insert(t);
        *image == t
    })
}

fn actions_match(rule: &Rule, map: &[Option<Var>], syms: &Symmetries) -> bool {
    let rename = |v: &Var| map[*v as usize].unwrap_or(*v);
    let renamed = |args: &[Var]| -> Vec<Var> { args.iter().map(rename).collect() };
    let absent = (rule.absent.iter()).all(|atom| renamed(&atom.args) == atom.args);
    absent
        && rule.actions.iter().all(|act| match act {
            Act::Insert(table, args)
            | Act::Lookup(table, args)
            | Act::Delete(table, args)
            | Act::Subsume(table, args) => syms.same(Target::Table(*table), &renamed(args), args),
            Act::Prim(_, args) | Act::Call(_, args) | Act::FindOrMake(_, args) => {
                renamed(args) == *args
            }
            Act::Union(x, y) => {
                let (rx, ry) = (rename(x), rename(y));
                (rx, ry) == (*x, *y) || (rx, ry) == (*y, *x)
            }
            Act::Panic(_) => true,
        })
}
