use std::collections::{HashMap, HashSet};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Copy, Hash)]
pub struct TypeVar(u32);

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    // Core (ground) types
    String,
    Integer,
    Bits(u16),
    /// `bits<expr>` whose width is a constant expression over ISA parameters
    /// (e.g. `bits<log2Ceil(self.XLEN)>`); resolved to `Bits` per ISA before
    /// type checking and code generation.
    BitsExpr(Box<crate::ast::Expr>),
    Struct(String),

    // HM types
    Var(TypeVar),
    Fn(Box<Type>, Box<Type>),
    Con(String, Vec<Type>),
}

#[derive(Debug, Clone, Default)]
pub struct Substitution {
    map: HashMap<TypeVar, Type>,
}

/// A plymorphic type
#[derive(Debug, Clone)]
pub struct TypeScheme {
    /// Quantified variables
    pub vars: Vec<TypeVar>,
    /// Body
    pub ty: Type,
}

#[derive(Debug, Clone, Default)]
pub struct TypeEnv {
    bindings: HashMap<String, TypeScheme>,
    parent: Option<Box<TypeEnv>>,
}

impl Type {
    /// Collect all free type variables.
    pub fn free_vars(&self) -> HashSet<TypeVar> {
        match self {
            Type::Var(v) => std::iter::once(*v).collect(),
            Type::Fn(a, b) => {
                let mut s = a.free_vars();
                s.extend(b.free_vars());
                s
            }
            Type::Con(_, args) => args.iter().flat_map(|a| a.free_vars()).collect(),
            // Ground types have no free variables
            _ => HashSet::new(),
        }
    }

    /// Apply a substitution to this type.
    pub fn apply(&self, subst: &Substitution) -> Type {
        match self {
            Type::Var(v) => subst.get(v),
            Type::Fn(a, b) => Type::Fn(Box::new(a.apply(subst)), Box::new(b.apply(subst))),
            Type::Con(name, args) => {
                Type::Con(name.clone(), args.iter().map(|a| a.apply(subst)).collect())
            }
            other => other.clone(),
        }
    }

    /// Check whether a type variable occurs in this type (for occurs check).
    pub fn occurs(&self, v: TypeVar) -> bool {
        match self {
            Type::Var(u) => *u == v,
            Type::Fn(a, b) => a.occurs(v) || b.occurs(v),
            Type::Con(_, args) => args.iter().any(|a| a.occurs(v)),
            _ => false,
        }
    }
}

impl Substitution {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, v: TypeVar, ty: Type) {
        self.map.insert(v, ty);
    }

    pub fn get(&self, v: &TypeVar) -> Type {
        match self.map.get(v) {
            Some(ty) => ty.clone(),
            None => Type::Var(*v),
        }
    }

    pub fn compose(mut self, other: &Self) -> Self {
        self.map.values_mut().for_each(|ty| *ty = ty.apply(other));
        other.map.iter().for_each(|(v, ty)| {
            self.map.entry(*v).or_insert_with(|| ty.clone());
        });
        self
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Allocates fresh type variables with monotonically increasing IDs.
#[derive(Debug, Clone, Default)]
pub struct TypeVarGen(u32);

impl TypeVarGen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn fresh(&mut self) -> TypeVar {
        let v = TypeVar(self.0);
        self.0 += 1;
        v
    }
}

impl TypeScheme {
    /// Monomorphic scheme - no quantification
    pub fn mono(ty: Type) -> Self {
        TypeScheme { vars: vec![], ty }
    }

    pub fn free_vars(&self) -> HashSet<TypeVar> {
        let mut fv = self.ty.free_vars();
        for v in &self.vars {
            fv.remove(v);
        }
        fv
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UnifyError {
    /// Two ground types that cannot be made equal.
    Mismatch(Type, Type),
    /// A type variable would need to contain itself.
    OccursCheck(TypeVar, Type),
}

impl fmt::Display for UnifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnifyError::Mismatch(a, b) => write!(f, "type mismatch: {a:?} vs {b:?}"),
            UnifyError::OccursCheck(v, t) => {
                write!(f, "occurs check: {v:?} appears inside {t:?}")
            }
        }
    }
}

/// Unify two types, returning a substitution that makes them equal.
///
/// `Con("bits", [arg])` and `Bits(n)` are treated as the same family:
/// `Bits(n)` acts as the ground form of a width argument, so
/// `unify(Con("bits", [Var(tv)]), Bits(32))` produces `{tv → Bits(32)}`.
pub fn unify(t1: &Type, t2: &Type) -> Result<Substitution, UnifyError> {
    match (t1, t2) {
        // Identical variables: trivially equal.
        (Type::Var(v), Type::Var(u)) if v == u => Ok(Substitution::new()),

        // Left variable: bind it (with occurs check).
        (Type::Var(v), t) => {
            if t.occurs(*v) {
                return Err(UnifyError::OccursCheck(*v, t.clone()));
            }
            let mut s = Substitution::new();
            s.insert(*v, t.clone());
            Ok(s)
        }

        // Right variable: bind it (with occurs check).
        (t, Type::Var(v)) => {
            if t.occurs(*v) {
                return Err(UnifyError::OccursCheck(*v, t.clone()));
            }
            let mut s = Substitution::new();
            s.insert(*v, t.clone());
            Ok(s)
        }

        // Ground type equality.
        (Type::Integer, Type::Integer) | (Type::String, Type::String) => Ok(Substitution::new()),
        (Type::Struct(a), Type::Struct(b)) if a == b => Ok(Substitution::new()),

        // Concrete bits types: equal iff same width.
        (Type::Bits(n), Type::Bits(m)) => {
            if n == m {
                Ok(Substitution::new())
            } else {
                Err(UnifyError::Mismatch(t1.clone(), t2.clone()))
            }
        }

        // Bridge: Con("bits", [arg]) ↔ Bits(n).
        // By recursing into unify(arg, Bits(n)), the Var arm handles the common case
        // Con("bits", [Var(tv)]) ↔ Bits(32) → {tv → Bits(32)}.
        (Type::Con(name, args), Type::Bits(n)) if name == "bits" && args.len() == 1 => {
            unify(&args[0], &Type::Bits(*n))
        }
        (Type::Bits(n), Type::Con(name, args)) if name == "bits" && args.len() == 1 => {
            unify(&Type::Bits(*n), &args[0])
        }

        // Constructor types: names and arity must match; unify args pairwise.
        (Type::Con(n1, args1), Type::Con(n2, args2)) => {
            if n1 != n2 || args1.len() != args2.len() {
                return Err(UnifyError::Mismatch(t1.clone(), t2.clone()));
            }
            let mut subst = Substitution::new();
            for (a1, a2) in args1.iter().zip(args2.iter()) {
                let s = unify(&a1.apply(&subst), &a2.apply(&subst))?;
                subst = subst.compose(&s);
            }
            Ok(subst)
        }

        // Function types: unify domain, then codomain under the resulting substitution.
        (Type::Fn(a1, b1), Type::Fn(a2, b2)) => {
            let s1 = unify(a1, a2)?;
            let s2 = unify(&b1.apply(&s1), &b2.apply(&s1))?;
            Ok(s1.compose(&s2))
        }

        _ => Err(UnifyError::Mismatch(t1.clone(), t2.clone())),
    }
}

impl TypeEnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enter_scope(self) -> Self {
        TypeEnv {
            bindings: HashMap::new(),
            parent: Some(Box::new(self)),
        }
    }

    pub fn exit_scope(self) -> Option<Self> {
        self.parent.map(|p| *p)
    }

    pub fn bind(&mut self, name: impl Into<String>, scheme: TypeScheme) {
        self.bindings.insert(name.into(), scheme);
    }

    pub fn get(&self, name: impl AsRef<str>) -> Option<&TypeScheme> {
        self.bindings
            .get(name.as_ref())
            .or_else(|| self.parent.as_ref().and_then(|p| p.get(name)))
    }

    pub fn free_vars(&self) -> HashSet<TypeVar> {
        let mut fv: HashSet<TypeVar> = self.bindings.values().flat_map(|s| s.free_vars()).collect();
        if let Some(parent) = &self.parent {
            fv.extend(parent.free_vars());
        }
        fv
    }
}
