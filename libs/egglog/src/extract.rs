// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, src/extract.rs. See libs/egglog/LICENSE-MIT.

//! Extraction of the cheapest term of an e-class, over a [`Database`].
//!
//! A [`TreeCostModel`] gives each row a cost from those of its children; a
//! [`DagCostModel`] gives marginal costs that [`TreeCostModelFromDag`]
//! adds up. Constructors marked unextractable are skipped, and a `:cost`
//! replaces the cost of one row of its constructor in the default model. An
//! e-class of a sort that no constructor makes is a leaf, printed `Sort#id`.

use std::collections::{HashMap, HashSet};

use tir_adt::FxBuildHasher;

use crate::ast::Expr;
use crate::sort::{BigInt, BigRat};
use crate::{
    ArcSort, Color, CommandOutput, Database, EGraph, Enode, Error, SortId, Span, TableId,
    TableKind, TableSchema, TermDag, TermId, Value,
};

type Map<K, V> = HashMap<K, V, FxBuildHasher>;
// The key terms of rows, their output terms if asked for, and their dag.
type RowTerms = (Vec<TermId>, Option<Vec<TermId>>, TermDag);

/// A value that ranks extraction candidates.
pub trait Cost: Clone + Ord {}

impl<T: Clone + Ord> Cost for T {}

/// A cost with an identity and an associative, commutative, monotone
/// combination.
pub trait MonoidCost: Cost {
    fn identity() -> Self;
    fn combine(self, other: &Self) -> Self;
}

macro_rules! monoid_unsigned_cost {
    ($($cost:ty),*) => {$(
        impl MonoidCost for $cost {
            fn identity() -> Self {
                0
            }

            fn combine(self, other: &Self) -> Self {
                self.saturating_add(*other)
            }
        }
    )*};
}

monoid_unsigned_cost!(u8, u16, u32, u64, u128, usize);

impl MonoidCost for BigInt {
    fn identity() -> Self {
        Self::new(0)
    }

    fn combine(self, other: &Self) -> Self {
        &self + other
    }
}

impl MonoidCost for BigRat {
    fn identity() -> Self {
        Self::new(BigInt::new(0), BigInt::new(1)).expect("a nonzero denominator")
    }

    fn combine(self, other: &Self) -> Self {
        &self + other
    }
}

/// Marginal costs, for extraction that charges shared children once. The
/// costs leave out those of the children.
pub trait DagCostModel<C: MonoidCost> {
    fn base_value_cost(&self, db: &Database, sort: &ArcSort, value: Value) -> C;
    fn enode_cost(&self, db: &Database, func: &TableSchema, enode: &Enode<'_>) -> C;

    fn container_cost(&self, db: &Database, sort: &ArcSort, value: Value) -> C {
        let _ = (db, sort, value);
        C::identity()
    }
}

/// The cost of a tree from a note on its root and the costs of its children.
/// It must be deterministic, and a cheaper child must never make its parent
/// dearer.
pub trait TreeCostModel<C: Cost> {
    type EnodeCost;
    type ContainerCost;

    fn base_value_cost(&self, db: &Database, sort: &ArcSort, value: Value) -> C;
    fn enode_cost(&self, db: &Database, func: &TableSchema, enode: &Enode<'_>) -> Self::EnodeCost;
    fn container_cost(&self, db: &Database, sort: &ArcSort, value: Value) -> Self::ContainerCost;
    fn fold_enode_cost(&self, enode_cost: Self::EnodeCost, child_costs: &[C]) -> C;
    fn fold_container_cost(&self, container_cost: Self::ContainerCost, element_costs: &[C]) -> C;
}

// A tree cost model with its two stages fused.
trait TotalCost<C> {
    fn base(&self, db: &Database, sort: &ArcSort, value: Value) -> C;
    fn enode(&self, db: &Database, func: &TableSchema, enode: &Enode<'_>, children: &[C]) -> C;
    fn container(&self, db: &Database, sort: &ArcSort, value: Value, elements: &[C]) -> C;
}

impl<C: Cost, M: TreeCostModel<C>> TotalCost<C> for M {
    fn base(&self, db: &Database, sort: &ArcSort, value: Value) -> C {
        self.base_value_cost(db, sort, value)
    }

    fn enode(&self, db: &Database, func: &TableSchema, enode: &Enode<'_>, children: &[C]) -> C {
        self.fold_enode_cost(self.enode_cost(db, func, enode), children)
    }

    fn container(&self, db: &Database, sort: &ArcSort, value: Value, elements: &[C]) -> C {
        self.fold_container_cost(self.container_cost(db, sort, value), elements)
    }
}

/// A [`DagCostModel`] as a tree model: each cost combined with its children's.
#[derive(Clone, Debug)]
pub struct TreeCostModelFromDag<M>(pub M);

impl<C: MonoidCost, M: DagCostModel<C>> TreeCostModel<C> for TreeCostModelFromDag<M> {
    type EnodeCost = C;
    type ContainerCost = C;

    fn base_value_cost(&self, db: &Database, sort: &ArcSort, value: Value) -> C {
        self.0.base_value_cost(db, sort, value)
    }

    fn enode_cost(&self, db: &Database, func: &TableSchema, enode: &Enode<'_>) -> C {
        self.0.enode_cost(db, func, enode)
    }

    fn container_cost(&self, db: &Database, sort: &ArcSort, value: Value) -> C {
        self.0.container_cost(db, sort, value)
    }

    fn fold_enode_cost(&self, enode_cost: C, child_costs: &[C]) -> C {
        child_costs
            .iter()
            .fold(enode_cost, |cost, child| cost.combine(child))
    }

    fn fold_container_cost(&self, container_cost: C, element_costs: &[C]) -> C {
        element_costs
            .iter()
            .fold(container_cost, |cost, element| cost.combine(element))
    }
}

pub type DefaultCost = u64;

/// Each row and value costs `node_cost`, or a constructor's `:cost`.
#[derive(Clone, Debug)]
pub struct AdditiveCostModel {
    pub node_cost: DefaultCost,
}

impl Default for AdditiveCostModel {
    fn default() -> Self {
        Self { node_cost: 1 }
    }
}

pub const DEFAULT_COST_MODEL: TreeCostModelFromDag<AdditiveCostModel> =
    TreeCostModelFromDag(AdditiveCostModel { node_cost: 1 });

impl DagCostModel<DefaultCost> for AdditiveCostModel {
    fn base_value_cost(&self, _: &Database, _: &ArcSort, _: Value) -> DefaultCost {
        self.node_cost
    }

    fn enode_cost(&self, _: &Database, func: &TableSchema, _: &Enode<'_>) -> DefaultCost {
        func.cost().unwrap_or(self.node_cost)
    }
}

/// One extracted root or variant: its cost and its term in the termdag of
/// the result.
#[derive(Clone, Debug)]
pub struct ExtractedTerm<C> {
    pub cost: C,
    pub term: TermId,
}

/// The best term of each root, `None` for a root with no term.
#[derive(Clone, Debug)]
pub struct ExtractedTerms<C> {
    pub termdag: TermDag,
    pub terms: Vec<Option<ExtractedTerm<C>>>,
}

/// The variants of each root.
#[derive(Clone, Debug)]
pub struct ExtractedTermVariants<C> {
    pub termdag: TermDag,
    pub variants: Vec<Vec<ExtractedTerm<C>>>,
}

/// The cheapest term of each e-class of a database (Bellman-Ford over the
/// rows of the extractable constructors). Borrowing the database keeps the
/// costs from outliving a change to it.
pub struct TreeExtractor<'g, C: Cost> {
    db: &'g Database,
    // The color whose e-classes and rows it reads.
    color: Color,
    model: Box<dyn TotalCost<C> + 'g>,
    tables: Vec<TableId>,
    // The Eq sorts some constructor makes; e-classes of others are leaves.
    made: HashSet<SortId, FxBuildHasher>,
    costs: Map<Value, C>,
    rank: Map<Value, usize>,
    // The row each e-class extracts to.
    parent: Map<Value, (TableId, Vec<Value>)>,
}

/// An extractor with the default cost model.
pub type Extractor<'g> = TreeExtractor<'g, DefaultCost>;

impl<'g> TreeExtractor<'g, DefaultCost> {
    pub fn new(db: &'g Database) -> Self {
        Self::new_in(db, Color::BASE)
    }

    /// The cheapest terms of the e-classes of `color`, from its rows.
    pub fn new_in(db: &'g Database, color: Color) -> Self {
        Self::build(db, color, DEFAULT_COST_MODEL)
    }

    /// The best term of the e-class `value`.
    pub fn best(&self, value: Value) -> Result<(TermDag, TermId), Error> {
        let mut termdag = TermDag::default();
        let class = self.db.find_in(self.color, value);
        let missing = || Error::ExtractError(format!("no term for {value:?}"));
        let (table, _) = self.parent.get(&class).ok_or_else(missing)?;
        let sort = self
            .db
            .schema()
            .table(*table)
            .columns()
            .last()
            .copied()
            .expect("an output");
        let term = self.term(&mut termdag, class, sort, &mut Map::default());
        Ok((termdag, term))
    }
}

impl<'g, C: Cost> TreeExtractor<'g, C> {
    pub fn with_cost_model(db: &'g Database, model: impl TreeCostModel<C> + 'g) -> Self {
        Self::build(db, Color::BASE, model)
    }

    fn build(db: &'g Database, color: Color, model: impl TreeCostModel<C> + 'g) -> Self {
        let schema = db.schema();
        let constructors = schema
            .tables()
            .filter(|(_, t)| t.kind() == TableKind::Constructor);
        let made = constructors
            .map(|(_, t)| *t.columns().last().expect("an output"))
            .collect();
        let tables = (schema.tables())
            .filter(|(_, t)| {
                t.kind() == TableKind::Constructor
                    && !t.is_unextractable()
                    && !t.name().starts_with('@')
            })
            .map(|(id, _)| id)
            .collect();
        let mut extractor = Self {
            db,
            color,
            model: Box::new(model),
            tables,
            made,
            costs: Map::default(),
            rank: Map::default(),
            parent: Map::default(),
        };
        extractor.bellman_ford();
        extractor
    }

    /// Like [`TreeExtractor::with_cost_model`]; the costs cover every sort,
    /// so `rootsorts` only names the ones that will be asked for.
    pub fn compute_costs_from_rootsorts(
        rootsorts: Option<Vec<ArcSort>>,
        db: &'g Database,
        model: impl TreeCostModel<C> + 'g,
    ) -> Self {
        let _ = rootsorts;
        Self::with_cost_model(db, model)
    }

    fn sort(&self, id: SortId) -> &'g ArcSort {
        self.db.schema().sort(id).arcsort()
    }

    // The cost of `value` of `sort`; `None` if it has no term yet.
    fn cost(&self, value: Value, sort: &ArcSort) -> Option<C> {
        if sort.is_container_sort() {
            let elements = sort.inner_values(self.db.container_values(), value);
            let costs: Option<Vec<C>> = elements.iter().map(|(s, v)| self.cost(*v, s)).collect();
            Some(self.model.container(self.db, sort, value, &costs?))
        } else if sort.is_eq_sort() && self.made.contains(&self.db.sort(sort.name()).ok()?) {
            self.costs.get(&self.db.find_in(self.color, value)).cloned()
        } else {
            Some(self.model.base(self.db, sort, value))
        }
    }

    // The cost of a row, from the costs of its children.
    fn row_cost(&self, table: TableId, row: &[Value], subsumed: bool) -> Option<C> {
        let schema = self.db.schema().table(table);
        let keys = schema.key_arity();
        let children: Option<Vec<C>> = (row[..keys].iter().zip(schema.columns()))
            .map(|(&value, &sort)| self.cost(value, self.sort(sort)))
            .collect();
        let enode = Enode {
            name: schema.name(),
            children: &row[..keys],
            eclass: row[keys],
            subsumed,
        };
        Some(self.model.enode(self.db, schema, &enode, &children?))
    }

    // The highest rank of an e-class in `value` of `sort`.
    fn rank_of(&self, value: Value, sort: &ArcSort) -> usize {
        if sort.is_container_sort() {
            let inner = sort.inner_values(self.db.container_values(), value);
            inner
                .iter()
                .map(|(s, v)| self.rank_of(*v, s))
                .max()
                .unwrap_or(0)
        } else if sort.is_eq_sort() {
            self.rank
                .get(&self.db.find_in(self.color, value))
                .copied()
                .unwrap_or(0)
        } else {
            0
        }
    }

    fn row_rank(&self, table: TableId, row: &[Value]) -> usize {
        let schema = self.db.schema().table(table);
        let children = row[..schema.key_arity()].iter().zip(schema.columns());
        children
            .map(|(&value, &sort)| self.rank_of(value, self.sort(sort)))
            .max()
            .unwrap_or(0)
    }

    // Each e-class gets the cheapest row, and a rank from the order costs
    // improved in, so a row is only chosen above children of lower rank and
    // a cycle of equal costs cannot be chosen.
    fn bellman_ford(&mut self) {
        let (mut row, mut updates) = (Vec::new(), 0);
        let mut changed = true;
        while changed {
            changed = false;
            for &table in &self.tables {
                for found in self
                    .db
                    .rows_in(self.color, table)
                    .filter(|r| !r.is_subsumed())
                {
                    row.clear();
                    row.extend(found.values());
                    let (eclass, Some(cost)) = (
                        *row.last().expect("an output"),
                        self.row_cost(table, &row, false),
                    ) else {
                        continue;
                    };
                    if self.costs.get(&eclass).is_none_or(|old| cost < *old) {
                        self.costs.insert(eclass, cost);
                        updates += 1;
                        self.rank.insert(eclass, updates);
                        changed = true;
                    }
                }
            }
        }
        for &table in &self.tables {
            for found in self
                .db
                .rows_in(self.color, table)
                .filter(|r| !r.is_subsumed())
            {
                let row: Vec<Value> = found.values().collect();
                let eclass = *row.last().expect("an output");
                let best = self.costs.get(&eclass) == self.row_cost(table, &row, false).as_ref();
                if best
                    && !self.parent.contains_key(&eclass)
                    && self.rank[&eclass] > self.row_rank(table, &row)
                {
                    self.parent.insert(eclass, (table, row));
                }
            }
        }
    }

    // The term of `value` of `sort`, which must have a cost.
    fn term(
        &self,
        termdag: &mut TermDag,
        value: Value,
        sort: SortId,
        cache: &mut Map<(Value, SortId), TermId>,
    ) -> TermId {
        if let Some(&term) = cache.get(&(value, sort)) {
            return term;
        }
        let arcsort = self.sort(sort);
        let term = if arcsort.is_container_sort() {
            let containers = self.db.container_values();
            let elements = arcsort.inner_values(containers, value);
            let terms = (elements.iter())
                .map(|(s, v)| {
                    self.term(termdag, *v, self.db.sort(s.name()).expect("a sort"), cache)
                })
                .collect();
            arcsort.reconstruct_termdag_container(containers, value, termdag, terms)
        } else if let Some((table, row)) = self
            .parent
            .get(&self.db.find_in(self.color, value))
            .filter(|_| arcsort.is_eq_sort())
        {
            let schema = self.db.schema().table(*table);
            let children = row.iter().zip(schema.columns()).take(schema.key_arity());
            let children = children
                .map(|(&v, &s)| self.term(termdag, v, s, cache))
                .collect();
            termdag.app(schema.name().to_owned(), children)
        } else {
            arcsort.reconstruct_termdag_base(self.db.base_values(), value, termdag)
        };
        cache.insert((value, sort), term);
        term
    }

    /// The best term of `value` of `sort`, if it has one.
    pub fn extract_best_with_sort(
        &self,
        termdag: &mut TermDag,
        value: Value,
        sort: ArcSort,
    ) -> Option<ExtractedTerm<C>> {
        self.best_cached(termdag, &mut Map::default(), value, &sort)
    }

    fn best_cached(
        &self,
        termdag: &mut TermDag,
        cache: &mut Map<(Value, SortId), TermId>,
        value: Value,
        sort: &ArcSort,
    ) -> Option<ExtractedTerm<C>> {
        let value = if sort.is_eq_sort() {
            self.db.find_in(self.color, value)
        } else {
            value
        };
        let cost = self.cost(value, sort)?;
        let term = self.term(termdag, value, self.db.sort(sort.name()).ok()?, cache);
        Some(ExtractedTerm { cost, term })
    }

    /// Up to `nvariants` terms of `value`: its cheapest rows, each with the
    /// best terms of its children. Of a value that is no e-class, its best.
    pub fn extract_variants_with_sort(
        &self,
        termdag: &mut TermDag,
        value: Value,
        nvariants: usize,
        sort: ArcSort,
    ) -> Vec<ExtractedTerm<C>> {
        if nvariants == 0 {
            return Vec::new();
        }
        if !sort.is_eq_sort() {
            return self
                .extract_best_with_sort(termdag, value, sort)
                .into_iter()
                .collect();
        }
        let class = self.db.find_in(self.color, value);
        let mut roots: Vec<(C, String, Vec<Value>, TableId)> = Vec::new();
        for &table in &self.tables {
            for found in self
                .db
                .rows_in(self.color, table)
                .filter(|r| !r.is_subsumed())
            {
                let row: Vec<Value> = found.values().collect();
                if row.last() == Some(&class)
                    && let Some(cost) = self.row_cost(table, &row, false)
                {
                    roots.push((
                        cost,
                        self.db.schema().table(table).name().to_owned(),
                        row,
                        table,
                    ));
                }
            }
        }
        roots.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
        roots.truncate(nvariants);
        let mut cache = Map::default();
        let mut variant = |(cost, name, row, table): (C, String, Vec<Value>, TableId),
                           termdag: &mut TermDag| {
            let schema = self.db.schema().table(table);
            let children = row.iter().zip(schema.columns()).take(schema.key_arity());
            let children = children
                .map(|(&v, &s)| self.term(termdag, v, s, &mut cache))
                .collect();
            ExtractedTerm {
                cost,
                term: termdag.app(name, children),
            }
        };
        roots
            .into_iter()
            .map(|root| variant(root, termdag))
            .collect()
    }
}

impl EGraph {
    pub fn extract_best(
        &self,
        roots: Vec<(ArcSort, Value)>,
    ) -> Result<ExtractedTerms<DefaultCost>, Error> {
        self.extract_best_with_cost_model(roots, DEFAULT_COST_MODEL)
    }

    pub fn extract_best_with_cost_model<C: Cost, M: TreeCostModel<C>>(
        &self,
        roots: Vec<(ArcSort, Value)>,
        cost_model: M,
    ) -> Result<ExtractedTerms<C>, Error> {
        let extractor = TreeExtractor::with_cost_model(&self.db, cost_model);
        let (mut termdag, mut cache) = (TermDag::default(), Map::default());
        let terms = (roots.into_iter())
            .map(|(sort, value)| extractor.best_cached(&mut termdag, &mut cache, value, &sort))
            .collect();
        Ok(ExtractedTerms { termdag, terms })
    }

    pub fn extract_variants(
        &self,
        roots: Vec<(ArcSort, Value)>,
        nvariants: usize,
    ) -> Result<ExtractedTermVariants<DefaultCost>, Error> {
        self.extract_variants_with_cost_model(roots, nvariants, DEFAULT_COST_MODEL)
    }

    pub fn extract_variants_with_cost_model<C: Cost, M: TreeCostModel<C>>(
        &self,
        roots: Vec<(ArcSort, Value)>,
        nvariants: usize,
        cost_model: M,
    ) -> Result<ExtractedTermVariants<C>, Error> {
        let extractor = TreeExtractor::with_cost_model(&self.db, cost_model);
        let mut termdag = TermDag::default();
        let variants = (roots.into_iter())
            .map(|(sort, value)| {
                extractor.extract_variants_with_sort(&mut termdag, value, nvariants, sort)
            })
            .collect();
        Ok(ExtractedTermVariants { termdag, variants })
    }

    /// The best term of one value under the default cost model.
    pub fn extract_value(
        &self,
        sort: &ArcSort,
        value: Value,
    ) -> Result<(TermDag, TermId, DefaultCost), Error> {
        let mut extracted = self.extract_best(vec![(sort.clone(), value)])?;
        let missing = || Error::ExtractError(format!("no term of sort {}", sort.name()));
        let root = extracted.terms.pop().flatten().ok_or_else(missing)?;
        Ok((extracted.termdag, root.term, root.cost))
    }

    /// Terms of up to `n` rows of a table, subsumed ones included, and of
    /// their outputs if asked.
    pub fn function_to_dag(
        &self,
        sym: &str,
        n: usize,
        include_output: bool,
    ) -> Result<RowTerms, Error> {
        let table = self.db.table(sym)?;
        let schema = self.db.schema().table(table);
        let extractor = Extractor::new(&self.db);
        let (mut termdag, mut inputs, mut outputs) = (TermDag::default(), Vec::new(), Vec::new());
        let term = |termdag: &mut TermDag, value: Value, sort: SortId| {
            let sort = self.db.schema().sort(sort).arcsort().clone();
            let found = extractor.extract_best_with_sort(termdag, value, sort);
            found.map_or_else(
                || termdag.var("Unextractable".to_owned()),
                |found| found.term,
            )
        };
        for row in self.db.rows(table).take(n) {
            let row: Vec<Value> = row.values().collect();
            let keys = row.iter().zip(schema.columns()).take(schema.key_arity());
            let children = keys
                .map(|(&value, &sort)| term(&mut termdag, value, sort))
                .collect();
            inputs.push(termdag.app(sym.to_owned(), children));
            outputs.push(match row.get(schema.key_arity()) {
                Some(&value) => term(&mut termdag, value, schema.columns()[schema.key_arity()]),
                None => termdag.lit(crate::ast::Literal::Unit),
            });
        }
        Ok((inputs, include_output.then_some(outputs), termdag))
    }

    pub(crate) fn extract_command(
        &mut self,
        _span: Span,
        expr: &Expr,
        variants: &Expr,
    ) -> Result<Vec<CommandOutput>, Error> {
        let (sort, value) = self.eval_expr(expr)?;
        let (_, n) = self.eval_expr(variants)?;
        let n: i64 = self.value_to_base(n);
        if n == 0 {
            let (termdag, term, cost) = self.extract_value(&sort, value)?;
            return Ok(vec![CommandOutput::ExtractBest(termdag, cost, term)]);
        }
        let n = usize::try_from(n)
            .map_err(|_| Error::ExtractError("a negative number of variants".to_owned()))?;
        let mut extracted = self.extract_variants(vec![(sort, value)], n)?;
        let terms = extracted
            .variants
            .remove(0)
            .into_iter()
            .map(|variant| variant.term)
            .collect();
        Ok(vec![CommandOutput::ExtractVariants(
            extracted.termdag,
            terms,
        )])
    }
}
