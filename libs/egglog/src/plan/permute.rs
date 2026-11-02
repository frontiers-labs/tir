// Invariant column permutations of single tables (thesis §3.3.2). In a
// relaxed ruleset, a rule that only permutes the columns of one table, such
// as (rewrite (Add a b) (Add b a)), leaves the ruleset and becomes a symmetry
// of the table. Rebuild then writes every row in each symmetric order, so
// one index serves all of them, and atoms compare equal modulo the symmetry.

use super::hir::{Act, Rule, Target, Var};
use crate::core::close_group;
use crate::{Schema, TableId, TableKind};

// Per table, the column orders other than the identity that map its rows
// onto its rows.
pub(crate) struct Symmetries(Vec<Vec<Vec<usize>>>);

impl Symmetries {
    pub(crate) fn new(schema: Schema<'_>) -> Self {
        Self(
            schema
                .tables()
                .map(|(_, table)| table.symmetries().to_vec())
                .collect(),
        )
    }

    pub(crate) fn of(&self, table: TableId) -> &[Vec<usize>] {
        &self.0[table.index()]
    }

    // `args` in each column order the target allows, the given one first.
    pub(crate) fn forms(&self, target: Target, args: &[Var]) -> Vec<Vec<Var>> {
        let mut forms = vec![args.to_vec()];
        if let Target::Table(table) = target {
            let permute = |p: &Vec<usize>| p[..args.len()].iter().map(|&c| args[c]).collect();
            forms.extend(self.of(table).iter().map(permute));
        }
        forms
    }

    // Whether the first `keys` columns of `a` and of some order of `b` agree.
    pub(crate) fn same_key(&self, target: Target, a: &[Var], b: &[Var], keys: usize) -> bool {
        self.forms(target, b)
            .iter()
            .any(|form| form[..keys] == a[..keys])
    }

    // Whether `a` equals some order of `b`.
    pub(crate) fn same(&self, target: Target, a: &[Var], b: &[Var]) -> bool {
        a.len() == b.len() && self.same_key(target, a, b, a.len())
    }
}

// Moves each symmetry rule of a relaxed ruleset into `syms`.
pub(crate) fn extract_symmetries(rules: &mut Vec<Rule>, schema: Schema<'_>, syms: &mut Symmetries) {
    rules.retain(|rule| match permutation(rule, schema) {
        Some((table, order)) => {
            close_group(&mut syms.0[table.index()], &order);
            false
        }
        None => true,
    });
}

// The table and column order of a rule whose premise is one atom of distinct
// variables and whose only action writes the same variables in another
// order of the same table, keeping the output in place.
fn permutation(rule: &Rule, schema: Schema<'_>) -> Option<(TableId, Vec<usize>)> {
    let ([atom], [Act::Insert(table, args)]) = (&rule.premise[..], &rule.actions[..]) else {
        return None;
    };
    let constant = |v: &Var| rule.constants[*v as usize].is_some();
    if atom.target != Target::Table(*table)
        || !distinct(&atom.args)
        || atom.args.iter().any(constant)
    {
        return None;
    }
    let order: Option<Vec<usize>> = args
        .iter()
        .map(|v| atom.args.iter().position(|w| w == v))
        .collect();
    let order = order.filter(|order| distinct(order))?;
    let schema = schema.table(*table);
    let keys = schema.key_arity();
    let moves_output = schema.kind() != TableKind::Relation && order[keys] != keys;
    (!moves_output).then_some((*table, order))
}

fn distinct<T: PartialEq>(items: &[T]) -> bool {
    (0..items.len()).all(|i| !items[..i].contains(&items[i]))
}
