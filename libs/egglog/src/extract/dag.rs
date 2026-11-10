// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog-experimental 61e68719559145842b03c7556ddc0687cb00f162,
// src/greedy_dag_extract.rs. See libs/egglog/LICENSE-MIT.

use std::collections::{HashSet, VecDeque, hash_map::Entry};
use std::sync::Arc;

use tir_adt::FxBuildHasher;

use super::{DagCostModel, ExtractedTerm, Map, MonoidCost};
use crate::{
    ArcSort, Color, Database, Enode, Error, SortId, TableId, TableKind, TermDag, TermId, Value,
};

type Set<T> = HashSet<T, FxBuildHasher>;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    sort: SortId,
    value: Value,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ProducerId(usize);

struct Producer<C> {
    table: TableId,
    target: Key,
    children: Vec<Key>,
    cost: C,
}

struct Structural<C> {
    children: Vec<Key>,
    cost: C,
}

// Both maps describe the same closed, acyclic witness. Later improvements
// to a child cannot change an already selected parent's producer choices.
struct Candidate<C> {
    paid: Map<Key, C>,
    plan: Map<Key, ProducerId>,
    cost: C,
}

impl<C: MonoidCost> Candidate<C> {
    fn new(paid: Map<Key, C>, plan: Map<Key, ProducerId>) -> Self {
        let cost = paid
            .values()
            .fold(C::identity(), |cost, own| cost.combine(own));
        Self { paid, plan, cost }
    }
}

/// Sharing-aware greedy extraction with fixed marginal costs.
///
/// Preparation freezes costs for constructors and structural values reachable
/// from `roots`. Rebuild the extractor after changing the database or model.
/// Each root selects an independent, closed producer plan. For one shared
/// objective over several outputs, supply one zero-cost composite root.
///
/// This heuristic retains one candidate per e-class and only replaces it on
/// a strict cost improvement. It does not promise a minimum-cost DAG.
pub struct GreedyDagExtractor<'g, C: MonoidCost> {
    db: &'g Database,
    color: Color,
    made: Set<SortId>,
    producers: Vec<Producer<C>>,
    structural: Map<Key, Structural<C>>,
    dependencies: Map<Key, Vec<ProducerId>>,
    best: Map<Key, Arc<Candidate<C>>>,
}

impl<'g, C: MonoidCost> GreedyDagExtractor<'g, C> {
    /// Prepare the values reachable from `roots` in the base color.
    pub fn with_cost_model(
        db: &'g Database,
        roots: &[(ArcSort, Value)],
        model: impl DagCostModel<C>,
    ) -> Result<Self, Error> {
        Self::with_cost_model_in(db, Color::BASE, roots, model)
    }

    /// Prepare reachable values using the rows and equalities of `color`.
    /// Each reachable producer or distinct structural value is costed once.
    pub fn with_cost_model_in(
        db: &'g Database,
        color: Color,
        roots: &[(ArcSort, Value)],
        model: impl DagCostModel<C>,
    ) -> Result<Self, Error> {
        let made = db
            .schema()
            .tables()
            .filter(|(_, t)| t.kind() == TableKind::Constructor)
            .map(|(_, t)| t.columns()[t.key_arity()])
            .collect();
        let mut this = Self {
            db,
            color,
            made,
            producers: Vec::new(),
            structural: Map::default(),
            dependencies: Map::default(),
            best: Map::default(),
        };
        let mut rows: Map<Key, Vec<(TableId, Vec<Value>)>> = Map::default();
        let mut indexed = Set::default();
        let mut pending = roots
            .iter()
            .map(|(sort, value)| Ok(this.key(db.sort(sort.name())?, *value)))
            .collect::<Result<Vec<_>, Error>>()?;
        let mut seen = Set::default();
        while let Some(key) = pending.pop() {
            if !seen.insert(key) {
                continue;
            }
            let sort = db.schema().sort(key.sort).arcsort();
            if sort.is_container_sort() {
                let children = this.container_children(key)?;
                pending.extend(children.iter().copied());
                this.structural.insert(
                    key,
                    Structural {
                        children,
                        cost: model.container_cost(db, sort, key.value),
                    },
                );
            } else if sort.is_eq_sort() && this.made.contains(&key.sort) {
                if indexed.insert(key.sort) {
                    for (table, schema) in db.schema().tables().filter(|(_, t)| {
                        t.kind() == TableKind::Constructor
                            && !t.is_unextractable()
                            && !t.name().starts_with('@')
                            && t.columns()[t.key_arity()] == key.sort
                    }) {
                        for row in db.rows_in(color, table).filter(|r| !r.is_subsumed()) {
                            let values: Vec<_> = row.values().collect();
                            let target = this.key(
                                schema.columns()[schema.key_arity()],
                                values[schema.key_arity()],
                            );
                            rows.entry(target).or_default().push((table, values));
                        }
                    }
                }
                for (table, values) in rows.remove(&key).unwrap_or_default() {
                    let schema = db.schema().table(table);
                    let children: Vec<_> = schema
                        .columns()
                        .iter()
                        .copied()
                        .zip(values.iter().copied())
                        .take(schema.key_arity())
                        .map(|(sort, value)| this.key(sort, value))
                        .collect();
                    let enode = Enode {
                        name: schema.name(),
                        children: &values[..schema.key_arity()],
                        eclass: key.value,
                        subsumed: false,
                    };
                    let id = ProducerId(this.producers.len());
                    let mut deps = Set::default();
                    let mut nested = children.clone();
                    let mut visited = Set::default();
                    while let Some(child) = nested.pop() {
                        if !visited.insert(child) {
                            continue;
                        }
                        let sort = db.schema().sort(child.sort).arcsort();
                        if sort.is_container_sort() {
                            nested.extend(this.container_children(child)?);
                        } else if sort.is_eq_sort() {
                            deps.insert(child);
                        }
                    }
                    for dep in deps {
                        this.dependencies.entry(dep).or_default().push(id);
                    }
                    pending.extend(children.iter().copied());
                    this.producers.push(Producer {
                        table,
                        target: key,
                        children,
                        cost: model.enode_cost(db, schema, &enode),
                    });
                }
            } else {
                this.structural.insert(
                    key,
                    Structural {
                        children: Vec::new(),
                        cost: model.base_value_cost(db, sort, key.value),
                    },
                );
            }
        }
        this.propagate();
        Ok(this)
    }

    fn key(&self, sort: SortId, mut value: Value) -> Key {
        let arcsort = self.db.schema().sort(sort).arcsort();
        if arcsort.is_container_sort() && self.color != Color::BASE {
            value = arcsort
                .value_type()
                .and_then(|ty| {
                    self.db.container_values().lookup_mapped(
                        ty,
                        self.db.find_in(self.color, value),
                        &|v| self.db.find_in(self.color, v),
                    )
                })
                .unwrap_or(value);
        } else if arcsort.is_eq_sort() {
            value = self.db.find_in(self.color, value);
        }
        Key { sort, value }
    }

    fn container_children(&self, key: Key) -> Result<Vec<Key>, Error> {
        self.db
            .schema()
            .sort(key.sort)
            .arcsort()
            .inner_values(self.db.container_values(), key.value)
            .into_iter()
            .map(|(sort, value)| Ok(self.key(self.db.sort(sort.name())?, value)))
            .collect()
    }

    fn candidate(&self, key: Key) -> Option<Arc<Candidate<C>>> {
        let mut pending = vec![(key, false)];
        let mut ready: Map<Key, Arc<Candidate<C>>> = Map::default();
        while let Some((key, exit)) = pending.pop() {
            if ready.contains_key(&key) {
                continue;
            }
            if let Some(best) = self.best.get(&key) {
                ready.insert(key, best.clone());
                continue;
            }
            let value = self.structural.get(&key)?;
            if exit {
                let children = value
                    .children
                    .iter()
                    .map(|key| ready.get(key).cloned())
                    .collect::<Option<Vec<_>>>()?;
                ready.insert(key, self.merge(key, &children, value.cost.clone(), None)?);
            } else {
                pending.push((key, true));
                pending.extend(value.children.iter().rev().map(|&key| (key, false)));
            }
        }
        ready.remove(&key)
    }

    fn producer_candidate(&self, id: ProducerId) -> Option<Arc<Candidate<C>>> {
        let row = &self.producers[id.0];
        let children = row
            .children
            .iter()
            .map(|&key| self.candidate(key))
            .collect::<Option<Vec<_>>>()?;
        self.merge(row.target, &children, row.cost.clone(), Some(id))
    }

    fn merge(
        &self,
        key: Key,
        children: &[Arc<Candidate<C>>],
        cost: C,
        producer: Option<ProducerId>,
    ) -> Option<Arc<Candidate<C>>> {
        let root_reachable = children.iter().any(|c| c.paid.contains_key(&key));
        let largest = children.iter().max_by_key(|c| c.plan.len());
        let mut plan = largest.map(|c| c.plan.clone()).unwrap_or_default();
        let mut conflict = false;
        for child in children {
            if largest.is_some_and(|c| Arc::ptr_eq(c, child)) {
                continue;
            }
            for (&key, &row) in &child.plan {
                match plan.entry(key) {
                    Entry::Vacant(entry) => {
                        entry.insert(row);
                    }
                    Entry::Occupied(entry) => {
                        conflict |= *entry.get() != row;
                    }
                }
            }
        }
        if root_reachable && !conflict {
            return None;
        }
        if let Some(id) = producer {
            plan.insert(key, id);
        }
        if conflict {
            return self.rescore(key, &plan).map(Arc::new);
        }
        let mut paid = children
            .iter()
            .max_by_key(|c| c.paid.len())
            .map(|c| c.paid.clone())
            .unwrap_or_default();
        for child in children {
            for (&key, value) in &child.paid {
                paid.entry(key).or_insert_with(|| value.clone());
            }
        }
        paid.entry(key).or_insert(cost);
        Some(Arc::new(Candidate::new(paid, plan)))
    }

    fn node<'a>(
        &'a self,
        key: Key,
        plan: &Map<Key, ProducerId>,
    ) -> Option<(&'a [Key], &'a C, Option<ProducerId>)> {
        if let Some(value) = self.structural.get(&key) {
            Some((&value.children, &value.cost, None))
        } else {
            let id = *plan.get(&key)?;
            let row = &self.producers[id.0];
            Some((&row.children, &row.cost, Some(id)))
        }
    }

    fn rescore(&self, key: Key, plan: &Map<Key, ProducerId>) -> Option<Candidate<C>> {
        let mut pending = vec![(key, false)];
        let mut visiting = Set::default();
        let mut paid = Map::default();
        let mut used = Map::default();
        while let Some((key, exit)) = pending.pop() {
            if paid.contains_key(&key) {
                continue;
            }
            let (children, cost, row) = self.node(key, plan)?;
            if exit {
                visiting.remove(&key);
                if let Some(id) = row {
                    used.insert(key, id);
                }
                paid.insert(key, cost.clone());
            } else {
                if !visiting.insert(key) {
                    return None;
                }
                pending.push((key, true));
                pending.extend(children.iter().rev().map(|&key| (key, false)));
            }
        }
        Some(Candidate::new(paid, used))
    }

    fn update(&mut self, id: ProducerId, pending: &mut VecDeque<Key>, queued: &mut Set<Key>) {
        let Some(candidate) = self.producer_candidate(id) else {
            return;
        };
        let key = self.producers[id.0].target;
        if self
            .best
            .get(&key)
            .is_none_or(|old| candidate.cost < old.cost)
        {
            self.best.insert(key, candidate);
            if queued.insert(key) {
                pending.push_back(key);
            }
        }
    }

    fn propagate(&mut self) {
        let mut pending = VecDeque::new();
        let mut queued = Set::default();
        for i in 0..self.producers.len() {
            self.update(ProducerId(i), &mut pending, &mut queued);
        }
        while let Some(key) = pending.pop_front() {
            queued.remove(&key);
            for id in self.dependencies.get(&key).cloned().unwrap_or_default() {
                self.update(id, &mut pending, &mut queued);
            }
        }
    }

    /// Extract a prepared root from its fixed producer plan into `termdag`.
    /// A finite zero or saturated maximum cost still returns a term. Values
    /// outside the prepared reachable problem, or without a finite term, return
    /// `None`. Sharing the termdag across calls does not combine root objectives.
    pub fn extract_best_with_sort(
        &self,
        termdag: &mut TermDag,
        value: Value,
        sort: ArcSort,
    ) -> Option<ExtractedTerm<C>> {
        let key = self.key(self.db.sort(sort.name()).ok()?, value);
        let candidate = self.candidate(key)?;
        let mut pending = vec![(key, false)];
        let mut terms: Map<Key, TermId> = Map::default();
        while let Some((key, exit)) = pending.pop() {
            if terms.contains_key(&key) {
                continue;
            }
            let (children, _, producer) = self.node(key, &candidate.plan)?;
            if exit {
                let children = children
                    .iter()
                    .map(|key| terms.get(key).copied())
                    .collect::<Option<Vec<_>>>()?;
                let sort = self.db.schema().sort(key.sort).arcsort();
                let term = if let Some(id) = producer {
                    termdag.app(
                        self.db
                            .schema()
                            .table(self.producers[id.0].table)
                            .name()
                            .to_owned(),
                        children,
                    )
                } else if sort.is_container_sort() {
                    sort.reconstruct_termdag_container(
                        self.db.container_values(),
                        key.value,
                        termdag,
                        children,
                    )
                } else {
                    sort.reconstruct_termdag_base(self.db.base_values(), key.value, termdag)
                };
                terms.insert(key, term);
            } else {
                pending.push((key, true));
                pending.extend(children.iter().rev().map(|&key| (key, false)));
            }
        }
        Some(ExtractedTerm {
            cost: candidate.cost.clone(),
            term: *terms.get(&key)?,
        })
    }
}
