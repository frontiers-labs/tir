//! The function's value matches, indexed by the class of the value they root.

use std::collections::HashMap;

use tir_relational::Csr;
use tir_relational::{ClassId as Id, Ref};

use super::pattern::Found;

/// One match: the pattern that produced it, the value it rooted at, and the
/// value every pattern node bound.
#[derive(Clone, Copy)]
pub(crate) struct MatchRef<'a> {
    pub(crate) pattern: usize,
    pub(crate) root: Ref,
    pub(crate) bindings: &'a [Ref],
}

/// Every value match of a function rooted at a row, in columns, indexed by the
/// class of the value it roots: a class holds rows at several offsets, and
/// [`Matches::at`] reads the ones at one value.
///
/// The base search runs once per function over the unscoped graph. An assumption
/// scope opens a frame naming the classes its saturation changed; everywhere else
/// the scoped graph is the base graph node for node, so the base matches still
/// speak for it. A named class is re-searched the first time any block under the
/// scope asks for it and the answer serves the rest of them, and the pop drops
/// the frame with the rows it added.
///
/// Two invariants make that memo sound, and both are structural rather than
/// asserted. A frame's set of changed classes is fixed for the frame's whole
/// lifetime, because the only mutation under a scope is a *nested* scope, whose
/// own frame shadows this one and whose `pop_context` restores the graph exactly.
/// And a class id is stable across a pop while a row id is not, so this indexes
/// classes only.
pub(crate) struct Matches {
    pattern: Vec<u32>,
    root: Vec<Ref>,
    /// `bindings[start[i]..start[i + 1]]` is match `i`'s, one value per pattern
    /// node.
    start: Vec<u32>,
    bindings: Vec<Ref>,
    base: Csr,
    scopes: Vec<Scope>,
}

/// One open assumption scope.
struct Scope {
    /// The classes it changed, ascending, so a lookup binary-searches them.
    changed: Vec<Id>,
    /// Of those, the ones some block under the scope has already asked for.
    searched: HashMap<Id, Vec<u32>>,
    mark: usize,
}

impl Matches {
    /// The function-wide index. `found` is every value match in the order the
    /// cover must see it: ascending pattern index, then production order.
    /// `classes` is the engine's class-id space, which the base index is keyed on.
    pub(crate) fn base(classes: usize, found: Vec<(usize, Found)>) -> Self {
        let entries: Vec<(u32, u32)> = found
            .iter()
            .enumerate()
            .map(|(index, (_, m))| (m.root.class.0, index as u32))
            .collect();
        let mut matches = Self {
            pattern: Vec::with_capacity(found.len()),
            root: Vec::with_capacity(found.len()),
            start: vec![0],
            bindings: Vec::new(),
            base: Csr::default(),
            scopes: Vec::new(),
        };
        for (pattern, m) in found {
            matches.push(pattern, &m);
        }
        matches.base = Csr::build(classes, entries);
        matches
    }

    /// Open a frame for an assumption scope over the classes its saturation
    /// changed, ascending.
    pub(crate) fn open_scope(&mut self, changed: Vec<Id>) {
        self.scopes.push(Scope {
            changed,
            searched: HashMap::new(),
            mark: self.pattern.len(),
        });
    }

    pub(crate) fn close_scope(&mut self) {
        let scope = self.scopes.pop().expect("open scope");
        self.bindings.truncate(self.start[scope.mark] as usize);
        self.pattern.truncate(scope.mark);
        self.root.truncate(scope.mark);
        self.start.truncate(scope.mark + 1);
    }

    /// Re-search `class` under the open assumption if it changed there and no
    /// block under the scope has asked yet. `search` must yield its matches in
    /// the order the cover sees them: ascending pattern index, then production
    /// order.
    pub(crate) fn ensure(&mut self, class: Id, search: impl FnOnce() -> Vec<(usize, Found)>) {
        let Some(scope) = self.scopes.last() else {
            return;
        };
        if scope.searched.contains_key(&class) || !self.changed(class) {
            return;
        }
        let found = search();
        let mut indices = Vec::with_capacity(found.len());
        for (pattern, m) in found {
            indices.push(self.pattern.len() as u32);
            self.push(pattern, &m);
        }
        self.scopes
            .last_mut()
            .expect("open scope")
            .searched
            .insert(class, indices);
    }

    /// The matches rooted at `value`, in production order. Call
    /// [`Self::ensure`] first: under an assumption, only it can answer for a
    /// class the assumption changed.
    pub(crate) fn at(&self, value: Ref) -> impl Iterator<Item = MatchRef<'_>> + '_ {
        self.indices(value.class)
            .iter()
            .map(|&index| index as usize)
            .filter(move |&index| self.root[index] == value)
            .map(|index| MatchRef {
                pattern: self.pattern[index] as usize,
                root: self.root[index],
                bindings: &self.bindings
                    [self.start[index] as usize..self.start[index + 1] as usize],
            })
    }

    /// The matches the function-wide search rooted at `value`, whatever an
    /// open assumption changed. A constant's own rows are those: what an
    /// assumption merges into a constant is a computation selection has no
    /// use for, since the constant is materialized instead.
    pub(crate) fn base_at(&self, value: Ref) -> impl Iterator<Item = MatchRef<'_>> + '_ {
        self.base
            .get(value.class.0)
            .iter()
            .map(|&index| index as usize)
            .filter(move |&index| self.root[index] == value)
            .map(|index| MatchRef {
                pattern: self.pattern[index] as usize,
                root: self.root[index],
                bindings: &self.bindings
                    [self.start[index] as usize..self.start[index + 1] as usize],
            })
    }

    /// The matches rooted at any value of `class`, in production order. The
    /// same contract as [`Self::at`].
    pub(crate) fn in_class(&self, class: Id) -> impl Iterator<Item = MatchRef<'_>> + '_ {
        self.indices(class).iter().map(|&index| {
            let index = index as usize;
            MatchRef {
                pattern: self.pattern[index] as usize,
                root: self.root[index],
                bindings: &self.bindings
                    [self.start[index] as usize..self.start[index + 1] as usize],
            }
        })
    }

    /// Whether any open assumption changed `class`, so the function-wide search
    /// no longer speaks for it.
    fn changed(&self, class: Id) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.changed.binary_search(&class).is_ok())
    }

    fn indices(&self, class: Id) -> &[u32] {
        for scope in self.scopes.iter().rev() {
            if let Some(found) = scope.searched.get(&class) {
                return found;
            }
        }
        assert!(
            !self.changed(class),
            "class {} was read before the open assumption re-searched it",
            class.0
        );
        self.base.get(class.0)
    }

    fn push(&mut self, pattern: usize, m: &Found) {
        self.pattern.push(pattern as u32);
        self.root.push(m.root);
        self.bindings.extend_from_slice(&m.bindings);
        self.start.push(self.bindings.len() as u32);
    }
}
