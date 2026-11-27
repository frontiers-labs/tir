use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use tir_adt::FxBuildHasher;

pub(crate) type ClassRoots = HashMap<u32, u32, FxBuildHasher>;

// One union-find over the ids of every Eq sort and container. The younger of
// two roots joins the older and paths are never compressed, so `parents` is
// the map `select_moved` reads: a cell has moved exactly when its parent is
// some other id.
#[derive(Clone, Default)]
pub(crate) struct UnionFind {
    parents: Vec<u32>,
    // The ids that joined another root since the last `take_moved`.
    moved: Vec<u32>,
    revision: u64,
    captured: OnceLock<Arc<ClassRoots>>,
}

impl UnionFind {
    pub(crate) fn fresh(&mut self) -> u32 {
        let id = u32::try_from(self.parents.len()).expect("ids fit in u32");
        self.parents.push(id);
        id
    }

    pub(crate) fn len(&self) -> u32 {
        u32::try_from(self.parents.len()).expect("ids fit in u32")
    }

    // Forget the ids from `len` on. None of them may be the parent of an
    // older id.
    pub(crate) fn truncate(&mut self, len: u32) {
        debug_assert!(
            self.parents[..len as usize]
                .iter()
                .all(|&parent| parent < len)
        );
        self.parents.truncate(len as usize);
        self.moved.retain(|&id| id < len);
        self.captured.take();
    }

    pub(crate) fn contains(&self, x: u32) -> bool {
        (x as usize) < self.parents.len()
    }

    // A cell past the end is no id of ours, so it is its own root. Container
    // contents can hold base values next to ids.
    pub(crate) fn find(&self, mut x: u32) -> u32 {
        while let Some(&parent) = self.parents.get(x as usize)
            && parent != x
        {
            x = parent;
        }
        x
    }

    pub(crate) fn is_root(&self, x: u32) -> bool {
        self.parents
            .get(x as usize)
            .is_none_or(|&parent| parent == x)
    }

    pub(crate) fn union(&mut self, a: u32, b: u32) -> u32 {
        let (a, b) = (self.find(a), self.find(b));
        let (younger, older) = (a.max(b), a.min(b));
        if younger != older {
            self.parents[younger as usize] = older;
            self.moved.push(younger);
            self.revision += 1;
            self.captured.take();
        }
        older
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn parents(&self) -> &[u32] {
        &self.parents
    }

    // Captures share the non-root entries until the next union. Fresh roots
    // are implicit in the map, so allocating an id never invalidates it.
    pub(crate) fn capture(&self) -> Arc<ClassRoots> {
        Arc::clone(self.captured.get_or_init(|| {
            let mut roots = ClassRoots::default();
            if self.revision != 0 {
                for (id, &parent) in (0..).zip(&self.parents) {
                    if id != parent {
                        roots.insert(id, roots.get(&parent).copied().unwrap_or(parent));
                    }
                }
            }
            Arc::new(roots)
        }))
    }

    pub(crate) fn unions(&self) -> usize {
        self.moved.len()
    }

    pub(crate) fn take_moved(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.moved)
    }
}
