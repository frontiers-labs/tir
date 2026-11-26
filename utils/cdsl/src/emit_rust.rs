//! Thin Rocq-Style Rust Printer (`MiniML -> Rust`).
//!
//! Resolves every library type/constant strictly via `sig.extracts (ExRust, _)`.
//! ZERO hardcoded domain names (no "OperationId", "List", "Rewrite", "OpSchema", etc.).

use std::fmt::Write;

use crate::{
    check::Signature,
    core::{ExtractSpec, ExtractTarget, Lit, Neutral, Pattern, PrimOp, Sym, Value},
    erase::{MlDecl, MlExpr, MlProgram},
    nbe,
    tegg::IrSchedule,
};

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

#[inline]
fn sanitize_ident_into(s: &str, out: &mut String) {
    if matches!(s, "loop" | "match" | "type" | "mod" | "fn" | "let" | "in") {
        out.push_str("r#");
        out.push_str(s);
        return;
    }
    if s.bytes().all(|b| b.is_ascii_alphanumeric()) {
        out.push_str(s);
        return;
    }
    let start = out.len();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if matches!(
        &out[start..],
        "loop" | "match" | "type" | "mod" | "fn" | "let" | "in"
    ) {
        out.insert_str(start, "r#");
    }
}

#[inline]
fn rust_const_name_into(s: &str, out: &mut String) {
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_uppercase());
        } else {
            out.push('_');
        }
    }
}

#[inline]
fn lookup_rust_str(sig: &Signature, qname: Sym) -> Option<&str> {
    match sig.extracts.get(&(ExtractTarget::Rust, qname)) {
        Some(ExtractSpec::Const(s))
        | Some(ExtractSpec::Inductive(s, _))
        | Some(ExtractSpec::Tag(s, _)) => Some(s.as_str()),
        None => None,
    }
}

#[inline]
fn lookup_rust_into(sig: &Signature, qname: Sym, out: &mut String) {
    if let Some(s) = lookup_rust_str(sig, qname) {
        out.push_str(s);
    } else {
        sanitize_ident_into(sig.interner.resolve(qname), out);
    }
}

#[inline]
fn lookup_rust_ctor_into(sig: &Signature, tname: Sym, cname: Sym, out: &mut String) {
    if let Some(ExtractSpec::Inductive(_, cs)) = sig.extracts.get(&(ExtractTarget::Rust, tname))
        && let Some((_, target)) = cs.iter().find(|(c, _)| *c == cname)
    {
        out.push_str(target);
        return;
    }
    sanitize_ident_into(sig.interner.resolve(tname), out);
    out.push_str("::");
    out.push_str(sig.interner.resolve(cname));
}

pub(crate) struct ScheduleSyntax {
    pub run_open: &'static str,
    pub sat_run_open: &'static str,
    pub sat_run_close: &'static str,
    pub rep_run_open: &'static str,
    pub rep_run_mid: &'static str,
    pub rep_run_close: &'static str,
    pub seq_open: &'static str,
    pub seq_close: &'static str,
    pub sat_sched_open: &'static str,
    pub rep_sched_open: &'static str,
}

pub(crate) fn emit_schedule_with(sched: &IrSchedule, syn: &ScheduleSyntax, out: &mut String) {
    match sched {
        IrSchedule::Run(rs, 0) => {
            let _ = write!(out, "{}{rs:?}{}", syn.sat_run_open, syn.sat_run_close);
        }
        IrSchedule::Run(rs, 1) => {
            let _ = write!(out, "{}{rs:?})", syn.run_open);
        }
        IrSchedule::Run(rs, n) => {
            let _ = write!(
                out,
                "{}{n}usize{}{rs:?}{}",
                syn.rep_run_open, syn.rep_run_mid, syn.rep_run_close
            );
        }
        IrSchedule::Seq(steps) => {
            out.push_str(syn.seq_open);
            for (i, st) in steps.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_schedule_with(st, syn, out);
            }
            out.push_str(syn.seq_close);
        }
        IrSchedule::Repeat(0, inner) => {
            out.push_str(syn.sat_sched_open);
            emit_schedule_with(inner, syn, out);
            out.push(')');
        }
        IrSchedule::Repeat(n, inner) => {
            let _ = write!(out, "{}{n}usize, ", syn.rep_sched_open);
            emit_schedule_with(inner, syn, out);
            out.push(')');
        }
    }
}

fn emit_schedule_expr(sched: &IrSchedule, out: &mut String) {
    const SYN: ScheduleSyntax = ScheduleSyntax {
        run_open: "crate::tir_cdsl_rt::sched_run(",
        sat_run_open: "crate::tir_cdsl_rt::sched_saturate(",
        sat_run_close: ")",
        rep_run_open: "crate::tir_cdsl_rt::sched_repeat(",
        rep_run_mid: ", ",
        rep_run_close: ")",
        seq_open: "crate::tir_cdsl_rt::sched_seq(vec![",
        seq_close: "])",
        sat_sched_open: "crate::tir_cdsl_rt::sched_saturate_sched(",
        rep_sched_open: "crate::tir_cdsl_rt::sched_repeat_sched(",
    };
    emit_schedule_with(sched, &SYN, out);
}

fn emit_bytes_const(out: &mut String, id: &str, bytes: &[u8]) {
    let mut name = String::with_capacity(id.len());
    rust_const_name_into(id, &mut name);
    let len = bytes.len();
    out.reserve(len * 7 + 160);
    let _ = writeln!(out, "#[repr(C, align(4))]");
    let _ = writeln!(out, "pub struct Aligned_{name}(pub [u8; {len}]);");
    let _ = write!(
        out,
        "pub static {name}_ALIGNED: Aligned_{name} = Aligned_{name}([\n    "
    );
    for (i, &b) in bytes.iter().enumerate() {
        out.push_str("0x");
        out.push(HEX_CHARS[(b >> 4) as usize] as char);
        out.push(HEX_CHARS[(b & 0x0f) as usize] as char);
        out.push(',');
        if (i + 1) % 16 == 0 && i + 1 < len {
            out.push_str("\n    ");
        } else {
            out.push(' ');
        }
    }
    out.push_str("\n]);\n");
    let _ = writeln!(out, "pub const {name}: &[u8] = &{name}_ALIGNED.0;\n");
}

fn emit_expr(sig: &Signature, e: &MlExpr, out: &mut String) {
    match e {
        MlExpr::Erased => out.push_str("()"),
        MlExpr::Var(x) => sanitize_ident_into(sig.interner.resolve(*x), out),
        MlExpr::Global(g) => lookup_rust_into(sig, *g, out),
        MlExpr::Lit(Lit::Int(n)) => {
            let _ = write!(out, "{n}i64");
        }
        MlExpr::Lit(Lit::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
        MlExpr::Lit(Lit::Str(s)) => {
            let _ = write!(out, "{s:?}.to_string()");
        }
        MlExpr::Lit(Lit::Unit) => out.push_str("()"),
        MlExpr::Schedule(sched) => {
            crate::emit_program::emit_schedule_expr(sched, out);
        }
        MlExpr::Bytecode {
            bc_id, bc_schedule, ..
        } => {
            out.push_str("crate::tir_cdsl_rt::CompiledStage { bytes: ");
            rust_const_name_into(bc_id, out);
            out.push_str(", schedule: ");
            emit_schedule_expr(bc_schedule, out);
            out.push_str(" }");
        }
        MlExpr::Lam(x, body) => {
            out.push_str("(|");
            sanitize_ident_into(sig.interner.resolve(*x), out);
            out.push_str("| ");
            emit_expr(sig, body, out);
            out.push(')');
        }
        MlExpr::App(_, _) => {
            let mut args = Vec::with_capacity(4);
            let mut cur = e;
            while let MlExpr::App(f, a) = cur {
                args.push(a.as_ref());
                cur = f.as_ref();
            }
            args.reverse();
            if let MlExpr::Global(g) = cur
                && args.len() == 1
                && lookup_rust_str(sig, *g) == Some("@try")
            {
                out.push('(');
                emit_expr(sig, args[0], out);
                out.push_str(")?");
                return;
            }
            emit_expr(sig, cur, out);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_expr(sig, a, out);
            }
            out.push(')');
        }
        MlExpr::Let(x, rhs, body) => {
            let xs = sig.interner.resolve(*x);
            if xs == "_" {
                out.push_str("{\n    ");
                emit_expr(sig, rhs, out);
                out.push_str(";\n    ");
                emit_expr(sig, body, out);
                out.push_str("\n}");
            } else {
                out.push_str("{\n    let ");
                sanitize_ident_into(xs, out);
                out.push_str(" = ");
                emit_expr(sig, rhs, out);
                out.push_str(";\n    ");
                emit_expr(sig, body, out);
                out.push_str("\n}");
            }
        }
        MlExpr::Ctor(tname, cname, args) => {
            lookup_rust_ctor_into(sig, *tname, *cname, out);
            if !args.is_empty() {
                out.push('(');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    emit_expr(sig, a, out);
                }
                out.push(')');
            }
        }
        MlExpr::Prim(op, args) => match (op, args.as_slice()) {
            (PrimOp::Not, [a]) => {
                out.push_str("(!(");
                emit_expr(sig, a, out);
                out.push_str("))");
            }
            (PrimOp::StrCat, [a, b]) => {
                out.push_str("format!(\"{}{}\", ");
                emit_expr(sig, a, out);
                out.push_str(", ");
                emit_expr(sig, b, out);
                out.push(')');
            }
            (_, [a, b]) => {
                let sym = match op {
                    PrimOp::Add => "+",
                    PrimOp::Sub => "-",
                    PrimOp::Mul => "*",
                    PrimOp::Div => "/",
                    PrimOp::Rem => "%",
                    PrimOp::Eq => "==",
                    PrimOp::Neq => "!=",
                    PrimOp::Lt => "<",
                    PrimOp::Le => "<=",
                    PrimOp::Gt => ">",
                    PrimOp::Ge => ">=",
                    PrimOp::And => "&&",
                    PrimOp::Or => "||",
                    _ => unreachable!(),
                };
                out.push('(');
                emit_expr(sig, a, out);
                out.push(' ');
                out.push_str(sym);
                out.push(' ');
                emit_expr(sig, b, out);
                out.push(')');
            }
            _ => out.push_str("()"),
        },
        MlExpr::Rec(fs) => {
            out.push('(');
            for (i, (_, v)) in fs.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_expr(sig, v, out);
            }
            out.push(')');
        }
        MlExpr::Proj(obj, f) => {
            emit_expr(sig, obj, out);
            out.push('.');
            sanitize_ident_into(sig.interner.resolve(*f), out);
        }
        MlExpr::Match(scrut, branches) => {
            out.push_str("match ");
            emit_expr(sig, scrut, out);
            out.push_str(" {\n");
            for (pat, rhs) in branches {
                out.push_str("        ");
                match pat {
                    Pattern::Wild => out.push('_'),
                    Pattern::Var(x) => sanitize_ident_into(sig.interner.resolve(*x), out),
                    Pattern::Lit(Lit::Int(n)) => {
                        let _ = write!(out, "{n}");
                    }
                    Pattern::Lit(Lit::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
                    Pattern::Lit(Lit::Str(s)) => {
                        let _ = write!(out, "{s:?}");
                    }
                    Pattern::Lit(Lit::Unit) => out.push_str("()"),
                    Pattern::Ctor(c, xs) => {
                        out.push_str(sig.interner.resolve(*c));
                        if !xs.is_empty() {
                            out.push('(');
                            for (i, x) in xs.iter().enumerate() {
                                if i > 0 {
                                    out.push_str(", ");
                                }
                                sanitize_ident_into(sig.interner.resolve(*x), out);
                            }
                            out.push(')');
                        }
                    }
                }
                out.push_str(" => ");
                emit_expr(sig, rhs, out);
                out.push_str(",\n");
            }
            out.push_str("    }");
        }
        MlExpr::Fix(_, body) => emit_expr(sig, body, out),
    }
}

fn emit_rust_type(sig: &Signature, ty: &Value, out: &mut String) {
    match ty {
        Value::Ind(name, args) => {
            if let Some(s) = lookup_rust_str(sig, *name) {
                out.push_str(s);
            } else if *name == sig.sym_int {
                out.push_str("i64");
            } else if *name == sig.sym_bool {
                out.push_str("bool");
            } else if *name == sig.sym_str {
                out.push_str("String");
            } else if *name == sig.sym_unit {
                out.push_str("()");
            } else {
                sanitize_ident_into(sig.interner.resolve(*name), out);
            }
            if !args.is_empty() {
                out.push('<');
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    emit_rust_type(sig, a, out);
                }
                out.push('>');
            }
        }
        _ => out.push_str("()"),
    }
}

pub fn emit_program(sig: &Signature, ml: &MlProgram) -> String {
    emit_program_in_module(sig, ml, None)
}

pub fn emit_program_in_module(
    sig: &Signature,
    ml: &MlProgram,
    strip_prefix: Option<&str>,
) -> String {
    let bc_est: usize = ml.bytecodes.iter().map(|(_, b, _)| b.len() * 7 + 160).sum();
    let mut out = String::with_capacity(ml.decls.len() * 192 + bc_est + 512);
    out.push_str("// Automatically extracted from CDSL v3 via Rocq-style MiniML erasure.\n");
    out.push_str("// DO NOT EDIT.\n\n");
    out.push_str("#![allow(dead_code, unused_variables, unused_parens, non_camel_case_types, non_snake_case)]\n\n");

    // 1. Static TEGG v15 bytecode blobs
    for (id, bytes, _egg) in &ml.bytecodes {
        emit_bytes_const(&mut out, id, bytes);
    }

    // 2. Emitted declarations
    for decl in &ml.decls {
        match decl {
            MlDecl::Ind(ind) => {
                let is_copy = ind.ictors.iter().all(|c| c.cparams.is_empty());
                if is_copy {
                    out.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub enum ");
                } else {
                    out.push_str("#[derive(Clone, Debug, PartialEq, Eq)]\npub enum ");
                }
                let raw_name = sig.interner.resolve(ind.iname);
                let short_name = strip_prefix
                    .and_then(|p| raw_name.strip_prefix(p))
                    .and_then(|r| r.strip_prefix("::"))
                    .unwrap_or(raw_name);
                sanitize_ident_into(short_name, &mut out);
                out.push_str(" {\n");
                for c in &ind.ictors {
                    let cname = sig.interner.resolve(c.cname);
                    if c.cparams.is_empty() {
                        out.push_str("    ");
                        out.push_str(cname);
                        out.push_str(",\n");
                    } else {
                        out.push_str("    ");
                        out.push_str(cname);
                        out.push('(');
                        for (i, (_, ft)) in c.cparams.iter().enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            if let Ok(vt) = nbe::eval(&sig.values, &sig.interner, &None, ft) {
                                emit_rust_type(sig, &vt, &mut out);
                            } else {
                                out.push_str("()");
                            }
                        }
                        out.push_str("),\n");
                    }
                }
                out.push_str("}\n\n");
            }
            MlDecl::OpDef(op) => {
                let r = &op.op_rust_name;
                let _ = writeln!(out, "#[derive(Clone, Copy, Debug, PartialEq, Eq)]");
                let _ = writeln!(out, "pub struct {r};");
                let _ = writeln!(out, "impl {r} {{");
                let _ = writeln!(
                    out,
                    "    pub const DIALECT: &'static str = {:?};",
                    op.op_dialect
                );
                let _ = writeln!(out, "    pub const NAME: &'static str = {:?};", op.op_name);
                let _ = writeln!(
                    out,
                    "    pub const TAG: &'static str = \"{}.{}\";",
                    op.op_dialect, op.op_name
                );
                out.push_str("    pub const INTERFACES: &'static [&'static str] = &[");
                for (i, iface) in op.op_interfaces.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "{iface:?}");
                }
                out.push_str("];\n}\n\n");
            }
            MlDecl::Memo {
                name,
                param,
                bc_id,
                schedule,
                outputs,
                ..
            } => {
                let raw_name = sig.interner.resolve(*name);
                let mut fn_name = String::with_capacity(raw_name.len());
                sanitize_ident_into(raw_name, &mut fn_name);
                let mut param_name = String::new();
                sanitize_ident_into(sig.interner.resolve(*param), &mut param_name);
                let res_ty = format!("{fn_name}_result");
                let mut bc_const = String::with_capacity(bc_id.len());
                rust_const_name_into(bc_id, &mut bc_const);
                let _ = writeln!(out, "#[derive(Clone, Debug)]\npub struct {res_ty} {{");
                for (field, _) in outputs {
                    out.push_str("    pub ");
                    sanitize_ident_into(sig.interner.resolve(*field), &mut out);
                    out.push_str(": &'static str,\n");
                }
                out.push_str("}\n");
                let _ = write!(
                    out,
                    "pub fn {fn_name}(db: &mut crate::tir_cdsl_rt::Session, {param_name}: i64) -> ::core::result::Result<{res_ty}, String> {{\n    crate::tir_cdsl_rt::run_memo(db, {raw_name:?}, {bc_const}, "
                );
                emit_schedule_expr(schedule, &mut out);
                let _ = writeln!(out, ", {param_name})?;");
                let _ = write!(out, "    Ok({res_ty} {{ ");
                for (i, (field, tbl)) in outputs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    sanitize_ident_into(sig.interner.resolve(*field), &mut out);
                    let _ = write!(out, ": {tbl:?}");
                }
                out.push_str(" })\n}\n\n");
            }
            MlDecl::Def(name, body) => {
                let mut rname = String::new();
                sanitize_ident_into(sig.interner.resolve(*name), &mut rname);
                emit_driver_fn(sig, *name, &rname, body, &mut out);
            }
            _ => {}
        }
    }

    out
}

pub(crate) fn emit_driver_fn(
    sig: &Signature,
    name: Sym,
    rust_name: &str,
    body: &MlExpr,
    out: &mut String,
) {
    out.push_str("#[allow(unused_variables, unused_parens, clippy::all)]\npub fn ");
    out.push_str(rust_name);
    out.push('(');
    let mut cur_e = body;
    let mut cur_ty = sig.types[&name].clone();
    let mut first = true;
    while let (MlExpr::Lam(x, b), Value::Pi(_, dom, cod)) = (cur_e, cur_ty.as_ref().clone()) {
        if !first {
            out.push_str(", ");
        }
        first = false;
        sanitize_ident_into(sig.interner.resolve(*x), out);
        out.push_str(": ");
        emit_rust_type(sig, &dom, out);
        let next_v = std::rc::Rc::new(Value::Neutral(Neutral::Global(*x)));
        cur_ty = nbe::apply_closure(&sig.values, &sig.interner, &cod, next_v)
            .unwrap_or_else(|_| sig.val_unit.clone());
        cur_e = b;
    }
    out.push_str(") -> ");
    emit_rust_type(sig, &cur_ty, out);
    out.push_str(" {\n    ");
    emit_expr(sig, cur_e, out);
    out.push_str("\n}\n\n");
}
