//! Trusted Kernel Bidirectional Typechecker.
//!
//! Enforces:
//!   1. Sound typing of lambda calculus + parameterized inductives + match + records.
//!   2. Rocq's `Prop` elimination restriction: delegates directly to the formally
//!      verified `kernel_core::prop_elim_allowed` (proved in Lean 4 via Hax).
//!   3. ZERO string-matching on any library or domain name!
//!   4. Extract declarations are stored in an auxiliary table and NEVER read
//!      by the typing rules.

use std::{hash::BuildHasherDefault, rc::Rc};

use crate::{
    core::{
        Closure, Decl, Env, ExtractSpec, ExtractTarget, FxHashMap, IndDecl, Interner, Lit, Neutral,
        Pattern, PrimOp, Sort, Sym, Term, Value, env_extend, pp_term,
    },
    error::Error,
    kernel_core,
    nbe::{self, Globals},
};

pub type OpaqueTypeInfo = Rc<(Vec<(Sym, Term)>, Sort)>;

#[derive(Clone, Debug)]
pub struct Signature {
    pub interner: Interner,
    pub sym_int: Sym,
    pub sym_bool: Sym,
    pub sym_str: Sym,
    pub sym_unit: Sym,
    pub sym_wild: Sym,
    pub val_int: Rc<Value>,
    pub val_bool: Rc<Value>,
    pub val_str: Rc<Value>,
    pub val_unit: Rc<Value>,
    pub val_sort_type: Rc<Value>,
    pub val_sort_prop: Rc<Value>,
    pub inds: FxHashMap<Sym, Rc<IndDecl>>,
    pub opaque_types: FxHashMap<Sym, OpaqueTypeInfo>,
    pub types: FxHashMap<Sym, Rc<Value>>,
    pub values: Globals,
    pub extracts: FxHashMap<(ExtractTarget, Sym), ExtractSpec>,
    pub decl_order: Vec<Decl>,
}

impl Signature {
    pub fn new(mut interner: Interner) -> Self {
        let sym_int = interner.intern("Int");
        let sym_bool = interner.intern("Bool");
        let sym_str = interner.intern("String");
        let sym_unit = interner.intern("Unit");
        let sym_wild = interner.intern("_");
        let val_int = Rc::new(Value::Ind(sym_int, Vec::new()));
        let val_bool = Rc::new(Value::Ind(sym_bool, Vec::new()));
        let val_str = Rc::new(Value::Ind(sym_str, Vec::new()));
        let val_unit = Rc::new(Value::Ind(sym_unit, Vec::new()));
        let val_sort_type = Rc::new(Value::Sort(Sort::Type));
        let val_sort_prop = Rc::new(Value::Sort(Sort::Prop));
        let mut opaque_types =
            FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default());
        let empty_type = Rc::new((Vec::new(), Sort::Type));
        for s in [sym_int, sym_bool, sym_str, sym_unit] {
            opaque_types.insert(s, empty_type.clone());
        }
        Self {
            interner,
            sym_int,
            sym_bool,
            sym_str,
            sym_unit,
            sym_wild,
            val_int,
            val_bool,
            val_str,
            val_unit,
            val_sort_type,
            val_sort_prop,
            inds: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()),
            opaque_types,
            types: FxHashMap::with_capacity_and_hasher(256, BuildHasherDefault::default()),
            values: FxHashMap::with_capacity_and_hasher(256, BuildHasherDefault::default()),
            extracts: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()),
            decl_order: Vec::new(),
        }
    }

    #[inline]
    pub fn lit_type(&self, l: &Lit) -> Rc<Value> {
        match l {
            Lit::Int(_) => self.val_int.clone(),
            Lit::Bool(_) => self.val_bool.clone(),
            Lit::Str(_) => self.val_str.clone(),
            Lit::Unit => self.val_unit.clone(),
        }
    }

    #[inline]
    pub fn sort_val(&self, s: Sort) -> Rc<Value> {
        match s {
            Sort::Type => self.val_sort_type.clone(),
            Sort::Prop => self.val_sort_prop.clone(),
        }
    }

    #[inline]
    pub fn prim_sig(&self, op: PrimOp) -> (&[Rc<Value>], Rc<Value>) {
        // Fast non-allocating check helper via `prim_check`.
        let ret = match op {
            PrimOp::Add | PrimOp::Sub | PrimOp::Mul | PrimOp::Div | PrimOp::Rem => {
                self.val_int.clone()
            }
            PrimOp::Lt
            | PrimOp::Le
            | PrimOp::Gt
            | PrimOp::Ge
            | PrimOp::And
            | PrimOp::Or
            | PrimOp::Not
            | PrimOp::Eq
            | PrimOp::Neq => self.val_bool.clone(),
            PrimOp::StrCat => self.val_str.clone(),
        };
        (&[], ret)
    }

    #[inline]
    pub fn prim_arg_ty(&self, op: PrimOp) -> (usize, Rc<Value>, Rc<Value>) {
        match op {
            PrimOp::Add | PrimOp::Sub | PrimOp::Mul | PrimOp::Div | PrimOp::Rem => {
                (2, self.val_int.clone(), self.val_int.clone())
            }
            PrimOp::Lt | PrimOp::Le | PrimOp::Gt | PrimOp::Ge => {
                (2, self.val_int.clone(), self.val_bool.clone())
            }
            PrimOp::And | PrimOp::Or => (2, self.val_bool.clone(), self.val_bool.clone()),
            PrimOp::Not => (1, self.val_bool.clone(), self.val_bool.clone()),
            PrimOp::StrCat => (2, self.val_str.clone(), self.val_str.clone()),
            PrimOp::Eq | PrimOp::Neq => (2, self.val_unit.clone(), self.val_bool.clone()),
        }
    }

    pub fn sort_of_type(&self, lvl: u32, ty: &Rc<Value>) -> Sort {
        match ty.as_ref() {
            Value::Sort(Sort::Prop) | Value::Sort(Sort::Type) => Sort::Type,
            Value::Ind(name, _) => {
                if let Some(ind) = self.inds.get(name) {
                    ind.isort
                } else if let Some(entry) = self.opaque_types.get(name) {
                    entry.1
                } else {
                    Sort::Type
                }
            }
            Value::Pi(_, _, clos) => {
                let arg = Rc::new(Value::Neutral(Neutral::Var(self.sym_wild, lvl)));
                if let Ok(res) = nbe::apply_closure(&self.values, &self.interner, clos, arg) {
                    match res.as_ref() {
                        Value::Ind(name, _) => {
                            if let Some(ind) = self.inds.get(name) {
                                ind.isort
                            } else if let Some(entry) = self.opaque_types.get(name) {
                                entry.1
                            } else {
                                Sort::Type
                            }
                        }
                        Value::Sort(Sort::Prop) => Sort::Prop,
                        Value::Pi(..) => self.sort_of_type(lvl + 1, &res),
                        _ => Sort::Type,
                    }
                } else {
                    Sort::Type
                }
            }
            _ => Sort::Type,
        }
    }

    #[inline]
    pub fn is_prop_type(&self, lvl: u32, ty: &Rc<Value>) -> bool {
        self.sort_of_type(lvl, ty) == Sort::Prop
    }
}

/// Mutable rollback context for zero-clone bidirectional typechecking and erasure.
pub struct Ctx {
    pub locals: Vec<(Sym, Rc<Value>)>,
    pub env: Env,
    pub lvl: u32,
}

#[derive(Clone)]
pub struct CtxMark {
    locals_len: usize,
    env: Env,
    lvl: u32,
}

impl Ctx {
    pub fn empty() -> Self {
        Self {
            locals: Vec::with_capacity(16),
            env: None,
            lvl: 0,
        }
    }

    #[inline]
    pub fn mark(&self) -> CtxMark {
        CtxMark {
            locals_len: self.locals.len(),
            env: self.env.clone(),
            lvl: self.lvl,
        }
    }

    #[inline]
    pub fn restore(&mut self, mark: CtxMark) {
        self.locals.truncate(mark.locals_len);
        self.env = mark.env;
        self.lvl = mark.lvl;
    }

    #[inline]
    pub fn push_local(&mut self, x: Sym, ty: Rc<Value>, v: Rc<Value>) {
        self.locals.push((x, ty));
        self.env = env_extend(&self.env, x, v);
        self.lvl += 1;
    }

    #[inline]
    pub fn push_var(&mut self, x: Sym, ty: Rc<Value>) -> Rc<Value> {
        let v = Rc::new(Value::Neutral(Neutral::Var(x, self.lvl)));
        self.push_local(x, ty, v.clone());
        v
    }

    #[inline]
    pub fn lookup_local(&self, x: Sym) -> Option<Rc<Value>> {
        for (k, ty) in self.locals.iter().rev() {
            if *k == x {
                return Some(ty.clone());
            }
        }
        None
    }
}

fn quote_pp(sig: &mut Signature, lvl: u32, v: &Rc<Value>) -> String {
    match nbe::quote(&sig.values, &mut sig.interner, lvl, v) {
        Ok(t) => pp_term(&sig.interner, &t),
        Err(e) => e.0,
    }
}

pub fn check_type(sig: &mut Signature, ctx: &mut Ctx, t: &Term) -> Result<Sort, Error> {
    match t {
        Term::Sort(Sort::Prop) | Term::Sort(Sort::Type) => Ok(Sort::Type),
        Term::Pi(x, a, b) => {
            check_type(sig, ctx, a)?;
            let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
            let mark = ctx.mark();
            ctx.push_var(*x, va);
            let res = check_type(sig, ctx, b);
            ctx.restore(mark);
            res
        }
        Term::Ind(name, args) => {
            if let Some(ind) = sig.inds.get(name).cloned() {
                if !ind.iparams.is_empty() || !args.is_empty() {
                    check_param_args(sig, ctx, *name, &ind.iparams, args)?;
                }
                Ok(ind.isort)
            } else if let Some(entry) = sig.opaque_types.get(name).cloned() {
                if !entry.0.is_empty() || !args.is_empty() {
                    check_param_args(sig, ctx, *name, &entry.0, args)?;
                }
                Ok(entry.1)
            } else {
                Err(Error(format!(
                    "unknown type constructor: {}",
                    sig.interner.resolve(*name)
                )))
            }
        }
        Term::RecTy(fs) => {
            for (_, ft) in fs {
                check_type(sig, ctx, ft)?;
            }
            Ok(Sort::Type)
        }
        _ => {
            let ty = infer(sig, ctx, t)?;
            match ty.as_ref() {
                Value::Sort(s) => Ok(*s),
                _ => Err(Error(format!(
                    "expected a type, got term of type {}",
                    quote_pp(sig, ctx.lvl, &ty)
                ))),
            }
        }
    }
}

fn check_param_args(
    sig: &mut Signature,
    ctx: &mut Ctx,
    name: Sym,
    params: &[(Sym, Term)],
    args: &[Term],
) -> Result<(), Error> {
    if params.len() != args.len() {
        return Err(Error(format!(
            "type {} expects {} args, got {}",
            sig.interner.resolve(name),
            params.len(),
            args.len()
        )));
    }
    let mut env: Env = None;
    for ((px, pty), a) in params.iter().zip(args) {
        let vpty = nbe::eval(&sig.values, &sig.interner, &env, pty)?;
        check(sig, ctx, a, &vpty)?;
        let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
        env = env_extend(&env, *px, va);
    }
    Ok(())
}

pub fn infer(sig: &mut Signature, ctx: &mut Ctx, t: &Term) -> Result<Rc<Value>, Error> {
    match t {
        Term::Var(x) => {
            if let Some(ty) = ctx.lookup_local(*x) {
                Ok(ty)
            } else if let Some(ty) = sig.types.get(x) {
                Ok(ty.clone())
            } else {
                Err(Error(format!(
                    "unbound variable: {}",
                    sig.interner.resolve(*x)
                )))
            }
        }
        Term::Global(g) => sig
            .types
            .get(g)
            .cloned()
            .ok_or_else(|| Error(format!("unbound global: {}", sig.interner.resolve(*g)))),
        Term::Sort(_) => Ok(sig.val_sort_type.clone()),
        Term::Pi(x, a, b) => {
            check_type(sig, ctx, a)?;
            let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
            let mark = ctx.mark();
            ctx.push_var(*x, va);
            let sb = check_type(sig, ctx, b);
            ctx.restore(mark);
            Ok(sig.sort_val(sb?))
        }
        Term::Ind(_, _) => {
            let s = check_type(sig, ctx, t)?;
            Ok(sig.sort_val(s))
        }
        Term::RecTy(fs) => {
            for (_, ft) in fs {
                check_type(sig, ctx, ft)?;
            }
            Ok(sig.val_sort_type.clone())
        }
        Term::Lit(l) => Ok(sig.lit_type(l)),
        Term::Lam(x, Some(a), body) => {
            check_type(sig, ctx, a)?;
            let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
            let env_snap = ctx.env.clone();
            let mark = ctx.mark();
            ctx.push_var(*x, va.clone());
            let vb = infer(sig, ctx, body);
            let lvl2 = ctx.lvl;
            ctx.restore(mark);
            let vb = vb?;
            let qb = nbe::quote(&sig.values, &mut sig.interner, lvl2, &vb)?;
            Ok(Rc::new(Value::Pi(
                *x,
                va,
                Closure::Term {
                    env: env_snap,
                    param: *x,
                    body: Rc::new(qb),
                },
            )))
        }
        Term::Lam(x, None, _) => Err(Error(format!(
            "cannot infer unannotated lambda |{}| (needs expected type)",
            sig.interner.resolve(*x)
        ))),
        Term::App(_, _) => infer_app_spine(sig, ctx, t),
        Term::Let(x, ann, rhs, body) => {
            let ty_rhs = match ann {
                Some(a) => {
                    check_type(sig, ctx, a)?;
                    let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
                    check(sig, ctx, rhs, &va)?;
                    va
                }
                None => infer(sig, ctx, rhs)?,
            };
            let vrhs = if *x == sig.sym_wild {
                sig.val_unit.clone()
            } else {
                nbe::eval(&sig.values, &sig.interner, &ctx.env, rhs)?
            };
            let mark = ctx.mark();
            ctx.push_local(*x, ty_rhs, vrhs);
            let res = infer(sig, ctx, body);
            ctx.restore(mark);
            res
        }
        Term::Ctor(tname, cname, args) => infer_ctor(sig, ctx, *tname, *cname, args),
        Term::Match(scrut, branches) => {
            let sty = infer(sig, ctx, scrut)?;
            let Some(((p0, rhs0), rest)) = branches.split_first() else {
                return Err(Error("empty match expression".into()));
            };
            let mark0 = ctx.mark();
            bind_pattern_in_place(sig, ctx, &sty, p0)?;
            let rty = infer(sig, ctx, rhs0);
            ctx.restore(mark0);
            let rty = rty?;
            check_prop_elim(sig, ctx.lvl, &sty, &rty)?;
            for (p, rhs) in rest {
                let mark = ctx.mark();
                bind_pattern_in_place(sig, ctx, &sty, p)?;
                let res = check(sig, ctx, rhs, &rty);
                ctx.restore(mark);
                res?;
            }
            Ok(rty)
        }
        Term::Fix(f, ty_ann, body) => {
            check_type(sig, ctx, ty_ann)?;
            let vty = nbe::eval(&sig.values, &sig.interner, &ctx.env, ty_ann)?;
            let mark = ctx.mark();
            ctx.push_var(*f, vty.clone());
            let res = check(sig, ctx, body, &vty);
            ctx.restore(mark);
            res?;
            Ok(vty)
        }
        Term::Prim(op, args) => match op {
            PrimOp::Eq | PrimOp::Neq => {
                let [a, b] = args.as_slice() else {
                    return Err(Error("equality expects 2 arguments".into()));
                };
                let ta = infer(sig, ctx, a)?;
                check(sig, ctx, b, &ta)?;
                Ok(sig.val_bool.clone())
            }
            _ => {
                let (arity, arg_ty, ret) = sig.prim_arg_ty(*op);
                if args.len() != arity {
                    return Err(Error(format!(
                        "primitive expects {arity} args, got {}",
                        args.len()
                    )));
                }
                for a in args {
                    check(sig, ctx, a, &arg_ty)?;
                }
                Ok(ret)
            }
        },
        Term::Rec(fs) => {
            let mut rfs = Vec::with_capacity(fs.len());
            for (k, e) in fs {
                rfs.push((*k, infer(sig, ctx, e)?));
            }
            Ok(Rc::new(Value::RecTy(rfs)))
        }
        Term::Proj(e, field) => {
            let te = infer(sig, ctx, e)?;
            match te.as_ref() {
                Value::RecTy(fs) => fs
                    .iter()
                    .find(|(k, _)| k == field)
                    .map(|(_, ty)| ty.clone())
                    .ok_or_else(|| {
                        Error(format!(
                            "record has no field .{}",
                            sig.interner.resolve(*field)
                        ))
                    }),
                _ => {
                    let ty_str = quote_pp(sig, ctx.lvl, &te);
                    Err(Error(format!(
                        "cannot project .{} from non-record type {}",
                        sig.interner.resolve(*field),
                        ty_str
                    )))
                }
            }
        }
        Term::Opaque(_, ty) => nbe::eval(&sig.values, &sig.interner, &ctx.env, ty),
        Term::Hole => Err(Error(
            "unsolved implicit type hole (needs expected type)".into(),
        )),
    }
}

pub(crate) fn is_explicit_type_arg(ctx: &Ctx, a: &Term) -> bool {
    match a {
        Term::Sort(_) | Term::Ind(_, _) | Term::Pi(_, _, _) | Term::RecTy(_) => true,
        Term::Var(x) => ctx
            .lookup_local(*x)
            .is_some_and(|ty| matches!(ty.as_ref(), Value::Sort(Sort::Type))),
        _ => false,
    }
}

pub(crate) fn unify_implicits(
    sig: &Signature,
    lvl_start: u32,
    lvl_next: u32,
    pat: &Rc<Value>,
    got: &Rc<Value>,
    solutions: &mut [Option<Rc<Value>>],
) {
    match (pat.as_ref(), got.as_ref()) {
        (Value::Neutral(Neutral::Var(_, l)), _)
            if *l >= lvl_start && ((*l - lvl_start) as usize) < solutions.len() =>
        {
            let slot = &mut solutions[(*l - lvl_start) as usize];
            if slot.is_none() {
                *slot = Some(got.clone());
            }
        }
        (Value::Ind(t1, args1), Value::Ind(t2, args2))
            if t1 == t2 && args1.len() == args2.len() =>
        {
            for (p, g) in args1.iter().zip(args2.iter()) {
                unify_implicits(sig, lvl_start, lvl_next, p, g, solutions);
            }
        }
        (Value::Pi(_, d1, c1), Value::Pi(_, d2, c2)) => {
            unify_implicits(sig, lvl_start, lvl_next, d1, d2, solutions);
            let arg_v = Rc::new(Value::Neutral(Neutral::Var(sig.sym_wild, lvl_next)));
            if let (Ok(r1), Ok(r2)) = (
                nbe::apply_closure(&sig.values, &sig.interner, c1, arg_v.clone()),
                nbe::apply_closure(&sig.values, &sig.interner, c2, arg_v),
            ) {
                unify_implicits(sig, lvl_start, lvl_next + 1, &r1, &r2, solutions);
            }
        }
        (Value::RecTy(fs1), Value::RecTy(fs2)) if fs1.len() == fs2.len() => {
            for ((_, p), (_, g)) in fs1.iter().zip(fs2.iter()) {
                unify_implicits(sig, lvl_start, lvl_next, p, g, solutions);
            }
        }
        _ => {}
    }
}

fn infer_app_spine(sig: &mut Signature, ctx: &mut Ctx, t: &Term) -> Result<Rc<Value>, Error> {
    infer_app_spine_with_expected(sig, ctx, t, None)
}

fn infer_app_spine_with_expected(
    sig: &mut Signature,
    ctx: &mut Ctx,
    t: &Term,
    expected_ret: Option<&Rc<Value>>,
) -> Result<Rc<Value>, Error> {
    let mut head = t;
    let mut args = Vec::new();
    while let Term::App(f, a) = head {
        args.push(a.as_ref());
        head = f.as_ref();
    }
    args.reverse();

    let mut cur_ty = infer(sig, ctx, head)?;
    let mut idx = 0;
    while idx < args.len() {
        let a = args[idx];
        match cur_ty.as_ref() {
            Value::Pi(_, dom, cod)
                if matches!(dom.as_ref(), Value::Sort(Sort::Type))
                    && is_explicit_type_arg(ctx, a) =>
            {
                let stype = sig.val_sort_type.clone();
                check(sig, ctx, a, &stype)?;
                let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
                cur_ty = nbe::apply_closure(&sig.values, &sig.interner, cod, va)?;
                idx += 1;
            }
            Value::Pi(_, dom, _)
                if matches!(dom.as_ref(), Value::Sort(Sort::Type)) || matches!(a, Term::Hole) =>
            {
                // Collect leading implicit binders (either `Type` params or explicit `Term::Hole`s
                // inserted for `<w: Nat>` / `<T>` polymorphic definitions) and solve them from
                // the remaining value arguments and (when available) the expected return type.
                let lvl_start = ctx.lvl;
                let mut probe = cur_ty.clone();
                let mut n_impl = 0usize;
                let mut n_holes = 0usize;
                while let Value::Pi(px, d, c) = probe.as_ref() {
                    let is_hole_here =
                        idx + n_holes < args.len() && matches!(args[idx + n_holes], Term::Hole);
                    if !matches!(d.as_ref(), Value::Sort(Sort::Type)) && !is_hole_here {
                        break;
                    }
                    if is_hole_here {
                        n_holes += 1;
                    }
                    let var = Rc::new(Value::Neutral(Neutral::Var(*px, lvl_start + n_impl as u32)));
                    probe = nbe::apply_closure(&sig.values, &sig.interner, c, var)?;
                    n_impl += 1;
                }
                let val_start = idx + n_holes;
                let mut solutions: Vec<Option<Rc<Value>>> = vec![None; n_impl];
                let mut probe_arg_ty = probe;
                let mut next_lvl = lvl_start + n_impl as u32;
                for &rem_arg in &args[val_start..] {
                    let Value::Pi(px, d, c) = probe_arg_ty.as_ref() else {
                        break;
                    };
                    if let Ok(ta) = infer(sig, ctx, rem_arg) {
                        unify_implicits(sig, lvl_start, next_lvl, d, &ta, &mut solutions);
                    }
                    let dummy = Rc::new(Value::Neutral(Neutral::Var(*px, next_lvl)));
                    let Ok(next_probe) = nbe::apply_closure(&sig.values, &sig.interner, c, dummy)
                    else {
                        break;
                    };
                    probe_arg_ty = next_probe;
                    next_lvl += 1;
                }
                if let Some(exp) = expected_ret
                    && solutions.iter().any(Option::is_none)
                {
                    unify_implicits(sig, lvl_start, next_lvl, &probe_arg_ty, exp, &mut solutions);
                }
                for sol in solutions {
                    let Some(ty_val) = sol else {
                        return Err(Error(format!(
                            "implicit type instantiation failed on {}",
                            pp_term(&sig.interner, head)
                        )));
                    };
                    let Value::Pi(_, _, cod) = cur_ty.as_ref() else {
                        return Err(Error(format!(
                            "implicit type instantiation failed on {}",
                            pp_term(&sig.interner, head)
                        )));
                    };
                    cur_ty = nbe::apply_closure(&sig.values, &sig.interner, cod, ty_val)?;
                }
                idx = val_start;
            }
            Value::Pi(px, dom, cod) => {
                check(sig, ctx, a, dom)?;
                let va = if *px == sig.sym_wild {
                    sig.val_unit.clone()
                } else {
                    nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?
                };
                cur_ty = nbe::apply_closure(&sig.values, &sig.interner, cod, va)?;
                idx += 1;
            }
            _ => {
                return Err(Error(format!(
                    "cannot apply non-function of type {}",
                    quote_pp(sig, ctx.lvl, &cur_ty)
                )));
            }
        }
    }
    Ok(cur_ty)
}

pub fn check(
    sig: &mut Signature,
    ctx: &mut Ctx,
    t: &Term,
    expected: &Rc<Value>,
) -> Result<(), Error> {
    match (t, expected.as_ref()) {
        (Term::Lam(x, None, body), Value::Pi(_, dom, cod)) => {
            let mark = ctx.mark();
            let v = ctx.push_var(*x, dom.clone());
            let exp2 = nbe::apply_closure(&sig.values, &sig.interner, cod, v);
            let res = match exp2 {
                Ok(exp2) => check(sig, ctx, body, &exp2),
                Err(e) => Err(e),
            };
            ctx.restore(mark);
            res
        }
        (Term::Lam(x, Some(a), body), Value::Pi(_, dom, cod)) => {
            check_type(sig, ctx, a)?;
            let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
            if !nbe::conv(&sig.values, &sig.interner, ctx.lvl, &va, dom)? {
                let x_name = sig.interner.resolve(*x).to_owned();
                let exp_s = quote_pp(sig, ctx.lvl, dom);
                let got_s = quote_pp(sig, ctx.lvl, &va);
                return Err(Error(format!(
                    "lambda parameter {x_name}: expected {exp_s}, got {got_s}"
                )));
            }
            let mark = ctx.mark();
            let v = ctx.push_var(*x, dom.clone());
            let exp2 = nbe::apply_closure(&sig.values, &sig.interner, cod, v);
            let res = match exp2 {
                Ok(exp2) => check(sig, ctx, body, &exp2),
                Err(e) => Err(e),
            };
            ctx.restore(mark);
            res
        }
        (Term::Match(scrut, branches), _) => {
            let sty = infer(sig, ctx, scrut)?;
            check_prop_elim(sig, ctx.lvl, &sty, expected)?;
            for (p, rhs) in branches.iter() {
                let mark = ctx.mark();
                bind_pattern_in_place(sig, ctx, &sty, p)?;
                let res = check(sig, ctx, rhs, expected);
                ctx.restore(mark);
                res?;
            }
            Ok(())
        }
        (Term::Let(x, ann, rhs, body), _) => {
            let ty_rhs = match ann {
                Some(a) => {
                    check_type(sig, ctx, a)?;
                    let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
                    check(sig, ctx, rhs, &va)?;
                    va
                }
                None => infer(sig, ctx, rhs)?,
            };
            let vrhs = if *x == sig.sym_wild {
                sig.val_unit.clone()
            } else {
                nbe::eval(&sig.values, &sig.interner, &ctx.env, rhs)?
            };
            let mark = ctx.mark();
            ctx.push_local(*x, ty_rhs, vrhs);
            let res = check(sig, ctx, body, expected);
            ctx.restore(mark);
            res
        }
        (Term::Rec(fs), Value::RecTy(exp_fs)) => {
            if fs.len() != exp_fs.len() {
                return Err(Error("record arity mismatch".into()));
            }
            for ((k1, e), (k2, ty)) in fs.iter().zip(exp_fs) {
                if k1 != k2 {
                    return Err(Error(format!(
                        "record field mismatch: expected {}, got {}",
                        sig.interner.resolve(*k2),
                        sig.interner.resolve(*k1)
                    )));
                }
                check(sig, ctx, e, ty)?;
            }
            Ok(())
        }
        (Term::Ctor(tname, cname, args), Value::Ind(etname, etargs)) if tname == etname => {
            let ind = sig.inds.get(tname).cloned().ok_or_else(|| {
                Error(format!(
                    "unknown inductive {}",
                    sig.interner.resolve(*tname)
                ))
            })?;
            let n_tp = ind.iparams.len();
            let actual = if n_tp > 0
                && args.len() >= n_tp
                && args[..n_tp].iter().any(|a| matches!(a, Term::Hole))
            {
                let mut filled = Vec::with_capacity(args.len());
                for (i, a) in args.iter().enumerate() {
                    if i < n_tp && matches!(a, Term::Hole) {
                        filled.push(nbe::quote(
                            &sig.values,
                            &mut sig.interner,
                            ctx.lvl,
                            &etargs[i],
                        )?);
                    } else {
                        filled.push(a.clone());
                    }
                }
                infer_ctor(sig, ctx, *tname, *cname, &filled)?
            } else {
                infer_ctor(sig, ctx, *tname, *cname, args)?
            };
            if !nbe::conv(&sig.values, &sig.interner, ctx.lvl, &actual, expected)? {
                let tn = sig.interner.resolve(*tname).to_owned();
                let cn = sig.interner.resolve(*cname).to_owned();
                let exp_s = quote_pp(sig, ctx.lvl, expected);
                let got_s = quote_pp(sig, ctx.lvl, &actual);
                return Err(Error(format!(
                    "constructor {tn}::{cn} type mismatch: expected {exp_s}, got {got_s}"
                )));
            }
            Ok(())
        }
        (Term::App(_, _), _) => {
            let actual = infer_app_spine_with_expected(sig, ctx, t, Some(expected))?;
            if !nbe::conv(&sig.values, &sig.interner, ctx.lvl, &actual, expected)? {
                let exp_s = quote_pp(sig, ctx.lvl, expected);
                let got_s = quote_pp(sig, ctx.lvl, &actual);
                let tm_s = pp_term(&sig.interner, t);
                return Err(Error(format!(
                    "type mismatch: expected {exp_s}, got {got_s} (in term {tm_s})"
                )));
            }
            Ok(())
        }
        _ => {
            let actual = infer(sig, ctx, t)?;
            if !nbe::conv(&sig.values, &sig.interner, ctx.lvl, &actual, expected)? {
                let exp_s = quote_pp(sig, ctx.lvl, expected);
                let got_s = quote_pp(sig, ctx.lvl, &actual);
                let tm_s = pp_term(&sig.interner, t);
                return Err(Error(format!(
                    "type mismatch: expected {exp_s}, got {got_s} (in term {tm_s})"
                )));
            }
            Ok(())
        }
    }
}

// Rocq's `Prop`-elimination restriction: delegates directly to the formally
// verified `kernel_core::prop_elim_allowed` (proved in Lean 4 via Hax).
fn check_prop_elim(
    sig: &mut Signature,
    lvl: u32,
    scrut_ty: &Rc<Value>,
    res_ty: &Rc<Value>,
) -> Result<(), Error> {
    let scrut_prop = sig.is_prop_type(lvl, scrut_ty);
    let res_prop = sig.is_prop_type(lvl, res_ty);
    if !kernel_core::prop_elim_allowed(scrut_prop, res_prop) {
        let s1 = quote_pp(sig, lvl, scrut_ty);
        let s2 = quote_pp(sig, lvl, res_ty);
        return Err(Error(format!(
            "cannot eliminate Prop scrutinee ({s1}) into Type result ({s2}) — Prop is erased!"
        )));
    }
    Ok(())
}

fn infer_ctor(
    sig: &mut Signature,
    ctx: &mut Ctx,
    tname: Sym,
    cname: Sym,
    args: &[Term],
) -> Result<Rc<Value>, Error> {
    let ind = sig.inds.get(&tname).cloned().ok_or_else(|| {
        Error(format!(
            "unknown inductive type {}",
            sig.interner.resolve(tname)
        ))
    })?;
    let ctor_idx = ind
        .ictors
        .iter()
        .position(|c| c.cname == cname)
        .ok_or_else(|| {
            Error(format!(
                "unknown constructor {}::{}",
                sig.interner.resolve(tname),
                sig.interner.resolve(cname)
            ))
        })?;
    let ctor = &ind.ictors[ctor_idx];
    let n_tp = ind.iparams.len();
    let total = n_tp + ctor.cparams.len();
    if args.len() != total {
        return Err(Error(format!(
            "constructor {}::{} expects {} args (including {} type params), got {}",
            sig.interner.resolve(tname),
            sig.interner.resolve(cname),
            total,
            n_tp,
            args.len()
        )));
    }
    if n_tp == 0 {
        let mut env: Env = None;
        for ((px, pty), a) in ctor.cparams.iter().zip(args) {
            let vpty = nbe::eval(&sig.values, &sig.interner, &env, pty)?;
            check(sig, ctx, a, &vpty)?;
            if *px != sig.sym_wild {
                let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
                env = env_extend(&env, *px, va);
            }
        }
        return nbe::eval(&sig.values, &sig.interner, &env, &ctor.cret);
    }

    let mut solved_targs: Vec<Option<Rc<Value>>> = vec![None; n_tp];
    for (i, a) in args.iter().take(n_tp).enumerate() {
        if !matches!(a, Term::Hole) {
            solved_targs[i] = Some(nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?);
        }
    }
    for ((_, fty), varg) in ctor.cparams.iter().zip(&args[n_tp..]) {
        if let Term::Var(px) = fty {
            for (ti, (p, _)) in ind.iparams.iter().enumerate() {
                if p == px
                    && solved_targs[ti].is_none()
                    && let Ok(v) = infer(sig, ctx, varg)
                {
                    solved_targs[ti] = Some(v);
                }
            }
        }
    }
    let mut env: Env = None;
    for (idx, ((px, pty), a)) in ind
        .iparams
        .iter()
        .chain(&ctor.cparams)
        .zip(args)
        .enumerate()
    {
        if idx < n_tp && matches!(a, Term::Hole) {
            let va = solved_targs[idx]
                .clone()
                .unwrap_or_else(|| sig.val_unit.clone());
            env = env_extend(&env, *px, va);
        } else {
            let vpty = nbe::eval(&sig.values, &sig.interner, &env, pty)?;
            check(sig, ctx, a, &vpty)?;
            let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
            env = env_extend(&env, *px, va);
        }
    }
    nbe::eval(&sig.values, &sig.interner, &env, &ctor.cret)
}

pub fn bind_pattern_in_place(
    sig: &Signature,
    ctx: &mut Ctx,
    sty: &Rc<Value>,
    pat: &Pattern,
) -> Result<(), Error> {
    match pat {
        Pattern::Wild => Ok(()),
        Pattern::Var(x) => {
            ctx.push_var(*x, sty.clone());
            Ok(())
        }
        Pattern::Lit(l) => {
            let lt = sig.lit_type(l);
            if !nbe::conv(&sig.values, &sig.interner, ctx.lvl, &lt, sty)? {
                return Err(Error("literal pattern type mismatch".into()));
            }
            Ok(())
        }
        Pattern::Ctor(cname, xs) => match sty.as_ref() {
            Value::Ind(tname, targs) => {
                let ind = sig.inds.get(tname).ok_or_else(|| {
                    Error(format!(
                        "match on non-inductive {}",
                        sig.interner.resolve(*tname)
                    ))
                })?;
                let ctor = ind
                    .ictors
                    .iter()
                    .find(|c| c.cname == *cname)
                    .ok_or_else(|| {
                        Error(format!(
                            "constructor {} does not belong to {}",
                            sig.interner.resolve(*cname),
                            sig.interner.resolve(*tname)
                        ))
                    })?;
                if xs.len() != ctor.cparams.len() {
                    return Err(Error(format!(
                        "pattern {} expects {} fields, got {}",
                        sig.interner.resolve(*cname),
                        ctor.cparams.len(),
                        xs.len()
                    )));
                }
                let mut env: Env = None;
                for ((p, _), ta) in ind.iparams.iter().zip(targs) {
                    env = env_extend(&env, *p, ta.clone());
                }
                for ((px, pty), vname) in ctor.cparams.iter().zip(xs) {
                    let vpty = nbe::eval(&sig.values, &sig.interner, &env, pty)?;
                    let v = ctx.push_var(*vname, vpty);
                    env = env_extend(&env, *px, v);
                }
                Ok(())
            }
            _ => Err(Error(format!(
                "constructor pattern {} on non-inductive scrutinee",
                sig.interner.resolve(*cname)
            ))),
        },
    }
}

pub fn check_decl(sig: &mut Signature, d: Decl) -> Result<(), Error> {
    let mut ctx = Ctx::empty();
    match d {
        Decl::Ind(ind) => {
            let ind_rc = Rc::new(ind);
            sig.inds.insert(ind_rc.iname, ind_rc.clone());
            let mark_p = ctx.mark();
            for (px, pty) in &ind_rc.iparams {
                check_type(sig, &mut ctx, pty)?;
                let vpty = nbe::eval(&sig.values, &sig.interner, &ctx.env, pty)?;
                ctx.push_var(*px, vpty);
            }
            for ctor in &ind_rc.ictors {
                let mark_c = ctx.mark();
                for (px, pty) in &ctor.cparams {
                    check_type(sig, &mut ctx, pty)?;
                    let vpty = nbe::eval(&sig.values, &sig.interner, &ctx.env, pty)?;
                    ctx.push_var(*px, vpty);
                }
                check_type(sig, &mut ctx, &ctor.cret)?;
                ctx.restore(mark_c);
            }
            ctx.restore(mark_p);
            sig.decl_order.push(Decl::Ind((*ind_rc).clone()));
        }
        Decl::Type(name, params, sort) => {
            for (px, pty) in &params {
                check_type(sig, &mut ctx, pty)?;
                let vpty = nbe::eval(&sig.values, &sig.interner, &ctx.env, pty)?;
                ctx.push_var(*px, vpty);
            }
            sig.opaque_types
                .insert(name, Rc::new((params.clone(), sort)));
            sig.decl_order.push(Decl::Type(name, params, sort));
        }
        Decl::Axiom(name, ty) => {
            check_type(sig, &mut ctx, &ty)?;
            let vty = nbe::eval(&sig.values, &sig.interner, &None, &ty)?;
            sig.types.insert(name, vty);
            sig.values
                .insert(name, Rc::new(Value::Neutral(Neutral::Global(name))));
            sig.decl_order.push(Decl::Axiom(name, ty));
        }
        Decl::Def(name, Some(ty), body) => {
            check_type(sig, &mut ctx, &ty)?;
            let vty = nbe::eval(&sig.values, &sig.interner, &None, &ty)?;
            check(sig, &mut ctx, &body, &vty)?;
            let vbody = nbe::eval(&sig.values, &sig.interner, &None, &body)?;
            sig.types.insert(name, vty);
            sig.values.insert(name, vbody);
            sig.decl_order.push(Decl::Def(name, Some(ty), body));
        }
        Decl::Def(name, None, body) => {
            let vty = infer(sig, &mut ctx, &body)?;
            let vbody = nbe::eval(&sig.values, &sig.interner, &None, &body)?;
            let qty = nbe::quote(&sig.values, &mut sig.interner, 0, &vty)?;
            sig.types.insert(name, vty);
            sig.values.insert(name, vbody);
            sig.decl_order.push(Decl::Def(name, Some(qty), body));
        }
        Decl::Extract(target, name, spec) => {
            // Extract declarations are pure backend metadata — NEVER inspected by typing!
            sig.extracts.insert((target, name), spec.clone());
            sig.decl_order.push(Decl::Extract(target, name, spec));
        }
    }
    Ok(())
}

pub fn check_program(interner: Interner, decls: Vec<Decl>) -> Result<Signature, Error> {
    let mut sig = Signature::new(interner);
    sig.decl_order.reserve(decls.len());
    for d in decls {
        let label = match &d {
            Decl::Ind(i) => Some(i.iname),
            Decl::Type(n, _, _) | Decl::Axiom(n, _) | Decl::Def(n, _, _) => Some(*n),
            Decl::Extract(_, _, _) => None,
        };
        if let Err(e) = check_decl(&mut sig, d) {
            if let Some(sym) = label {
                return Err(Error(format!(
                    "in `{}`: {}",
                    sig.interner.resolve(sym),
                    e.0
                )));
            }
            return Err(e);
        }
    }
    Ok(sig)
}
