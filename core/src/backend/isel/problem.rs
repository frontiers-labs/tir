//! The frozen selection problem of a function.
//!
//! Matching runs under the entry facts of the regions it visits, so what a
//! region offers is read off the scoped graph before its scope closes and
//! recorded here: the implementation instances, what each demands of the
//! classes it binds, the registers that exist without an instance, the effects
//! and the control alternatives. Nothing here names a solver variable, and
//! nothing changes once every region is recorded. A class id is the scope's
//! representative, a key inside its own region's record and meaningless
//! outside it; what crosses regions is the base member a register is named by.

use std::collections::{HashMap, HashSet};

use tir::{OpId, RegionId, sem::SymKind};
use tir_adt::APInt;
use tir_relational::ClassId as Id;

use super::builder::ControlSlot;
use super::cover::{ChildDemand, PbqpIselAlternative, PbqpIselMatch};

/// Whether a base member's value sits in a register an enclosing region
/// produced.
pub(crate) type HasRegister<'a> = &'a dyn Fn(Id, RegionId) -> bool;

/// The registers holding a class without an instance of its own region.
#[derive(Clone, Debug, Default)]
pub(crate) struct Availability {
    /// A port, a surviving operation or an entry input holds it.
    pub(crate) always: bool,
    /// Enclosing regions that define it ahead of this one. Each supplies it
    /// only where that region's assignment produces the register.
    pub(crate) ancestors: Vec<RegionId>,
}

/// What selection reads off one class of a region.
#[derive(Clone, Debug)]
pub(crate) struct ClassFacts {
    /// The base classes the scope merged into this one.
    pub(crate) members: Vec<Id>,
    /// The base classes whose values may name a register holding the class.
    pub(crate) binding_members: Vec<Id>,
    /// The constant the class is proven to be.
    pub(crate) int: Option<APInt>,
    pub(crate) pure: bool,
    pub(crate) width: Option<u32>,
    /// The class whose register a low-bit view reads; the class itself when it
    /// is no view.
    pub(crate) source: Id,
    /// The semantic kind a missing-rule diagnostic names.
    pub(crate) kind: Option<SymKind>,
    pub(crate) has_values: bool,
    /// The region must compute the class: a register or effect demand.
    pub(crate) placed: bool,
    /// A reader outside the cover needs the class in a register.
    pub(crate) register_demand: bool,
    pub(crate) availability: Availability,
    /// A constant the target's own hook materializes after selection.
    pub(crate) hook_constant: bool,
}

impl ClassFacts {
    pub(crate) fn has_register(&self, has_register: HasRegister) -> bool {
        self.availability.always
            || self.availability.ancestors.iter().any(|&region| {
                self.members
                    .iter()
                    .any(|&member| has_register(member, region))
            })
    }
}

/// Whether a register holding a class can be named where a branch reads it:
/// always, or once an enclosing region's assignment leaves one of these.
#[derive(Clone, Debug, Default)]
pub(crate) struct Naming {
    pub(crate) always: bool,
    pub(crate) registers: Vec<(Id, RegionId)>,
}

/// One operand of a fused branch.
#[derive(Clone, Debug)]
pub(crate) struct GuardOperand {
    pub(crate) symbol: u32,
    pub(crate) class: Id,
    /// The branch reads the operand as a register.
    pub(crate) register: bool,
    pub(crate) naming: Naming,
}

/// A conditional-branch instance testing one control outcome: the rule and
/// the classes its operands bind. Operand registers are named once the
/// assignment says which exist.
#[derive(Clone, Debug)]
pub(crate) struct GuardCandidate {
    pub(crate) rule_index: usize,
    pub(crate) cost: u64,
    pub(crate) target_symbol: u32,
    pub(crate) captures: Vec<GuardOperand>,
}

/// One control outcome a region must realize, with every way to do it.
#[derive(Clone, Debug)]
pub(crate) struct ControlCandidates {
    pub(crate) op: OpId,
    pub(crate) slot: ControlSlot,
    /// The class whose register a branch-if-nonzero reads.
    pub(crate) condition: Id,
    /// The outcome the region's facts already prove.
    pub(crate) decided: Option<bool>,
    /// Fused branch instances, cheapest and most specific first.
    pub(crate) fused: Vec<GuardCandidate>,
    /// Fused branches over the complement, in the same order.
    pub(crate) inverses: Vec<GuardCandidate>,
    /// Classes a fused branch leaves without a value instance.
    pub(crate) waives: Vec<Id>,
    /// Where a fused branch reads its operands.
    pub(crate) anchor: Option<OpId>,
    /// Where a branch-if-nonzero reads the condition.
    pub(crate) at: Option<OpId>,
    /// Whether the condition's register can be named there without an
    /// instance of this region defining it.
    pub(crate) nonzero_naming: Naming,
}

/// How one control outcome is realized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlChoice {
    Decided(bool),
    /// An index into [`ControlCandidates::fused`].
    Fused(usize),
    Nonzero,
}

/// Everything one region offers and requires.
pub(crate) struct RegionProblem {
    pub(crate) region: RegionId,
    /// The region's operations in the order they were lowered in.
    pub(crate) order: Vec<OpId>,
    /// The class each rooted operation of the region computes, in that order.
    pub(crate) op_class: Vec<(OpId, Id)>,
    /// The earliest operation of the region rooting each class.
    pub(crate) source_ops: HashMap<Id, OpId>,
    /// The value sites the assignment decides, ascending.
    pub(crate) classes: Vec<Id>,
    pub(crate) facts: HashMap<Id, ClassFacts>,
    /// The implementation instances.
    pub(crate) matches: Vec<PbqpIselMatch>,
    pub(crate) controls: Vec<ControlCandidates>,
    /// The base member naming the register an effect instance at a class
    /// leaves for the regions this one encloses.
    pub(crate) register_names: HashMap<Id, Id>,
    /// Operations whose effect is the identity: readers take the state they
    /// observed.
    pub(crate) identities: Vec<OpId>,
    /// The cost of the target's branch-if-nonzero.
    pub(crate) nonzero_cost: u64,
}

/// The chosen instances and control realizations of one region.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RegionAssignment {
    /// The instance producing each class it roots.
    pub(crate) tiles: HashMap<Id, usize>,
    pub(crate) controls: Vec<ControlChoice>,
}

/// What a region's control choices demand of its classes.
pub(crate) struct Policy {
    /// Classes a selected branch reads as registers.
    pub(crate) overlay: HashSet<Id>,
    pub(crate) demanded: HashSet<Id>,
    pub(crate) available: HashSet<Id>,
    /// The low-bit views of each demanded source no operation computes.
    pub(crate) views: HashMap<Id, Vec<Id>>,
}

impl Policy {
    pub(crate) fn demanded(&self, class: Id) -> bool {
        self.demanded.contains(&class)
    }

    pub(crate) fn available(&self, class: Id) -> bool {
        self.available.contains(&class)
    }
}

impl RegionProblem {
    pub(crate) fn facts(&self, class: Id) -> &ClassFacts {
        &self.facts[&class]
    }

    /// The earliest operation of the region rooting `class`.
    pub(crate) fn source_op(&self, class: Id) -> Option<OpId> {
        self.source_ops.get(&class).copied()
    }

    /// The registers a fused branch reads without a constant to fold: the
    /// classes its choice demands.
    pub(crate) fn guard_demands<'a>(
        &'a self,
        guard: &'a GuardCandidate,
    ) -> impl Iterator<Item = Id> + 'a {
        guard
            .captures
            .iter()
            .filter(|operand| self.facts(operand.class).int.is_none())
            .map(|operand| self.facts(operand.class).source)
    }

    /// The registers an assignment leaves for the regions this one encloses.
    pub(crate) fn registers<'a>(
        &'a self,
        assignment: &'a RegionAssignment,
    ) -> impl Iterator<Item = (Id, RegionId)> + 'a {
        assignment
            .tiles
            .keys()
            .filter_map(|class| self.register_names.get(class))
            .map(|&member| (member, self.region))
    }

    pub(crate) fn materialized(&self, policy: &Policy, class: Id) -> bool {
        policy.overlay.contains(&class) || self.facts(class).register_demand
    }

    /// The demand the chosen control realizations place on the region's
    /// classes, given the registers enclosing regions produced.
    pub(crate) fn policy(&self, choices: &[ControlChoice], has_register: HasRegister) -> Policy {
        let mut overlay = HashSet::new();
        // A direct control fused into its branch needs no separate value.
        let mut waived = HashSet::new();
        for (control, choice) in self.controls.iter().zip(choices) {
            match *choice {
                ControlChoice::Decided(_) => {}
                ControlChoice::Fused(index) => {
                    overlay.extend(self.guard_demands(&control.fused[index]));
                    waived.extend(control.waives.iter().copied());
                }
                ControlChoice::Nonzero => {
                    overlay.insert(self.facts(control.condition).source);
                }
            }
        }
        let demanded: HashSet<Id> = self
            .classes
            .iter()
            .copied()
            .filter(|class| {
                (!waived.contains(class) || overlay.contains(class))
                    && (overlay.contains(class) || self.facts(*class).placed)
            })
            .collect();
        // A low-extract view owns no register of its own: it re-views its
        // source's. So it is available exactly when that source is — either
        // already in a register, or produced here.
        let available = self
            .classes
            .iter()
            .copied()
            .filter(|&class| {
                let source = self.facts(class).source;
                let facts = self.facts(source);
                facts.has_register(has_register)
                    || facts.hook_constant
                    || (source != class && demanded.contains(&source))
            })
            .collect();
        let mut views: HashMap<Id, Vec<Id>> = HashMap::new();
        for &class in &self.classes {
            let source = self.facts(class).source;
            if source != class && demanded.contains(&source) && !self.facts(source).has_values {
                views.entry(source).or_default().push(class);
            }
        }
        Policy {
            overlay,
            demanded,
            available,
            views,
        }
    }

    /// What each class resolves to under an assignment: its instance, an
    /// existing register, the instance performing it, or its views.
    fn states(
        &self,
        assignment: &RegionAssignment,
        policy: &Policy,
    ) -> Result<HashMap<Id, PbqpIselAlternative>, String> {
        let mut states = HashMap::new();
        for &class in &self.classes {
            let state = if let Some(&match_id) = assignment.tiles.get(&class) {
                PbqpIselAlternative::Tile { match_id }
            } else if !policy.demanded(class) || policy.available(class) {
                PbqpIselAlternative::NotDemanded
            } else if let Some(&match_id) = assignment.tiles.values().find(|&&match_id| {
                !self.materialized(policy, class) && self.matches[match_id].covers.contains(&class)
            }) {
                PbqpIselAlternative::CoveredBy { match_id }
            } else if policy.views.contains_key(&class) {
                PbqpIselAlternative::Deferred
            } else {
                return Err(format!(
                    "{:?} computes no demanded {:?}",
                    self.region,
                    self.facts(class).kind
                ));
            };
            states.insert(class, state);
        }
        Ok(states)
    }

    /// Check an assignment against the region's records, independently of how
    /// it was found. `guard_resolves` says whether a fused branch can name its
    /// operands; `nonzero_resolves` the same for a materialized condition.
    pub(crate) fn validate(
        &self,
        assignment: &RegionAssignment,
        has_register: HasRegister,
        guard_resolves: &dyn Fn(usize, usize) -> bool,
        nonzero_resolves: &dyn Fn(usize) -> bool,
    ) -> Result<(), String> {
        let invalid = |what: &str| Err(format!("{:?}: {what}", self.region));
        if assignment.controls.len() != self.controls.len() {
            return invalid("a control outcome has no realization");
        }
        for (index, (control, choice)) in self.controls.iter().zip(&assignment.controls).enumerate()
        {
            let legal = match (*choice, control.decided) {
                (ControlChoice::Decided(holds), Some(known)) => holds == known,
                (ControlChoice::Fused(guard), None) => {
                    guard < control.fused.len() && guard_resolves(index, guard)
                }
                (ControlChoice::Nonzero, None) => {
                    assignment
                        .tiles
                        .contains_key(&self.facts(control.condition).source)
                        || nonzero_resolves(index)
                }
                _ => false,
            };
            if !legal {
                return invalid("a control realization cannot read its operands");
            }
        }
        for (&class, &match_id) in &assignment.tiles {
            if self.matches.get(match_id).map(|matched| matched.root) != Some(class) {
                return invalid("an instance is selected at a class it does not root");
            }
        }

        let policy = self.policy(&assignment.controls, has_register);
        let states = self.states(assignment, &policy)?;
        for &match_id in assignment.tiles.values() {
            let matched = &self.matches[match_id];
            for (class, demand) in &matched.uses {
                let Some(state) = states.get(class) else {
                    continue;
                };
                if !demand.accepts(
                    match_id,
                    state,
                    &self.matches,
                    self.facts(*class).int.is_some(),
                    policy.available(*class),
                ) {
                    return invalid("an instance input has no compatible producer");
                }
            }
        }

        // One instance performs each effect, and a deferred source leaves every
        // view to an instance of its own.
        let mut owner: HashMap<Id, usize> = HashMap::new();
        for &match_id in assignment.tiles.values() {
            for &effect in &self.matches[match_id].effects {
                if owner.insert(effect, match_id).is_some_and(|other| {
                    other != match_id && self.matches[other].root != self.matches[match_id].root
                }) {
                    return invalid("two instances perform one effect");
                }
            }
        }
        for (class, state) in &states {
            if matches!(state, PbqpIselAlternative::Deferred)
                && policy.views[class]
                    .iter()
                    .any(|view| !assignment.tiles.contains_key(view))
            {
                return invalid("a deferred source has an uncomputed view");
            }
        }

        if super::emit::order_tiles(&self.matches, &assignment.tiles, |_| None).is_none() {
            return invalid("selected instances depend on each other in a cycle");
        }
        Ok(())
    }

    /// Drop the selected instances no obligation reaches: a demanded class, an
    /// input of an instance that is itself reached, an effect a demanded class
    /// is performed inside, or a view a deferred source leaves to it. A search
    /// answer may select instances nothing reads; they would be emitted.
    pub(crate) fn drop_unreached(
        &self,
        assignment: &mut RegionAssignment,
        has_register: HasRegister,
    ) {
        let policy = self.policy(&assignment.controls, has_register);
        let mut reached: HashSet<Id> = HashSet::new();
        let mut pending: Vec<Id> = self
            .classes
            .iter()
            .copied()
            // An instance that leaves a register for enclosed regions answers
            // to them as well.
            .filter(|class| policy.demanded(*class) || self.register_names.contains_key(class))
            .collect();
        while let Some(class) = pending.pop() {
            if !reached.insert(class) {
                continue;
            }
            match assignment.tiles.get(&class) {
                // Only a register input reads the instance of its class: an
                // immediate is folded and an effect is performed inside.
                Some(&match_id) => pending.extend(
                    self.matches[match_id]
                        .uses
                        .iter()
                        .filter(|(_, demand)| matches!(demand, ChildDemand::Register { .. }))
                        .map(|(input, _)| *input)
                        .filter(|input| assignment.tiles.contains_key(input)),
                ),
                None if policy.available(class) => {}
                None => {
                    pending.extend(
                        assignment
                            .tiles
                            .iter()
                            .filter(|(_, match_id)| {
                                self.matches[**match_id].covers.contains(&class)
                            })
                            .map(|(root, _)| *root),
                    );
                    pending.extend(policy.views.get(&class).into_iter().flatten().copied());
                }
            }
        }
        assignment.tiles.retain(|class, _| reached.contains(class));
    }

    /// The model cost of an assignment: every selected instance and control
    /// realization once.
    pub(crate) fn cost(&self, assignment: &RegionAssignment) -> u64 {
        // The results of one instance are emitted, and paid for, once.
        let mut instances = HashSet::new();
        let tiles: u64 = assignment
            .tiles
            .values()
            .map(|&match_id| &self.matches[match_id])
            .filter(|matched| instances.insert(matched.instance))
            .map(|matched| matched.cost)
            .sum();
        let controls: u64 = self
            .controls
            .iter()
            .zip(&assignment.controls)
            .map(|(control, choice)| match *choice {
                ControlChoice::Decided(_) => 0,
                ControlChoice::Fused(index) => control.fused[index].cost,
                ControlChoice::Nonzero => self.nonzero_cost,
            })
            .sum();
        tiles + controls
    }
}

impl ChildDemand {
    /// Whether the class this demand names answers it in `state`.
    pub(crate) fn accepts(
        &self,
        owner: usize,
        state: &PbqpIselAlternative,
        matches: &[PbqpIselMatch],
        constant: bool,
        available: bool,
    ) -> bool {
        match self {
            ChildDemand::Register { offsets, widths } => {
                let (offset, width) = match state {
                    PbqpIselAlternative::Tile { match_id } => (
                        matches[*match_id].result_view_offset,
                        matches[*match_id].result_width,
                    ),
                    // An already-available value lives in an ordinary offset-0
                    // register, at the width its own class carries.
                    _ => (0, None),
                };
                if offsets.as_slice() != [offset] {
                    return false;
                }
                // A direct operand read whole needs an exact-width definition. A
                // low-extract view may read the low demanded bits of a wider
                // definition.
                if let Some(width) = width
                    && widths.iter().any(|(demanded, low_extract)| {
                        *demanded != width && !(*low_extract && *demanded < width)
                    })
                {
                    return false;
                }
                match state {
                    PbqpIselAlternative::Tile { .. } => true,
                    PbqpIselAlternative::NotDemanded => available,
                    PbqpIselAlternative::CoveredBy { .. } | PbqpIselAlternative::Deferred => false,
                }
            }
            ChildDemand::Immediate => constant,
            ChildDemand::Effect => match state {
                PbqpIselAlternative::NotDemanded => true,
                PbqpIselAlternative::CoveredBy { match_id } => *match_id == owner,
                PbqpIselAlternative::Tile { .. } | PbqpIselAlternative::Deferred => false,
            },
            ChildDemand::None => true,
        }
    }
}
