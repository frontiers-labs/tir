// Copyright (c) 2025 egraphs-good
// SPDX-License-Identifier: MIT
// Adapted from egglog 4b5686be, core-relations/src/containers/mod.rs. See libs/egglog/LICENSE-MIT.

use std::any::{Any, TypeId};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use tir_adt::FxBuildHasher;

use super::Value;
use super::uf::UnionFind;

/// A container type, such as a vector or a set of values.
///
/// Containers are hash-consed like base values, but their ids come from the
/// union-find of e-class ids: when a union makes two containers equal, a
/// rebuild unions their ids.
pub trait ContainerValue: Hash + Eq + Clone + Send + Sync + 'static {
    /// Replace each e-class id inside with `find` of it, and return whether
    /// anything changed. A container that returns `false` must be unchanged.
    fn rebuild_contents(&mut self, find: &dyn Fn(Value) -> Value) -> bool;

    /// The values inside. A rebuild skips a container all of whose values
    /// are canonical, so this must yield every e-class id inside; yielding
    /// base values too is harmless.
    fn iter(&self) -> impl Iterator<Item = Value> + '_;
}

trait Env: Any + Send + Sync {
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn clone_box(&self) -> Box<dyn Env>;
    fn len(&self) -> usize;
    fn retains_from(&self, mark: u32, end: u32) -> bool;
    fn truncate(&mut self, mark: u32);
    fn rebuild(&mut self, uf: &mut UnionFind, dirty: &mut Vec<u32>);
    fn holding(&self, ids: &HashSet<u32, FxBuildHasher>, out: &mut Vec<u32>);
    fn lookup_mapped(&self, id: Value, find: &dyn Fn(Value) -> Value) -> Option<Value>;
    fn map(
        &mut self,
        id: Value,
        f: &dyn Fn(Value, &UnionFind) -> Value,
        uf: &mut UnionFind,
    ) -> Option<Value>;
}

#[derive(Clone)]
struct ContainerEnv<C> {
    ids: HashMap<C, Value, FxBuildHasher>,
    values: HashMap<Value, C, FxBuildHasher>,
}

impl<C: ContainerValue> ContainerEnv<C> {
    fn intern(&mut self, container: C, uf: &mut UnionFind) -> Value {
        if let Some(&id) = self.ids.get(&container) {
            return id;
        }
        let id = Value::new(uf.fresh());
        self.values.insert(id, container.clone());
        self.ids.insert(container, id);
        id
    }
}

impl<C: ContainerValue> Env for ContainerEnv<C> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn clone_box(&self) -> Box<dyn Env> {
        Box::new(self.clone())
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn retains_from(&self, mark: u32, end: u32) -> bool {
        self.ids.iter().any(|(container, id)| {
            id.raw() < mark && container.iter().any(|v| (mark..end).contains(&v.raw()))
        })
    }

    fn truncate(&mut self, mark: u32) {
        self.ids.retain(|_, id| id.raw() < mark);
        self.values.retain(|id, _| id.raw() < mark);
    }

    fn lookup_mapped(&self, id: Value, find: &dyn Fn(Value) -> Value) -> Option<Value> {
        let mut container = self.values.get(&id)?.clone();
        container.rebuild_contents(find);
        self.ids.get(&container).copied()
    }

    fn map(
        &mut self,
        id: Value,
        f: &dyn Fn(Value, &UnionFind) -> Value,
        uf: &mut UnionFind,
    ) -> Option<Value> {
        let mut container = self.values.get(&id)?.clone();
        container.rebuild_contents(&|value| f(value, uf));
        Some(self.intern(container, uf))
    }

    // A container whose contents change keeps its id unless an equal one
    // exists, which it is unioned with. One whose id stays while its contents
    // change goes to `dirty`: the rows holding it need a new timestamp.
    fn rebuild(&mut self, uf: &mut UnionFind, dirty: &mut Vec<u32>) {
        let stale = |c: &C, id: &mut Value| {
            !uf.is_root(id.raw()) || c.iter().any(|value| !uf.is_root(value.raw()))
        };
        let moved: Vec<(C, Value)> = self.ids.extract_if(stale).collect();
        for (mut container, old) in moved {
            self.values.remove(&old);
            let id = uf.find(old.raw());
            let changed = container.rebuild_contents(&|value| Value::new(uf.find(value.raw())));
            match self.ids.entry(container) {
                Entry::Occupied(mut entry) => {
                    let other = *entry.get();
                    let root = Value::new(uf.union(other.raw(), id));
                    if changed && root == old {
                        dirty.push(root.raw());
                    }
                    if root != other {
                        let container = self.values.remove(&other).expect("both maps agree");
                        self.values.insert(root, container);
                        entry.insert(root);
                    }
                }
                Entry::Vacant(entry) => {
                    if changed && id == old.raw() {
                        dirty.push(id);
                    }
                    self.values.insert(Value::new(id), entry.key().clone());
                    entry.insert(Value::new(id));
                }
            }
        }
    }

    fn holding(&self, ids: &HashSet<u32, FxBuildHasher>, out: &mut Vec<u32>) {
        for (container, id) in &self.ids {
            if container.iter().any(|value| ids.contains(&value.raw())) {
                out.push(id.raw());
            }
        }
    }
}

/// The containers of a [`Database`](crate::Database), hash-consed per type.
#[derive(Default)]
pub struct ContainerValues {
    types: HashMap<TypeId, usize, FxBuildHasher>,
    envs: Vec<Box<dyn Env>>,
    revision: u64,
}

impl Clone for ContainerValues {
    fn clone(&self) -> Self {
        Self {
            revision: self.revision,
            types: self.types.clone(),
            envs: self.envs.iter().map(|env| env.clone_box()).collect(),
        }
    }
}

impl ContainerValues {
    pub(crate) fn intern<C: ContainerValue>(&mut self, container: C, uf: &mut UnionFind) -> Value {
        let at = *self.types.entry(TypeId::of::<C>()).or_insert_with(|| {
            self.envs.push(Box::new(ContainerEnv::<C> {
                ids: HashMap::default(),
                values: HashMap::default(),
            }));
            self.envs.len() - 1
        });
        let env: &mut ContainerEnv<C> = self.envs[at]
            .as_any_mut()
            .downcast_mut()
            .expect("one env per type");
        let before = env.len();
        let id = env.intern(container, uf);
        self.revision += u64::from(env.len() != before);
        id
    }

    /// The container of type `C` with id `id`, if there is one.
    pub fn get_val<C: ContainerValue>(&self, id: Value) -> Option<&C> {
        let env = self.envs[*self.types.get(&TypeId::of::<C>())?].as_any();
        env.downcast_ref::<ContainerEnv<C>>()?.values.get(&id)
    }

    // Read the counterpart already interned by a colored rebuild without
    // changing the contents shared with the base and other colors.
    pub(crate) fn lookup_mapped(
        &self,
        ty: TypeId,
        id: Value,
        find: &dyn Fn(Value) -> Value,
    ) -> Option<Value> {
        self.envs[*self.types.get(&ty)?].lookup_mapped(id, find)
    }

    // The id of the container of type `ty` at `id` with `f` applied to the
    // values inside.
    pub(crate) fn map(
        &mut self,
        ty: TypeId,
        id: Value,
        f: &dyn Fn(Value, &UnionFind) -> Value,
        uf: &mut UnionFind,
    ) -> Option<Value> {
        let at = *self.types.get(&ty)?;
        let before = self.envs[at].len();
        let mapped = self.envs[at].map(id, f, uf);
        self.revision += u64::from(self.envs[at].len() != before);
        mapped
    }

    pub(crate) fn holding(&self, ids: &[u32], find: &dyn Fn(u32) -> u32) -> Vec<u32> {
        let mut seen: HashSet<u32, FxBuildHasher> = ids.iter().copied().collect();
        let mut frontier = ids.to_vec();
        let mut affected = ids.to_vec();
        while !frontier.is_empty() {
            let ids = frontier.drain(..).collect();
            for env in &self.envs {
                env.holding(&ids, &mut frontier);
            }
            let roots: Vec<u32> = frontier.iter().map(|&id| find(id)).collect();
            frontier.extend(roots);
            frontier.retain(|id| seen.insert(*id));
            affected.extend_from_slice(&frontier);
        }
        affected
    }

    pub(crate) fn check_truncate(&self, mark: u32, end: u32) {
        assert!(
            self.envs.iter().all(|env| !env.retains_from(mark, end)),
            "a retained container holds an id made after the mark"
        );
    }

    pub(crate) fn truncate(&mut self, mark: u32) {
        for env in &mut self.envs {
            env.truncate(mark);
        }
        self.revision += 1;
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    // Map only containers that hold an affected id; nested containers follow
    // through the same equality fixed point as table congruence.
    pub(crate) fn map_holding(
        &mut self,
        ids: &HashSet<u32, FxBuildHasher>,
        find: &dyn Fn(Value) -> Value,
        uf: &mut UnionFind,
    ) -> Vec<(Value, Value)> {
        let mut mapped = Vec::new();
        for at in 0..self.envs.len() {
            let mut holding = Vec::new();
            self.envs[at].holding(ids, &mut holding);
            for id in holding {
                let before = self.envs[at].len();
                let old = Value::new(id);
                if let Some(new) =
                    self.envs[at].map(old, &|v, uf| find(Value::new(uf.find(v.raw()))), uf)
                {
                    self.revision += u64::from(self.envs[at].len() != before);
                    if find(old) != find(new) {
                        mapped.push((old, new));
                    }
                }
            }
        }
        mapped
    }

    // Returns the ids whose contents changed in place, and of the containers
    // that hold them, transitively.
    pub(crate) fn rebuild(&mut self, uf: &mut UnionFind) -> Vec<u32> {
        self.revision += 1;
        let mut dirty = Vec::new();
        for env in &mut self.envs {
            env.rebuild(uf, &mut dirty);
        }
        self.holding(&dirty, &|id| uf.find(id))
    }
}
