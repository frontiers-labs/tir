use std::collections::HashMap;
use std::fmt;

use crate::term::{CtorId, Kind, Param, Terms, TypeArg, TypeId, ctor};

/// A failed unification, its types spelled when the error was made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeError {
    Mismatch(String, String),
    WidthMismatch(String, String),
    /// The variable occurs in the type it would be bound to.
    Infinite(u32, String),
    InfiniteWidth(u32, String),
    InvalidStateField {
        resource: u64,
        field: u64,
    },
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeError::Mismatch(lhs, rhs) => write!(f, "type mismatch: {lhs} vs {rhs}"),
            TypeError::WidthMismatch(lhs, rhs) => write!(f, "width mismatch: {lhs} vs {rhs}"),
            TypeError::Infinite(var, ty) => write!(f, "infinite type: ${var} occurs in {ty}"),
            TypeError::InfiniteWidth(var, width) => {
                write!(f, "infinite width: ${var} occurs in {width}")
            }
            TypeError::InvalidStateField { resource, field } => {
                write!(f, "state resource {resource} has no field {field}")
            }
        }
    }
}

impl std::error::Error for TypeError {}

pub(crate) fn semantic(terms: &Terms, ctor: CtorId, args: Vec<TypeArg>) -> TypeArg {
    TypeArg::Type(
        terms
            .intern(ctor, args)
            .expect("a well-formed semantic type"),
    )
}

pub(crate) fn bits(terms: &Terms, width: u64) -> TypeArg {
    TypeArg::Type(terms.int(width))
}

fn is_width(terms: &Terms, arg: &TypeArg) -> bool {
    match arg {
        TypeArg::Nat(_) => true,
        TypeArg::Type(id) => terms.kind(*id) == Kind::Nat,
        _ => false,
    }
}

/// Unifies semantic types and widths of one store. Both kinds of inference
/// variable are [`TypeArg::Var`]s of one numbering.
pub struct TypeUnifier<'t> {
    terms: &'t Terms,
    next_var: u32,
    bound: HashMap<u32, TypeArg>,
}

impl<'t> TypeUnifier<'t> {
    pub fn new(terms: &'t Terms) -> Self {
        Self {
            terms,
            next_var: 0,
            bound: HashMap::new(),
        }
    }

    pub fn terms(&self) -> &'t Terms {
        self.terms
    }

    pub(crate) fn fresh_var(&mut self) -> TypeArg {
        let var = self.next_var;
        self.next_var += 1;
        TypeArg::Var(var)
    }

    pub(crate) fn fresh_bits(&mut self) -> TypeArg {
        let width = self.fresh_var();
        semantic(self.terms, ctor::INT, vec![width])
    }

    pub(crate) fn fresh_float(&mut self) -> TypeArg {
        let exponent = self.fresh_var();
        let mantissa = self.fresh_var();
        semantic(self.terms, ctor::FLOAT, vec![exponent, mantissa])
    }

    pub fn unify(&mut self, lhs: &TypeArg, rhs: &TypeArg) -> Result<(), TypeError> {
        let lhs = self.walk(lhs);
        let rhs = self.walk(rhs);
        if lhs == rhs {
            return Ok(());
        }
        if is_width(self.terms, &lhs) || is_width(self.terms, &rhs) {
            return self.unify_width(&lhs, &rhs);
        }
        match (&lhs, &rhs) {
            (TypeArg::Var(var), ty) | (ty, TypeArg::Var(var)) => self.bind(*var, ty),
            (TypeArg::Type(a), TypeArg::Type(b)) => self.unify_terms(*a, *b),
            (TypeArg::List(a), TypeArg::List(b)) if a.len() == b.len() => {
                a.iter().zip(b).try_for_each(|(a, b)| self.unify(a, b))
            }
            _ => Err(TypeError::Mismatch(self.show(&lhs), self.show(&rhs))),
        }
    }

    fn unify_terms(&mut self, a: TypeId, b: TypeId) -> Result<(), TypeError> {
        let store = self.terms;
        let (lhs, rhs) = (store.term(a), store.term(b));
        let raw_view = |raw: CtorId, other: CtorId| {
            raw == ctor::RAW && matches!(other, ctor::INT | ctor::FLOAT)
        };
        if lhs.ctor() == rhs.ctor() && lhs.args().len() == rhs.args().len() {
            return lhs
                .args()
                .iter()
                .zip(rhs.args())
                .try_for_each(|(a, b)| self.unify(a, b));
        }
        // A variable in a trailing optional argument also stands for its
        // absence: `ptr.p<T>` takes `ptr.p` and leaves `T` free.
        if lhs.ctor() == rhs.ctor() {
            let (short, long) = match lhs.args().len() < rhs.args().len() {
                true => (&lhs, &rhs),
                false => (&rhs, &lhs),
            };
            let optional_tail = matches!(
                store.ctor_info(lhs.ctor()).params.last(),
                Some((_, Param::Opt(_)))
            );
            if optional_tail
                && long.args().len() == short.args().len() + 1
                && matches!(self.walk(&long.args()[short.args().len()]), TypeArg::Var(_))
            {
                return short
                    .args()
                    .iter()
                    .zip(long.args())
                    .try_for_each(|(a, b)| self.unify(a, b));
            }
        }
        if raw_view(lhs.ctor(), rhs.ctor()) || raw_view(rhs.ctor(), lhs.ctor()) {
            let carrier = |id| store.carrier(id).expect("a scalar carrier");
            return self.unify_width(&carrier(a), &carrier(b));
        }
        let (a, b) = (TypeArg::Type(a), TypeArg::Type(b));
        Err(TypeError::Mismatch(self.show(&a), self.show(&b)))
    }

    fn unify_width(&mut self, lhs: &TypeArg, rhs: &TypeArg) -> Result<(), TypeError> {
        let lhs = self.resolve(lhs);
        let rhs = self.resolve(rhs);
        if lhs == rhs {
            return Ok(());
        }
        match (&lhs, &rhs) {
            (TypeArg::Var(var), width) | (width, TypeArg::Var(var)) => self.bind(*var, width),
            (TypeArg::Type(a), TypeArg::Type(b))
                if self.terms.ctor(*a) == ctor::NAT_ADD && self.terms.ctor(*b) == ctor::NAT_ADD =>
            {
                let (a, b) = (self.terms.term(*a), self.terms.term(*b));
                match (a.args(), b.args()) {
                    ([TypeArg::List(a)], [TypeArg::List(b)]) if a.len() == b.len() => a
                        .iter()
                        .zip(b)
                        .try_for_each(|(a, b)| self.unify_width(a, b)),
                    _ => Err(self.width_mismatch(&lhs, &rhs)),
                }
            }
            _ => Err(self.width_mismatch(&lhs, &rhs)),
        }
    }

    fn show(&self, arg: &TypeArg) -> String {
        self.terms.display_arg(&self.resolve(arg))
    }

    fn width_mismatch(&self, lhs: &TypeArg, rhs: &TypeArg) -> TypeError {
        let show = |arg| self.terms.display_arg(arg);
        TypeError::WidthMismatch(show(lhs), show(rhs))
    }

    fn bind(&mut self, var: u32, value: &TypeArg) -> Result<(), TypeError> {
        if self.occurs(var, value) {
            let value = self.resolve(value);
            let shown = self.terms.display_arg(&value);
            return Err(match is_width(self.terms, &value) {
                true => TypeError::InfiniteWidth(var, shown),
                false => TypeError::Infinite(var, shown),
            });
        }
        self.bound.insert(var, value.clone());
        Ok(())
    }

    fn walk(&self, arg: &TypeArg) -> TypeArg {
        match arg {
            TypeArg::Var(var) => match self.bound.get(var) {
                Some(bound) => self.walk(bound),
                None => arg.clone(),
            },
            _ => arg.clone(),
        }
    }

    fn occurs(&self, var: u32, arg: &TypeArg) -> bool {
        match arg {
            TypeArg::Var(other) => {
                *other == var || self.bound.get(other).is_some_and(|b| self.occurs(var, b))
            }
            TypeArg::Type(id) => self
                .terms
                .term(*id)
                .args()
                .iter()
                .any(|arg| self.occurs(var, arg)),
            TypeArg::List(items) => items.iter().any(|item| self.occurs(var, item)),
            TypeArg::Nat(_) | TypeArg::Sym(_) | TypeArg::Lam(..) => false,
        }
    }

    /// `arg` with every bound variable replaced, sums of widths folded.
    pub fn resolve(&self, arg: &TypeArg) -> TypeArg {
        match arg {
            TypeArg::Var(var) => match self.bound.get(var) {
                Some(bound) => self.resolve(bound),
                None => arg.clone(),
            },
            TypeArg::Type(id) => {
                let term = self.terms.term(*id);
                let args: Vec<TypeArg> = term.args().iter().map(|a| self.resolve(a)).collect();
                if args == term.args() {
                    return arg.clone();
                }
                match (term.ctor(), args.as_slice()) {
                    (ctor::NAT_ADD, [TypeArg::List(parts)]) => parts
                        .iter()
                        .cloned()
                        .fold(TypeArg::Nat(0), |sum, part| self.terms.nat_add(sum, part)),
                    _ => semantic(self.terms, term.ctor(), args),
                }
            }
            TypeArg::List(items) => TypeArg::List(items.iter().map(|i| self.resolve(i)).collect()),
            TypeArg::Nat(_) | TypeArg::Sym(_) | TypeArg::Lam(..) => arg.clone(),
        }
    }
}
