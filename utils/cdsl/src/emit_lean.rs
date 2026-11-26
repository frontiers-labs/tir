//! Lean 4 Proof-Skeleton Printer (from UNERASED Kernel AST).
//!
//! Skips `erase.rs` completely so `Prop` obligations emit as `theorem`s and
//! computational definitions / op `exec` semantics remain intact.

use std::fmt::Write;

use crate::{
    check::Signature,
    core::{Decl, ExtractSpec, ExtractTarget, Lit, PrimOp, Sort, Term},
    nbe,
};

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn lean_term(sig: &Signature, t: &Term, out: &mut String) {
    match t {
        Term::Var(x) | Term::Opaque(x, _) => out.push_str(&sanitize(sig.interner.resolve(*x))),
        Term::Global(g) => {
            if let Some(ExtractSpec::Const(s)) = sig.extracts.get(&(ExtractTarget::Lean, *g)) {
                out.push_str(s);
            } else {
                out.push_str(&sanitize(sig.interner.resolve(*g)));
            }
        }
        Term::Sort(Sort::Type) => out.push_str("Type"),
        Term::Sort(Sort::Prop) => out.push_str("Prop"),
        Term::Pi(x, a, b) => {
            let xs = sig.interner.resolve(*x);
            if xs == "_" {
                out.push('(');
                lean_term(sig, a, out);
                out.push_str(" -> ");
                lean_term(sig, b, out);
                out.push(')');
            } else {
                out.push_str("(forall (");
                out.push_str(&sanitize(xs));
                out.push_str(" : ");
                lean_term(sig, a, out);
                out.push_str("), ");
                lean_term(sig, b, out);
                out.push(')');
            }
        }
        Term::Lam(x, ann, b) => {
            out.push_str("(fun ");
            if let Some(a) = ann {
                out.push('(');
                out.push_str(&sanitize(sig.interner.resolve(*x)));
                out.push_str(" : ");
                lean_term(sig, a, out);
                out.push(')');
            } else {
                out.push_str(&sanitize(sig.interner.resolve(*x)));
            }
            out.push_str(" => ");
            lean_term(sig, b, out);
            out.push(')');
        }
        Term::App(f, a) => {
            out.push('(');
            lean_term(sig, f, out);
            out.push(' ');
            lean_term(sig, a, out);
            out.push(')');
        }
        Term::Let(x, _, v, b) => {
            out.push_str("(let ");
            out.push_str(&sanitize(sig.interner.resolve(*x)));
            out.push_str(" := ");
            lean_term(sig, v, out);
            out.push_str("; ");
            lean_term(sig, b, out);
            out.push(')');
        }
        Term::Ind(t, xs) => {
            let head = match sig.extracts.get(&(ExtractTarget::Lean, *t)) {
                Some(ExtractSpec::Inductive(s, _)) | Some(ExtractSpec::Const(s)) => s.clone(),
                _ => sanitize(sig.interner.resolve(*t)),
            };
            if xs.is_empty() {
                out.push_str(&head);
            } else {
                out.push('(');
                out.push_str(&head);
                for x in xs {
                    out.push(' ');
                    lean_term(sig, x, out);
                }
                out.push(')');
            }
        }
        Term::Ctor(t, c, xs) => {
            let cname = match sig.extracts.get(&(ExtractTarget::Lean, *t)) {
                Some(ExtractSpec::Inductive(_, cs)) => cs
                    .iter()
                    .find(|(k, _)| k == c)
                    .map(|(_, s)| s.clone())
                    .unwrap_or_else(|| {
                        format!(
                            "{}.{}",
                            sanitize(sig.interner.resolve(*t)),
                            sig.interner.resolve(*c)
                        )
                    }),
                _ => format!(
                    "{}.{}",
                    sanitize(sig.interner.resolve(*t)),
                    sig.interner.resolve(*c)
                ),
            };
            if xs.is_empty() {
                out.push_str(&cname);
            } else {
                out.push('(');
                out.push_str(&cname);
                for x in xs {
                    out.push(' ');
                    lean_term(sig, x, out);
                }
                out.push(')');
            }
        }
        Term::Lit(Lit::Int(n)) => {
            let _ = write!(out, "{n}");
        }
        Term::Lit(Lit::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
        Term::Lit(Lit::Str(s)) => {
            let _ = write!(out, "{s:?}");
        }
        Term::Lit(Lit::Unit) => out.push_str("()"),
        Term::Prim(PrimOp::Not, args) if args.len() == 1 => {
            out.push_str("(! ");
            lean_term(sig, &args[0], out);
            out.push(')');
        }
        Term::Prim(op, args) if args.len() == 2 => {
            let s = match op {
                PrimOp::Add => "+",
                PrimOp::Sub => "-",
                PrimOp::Mul => "*",
                PrimOp::Div => "/",
                PrimOp::Rem => "%",
                PrimOp::Eq => "=",
                PrimOp::Neq => "!=",
                PrimOp::Lt => "<",
                PrimOp::Le => "<=",
                PrimOp::Gt => ">",
                PrimOp::Ge => ">=",
                PrimOp::And => "&&",
                PrimOp::Or => "||",
                PrimOp::StrCat => "++",
                PrimOp::Not => "!",
            };
            out.push('(');
            lean_term(sig, &args[0], out);
            out.push(' ');
            out.push_str(s);
            out.push(' ');
            lean_term(sig, &args[1], out);
            out.push(')');
        }
        Term::Rec(fs) => {
            out.push('⟨');
            for (i, (_, v)) in fs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                lean_term(sig, v, out);
            }
            out.push('⟩');
        }
        Term::Proj(e, f) => {
            lean_term(sig, e, out);
            out.push('.');
            out.push_str(sig.interner.resolve(*f));
        }
        Term::Match(s, _) => {
            out.push_str("match ");
            lean_term(sig, s, out);
            out.push_str(" with | _ => sorry");
        }
        Term::Fix(_, _, b) => lean_term(sig, b, out),
        Term::RecTy(_) => out.push_str("Type"),
        Term::Hole => out.push('_'),
        Term::Prim(_, _) => out.push_str("sorry"),
    }
}

pub fn emit_lean(sig: &Signature) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("-- Lean 4 proof skeleton extracted from UNERASED CDSL v3 kernel AST.\n");
    out.push_str("-- Preserves Prop obligations and op exec semantics.\n\n");
    for d in &sig.decl_order {
        match d {
            Decl::Ind(ind) if !sig.extracts.contains_key(&(ExtractTarget::Lean, ind.iname)) => {
                let sort_s = match ind.isort {
                    Sort::Type => "Type",
                    Sort::Prop => "Prop",
                };
                out.push_str("inductive ");
                out.push_str(&sanitize(sig.interner.resolve(ind.iname)));
                for (x, t) in &ind.iparams {
                    out.push_str(" (");
                    out.push_str(&sanitize(sig.interner.resolve(*x)));
                    out.push_str(" : ");
                    lean_term(sig, t, &mut out);
                    out.push(')');
                }
                let _ = writeln!(out, " : {sort_s} where");
                for c in &ind.ictors {
                    out.push_str("  | ");
                    out.push_str(sig.interner.resolve(c.cname));
                    for (x, t) in &c.cparams {
                        out.push_str(" (");
                        out.push_str(&sanitize(sig.interner.resolve(*x)));
                        out.push_str(" : ");
                        lean_term(sig, t, &mut out);
                        out.push(')');
                    }
                    out.push_str(" : ");
                    lean_term(sig, &c.cret, &mut out);
                    out.push('\n');
                }
                out.push('\n');
            }
            Decl::Def(name, Some(ty), body) => {
                let is_prop = nbe::eval(&sig.values, &sig.interner, &None, ty)
                    .is_ok_and(|vty| sig.is_prop_type(0, &vty));
                let kw = if is_prop { "theorem" } else { "def" };
                out.push_str(kw);
                out.push(' ');
                out.push_str(&sanitize(sig.interner.resolve(*name)));
                out.push_str(" : ");
                lean_term(sig, ty, &mut out);
                out.push_str(" :=\n  ");
                lean_term(sig, body, &mut out);
                out.push_str("\n\n");
            }
            _ => {}
        }
    }
    out
}
