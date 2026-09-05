use std::{collections::HashMap, mem};

use crate::FxBuildHasher;

/// A handle to a string interned in an [`Interner`].
///
/// Ids are assigned in intern order, so they are reproducible across runs but
/// only meaningful relative to the interner that produced them.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Sym(u32);

impl Sym {
    /// Position in the interner's id space: ids are handed out in intern order.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A string interner mapping equal strings to a single [`Sym`] id.
///
/// Strings are copied into a doubling arena of `String` buffers that are never
/// dropped or shrunk while the interner lives.
#[derive(Default)]
pub struct Interner {
    map: HashMap<&'static str, Sym, FxBuildHasher>,
    vec: Vec<&'static str>,
    buf: String,
    full: Vec<String>,
}

impl Interner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Interns `name`, copying it into the arena on first sight.
    pub fn intern(&mut self, name: &str) -> Sym {
        if let Some(&sym) = self.map.get(name) {
            return sym;
        }
        let name = self.alloc(name);
        self.insert(name)
    }

    /// Interns a `'static` string without copying it into the arena.
    pub fn intern_static(&mut self, name: &'static str) -> Sym {
        if let Some(&sym) = self.map.get(name) {
            return sym;
        }
        self.insert(name)
    }

    /// The id `name` already has, without interning it.
    pub fn lookup(&self, name: &str) -> Option<Sym> {
        self.map.get(name).copied()
    }

    pub fn resolve(&self, sym: Sym) -> &str {
        self.vec[sym.0 as usize]
    }

    fn insert(&mut self, name: &'static str) -> Sym {
        let sym = Sym(self.vec.len() as u32);
        self.map.insert(name, sym);
        self.vec.push(name);
        sym
    }

    fn alloc(&mut self, name: &str) -> &'static str {
        if self.buf.capacity() - self.buf.len() < name.len() {
            let capacity = (self.buf.capacity().max(name.len()) + 1).next_power_of_two();
            let retired = mem::replace(&mut self.buf, String::with_capacity(capacity));
            self.full.push(retired);
        }

        let start = self.buf.len();
        self.buf.push_str(name);
        let interned: &str = &self.buf[start..];

        // Safety: the reference points into a heap allocation that outlives every
        // borrow handed out by `resolve` (which reborrows through `&self`). The
        // active buffer never reallocates - it is reserved up front and retired
        // into `full` once full - and retired buffers are neither dropped nor
        // mutated for the lifetime of the interner, so the pointee never moves.
        unsafe { &*(interned as *const str) }
    }
}

#[cfg(test)]
mod tests {
    use super::{Interner, Sym};

    #[test]
    fn repeated_intern_returns_same_sym() {
        let mut interner = Interner::new();

        let first = interner.intern("add");
        let second = interner.intern("add");

        assert_eq!(first, second);
        assert_eq!(interner.resolve(first), "add");
    }

    #[test]
    fn static_and_copied_strings_share_one_id_space() {
        let mut interner = Interner::new();

        let add = interner.intern_static("add");
        let sub = interner.intern(&String::from("sub"));
        let add_again = interner.intern(&String::from("add"));
        let mul = interner.intern_static("mul");
        let sub_again = interner.intern_static("sub");

        assert_eq!([add, sub, mul], [Sym(0), Sym(1), Sym(2)]);
        assert_eq!(add_again, add);
        assert_eq!(sub_again, sub);
        assert_eq!(interner.resolve(add), "add");
        assert_eq!(interner.resolve(sub), "sub");
        assert_eq!(interner.resolve(mul), "mul");
    }

    #[test]
    fn strings_survive_buffer_growth() {
        let mut interner = Interner::new();

        let first = interner.intern("first");
        let syms: Vec<Sym> = (0..64)
            .map(|i| interner.intern(&("x".repeat(64) + &i.to_string())))
            .collect();
        let last = interner.intern("last");

        assert_eq!(interner.resolve(first), "first");
        assert_eq!(interner.resolve(last), "last");
        for (i, sym) in syms.iter().enumerate() {
            assert_eq!(interner.resolve(*sym), "x".repeat(64) + &i.to_string());
        }
    }
}
