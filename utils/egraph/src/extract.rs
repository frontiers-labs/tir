use tir_relational::extract::fold;

use crate::{ClassId as Id, Engine, FxHashMap, Label as ENode, Ref, RowId};

/// Cheapest representative e-node per e-class, chosen by [`Engine::extract_best`].
///
/// A scope's extraction is a layer over the one it refreshed rather than a copy
/// of it, so it holds only the classes its assumption dirtied and costs the
/// dirty set instead of the graph.
pub struct Extraction<'a, L: ENode> {
    base: Option<&'a Extraction<'a, L>>,
    /// What this layer settled. An explicit `None` is a class the layer
    /// recomputed and found no finite cost for; it must read as nothing rather
    /// than fall through to the base's stale answer.
    best: FxHashMap<Id, Option<Chosen<L>>>,
}

/// What a class was extracted as, and what that spelling cost. The cost is what
/// an incremental re-extraction reads for a class it does not recompute.
#[derive(Clone)]
struct Chosen<L> {
    node: L,
    row: RowId,
    cost: u64,
}

impl<'a, L: ENode> Extraction<'a, L> {
    /// Chosen node for `id`'s class, or `None` if no node has finite cost. `id` must
    /// be canonical ([`Engine::find`]).
    pub fn node(&self, id: Id) -> Option<&L> {
        self.chosen(id).map(|chosen| &chosen.node)
    }

    /// The e-node chosen for `id`'s class: [`Engine::children`] reads its
    /// operands as references, and [`Engine::value`] the offset it sits at
    /// from the class, which a spelling of the class subtracts.
    pub fn row(&self, id: Id) -> Option<RowId> {
        self.chosen(id).map(|chosen| chosen.row)
    }

    /// This extraction with the classes in `dirty`, and only those, recomputed
    /// against `eg` as it stands now.
    ///
    /// A class's answer depends on its own rows and its children's costs, so
    /// `dirty` must name every class changed since `self` was taken, closed
    /// upward over parents: [`Engine::scope_dirty`] where `self` is the base
    /// extraction, [`Engine::innermost_dirty`] where it is the one the enclosing
    /// scope refreshed. A class outside it has the rows and
    /// the child costs it had when `self` was taken, so it keeps its answer.
    /// Ascending order makes the sweep the same scan a full pass takes over those
    /// classes, which is what keeps the cost ties broken the same way.
    pub fn refresh<'b>(
        &'b self,
        eg: &Engine<L>,
        dirty: &[Id],
        cost_of: impl Fn(Id, &L, &[Ref], u64) -> u64,
    ) -> Extraction<'b, L> {
        let dirty: Vec<u32> = dirty.iter().map(|id| id.0).collect();
        eg.extraction(&dirty, Some(self), cost_of)
    }

    /// What the spelling chosen for `id`'s class costs, its operands'
    /// spellings included.
    pub fn cost(&self, id: Id) -> Option<u64> {
        self.chosen(id).map(|chosen| chosen.cost)
    }

    /// What the innermost layer holding `id` settled on, `None` once a layer
    /// says the class has no finite cost.
    fn chosen(&self, id: Id) -> Option<&Chosen<L>> {
        match self.best.get(&id) {
            Some(settled) => settled.as_ref(),
            None => self.base?.chosen(id),
        }
    }
}

impl<L: ENode> Engine<L> {
    /// Greedy bottom-up extraction: per class, the node minimizing
    /// `cost_of(class, node, operands, offset)` plus each operand class's
    /// chosen cost. The class is the canonical id the node would represent,
    /// so a cost model may reject a form the class cannot be spelled in. The
    /// operands are the node's canonical references, and `offset` is where the
    /// node sits from the class: a model charges an add-of-constant for each
    /// non-zero one it has to spell, and nothing for an operand
    /// [`Engine::int_const`] reads as a constant. A [`ENode::is_unique`] node
    /// costs its own price alone: it stands for what it names, which no
    /// spelling of its operands is. Cycle-tolerant — a node with un-costed
    /// children is skipped and revisited to a fixpoint, so a cycle is costed
    /// through its non-cyclic input. Scope-aware via
    /// [`Engine::classes`]/[`Engine::find`].
    pub fn extract_best(
        &self,
        cost_of: impl Fn(Id, &L, &[Ref], u64) -> u64,
    ) -> Extraction<'static, L> {
        let classes: Vec<u32> = self.class_ids().map(|id| id.0).collect();
        self.extraction(&classes, None, cost_of)
    }

    /// Fold `classes` over their e-nodes in class-list order, a child outside
    /// them costed from `base`, and record the winners as a layer over `base`.
    fn extraction<'b>(
        &self,
        classes: &[u32],
        base: Option<&'b Extraction<'b, L>>,
        cost_of: impl Fn(Id, &L, &[Ref], u64) -> u64,
    ) -> Extraction<'b, L> {
        let outside = |child| base.and_then(|base| base.cost(Id(child)));
        let chosen = fold(self.class_count(), classes, &outside, |class, out| {
            for row in self.rows(Id(class)) {
                let node = self.node(row);
                let operands = self.children(row);
                let price = cost_of(Id(class), node, &operands, self.value(row).offset);
                let spelled = operands.iter().filter(|_| !node.is_unique());
                out.push(row.0, price, spelled.map(|operand| operand.class.0));
            }
        });
        let mut best = FxHashMap::default();
        for (&class, chosen) in classes.iter().zip(chosen) {
            match chosen {
                Some((row, cost)) => {
                    let row = RowId(row);
                    let node = self.node(row).clone();
                    best.insert(Id(class), Some(Chosen { node, row, cost }));
                }
                // Only a layer has a base to shadow; the bottom one says nothing
                // about a class it could not cost.
                None if base.is_some() => {
                    best.insert(Id(class), None);
                }
                None => {}
            }
        }
        Extraction { base, best }
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::Term;
    use crate::{ClassId, Engine, Label, Ref};

    /// Charges one per node, a hundred for a leaf named `r`, and ten for each
    /// add-of-constant a spelling needs: a non-zero operand offset that is not
    /// a constant, and a node that sits at an offset from its class.
    fn cost(eg: &Engine<Term>) -> impl Fn(ClassId, &Term, &[Ref], u64) -> u64 + '_ {
        move |_, node, operands, offset| {
            let adds = operands
                .iter()
                .filter(|operand| operand.offset != 0 && eg.int_const(**operand).is_none())
                .count() as u64
                + u64::from(offset != 0);
            1 + 99 * u64::from(node.op == "r") + 10 * adds
        }
    }

    #[test]
    fn a_spelling_pays_for_the_offsets_it_has_to_add() {
        let mut eg = Engine::new();
        let r = eg.add(Term::typed("r", 8, &[]));
        let s = eg.add(Term::typed("s", 8, &[]));
        let p = eg.add(Term::typed("p", 8, &[]));
        // r == s + 7, and one class holds h(p + 4) and k(p).
        eg.union(r, Ref::new(s.class, 7)).unwrap();
        let shifted = eg.insert(Term::typed("h", 8, &[ClassId(0)]), &[Ref::new(p.class, 4)]);
        let plain = eg.insert(Term::typed("k", 8, &[ClassId(0)]), &[p]);
        eg.union(shifted, plain).unwrap();
        eg.rebuild();
        let extraction = eg.extract_best(cost(&eg));

        let class = eg.root(plain.class);
        assert_eq!(
            extraction.node(class).map(|node| node.op.as_str()),
            Some("k")
        );
        // `r` names the class, but `s - 7` is the cheaper spelling of it.
        let class = eg.root(r.class);
        let row = extraction.row(class).expect("a costed class");
        assert_eq!(eg.node(row).op, "s");
        assert_eq!(eg.value(row), Ref::new(class, 249));
        assert_eq!(eg.node(row).children().len(), 0);
    }

    // `st(c)` in `c`, at the top price, costs nothing of `c` its other
    // members did not, wherever a union leaves it.
    #[test]
    fn a_self_loop_at_the_top_price_leaves_its_class_its_cost() {
        let mut eg = Engine::new();
        let x = eg.add(Term::typed("x", 8, &[]));
        let y = eg.add(Term::typed("r", 8, &[]));
        for class in [x, y] {
            let seed = eg.insert(Term::typed("st", 8, &[ClassId(0)]), &[class]);
            eg.union(seed, class).unwrap();
        }
        let user = eg.insert(Term::typed("k", 8, &[ClassId(0)]), &[y]);
        eg.union(y, Ref::new(x.class, 5)).unwrap();
        eg.rebuild();
        let priced = cost(&eg);
        let extraction = eg.extract_best(|class, node, operands, offset| match node.op.as_str() {
            "st" => u64::MAX,
            _ => priced(class, node, operands, offset),
        });
        let class = eg.root(x.class);
        assert_eq!(
            extraction.node(class).map(|node| node.op.as_str()),
            Some("x")
        );
        assert_eq!(extraction.cost(class), Some(1));
        // k(x + 5): itself, the add, and x.
        assert_eq!(extraction.cost(eg.root(user.class)), Some(12));
    }
}
