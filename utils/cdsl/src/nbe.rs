//! Trusted Kernel Normalization by Evaluation (NbE) and Conversion Checking.
//!
//! Evaluates pure library functions (list combinators, Rewrite lowering, HOAS
//! binders, OpDef projections, CDSL-defined Rust generators) into canonical
//! normal forms with ZERO hardcoded library knowledge.

use std::rc::Rc;

use crate::{
    core::{
        Closure, Env, FxHashMap, Interner, Lit, Neutral, Pattern, PrimOp, Sym, Term, Value,
        env_extend, env_lookup,
    },
    error::Error,
};

pub type Globals = FxHashMap<Sym, Rc<Value>>;

fn eval_prim(op: PrimOp, args: &[Rc<Value>]) -> Option<Rc<Value>> {
    match (op, args) {
        (PrimOp::Add, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) => {
                Some(Rc::new(Value::Lit(Lit::Int(x.wrapping_add(*y)))))
            }
            _ => None,
        },
        (PrimOp::Sub, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) => {
                Some(Rc::new(Value::Lit(Lit::Int(x.wrapping_sub(*y)))))
            }
            _ => None,
        },
        (PrimOp::Mul, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) => {
                Some(Rc::new(Value::Lit(Lit::Int(x.wrapping_mul(*y)))))
            }
            _ => None,
        },
        (PrimOp::Div, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) if *y != 0 => {
                Some(Rc::new(Value::Lit(Lit::Int(x.wrapping_div(*y)))))
            }
            _ => None,
        },
        (PrimOp::Rem, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) if *y != 0 => {
                Some(Rc::new(Value::Lit(Lit::Int(x.wrapping_rem(*y)))))
            }
            _ => None,
        },
        (PrimOp::Eq, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(x), Value::Lit(y)) => Some(Rc::new(Value::Lit(Lit::Bool(x == y)))),
            _ => None,
        },
        (PrimOp::Neq, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(x), Value::Lit(y)) => Some(Rc::new(Value::Lit(Lit::Bool(x != y)))),
            _ => None,
        },
        (PrimOp::Lt, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) => {
                Some(Rc::new(Value::Lit(Lit::Bool(x < y))))
            }
            _ => None,
        },
        (PrimOp::Le, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) => {
                Some(Rc::new(Value::Lit(Lit::Bool(x <= y))))
            }
            _ => None,
        },
        (PrimOp::Gt, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) => {
                Some(Rc::new(Value::Lit(Lit::Bool(x > y))))
            }
            _ => None,
        },
        (PrimOp::Ge, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Int(x)), Value::Lit(Lit::Int(y))) => {
                Some(Rc::new(Value::Lit(Lit::Bool(x >= y))))
            }
            _ => None,
        },
        (PrimOp::And, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Bool(x)), Value::Lit(Lit::Bool(y))) => {
                Some(Rc::new(Value::Lit(Lit::Bool(*x && *y))))
            }
            _ => None,
        },
        (PrimOp::Or, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Bool(x)), Value::Lit(Lit::Bool(y))) => {
                Some(Rc::new(Value::Lit(Lit::Bool(*x || *y))))
            }
            _ => None,
        },
        (PrimOp::Not, [a]) => match a.as_ref() {
            Value::Lit(Lit::Bool(x)) => Some(Rc::new(Value::Lit(Lit::Bool(!*x)))),
            _ => None,
        },
        (PrimOp::StrCat, [a, b]) => match (a.as_ref(), b.as_ref()) {
            (Value::Lit(Lit::Str(x)), Value::Lit(Lit::Str(y))) => {
                let mut s = String::with_capacity(x.len() + y.len());
                s.push_str(x);
                s.push_str(y);
                Some(Rc::new(Value::Lit(Lit::Str(Rc::from(s)))))
            }
            _ => None,
        },
        _ => None,
    }
}

pub fn apply_closure(
    g: &Globals,
    interner: &Interner,
    clos: &Closure,
    arg: Rc<Value>,
) -> Result<Rc<Value>, Error> {
    match clos {
        Closure::Term { env, param, body } => {
            let env2 = env_extend(env, *param, arg);
            eval(g, interner, &env2, body)
        }
        Closure::FixLam {
            env,
            self_name,
            param,
            body,
        } => {
            let self_val = Rc::new(Value::Lam(*param, clos.clone()));
            let env1 = env_extend(env, *self_name, self_val);
            let env2 = env_extend(&env1, *param, arg);
            eval(g, interner, &env2, body)
        }
    }
}

pub fn vapp(
    g: &Globals,
    interner: &Interner,
    f: &Rc<Value>,
    a: Rc<Value>,
) -> Result<Rc<Value>, Error> {
    match f.as_ref() {
        Value::Lam(_, clos) => apply_closure(g, interner, clos, a),
        Value::Neutral(n) => Ok(Rc::new(Value::Neutral(Neutral::App(
            Box::new(n.clone()),
            a,
        )))),
        _ => Err(Error("NbE: applied non-function value".into())),
    }
}

fn match_branch(
    g: &Globals,
    interner: &Interner,
    env: &Env,
    scrut: Rc<Value>,
    branches: &Rc<Vec<(Pattern, Term)>>,
) -> Result<Rc<Value>, Error> {
    if let Value::Neutral(n) = scrut.as_ref() {
        return Ok(Rc::new(Value::Neutral(Neutral::Match(
            Box::new(n.clone()),
            env.clone(),
            Rc::clone(branches),
        ))));
    }
    for (pat, rhs) in branches.iter() {
        match pat {
            Pattern::Wild => return eval(g, interner, env, rhs),
            Pattern::Var(x) => {
                let env2 = env_extend(env, *x, scrut);
                return eval(g, interner, &env2, rhs);
            }
            Pattern::Lit(l) => {
                if let Value::Lit(l2) = scrut.as_ref()
                    && l == l2
                {
                    return eval(g, interner, env, rhs);
                }
            }
            Pattern::Ctor(c, xs) => {
                if let Value::Ctor(_, c2, vs) = scrut.as_ref()
                    && c == c2
                    && xs.len() <= vs.len()
                {
                    let drop = vs.len() - xs.len();
                    let mut env2 = env.clone();
                    for (x, v) in xs.iter().zip(&vs[drop..]) {
                        env2 = env_extend(&env2, *x, v.clone());
                    }
                    return eval(g, interner, &env2, rhs);
                }
            }
        }
    }
    let got = match scrut.as_ref() {
        Value::Ctor(t, c, vs) => format!(
            "{}::{}({} args)",
            interner.resolve(*t),
            interner.resolve(*c),
            vs.len()
        ),
        Value::Lit(l) => format!("lit {l:?}"),
        _ => format!("{scrut:?}"),
    };
    Err(Error(format!("NbE: non-exhaustive match on {got}")))
}

pub fn eval(g: &Globals, interner: &Interner, env: &Env, t: &Term) -> Result<Rc<Value>, Error> {
    match t {
        Term::Var(x) => {
            if let Some(v) = env_lookup(env.as_deref(), *x) {
                Ok(v)
            } else if let Some(v) = g.get(x) {
                Ok(v.clone())
            } else {
                Ok(Rc::new(Value::Neutral(Neutral::Global(*x))))
            }
        }
        Term::Global(name) => {
            if let Some(v) = g.get(name) {
                Ok(v.clone())
            } else {
                Ok(Rc::new(Value::Neutral(Neutral::Global(*name))))
            }
        }
        Term::Sort(s) => Ok(Rc::new(Value::Sort(*s))),
        Term::Pi(x, a, b) => {
            let va = eval(g, interner, env, a)?;
            Ok(Rc::new(Value::Pi(
                *x,
                va,
                Closure::Term {
                    env: env.clone(),
                    param: *x,
                    body: Rc::clone(b),
                },
            )))
        }
        Term::Lam(x, _, b) => Ok(Rc::new(Value::Lam(
            *x,
            Closure::Term {
                env: env.clone(),
                param: *x,
                body: Rc::clone(b),
            },
        ))),
        Term::App(f, a) => {
            let vf = eval(g, interner, env, f)?;
            let va = eval(g, interner, env, a)?;
            vapp(g, interner, &vf, va)
        }
        Term::Let(x, _, v, b) => {
            let vv = eval(g, interner, env, v)?;
            let env2 = env_extend(env, *x, vv);
            eval(g, interner, &env2, b)
        }
        Term::Ind(name, args) => {
            let mut vargs = Vec::with_capacity(args.len());
            for a in args {
                vargs.push(eval(g, interner, env, a)?);
            }
            Ok(Rc::new(Value::Ind(*name, vargs)))
        }
        Term::Ctor(tname, cname, args) => {
            let mut vargs = Vec::with_capacity(args.len());
            for a in args {
                vargs.push(eval(g, interner, env, a)?);
            }
            Ok(Rc::new(Value::Ctor(*tname, *cname, vargs)))
        }
        Term::Match(scrut, branches) => {
            let vs = eval(g, interner, env, scrut)?;
            match_branch(g, interner, env, vs, branches)
        }
        Term::Fix(f, _, body) => match body.as_ref() {
            Term::Lam(x, _, b) => Ok(Rc::new(Value::Lam(
                *x,
                Closure::FixLam {
                    env: env.clone(),
                    self_name: *f,
                    param: *x,
                    body: Rc::clone(b),
                },
            ))),
            _ => {
                let placeholder = Rc::new(Value::Neutral(Neutral::Global(*f)));
                let env2 = env_extend(env, *f, placeholder);
                eval(g, interner, &env2, body)
            }
        },
        Term::Lit(l) => Ok(Rc::new(Value::Lit(l.clone()))),
        Term::Prim(op, args) => {
            let mut vargs = Vec::with_capacity(args.len());
            for a in args {
                vargs.push(eval(g, interner, env, a)?);
            }
            if let Some(v) = eval_prim(*op, &vargs) {
                Ok(v)
            } else {
                Ok(Rc::new(Value::Neutral(Neutral::Prim(*op, vargs))))
            }
        }
        Term::Rec(fs) => {
            let mut vfs = Vec::with_capacity(fs.len());
            for (k, v) in fs {
                vfs.push((*k, eval(g, interner, env, v)?));
            }
            Ok(Rc::new(Value::Rec(vfs)))
        }
        Term::RecTy(fs) => {
            let mut vfs = Vec::with_capacity(fs.len());
            for (k, v) in fs {
                vfs.push((*k, eval(g, interner, env, v)?));
            }
            Ok(Rc::new(Value::RecTy(vfs)))
        }
        Term::Proj(e, field) => {
            let ve = eval(g, interner, env, e)?;
            match ve.as_ref() {
                Value::Rec(fs) => fs
                    .iter()
                    .find(|(k, _)| k == field)
                    .map(|(_, v)| v.clone())
                    .ok_or_else(|| {
                        Error(format!(
                            "NbE: missing record field .{}",
                            interner.resolve(*field)
                        ))
                    }),
                Value::Neutral(n) => Ok(Rc::new(Value::Neutral(Neutral::Proj(
                    Box::new(n.clone()),
                    *field,
                )))),
                _ => Err(Error(format!(
                    "NbE: projected non-record .{}",
                    interner.resolve(*field)
                ))),
            }
        }
        Term::Opaque(name, _) => Ok(Rc::new(Value::Neutral(Neutral::Global(*name)))),
        Term::Hole => {
            let wild = interner.lookup("_").unwrap_or(Sym(0));
            Ok(Rc::new(Value::Neutral(Neutral::Global(wild))))
        }
    }
}

fn fresh_name(interner: &mut Interner, base: Sym, lvl: u32) -> Sym {
    let base_str = interner.resolve(base);
    let prefix = if base_str == "_" { "v" } else { base_str };
    let name = format!("{prefix}#{lvl}");
    interner.intern(&name)
}

pub fn quote(g: &Globals, interner: &mut Interner, lvl: u32, v: &Rc<Value>) -> Result<Term, Error> {
    match v.as_ref() {
        Value::Sort(s) => Ok(Term::Sort(*s)),
        Value::Pi(x, a, clos) => {
            let vn = fresh_name(interner, *x, lvl);
            let va = quote(g, interner, lvl, a)?;
            let arg = Rc::new(Value::Neutral(Neutral::Var(vn, lvl)));
            let body_val = apply_closure(g, interner, clos, arg)?;
            let vb = quote(g, interner, lvl + 1, &body_val)?;
            Ok(Term::Pi(vn, Box::new(va), Rc::new(vb)))
        }
        Value::Lam(x, clos) => {
            let vn = fresh_name(interner, *x, lvl);
            let arg = Rc::new(Value::Neutral(Neutral::Var(vn, lvl)));
            let body_val = apply_closure(g, interner, clos, arg)?;
            let vb = quote(g, interner, lvl + 1, &body_val)?;
            Ok(Term::Lam(vn, None, Rc::new(vb)))
        }
        Value::Ind(name, args) => {
            let mut qargs = Vec::with_capacity(args.len());
            for a in args {
                qargs.push(quote(g, interner, lvl, a)?);
            }
            Ok(Term::Ind(*name, qargs))
        }
        Value::Ctor(tname, cname, args) => {
            let mut qargs = Vec::with_capacity(args.len());
            for a in args {
                qargs.push(quote(g, interner, lvl, a)?);
            }
            Ok(Term::Ctor(*tname, *cname, qargs))
        }
        Value::Lit(l) => Ok(Term::Lit(l.clone())),
        Value::Rec(fs) => {
            let mut qfs = Vec::with_capacity(fs.len());
            for (k, v) in fs {
                qfs.push((*k, quote(g, interner, lvl, v)?));
            }
            Ok(Term::Rec(qfs))
        }
        Value::RecTy(fs) => {
            let mut qfs = Vec::with_capacity(fs.len());
            for (k, v) in fs {
                qfs.push((*k, quote(g, interner, lvl, v)?));
            }
            Ok(Term::RecTy(qfs))
        }
        Value::Neutral(n) => quote_neutral(g, interner, lvl, n),
    }
}

fn quote_neutral(
    g: &Globals,
    interner: &mut Interner,
    lvl: u32,
    n: &Neutral,
) -> Result<Term, Error> {
    match n {
        Neutral::Var(x, _) => Ok(Term::Var(*x)),
        Neutral::Global(s) => Ok(Term::Global(*s)),
        Neutral::App(f, a) => Ok(Term::App(
            Box::new(quote_neutral(g, interner, lvl, f)?),
            Box::new(quote(g, interner, lvl, a)?),
        )),
        Neutral::Match(s, env, branches) => {
            let qs = quote_neutral(g, interner, lvl, s)?;
            let mut qbs = Vec::with_capacity(branches.len());
            for (pat, rhs) in branches.iter() {
                let mut env2 = env.clone();
                let mut bound_count = 0u32;
                match pat {
                    Pattern::Wild | Pattern::Lit(_) => {}
                    Pattern::Var(x) => {
                        env2 =
                            env_extend(&env2, *x, Rc::new(Value::Neutral(Neutral::Var(*x, lvl))));
                        bound_count = 1;
                    }
                    Pattern::Ctor(_, xs) => {
                        for (i, x) in xs.iter().enumerate() {
                            env2 = env_extend(
                                &env2,
                                *x,
                                Rc::new(Value::Neutral(Neutral::Var(*x, lvl + i as u32))),
                            );
                        }
                        bound_count = xs.len() as u32;
                    }
                }
                let ev = eval(g, interner, &env2, rhs)?;
                let qrhs = quote(g, interner, lvl + bound_count, &ev)?;
                qbs.push((pat.clone(), qrhs));
            }
            Ok(Term::Match(Box::new(qs), Rc::new(qbs)))
        }
        Neutral::Proj(e, f) => Ok(Term::Proj(
            Box::new(quote_neutral(g, interner, lvl, e)?),
            *f,
        )),
        Neutral::Prim(op, args) => {
            let mut qargs = Vec::with_capacity(args.len());
            for a in args {
                qargs.push(quote(g, interner, lvl, a)?);
            }
            Ok(Term::Prim(*op, qargs))
        }
    }
}

/// Beta-eta definitional conversion check.
pub fn conv(
    g: &Globals,
    interner: &Interner,
    lvl: u32,
    v1: &Rc<Value>,
    v2: &Rc<Value>,
) -> Result<bool, Error> {
    if Rc::ptr_eq(v1, v2) {
        return Ok(true);
    }
    match (v1.as_ref(), v2.as_ref()) {
        (Value::Sort(s1), Value::Sort(s2)) => Ok(s1 == s2),
        (Value::Lit(l1), Value::Lit(l2)) => Ok(l1 == l2),
        (Value::Ind(t1, a1), Value::Ind(t2, a2)) => {
            if t1 != t2 || a1.len() != a2.len() {
                return Ok(false);
            }
            for (x, y) in a1.iter().zip(a2) {
                if !conv(g, interner, lvl, x, y)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Ctor(t1, c1, a1), Value::Ctor(t2, c2, a2)) => {
            if t1 != t2 || c1 != c2 || a1.len() != a2.len() {
                return Ok(false);
            }
            for (x, y) in a1.iter().zip(a2) {
                if !conv(g, interner, lvl, x, y)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Pi(x, a1, b1), Value::Pi(_, a2, b2)) => {
            if !conv(g, interner, lvl, a1, a2)? {
                return Ok(false);
            }
            let arg = Rc::new(Value::Neutral(Neutral::Var(*x, lvl)));
            let r1 = apply_closure(g, interner, b1, arg.clone())?;
            let r2 = apply_closure(g, interner, b2, arg)?;
            conv(g, interner, lvl + 1, &r1, &r2)
        }
        (Value::Lam(x, b1), Value::Lam(_, b2)) => {
            let arg = Rc::new(Value::Neutral(Neutral::Var(*x, lvl)));
            let r1 = apply_closure(g, interner, b1, arg.clone())?;
            let r2 = apply_closure(g, interner, b2, arg)?;
            conv(g, interner, lvl + 1, &r1, &r2)
        }
        (Value::Lam(x, b1), _) => {
            let arg = Rc::new(Value::Neutral(Neutral::Var(*x, lvl)));
            let r1 = apply_closure(g, interner, b1, arg.clone())?;
            let r2 = vapp(g, interner, v2, arg)?;
            conv(g, interner, lvl + 1, &r1, &r2)
        }
        (_, Value::Lam(x, b2)) => {
            let arg = Rc::new(Value::Neutral(Neutral::Var(*x, lvl)));
            let r1 = vapp(g, interner, v1, arg.clone())?;
            let r2 = apply_closure(g, interner, b2, arg)?;
            conv(g, interner, lvl + 1, &r1, &r2)
        }
        (Value::Rec(fs1), Value::Rec(fs2)) | (Value::RecTy(fs1), Value::RecTy(fs2)) => {
            if fs1.len() != fs2.len() {
                return Ok(false);
            }
            for ((k1, x1), (k2, x2)) in fs1.iter().zip(fs2) {
                if k1 != k2 || !conv(g, interner, lvl, x1, x2)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Neutral(n1), Value::Neutral(n2)) => conv_neutral(g, interner, lvl, n1, n2),
        _ => Ok(false),
    }
}

fn conv_neutral(
    g: &Globals,
    interner: &Interner,
    lvl: u32,
    n1: &Neutral,
    n2: &Neutral,
) -> Result<bool, Error> {
    match (n1, n2) {
        (Neutral::Var(_, l1), Neutral::Var(_, l2)) => Ok(l1 == l2),
        (Neutral::Global(g1), Neutral::Global(g2)) => Ok(g1 == g2),
        (Neutral::App(f1, a1), Neutral::App(f2, a2)) => {
            Ok(conv_neutral(g, interner, lvl, f1, f2)? && conv(g, interner, lvl, a1, a2)?)
        }
        (Neutral::Proj(e1, f1), Neutral::Proj(e2, f2)) => {
            Ok(f1 == f2 && conv_neutral(g, interner, lvl, e1, e2)?)
        }
        (Neutral::Prim(o1, a1), Neutral::Prim(o2, a2)) => {
            if o1 != o2 || a1.len() != a2.len() {
                return Ok(false);
            }
            for (x, y) in a1.iter().zip(a2) {
                if !conv(g, interner, lvl, x, y)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(false),
    }
}

pub fn normalize(g: &Globals, interner: &mut Interner, t: &Term) -> Result<Term, Error> {
    let v = eval(g, interner, &None, t)?;
    quote(g, interner, 0, &v)
}
