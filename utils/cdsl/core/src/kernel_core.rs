//! Pure, formally verified CDSL v3 kernel core.
//!
//! Extracted to Lean 4 via Hax and proved to satisfy `CdslV3Spec.KernelSpec`.
//! `prop_elim_allowed` is called directly by `check.rs::check_prop_elim`.

pub enum Ty {
    Int,
    Bool,
    Proof,
    Opt(Box<Ty>),
    Arr(Box<Ty>, Box<Ty>),
}

pub enum TyList {
    Nil,
    Cons(Ty, Box<TyList>),
}

pub fn ty_clone(t: &Ty) -> Ty {
    match t {
        Ty::Int => Ty::Int,
        Ty::Bool => Ty::Bool,
        Ty::Proof => Ty::Proof,
        Ty::Opt(elem) => Ty::Opt(Box::new(ty_clone(elem))),
        Ty::Arr(dom, cod) => Ty::Arr(Box::new(ty_clone(dom)), Box::new(ty_clone(cod))),
    }
}

pub fn ty_eq(a: &Ty, b: &Ty) -> bool {
    match a {
        Ty::Int => matches!(b, Ty::Int),
        Ty::Bool => matches!(b, Ty::Bool),
        Ty::Proof => matches!(b, Ty::Proof),
        Ty::Opt(ae) => match b {
            Ty::Opt(be) => ty_eq(ae, be),
            _ => false,
        },
        Ty::Arr(ad, ac) => match b {
            Ty::Arr(bd, bc) => ty_eq(ad, bd) && ty_eq(ac, bc),
            _ => false,
        },
    }
}

pub fn ty_list_clone(xs: &TyList) -> TyList {
    match xs {
        TyList::Nil => TyList::Nil,
        TyList::Cons(head, tail) => TyList::Cons(ty_clone(head), Box::new(ty_list_clone(tail))),
    }
}

pub fn ctx_get(ctx: &TyList, idx: usize) -> Option<Ty> {
    match ctx {
        TyList::Nil => None,
        TyList::Cons(head, tail) => {
            if idx == 0 {
                Some(ty_clone(head))
            } else {
                ctx_get(tail, idx - 1)
            }
        }
    }
}

pub fn ty_is_prop(t: &Ty) -> bool {
    match t {
        Ty::Proof => true,
        Ty::Arr(_, cod) => ty_is_prop(cod),
        Ty::Int | Ty::Bool | Ty::Opt(_) => false,
    }
}

/// Rocq's `Prop`-elimination guard: a `Prop` scrutinee may only be eliminated
/// to produce a `Prop` result, never a computational `Type` value.
#[inline]
pub fn prop_elim_allowed(scrut_is_prop: bool, res_is_prop: bool) -> bool {
    if scrut_is_prop { res_is_prop } else { true }
}

pub enum Term {
    Var(usize),
    LitInt(i64),
    LitBool(bool),
    Add(Box<Term>, Box<Term>),
    Lam(Ty, Box<Term>),
    App(Box<Term>, Box<Term>),
    LetE(Ty, Box<Term>, Box<Term>),
    NoneE(Ty),
    SomeE(Box<Term>),
    MatchOpt(Box<Term>, Box<Term>, Box<Term>),
    MkProof,
    MatchProof(Box<Term>, Box<Term>),
}

pub fn infer_type(ctx: &TyList, t: &Term) -> Option<Ty> {
    match t {
        Term::Var(i) => ctx_get(ctx, *i),
        Term::LitInt(_) => Some(Ty::Int),
        Term::LitBool(_) => Some(Ty::Bool),
        Term::Add(a, b) => match infer_type(ctx, a) {
            Some(ta) => match &ta {
                Ty::Int => match infer_type(ctx, b) {
                    Some(tb) => match &tb {
                        Ty::Int => Some(Ty::Int),
                        _ => None,
                    },
                    None => None,
                },
                _ => None,
            },
            None => None,
        },
        Term::Lam(dom, body) => {
            let ext = TyList::Cons(ty_clone(dom), Box::new(ty_list_clone(ctx)));
            match infer_type(&ext, body) {
                Some(cod) => Some(Ty::Arr(Box::new(ty_clone(dom)), Box::new(cod))),
                None => None,
            }
        }
        Term::App(f, a) => match infer_type(ctx, f) {
            Some(tf) => match &tf {
                Ty::Arr(dom, cod) => match infer_type(ctx, a) {
                    Some(ta) => {
                        if ty_eq(&ta, dom) {
                            Some(ty_clone(cod))
                        } else {
                            None
                        }
                    }
                    None => None,
                },
                _ => None,
            },
            None => None,
        },
        Term::LetE(ann, rhs, body) => match infer_type(ctx, rhs) {
            Some(tr) => {
                if ty_eq(&tr, ann) {
                    let ext = TyList::Cons(ty_clone(ann), Box::new(ty_list_clone(ctx)));
                    infer_type(&ext, body)
                } else {
                    None
                }
            }
            None => None,
        },
        Term::NoneE(elem_ty) => Some(Ty::Opt(Box::new(ty_clone(elem_ty)))),
        Term::SomeE(v) => match infer_type(ctx, v) {
            Some(elem_ty) => Some(Ty::Opt(Box::new(elem_ty))),
            None => None,
        },
        Term::MatchOpt(s, n, c) => match infer_type(ctx, s) {
            Some(ts) => match &ts {
                Ty::Opt(elem_ty) => {
                    let ext = TyList::Cons(ty_clone(elem_ty), Box::new(ty_list_clone(ctx)));
                    match infer_type(ctx, n) {
                        Some(tn) => match infer_type(&ext, c) {
                            Some(tc) => {
                                if ty_eq(&tn, &tc) {
                                    Some(ty_clone(&tn))
                                } else {
                                    None
                                }
                            }
                            None => None,
                        },
                        None => None,
                    }
                }
                _ => None,
            },
            None => None,
        },
        Term::MkProof => Some(Ty::Proof),
        Term::MatchProof(s, b) => match infer_type(ctx, s) {
            Some(ts) => match &ts {
                Ty::Proof => match infer_type(ctx, b) {
                    Some(res) => {
                        if prop_elim_allowed(true, ty_is_prop(&res)) {
                            Some(ty_clone(&res))
                        } else {
                            None
                        }
                    }
                    None => None,
                },
                _ => None,
            },
            None => None,
        },
    }
}

pub enum MlExpr {
    Var(usize),
    LitInt(i64),
    LitBool(bool),
    Add(Box<MlExpr>, Box<MlExpr>),
    Lam(Box<MlExpr>),
    App(Box<MlExpr>, Box<MlExpr>),
    LetE(Box<MlExpr>, Box<MlExpr>),
    None,
    Some(Box<MlExpr>),
    MatchOpt(Box<MlExpr>, Box<MlExpr>, Box<MlExpr>),
    Erased,
}

pub fn erase_term(ctx: &TyList, t: &Term) -> MlExpr {
    match infer_type(ctx, t) {
        Some(ty) => {
            if ty_is_prop(&ty) {
                MlExpr::Erased
            } else {
                match t {
                    Term::Var(i) => MlExpr::Var(*i),
                    Term::LitInt(n) => MlExpr::LitInt(*n),
                    Term::LitBool(b) => MlExpr::LitBool(*b),
                    Term::Add(a, b) => {
                        MlExpr::Add(Box::new(erase_term(ctx, a)), Box::new(erase_term(ctx, b)))
                    }
                    Term::Lam(dom, body) => {
                        let ext = TyList::Cons(ty_clone(dom), Box::new(ty_list_clone(ctx)));
                        MlExpr::Lam(Box::new(erase_term(&ext, body)))
                    }
                    Term::App(f, a) => {
                        MlExpr::App(Box::new(erase_term(ctx, f)), Box::new(erase_term(ctx, a)))
                    }
                    Term::LetE(ann, rhs, body) => {
                        let ext = TyList::Cons(ty_clone(ann), Box::new(ty_list_clone(ctx)));
                        MlExpr::LetE(
                            Box::new(erase_term(ctx, rhs)),
                            Box::new(erase_term(&ext, body)),
                        )
                    }
                    Term::NoneE(_) => MlExpr::None,
                    Term::SomeE(v) => MlExpr::Some(Box::new(erase_term(ctx, v))),
                    Term::MatchOpt(s, n, c) => match infer_type(ctx, s) {
                        Some(ts) => match &ts {
                            Ty::Opt(elem_ty) => {
                                let ext =
                                    TyList::Cons(ty_clone(elem_ty), Box::new(ty_list_clone(ctx)));
                                MlExpr::MatchOpt(
                                    Box::new(erase_term(ctx, s)),
                                    Box::new(erase_term(ctx, n)),
                                    Box::new(erase_term(&ext, c)),
                                )
                            }
                            _ => MlExpr::Erased,
                        },
                        None => MlExpr::Erased,
                    },
                    Term::MkProof | Term::MatchProof(_, _) => MlExpr::Erased,
                }
            }
        }
        None => MlExpr::Erased,
    }
}
