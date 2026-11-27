//! Direct Rust printer for typed `MlOpDef` declarations (`OutputKind::OpDef`).
//!
//! Emits TIR `OpSchema` (in `tir::OP_SCHEMAS`), `tir::schema::Signature`
//! (with exact de Bruijn `Bound(i)` indices derived directly from the kernel's
//! dependent types), `OpHandle` structs, `Operation`, `NativeDeclaration`,
//! `ImplementsOpInterface`, builders, constructors, generic parser/printer,
//! `Theta`/`Gamma`/`CountedLoop`, and immediate validators — with ZERO string
//! templates in `.cdsl` files!

use std::fmt::Write;

use crate::{
    check::Signature,
    core::{Lit, Sym, Term},
    erase::{
        MlAttr, MlBind, MlConstraintArg, MlCounted, MlDecl, MlImm, MlInterface, MlOpDef,
        MlParamKind, MlPort, MlProgram, MlRegion, PortArity,
    },
};

fn escape_rust_ident(s: &str) -> String {
    match s {
        "as" | "async" | "await" | "break" | "const" | "continue" | "crate" | "dyn" | "else"
        | "enum" | "extern" | "false" | "fn" | "for" | "if" | "in" | "let" | "loop" | "match"
        | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "self" | "Self" | "static"
        | "struct" | "super" | "trait" | "true" | "type" | "unsafe" | "use" | "where" | "while"
        | "yield" | "abstract" | "become" | "box" | "do" | "final" | "macro" | "override"
        | "priv" | "try" | "typeof" | "unsized" | "virtual" => {
            format!("r#{s}")
        }
        _ => s.to_owned(),
    }
}

fn leaf_name(sig: &Signature, sym: Sym) -> &str {
    let s = sig.interner.resolve(sym);
    s.rsplit("::").next().unwrap_or(s)
}

fn is_state_port(p: &MlPort) -> bool {
    p.is_state
}

fn format_field_ty(sig: &Signature, p: &MlPort, widened: bool) -> String {
    if p.is_state {
        return "*state<_>".to_owned();
    }
    let base = if widened {
        "_".to_owned()
    } else {
        format_ir_type_str(sig, &p.ty)
    };
    match p.arity {
        PortArity::Fixed => base,
        PortArity::Optional => format!("?{base}"),
        PortArity::Variadic => format!("*{base}"),
    }
}

fn format_ir_type_str(sig: &Signature, t: &Term) -> String {
    match t {
        Term::Var(x) => sig.interner.resolve(*x).to_ascii_uppercase(),
        Term::Lit(Lit::Int(n)) => n.to_string(),
        Term::Ind(sym, args) => {
            let nm = leaf_name(sig, *sym);
            match (nm, args.as_slice()) {
                ("Any", []) => "_".to_owned(),
                ("State", []) => "state<_>".to_owned(),
                ("Index", []) => "index".to_owned(),
                ("UnitTy", []) | ("Unit", []) => "unit".to_owned(),
                ("BackendReg", []) => "backend.reg<_, _>".to_owned(),
                ("VaList", []) => "cir.va_list".to_owned(),
                ("El", [inner])
                | ("Counter", [inner])
                | ("Option", [inner])
                | ("Optional", [inner])
                | ("Variadic", [inner]) => format_ir_type_str(sig, inner),
                ("Bits", [w]) => format!("int<{}>", format_ir_type_str(sig, w)),
                ("Ptr", [_]) => "ptr.p<_>".to_owned(),
                ("Float", [f]) => format_ir_type_str(sig, f),
                ("Computation", [f, r, c]) => format!(
                    "fp.computation<{}, {}, {}>",
                    format_ir_type_str(sig, f),
                    format_ir_type_str(sig, r),
                    format_ir_type_str(sig, c)
                ),
                ("Vector", [elem, n]) => format!(
                    "vector.v<{}, {}>",
                    format_ir_type_str(sig, elem),
                    format_ir_type_str(sig, n)
                ),
                ("Array", [elem, n]) => format!(
                    "array<{}, {}>",
                    format_ir_type_str(sig, elem),
                    format_ir_type_str(sig, n)
                ),
                ("Tuple", [_]) => "tuple<_>".to_owned(),
                ("Function", [_, _]) => "fn<_, _>".to_owned(),
                _ if args.is_empty() => nm.to_lowercase(),
                _ => {
                    let sargs: Vec<String> =
                        args.iter().map(|a| format_ir_type_str(sig, a)).collect();
                    format!("{}<{}>", nm.to_lowercase(), sargs.join(", "))
                }
            }
        }
        _ => "_".to_owned(),
    }
}

fn format_attr_ty_str(sig: &Signature, attr: &MlAttr) -> (&'static str, &'static str) {
    let (base, vocab) = match &attr.ty {
        Term::Ind(sym, args) => {
            let nm = leaf_name(sig, *sym);
            match (nm, args.as_slice()) {
                ("Int", []) | ("Nat", []) => ("Int", "&[]"),
                ("UInt", []) => ("UInt", "&[]"),
                ("Bool", []) | ("Flag", []) => {
                    if attr.optional {
                        return ("?Flag", "&[]");
                    }
                    ("Bool", "&[]")
                }
                ("String", []) | ("Str", []) => ("Str", "&[]"),
                ("Symbol", []) => ("Symbol", "&[]"),
                ("TypeAttr", []) | ("DataType", []) => ("Type", "&[]"),
                ("BytesAttr", []) => ("Bytes", "&[]"),
                ("IntArray", []) => ("IntArray", "&[]"),
                ("StrArray", []) => ("StrArray", "&[]"),
                ("IntTuple", []) => ("IntTuple", "&[]"),
                ("NamedAttr", []) => ("Named", "&[]"),
                ("DictAttr", []) => ("Dict", "&[]"),
                ("ArrayAttr", []) => ("Array", "&[]"),
                ("Register", []) => ("Register", "&[]"),
                ("Block", []) => ("Block", "&[]"),
                ("IntegerPredicate", []) => ("Predicate", "tir::attributes::Predicate::INTEGER"),
                ("FloatPredicate", []) => ("Predicate", "tir::attributes::Predicate::FLOAT"),
                ("Predicate", [v]) => {
                    let voc = match v {
                        Term::Ind(vs, _) => match leaf_name(sig, *vs) {
                            "FloatPred" | "Float" => "tir::attributes::Predicate::FLOAT",
                            "PointerPred" | "Pointer" => "tir::attributes::Predicate::POINTER",
                            _ => "tir::attributes::Predicate::INTEGER",
                        },
                        _ => "tir::attributes::Predicate::INTEGER",
                    };
                    ("Predicate", voc)
                }
                ("Any", []) => ("any", "&[]"),
                _ => ("any", "&[]"),
            }
        }
        Term::Sort(_) => ("Type", "&[]"),
        Term::Global(sym) => match leaf_name(sig, *sym) {
            "Nat" => ("Int", "&[]"),
            _ => ("any", "&[]"),
        },
        _ => ("any", "&[]"),
    };
    if attr.optional {
        let opt_ty = match base {
            "Int" => "?Int",
            "UInt" => "?UInt",
            "Bool" => "?Flag",
            "Str" => "?Str",
            "Type" => "?Type",
            "Symbol" => "?Symbol",
            "Predicate" => "?Predicate",
            "Bytes" => "?Bytes",
            "IntArray" => "?IntArray",
            "StrArray" => "?StrArray",
            "IntTuple" => "?IntTuple",
            "Dict" => "?Dict",
            "Array" => "?Array",
            "Register" => "?Register",
            _ => "?any",
        };
        (opt_ty, vocab)
    } else {
        (base, vocab)
    }
}

fn emit_pattern(
    sig: &Signature,
    t: &Term,
    tparams: &[(String, MlParamKind)],
    widened: bool,
    out: &mut String,
) {
    if widened {
        out.push_str("tir::schema::Pattern::Wildcard");
        return;
    }
    match t {
        Term::Var(x) => {
            let name = sig.interner.resolve(*x);
            if let Some(idx) = tparams.iter().position(|(pn, _)| pn == name) {
                let _ = write!(out, "tir::schema::Pattern::Bound({idx}usize)");
            } else {
                out.push_str("tir::schema::Pattern::Wildcard");
            }
        }
        Term::Lit(Lit::Int(n)) => {
            let _ = write!(out, "tir::schema::Pattern::Nat({n}u64)");
        }
        Term::Ind(sym, args) => {
            let nm = leaf_name(sig, *sym);
            match (nm, args.as_slice()) {
                ("Any", []) => out.push_str("tir::schema::Pattern::Wildcard"),
                ("State", []) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"builtin\", \"state\", &[tir::schema::Pattern::Wildcard])");
                }
                ("Index", []) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"builtin\", \"index\", &[])");
                }
                ("UnitTy", []) | ("Unit", []) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"builtin\", \"unit\", &[])");
                }
                ("BackendReg", []) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"backend\", \"reg\", &[tir::schema::Pattern::Wildcard, tir::schema::Pattern::Wildcard])");
                }
                ("VaList", []) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"cir\", \"va_list\", &[])");
                }
                ("El", [inner])
                | ("Counter", [inner])
                | ("Option", [inner])
                | ("Optional", [inner])
                | ("Variadic", [inner]) => emit_pattern(sig, inner, tparams, false, out),
                ("Bits", [w]) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"builtin\", \"i\", &[");
                    emit_pattern(sig, w, tparams, false, out);
                    out.push_str("])");
                }
                ("Ptr", [_]) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"ptr\", \"p\", &[tir::schema::Pattern::Wildcard])");
                }
                ("Float", [f]) => emit_pattern(sig, f, tparams, false, out),
                ("Computation", [f, r, c]) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"fp\", \"computation\", &[");
                    emit_pattern(sig, f, tparams, false, out);
                    out.push_str(", ");
                    emit_pattern(sig, r, tparams, false, out);
                    out.push_str(", ");
                    emit_pattern(sig, c, tparams, false, out);
                    out.push_str("])");
                }
                ("Vector", [elem, n]) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"vector\", \"v\", &[");
                    emit_pattern(sig, elem, tparams, false, out);
                    out.push_str(", ");
                    emit_pattern(sig, n, tparams, false, out);
                    out.push_str("])");
                }
                ("Tuple", [_]) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"builtin\", \"tuple\", &[tir::schema::Pattern::Wildcard])");
                }
                ("Function", [_, _]) => {
                    out.push_str("tir::schema::Pattern::Constructor(\"builtin\", \"fn\", &[tir::schema::Pattern::Wildcard, tir::schema::Pattern::Wildcard])");
                }
                _ => out.push_str("tir::schema::Pattern::Wildcard"),
            }
        }
        _ => out.push_str("tir::schema::Pattern::Wildcard"),
    }
}

fn emit_signature_const(sig: &Signature, op: &MlOpDef, out: &mut String) {
    out.push_str("&tir::schema::Signature { binders: &[");
    if !op.widened {
        for (i, (_, pk)) in op.tparams.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            match pk {
                MlParamKind::Nat => out.push_str("tir::schema::BinderKind::Nat"),
                MlParamKind::Data => out.push_str("tir::schema::BinderKind::Data"),
                MlParamKind::Sym => out.push_str("tir::schema::BinderKind::Sym"),
                MlParamKind::FloatFormat => {
                    out.push_str("tir::schema::BinderKind::Shapes(&[\
                        tir::schema::Pattern::Constructor(\"builtin\", \"f\", &[tir::schema::Pattern::Nat(5u64), tir::schema::Pattern::Nat(10u64)]), \
                        tir::schema::Pattern::Constructor(\"builtin\", \"f\", &[tir::schema::Pattern::Nat(8u64), tir::schema::Pattern::Nat(23u64)]), \
                        tir::schema::Pattern::Constructor(\"builtin\", \"f\", &[tir::schema::Pattern::Nat(11u64), tir::schema::Pattern::Nat(52u64)])\
                    ])");
                }
                MlParamKind::IndexOrInt => {
                    out.push_str("tir::schema::BinderKind::Shapes(&[\
                        tir::schema::Pattern::Constructor(\"builtin\", \"index\", &[]), \
                        tir::schema::Pattern::Constructor(\"builtin\", \"i\", &[tir::schema::Pattern::Wildcard])\
                    ])");
                }
            }
        }
    }
    out.push_str("], inputs: &[");
    for (i, p) in op.operands.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if p.is_state {
            out.push_str("tir::schema::Port::Many(tir::schema::Pattern::Constructor(\"builtin\", \"state\", &[tir::schema::Pattern::Wildcard]))");
            continue;
        }
        let kind = match p.arity {
            PortArity::Fixed => "Fixed",
            PortArity::Optional => "Optional",
            PortArity::Variadic => "Many",
        };
        let _ = write!(out, "tir::schema::Port::{kind}(");
        emit_pattern(sig, &p.ty, &op.tparams, op.widened, out);
        out.push(')');
    }
    out.push_str("], outputs: &[");
    for (i, p) in op.results.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        if p.is_state {
            out.push_str("tir::schema::Port::Many(tir::schema::Pattern::Constructor(\"builtin\", \"state\", &[tir::schema::Pattern::Wildcard]))");
            continue;
        }
        let kind = match p.arity {
            PortArity::Fixed => "Fixed",
            PortArity::Optional => "Optional",
            PortArity::Variadic => "Many",
        };
        let _ = write!(out, "tir::schema::Port::{kind}(");
        emit_pattern(sig, &p.ty, &op.tparams, op.widened, out);
        out.push(')');
    }
    out.push_str("] }");
}

fn format_scheme_str(sig: &Signature, op: &MlOpDef) -> String {
    let mut s = String::new();
    if !op.widened && !op.tparams.is_empty() {
        s.push_str("forall ");
        for (i, (pn, pk)) in op.tparams.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            let up = pn.to_ascii_uppercase();
            match pk {
                MlParamKind::Nat => {
                    let _ = write!(s, "{up}: nat");
                }
                MlParamKind::Data => {
                    let _ = write!(s, "{up}: data");
                }
                MlParamKind::Sym => {
                    let _ = write!(s, "{up}: sym");
                }
                MlParamKind::FloatFormat => {
                    let _ = write!(s, "{up}: float<5, 10> | float<8, 23> | float<11, 52>");
                }
                MlParamKind::IndexOrInt => {
                    let _ = write!(s, "{up}: index | int<_>");
                }
            }
        }
        s.push_str(". ");
    }
    s.push('(');
    for (i, p) in op.operands.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let ty = format_field_ty(sig, p, op.widened);
        if let Some(rest) = ty.strip_prefix('*') {
            let _ = write!(s, "{rest}*");
        } else if let Some(rest) = ty.strip_prefix('?') {
            let _ = write!(s, "{rest}?");
        } else {
            s.push_str(&ty);
        }
    }
    s.push_str(") -> (");
    for (i, p) in op.results.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        let ty = format_field_ty(sig, p, op.widened);
        if let Some(rest) = ty.strip_prefix('*') {
            let _ = write!(s, "{rest}*");
        } else if let Some(rest) = ty.strip_prefix('?') {
            let _ = write!(s, "{rest}?");
        } else {
            s.push_str(&ty);
        }
    }
    s.push(')');
    s
}

fn emit_field_schemas(sig: &Signature, ports: &[MlPort], widened: bool, out: &mut String) {
    out.push_str("&[");
    for (i, p) in ports.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let ty = format_field_ty(sig, p, widened);
        let variadic = p.is_state || matches!(p.arity, PortArity::Variadic);
        let _ = write!(
            out,
            "tir::FieldSchema {{ name: {:?}, ty: {:?}, variadic: {variadic} }}",
            p.name, ty
        );
    }
    out.push(']');
}

fn emit_attr_schemas(sig: &Signature, attrs: &[MlAttr], out: &mut String) {
    out.push_str("&[");
    for (i, a) in attrs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let (ty, vocab) = format_attr_ty_str(sig, a);
        let _ = write!(
            out,
            "tir::AttrSchema {{ name: {:?}, ty: {:?}, vocabulary: {vocab} }}",
            a.name, ty
        );
    }
    out.push(']');
}

fn emit_region_schemas(regions: &[MlRegion], out: &mut String) {
    out.push_str("&[");
    for (i, r) in regions.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let rk = match r.kind.as_str() {
            "Nodes" => "tir::RegionKind::Nodes",
            "Graph" => "tir::RegionKind::Graph",
            "Any" => "tir::RegionKind::Any",
            _ => "tir::RegionKind::Blocks",
        };
        let _ = write!(out, "({rk}, {})", r.variadic);
    }
    out.push(']');
}

fn interface_trait_info(iface: &MlInterface) -> Option<(&'static str, &'static str, bool)> {
    // Returns (schema_name, trait_path, auto_blanket_impl)
    match iface {
        MlInterface::Pure | MlInterface::PureInterface => {
            Some(("PureInterface", "tir::Pure", true))
        }
        MlInterface::ConstantLike | MlInterface::ConstantLikeExternal => {
            Some(("ConstantLike", "tir::ConstantLike", false))
        }
        MlInterface::Speculatable => Some(("Speculatable", "tir::Speculatable", true)),
        MlInterface::Commutative => Some(("Commutative", "tir::Commutative", true)),
        MlInterface::SameOperandAndResultType => Some((
            "SameOperandAndResultType",
            "tir::SameOperandAndResultType",
            true,
        )),
        MlInterface::IntegerArithmetic => {
            Some(("IntegerArithmetic", "tir::IntegerArithmetic", true))
        }
        MlInterface::Additive => Some(("Additive", "tir::Additive", true)),
        MlInterface::Subtractive => Some(("Subtractive", "tir::Subtractive", true)),
        MlInterface::OpCost(_) => Some(("OpCost", "tir::OpCost", true)),
        MlInterface::MemoryRead => Some(("MemoryRead", "tir::MemoryRead", false)),
        MlInterface::MemoryWrite => Some(("MemoryWrite", "tir::MemoryWrite", false)),
        MlInterface::ResourceEffects => Some(("ResourceEffects", "tir::ResourceEffects", false)),
        MlInterface::PromotableAllocation => {
            Some(("PromotableAllocation", "tir::PromotableAllocation", false))
        }
        MlInterface::Terminator => Some(("Terminator", "tir::Terminator", false)),
        MlInterface::ExitScope => Some(("ExitScope", "tir::ExitScope", false)),
        MlInterface::Congruent => Some(("Congruent", "tir::Congruent", true)),
        MlInterface::Symbol => Some(("Symbol", "tir::Symbol", false)),
        MlInterface::Visibility => None,
        MlInterface::Callable => Some(("Callable", "tir::Callable", false)),
        MlInterface::Apply => Some(("Apply", "tir::Apply", false)),
        MlInterface::BranchGuard => Some(("BranchGuard", "tir::BranchGuard", false)),
        MlInterface::BranchTerminator => Some(("BranchTerminator", "tir::BranchTerminator", false)),
        MlInterface::CaseGuard => Some(("CaseGuard", "tir::CaseGuard", false)),
        MlInterface::CountedLoop => Some(("CountedLoop", "tir::CountedLoop", false)),
        MlInterface::Gamma => Some(("Gamma", "tir::Gamma", false)),
        MlInterface::Global => Some(("Global", "tir::Global", false)),
        MlInterface::NonLocalExit => Some(("NonLocalExit", "tir::NonLocalExit", false)),
        MlInterface::RegionBinding => Some(("RegionBinding", "tir::RegionBinding", false)),
        MlInterface::Theta => Some(("Theta", "tir::Theta", false)),
        MlInterface::FloatingPoint => None,
        MlInterface::Custom(_) => None,
    }
}

#[allow(clippy::cognitive_complexity)]
fn emit_op_body(sig: &Signature, op: &MlOpDef, in_macro: bool, out: &mut String) {
    let rname = &op.op_rust_name;
    let bname = format!("{rname}Builder");
    let safe_op_name = op.op_name.replace(['.', '-'], "_");
    let fn_ident = escape_rust_ident(&safe_op_name);
    let schema_const = format!(
        "__TIR_OP_SCHEMA_{}_{}",
        op.op_dialect.replace(['.', '-'], "_").to_ascii_uppercase(),
        rname
    );
    let spelled = if op.op_dialect == "builtin" {
        op.op_name.clone()
    } else {
        format!("{}.{}", op.op_dialect, op.op_name)
    };
    let scheme = format_scheme_str(sig, op);

    let has_var = op
        .operands
        .iter()
        .chain(&op.results)
        .any(|p| p.is_state || !matches!(p.arity, PortArity::Fixed));
    let ports_expr = if has_var {
        "None".to_owned()
    } else {
        format!(
            "Some(({}usize, {}usize))",
            op.operands.len(),
            op.results.len()
        )
    };

    // Collect effective interfaces (declared + Theta/Gamma/CountedLoop from binds/counted)
    let mut iface_entries: Vec<(String, String, bool, Option<i64>)> = Vec::new();
    for iface in &op.interfaces {
        if let MlInterface::Custom(p) = iface {
            let short = p.rsplit("::").next().unwrap_or(p).to_owned();
            iface_entries.push((short, p.clone(), false, None));
        } else if let Some((sname, tpath, auto_impl)) = interface_trait_info(iface) {
            let cost = match iface {
                MlInterface::OpCost(c) => Some(*c),
                _ => None,
            };
            // Inside macro_def (native ops like ptr/scf), traits like Additive/Subtractive/
            // Speculatable/Congruent are sometimes implemented by hand in Rust; for non-macro
            // ops (builtin/fp/test), auto-implement marker traits!
            let do_impl = auto_impl && (!in_macro || matches!(iface, MlInterface::OpCost(_)));
            iface_entries.push((sname.to_owned(), tpath.to_owned(), do_impl, cost));
        }
    }
    if let Some(bind) = &op.binds {
        match bind {
            MlBind::Theta { .. } => {
                iface_entries.push(("Theta".to_owned(), "tir::Theta".to_owned(), false, None));
            }
            MlBind::Gamma { .. } => {
                iface_entries.push(("Gamma".to_owned(), "tir::Gamma".to_owned(), false, None));
            }
        }
    }
    if op.counted.is_some() {
        iface_entries.push((
            "CountedLoop".to_owned(),
            "tir::CountedLoop".to_owned(),
            false,
            None,
        ));
    }

    let binding_inputs = match &op.binds {
        Some(MlBind::Theta {
            inits_group: (f, l),
            ..
        })
        | Some(MlBind::Gamma {
            inputs_group: (f, l),
            ..
        }) => {
            format!("Some(({f}usize, {l}usize))")
        }
        None => "None".to_owned(),
    };
    let counted_groups = match &op.counted {
        Some(c) => format!(
            "Some(({}usize, {}usize, {}usize))",
            c.induction, c.upper_group, c.step_group
        ),
        None => "None".to_owned(),
    };

    // 1. Static OpSchema registered in tir::OP_SCHEMAS
    let _ = writeln!(out, "#[allow(non_upper_case_globals)]");
    let _ = writeln!(out, "#[tir::linkme::distributed_slice(tir::OP_SCHEMAS)]");
    let _ = writeln!(out, "#[linkme(crate = tir::linkme)]");
    let _ = writeln!(
        out,
        "static {schema_const}: tir::OpSchema = tir::OpSchema {{"
    );
    let _ = writeln!(out, "    dialect: {:?},", op.op_dialect);
    let _ = writeln!(out, "    name: {:?},", op.op_name);
    let _ = writeln!(out, "    declaration: tir::NativeIdentity::of::<{rname}>,");
    let _ = writeln!(out, "    bind: tir::bind_native_operation::<{rname}>,");
    let _ = writeln!(
        out,
        "    construct: <{rname} as tir::Operation>::from_op_instance_dyn,"
    );
    out.push_str("    operands: ");
    emit_field_schemas(sig, &op.operands, op.widened, out);
    out.push_str(",\n    results: ");
    emit_field_schemas(sig, &op.results, op.widened, out);
    out.push_str(",\n    attributes: ");
    emit_attr_schemas(sig, &op.attributes, out);
    out.push_str(",\n    interfaces: &[");
    for (sname, _, _, _) in &iface_entries {
        let _ = write!(out, "{sname:?}, ");
    }
    if in_macro {
        out.push_str("$(const { let name = stringify!($host); let mut start = name.len(); while start > 0 && name.as_bytes()[start - 1] != b':' { start -= 1; } name.split_at(start).1.trim_ascii() }),*");
    }
    let schema_cost = iface_entries
        .iter()
        .find_map(|(_, _, _, c)| *c)
        .unwrap_or(1);
    out.push_str("],\n    cost: ");
    let _ = write!(out, "{schema_cost}u32");
    out.push_str(",\n    regions: ");
    emit_region_schemas(&op.regions, out);
    let _ = writeln!(out, ",\n    binding_inputs: {binding_inputs},");
    let _ = writeln!(out, "    counted_groups: {counted_groups},");
    let _ = writeln!(out, "    scheme: {scheme:?},");
    out.push_str("    signature: ");
    emit_signature_const(sig, op, out);
    out.push_str(",\n    type_constraints: &[");
    for check in &op.type_constraints {
        let _ = write!(out, "tir::schema::TypeConstraint::{}(", check.constructor);
        for arg in &check.arguments {
            match arg {
                MlConstraintArg::Input(index) => {
                    let _ = write!(out, "tir::schema::ConstraintPort::Input({index}), ");
                }
                MlConstraintArg::Output(index) => {
                    let _ = write!(out, "tir::schema::ConstraintPort::Output({index}), ");
                }
                MlConstraintArg::Region(index) => {
                    let _ = write!(out, "{index}, ");
                }
                MlConstraintArg::Text(text) => {
                    let _ = write!(out, "{text:?}, ");
                }
                MlConstraintArg::TextList(entries) => {
                    let _ = write!(out, "&{entries:?}, ");
                }
            }
        }
        out.push_str("), ");
    }
    out.push_str("],\n    assembly: ");
    if op.machine {
        out.push_str("Some(tir::schema::OpLayout { operands: &[], results: &[], attributes: ");
        emit_attr_schemas(sig, &op.assembly_attrs, out);
        out.push_str(" })");
    } else {
        out.push_str("None");
    }
    out.push_str(",\n};\n\n");

    // 2. Op struct + NativeDeclaration + Operation
    let vis = if in_macro { "" } else { "pub(crate) " };
    let _ = writeln!(out, "pub struct {rname}({vis}tir::OpHandle);\n");

    let _ = writeln!(out, "impl tir::NativeDeclaration for {rname} {{");
    let _ = writeln!(
        out,
        "    const NAME: &'static str = \"{}.{}\";",
        op.op_dialect, op.op_name
    );
    let _ = writeln!(
        out,
        "    const SYMBOL: &'static str = \"@cdsl:decl:{}.{}\";",
        op.op_dialect, op.op_name
    );
    let _ = writeln!(
        out,
        "    const PORTS: Option<(usize, usize)> = {ports_expr};"
    );
    out.push_str("}\n\n");

    let _ = writeln!(out, "impl tir::Operation for {rname} {{");
    let _ = writeln!(
        out,
        "    fn native_identity() -> tir::NativeIdentity {{ tir::NativeIdentity::of::<Self>() }}"
    );
    let _ = writeln!(out, "    fn name() -> &'static str {{ {:?} }}", op.op_name);
    let _ = writeln!(
        out,
        "    fn dialect() -> &'static str {{ {:?} }}",
        op.op_dialect
    );
    let _ = writeln!(out, "    fn handle(&self) -> &tir::OpHandle {{ &self.0 }}");
    let _ = writeln!(
        out,
        "    fn from_op_instance(instance: tir::OpHandle) -> Self {{"
    );
    let _ = writeln!(
        out,
        "        assert_eq!(instance.name(), tir::OperationName::of::<Self>());"
    );
    let _ = writeln!(out, "        Self(instance)");
    let _ = writeln!(out, "    }}");
    let _ = writeln!(
        out,
        "    fn from_op_instance_dyn(instance: tir::OpHandle) -> Box<dyn tir::Operation> {{ Box::new(Self::from_op_instance(instance)) }}"
    );
    let _ = writeln!(
        out,
        "    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {{ self }}"
    );

    // print / parse
    let _ = writeln!(
        out,
        "    fn print<'a, 'b: 'a>(&'a self, formatter: &'a mut tir::IRFormatter<'b>) -> std::fmt::Result {{"
    );
    if op.custom_format || op.format_enabled {
        out.push_str("        self.custom_print(formatter)\n");
    } else if let Some(bind) = &op.binds {
        match bind {
            MlBind::Theta { body, .. } => {
                let bm = escape_rust_ident(body);
                let _ = writeln!(
                    out,
                    "        tir::binding::print_theta(formatter, &self.0, {spelled:?}, self.{bm}(), &tir::Theta::binding(self))"
                );
            }
            MlBind::Gamma {
                arms,
                predicate_slot,
                ..
            } => {
                let am = escape_rust_ident(arms);
                let _ = writeln!(
                    out,
                    "        tir::binding::print_gamma(formatter, &self.0, {spelled:?}, self.0.operands()[{predicate_slot}usize], &self.{am}(), &tir::Gamma::binding(self))"
                );
            }
        }
    } else {
        let _ = writeln!(
            out,
            "        tir::region_format::print_generic(formatter, &self.0, {spelled:?})"
        );
    }
    out.push_str("    }\n");

    let _ = writeln!(
        out,
        "    #[allow(unused_mut)]\n    fn parse(parser: &mut tir::parse::text::Parser<'_>, context: &tir::Context) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {{"
    );
    if op.custom_format || op.format_enabled {
        out.push_str("        Self::custom_parse(parser, context)\n");
    } else if let Some(bind) = &op.binds {
        emit_bind_parse(op, bind, &bname, out);
    } else {
        emit_generic_parse(sig, op, &bname, out);
    }
    out.push_str("    }\n");

    let _ = writeln!(
        out,
        "    fn register_interfaces(_context: &tir::Context) {{"
    );
    for (_, tpath, _, _) in &iface_entries {
        let _ = writeln!(
            out,
            "        _context.register_operation_interface::<Self, dyn {tpath}>();"
        );
    }
    if in_macro {
        out.push_str("        $(_context.register_operation_interface::<Self, dyn $host>();)*\n");
    }
    out.push_str("    }\n}\n\n");

    // 3. ImplementsOpInterface + auto marker trait impls
    for (_, tpath, do_impl, cost) in &iface_entries {
        let _ = writeln!(
            out,
            "impl tir::ImplementsOpInterface<dyn {tpath}> for {rname} {{ fn into_interface(self: Box<Self>) -> Box<dyn {tpath}> {{ self }} }}"
        );
        if *do_impl {
            if let Some(c) = cost {
                let _ = writeln!(
                    out,
                    "impl {tpath} for {rname} {{ fn cost(&self) -> u32 {{ {c}u32 }} }}"
                );
            } else {
                let _ = writeln!(out, "impl {tpath} for {rname} {{}}");
            }
        } else if !in_macro && *tpath == "tir::Terminator" {
            let _ = writeln!(
                out,
                "impl tir::Terminator for {rname} {{ fn successors(&self) -> Vec<tir::BlockId> {{ Vec::new() }} }}"
            );
        }
    }
    if in_macro {
        let _ = writeln!(
            out,
            "$(impl tir::ImplementsOpInterface<dyn $host> for {rname} {{ fn into_interface(self: Box<Self>) -> Box<dyn $host> {{ self }} }})*"
        );
    }
    let _ = writeln!(
        out,
        "impl From<&{rname}> for tir::OpId {{ fn from(operation: &{rname}) -> Self {{ tir::Operation::id(operation) }} }}"
    );
    let _ = writeln!(
        out,
        "impl From<{rname}> for tir::OpId {{ fn from(operation: {rname}) -> Self {{ tir::Operation::id(&operation) }} }}\n"
    );

    // 4. Op methods (result, attribute getters, region getters, assembly_layout, check_immediates)
    let data_results: Vec<&MlPort> = op.results.iter().filter(|p| !is_state_port(p)).collect();
    let _ = writeln!(out, "impl {rname} {{");
    if !data_results.is_empty() {
        let _ = writeln!(
            out,
            "    pub fn result(&self) -> tir::ValueId {{ self.0.results()[0] }}"
        );
    }
    for (idx, r) in op.regions.iter().enumerate() {
        let m = escape_rust_ident(&r.name);
        if r.variadic {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Vec<tir::RegionId> {{ self.0.regions()[{idx}usize..].to_vec() }}"
            );
        } else if r.kind == "Nodes" {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> tir::RegionId {{ self.0.regions()[{idx}usize] }}"
            );
        } else {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> tir::BlockHandle {{ let r = self.0.regions()[{idx}usize]; let b = self.0.context.get_region(r).entry_block(); self.0.context.get_block(b) }}"
            );
        }
    }
    for a in &op.attributes {
        emit_attr_getter(sig, a, out);
    }
    if op.machine {
        let _ = writeln!(
            out,
            "    pub fn assembly_layout() -> &'static tir::schema::OpLayout {{ {schema_const}.assembly.as_ref().expect(\"machine op assembly layout\") }}"
        );
        let _ = writeln!(
            out,
            "    pub fn check_immediates(attributes: &[tir::attributes::NamedAttribute], context: &tir::Context) -> Result<(), tir::Error> {{"
        );
        let _ = writeln!(out, "        let _ = (attributes, context);");
        for imm in &op.immediates {
            emit_imm_check(imm, &spelled, out);
        }
        out.push_str("        Ok(())\n    }\n");
    }
    out.push_str("}\n\n");

    // Immediate brand structs (for machine ops)
    if op.machine && !op.immediates.is_empty() {
        let mod_name = format!("__cdsl_immediates_{rname}");
        let _ = writeln!(out, "#[allow(non_snake_case)]\nmod {mod_name} {{");
        for imm in &op.immediates {
            let brand = &imm.brand;
            let _ = writeln!(out, "    #[derive(Clone, Copy, Debug, PartialEq, Eq)]");
            let _ = writeln!(out, "    pub struct {brand}(i64);");
            let _ = writeln!(out, "    impl {brand} {{");
            let _ = writeln!(
                out,
                "        pub(super) const CONSTRAINT: tir::backend::operand::ImmediateConstraint = tir::backend::operand::ImmediateConstraint {{ bits: {}u16, align: {}u32, nonzero: {} }};",
                imm.bits, imm.align, imm.nonzero
            );
            let _ = writeln!(out, "    }}");
            let _ = writeln!(
                out,
                "    impl TryFrom<i64> for {brand} {{\n\
                 \x20       type Error = tir::Error;\n\
                 \x20       fn try_from(value: i64) -> Result<Self, Self::Error> {{\n\
                 \x20           tir::backend::operand::check_immediate(Self::CONSTRAINT, &tir::attributes::AttributeValue::Int(value))?;\n\
                 \x20           Ok(Self(value))\n\
                 \x20       }}\n\
                 \x20   }}"
            );
            let _ = writeln!(
                out,
                "    impl From<{brand}> for tir::attributes::AttributeValue {{ fn from(v: {brand}) -> Self {{ tir::attributes::AttributeValue::Int(v.0) }} }}"
            );
        }
        out.push_str("}\npub use ");
        out.push_str(&mod_name);
        out.push_str("::{");
        for (i, imm) in op.immediates.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&imm.brand);
        }
        out.push_str("};\n\n");
    }

    // 5. Theta / Gamma / CountedLoop impls
    if let Some(bind) = &op.binds {
        emit_bind_impl(op, rname, bind, out);
    }
    if let Some(counted) = &op.counted {
        emit_counted_impl(op, rname, counted, out);
    }

    // 6. Builder + constructor function
    emit_builder_and_ctor(sig, op, &bname, &fn_ident, out);
}

fn emit_imm_check(imm: &MlImm, spelled: &str, out: &mut String) {
    let brand = &imm.brand;
    let invalid = format!("{spelled} immediate '{}'", imm.name);
    let _ = writeln!(
        out,
        "        if let Some(attribute) = tir::attributes::find_attribute(context, attributes, {:?}) {{",
        imm.name
    );
    out.push_str("            match &attribute.value {\n");
    if imm.allow_text {
        let _ = writeln!(
            out,
            "                tir::attributes::AttributeValue::Str(value) => tir::backend::operand::check_text_immediate({brand}::CONSTRAINT, value),"
        );
    } else if imm.allow_fixups {
        out.push_str("                tir::attributes::AttributeValue::Str(_) => Ok(()),\n");
    }
    if imm.allow_fixups {
        out.push_str("                tir::attributes::AttributeValue::Block(_) => Ok(()),\n");
    }
    let _ = writeln!(
        out,
        "                value => tir::backend::operand::check_immediate({brand}::CONSTRAINT, value),\n\
         \x20           }}.map_err(|error| tir::Error::VerificationError(format!(\"{{}}: {{}}\", {invalid:?}, error)))?;\n\
         \x20       }}"
    );
}

fn emit_attr_getter(sig: &Signature, attr: &MlAttr, out: &mut String) {
    let m = escape_rust_ident(&attr.name);
    let (ty_str, _) = format_attr_ty_str(sig, attr);
    match ty_str {
        "Int" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<i64> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Int(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        "?Int" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<i64> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Int(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        "UInt" | "?UInt" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<u64> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::UInt(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        "Bool" | "?Flag" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<bool> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Bool(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        "Str" | "Symbol" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> String {{ self.0.attr({:?}).and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default() }}",
                attr.name
            );
        }
        "?Str" | "?Symbol" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<String> {{ self.0.attr({:?}).and_then(|v| v.as_str().map(str::to_owned)) }}",
                attr.name
            );
        }
        "Predicate" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<tir::attributes::Predicate> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Predicate(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        "?Predicate" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<tir::attributes::Predicate> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Predicate(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        "Type" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> tir::TypeId {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Type(v)) => v, _ => panic!(\"missing required type attribute {}\") }} }}",
                attr.name, attr.name
            );
        }
        "?Type" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<tir::TypeId> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Type(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        "Block" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> tir::BlockId {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Block(v)) => v, _ => panic!(\"missing required block attribute {}\") }} }}",
                attr.name, attr.name
            );
        }
        "?Block" => {
            let _ = writeln!(
                out,
                "    pub fn {m}(&self) -> Option<tir::BlockId> {{ match self.0.attr({:?}) {{ Some(tir::attributes::AttributeValue::Block(v)) => Some(v), _ => None }} }}",
                attr.name
            );
        }
        _ => {}
    }
}

fn emit_bind_impl(op: &MlOpDef, rname: &str, bind: &MlBind, out: &mut String) {
    let ngroups = op.operands.len().max(1);
    match bind {
        MlBind::Theta {
            body,
            predicate_slot,
            inits_group,
            ..
        } => {
            let bm = escape_rust_ident(body);
            let (first, last) = inits_group;
            let _ = writeln!(
                out,
                "impl tir::Theta for {rname} {{\n\
                 \x20   fn predicate_slot(&self) -> usize {{ {predicate_slot}usize }}\n\
                 \x20   fn body(&self) -> tir::RegionId {{ self.{bm}() }}\n\
                 \x20   fn binding(&self) -> tir::Binding {{\n\
                 \x20       let segs = tir::binding::operand_segments(&self.0, {ngroups}usize);\n\
                 \x20       let operands = segs[{first}usize].start..segs[{last}usize].end;\n\
                 \x20       let n = operands.len();\n\
                 \x20       let p = {predicate_slot}usize;\n\
                 \x20       tir::Binding {{ operands, ports: 0..n, continue_: (p + 1)..(p + 1 + n), exit: (p + 1 + n)..(p + 1 + 2 * n), results: 0..self.0.results().len() }}\n\
                 \x20   }}\n\
                 \x20   fn predicate(&self) -> tir::ValueId {{ self.0.context.get_region(self.{bm}()).results()[{predicate_slot}usize] }}\n\
                 }}\n"
            );
        }
        MlBind::Gamma {
            arms,
            predicate_slot,
            inputs_group,
            ..
        } => {
            let am = escape_rust_ident(arms);
            let (first, last) = inputs_group;
            let _ = writeln!(
                out,
                "impl tir::Gamma for {rname} {{\n\
                 \x20   fn predicate_slot(&self) -> usize {{ {predicate_slot}usize }}\n\
                 \x20   fn predicate(&self) -> tir::ValueId {{ self.0.operands()[{predicate_slot}usize] }}\n\
                 \x20   fn arms(&self) -> Vec<tir::RegionId> {{ self.{am}() }}\n\
                 \x20   fn binding(&self) -> tir::Binding {{\n\
                 \x20       let segs = tir::binding::operand_segments(&self.0, {ngroups}usize);\n\
                 \x20       let operands = segs[{first}usize].start..segs[{last}usize].end;\n\
                 \x20       let n = operands.len();\n\
                 \x20       let r = self.0.results().len();\n\
                 \x20       tir::Binding {{ operands, ports: 0..n, continue_: 0..0, exit: 0..r, results: 0..r }}\n\
                 \x20   }}\n\
                 \x20   fn cases(&self) -> Vec<u64> {{ tir::binding::gamma_cases(&self.0, self.{am}().len()) }}\n\
                 }}\n"
            );
        }
    }
}

fn emit_counted_impl(op: &MlOpDef, rname: &str, c: &MlCounted, out: &mut String) {
    let ngroups = op.operands.len().max(1);
    let _ = writeln!(
        out,
        "impl tir::CountedLoop for {rname} {{\n\
         \x20   fn lower_bound(&self) -> tir::ValueId {{ self.0.operands()[0] }}\n\
         \x20   fn upper_bound(&self) -> tir::ValueId {{ let s = &tir::binding::operand_segments(&self.0, {ngroups}usize)[{}usize]; self.0.operands()[s.start] }}\n\
         \x20   fn step(&self) -> tir::ValueId {{ let s = &tir::binding::operand_segments(&self.0, {ngroups}usize)[{}usize]; self.0.operands()[s.start] }}\n\
         \x20   fn induction(&self) -> Option<usize> {{ Some({}usize) }}\n\
         }}\n",
        c.upper_group, c.step_group, c.induction
    );
}

fn emit_bind_parse(op: &MlOpDef, bind: &MlBind, bname: &str, out: &mut String) {
    match bind {
        MlBind::Theta { inits, body, .. } => {
            let im = escape_rust_ident(inits);
            let bm = escape_rust_ident(body);
            let _ = writeln!(
                out,
                "        let parsed = tir::binding::parse_theta(parser, context)?;\n\
                 \x20       let builder = {bname}::new(context).{im}(parsed.inits).result_types(parsed.result_types).{bm}(parsed.body);\n\
                 \x20       Ok(Box::new(builder.build()))"
            );
        }
        MlBind::Gamma {
            predicate,
            inputs,
            arms,
            ..
        } => {
            let pm = escape_rust_ident(predicate);
            let im = escape_rust_ident(inputs);
            let am = escape_rust_ident(arms);
            let _ = writeln!(
                out,
                "        let parsed = tir::binding::parse_gamma(parser, context)?;\n\
                 \x20       let mut builder = {bname}::new(context).{pm}(parsed.predicate).{im}(parsed.inputs).result_types(parsed.result_types).{am}(parsed.arms);\n\
                 \x20       if let Some(cases) = parsed.cases {{ builder = builder.attr(tir::binding::GAMMA_CASES, cases); }}\n\
                 \x20       Ok(Box::new(builder.build()))"
            );
        }
    }
    let _ = op;
}

fn emit_generic_parse(sig: &Signature, op: &MlOpDef, bname: &str, out: &mut String) {
    let _ = writeln!(out, "        let mut builder = {bname}::new(context);");
    let mut pos = 0usize;
    let mut has_state_in = false;
    let mut state_in_method = String::from("state_operands");
    for p in &op.operands {
        if is_state_port(p) {
            has_state_in = true;
            if p.name != "state" {
                state_in_method = escape_rust_ident(&p.name);
            }
            continue;
        }
        if pos > 0 {
            out.push_str("        parser.parse_token(\",\");\n");
        }
        pos += 1;
        let m = escape_rust_ident(&p.name);
        if matches!(p.arity, PortArity::Variadic) {
            let _ = writeln!(
                out,
                "        let mut values = Vec::new();\n\
                 \x20       while let Some(reference) = parser.parse_value_ref() {{\n\
                 \x20           values.push(parser.resolve_value(context, reference));\n\
                 \x20           if !parser.parse_token(\",\") {{ break; }}\n\
                 \x20       }}\n\
                 \x20       builder = builder.{m}(values);"
            );
        } else {
            let _ = writeln!(
                out,
                "        if let Some(reference) = parser.parse_value_ref() {{ builder = builder.{m}(parser.resolve_value(context, reference)); }}"
            );
        }
    }
    if has_state_in {
        let _ = writeln!(
            out,
            "        builder = builder.{state_in_method}(parser.parse_state_operands(context)?);"
        );
    }
    // Optional attributes `{ k = v, ... }`
    out.push_str(
        "        let mark = parser.pos();\n\
         \x20       let mut parsed_attributes = Vec::new();\n\
         \x20       if parser.parse_token(\"{\") && !parser.parse_token(\"}\") {\n\
         \x20           let complete = loop {\n\
         \x20               let Some(name) = parser.parse_attribute_name() else { break false; };\n\
         \x20               if !parser.parse_token(\"=\") { break false; }\n\
         \x20               #[allow(unused_mut)]\n\
         \x20               let Some(mut value) = parser.parse_attribute_value(context)? else { break false; };\n",
    );
    for a in &op.attributes {
        let (ty_s, _) = format_attr_ty_str(sig, a);
        if ty_s == "UInt" || ty_s == "?UInt" {
            let _ = writeln!(
                out,
                "                if name == {:?} && let tir::attributes::AttributeValue::Int(integer) = value {{ let Ok(integer) = u64::try_from(integer) else {{ break false; }}; value = tir::attributes::AttributeValue::UInt(integer); }}",
                a.name
            );
        }
    }
    out.push_str(
        "                parsed_attributes.push(context.named_attribute(&name, value));\n\
         \x20               if parser.parse_token(\"}\") { break true; }\n\
         \x20               if !parser.parse_token(\",\") { break false; }\n\
         \x20           };\n\
         \x20           if !complete { parser.set_pos(mark); parsed_attributes.clear(); }\n\
         \x20       }\n",
    );

    // Result type `: !ty`
    if let Some(res) = op.results.iter().find(|p| !is_state_port(p)) {
        let set = if matches!(res.arity, PortArity::Variadic) {
            "builder = builder.result_types(vec![ty]);"
        } else {
            "builder = builder.result_type(ty);"
        };
        let _ = writeln!(
            out,
            "        if !parser.parse_token(\":\") {{ return Err((tir::parse::common::Cursor::span(parser), tir::Error::ExpectedToken(\":\"))); }}\n\
             \x20       let ty = parser.parse_type(context)?.ok_or_else(|| (tir::parse::common::Cursor::span(parser), tir::Error::ExpectedType))?;\n\
             \x20       {set}"
        );
    }

    // Single non-variadic region
    if let [r] = op.regions.as_slice()
        && !r.variadic
    {
        let rm = escape_rust_ident(&r.name);
        let _ = writeln!(
            out,
            "        builder = builder.{rm}(parser.parse_region(context)?.id());"
        );
    }

    out.push_str(
        "        for attribute in parsed_attributes { builder = builder.attr_sym(attribute.name, attribute.value); }\n",
    );
    if op.machine {
        out.push_str(
            "        Ok(Box::new(builder.try_build().map_err(|e| (tir::parse::common::Cursor::span(parser), e))?))\n",
        );
    } else {
        out.push_str("        Ok(Box::new(builder.build()))\n");
    }
}

#[allow(clippy::cognitive_complexity)]
fn emit_builder_and_ctor(
    sig: &Signature,
    op: &MlOpDef,
    bname: &str,
    fn_ident: &str,
    out: &mut String,
) {
    let rname = &op.op_rust_name;
    let has_segments = op
        .operands
        .iter()
        .any(|p| p.is_state || matches!(p.arity, PortArity::Variadic));
    let data_results: Vec<&MlPort> = op.results.iter().filter(|p| !is_state_port(p)).collect();
    let multiple_results = data_results.len() > 1;
    let has_state_out = op.results.iter().any(is_state_port);

    // Builder struct
    let _ = writeln!(out, "pub struct {bname} {{");
    let _ = writeln!(out, "    context: tir::Context,");
    let _ = writeln!(out, "    parts: tir::NewOpParts,");
    let _ = writeln!(out, "    insertion: tir::InsertionPoint,");
    for p in &op.operands {
        let m = escape_rust_ident(&p.name);
        if p.is_state || matches!(p.arity, PortArity::Variadic) {
            let _ = writeln!(out, "    {m}: Vec<tir::ValueId>,");
        } else {
            let _ = writeln!(out, "    {m}: Option<tir::ValueId>,");
        }
    }
    if multiple_results {
        for r in &data_results {
            let m = escape_rust_ident(&r.name);
            let _ = writeln!(out, "    res_{m}: Option<tir::TypeId>,");
        }
    } else if let Some(r) = data_results.first() {
        if matches!(r.arity, PortArity::Variadic) {
            let _ = writeln!(out, "    result_types: Vec<tir::TypeId>,");
            let _ = writeln!(out, "    result_values: Vec<tir::ValueId>,");
        } else {
            let _ = writeln!(out, "    result_type: Option<tir::TypeId>,");
        }
    }
    if has_state_out {
        let _ = writeln!(out, "    state_result_types: Vec<tir::TypeId>,");
    }
    for r in &op.regions {
        let m = escape_rust_ident(&r.name);
        if r.variadic {
            let _ = writeln!(out, "    reg_{m}: Vec<tir::RegionId>,");
        } else {
            let _ = writeln!(out, "    reg_{m}: Option<tir::RegionId>,");
        }
    }
    out.push_str("}\n\n");

    // Builder impl
    let _ = writeln!(out, "impl {bname} {{");
    let _ = writeln!(out, "    pub fn new(context: &tir::Context) -> Self {{");
    let _ = writeln!(out, "        Self {{");
    let _ = writeln!(out, "            context: context.clone(),");
    let _ = writeln!(out, "            parts: tir::NewOpParts::default(),");
    let _ = writeln!(out, "            insertion: tir::InsertionPoint::Root,");
    for p in &op.operands {
        let m = escape_rust_ident(&p.name);
        if p.is_state || matches!(p.arity, PortArity::Variadic) {
            let _ = writeln!(out, "            {m}: Vec::new(),");
        } else {
            let _ = writeln!(out, "            {m}: None,");
        }
    }
    if multiple_results {
        for r in &data_results {
            let m = escape_rust_ident(&r.name);
            let _ = writeln!(out, "            res_{m}: None,");
        }
    } else if let Some(r) = data_results.first() {
        if matches!(r.arity, PortArity::Variadic) {
            let _ = writeln!(out, "            result_types: Vec::new(),");
            let _ = writeln!(out, "            result_values: Vec::new(),");
        } else {
            let _ = writeln!(out, "            result_type: None,");
        }
    }
    if has_state_out {
        let _ = writeln!(out, "            state_result_types: Vec::new(),");
    }
    for r in &op.regions {
        let m = escape_rust_ident(&r.name);
        if r.variadic {
            let _ = writeln!(out, "            reg_{m}: Vec::new(),");
        } else {
            let _ = writeln!(out, "            reg_{m}: None,");
        }
    }
    out.push_str("        }\n    }\n");

    // Operand setters
    for p in &op.operands {
        let m = escape_rust_ident(&p.name);
        if p.is_state && p.name == "state" {
            let _ = writeln!(
                out,
                "    pub fn state(mut self, value: tir::ValueId) -> Self {{ self.{m}.push(value); self }}"
            );
            let _ = writeln!(
                out,
                "    pub fn state_operand(mut self, value: tir::ValueId) -> Self {{ self.{m}.push(value); self }}"
            );
            let _ = writeln!(
                out,
                "    pub fn state_operands(mut self, values: Vec<tir::ValueId>) -> Self {{ self.{m} = values; self }}"
            );
        } else if p.is_state || matches!(p.arity, PortArity::Variadic) {
            let _ = writeln!(
                out,
                "    pub fn {m}(mut self, values: Vec<tir::ValueId>) -> Self {{ self.{m} = values; self }}"
            );
        } else {
            let _ = writeln!(
                out,
                "    pub fn {m}(mut self, value: tir::ValueId) -> Self {{ self.{m} = Some(value); self }}"
            );
        }
    }

    // Result setters
    if multiple_results {
        for r in &data_results {
            let m = escape_rust_ident(&r.name);
            let _ = writeln!(
                out,
                "    pub fn {m}(mut self, ty: tir::TypeId) -> Self {{ self.res_{m} = Some(ty); self }}"
            );
        }
    } else if let Some(r) = data_results.first() {
        if matches!(r.arity, PortArity::Variadic) {
            let _ = writeln!(
                out,
                "    pub fn result_types(mut self, types: Vec<tir::TypeId>) -> Self {{ self.result_types = types; self }}"
            );
            let _ = writeln!(
                out,
                "    pub fn result_values(mut self, values: Vec<tir::ValueId>) -> Self {{ self.result_values = values; self }}"
            );
        } else {
            let _ = writeln!(
                out,
                "    pub fn result_type(mut self, ty: tir::TypeId) -> Self {{ self.result_type = Some(ty); self }}"
            );
        }
    }
    if has_state_out {
        let _ = writeln!(
            out,
            "    pub fn state_result(mut self, ty: tir::TypeId) -> Self {{ self.state_result_types.push(ty); self }}"
        );
        let _ = writeln!(
            out,
            "    pub fn state_results(mut self, types: Vec<tir::TypeId>) -> Self {{ self.state_result_types = types; self }}"
        );
        if let Some(st_r) = op.results.iter().find(|p| is_state_port(p))
            && st_r.name != "state"
            && st_r.name != "out_state"
            && st_r.name != "state_result"
            && st_r.name != "state_results"
        {
            let m = escape_rust_ident(&st_r.name);
            let _ = writeln!(
                out,
                "    pub fn {m}(mut self, types: Vec<tir::TypeId>) -> Self {{ self.state_result_types = types; self }}"
            );
        }
    }

    // Region setters
    for r in &op.regions {
        let m = escape_rust_ident(&r.name);
        if r.variadic {
            let _ = writeln!(
                out,
                "    pub fn {m}(mut self, regions: Vec<tir::RegionId>) -> Self {{ self.reg_{m} = regions; self }}"
            );
        } else {
            let _ = writeln!(
                out,
                "    pub fn {m}(mut self, region: tir::RegionId) -> Self {{ self.reg_{m} = Some(region); self }}"
            );
        }
    }

    // Attribute setters
    for a in &op.attributes {
        let m = escape_rust_ident(&a.name);
        if let Some(imm) = op.immediates.iter().find(|im| im.name == a.name) {
            let _ = writeln!(
                out,
                "    pub fn {m}(self, value: {}) -> Self {{ self.attr({:?}, value.into()) }}",
                imm.brand, a.name
            );
        } else {
            let (ty_s, _) = format_attr_ty_str(sig, a);
            let (arg_ty, conv) = match ty_s {
                "Int" | "?Int" => ("impl Into<tir::attributes::AttributeValue>", "value.into()"),
                "UInt" | "?UInt" => ("u64", "tir::attributes::AttributeValue::UInt(value)"),
                "Bool" | "?Flag" => ("bool", "tir::attributes::AttributeValue::Bool(value)"),
                "Str" | "?Str" | "Symbol" | "?Symbol" => (
                    "impl Into<Box<str>>",
                    "tir::attributes::AttributeValue::Str(value.into())",
                ),
                "Predicate" | "?Predicate" => (
                    "tir::attributes::Predicate",
                    "tir::attributes::AttributeValue::Predicate(value)",
                ),
                "Type" | "?Type" => (
                    "tir::TypeId",
                    "tir::attributes::AttributeValue::Type(value)",
                ),
                "Block" | "?Block" => (
                    "tir::BlockId",
                    "tir::attributes::AttributeValue::Block(value)",
                ),
                _ => ("impl Into<tir::attributes::AttributeValue>", "value.into()"),
            };
            let _ = writeln!(
                out,
                "    pub fn {m}(self, value: {arg_ty}) -> Self {{ self.attr({:?}, {conv}) }}",
                a.name
            );
        }
    }

    let _ = writeln!(
        out,
        "    pub fn at(mut self, insertion: tir::InsertionPoint) -> Self {{ self.insertion = insertion; self }}"
    );
    let _ = writeln!(
        out,
        "    pub fn attr(self, name: &str, value: tir::attributes::AttributeValue) -> Self {{ let name = self.context.intern(name); self.attr_sym(name, value) }}"
    );
    let _ = writeln!(
        out,
        "    pub fn attr_sym(mut self, name: tir::Sym, value: tir::attributes::AttributeValue) -> Self {{ self.parts.attributes.push(tir::attributes::NamedAttribute::new(name, value)); self }}"
    );

    // build / try_build
    let build_sig = if op.machine {
        format!(
            "    #[allow(unused_mut)]\n    pub fn try_build(mut self) -> Result<{rname}, tir::Error> {{"
        )
    } else {
        format!("    #[allow(unused_mut)]\n    pub fn build(mut self) -> {rname} {{")
    };
    let _ = writeln!(out, "{build_sig}");
    if op.machine {
        let _ = writeln!(
            out,
            "        {rname}::check_immediates(&self.parts.attributes, &self.context)?;"
        );
    } else {
        for a in &op.attributes {
            if !a.optional {
                let _ = writeln!(
                    out,
                    "        assert!(self.parts.attributes.iter().any(|attr| Some(attr.name) == self.context.sym({:?})), \"Missing required attribute: {}\");",
                    a.name, a.name
                );
            }
        }
    }
    let _ = writeln!(out, "        let mut operands = Vec::new();");
    if has_segments {
        let _ = writeln!(out, "        let mut segment_sizes = Vec::new();");
    }
    for p in &op.operands {
        let m = escape_rust_ident(&p.name);
        if p.is_state || matches!(p.arity, PortArity::Variadic) {
            if has_segments {
                let _ = writeln!(
                    out,
                    "        segment_sizes.push(tir::attributes::AttributeValue::UInt(self.{m}.len() as u64));"
                );
            }
            let _ = writeln!(out, "        operands.extend(self.{m});");
        } else {
            if has_segments {
                let _ = writeln!(
                    out,
                    "        segment_sizes.push(tir::attributes::AttributeValue::UInt(u64::from(self.{m}.is_some())));"
                );
            }
            let _ = writeln!(out, "        operands.extend(self.{m});");
        }
    }
    if has_segments {
        let _ = writeln!(
            out,
            "        self.parts.attributes.push(self.context.named_attribute(\"operand_segment_sizes\", tir::attributes::AttributeValue::Array(segment_sizes.into())));"
        );
    }
    let _ = writeln!(out, "        let mut results = Vec::new();");
    if multiple_results {
        for r in &data_results {
            let m = escape_rust_ident(&r.name);
            let _ = writeln!(
                out,
                "        let ty = self.res_{m}.expect(\"result type for '{}' must be set\");\n        results.push(self.context.create_value(ty, None).id());",
                r.name
            );
        }
    } else if let Some(r) = data_results.first() {
        if matches!(r.arity, PortArity::Variadic) {
            let _ = writeln!(
                out,
                "        if self.result_values.is_empty() {{ results.extend(self.result_types.into_iter().map(|ty| self.context.create_value(ty, None).id())); }} else {{ results.extend(self.result_values); }}"
            );
        } else {
            let _ = writeln!(
                out,
                "        let ty = self.result_type.expect(\"result_type must be set for ops with results\");\n        results.push(self.context.create_value(ty, None).id());"
            );
        }
    }
    if has_state_out {
        let _ = writeln!(
            out,
            "        results.extend(self.state_result_types.into_iter().map(|ty| self.context.create_value(ty, None).id()));"
        );
    }
    let _ = writeln!(out, "        let mut regions = Vec::new();");
    for r in &op.regions {
        let m = escape_rust_ident(&r.name);
        if r.variadic || r.kind == "Nodes" {
            let _ = writeln!(out, "        regions.extend(self.reg_{m});");
        } else {
            let _ = writeln!(
                out,
                "        if let Some(region) = self.reg_{m} {{ regions.push(region); }} else {{ let region = self.context.create_region(); let block = self.context.create_block(Vec::new()); region.add_block(block.id()); regions.push(region.id()); }}"
            );
        }
    }
    let _ = writeln!(
        out,
        "        let op = tir::NewOp::new::<{rname}>(self.context.clone(), operands, results, regions, self.parts);"
    );
    if op.machine {
        let _ = writeln!(
            out,
            "        Ok({rname}(self.context.add_operation(self.insertion, op)))"
        );
        out.push_str("    }\n");
        let _ = writeln!(
            out,
            "    pub fn build(self) -> {rname} {{ self.try_build().expect(\"valid machine operation\") }}"
        );
    } else {
        let _ = writeln!(
            out,
            "        {rname}(self.context.add_operation(self.insertion, op))"
        );
        out.push_str("    }\n");
    }
    out.push_str("}\n\n");

    emit_ctor_fn(
        sig,
        op,
        bname,
        fn_ident,
        &data_results,
        multiple_results,
        out,
    );
}

#[allow(clippy::cognitive_complexity)]
fn emit_ctor_fn(
    sig: &Signature,
    op: &MlOpDef,
    bname: &str,
    fn_ident: &str,
    data_results: &[&MlPort],
    multiple_results: bool,
    out: &mut String,
) {
    if op.machine {
        return;
    }
    let _ = write!(
        out,
        "#[allow(clippy::too_many_arguments)]\npub fn {fn_ident}(context: &tir::Context"
    );
    let mut call_steps = String::new();
    for p in &op.operands {
        if is_state_port(p) {
            continue;
        }
        let m = escape_rust_ident(&p.name);
        if matches!(p.arity, PortArity::Variadic) {
            let _ = write!(out, ", {m}: Vec<tir::ValueId>");
            let _ = writeln!(call_steps, "    builder = builder.{m}({m});");
        } else if op.macro_def {
            let _ = write!(out, ", {m}: impl Into<tir::Operand>");
            let _ = writeln!(
                call_steps,
                "    if let Some(v) = {m}.into().into_option() {{ builder = builder.{m}(v); }}"
            );
        } else {
            let _ = write!(out, ", {m}: tir::ValueId");
            let _ = writeln!(call_steps, "    builder = builder.{m}({m});");
        }
    }
    for a in &op.attributes {
        if a.optional {
            continue;
        }
        let m = escape_rust_ident(&a.name);
        if let Some(imm) = op.immediates.iter().find(|im| im.name == a.name) {
            let _ = write!(out, ", {m}: {}", imm.brand);
            let _ = writeln!(call_steps, "    builder = builder.{m}({m});");
        } else if op.macro_def {
            let _ = write!(out, ", {m}: impl Into<tir::attributes::AttributeValue>");
            let _ = writeln!(
                call_steps,
                "    builder = builder.attr({:?}, {m}.into());",
                a.name
            );
        } else {
            let (ty_s, _) = format_attr_ty_str(sig, a);
            let arg_ty = match ty_s {
                "Int" => "impl Into<tir::attributes::AttributeValue>",
                "UInt" => "u64",
                "Bool" => "bool",
                "Str" | "Symbol" => "impl Into<Box<str>>",
                "Predicate" => "tir::attributes::Predicate",
                "Type" => "tir::TypeId",
                "Block" => "tir::BlockId",
                _ => "impl Into<tir::attributes::AttributeValue>",
            };
            let _ = write!(out, ", {m}: {arg_ty}");
            let _ = writeln!(call_steps, "    builder = builder.{m}({m});");
        }
    }
    if multiple_results {
        for r in data_results {
            let m = escape_rust_ident(&r.name);
            let _ = write!(out, ", {m}: tir::TypeId");
            let _ = writeln!(call_steps, "    builder = builder.{m}({m});");
        }
    } else if let Some(r) = data_results.first() {
        if matches!(r.arity, PortArity::Variadic) {
            out.push_str(", result_types: Vec<tir::TypeId>");
            call_steps.push_str("    builder = builder.result_types(result_types);\n");
        } else {
            out.push_str(", result_type: tir::TypeId");
            call_steps.push_str("    builder = builder.result_type(result_type);\n");
        }
    }
    for r in &op.regions {
        let m = escape_rust_ident(&r.name);
        if r.variadic {
            let _ = write!(out, ", {m}: Vec<tir::RegionId>");
            let _ = writeln!(call_steps, "    builder = builder.{m}({m});");
        } else if r.kind == "Nodes" {
            let _ = write!(out, ", {m}: tir::RegionId");
            let _ = writeln!(call_steps, "    builder = builder.{m}({m});");
        } else {
            let _ = write!(out, ", {m}: Option<tir::RegionId>");
            let _ = writeln!(
                call_steps,
                "    if let Some(region) = {m} {{ builder = builder.{m}(region); }}"
            );
        }
    }
    let _ = writeln!(out, ") -> {bname} {{");
    let _ = writeln!(
        out,
        "    #[allow(unused_mut)]\n    let mut builder = {bname}::new(context);"
    );
    out.push_str(&call_steps);
    out.push_str("    builder\n}\n");
}

fn emit_tir_interfaces(out: &mut String) {
    out.push_str(
        "pub trait Additive {}\n\
pub trait Apply {\n    fn callee(&self) -> ::tir::ValueId;\n    fn args(&self) -> ::std::ops::Range<usize>;\n}\n\
pub trait BranchGuard {\n    fn guarded_successors(&self) -> Vec<(::tir::BlockId, ::tir::ValueId, bool)>;\n}\n\
pub trait BranchTerminator {\n    fn successor_operands(&self) -> Vec<(::tir::BlockId, Vec<::tir::ValueId>)>;\n    fn successor_operand_slots(&self) -> Vec<(::tir::BlockId, Vec<usize>)>;\n}\n\
pub trait Callable {\n    fn body(&self) -> Option<::tir::RegionId>;\n    fn value(&self) -> ::tir::ValueId;\n    fn params(&self) -> Vec<::tir::TypeId>;\n    fn result(&self) -> ::tir::TypeId;\n}\n\
pub trait CaseGuard {\n    fn selector(&self) -> ::tir::ValueId;\n    fn cases(&self) -> Vec<u64>;\n}\n\
pub trait Commutative {\n    fn is_commutative(&self) -> bool { true }\n}\n\
pub trait Congruent {}\n\
pub trait ConstantLike {\n    fn constant_value(&self) -> ::tir::utils::APInt;\n}\n\
pub trait CountedLoop {\n    fn lower_bound(&self) -> ::tir::ValueId;\n    fn upper_bound(&self) -> ::tir::ValueId;\n    fn step(&self) -> ::tir::ValueId;\n    fn induction(&self) -> Option<usize> { None }\n}\n\
pub trait ExitScope {\n    fn exit_scope(&self) -> ::tir::ExitScopeKind;\n    fn label(&self) -> Option<String> { None }\n}\n\
pub trait Gamma {\n    fn predicate_slot(&self) -> usize;\n    fn predicate(&self) -> ::tir::ValueId;\n    fn arms(&self) -> Vec<::tir::RegionId>;\n    fn binding(&self) -> ::tir::Binding;\n    fn cases(&self) -> Vec<u64>;\n    fn indexes_arms(&self) -> bool { let values = self.cases(); values.into_iter().enumerate().all(|(index, value)| index as u64 == value) }\n    fn arm_for(&self, bits: u64) -> usize { let values = self.cases(); let len = values.len(); values.into_iter().position(|case| case == bits).unwrap_or(len) }\n}\n\
pub trait Global {\n    fn address(&self) -> ::tir::ValueId;\n    fn initializer(&self) -> Option<Vec<u8>>;\n}\n\
pub trait IntegerArithmetic {}\n\
pub trait MemoryRead {\n    fn read_location(&self) -> ::tir::ValueId;\n    fn read_value(&self) -> ::tir::ValueId;\n}\n\
pub trait MemoryWrite {\n    fn write_location(&self) -> ::tir::ValueId;\n    fn written_value(&self) -> ::tir::ValueId;\n}\n\
pub trait NonLocalExit {\n    fn target(&self) -> ::tir::ExitTarget;\n    fn values(&self) -> Vec<::tir::ValueId>;\n}\n\
pub trait OpCost {\n    fn cost(&self) -> u32 { 1 }\n}\n\
pub trait PromotableAllocation {\n    fn promoted_location(&self) -> ::tir::ValueId;\n}\n\
pub trait PureInterface {}\n\
pub trait RegionBinding {\n    fn region(&self) -> ::tir::RegionId;\n    fn binding(&self) -> ::tir::Binding;\n}\n\
pub trait ResourceEffects {\n    fn resource_effects(&self) -> Vec<::tir::ResourceEffect>;\n}\n\
pub trait SameOperandAndResultType {}\n\
pub trait Speculatable {\n    fn is_speculatable(&self) -> bool { true }\n}\n\
pub trait Subtractive {}\n\
pub trait Symbol {\n    fn symbol_name(&self) -> String;\n    fn symbol_signature(&self) -> Option<Vec<::tir::TypeId>>;\n    fn symbol_result_type(&self) -> Option<::tir::TypeId>;\n    fn symbol_visibility(&self) -> Visibility;\n    fn is_definition(&self) -> bool;\n}\n\
pub trait Terminator {\n    fn successors(&self) -> Vec<::tir::BlockId> { Vec::new() }\n}\n\
pub trait Theta {\n    fn predicate_slot(&self) -> usize;\n    fn body(&self) -> ::tir::RegionId;\n    fn binding(&self) -> ::tir::Binding;\n    fn predicate(&self) -> ::tir::ValueId;\n}\n\
#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub enum Visibility {\n    Public,\n    Private,\n}\n",
    );
}

pub fn emit_opdef_module(sig: &Signature, ml: &MlProgram, strip_prefix: Option<&str>) -> String {
    let mut out = String::with_capacity(ml.decls.len() * 512 + 256);
    for decl in &ml.decls {
        match decl {
            MlDecl::TirInterfaces => {
                emit_tir_interfaces(&mut out);
            }
            MlDecl::ReexportInterfaces => {
                out.push_str(
                    "pub use crate::interfaces::{\n\
                     \x20   Additive, Commutative, Congruent, ConstantLike, IntegerArithmetic, OpCost,\n\
                     \x20   PureInterface, SameOperandAndResultType, Speculatable, Subtractive, Symbol,\n\
                     \x20   Terminator, Visibility,\n\
                     };\n\n",
                );
            }
            MlDecl::Ind(ind) => {
                let raw = sig.interner.resolve(ind.iname);
                let short = strip_prefix
                    .and_then(|p| raw.strip_prefix(p))
                    .and_then(|r| r.strip_prefix("::"))
                    .unwrap_or(raw);
                let _ = writeln!(
                    out,
                    "#[derive(Clone, Copy, Debug, PartialEq, Eq)]\npub enum {short} {{"
                );
                for c in &ind.ictors {
                    let cn = sig.interner.resolve(c.cname);
                    let _ = writeln!(out, "    {cn},");
                }
                out.push_str("}\n\n");
            }
            MlDecl::OpDef(op) => {
                if op.macro_def {
                    let _ = writeln!(
                        out,
                        "macro_rules! __cdsl_{}_{} {{\n    ($($host:path),* $(,)?) => {{",
                        op.op_dialect, op.op_rust_name
                    );
                    emit_op_body(sig, op, true, &mut out);
                    out.push_str("    };\n}\n\n");
                } else {
                    let mod_name = format!("__cdsl_{}", op.op_name);
                    let fn_ident = escape_rust_ident(&op.op_name);
                    let _ = writeln!(
                        out,
                        "mod {mod_name} {{\n#[allow(unused_imports)]\nuse super::*;\n#[allow(unused_imports)]\nuse ::tir;"
                    );
                    emit_op_body(sig, op, false, &mut out);
                    out.push_str("}\n");
                    let _ = writeln!(
                        out,
                        "pub use {mod_name}::{{{fn_ident}, {}, {}Builder}};\n",
                        op.op_rust_name, op.op_rust_name
                    );
                }
            }
            _ => {}
        }
    }
    out
}
