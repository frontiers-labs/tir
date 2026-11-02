// The query planner of oatlog (thesis §3.3-3.4): typed rules become the
// rule trie of each ruleset, decided from the schema alone.

pub(crate) mod hir;
mod order;
pub(crate) mod permute;
mod seminaive;
mod trie;

use std::cell::RefCell;

use hir::{Rule, simplify};
use permute::{Symmetries, extract_symmetries};

use crate::ast::Literal;
use crate::vm::{
    ConstDecl, ExternDecl, IndexDecl, ProgramFile, RulesetDecl, SortClass, SortDecl, TableDecl,
    map_merge,
};
use crate::{Error, ExternId, Merge, Schema, SortId, SortKind, TableId, Value, sort};

// The rules of one ruleset, or of a part of one. A query keeps the rules
// that have no actions, so their matches can be counted; a combined
// ruleset has no rules and names its members; the rules of a `once` part
// read no table, so they match on the first run only; those of a `colored`
// one match in the colors only.
pub(crate) struct Source<'a> {
    pub(crate) name: &'a str,
    pub(crate) relaxed: bool,
    pub(crate) query: bool,
    pub(crate) once: bool,
    pub(crate) colored: bool,
    pub(crate) rules: Vec<&'a Rule>,
    pub(crate) members: &'a [String],
}

// The tables, primitives and indexes a program refers to, by their
// position in the program file.
#[derive(Default)]
pub(crate) struct Symbols {
    pub(crate) tables: Vec<TableId>,
    pub(crate) externs: Vec<ExternId>,
    pub(crate) indexes: Vec<IndexDecl>,
}

impl Symbols {
    pub(crate) fn table(&mut self, table: TableId) -> u32 {
        position(&mut self.tables, table)
    }

    pub(crate) fn prim(&mut self, prim: ExternId) -> u32 {
        position(&mut self.externs, prim)
    }

    pub(crate) fn index(&mut self, table: TableId, order: Vec<usize>) -> u32 {
        let table = self.table(table);
        position(&mut self.indexes, IndexDecl { table, order })
    }
}

fn position<T: PartialEq>(items: &mut Vec<T>, item: T) -> u32 {
    let at = items.iter().position(|x| *x == item).unwrap_or_else(|| {
        items.push(item);
        items.len() - 1
    });
    at as u32
}

// Plans every ruleset into one program file, and returns it with the
// database ids of its tables and primitives.
pub(crate) fn compile(
    sources: &[Source<'_>],
    schema: Schema<'_>,
) -> Result<(ProgramFile, Vec<TableId>, Vec<ExternId>), Error> {
    let mut syms = Symmetries::new(schema);
    let mut rulesets: Vec<Vec<Rule>> = Vec::new();
    for source in sources {
        let mut rules: Vec<Rule> = source.rules.iter().map(|&rule| rule.clone()).collect();
        for rule in &mut rules {
            simplify(rule, schema, &syms);
        }
        if source.relaxed {
            extract_symmetries(&mut rules, schema, &mut syms);
        }
        rulesets.push(rules);
    }
    // Every symmetry is known now; simplify again modulo all of them.
    for (source, rules) in sources.iter().zip(&mut rulesets) {
        for rule in rules.iter_mut() {
            simplify(rule, schema, &syms);
        }
        rules.retain(|rule| source.query || !rule.actions.is_empty());
    }
    let (mut symbols, mut nodes) = (Symbols::default(), Vec::new());
    let mut decls = Vec::new();
    for (source, rules) in sources.iter().zip(&rulesets) {
        let variants: Vec<_> = (rules.iter().enumerate())
            .flat_map(|(i, rule)| seminaive::variants(i, rule, &syms, schema))
            .collect();
        let mut consts: Vec<Literal> = Vec::new();
        let literals = rules.iter().flat_map(|rule| &rule.constants);
        for literal in literals.flatten() {
            position(&mut consts, literal.clone());
        }
        let unit = position(&mut consts, Literal::Unit);
        let states = (variants.iter().enumerate())
            .map(|(i, variant)| order::State::new(i, &rules[variant.rule], &consts))
            .collect();
        let mut trie = trie::Trie {
            schema,
            syms: &syms,
            rules,
            variants: &variants,
            symbols: &mut symbols,
            nodes: &mut nodes,
            unit,
        };
        let roots = trie.children(states, consts.len() as u32);
        decls.push(RulesetDecl {
            name: source.name.to_owned(),
            roots,
            rules: rules.iter().map(|rule| rule.name.clone()).collect(),
            consts,
            members: source.members.to_vec(),
            once: source.once,
            colored: source.colored,
        });
    }
    // A table keeps its symmetries even when no rule is left to read it.
    for (table, _) in schema.tables() {
        if !syms.of(table).is_empty() {
            symbols.table(table);
        }
    }
    let (sorts, consts, tables, externs) = declare(schema, &mut symbols, &syms)?;
    let file = ProgramFile {
        sorts,
        consts,
        tables,
        externs,
        indexes: symbols.indexes,
        nodes,
        rulesets: decls,
    };
    Ok((file, symbols.tables, symbols.externs))
}

// The constants of a merge whose result is of `sort`, in order, with their
// sorts.
fn merge_constants(
    merge: &Merge,
    sort: SortId,
    schema: Schema<'_>,
    out: &mut Vec<(Value, SortId)>,
) {
    let (args, sorts) = match merge {
        Merge::Const(value) => return out.push((*value, sort)),
        Merge::Prim(prim, args) => (args, &schema.primitive(*prim).1.inputs[..]),
        Merge::Function(table, args) => (args, schema.table(*table).columns()),
        _ => return,
    };
    for (arg, &sort) in args.iter().zip(sorts) {
        merge_constants(arg, sort, schema, out);
    }
}

// The declarations of the tables the code refers to, of the tables and
// primitives their merges refer to, and of the sorts of all of them.
fn declare(
    schema: Schema<'_>,
    symbols: &mut Symbols,
    syms: &Symmetries,
) -> Result<Declared, Error> {
    let (mut sorts, mut consts): (Vec<SortId>, Vec<ConstDecl>) = (Vec::new(), Vec::new());
    let mut constant =
        |value: Value, sort: SortId, sorts: &mut Vec<SortId>| -> Result<Value, Error> {
            let value = sort::constant(schema.db, sort, value)?;
            let sort = position(sorts, sort);
            Ok(Value::new(position(&mut consts, ConstDecl { sort, value })))
        };
    let mut tables = Vec::new();
    while tables.len() < symbols.tables.len() {
        let id = symbols.tables[tables.len()];
        let table = schema.table(id);
        let output = table.columns().last().copied();
        let mut found = Vec::new();
        if let (Some(merge), Some(output)) = (table.merge(), output) {
            merge_constants(merge, output, schema, &mut found);
        }
        let found: Vec<Value> = (found.into_iter())
            .map(|(value, sort)| constant(value, sort, &mut sorts))
            .collect::<Result<_, _>>()?;
        let merge = table.merge().map(|merge| {
            let symbols = RefCell::new(&mut *symbols);
            let mut table = |t| TableId::new(symbols.borrow_mut().table(t) as usize);
            let mut prim = |e| ExternId::new(symbols.borrow_mut().prim(e) as usize);
            let mut found = found.iter().copied();
            let mut konst = |_| found.next().expect("one index per constant");
            map_merge(merge, &mut table, &mut prim, &mut konst)
        });
        let default = match (table.default_value(), output) {
            (Some(value), Some(output)) => Some(constant(value, output, &mut sorts)?.raw()),
            _ => None,
        };
        let columns = table.columns().iter();
        tables.push(TableDecl {
            name: table.name().to_owned(),
            kind: table.kind(),
            columns: columns.map(|&sort| position(&mut sorts, sort)).collect(),
            keys: table.key_arity(),
            merge,
            shapes: table.shapes().to_vec(),
            symmetries: syms.of(id).to_vec(),
            cost: table.cost(),
            unextractable: table.is_unextractable(),
            default,
        });
    }
    let externs = (symbols.externs.iter())
        .map(|&prim| {
            let (name, signature) = schema.primitive(prim);
            let inputs = signature.inputs.iter();
            ExternDecl {
                name: name.to_owned(),
                inputs: inputs.map(|&sort| position(&mut sorts, sort)).collect(),
                output: position(&mut sorts, signature.output),
            }
        })
        .collect();
    let sorts = (sorts.iter())
        .map(|&sort| {
            let sort = schema.sort(sort);
            let class = match sort.kind() {
                SortKind::Eq => SortClass::Eq,
                SortKind::Base(_) => SortClass::Base,
                SortKind::Container(_) => SortClass::Container,
            };
            let name = sort.name().to_owned();
            SortDecl { name, class }
        })
        .collect();
    Ok((sorts, consts, tables, externs))
}

// The sorts, constants, tables and primitives of a program file.
type Declared = (
    Vec<SortDecl>,
    Vec<ConstDecl>,
    Vec<TableDecl>,
    Vec<ExternDecl>,
);
