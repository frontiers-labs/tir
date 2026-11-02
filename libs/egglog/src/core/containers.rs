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
    fn rebuild(&mut self, uf: &mut UnionFind, dirty: &mut Vec<u32>);
    fn holding(&self, ids: &HashSet<u32, FxBuildHasher>, out: &mut Vec<u32>);
    fn map(&mut self, id: Value, f: &dyn Fn(Value) -> Value, uf: &mut UnionFind) -> Option<Value>;
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

    fn map(&mut self, id: Value, f: &dyn Fn(Value) -> Value, uf: &mut UnionFind) -> Option<Value> {
        let mut container = self.values.get(&id)?.clone();
        container.rebuild_contents(f);
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
            if !ids.contains(&id.raw()) && container.iter().any(|value| ids.contains(&value.raw()))
            {
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
}

impl Clone for ContainerValues {
    fn clone(&self) -> Self {
        Self {
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
        env.intern(container, uf)
    }

    /// The container of type `C` with id `id`, if there is one.
    pub fn get_val<C: ContainerValue>(&self, id: Value) -> Option<&C> {
        let env = self.envs[*self.types.get(&TypeId::of::<C>())?].as_any();
        env.downcast_ref::<ContainerEnv<C>>()?.values.get(&id)
    }

    // The id of the container of type `ty` at `id` with `f` applied to the
    // values inside.
    pub(crate) fn map(
        &mut self,
        ty: TypeId,
        id: Value,
        f: &dyn Fn(Value) -> Value,
        uf: &mut UnionFind,
    ) -> Option<Value> {
        let at = *self.types.get(&ty)?;
        self.envs[at].map(id, f, uf)
    }

    // Returns the ids whose contents changed in place, and of the containers
    // that hold them, transitively.
    pub(crate) fn rebuild(&mut self, uf: &mut UnionFind) -> Vec<u32> {
        let mut dirty = Vec::new();
        for env in &mut self.envs {
            env.rebuild(uf, &mut dirty);
        }
        let mut seen: HashSet<u32, FxBuildHasher> = dirty.iter().copied().collect();
        let mut frontier = dirty.clone();
        while !frontier.is_empty() {
            let ids: HashSet<u32, FxBuildHasher> = frontier.drain(..).collect();
            for env in &self.envs {
                env.holding(&ids, &mut frontier);
            }
            frontier.retain(|&id| seen.insert(id));
            dirty.extend(&frontier);
        }
        dirty
    }
}
