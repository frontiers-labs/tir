//! Pure substitution operations used by Hindley-Milner inference.

/// An inference variable, distinct from nominal and rigid type identities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct VarId(pub usize);

/// A nominal type constructor identity assigned by elaboration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConId(pub usize);

/// A universally quantified variable checked rigidly inside its definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RigidId(pub usize);

/// Monotypes, with exact function arity.
#[derive(Debug)]
pub enum Ty {
    Var(VarId),
    Rigid(RigidId),
    Con(ConId, Box<TyList>),
    Fun(Box<TyList>, Box<Ty>),
}

/// A strictly positive list of recursive type children, also used by extraction.
#[derive(Debug)]
pub enum TyList {
    Nil,
    Cons(Box<Ty>, Box<TyList>),
}

impl Clone for Ty {
    fn clone(&self) -> Self {
        clone_type(self)
    }
}
impl Clone for TyList {
    fn clone(&self) -> Self {
        clone_types(self)
    }
}
impl PartialEq for Ty {
    fn eq(&self, other: &Self) -> bool {
        equal_type(self, other)
    }
}
impl PartialEq for TyList {
    fn eq(&self, other: &Self) -> bool {
        equal_types(self, other)
    }
}
impl Eq for Ty {}
impl Eq for TyList {}

fn clone_type(ty: &Ty) -> Ty {
    match ty {
        Ty::Var(id) => Ty::Var(*id),
        Ty::Rigid(id) => Ty::Rigid(*id),
        Ty::Con(id, children) => Ty::Con(*id, Box::new(clone_types(children))),
        Ty::Fun(arguments, result) => Ty::Fun(
            Box::new(clone_types(arguments)),
            Box::new(clone_type(result)),
        ),
    }
}
fn clone_types(types: &TyList) -> TyList {
    match types {
        TyList::Nil => TyList::Nil,
        TyList::Cons(head, tail) => {
            TyList::Cons(Box::new(clone_type(head)), Box::new(clone_types(tail)))
        }
    }
}
fn equal_type(left: &Ty, right: &Ty) -> bool {
    match (left, right) {
        (Ty::Var(a), Ty::Var(b)) => a == b,
        (Ty::Rigid(a), Ty::Rigid(b)) => a == b,
        (Ty::Con(a, xs), Ty::Con(b, ys)) => a == b && equal_types(xs, ys),
        (Ty::Fun(xs, x), Ty::Fun(ys, y)) => equal_types(xs, ys) && equal_type(x, y),
        _ => false,
    }
}
fn equal_types(left: &TyList, right: &TyList) -> bool {
    match (left, right) {
        (TyList::Nil, TyList::Nil) => true,
        (TyList::Cons(x, xs), TyList::Cons(y, ys)) => equal_type(x, y) && equal_types(xs, ys),
        _ => false,
    }
}

/// A monotype with the listed inference variables universally quantified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scheme {
    pub quantified: Vec<VarId>,
    pub ty: Ty,
}

impl Scheme {
    /// Make a monomorphic environment entry.
    pub fn mono(ty: Ty) -> Self {
        Self {
            quantified: Vec::new(),
            ty,
        }
    }
}

/// A finite, simultaneous substitution.
#[derive(Debug, Default)]
pub struct Substitution {
    pub bindings: Vec<(VarId, Ty)>,
}

/// Unification failures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnifyError {
    Occurs(VarId, Ty),
    Mismatch(Ty, Ty),
    Arity(usize, usize),
    FreshExhausted,
}

impl Substitution {
    /// Apply once, simultaneously, to the free variables of a type.
    pub fn apply(&self, ty: &Ty) -> Ty {
        match ty {
            Ty::Var(id) => {
                for (variable, replacement) in &self.bindings {
                    if variable == id {
                        return replacement.clone();
                    }
                }
                ty.clone()
            }
            Ty::Rigid(_) => ty.clone(),
            Ty::Con(id, children) => Ty::Con(*id, Box::new(self.apply_list(children))),
            Ty::Fun(arguments, result) => Ty::Fun(
                Box::new(self.apply_list(arguments)),
                Box::new(self.apply(result)),
            ),
        }
    }

    fn apply_list(&self, types: &TyList) -> TyList {
        match types {
            TyList::Nil => TyList::Nil,
            TyList::Cons(head, tail) => {
                TyList::Cons(Box::new(self.apply(head)), Box::new(self.apply_list(tail)))
            }
        }
    }

    /// Apply without substituting quantified variables.
    pub fn apply_scheme(&self, scheme: &Scheme) -> Scheme {
        let mut free = Self::default();
        for (id, ty) in &self.bindings {
            if !scheme.quantified.contains(id) {
                free.bindings.push((*id, ty.clone()));
            }
        }
        Scheme {
            quantified: scheme.quantified.clone(),
            ty: free.apply(&scheme.ty),
        }
    }

    /// Compose `self` after `older`.
    pub fn compose(&self, older: &Self) -> Self {
        let mut result = Self::default();
        for (id, ty) in &older.bindings {
            result.bindings.push((*id, self.apply(ty)));
        }
        for (id, ty) in &self.bindings {
            let mut present = false;
            for (old, _) in &older.bindings {
                if old == id {
                    present = true;
                }
            }
            if !present {
                result.bindings.push((*id, ty.clone()));
            }
        }
        result
    }
}

/// Collect distinct free inference variables in traversal order.
pub fn free_variables(ty: &Ty) -> Vec<VarId> {
    let mut result = Vec::new();
    collect_free(ty, &mut result);
    result
}

fn collect_free(ty: &Ty, result: &mut Vec<VarId>) {
    match ty {
        Ty::Var(id) => {
            if !result.contains(id) {
                result.push(*id);
            }
        }
        Ty::Rigid(_) => {}
        Ty::Con(_, children) => collect_free_list(children, result),
        Ty::Fun(args, output) => {
            collect_free_list(args, result);
            collect_free(output, result);
        }
    }
}

fn collect_free_list(types: &TyList, result: &mut Vec<VarId>) {
    match types {
        TyList::Nil => {}
        TyList::Cons(head, tail) => {
            collect_free(head, result);
            collect_free_list(tail, result);
        }
    }
}

/// Compute a most general unifier, rejecting infinite types.
pub fn unify(left: &Ty, right: &Ty) -> Result<Substitution, UnifyError> {
    if left == right {
        return Ok(Substitution::default());
    }
    match (left, right) {
        (Ty::Var(id), ty) | (ty, Ty::Var(id)) => {
            if free_variables(ty).contains(id) {
                return Err(UnifyError::Occurs(*id, ty.clone()));
            }
            Ok(Substitution {
                bindings: vec![(*id, ty.clone())],
            })
        }
        (Ty::Con(a, xs), Ty::Con(b, ys)) if a == b => unify_many(xs, ys),
        (Ty::Fun(xs, x), Ty::Fun(ys, y)) => {
            let args = unify_many(xs, ys)?;
            let output = unify(&args.apply(x), &args.apply(y))?;
            Ok(output.compose(&args))
        }
        _ => Err(UnifyError::Mismatch(left.clone(), right.clone())),
    }
}

fn unify_many(left: &TyList, right: &TyList) -> Result<Substitution, UnifyError> {
    match (left, right) {
        (TyList::Nil, TyList::Nil) => Ok(Substitution::default()),
        (TyList::Cons(x, xs), TyList::Cons(y, ys)) => {
            let head = unify(x, y)?;
            let tail = unify_many(&head.apply_list(xs), &head.apply_list(ys))?;
            Ok(tail.compose(&head))
        }
        _ => Err(UnifyError::Arity(list_length(left), list_length(right))),
    }
}

fn list_length(types: &TyList) -> usize {
    match types {
        TyList::Nil => 0,
        TyList::Cons(_, tail) => 1 + list_length(tail),
    }
}

/// Quantify precisely the variables absent from the environment's free variables.
pub fn generalize(environment: &[Scheme], ty: &Ty) -> Scheme {
    let mut quantified = Vec::new();
    let variables = free_variables(ty);
    for id in &variables {
        if !free_in_environment(*id, environment) {
            quantified.push(*id);
        }
    }
    Scheme {
        quantified,
        ty: ty.clone(),
    }
}

fn free_in_environment(id: VarId, environment: &[Scheme]) -> bool {
    for scheme in environment {
        if !scheme.quantified.contains(&id) && free_variables(&scheme.ty).contains(&id) {
            return true;
        }
    }
    false
}

/// Allocate an inference variable without wrapping its identity.
pub fn fresh(next: &mut usize) -> Result<Ty, UnifyError> {
    let id = *next;
    *next = next.checked_add(1).ok_or(UnifyError::FreshExhausted)?;
    Ok(Ty::Var(VarId(id)))
}

/// Replace each quantified variable with a fresh inference variable.
pub fn instantiate(scheme: &Scheme, next: &mut usize) -> Result<Ty, UnifyError> {
    let mut substitution = Substitution::default();
    for id in &scheme.quantified {
        substitution.bindings.push((*id, fresh(next)?));
    }
    Ok(substitution.apply(&scheme.ty))
}
