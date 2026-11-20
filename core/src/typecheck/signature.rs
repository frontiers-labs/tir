//! Compiled carrier schemes for registered operations.
use crate::ty::{Kind, Terms, TypeArg, TypeId};
use tir_symbolic::term::ctor;

/// A kind or shape restriction on a quantified variable.
pub enum BinderKind {
    Data,
    Nat,
    Sym,
    Shapes(&'static [Pattern]),
}

/// One type expression in a compiled signature.
pub enum Pattern {
    Wildcard,
    Bound(usize),
    Nat(u64),
    NatSum(&'static [Pattern]),
    Constructor(&'static str, &'static str, &'static [Pattern]),
}

/// A declared operand or result field.
pub enum Port {
    Fixed(Pattern),
    Optional(Pattern),
    Many(Pattern),
}

/// A field of live operand or result types, after segment sizes have been checked.
#[derive(Clone, Copy)]
pub enum Argument<'a> {
    Fixed(TypeId),
    Optional(Option<TypeId>),
    Many(&'a [TypeId]),
}

/// A compiled signature shared by all instances of an operation.
pub struct Signature {
    pub binders: &'static [BinderKind],
    pub inputs: &'static [Port],
    pub outputs: &'static [Port],
}

impl Signature {
    /// Match the grouped ports and return each binder's value in declaration order.
    /// A binder occurring only in absent fields has no value.
    pub fn substitution(
        &self,
        terms: &Terms,
        inputs: &[Argument<'_>],
        outputs: &[Argument<'_>],
    ) -> Option<Vec<Option<TypeArg>>> {
        let mut call = Match::new(terms, self.binders);
        for (ports, args) in [(self.inputs, inputs), (self.outputs, outputs)] {
            if ports.len() != args.len() {
                return None;
            }
            for (port, &arg) in ports.iter().zip(args) {
                let matched = match (port, arg) {
                    (Port::Fixed(pattern), Argument::Fixed(id))
                    | (Port::Optional(pattern), Argument::Optional(Some(id))) => {
                        call.pattern(pattern, &TypeArg::Type(id))
                    }
                    (Port::Optional(_), Argument::Optional(None)) => true,
                    (Port::Many(pattern), Argument::Many(ids)) => ids
                        .iter()
                        .all(|&id| call.pattern(pattern, &TypeArg::Type(id))),
                    _ => false,
                };
                if !matched {
                    return None;
                }
            }
        }
        call.solve()
    }
}

struct Match<'a> {
    terms: &'a Terms,
    kinds: &'a [BinderKind],
    slots: Vec<Option<TypeArg>>,
    equations: Vec<(&'static [Pattern], u64)>,
}

impl<'a> Match<'a> {
    fn new(terms: &'a Terms, kinds: &'a [BinderKind]) -> Self {
        Self {
            terms,
            kinds,
            slots: vec![None; kinds.len()],
            equations: Vec::new(),
        }
    }

    fn pattern(&mut self, pattern: &'static Pattern, value: &TypeArg) -> bool {
        match pattern {
            Pattern::Wildcard => true,
            Pattern::Nat(n) => *value == TypeArg::Nat(*n),
            Pattern::Bound(index) => {
                let Some(kind) = self.kinds.get(*index) else {
                    return false;
                };
                if let Some(bound) = &self.slots[*index] {
                    return bound == value;
                }
                let valid = match (kind, value) {
                    (BinderKind::Shapes(shapes), _) => shapes.iter().any(|shape| {
                        // Shape alternatives are closed monomorphic patterns.
                        let mut candidate = Match::new(self.terms, &[]);
                        candidate.pattern(shape, value) && candidate.solve().is_some()
                    }),
                    (BinderKind::Data, TypeArg::Type(id)) => self.terms.kind(*id) == Kind::Data,
                    (BinderKind::Nat, TypeArg::Type(id)) => self.terms.kind(*id) == Kind::Nat,
                    (BinderKind::Nat, TypeArg::Nat(_)) | (BinderKind::Sym, TypeArg::Sym(_)) => true,
                    _ => false,
                };
                if valid {
                    self.slots[*index] = Some(value.clone());
                }
                valid
            }
            Pattern::NatSum(parts) => {
                let Some(n) = natural(self.terms, value) else {
                    return false;
                };
                self.equations.push((parts, n));
                true
            }
            Pattern::Constructor(dialect, name, patterns) => {
                let TypeArg::Type(id) = value else {
                    return false;
                };
                let term = self.terms.term(*id);
                let info = self.terms.ctor_info(term.ctor());
                if (info.dialect, info.name) != (*dialect, *name) {
                    return false;
                }
                let args = term.args();
                if (*dialect, *name) == ("ptr", "p")
                    && args.is_empty()
                    && matches!(patterns, [Pattern::Wildcard])
                {
                    return true;
                }
                patterns.len() == args.len()
                    && patterns
                        .iter()
                        .zip(args)
                        .all(|(pattern, value)| self.pattern(pattern, value))
            }
        }
    }

    fn solve(mut self) -> Option<Vec<Option<TypeArg>>> {
        let mut equations = std::mem::take(&mut self.equations);
        while !equations.is_empty() {
            let before = equations.len();
            let mut deferred = Vec::new();
            for (parts, target) in equations {
                let mut sum = Sum {
                    remaining: target,
                    unknown: None,
                    deferred: false,
                };
                sum.subtract(parts, self.terms, self.kinds, &self.slots)?;
                if sum.deferred {
                    deferred.push((parts, target));
                } else if let Some(index) = sum.unknown {
                    self.slots[index] = Some(TypeArg::Nat(sum.remaining));
                } else if sum.remaining != 0 {
                    return None;
                }
            }
            if deferred.len() == before {
                return None;
            }
            equations = deferred;
        }
        Some(self.slots)
    }
}

// TIR widths are u64. Subtracting known addends from the concrete target
// detects overflow and never invents a split between two unknown widths.
struct Sum {
    remaining: u64,
    unknown: Option<usize>,
    deferred: bool,
}

impl Sum {
    fn subtract(
        &mut self,
        parts: &[Pattern],
        terms: &Terms,
        kinds: &[BinderKind],
        slots: &[Option<TypeArg>],
    ) -> Option<()> {
        for part in parts {
            let n = match part {
                Pattern::Nat(n) => *n,
                Pattern::Bound(index) if matches!(kinds.get(*index), Some(BinderKind::Nat)) => {
                    match &slots[*index] {
                        Some(value) => natural(terms, value)?,
                        None => {
                            self.deferred |= self.unknown.replace(*index).is_some();
                            continue;
                        }
                    }
                }
                Pattern::NatSum(parts) => {
                    self.subtract(parts, terms, kinds, slots)?;
                    continue;
                }
                _ => return None,
            };
            self.remaining = self.remaining.checked_sub(n)?;
        }
        Some(())
    }
}

fn natural(terms: &Terms, arg: &TypeArg) -> Option<u64> {
    match arg {
        TypeArg::Nat(n) => Some(*n),
        TypeArg::Type(id) if terms.ctor(*id) == ctor::NAT_ADD => {
            let TypeArg::List(parts) = terms.arg(*id, 0)? else {
                return None;
            };
            parts
                .iter()
                .try_fold(0u64, |sum, part| sum.checked_add(natural(terms, part)?))
        }
        _ => None,
    }
}
