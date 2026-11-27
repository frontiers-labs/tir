//! Rocq-Style Erasure (`Core -> MiniML`) + Structured OpDef, TEGG, and Program Extraction.
//!
//! Directly mirrors Rocq's `plugins/extraction/miniml.mli` and `extraction.ml`:
//!   1. Drops `Prop` terms and `Type`-level arguments (`MlExpr::Erased`).
//!   2. Extracts typed `op { ... }` declarations into structured `MlOpDef` with
//!      dependent type parameters, port arities, attributes, regions, interfaces,
//!      bindings, and immediates (ZERO string templates!).
//!   3. Extracts relational programs (`Relation`, `Table`, `enum`, externs,
//!      `Rewrite`, `Rule`, `Schedule`, `memo`) into structured `MlProgramBundle`
//!      with compiled TEGG v15 bytecode and auto-linked `@cdsl:decl:*` ops.

use std::{
    collections::{BTreeMap, BTreeSet},
    rc::Rc,
};

use crate::{
    check::{self, Ctx, Signature},
    core::{
        Decl, ExtractSpec, ExtractTarget, IndDecl, Lit, Neutral, Pattern, PrimOp, Sort, Sym, Term,
        Value, env_extend, pp_term,
    },
    error::Error,
    nbe,
    tegg::{
        self, IrAction, IrAtom, IrExtern, IrFact, IrProgram, IrRule, IrRuleset, IrSchedule, IrSort,
        IrTable, MergeKind, SortClass, TableKind,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EraseMode {
    Standalone,
    OpDef,
    Program,
    Catalog,
}

#[derive(Clone, Debug)]
pub enum MlExpr {
    Var(Sym),
    Global(Sym),
    Lam(Sym, Box<MlExpr>),
    App(Box<MlExpr>, Box<MlExpr>),
    Let(Sym, Box<MlExpr>, Box<MlExpr>),
    Ctor(Sym, Sym, Vec<MlExpr>),
    Match(Box<MlExpr>, Vec<(Pattern, MlExpr)>),
    Fix(Sym, Box<MlExpr>),
    Lit(Lit),
    Prim(PrimOp, Vec<MlExpr>),
    Rec(Vec<(Sym, MlExpr)>),
    Proj(Box<MlExpr>, Sym),
    Erased,
    Schedule(IrSchedule),
    Bytecode {
        bc_id: String,
        bc_bytes: Vec<u8>,
        bc_egg: String,
        bc_schedule: IrSchedule,
    },
}

// ── Structured OpDef Extraction Types ────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortArity {
    Fixed,
    Optional,
    Variadic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MlParamKind {
    Nat,
    Data,
    Sym,
    FloatFormat,
    IndexOrInt,
}

#[derive(Clone, Debug)]
pub struct MlPort {
    pub name: String,
    pub ty: Term,
    pub arity: PortArity,
    pub is_state: bool,
}

#[derive(Clone, Debug)]
pub struct MlAttr {
    pub name: String,
    pub ty: Term,
    pub optional: bool,
}

#[derive(Clone, Debug)]
pub struct MlRegion {
    pub name: String,
    pub kind: String,
    pub variadic: bool,
    pub optional: bool,
}

#[derive(Clone, Debug)]
pub enum MlInterface {
    Pure,
    PureInterface,
    ConstantLike,
    ConstantLikeExternal,
    Speculatable,
    Commutative,
    SameOperandAndResultType,
    IntegerArithmetic,
    Additive,
    Subtractive,
    OpCost(i64),
    MemoryRead,
    MemoryWrite,
    ResourceEffects,
    PromotableAllocation,
    Terminator,
    ExitScope,
    Congruent,
    Symbol,
    Visibility,
    Callable,
    Apply,
    BranchGuard,
    BranchTerminator,
    CaseGuard,
    CountedLoop,
    Gamma,
    Global,
    NonLocalExit,
    RegionBinding,
    Theta,
    FloatingPoint,
    Custom(String),
}

#[derive(Clone, Debug)]
pub enum MlBind {
    Theta {
        inits: String,
        body: String,
        next: String,
        exit: String,
        results: String,
        predicate_slot: usize,
        inits_group: (usize, usize),
    },
    Gamma {
        predicate: String,
        inputs: String,
        arms: String,
        results: String,
        predicate_slot: usize,
        inputs_group: (usize, usize),
    },
}

#[derive(Clone, Debug)]
pub struct MlCounted {
    pub induction: usize,
    pub lower: String,
    pub upper: String,
    pub step: String,
    pub upper_group: usize,
    pub step_group: usize,
}

#[derive(Clone, Debug)]
pub struct MlImm {
    pub name: String,
    pub brand: String,
    pub bits: u16,
    pub align: u32,
    pub nonzero: bool,
    pub allow_fixups: bool,
    pub allow_text: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum MlConstraintArg {
    Input(usize),
    Output(usize),
    Region(usize),
    Text(String),
    TextList(Vec<String>),
}

#[derive(Clone, Debug)]
pub(crate) struct MlTypeConstraint {
    pub(crate) constructor: String,
    pub(crate) arguments: Vec<MlConstraintArg>,
}

#[derive(Clone, Debug)]
pub struct MlOpDef {
    pub op_qname: Sym,
    pub op_dialect: String,
    pub op_name: String,
    pub op_rust_name: String,
    pub op_interfaces: Vec<String>,
    pub tparams: Vec<(String, MlParamKind)>,
    pub operands: Vec<MlPort>,
    pub results: Vec<MlPort>,
    pub attributes: Vec<MlAttr>,
    pub assembly_attrs: Vec<MlAttr>,
    pub regions: Vec<MlRegion>,
    pub successors: Vec<MlPort>,
    pub interfaces: Vec<MlInterface>,
    pub(crate) type_constraints: Vec<MlTypeConstraint>,
    pub binds: Option<MlBind>,
    pub counted: Option<MlCounted>,
    pub immediates: Vec<MlImm>,
    pub macro_def: bool,
    pub custom_format: bool,
    pub widened: bool,
    pub format_enabled: bool,
    pub machine: bool,
}

// ── Structured Program / Catalog Extraction Types ────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MlCodec {
    Int,
    Bool,
    Str,
    BigInt,
    OpId,
    ValueId,
    ValueRef,
    Kind,
    Enum { rust_name: String, egg_sort: String },
}

#[derive(Clone, Debug)]
pub struct MlCol {
    pub name: Option<String>,
    pub codec: MlCodec,
    pub egg_sort: String,
}

#[derive(Clone, Debug)]
pub struct MlEqVariant {
    pub name: String,
    pub egg_ctor: String,
    pub fields: Vec<MlCodec>,
    pub egg_sorts: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MlEqEnum {
    pub qname: Sym,
    pub rust_name: String,
    pub egg_sort: String,
    pub variants: Vec<MlEqVariant>,
}

#[derive(Clone, Debug)]
pub struct MlTable {
    pub qname: Sym,
    pub rust_name: String,
    pub engine_name: String,
    pub kind: TableKind,
    pub cols: Vec<MlCol>,
    pub merge: MergeKind,
    pub exported: bool,
}

#[derive(Clone, Debug)]
pub struct MlExtern {
    pub qname: Sym,
    pub rust_name: String,
    pub engine_name: String,
    pub inputs: Vec<(MlCodec, String)>,
    pub output: (MlCodec, String),
    pub exported: bool,
}

#[derive(Clone, Debug)]
pub struct MlLinkedOp {
    pub dialect: String,
    pub name: String,
    pub symbol: String,
    pub ports: Option<(usize, usize)>,
}

#[derive(Clone, Debug)]
pub struct MlScheduleDecl {
    pub qname: Sym,
    pub rust_name: String,
    pub schedule: IrSchedule,
    pub is_paramless: bool,
}

#[derive(Clone, Debug)]
pub struct MlRuleCollection {
    pub qname: Sym,
    pub rust_name: String,
    pub ruleset_name: String,
}

#[derive(Clone, Debug)]
pub enum MlValueTy {
    Unit,
    Scalar(MlCodec),
    Tuple(Vec<MlValueTy>),
}

#[derive(Clone, Debug)]
pub enum MlMemoBody {
    Tables {
        outputs: Vec<(String, String, Vec<MlCodec>)>,
    },
    Query {
        tables: Vec<(String, String, Vec<MlCodec>)>,
        query_params: Vec<(Sym, MlCodec)>,
        query_expr: MlExpr,
    },
    Value {
        expr: MlExpr,
        ret_ty: MlValueTy,
    },
}

#[derive(Clone, Debug)]
pub struct MlProgramMemo {
    pub qname: Sym,
    pub rust_name: String,
    pub params: Vec<(Sym, MlCodec)>,
    pub locals: Vec<String>,
    pub reads: Vec<String>,
    pub bytes: Vec<u8>,
    pub schedule: IrSchedule,
    pub body: MlMemoBody,
}

#[derive(Clone, Debug)]
pub struct MlDriverFn {
    pub qname: Sym,
    pub rust_name: String,
    pub body: MlExpr,
}

#[derive(Clone, Debug)]
pub struct MlProgramBundle {
    pub enums: Vec<MlEqEnum>,
    pub tables: Vec<MlTable>,
    pub externs: Vec<MlExtern>,
    pub linked_ops: Vec<MlLinkedOp>,
    pub schedules: Vec<MlScheduleDecl>,
    pub collections: Vec<MlRuleCollection>,
    pub memos: Vec<MlProgramMemo>,
    pub fns: Vec<MlDriverFn>,
    pub catalog_bytes: Vec<u8>,
    pub code_bytes: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
pub enum MlDecl {
    Ind(IndDecl),
    Def(Sym, MlExpr),
    OpDef(Box<MlOpDef>),
    TirInterfaces,
    ReexportInterfaces,
    ProgramBundle(Box<MlProgramBundle>),
    Memo {
        name: Sym,
        param: Sym,
        bc_id: String,
        bytes: Vec<u8>,
        egg: String,
        schedule: IrSchedule,
        outputs: Vec<(Sym, String)>,
    },
}

#[derive(Clone, Debug)]
pub struct MlProgram {
    pub decls: Vec<MlDecl>,
    pub bytecodes: Vec<(String, Vec<u8>, String)>,
}

// ── Tag Lookups & Ground Term Decoders ───────────────────────────────────────

fn egg_tag(sig: &Signature, qname: Sym) -> Option<(&str, &[(Sym, String)])> {
    match sig.extracts.get(&(ExtractTarget::Egglog, qname)) {
        Some(ExtractSpec::Tag(tag, ctors)) => Some((tag.as_str(), ctors.as_slice())),
        Some(ExtractSpec::Const(tag)) => Some((tag.as_str(), &[])),
        _ => None,
    }
}

fn rust_tag(sig: &Signature, qname: Sym) -> Option<&str> {
    match sig.extracts.get(&(ExtractTarget::Rust, qname)) {
        Some(ExtractSpec::Const(s))
        | Some(ExtractSpec::Inductive(s, _))
        | Some(ExtractSpec::Tag(s, _)) => Some(s.as_str()),
        None => None,
    }
}

fn ctor_tag(sig: &Signature, tname: Sym, cname: Sym) -> Option<&str> {
    egg_tag(sig, tname).and_then(|(_, cs)| {
        cs.iter()
            .find(|(c, _)| *c == cname)
            .map(|(_, t)| t.as_str())
    })
}

fn leaf_name(s: &str) -> &str {
    s.rsplit("::").next().unwrap_or(s)
}

pub fn to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c == ':' {
            if !out.ends_with('_') {
                out.push('_');
            }
            continue;
        }
        if i != 0 && c.is_ascii_uppercase() && !out.ends_with('_') {
            out.push('_');
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

fn decode_list<'a>(sig: &Signature, mut t: &'a Term) -> Result<Vec<&'a Term>, Error> {
    let mut out = Vec::new();
    loop {
        match t {
            Term::Ctor(tname, cname, args) => match ctor_tag(sig, *tname, *cname) {
                Some("nil") => return Ok(out),
                Some("cons") if args.len() >= 2 => {
                    let n = args.len();
                    out.push(&args[n - 2]);
                    t = &args[n - 1];
                }
                _ => {
                    return Err(Error(format!(
                        "egglog decode: expected list ctor, got {}::{}",
                        sig.interner.resolve(*tname),
                        sig.interner.resolve(*cname)
                    )));
                }
            },
            _ => {
                return Err(Error(format!(
                    "egglog decode: expected list term, got {}",
                    pp_term(&sig.interner, t)
                )));
            }
        }
    }
}

fn decode_str(sig: &Signature, t: &Term) -> Result<String, Error> {
    match t {
        Term::Lit(Lit::Str(s)) => Ok(s.to_string()),
        _ => Err(Error(format!(
            "egglog decode: expected string, got {}",
            pp_term(&sig.interner, t)
        ))),
    }
}

fn decode_int(sig: &Signature, t: &Term) -> Result<i64, Error> {
    match t {
        Term::Lit(Lit::Int(n)) => Ok(*n),
        _ => Err(Error(format!(
            "egglog decode: expected int, got {}",
            pp_term(&sig.interner, t)
        ))),
    }
}

fn decode_bool(sig: &Signature, t: &Term) -> Result<bool, Error> {
    match t {
        Term::Lit(Lit::Bool(b)) => Ok(*b),
        _ => Err(Error(format!(
            "egglog decode: expected bool, got {}",
            pp_term(&sig.interner, t)
        ))),
    }
}

fn decode_atom(sig: &Signature, t: &Term) -> Result<IrAtom, Error> {
    match t {
        Term::Var(x) | Term::Global(x) => Ok(IrAtom::Var(sig.interner.resolve(*x).to_owned())),
        Term::Lit(Lit::Int(n)) => Ok(IrAtom::Int(*n)),
        Term::Lit(Lit::Bool(b)) => Ok(IrAtom::Bool(*b)),
        Term::Lit(Lit::Str(s)) => Ok(IrAtom::Str(s.to_string())),
        Term::Lit(Lit::Unit) => Ok(IrAtom::Unit),
        Term::Ctor(tname, cname, args) => match ctor_tag(sig, *tname, *cname) {
            Some("var") => Ok(IrAtom::Var(decode_str(sig, &args[0])?)),
            Some("int_lit") => Ok(IrAtom::Int(decode_int(sig, &args[0])?)),
            Some("bool_lit") => Ok(IrAtom::Bool(decode_bool(sig, &args[0])?)),
            Some("str_lit") => Ok(IrAtom::Str(decode_str(sig, &args[0])?)),
            Some("unit_lit") => Ok(IrAtom::Unit),
            Some("arg") => Ok(IrAtom::Arg(decode_int(sig, &args[0])? as u32)),
            Some("call") => {
                let f = decode_str(sig, &args[0])?;
                let xs = decode_list(sig, &args[1])?
                    .into_iter()
                    .map(|a| decode_atom(sig, a))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrAtom::Call(f, xs))
            }
            _ => Err(Error(format!(
                "egglog decode: unknown atom ctor {}::{}",
                sig.interner.resolve(*tname),
                sig.interner.resolve(*cname)
            ))),
        },
        other => Err(Error(format!(
            "egglog decode: unexpected atom {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_named_atoms(sig: &Signature, args: &[Term]) -> Result<(String, Vec<IrAtom>), Error> {
    let name = decode_str(sig, &args[0])?;
    let xs = decode_list(sig, &args[1])?
        .into_iter()
        .map(|a| decode_atom(sig, a))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((name, xs))
}

fn decode_atom_pair(sig: &Signature, args: &[Term]) -> Result<(IrAtom, IrAtom), Error> {
    Ok((decode_atom(sig, &args[0])?, decode_atom(sig, &args[1])?))
}

fn decode_fact(sig: &Signature, t: &Term) -> Result<IrFact, Error> {
    match t {
        Term::Ctor(tname, cname, args) => match ctor_tag(sig, *tname, *cname) {
            Some("row") => {
                let (rel, xs) = decode_named_atoms(sig, args)?;
                Ok(IrFact::Row(rel, xs))
            }
            Some("not_row") => {
                let (rel, xs) = decode_named_atoms(sig, args)?;
                Ok(IrFact::NotRow(rel, xs))
            }
            Some("eq") => {
                let (a, b) = decode_atom_pair(sig, args)?;
                Ok(IrFact::Eq(a, b))
            }
            Some("neq") => {
                let (a, b) = decode_atom_pair(sig, args)?;
                Ok(IrFact::Neq(a, b))
            }
            Some("lt") => {
                let (a, b) = decode_atom_pair(sig, args)?;
                Ok(IrFact::Lt(a, b))
            }
            Some("le") => {
                let (a, b) = decode_atom_pair(sig, args)?;
                Ok(IrFact::Le(a, b))
            }
            Some("gt") => {
                let (a, b) = decode_atom_pair(sig, args)?;
                Ok(IrFact::Gt(a, b))
            }
            Some("ge") => {
                let (a, b) = decode_atom_pair(sig, args)?;
                Ok(IrFact::Ge(a, b))
            }
            _ => Err(Error(format!(
                "egglog decode: unknown fact ctor {}::{}",
                sig.interner.resolve(*tname),
                sig.interner.resolve(*cname)
            ))),
        },
        other => Err(Error(format!(
            "egglog decode: unexpected fact {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_action(sig: &Signature, t: &Term) -> Result<IrAction, Error> {
    match t {
        Term::Ctor(tname, cname, args) => match ctor_tag(sig, *tname, *cname) {
            Some("insert") => {
                let (rel, xs) = decode_named_atoms(sig, args)?;
                Ok(IrAction::Insert(rel, xs))
            }
            Some("union") => {
                let (a, b) = decode_atom_pair(sig, args)?;
                Ok(IrAction::Union(a, b))
            }
            Some("delete") => {
                let (rel, xs) = decode_named_atoms(sig, args)?;
                Ok(IrAction::Delete(rel, xs))
            }
            Some("subsume") => {
                let (rel, xs) = decode_named_atoms(sig, args)?;
                Ok(IrAction::Subsume(rel, xs))
            }
            Some("call") => {
                let (op, xs) = decode_named_atoms(sig, args)?;
                Ok(IrAction::Call(op, xs))
            }
            Some("panic") => Ok(IrAction::Panic(decode_str(sig, &args[0])?)),
            _ => Err(Error(format!(
                "egglog decode: unknown action ctor {}::{}",
                sig.interner.resolve(*tname),
                sig.interner.resolve(*cname)
            ))),
        },
        other => Err(Error(format!(
            "egglog decode: unexpected action {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_rule(sig: &mut Signature, vidx: usize, t: &Term) -> Result<IrRule, Error> {
    match t {
        Term::Ctor(tname, cname, args) => match ctor_tag(sig, *tname, *cname) {
            Some("forall") => {
                let lam = args[0].clone();
                let vname = format!("v{vidx}");
                let vsym = sig.interner.intern(&vname);
                let app = Term::App(Box::new(lam), Box::new(Term::Var(vsym)));
                let inst = nbe::normalize(&sig.values, &mut sig.interner, &app)?;
                decode_rule(sig, vidx + 1, &inst)
            }
            Some("clause") => {
                let rname = decode_str(sig, &args[0])?;
                let facts = decode_list(sig, &args[1])?
                    .into_iter()
                    .map(|f| decode_fact(sig, f))
                    .collect::<Result<Vec<_>, _>>()?;
                let actions = decode_list(sig, &args[2])?
                    .into_iter()
                    .map(|a| decode_action(sig, a))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(IrRule {
                    name: rname,
                    facts,
                    actions,
                })
            }
            _ => Err(Error(format!(
                "egglog decode: unknown rule ctor {}::{}",
                sig.interner.resolve(*tname),
                sig.interner.resolve(*cname)
            ))),
        },
        other => Err(Error(format!(
            "egglog decode: unexpected rule {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_schema_sort(sig: &Signature, t: &Term) -> Result<IrSort, Error> {
    match t {
        Term::Ctor(tname, cname, args) => {
            let name = decode_str(sig, &args[0])?;
            let class = match ctor_tag(sig, *tname, *cname) {
                Some("eq_sort") => SortClass::EqSort,
                Some("container_sort") => SortClass::ContainerSort,
                _ => SortClass::BaseSort,
            };
            Ok(IrSort { name, class })
        }
        other => Err(Error(format!(
            "egglog decode: unexpected sort {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_merge_str(m_str: &str) -> MergeKind {
    match m_str {
        "max" => MergeKind::Max,
        "min" => MergeKind::Min,
        "union" => MergeKind::Union,
        "old" => MergeKind::Old,
        "new" => MergeKind::New,
        "" | "unique" | "assert_eq" => MergeKind::AssertEq,
        other => MergeKind::Prim(other.to_owned()),
    }
}

fn decode_schema_extern(sig: &Signature, t: &Term) -> Result<IrExtern, Error> {
    match t {
        Term::Ctor(tname, cname, args) if ctor_tag(sig, *tname, *cname) == Some("extern") => {
            let name = decode_str(sig, &args[0])?;
            let inputs = decode_list(sig, &args[1])?
                .into_iter()
                .map(|c| decode_str(sig, c))
                .collect::<Result<Vec<_>, _>>()?;
            let output = decode_str(sig, &args[2])?;
            Ok(IrExtern {
                name,
                inputs,
                output,
            })
        }
        other => Err(Error(format!(
            "egglog decode: unexpected extern {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_schema_table(sig: &Signature, t: &Term) -> Result<IrTable, Error> {
    match t {
        Term::Ctor(tname, cname, args) => {
            let name = decode_str(sig, &args[0])?;
            let cols = decode_list(sig, &args[1])?
                .into_iter()
                .map(|c| decode_str(sig, c))
                .collect::<Result<Vec<_>, _>>()?;
            match ctor_tag(sig, *tname, *cname) {
                Some("relation") => Ok(IrTable {
                    name,
                    kind: TableKind::Relation,
                    cols,
                    keys: None,
                    merge: MergeKind::None,
                }),
                Some("constructor") => Ok(IrTable {
                    name,
                    kind: TableKind::Constructor,
                    cols,
                    keys: None,
                    merge: MergeKind::Union,
                }),
                Some("function") => {
                    let m_str = decode_str(sig, &args[2])?;
                    Ok(IrTable {
                        name,
                        kind: TableKind::Function,
                        cols,
                        keys: None,
                        merge: decode_merge_str(&m_str),
                    })
                }
                Some("function_k") => {
                    let keys = decode_int(sig, &args[2])? as usize;
                    let m_str = decode_str(sig, &args[3])?;
                    Ok(IrTable {
                        name,
                        kind: TableKind::Function,
                        cols,
                        keys: Some(keys),
                        merge: decode_merge_str(&m_str),
                    })
                }
                _ => Err(Error(format!(
                    "egglog decode: unknown table ctor {}::{}",
                    sig.interner.resolve(*tname),
                    sig.interner.resolve(*cname)
                ))),
            }
        }
        other => Err(Error(format!(
            "egglog decode: unexpected table {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_ruleset(sig: &mut Signature, t: &Term) -> Result<IrRuleset, Error> {
    match t {
        Term::Ctor(tname, cname, args) => match ctor_tag(sig, *tname, *cname) {
            Some("ruleset") => {
                let name = decode_str(sig, &args[0])?;
                let items: Vec<Term> = decode_list(sig, &args[1])?.into_iter().cloned().collect();
                let once = if args.len() > 2 {
                    decode_bool(sig, &args[2])?
                } else {
                    false
                };
                let colored = if args.len() > 3 {
                    decode_bool(sig, &args[3])?
                } else {
                    false
                };
                let mut rules = Vec::with_capacity(items.len());
                for item in &items {
                    rules.push(decode_rule(sig, 0, item)?);
                }
                Ok(IrRuleset {
                    name,
                    rules,
                    members: Vec::new(),
                    once,
                    colored,
                })
            }
            Some("combined_ruleset") => {
                let name = decode_str(sig, &args[0])?;
                let members = decode_list(sig, &args[1])?
                    .into_iter()
                    .map(|m| decode_str(sig, m))
                    .collect::<Result<Vec<_>, _>>()?;
                let colored = if args.len() > 2 {
                    decode_bool(sig, &args[2])?
                } else {
                    false
                };
                Ok(IrRuleset {
                    name,
                    rules: Vec::new(),
                    members,
                    once: false,
                    colored,
                })
            }
            _ => Err(Error(format!(
                "egglog decode: unknown ruleset ctor {}::{}",
                sig.interner.resolve(*tname),
                sig.interner.resolve(*cname)
            ))),
        },
        other => Err(Error(format!(
            "egglog decode: unexpected ruleset {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

fn decode_schedule(
    sig: &mut Signature,
    rs_prefix: &str,
    t: &Term,
) -> Result<(Vec<IrRuleset>, IrSchedule), Error> {
    let mut counter = 0usize;
    let mut rulesets = Vec::new();
    fn go(
        sig: &mut Signature,
        rs_prefix: &str,
        t: &Term,
        counter: &mut usize,
        rulesets: &mut Vec<IrRuleset>,
    ) -> Result<IrSchedule, Error> {
        match t {
            Term::Ctor(tname, cname, args) => {
                let tag = ctor_tag(sig, *tname, *cname).map(str::to_owned);
                match tag.as_deref() {
                    Some("once") => {
                        *counter += 1;
                        let name = format!("{rs_prefix}_s{}", *counter);
                        let items: Vec<Term> =
                            decode_list(sig, &args[0])?.into_iter().cloned().collect();
                        let mut rs = Vec::with_capacity(items.len());
                        for item in &items {
                            rs.push(decode_rule(sig, 0, item)?);
                        }
                        rulesets.push(IrRuleset {
                            name: name.clone(),
                            rules: rs,
                            members: Vec::new(),
                            once: false,
                            colored: false,
                        });
                        Ok(IrSchedule::Run(name, 1))
                    }
                    Some("once_flag") => {
                        *counter += 1;
                        let name = format!("{rs_prefix}_s{}", *counter);
                        let items: Vec<Term> =
                            decode_list(sig, &args[0])?.into_iter().cloned().collect();
                        let mut rs = Vec::with_capacity(items.len());
                        for item in &items {
                            rs.push(decode_rule(sig, 0, item)?);
                        }
                        rulesets.push(IrRuleset {
                            name: name.clone(),
                            rules: rs,
                            members: Vec::new(),
                            once: true,
                            colored: false,
                        });
                        Ok(IrSchedule::Run(name, 1))
                    }
                    Some("colored") => {
                        *counter += 1;
                        let name = format!("{rs_prefix}_s{}", *counter);
                        let items: Vec<Term> =
                            decode_list(sig, &args[0])?.into_iter().cloned().collect();
                        let mut rs = Vec::with_capacity(items.len());
                        for item in &items {
                            rs.push(decode_rule(sig, 0, item)?);
                        }
                        rulesets.push(IrRuleset {
                            name: name.clone(),
                            rules: rs,
                            members: Vec::new(),
                            once: false,
                            colored: true,
                        });
                        Ok(IrSchedule::Run(name, 0))
                    }
                    Some("repeat") => {
                        *counter += 1;
                        let name = format!("{rs_prefix}_s{}", *counter);
                        let n = decode_int(sig, &args[0])? as usize;
                        let items: Vec<Term> =
                            decode_list(sig, &args[1])?.into_iter().cloned().collect();
                        let mut rs = Vec::with_capacity(items.len());
                        for item in &items {
                            rs.push(decode_rule(sig, 0, item)?);
                        }
                        rulesets.push(IrRuleset {
                            name: name.clone(),
                            rules: rs,
                            members: Vec::new(),
                            once: false,
                            colored: false,
                        });
                        Ok(IrSchedule::Run(name, n))
                    }
                    Some("saturate") => {
                        *counter += 1;
                        let name = format!("{rs_prefix}_s{}", *counter);
                        let items: Vec<Term> =
                            decode_list(sig, &args[0])?.into_iter().cloned().collect();
                        let mut rs = Vec::with_capacity(items.len());
                        for item in &items {
                            rs.push(decode_rule(sig, 0, item)?);
                        }
                        rulesets.push(IrRuleset {
                            name: name.clone(),
                            rules: rs,
                            members: Vec::new(),
                            once: false,
                            colored: false,
                        });
                        Ok(IrSchedule::Run(name, 0))
                    }
                    Some("run_named") => {
                        let name = decode_str(sig, &args[0])?;
                        let n = decode_int(sig, &args[1])? as usize;
                        Ok(IrSchedule::Run(name, n))
                    }
                    Some("repeat_named") => {
                        let name = decode_str(sig, &args[0])?;
                        let n = decode_int(sig, &args[1])? as usize;
                        let items: Vec<Term> =
                            decode_list(sig, &args[2])?.into_iter().cloned().collect();
                        let mut rs = Vec::with_capacity(items.len());
                        for item in &items {
                            rs.push(decode_rule(sig, 0, item)?);
                        }
                        rulesets.push(IrRuleset {
                            name: name.clone(),
                            rules: rs,
                            members: Vec::new(),
                            once: false,
                            colored: false,
                        });
                        Ok(IrSchedule::Run(name, n))
                    }
                    Some("seq") => {
                        let items: Vec<Term> =
                            decode_list(sig, &args[0])?.into_iter().cloned().collect();
                        let mut steps = Vec::with_capacity(items.len());
                        for item in &items {
                            steps.push(go(sig, rs_prefix, item, counter, rulesets)?);
                        }
                        Ok(IrSchedule::Seq(steps))
                    }
                    Some("repeat_sched") => {
                        let n = decode_int(sig, &args[0])? as usize;
                        let inner = go(sig, rs_prefix, &args[1], counter, rulesets)?;
                        Ok(IrSchedule::Repeat(n, Box::new(inner)))
                    }
                    Some("saturate_sched") => {
                        let inner = go(sig, rs_prefix, &args[0], counter, rulesets)?;
                        Ok(IrSchedule::Repeat(0, Box::new(inner)))
                    }
                    _ => Err(Error(format!(
                        "egglog decode: unknown schedule ctor {}::{}",
                        sig.interner.resolve(*tname),
                        sig.interner.resolve(*cname)
                    ))),
                }
            }
            other => Err(Error(format!(
                "egglog decode: unexpected schedule {}",
                pp_term(&sig.interner, other)
            ))),
        }
    }
    let sched = go(sig, rs_prefix, t, &mut counter, &mut rulesets)?;
    Ok((rulesets, sched))
}

pub fn decode_egglog_program(
    sig: &mut Signature,
    rs_prefix: &str,
    t: &Term,
) -> Result<IrProgram, Error> {
    match t {
        Term::Ctor(tname, cname, args) => match ctor_tag(sig, *tname, *cname) {
            Some("program") => {
                let sorts = decode_list(sig, &args[0])?
                    .into_iter()
                    .map(|s| decode_schema_sort(sig, s))
                    .collect::<Result<Vec<_>, _>>()?;
                let tables = decode_list(sig, &args[1])?
                    .into_iter()
                    .map(|tbl| decode_schema_table(sig, tbl))
                    .collect::<Result<Vec<_>, _>>()?;
                let sched_term = args[2].clone();
                let (rulesets, schedule) = decode_schedule(sig, rs_prefix, &sched_term)?;
                Ok(IrProgram {
                    sorts,
                    externs: Vec::new(),
                    tables,
                    rulesets,
                    schedule,
                })
            }
            Some("program_full") => {
                let sorts = decode_list(sig, &args[0])?
                    .into_iter()
                    .map(|s| decode_schema_sort(sig, s))
                    .collect::<Result<Vec<_>, _>>()?;
                let externs = decode_list(sig, &args[1])?
                    .into_iter()
                    .map(|e| decode_schema_extern(sig, e))
                    .collect::<Result<Vec<_>, _>>()?;
                let tables = decode_list(sig, &args[2])?
                    .into_iter()
                    .map(|tbl| decode_schema_table(sig, tbl))
                    .collect::<Result<Vec<_>, _>>()?;
                let rs_terms: Vec<Term> =
                    decode_list(sig, &args[3])?.into_iter().cloned().collect();
                let mut rulesets = Vec::with_capacity(rs_terms.len());
                for r in &rs_terms {
                    rulesets.push(decode_ruleset(sig, r)?);
                }
                let schedule = if args.len() > 4 {
                    let sched_term = args[4].clone();
                    let (extra, sched) = decode_schedule(sig, rs_prefix, &sched_term)?;
                    rulesets.extend(extra);
                    sched
                } else {
                    IrSchedule::Seq(Vec::new())
                };
                Ok(IrProgram {
                    sorts,
                    externs,
                    tables,
                    rulesets,
                    schedule,
                })
            }
            _ => Err(Error(format!(
                "egglog decode: expected egg_program ctor, got {}::{}",
                sig.interner.resolve(*tname),
                sig.interner.resolve(*cname)
            ))),
        },
        other => Err(Error(format!(
            "egglog decode: expected egg_program ctor, got {}",
            pp_term(&sig.interner, other)
        ))),
    }
}

// ── Structured OpDef Decoding (Real Dependent Types!) ────────────────────────

fn unwrap_port_type(sig: &Signature, t: &Term) -> (Term, PortArity, bool) {
    match t {
        Term::Ind(sym, args) => {
            let nm = leaf_name(sig.interner.resolve(*sym));
            match (nm, args.as_slice()) {
                ("Variadic", [inner]) => {
                    let (base, _, is_st) = unwrap_port_type(sig, inner);
                    (base, PortArity::Variadic, is_st)
                }
                ("Option" | "Optional", [inner]) => {
                    let (base, _, is_st) = unwrap_port_type(sig, inner);
                    (base, PortArity::Optional, is_st)
                }
                ("El", [inner]) => unwrap_port_type(sig, inner),
                ("State", []) => (t.clone(), PortArity::Fixed, true),
                _ => (t.clone(), PortArity::Fixed, false),
            }
        }
        _ => (t.clone(), PortArity::Fixed, false),
    }
}

fn decode_port_list(sig: &Signature, list_term: &Term) -> Result<Vec<MlPort>, Error> {
    let items = decode_list(sig, list_term)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Term::Ctor(_, _, args) = item
            && args.len() == 2
        {
            let name = decode_str(sig, &args[0])?;
            let (ty, arity, is_state) = unwrap_port_type(sig, &args[1]);
            out.push(MlPort {
                name,
                ty,
                arity,
                is_state,
            });
        }
    }
    Ok(out)
}

fn decode_attr_list(sig: &Signature, list_term: &Term) -> Result<Vec<MlAttr>, Error> {
    let items = decode_list(sig, list_term)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Term::Ctor(_, _, args) = item
            && args.len() == 2
        {
            let name = decode_str(sig, &args[0])?;
            if name == "__record"
                && let Term::RecTy(fields) = &args[1]
            {
                for (fk, fty) in fields {
                    let (ty, arity, _) = unwrap_port_type(sig, fty);
                    out.push(MlAttr {
                        name: sig.interner.resolve(*fk).to_owned(),
                        ty,
                        optional: matches!(arity, PortArity::Optional),
                    });
                }
                continue;
            }
            let (ty, arity, _) = unwrap_port_type(sig, &args[1]);
            out.push(MlAttr {
                name,
                ty,
                optional: matches!(arity, PortArity::Optional),
            });
        }
    }
    Ok(out)
}

fn decode_region_list(sig: &Signature, list_term: &Term) -> Result<Vec<MlRegion>, Error> {
    let items = decode_list(sig, list_term)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Term::Ctor(_, _, args) = item
            && args.len() == 2
        {
            let name = decode_str(sig, &args[0])?;
            let (base, arity, _) = unwrap_port_type(sig, &args[1]);
            let kind = match &base {
                Term::Ind(sym, inner) => match leaf_name(sig.interner.resolve(*sym)) {
                    "Nodes" => "Nodes".to_owned(),
                    "Graph" => "Graph".to_owned(),
                    "AnyRegion" => "Any".to_owned(),
                    "Cfg" | "Blocks" => "Blocks".to_owned(),
                    "Region" if !inner.is_empty() => match &inner[0] {
                        Term::Ind(s2, _) => match leaf_name(sig.interner.resolve(*s2)) {
                            "Nodes" => "Nodes".to_owned(),
                            "Graph" => "Graph".to_owned(),
                            _ => "Blocks".to_owned(),
                        },
                        _ => "Blocks".to_owned(),
                    },
                    _ => "Blocks".to_owned(),
                },
                _ => "Blocks".to_owned(),
            };
            out.push(MlRegion {
                name,
                kind,
                variadic: matches!(arity, PortArity::Variadic),
                optional: matches!(arity, PortArity::Optional),
            });
        }
    }
    Ok(out)
}

fn decode_interface(sig: &Signature, t: &Term) -> Result<(MlInterface, String), Error> {
    match t {
        Term::Ctor(_, cname, args) => {
            let nm = sig.interner.resolve(*cname);
            let iface = match nm {
                "Pure" => MlInterface::Pure,
                "PureInterface" => MlInterface::PureInterface,
                "ConstantLike" => MlInterface::ConstantLike,
                "ConstantLikeExternal" => MlInterface::ConstantLikeExternal,
                "Speculatable" => MlInterface::Speculatable,
                "Commutative" => MlInterface::Commutative,
                "SameOperandAndResultType" => MlInterface::SameOperandAndResultType,
                "IntegerArithmetic" => MlInterface::IntegerArithmetic,
                "Additive" => MlInterface::Additive,
                "Subtractive" => MlInterface::Subtractive,
                "OpCost" if !args.is_empty() => MlInterface::OpCost(decode_int(sig, &args[0])?),
                "MemoryRead" => MlInterface::MemoryRead,
                "MemoryWrite" => MlInterface::MemoryWrite,
                "ResourceEffects" => MlInterface::ResourceEffects,
                "PromotableAllocation" => MlInterface::PromotableAllocation,
                "Terminator" => MlInterface::Terminator,
                "ExitScope" => MlInterface::ExitScope,
                "Congruent" => MlInterface::Congruent,
                "Symbol" => MlInterface::Symbol,
                "Visibility" => MlInterface::Visibility,
                "Callable" => MlInterface::Callable,
                "Apply" => MlInterface::Apply,
                "BranchGuard" => MlInterface::BranchGuard,
                "BranchTerminator" => MlInterface::BranchTerminator,
                "CaseGuard" => MlInterface::CaseGuard,
                "CountedLoop" => MlInterface::CountedLoop,
                "Gamma" => MlInterface::Gamma,
                "Global" => MlInterface::Global,
                "NonLocalExit" => MlInterface::NonLocalExit,
                "RegionBinding" => MlInterface::RegionBinding,
                "Theta" => MlInterface::Theta,
                "FloatingPoint" => MlInterface::FloatingPoint,
                "Custom" if !args.is_empty() => MlInterface::Custom(decode_str(sig, &args[0])?),
                other => MlInterface::Custom(other.to_owned()),
            };
            Ok((iface, nm.to_owned()))
        }
        Term::Lit(Lit::Str(s)) => Ok((MlInterface::Custom(s.to_string()), s.to_string())),
        other => {
            let s = pp_term(&sig.interner, other);
            Ok((MlInterface::Custom(s.clone()), s))
        }
    }
}

fn decode_type_constraint(
    sig: &Signature,
    term: &Term,
    operands: &[MlPort],
    results: &[MlPort],
    regions: &[MlRegion],
) -> Result<Option<MlTypeConstraint>, Error> {
    let Term::Ctor(_, name, fields) = term else {
        return Ok(None);
    };
    if sig.interner.resolve(*name) != "Check" {
        return Ok(None);
    }
    let [Term::Ctor(_, constructor, arguments)] = fields.as_slice() else {
        return Err(Error("expected a type constraint after Check".into()));
    };
    let arguments: Vec<_> = arguments
        .iter()
        .map(|term| match term {
            Term::Lit(Lit::Str(text)) => Ok(MlConstraintArg::Text(text.to_string())),
            Term::Ctor(ty, name, _)
                if matches!(ctor_tag(sig, *ty, *name), Some("nil" | "cons")) =>
            {
                let entries = decode_list(sig, term)?
                    .into_iter()
                    .map(|entry| decode_str(sig, entry))
                    .collect::<Result<_, _>>()?;
                Ok(MlConstraintArg::TextList(entries))
            }
            Term::Ctor(_, name, fields) => {
                let [field] = fields.as_slice() else {
                    return Err(Error("expected one group name in type constraint".into()));
                };
                let port = decode_str(sig, field)?;
                if sig.interner.resolve(*name) == "Region" {
                    let index = regions
                        .iter()
                        .position(|field| field.name == port)
                        .ok_or_else(|| {
                            Error(format!(
                                "type constraint references unknown region {port:?}"
                            ))
                        })?;
                    if regions[..=index]
                        .iter()
                        .any(|region| region.optional || region.variadic)
                    {
                        return Err(Error(format!(
                            "type constraint requires a fixed slot for region {port:?}"
                        )));
                    }
                    return Ok(MlConstraintArg::Region(index));
                }
                let (ports, select): (_, fn(usize) -> MlConstraintArg) = match sig
                    .interner
                    .resolve(*name)
                {
                    "Input" => (operands, MlConstraintArg::Input),
                    "Output" => (results, MlConstraintArg::Output),
                    _ => return Err(Error("expected Input or Output in type constraint".into())),
                };
                let index = ports
                    .iter()
                    .position(|field| field.name == port)
                    .ok_or_else(|| {
                        Error(format!("type constraint references unknown port {port:?}"))
                    })?;
                Ok(select(index))
            }
            _ => Err(Error("invalid type constraint argument".into())),
        })
        .collect::<Result<_, _>>()?;
    let constructor = sig.interner.resolve(*constructor);
    if matches!(constructor, "OrderedLoop" | "SwitchCases")
        && !matches!(arguments.first(), Some(MlConstraintArg::Input(_)))
    {
        return Err(Error(format!("{constructor} requires an input port")));
    }
    Ok(Some(MlTypeConstraint {
        constructor: constructor.to_owned(),
        arguments,
    }))
}

fn find_operand_group(operands: &[MlPort], spec: &str) -> (usize, usize) {
    let parts: Vec<&str> = spec
        .trim_matches(|c| c == '(' || c == ')')
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let first = parts
        .first()
        .and_then(|n| operands.iter().position(|p| p.name == *n))
        .unwrap_or(0);
    let last = parts
        .last()
        .and_then(|n| operands.iter().position(|p| p.name == *n))
        .unwrap_or(first);
    (first, last)
}

fn decode_bind_spec(
    sig: &Signature,
    t: &Term,
    operands: &[MlPort],
) -> Result<Option<MlBind>, Error> {
    let Term::Ctor(_, cname, args) = t else {
        return Ok(None);
    };
    match sig.interner.resolve(*cname) {
        "Theta" if args.len() >= 6 => {
            let inits = decode_str(sig, &args[0])?;
            let body = decode_str(sig, &args[1])?;
            let next = decode_str(sig, &args[2])?;
            let exit = decode_str(sig, &args[3])?;
            let results = decode_str(sig, &args[4])?;
            let predicate_slot = decode_int(sig, &args[5])? as usize;
            let inits_group = find_operand_group(operands, &inits);
            Ok(Some(MlBind::Theta {
                inits,
                body,
                next,
                exit,
                results,
                predicate_slot,
                inits_group,
            }))
        }
        "Gamma" if args.len() >= 4 => {
            let predicate = decode_str(sig, &args[0])?;
            let inputs = decode_str(sig, &args[1])?;
            let arms = decode_str(sig, &args[2])?;
            let results = decode_str(sig, &args[3])?;
            let predicate_slot = operands
                .iter()
                .position(|p| p.name == predicate)
                .unwrap_or(0);
            let inputs_group = find_operand_group(operands, &inputs);
            Ok(Some(MlBind::Gamma {
                predicate,
                inputs,
                arms,
                results,
                predicate_slot,
                inputs_group,
            }))
        }
        _ => Ok(None),
    }
}

fn decode_counted_spec(
    sig: &Signature,
    t: &Term,
    operands: &[MlPort],
) -> Result<Option<MlCounted>, Error> {
    let Term::Ctor(_, cname, args) = t else {
        return Ok(None);
    };
    if sig.interner.resolve(*cname) == "Counted" && args.len() >= 4 {
        let lower = decode_str(sig, &args[0])?;
        let upper = decode_str(sig, &args[1])?;
        let step = decode_str(sig, &args[2])?;
        let induction = decode_str(sig, &args[3])?.parse::<usize>().unwrap_or(0);
        let upper_group = operands.iter().position(|p| p.name == upper).unwrap_or(0);
        let step_group = operands.iter().position(|p| p.name == step).unwrap_or(0);
        return Ok(Some(MlCounted {
            induction,
            lower,
            upper,
            step,
            upper_group,
            step_group,
        }));
    }
    Ok(None)
}

fn decode_imm_list(sig: &Signature, list_term: &Term) -> Result<Vec<MlImm>, Error> {
    let items = decode_list(sig, list_term)?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        if let Term::Ctor(_, _, args) = item
            && args.len() >= 7
        {
            out.push(MlImm {
                name: decode_str(sig, &args[0])?,
                brand: decode_str(sig, &args[1])?,
                bits: decode_int(sig, &args[2])? as u16,
                align: decode_int(sig, &args[3])? as u32,
                nonzero: decode_bool(sig, &args[4])?,
                allow_fixups: decode_bool(sig, &args[5])?,
                allow_text: decode_bool(sig, &args[6])?,
            });
        }
    }
    Ok(out)
}

fn is_op_def_result_type(sig: &Signature, ty: &Rc<Value>) -> bool {
    let mut cur = ty.clone();
    for _ in 0..32 {
        match cur.as_ref() {
            Value::Ind(name, _) => return rust_tag(sig, *name) == Some("@op_def"),
            Value::Pi(x, _, cod) => {
                let dummy = Rc::new(Value::Neutral(Neutral::Global(*x)));
                match nbe::apply_closure(&sig.values, &sig.interner, cod, dummy) {
                    Ok(next) => cur = next,
                    Err(_) => return false,
                }
            }
            _ => return false,
        }
    }
    false
}

fn classify_param_kind(sig: &Signature, ann: Option<&Term>) -> MlParamKind {
    match ann {
        Some(Term::Ind(s, _)) | Some(Term::Global(s)) => {
            match leaf_name(sig.interner.resolve(*s)) {
                "Nat" | "Int" => MlParamKind::Nat,
                "Format" => MlParamKind::FloatFormat,
                "IndexOrInt" => MlParamKind::IndexOrInt,
                "Rounding" | "Contraction" => MlParamKind::Sym,
                _ => MlParamKind::Data,
            }
        }
        _ => MlParamKind::Data,
    }
}

fn decode_op_def(sig: &mut Signature, name: Sym, body: &Term) -> Result<Option<MlOpDef>, Error> {
    // Peel leading `<w: Nat>`, `<T>`, `<P0: Ty>`, `<F: Format>` lambdas so type
    // parameter names stay intact as `Term::Var` while type aliases normalize!
    let mut tparams = Vec::new();
    let mut env = None;
    let mut lvl = 0u32;
    let mut cur = body;
    while let Term::Lam(x, ann, inner) = cur {
        let pname = sig.interner.resolve(*x).to_owned();
        let pkind = classify_param_kind(sig, ann.as_deref());
        tparams.push((pname, pkind));
        let v = Rc::new(Value::Neutral(Neutral::Var(*x, lvl)));
        env = env_extend(&env, *x, v);
        lvl += 1;
        cur = inner.as_ref();
    }
    let ev = nbe::eval(&sig.values, &sig.interner, &env, cur)?;
    let norm = nbe::quote(&sig.values, &mut sig.interner, lvl, &ev)?;

    match norm {
        Term::Ctor(_, _, args) if args.len() >= 15 => {
            let dialect = decode_str(sig, &args[0])?;
            let op_name = decode_str(sig, &args[1])?;
            let rust_name = decode_str(sig, &args[2])?;
            let operands = decode_port_list(sig, &args[3])?;
            let results = decode_port_list(sig, &args[4])?;
            let attributes = decode_attr_list(sig, &args[5])?;
            let regions = decode_region_list(sig, &args[6])?;
            let successors = decode_port_list(sig, &args[7])?;
            let iface_terms = decode_list(sig, &args[8])?;
            let mut interfaces = Vec::with_capacity(iface_terms.len());
            let mut op_interfaces = Vec::with_capacity(iface_terms.len());
            let mut type_constraints = Vec::new();
            for it in iface_terms {
                if let Some(check) = decode_type_constraint(sig, it, &operands, &results, &regions)?
                {
                    type_constraints.push(check);
                    continue;
                }
                let (ifv, nm) = decode_interface(sig, it)?;
                interfaces.push(ifv);
                op_interfaces.push(nm);
            }
            let binds = decode_bind_spec(sig, &args[11], &operands)?;
            let counted = decode_counted_spec(sig, &args[12], &operands)?;
            let immediates = decode_imm_list(sig, &args[13])?;
            let (assembly_attrs, macro_def, custom_format, widened, format_enabled, machine) =
                if args.len() >= 16 {
                    let asm = decode_attr_list(sig, &args[14])?;
                    let (m, cf, w, f, mach) = match &args[15] {
                        Term::Ctor(_, _, fargs) if fargs.len() >= 5 => (
                            decode_bool(sig, &fargs[0])?,
                            decode_bool(sig, &fargs[1])?,
                            decode_bool(sig, &fargs[2])?,
                            decode_bool(sig, &fargs[3])?,
                            decode_bool(sig, &fargs[4])?,
                        ),
                        _ => (false, false, false, false, false),
                    };
                    (asm, m, cf, w, f, mach)
                } else {
                    (
                        Vec::new(),
                        decode_bool(sig, &args[14])?,
                        false,
                        false,
                        false,
                        false,
                    )
                };
            Ok(Some(MlOpDef {
                op_qname: name,
                op_dialect: dialect,
                op_name,
                op_rust_name: rust_name,
                op_interfaces,
                tparams,
                operands,
                results,
                attributes,
                assembly_attrs,
                regions,
                successors,
                interfaces,
                type_constraints,
                binds,
                counted,
                immediates,
                macro_def,
                custom_format,
                widened,
                format_enabled,
                machine,
            }))
        }
        Term::Ctor(_, _, args) if args.len() >= 4 => {
            // Legacy 9-field OpDef::Define (used by standalone prototype tests/benches).
            let dialect = decode_str(sig, &args[0])?;
            let op_name = decode_str(sig, &args[1])?;
            let rust_name = decode_str(sig, &args[2])?;
            let lst = args.last().unwrap();
            let iface_terms = decode_list(sig, lst)?;
            let mut interfaces = Vec::with_capacity(iface_terms.len());
            let mut op_interfaces = Vec::with_capacity(iface_terms.len());
            for it in iface_terms {
                let (ifv, nm) = decode_interface(sig, it)?;
                interfaces.push(ifv);
                op_interfaces.push(nm);
            }
            Ok(Some(MlOpDef {
                op_qname: name,
                op_dialect: dialect,
                op_name,
                op_rust_name: rust_name,
                op_interfaces,
                tparams,
                operands: Vec::new(),
                results: Vec::new(),
                attributes: Vec::new(),
                assembly_attrs: Vec::new(),
                regions: Vec::new(),
                successors: Vec::new(),
                interfaces,
                type_constraints: Vec::new(),
                binds: None,
                counted: None,
                immediates: Vec::new(),
                macro_def: false,
                custom_format: false,
                widened: false,
                format_enabled: false,
                machine: false,
            }))
        }
        _ => Ok(None),
    }
}

// ── Rocq-style Erasure: Core -> MiniML ───────────────────────────────────────

fn is_erased_type(sig: &Signature, lvl: u32, ty: &Rc<Value>) -> bool {
    matches!(ty.as_ref(), Value::Sort(_)) || sig.is_prop_type(lvl, ty)
}

fn is_egg_program_type(sig: &Signature, ty: &Rc<Value>) -> bool {
    match ty.as_ref() {
        Value::Ind(name, _) => matches!(egg_tag(sig, *name), Some(("egg_program", _))),
        _ => false,
    }
}

fn is_schedule_type(sig: &Signature, ty: &Rc<Value>) -> bool {
    match ty.as_ref() {
        Value::Ind(name, _) => matches!(egg_tag(sig, *name), Some(("schedule", _))),
        _ => false,
    }
}

fn is_rewrite_list_type(sig: &Signature, ty: &Rc<Value>) -> bool {
    if let Value::Ind(name, args) = ty.as_ref()
        && matches!(egg_tag(sig, *name), Some(("list", _)))
        && let Some(elem) = args.first()
        && let Value::Ind(en, _) = elem.as_ref()
    {
        return matches!(egg_tag(sig, *en), Some(("@rewrite", _)));
    }
    false
}

fn is_rule_list_type(sig: &Signature, ty: &Rc<Value>) -> bool {
    if let Value::Ind(name, args) = ty.as_ref()
        && matches!(egg_tag(sig, *name), Some(("list", _)))
        && let Some(elem) = args.first()
        && let Value::Ind(en, _) = elem.as_ref()
    {
        return matches!(egg_tag(sig, *en), Some(("rule", _)));
    }
    false
}

fn is_ct_name(sig: &Signature, tn: Sym) -> bool {
    (sig.extracts.contains_key(&(ExtractTarget::Egglog, tn))
        && !sig.extracts.contains_key(&(ExtractTarget::Rust, tn)))
        || matches!(rust_tag(sig, tn), Some("@ct" | "@op_def"))
}

fn has_ct_type(sig: &Signature, ty: &Rc<Value>) -> bool {
    match ty.as_ref() {
        Value::Ind(tn, args) => is_ct_name(sig, *tn) || args.iter().any(|a| has_ct_type(sig, a)),
        Value::Pi(_, dom, cod) => {
            if has_ct_type(sig, dom) {
                return true;
            }
            let dummy = Rc::new(Value::Neutral(Neutral::Global(sig.sym_wild)));
            nbe::apply_closure(&sig.values, &sig.interner, cod, dummy)
                .is_ok_and(|res| has_ct_type(sig, &res))
        }
        _ => false,
    }
}

fn sanitize_bc_id(hint: &str, counter: &mut usize) -> String {
    *counter += 1;
    let clean: String = hint
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("{clean}_{}", *counter)
}

#[allow(clippy::too_many_arguments)]
fn compile_egg_bytecode(
    sig: &mut Signature,
    ctx: &Ctx,
    hint: &str,
    t: &Term,
    bytecodes: &mut Vec<(String, Vec<u8>, String)>,
    bc_counter: &mut usize,
) -> Result<MlExpr, Error> {
    let ev = nbe::eval(&sig.values, &sig.interner, &ctx.env, t)?;
    let norm = nbe::quote(&sig.values, &mut sig.interner, ctx.lvl, &ev)?;
    let ir = decode_egglog_program(sig, hint, &norm)?;
    let egg_txt = tegg::pp_egg(&ir);
    let bytes = tegg::compile(&ir)?;
    let id = sanitize_bc_id(hint, bc_counter);
    bytecodes.push((id.clone(), bytes.clone(), egg_txt.clone()));
    Ok(MlExpr::Bytecode {
        bc_id: id,
        bc_bytes: bytes,
        bc_egg: egg_txt,
        bc_schedule: ir.schedule,
    })
}

#[allow(clippy::too_many_arguments)]
fn erase_check(
    sig: &mut Signature,
    ctx: &mut Ctx,
    hint: &str,
    t: &Term,
    expected: &Rc<Value>,
    bytecodes: &mut Vec<(String, Vec<u8>, String)>,
    bc_counter: &mut usize,
) -> Result<MlExpr, Error> {
    if is_erased_type(sig, ctx.lvl, expected) {
        return Ok(MlExpr::Erased);
    }
    if is_egg_program_type(sig, expected) && !matches!(t, Term::Var(_)) {
        return compile_egg_bytecode(sig, ctx, hint, t, bytecodes, bc_counter);
    }
    match (t, expected.as_ref()) {
        (Term::Lam(x, ann, body), Value::Pi(_, dom, cod)) => {
            let dom_val = match ann {
                Some(a) => nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?,
                None => dom.clone(),
            };
            let dom_erased = is_erased_type(sig, ctx.lvl, &dom_val);
            let mark = ctx.mark();
            let v = ctx.push_var(*x, dom_val);
            let exp2 = nbe::apply_closure(&sig.values, &sig.interner, cod, v);
            let ebody = match exp2 {
                Ok(exp2) => erase_check(sig, ctx, hint, body, &exp2, bytecodes, bc_counter),
                Err(e) => Err(e),
            };
            ctx.restore(mark);
            let ebody = ebody?;
            if dom_erased {
                Ok(ebody)
            } else {
                Ok(MlExpr::Lam(*x, Box::new(ebody)))
            }
        }
        (Term::Let(x, ann, rhs, body), _) => {
            let (ty_rhs, erhs_opt) = match ann {
                Some(a) => (nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?, None),
                None => {
                    let x_name = sig.interner.resolve(*x).to_owned();
                    let sub_hint = format!("{hint}_{x_name}");
                    let (er, tr) = erase_infer(sig, ctx, &sub_hint, rhs, bytecodes, bc_counter)?;
                    (tr, Some(er))
                }
            };
            let skip = is_erased_type(sig, ctx.lvl, &ty_rhs)
                || (has_ct_type(sig, &ty_rhs) && !is_egg_program_type(sig, &ty_rhs));
            let vrhs = if *x == sig.sym_wild {
                sig.val_unit.clone()
            } else {
                nbe::eval(&sig.values, &sig.interner, &ctx.env, rhs)?
            };
            let erhs = if skip {
                None
            } else if let Some(er) = erhs_opt {
                Some(er)
            } else {
                let x_name = sig.interner.resolve(*x).to_owned();
                let sub_hint = format!("{hint}_{x_name}");
                Some(erase_check(
                    sig, ctx, &sub_hint, rhs, &ty_rhs, bytecodes, bc_counter,
                )?)
            };
            let mark = ctx.mark();
            ctx.push_local(*x, ty_rhs, vrhs);
            let ebody = erase_check(sig, ctx, hint, body, expected, bytecodes, bc_counter);
            ctx.restore(mark);
            let ebody = ebody?;
            match erhs {
                None => Ok(ebody),
                Some(er) => Ok(MlExpr::Let(*x, Box::new(er), Box::new(ebody))),
            }
        }
        (Term::Match(scrut, branches), _) => {
            let (es, sty) = erase_infer(sig, ctx, hint, scrut, bytecodes, bc_counter)?;
            let mut ebs = Vec::with_capacity(branches.len());
            for (p, rhs) in branches.iter() {
                let mark = ctx.mark();
                check::bind_pattern_in_place(sig, ctx, &sty, p)?;
                let erhs = erase_check(sig, ctx, hint, rhs, expected, bytecodes, bc_counter);
                ctx.restore(mark);
                ebs.push((p.clone(), erhs?));
            }
            Ok(MlExpr::Match(Box::new(es), ebs))
        }
        (Term::Rec(fs), Value::RecTy(exp_fs)) if fs.len() == exp_fs.len() => {
            let mut efs = Vec::with_capacity(fs.len());
            for ((k, v), (_, ft)) in fs.iter().zip(exp_fs) {
                let k_name = sig.interner.resolve(*k).to_owned();
                let sub_hint = format!("{hint}_{k_name}");
                efs.push((
                    *k,
                    erase_check(sig, ctx, &sub_hint, v, ft, bytecodes, bc_counter)?,
                ));
            }
            Ok(MlExpr::Rec(efs))
        }
        _ => Ok(erase_infer(sig, ctx, hint, t, bytecodes, bc_counter)?.0),
    }
}

#[allow(clippy::too_many_arguments, clippy::cognitive_complexity)]
fn erase_infer(
    sig: &mut Signature,
    ctx: &mut Ctx,
    hint: &str,
    t: &Term,
    bytecodes: &mut Vec<(String, Vec<u8>, String)>,
    bc_counter: &mut usize,
) -> Result<(MlExpr, Rc<Value>), Error> {
    match t {
        Term::Var(x) => {
            let ty = ctx
                .lookup_local(*x)
                .or_else(|| sig.types.get(x).cloned())
                .ok_or_else(|| Error(format!("unbound variable: {}", sig.interner.resolve(*x))))?;
            if is_erased_type(sig, ctx.lvl, &ty) {
                Ok((MlExpr::Erased, ty))
            } else {
                Ok((MlExpr::Var(*x), ty))
            }
        }
        Term::Global(g) => {
            let ty =
                sig.types.get(g).cloned().ok_or_else(|| {
                    Error(format!("unbound global: {}", sig.interner.resolve(*g)))
                })?;
            if is_erased_type(sig, ctx.lvl, &ty) {
                Ok((MlExpr::Erased, ty))
            } else if is_egg_program_type(sig, &ty) {
                let bc = compile_egg_bytecode(sig, ctx, hint, t, bytecodes, bc_counter)?;
                Ok((bc, ty))
            } else {
                Ok((MlExpr::Global(*g), ty))
            }
        }
        Term::Sort(_) | Term::Pi(_, _, _) | Term::Ind(_, _) | Term::RecTy(_) | Term::Hole => {
            Ok((MlExpr::Erased, sig.val_sort_type.clone()))
        }
        Term::Lit(l) => Ok((MlExpr::Lit(l.clone()), sig.lit_type(l))),
        Term::Lam(x, Some(a), body) => {
            let dom = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
            let dom_erased = is_erased_type(sig, ctx.lvl, &dom);
            let env_snap = ctx.env.clone();
            let mark = ctx.mark();
            ctx.push_var(*x, dom.clone());
            let res = erase_infer(sig, ctx, hint, body, bytecodes, bc_counter);
            let lvl2 = ctx.lvl;
            ctx.restore(mark);
            let (ebody, vb) = res?;
            let qb = nbe::quote(&sig.values, &mut sig.interner, lvl2, &vb)?;
            let ty = Rc::new(Value::Pi(
                *x,
                dom,
                crate::core::Closure::Term {
                    env: env_snap,
                    param: *x,
                    body: Rc::new(qb),
                },
            ));
            if is_erased_type(sig, ctx.lvl, &ty) {
                Ok((MlExpr::Erased, ty))
            } else if dom_erased {
                Ok((ebody, ty))
            } else {
                Ok((MlExpr::Lam(*x, Box::new(ebody)), ty))
            }
        }
        Term::Lam(x, None, body) => {
            let mark = ctx.mark();
            ctx.push_var(*x, sig.val_unit.clone());
            let res = erase_infer(sig, ctx, hint, body, bytecodes, bc_counter);
            ctx.restore(mark);
            let (ebody, vb) = res?;
            Ok((MlExpr::Lam(*x, Box::new(ebody)), vb))
        }
        Term::App(_, _) => {
            let mut head = t;
            let mut args = Vec::new();
            while let Term::App(f, a) = head {
                args.push(a.as_ref());
                head = f.as_ref();
            }
            args.reverse();

            let (mut res_expr, mut res_ty) =
                erase_infer(sig, ctx, hint, head, bytecodes, bc_counter)?;
            let mut idx = 0;
            while idx < args.len() {
                let a = args[idx];
                match res_ty.as_ref() {
                    Value::Pi(_, dom, cod)
                        if matches!(dom.as_ref(), Value::Sort(Sort::Type))
                            && check::is_explicit_type_arg(ctx, a) =>
                    {
                        let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
                        res_ty = nbe::apply_closure(&sig.values, &sig.interner, cod, va)?;
                        idx += 1;
                    }
                    Value::Pi(_, dom, _) if matches!(dom.as_ref(), Value::Sort(Sort::Type)) => {
                        let lvl_start = ctx.lvl;
                        let mut probe = res_ty.clone();
                        let mut n_impl = 0usize;
                        while let Value::Pi(px, d, c) = probe.as_ref() {
                            if !matches!(d.as_ref(), Value::Sort(Sort::Type)) {
                                break;
                            }
                            let var = Rc::new(Value::Neutral(Neutral::Var(
                                *px,
                                lvl_start + n_impl as u32,
                            )));
                            probe = nbe::apply_closure(&sig.values, &sig.interner, c, var)?;
                            n_impl += 1;
                        }
                        let mut n_holes = 0usize;
                        while n_holes < n_impl
                            && idx + n_holes < args.len()
                            && matches!(args[idx + n_holes], Term::Hole)
                        {
                            n_holes += 1;
                        }
                        let val_start = idx + n_holes;
                        let mut solutions: Vec<Option<Rc<Value>>> = vec![None; n_impl];
                        let mut probe_arg_ty = probe;
                        for (arg_lvl, &rem_arg) in
                            (lvl_start + n_impl as u32..).zip(args[val_start..].iter())
                        {
                            let Value::Pi(px, d, c) = probe_arg_ty.as_ref() else {
                                break;
                            };
                            if let Ok(ta) = check::infer(sig, ctx, rem_arg) {
                                check::unify_implicits(
                                    sig,
                                    lvl_start,
                                    arg_lvl,
                                    d,
                                    &ta,
                                    &mut solutions,
                                );
                            }
                            let dummy = Rc::new(Value::Neutral(Neutral::Var(*px, arg_lvl)));
                            let Ok(next_probe) =
                                nbe::apply_closure(&sig.values, &sig.interner, c, dummy)
                            else {
                                break;
                            };
                            probe_arg_ty = next_probe;
                            if solutions.iter().all(Option::is_some) {
                                break;
                            }
                        }
                        for sol in solutions {
                            let ty_val = sol.unwrap_or_else(|| sig.val_unit.clone());
                            if let Value::Pi(_, _, cod) = res_ty.as_ref() {
                                res_ty =
                                    nbe::apply_closure(&sig.values, &sig.interner, cod, ty_val)?;
                            }
                        }
                        idx = val_start;
                    }
                    Value::Pi(px, dom, cod) => {
                        let dom_erased = is_erased_type(sig, ctx.lvl, dom);
                        let va = if *px == sig.sym_wild {
                            sig.val_unit.clone()
                        } else {
                            nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?
                        };
                        let rty = nbe::apply_closure(&sig.values, &sig.interner, cod, va)?;
                        if !dom_erased {
                            let ea = erase_check(sig, ctx, hint, a, dom, bytecodes, bc_counter)?;
                            res_expr = MlExpr::App(Box::new(res_expr), Box::new(ea));
                        }
                        res_ty = rty;
                        idx += 1;
                    }
                    _ => {
                        let (ea, _) = erase_infer(sig, ctx, hint, a, bytecodes, bc_counter)?;
                        res_expr = MlExpr::App(Box::new(res_expr), Box::new(ea));
                        res_ty = sig.val_unit.clone();
                        idx += 1;
                    }
                }
            }
            if is_erased_type(sig, ctx.lvl, &res_ty) {
                Ok((MlExpr::Erased, res_ty))
            } else if is_egg_program_type(sig, &res_ty) {
                let bc = compile_egg_bytecode(sig, ctx, hint, t, bytecodes, bc_counter)?;
                Ok((bc, res_ty))
            } else {
                Ok((res_expr, res_ty))
            }
        }
        Term::Let(x, ann, rhs, body) => {
            let (ty_rhs, erhs_opt) = match ann {
                Some(a) => (nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?, None),
                None => {
                    let x_name = sig.interner.resolve(*x).to_owned();
                    let sub_hint = format!("{hint}_{x_name}");
                    let (er, tr) = erase_infer(sig, ctx, &sub_hint, rhs, bytecodes, bc_counter)?;
                    (tr, Some(er))
                }
            };
            let skip = is_erased_type(sig, ctx.lvl, &ty_rhs)
                || (has_ct_type(sig, &ty_rhs) && !is_egg_program_type(sig, &ty_rhs));
            let vrhs = if *x == sig.sym_wild {
                sig.val_unit.clone()
            } else {
                nbe::eval(&sig.values, &sig.interner, &ctx.env, rhs)?
            };
            let erhs = if skip {
                None
            } else if let Some(er) = erhs_opt {
                Some(er)
            } else {
                let x_name = sig.interner.resolve(*x).to_owned();
                let sub_hint = format!("{hint}_{x_name}");
                Some(erase_check(
                    sig, ctx, &sub_hint, rhs, &ty_rhs, bytecodes, bc_counter,
                )?)
            };
            let mark = ctx.mark();
            ctx.push_local(*x, ty_rhs, vrhs);
            let res = erase_infer(sig, ctx, hint, body, bytecodes, bc_counter);
            ctx.restore(mark);
            let (ebody, body_ty) = res?;
            if is_erased_type(sig, ctx.lvl, &body_ty) {
                Ok((MlExpr::Erased, body_ty))
            } else if is_egg_program_type(sig, &body_ty) {
                let bc = compile_egg_bytecode(sig, ctx, hint, t, bytecodes, bc_counter)?;
                Ok((bc, body_ty))
            } else {
                match erhs {
                    None => Ok((ebody, body_ty)),
                    Some(er) => Ok((MlExpr::Let(*x, Box::new(er), Box::new(ebody)), body_ty)),
                }
            }
        }
        Term::Ctor(tname, cname, args) => {
            let ind = sig.inds.get(tname).cloned();
            let n_tparams = ind.as_ref().map_or(0, |i| i.iparams.len());
            let mut eargs = Vec::with_capacity(args.len().saturating_sub(n_tparams));
            let mut arg_tys = Vec::with_capacity(args.len().saturating_sub(n_tparams));
            for a in &args[n_tparams.min(args.len())..] {
                let (ea, aty) = erase_infer(sig, ctx, hint, a, bytecodes, bc_counter)?;
                if !matches!(ea, MlExpr::Erased) {
                    eargs.push(ea);
                }
                arg_tys.push(aty);
            }
            let rty = if let Some(ind) = ind {
                if n_tparams == 0 {
                    Rc::new(Value::Ind(*tname, Vec::new()))
                } else {
                    let mut targs = Vec::with_capacity(n_tparams);
                    for (i, a) in args.iter().take(n_tparams).enumerate() {
                        if !matches!(a, Term::Hole) {
                            targs.push(nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?);
                        } else if let Some(ctor) = ind.ictors.iter().find(|c| c.cname == *cname) {
                            let param_sym = ind.iparams[i].0;
                            let found =
                                ctor.cparams
                                    .iter()
                                    .zip(&arg_tys)
                                    .find_map(|((_, fty), aty)| {
                                        matches!(fty, Term::Var(px) if *px == param_sym)
                                            .then(|| aty.clone())
                                    });
                            targs.push(found.unwrap_or_else(|| sig.val_unit.clone()));
                        } else {
                            targs.push(sig.val_unit.clone());
                        }
                    }
                    Rc::new(Value::Ind(*tname, targs))
                }
            } else {
                Rc::new(Value::Ind(*tname, Vec::new()))
            };
            if is_erased_type(sig, ctx.lvl, &rty) {
                Ok((MlExpr::Erased, rty))
            } else if is_egg_program_type(sig, &rty) {
                let bc = compile_egg_bytecode(sig, ctx, hint, t, bytecodes, bc_counter)?;
                Ok((bc, rty))
            } else {
                Ok((MlExpr::Ctor(*tname, *cname, eargs), rty))
            }
        }
        Term::Match(scrut, branches) => {
            let (es, sty) = erase_infer(sig, ctx, hint, scrut, bytecodes, bc_counter)?;
            let mut ebs = Vec::with_capacity(branches.len());
            let mut rty = sig.val_unit.clone();
            for (idx, (p, rhs)) in branches.iter().enumerate() {
                let mark = ctx.mark();
                check::bind_pattern_in_place(sig, ctx, &sty, p)?;
                let res = if idx == 0 {
                    let (er, ty) = erase_infer(sig, ctx, hint, rhs, bytecodes, bc_counter)?;
                    rty = ty;
                    Ok(er)
                } else {
                    erase_check(sig, ctx, hint, rhs, &rty, bytecodes, bc_counter)
                };
                ctx.restore(mark);
                ebs.push((p.clone(), res?));
            }
            if is_erased_type(sig, ctx.lvl, &rty) {
                Ok((MlExpr::Erased, rty))
            } else {
                Ok((MlExpr::Match(Box::new(es), ebs), rty))
            }
        }
        Term::Fix(f, ty_ann, body) => {
            let vty = nbe::eval(&sig.values, &sig.interner, &ctx.env, ty_ann)?;
            if is_erased_type(sig, ctx.lvl, &vty) {
                return Ok((MlExpr::Erased, vty));
            }
            let mark = ctx.mark();
            ctx.push_var(*f, vty.clone());
            let eb = erase_check(sig, ctx, hint, body, &vty, bytecodes, bc_counter);
            ctx.restore(mark);
            Ok((MlExpr::Fix(*f, Box::new(eb?)), vty))
        }
        Term::Prim(op, args) => {
            let (_, _, ret) = sig.prim_arg_ty(*op);
            let mut eargs = Vec::with_capacity(args.len());
            for a in args {
                eargs.push(erase_infer(sig, ctx, hint, a, bytecodes, bc_counter)?.0);
            }
            Ok((MlExpr::Prim(*op, eargs), ret))
        }
        Term::Rec(fs) => {
            let mut efs = Vec::with_capacity(fs.len());
            let mut rfs = Vec::with_capacity(fs.len());
            for (k, v) in fs {
                let k_name = sig.interner.resolve(*k).to_owned();
                let sub_hint = format!("{hint}_{k_name}");
                let (ev, tv) = erase_infer(sig, ctx, &sub_hint, v, bytecodes, bc_counter)?;
                efs.push((*k, ev));
                rfs.push((*k, tv));
            }
            Ok((MlExpr::Rec(efs), Rc::new(Value::RecTy(rfs))))
        }
        Term::Proj(e, f) => {
            let (ee, te) = erase_infer(sig, ctx, hint, e, bytecodes, bc_counter)?;
            let fty = match te.as_ref() {
                Value::RecTy(fs) => fs
                    .iter()
                    .find(|(k, _)| k == f)
                    .map(|(_, ty)| ty.clone())
                    .unwrap_or_else(|| sig.val_unit.clone()),
                _ => sig.val_unit.clone(),
            };
            if is_erased_type(sig, ctx.lvl, &fty) {
                Ok((MlExpr::Erased, fty))
            } else {
                Ok((MlExpr::Proj(Box::new(ee), *f), fty))
            }
        }
        Term::Opaque(n, ty) => {
            let vty = nbe::eval(&sig.values, &sig.interner, &ctx.env, ty)?;
            if is_erased_type(sig, ctx.lvl, &vty) {
                Ok((MlExpr::Erased, vty))
            } else {
                Ok((MlExpr::Global(*n), vty))
            }
        }
    }
}

// ── Program / Catalog Bundle Extraction ──────────────────────────────────────

fn matches_module_prefix(name: &str, module_prefix: Option<&str>) -> bool {
    let Some(prefix) = module_prefix else {
        return true;
    };
    if prefix.is_empty() {
        return !name.contains("::");
    }
    name == prefix
        || name
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix("::"))
            .is_some_and(|rest| !rest.contains("::"))
}

fn matches_subtree(name: &str, module_prefix: Option<&str>) -> bool {
    let Some(prefix) = module_prefix else {
        return true;
    };
    if prefix.is_empty() {
        return !name.starts_with("std::");
    }
    name == prefix
        || name
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with("::"))
}

fn rel_name_from_prefix(full: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(p) if !p.is_empty() => {
            // Strip `p::` or any ancestor module prefix of `p` (e.g. `eqsat::Scalar` under `eqsat::masks`)
            if let Some(rest) = full.strip_prefix(p).and_then(|r| r.strip_prefix("::")) {
                return rest.to_owned();
            }
            let mut anc = p;
            while let Some((parent, _)) = anc.rsplit_once("::") {
                if let Some(rest) = full.strip_prefix(parent).and_then(|r| r.strip_prefix("::"))
                    && !rest.contains("::")
                {
                    return rest.to_owned();
                }
                anc = parent;
            }
            full.to_owned()
        }
        _ => full.to_owned(),
    }
}

fn enum_rust_and_egg_name(full: &str, prefix: Option<&str>) -> (String, String) {
    let rel = rel_name_from_prefix(full, prefix);
    let rust = rel.replace("::", "_");
    let egg = rel;
    (rust, egg)
}

fn value_to_codec(
    sig: &Signature,
    ty: &Value,
    prefix: Option<&str>,
) -> Result<(MlCodec, String), Error> {
    match ty {
        Value::Ind(sym, _) => {
            if *sym == sig.sym_int {
                return Ok((MlCodec::Int, "i64".to_owned()));
            }
            if *sym == sig.sym_bool {
                return Ok((MlCodec::Bool, "bool".to_owned()));
            }
            if *sym == sig.sym_str {
                return Ok((MlCodec::Str, "String".to_owned()));
            }
            let full = sig.interner.resolve(*sym);
            let leaf = leaf_name(full);
            match leaf {
                "BigInt" => return Ok((MlCodec::BigInt, "BigInt".to_owned())),
                "OperationId" => return Ok((MlCodec::OpId, "i64".to_owned())),
                "ValueId" => return Ok((MlCodec::ValueId, "i64".to_owned())),
                "ValueRef" | "IntegerRef" => return Ok((MlCodec::ValueRef, "i64".to_owned())),
                "OperationKind" | "BinaryIntegerOperation" => {
                    return Ok((MlCodec::Kind, "NativeIdentity".to_owned()));
                }
                "RegionId" | "BlockId" | "TypeId" | "AttrHandle" | "Nat" => {
                    return Ok((MlCodec::Int, "i64".to_owned()));
                }
                _ => {}
            }
            if let Some((egg_s, _)) = egg_tag(sig, *sym) {
                let codec = match egg_s {
                    "i64" => MlCodec::Int,
                    "bool" => MlCodec::Bool,
                    "String" => MlCodec::Str,
                    "BigInt" => MlCodec::BigInt,
                    "NativeIdentity" => MlCodec::Kind,
                    _ => {
                        let (rn, en) = enum_rust_and_egg_name(full, prefix);
                        MlCodec::Enum {
                            rust_name: rn,
                            egg_sort: en,
                        }
                    }
                };
                return Ok((codec, egg_s.to_owned()));
            }
            if sig.inds.contains_key(sym) {
                let (rn, en) = enum_rust_and_egg_name(full, prefix);
                return Ok((
                    MlCodec::Enum {
                        rust_name: rn,
                        egg_sort: en.clone(),
                    },
                    en,
                ));
            }
            Err(Error(format!("unsupported table column type `{full}`")))
        }
        _ => Err(Error(
            "unsupported non-inductive column type in program schema".to_owned(),
        )),
    }
}

fn value_to_ml_val_ty(
    sig: &Signature,
    ty: &Value,
    prefix: Option<&str>,
) -> Result<MlValueTy, Error> {
    match ty {
        Value::Ind(sym, _) if *sym == sig.sym_unit => Ok(MlValueTy::Unit),
        Value::RecTy(fs) => {
            let mut items = Vec::with_capacity(fs.len());
            for (_, fty) in fs {
                items.push(value_to_ml_val_ty(sig, fty, prefix)?);
            }
            Ok(MlValueTy::Tuple(items))
        }
        other => Ok(MlValueTy::Scalar(value_to_codec(sig, other, prefix)?.0)),
    }
}

fn decode_cols_from_value(
    sig: &Signature,
    v: &Value,
    prefix: Option<&str>,
) -> Result<Vec<MlCol>, Error> {
    match v {
        Value::RecTy(fields) => {
            let mut cols = Vec::with_capacity(fields.len());
            for (k, fty) in fields {
                let kstr = sig.interner.resolve(*k);
                let col_name =
                    if kstr.starts_with('_') && kstr[1..].chars().all(|c| c.is_ascii_digit()) {
                        None
                    } else {
                        Some(kstr.to_owned())
                    };
                let (codec, egg_sort) = value_to_codec(sig, fty, prefix)?;
                cols.push(MlCol {
                    name: col_name,
                    codec,
                    egg_sort,
                });
            }
            Ok(cols)
        }
        Value::Ind(sym, _) if *sym == sig.sym_unit => Ok(Vec::new()),
        other => {
            let (codec, egg_sort) = value_to_codec(sig, other, prefix)?;
            Ok(vec![MlCol {
                name: None,
                codec,
                egg_sort,
            }])
        }
    }
}

fn merge_prim_name(sig: &Signature, sym: Sym) -> String {
    egg_tag(sig, sym)
        .map(|(t, _)| t.to_owned())
        .unwrap_or_else(|| leaf_name(sig.interner.resolve(sym)).to_owned())
}

fn is_merge_var(v: &Value, lvl: u32) -> bool {
    matches!(v, Value::Neutral(Neutral::Var(_, l)) if *l == lvl)
}

fn decode_merge_fn(sig: &Signature, f: &Value) -> Result<MergeKind, Error> {
    match f {
        Value::Neutral(Neutral::Global(sym)) => Ok(MergeKind::Prim(merge_prim_name(sig, *sym))),
        Value::Lam(p0, c0) => {
            let v0 = Rc::new(Value::Neutral(Neutral::Var(*p0, 0)));
            let step = nbe::apply_closure(&sig.values, &sig.interner, c0, v0)?;
            let Value::Lam(p1, c1) = step.as_ref() else {
                return Err(Error(
                    "Merge::With expects a binary merge function (V, V) -> V".into(),
                ));
            };
            let v1 = Rc::new(Value::Neutral(Neutral::Var(*p1, 1)));
            let body = nbe::apply_closure(&sig.values, &sig.interner, c1, v1)?;
            match body.as_ref() {
                Value::Neutral(Neutral::Var(_, 0)) => Ok(MergeKind::Old),
                Value::Neutral(Neutral::Var(_, 1)) => Ok(MergeKind::New),
                Value::Neutral(Neutral::App(inner, a1))
                    if is_merge_var(a1, 1)
                        && matches!(
                            inner.as_ref(),
                            Neutral::App(head, a0)
                                if is_merge_var(a0, 0)
                                    && matches!(head.as_ref(), Neutral::Global(_))
                        ) =>
                {
                    let Neutral::App(head, _) = inner.as_ref() else {
                        unreachable!();
                    };
                    let Neutral::Global(sym) = head.as_ref() else {
                        unreachable!();
                    };
                    Ok(MergeKind::Prim(merge_prim_name(sig, *sym)))
                }
                Value::Neutral(Neutral::Prim(op, args))
                    if args.len() == 2
                        && is_merge_var(&args[0], 0)
                        && is_merge_var(&args[1], 1) =>
                {
                    let prim = match op {
                        PrimOp::Add => "+",
                        PrimOp::Sub => "-",
                        PrimOp::Mul => "*",
                        PrimOp::Div => "/",
                        PrimOp::Rem => "%",
                        PrimOp::And => "and",
                        PrimOp::Or => "or",
                        _ => {
                            return Err(Error(format!(
                                "Merge::With primitive {op:?} is not a binary egglog merge op"
                            )));
                        }
                    };
                    Ok(MergeKind::Prim(prim.to_owned()))
                }
                _ => Err(Error(
                    "Merge::With expects a binary extern/primitive function or `|a, b| op(a, b)`"
                        .into(),
                )),
            }
        }
        _ => Err(Error(
            "Merge::With expects a binary extern/primitive function or lambda".into(),
        )),
    }
}

fn decode_merge_value(sig: &Signature, v: &Value) -> Result<MergeKind, Error> {
    match v {
        Value::Ctor(tname, cname, args) => {
            let tag = ctor_tag(sig, *tname, *cname)
                .map(str::to_owned)
                .unwrap_or_else(|| sig.interner.resolve(*cname).to_ascii_lowercase());
            if tag == "custom" {
                let func = args
                    .last()
                    .ok_or_else(|| Error("Merge::With missing merge function argument".into()))?;
                decode_merge_fn(sig, func)
            } else {
                Ok(decode_merge_str(&tag))
            }
        }
        _ => Ok(MergeKind::AssertEq),
    }
}

fn try_decode_table_axiom(
    sig: &Signature,
    name: Sym,
    vty: &Value,
    prefix: Option<&str>,
    exported: bool,
) -> Result<Option<MlTable>, Error> {
    let Value::Ind(tcon, args) = vty else {
        return Ok(None);
    };
    let full = sig.interner.resolve(name);
    let engine_name = leaf_name(full).to_owned();
    let rust_name = to_snake(&engine_name);
    match egg_tag(sig, *tcon).map(|(t, _)| t) {
        Some("@relation") if args.len() == 1 => {
            let cols = decode_cols_from_value(sig, &args[0], prefix)?;
            Ok(Some(MlTable {
                qname: name,
                rust_name,
                engine_name,
                kind: TableKind::Relation,
                cols,
                merge: MergeKind::None,
                exported,
            }))
        }
        Some("@table") if args.len() == 3 => {
            let mut cols = decode_cols_from_value(sig, &args[0], prefix)?;
            let (val_codec, val_sort) = value_to_codec(sig, &args[1], prefix)?;
            cols.push(MlCol {
                name: None,
                codec: val_codec,
                egg_sort: val_sort,
            });
            let merge = decode_merge_value(sig, &args[2])?;
            Ok(Some(MlTable {
                qname: name,
                rust_name,
                engine_name,
                kind: TableKind::Function,
                cols,
                merge,
                exported,
            }))
        }
        _ => Ok(None),
    }
}

fn try_decode_extern_axiom(
    sig: &Signature,
    name: Sym,
    vty: &Rc<Value>,
    prefix: Option<&str>,
    exported: bool,
) -> Result<Option<MlExtern>, Error> {
    if !matches!(vty.as_ref(), Value::Pi(_, _, _)) {
        return Ok(None);
    }
    let full = sig.interner.resolve(name);
    // Skip std::core::memo, built-in egglog primitives, and Rust-mapped axioms
    if rust_tag(sig, name).is_some() || egg_tag(sig, name).is_some() {
        return Ok(None);
    }
    let mut inputs = Vec::new();
    let mut cur = vty.clone();
    while let Value::Pi(x, dom, cod) = cur.as_ref() {
        if matches!(dom.as_ref(), Value::Sort(_)) {
            return Ok(None);
        }
        let Ok(c) = value_to_codec(sig, dom, prefix) else {
            return Ok(None);
        };
        inputs.push(c);
        let dummy = Rc::new(Value::Neutral(Neutral::Global(*x)));
        let Ok(next) = nbe::apply_closure(&sig.values, &sig.interner, cod, dummy) else {
            return Ok(None);
        };
        cur = next;
    }
    let Ok(output) = value_to_codec(sig, &cur, prefix) else {
        return Ok(None);
    };
    let engine_name = leaf_name(full).to_owned();
    let rel = rel_name_from_prefix(full, prefix);
    let rust_name = to_snake(&rel);
    Ok(Some(MlExtern {
        qname: name,
        rust_name,
        engine_name,
        inputs,
        output,
        exported,
    }))
}

fn collect_refs_in_atom(
    a: &IrAtom,
    tables_used: &mut BTreeSet<String>,
    calls_used: &mut BTreeSet<String>,
    decls_used: &mut BTreeSet<String>,
) {
    if let IrAtom::Call(f, args) = a {
        if f.starts_with("@cdsl:decl:") {
            decls_used.insert(f.clone());
        } else {
            calls_used.insert(f.clone());
            tables_used.insert(f.clone());
        }
        for arg in args {
            collect_refs_in_atom(arg, tables_used, calls_used, decls_used);
        }
    }
}

fn collect_refs_in_rule(
    r: &IrRule,
    tables_used: &mut BTreeSet<String>,
    calls_used: &mut BTreeSet<String>,
    decls_used: &mut BTreeSet<String>,
    written_tables: &mut BTreeSet<String>,
    read_tables: &mut BTreeSet<String>,
) {
    for f in &r.facts {
        match f {
            IrFact::Row(rel, xs) | IrFact::NotRow(rel, xs) => {
                tables_used.insert(rel.clone());
                read_tables.insert(rel.clone());
                for a in xs {
                    collect_refs_in_atom(a, tables_used, calls_used, decls_used);
                }
            }
            IrFact::Eq(a, b)
            | IrFact::Neq(a, b)
            | IrFact::Lt(a, b)
            | IrFact::Le(a, b)
            | IrFact::Gt(a, b)
            | IrFact::Ge(a, b) => {
                collect_refs_in_atom(a, tables_used, calls_used, decls_used);
                collect_refs_in_atom(b, tables_used, calls_used, decls_used);
            }
        }
    }
    for act in &r.actions {
        match act {
            IrAction::Insert(rel, xs) | IrAction::Delete(rel, xs) | IrAction::Subsume(rel, xs) => {
                tables_used.insert(rel.clone());
                written_tables.insert(rel.clone());
                for a in xs {
                    collect_refs_in_atom(a, tables_used, calls_used, decls_used);
                }
            }
            IrAction::Union(a, b) => {
                collect_refs_in_atom(a, tables_used, calls_used, decls_used);
                collect_refs_in_atom(b, tables_used, calls_used, decls_used);
            }
            IrAction::Call(f, xs) => {
                calls_used.insert(f.clone());
                for a in xs {
                    collect_refs_in_atom(a, tables_used, calls_used, decls_used);
                }
            }
            IrAction::Panic(_) => {}
        }
    }
}

fn build_eq_enum(
    sig: &Signature,
    ind: &IndDecl,
    prefix: Option<&str>,
) -> Result<Option<MlEqEnum>, Error> {
    if ind.isort != Sort::Type || !ind.iparams.is_empty() {
        return Ok(None);
    }
    if sig.extracts.contains_key(&(ExtractTarget::Rust, ind.iname))
        || sig
            .extracts
            .contains_key(&(ExtractTarget::Egglog, ind.iname))
    {
        return Ok(None);
    }
    let full = sig.interner.resolve(ind.iname);
    if full.starts_with("std::") {
        return Ok(None);
    }
    let (rust_name, egg_sort) = enum_rust_and_egg_name(full, prefix);
    let mut variants = Vec::with_capacity(ind.ictors.len());
    for c in &ind.ictors {
        let cname = sig.interner.resolve(c.cname).to_owned();
        let egg_ctor = format!("{egg_sort}::{cname}");
        let mut fields = Vec::with_capacity(c.cparams.len());
        let mut egg_sorts = Vec::with_capacity(c.cparams.len());
        for (_, fty) in &c.cparams {
            let fv = nbe::eval(&sig.values, &sig.interner, &None, fty)?;
            let (codec, sort_s) = value_to_codec(sig, &fv, prefix)?;
            fields.push(codec);
            egg_sorts.push(sort_s);
        }
        variants.push(MlEqVariant {
            name: cname,
            egg_ctor,
            fields,
            egg_sorts,
        });
    }
    Ok(Some(MlEqEnum {
        qname: ind.iname,
        rust_name,
        egg_sort,
        variants,
    }))
}

#[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
fn build_program_bundle(
    sig: &mut Signature,
    module_prefix: Option<&str>,
    catalog_only: bool,
) -> Result<MlProgramBundle, Error> {
    let decl_order = sig.decl_order.clone();

    // Determine whether `module_prefix` has direct items or is a container module
    // (like `analysis` which contains `cfg` and `destructure`).
    let has_direct_programs = decl_order.iter().any(|d| match d {
        Decl::Def(name, Some(_), body) => {
            if !matches_module_prefix(sig.interner.resolve(*name), module_prefix) {
                return false;
            }
            if let Term::App(f, _) = body
                && let Term::Global(g) = f.as_ref()
                && rust_tag(sig, *g) == Some("@memo")
            {
                return true;
            }
            if let Some(vty) = sig.types.get(name) {
                let inner_ty = unwrap_unit_fn_ty(sig, vty);
                return is_schedule_type(sig, &inner_ty)
                    || is_rewrite_list_type(sig, &inner_ty)
                    || is_rule_list_type(sig, &inner_ty);
            }
            false
        }
        Decl::Axiom(name, _) => {
            matches_module_prefix(sig.interner.resolve(*name), module_prefix)
                && sig
                    .types
                    .get(name)
                    .is_some_and(|vty| matches!(vty.as_ref(), Value::Ind(tc, _) if matches!(egg_tag(sig, *tc), Some(("@relation" | "@table", _)))))
        }
        _ => false,
    });

    let is_selected = |full: &str| -> bool {
        if has_direct_programs {
            matches_module_prefix(full, module_prefix)
        } else {
            matches_subtree(full, module_prefix)
        }
    };

    // Index ALL tables, externs, enums, and op_defs across the entire program so
    // transitive references from rules (e.g. `ir::Operation`, `analysis::cfg::*`,
    // `dialects::builtin::IntegerPredicate`, `dialects::builtin::addi`) resolve cleanly!
    let mut all_tables_by_engine: BTreeMap<String, MlTable> = BTreeMap::new();
    let mut all_externs_by_engine: BTreeMap<String, MlExtern> = BTreeMap::new();
    let mut all_enums_by_sort: BTreeMap<String, MlEqEnum> = BTreeMap::new();
    let mut all_ops_by_symbol: BTreeMap<String, MlLinkedOp> = BTreeMap::new();

    for d in &decl_order {
        match d {
            Decl::Ind(ind) => {
                if let Some(en) = build_eq_enum(sig, ind, module_prefix)? {
                    all_enums_by_sort.insert(en.egg_sort.clone(), en);
                }
            }
            Decl::Axiom(name, _) => {
                let full = sig.interner.resolve(*name).to_owned();
                let exported = is_selected(&full);
                let Some(vty) = sig.types.get(name).cloned() else {
                    continue;
                };
                if let Some(tbl) =
                    try_decode_table_axiom(sig, *name, &vty, module_prefix, exported)?
                {
                    let eng = tbl.engine_name.clone();
                    // Prefer selected/exported table definition if two modules declare the same engine name.
                    if exported || !all_tables_by_engine.contains_key(&eng) {
                        all_tables_by_engine.insert(eng, tbl);
                    }
                } else if let Some(ext) =
                    try_decode_extern_axiom(sig, *name, &vty, module_prefix, exported)?
                {
                    let eng = ext.engine_name.clone();
                    if exported || !all_externs_by_engine.contains_key(&eng) {
                        all_externs_by_engine.insert(eng, ext);
                    }
                }
            }
            Decl::Def(name, Some(_), body) => {
                if let Some(vty) = sig.types.get(name).cloned()
                    && is_op_def_result_type(sig, &vty)
                    && let Some(op) = decode_op_def(sig, *name, body)?
                {
                    let sym = format!("@cdsl:decl:{}.{}", op.op_dialect, op.op_name);
                    let has_var = op
                        .operands
                        .iter()
                        .chain(&op.results)
                        .any(|p| p.is_state || !matches!(p.arity, PortArity::Fixed));
                    let ports = if has_var {
                        None
                    } else {
                        Some((op.operands.len(), op.results.len()))
                    };
                    all_ops_by_symbol.insert(
                        sym.clone(),
                        MlLinkedOp {
                            dialect: op.op_dialect,
                            name: op.op_name,
                            symbol: sym,
                            ports,
                        },
                    );
                }
            }
            _ => {}
        }
    }

    // Collect selected schedules, rule collections, memos, and CDSL pass driver functions.
    let mut schedules: Vec<MlScheduleDecl> = Vec::new();
    let mut collections: Vec<MlRuleCollection> = Vec::new();
    let mut memos: Vec<MlProgramMemo> = Vec::new();
    let mut fns: Vec<MlDriverFn> = Vec::new();
    let mut all_rulesets: Vec<IrRuleset> = Vec::new();
    let mut tables_used: BTreeSet<String> = BTreeSet::new();
    let mut rule_tables_used: BTreeSet<String> = BTreeSet::new();
    let mut calls_used: BTreeSet<String> = BTreeSet::new();
    let mut decls_used: BTreeSet<String> = BTreeSet::new();
    let mut memo_local_tables: BTreeSet<String> = BTreeSet::new();

    // Seed with every table, enum, and extern declared directly in the selected module.
    for (eng, tbl) in &all_tables_by_engine {
        if tbl.exported {
            tables_used.insert(eng.clone());
        }
    }
    for (eng, ext) in &all_externs_by_engine {
        if ext.exported {
            calls_used.insert(eng.clone());
        }
    }

    if !catalog_only {
        for d in &decl_order {
            let Decl::Def(name, Some(_), body) = d else {
                continue;
            };
            let full = sig.interner.resolve(*name).to_owned();
            if !is_selected(&full) {
                continue;
            }
            if rust_tag(sig, *name) == Some("@ct") {
                continue;
            }
            let Some(vty) = sig.types.get(name).cloned() else {
                continue;
            };
            if is_erased_type(sig, 0, &vty) || is_op_def_result_type(sig, &vty) {
                continue;
            }

            let rel = rel_name_from_prefix(&full, module_prefix);
            let rust_name = to_snake(&rel);

            // 1. Check `@memo` definition
            if let Term::App(f, a) = body
                && let Term::Global(mfn) = f.as_ref()
                && rust_tag(sig, *mfn) == Some("@memo")
            {
                let memo = decode_program_memo(
                    sig,
                    *name,
                    &rust_name,
                    a,
                    &vty,
                    module_prefix,
                    &all_tables_by_engine,
                    &all_externs_by_engine,
                    &all_enums_by_sort,
                    &mut tables_used,
                    &mut calls_used,
                    &mut decls_used,
                    &mut memo_local_tables,
                )?;
                memos.push(memo);
                continue;
            }

            // 2. Check `Schedule` or `() -> Schedule`
            let (is_paramless, inner_ty, eval_body) = peel_unit_fn(sig, &vty, body);
            if is_schedule_type(sig, &inner_ty) {
                let norm = nbe::normalize(&sig.values, &mut sig.interner, &eval_body)?;
                let (rss, sched) = decode_schedule(sig, &rust_name, &norm)?;
                let mut dummy_w = BTreeSet::new();
                let mut dummy_r = BTreeSet::new();
                for rs in &rss {
                    for r in &rs.rules {
                        collect_refs_in_rule(
                            r,
                            &mut tables_used,
                            &mut calls_used,
                            &mut decls_used,
                            &mut dummy_w,
                            &mut dummy_r,
                        );
                    }
                }
                all_rulesets.extend(rss);
                schedules.push(MlScheduleDecl {
                    qname: *name,
                    rust_name,
                    schedule: sched,
                    is_paramless,
                });
                continue;
            }

            // 3. Check `List<Rewrite>` / `() -> List<Rewrite>` (auto-lower via `std::ir::rewrites::lower_tir_rewrites`)
            if is_rewrite_list_type(sig, &inner_ty) {
                let lower_sym = sig
                    .interner
                    .lookup("std::ir::rewrites::lower_tir_rewrites")
                    .ok_or_else(|| Error("missing std::ir::rewrites::lower_tir_rewrites".into()))?;
                let call = Term::App(
                    Box::new(Term::App(
                        Box::new(Term::Global(lower_sym)),
                        Box::new(Term::Lit(Lit::Str(Rc::from(rust_name.as_str())))),
                    )),
                    Box::new(eval_body),
                );
                let norm = nbe::normalize(&sig.values, &mut sig.interner, &call)?;
                let rule_terms: Vec<Term> = decode_list(sig, &norm)?.into_iter().cloned().collect();
                let mut rules = Vec::with_capacity(rule_terms.len());
                let mut dummy_w = BTreeSet::new();
                let mut dummy_r = BTreeSet::new();
                for rt in &rule_terms {
                    let r = decode_rule(sig, 0, rt)?;
                    collect_refs_in_rule(
                        &r,
                        &mut tables_used,
                        &mut calls_used,
                        &mut decls_used,
                        &mut dummy_w,
                        &mut dummy_r,
                    );
                    rules.push(r);
                }
                let rs_name = format!("collection_{rust_name}");
                all_rulesets.push(IrRuleset {
                    name: rs_name.clone(),
                    rules,
                    members: Vec::new(),
                    once: false,
                    colored: false,
                });
                collections.push(MlRuleCollection {
                    qname: *name,
                    rust_name,
                    ruleset_name: rs_name,
                });
                continue;
            }

            // 4. Check `List<Rule>` / `() -> List<Rule>` (if explicitly marked as collection or exported)
            if is_rule_list_type(sig, &inner_ty) {
                let norm = nbe::normalize(&sig.values, &mut sig.interner, &eval_body)?;
                let rule_terms: Vec<Term> = decode_list(sig, &norm)?.into_iter().cloned().collect();
                let mut rules = Vec::with_capacity(rule_terms.len());
                let mut dummy_w = BTreeSet::new();
                let mut dummy_r = BTreeSet::new();
                for rt in &rule_terms {
                    let r = decode_rule(sig, 0, rt)?;
                    collect_refs_in_rule(
                        &r,
                        &mut tables_used,
                        &mut calls_used,
                        &mut decls_used,
                        &mut dummy_w,
                        &mut dummy_r,
                    );
                    rules.push(r);
                }
                let rs_name = format!("collection_{rust_name}");
                all_rulesets.push(IrRuleset {
                    name: rs_name.clone(),
                    rules,
                    members: Vec::new(),
                    once: false,
                    colored: false,
                });
                collections.push(MlRuleCollection {
                    qname: *name,
                    rust_name,
                    ruleset_name: rs_name,
                });
                continue;
            }

            // 5. Check CDSL runtime pass driver function (`pub let run_* = |session: Session, ...| { ... }`)
            if matches!(vty.as_ref(), Value::Pi(_, _, _)) && !has_ct_type(sig, &vty) {
                let mut sink = DriverSink {
                    rulesets: &mut all_rulesets,
                    tables_used: &mut rule_tables_used,
                    calls_used: &mut calls_used,
                    decls_used: &mut decls_used,
                    stage_counter: 0,
                };
                let mut ctx = Ctx::empty();
                let em = erase_driver_check(sig, &mut ctx, &rust_name, body, &vty, &mut sink)?;
                if !matches!(em, MlExpr::Erased) {
                    fns.push(MlDriverFn {
                        qname: *name,
                        rust_name,
                        body: em,
                    });
                }
            }
        }
    }
    tables_used.extend(rule_tables_used.iter().cloned());

    // Collect needed enums (selected module enums + any enum referenced in tables/externs/rules).
    let mut enums_needed: BTreeSet<String> = BTreeSet::new();
    for (sort, en) in &all_enums_by_sort {
        let full = sig.interner.resolve(en.qname);
        if is_selected(full) {
            enums_needed.insert(sort.clone());
        }
    }
    for call in &calls_used {
        if let Some((sort, _)) = call.rsplit_once("::")
            && all_enums_by_sort.contains_key(sort)
        {
            enums_needed.insert(sort.to_owned());
        }
    }

    // Build final tables list (excluding pure memo-local tables unless read by an outer ruleset)
    let mut tables: Vec<MlTable> = Vec::new();
    let mut code_only_tables: Vec<MlTable> = Vec::new();
    for (eng, tbl) in &all_tables_by_engine {
        if (tbl.exported || tables_used.contains(eng)) && !memo_local_tables.contains(eng) {
            for c in &tbl.cols {
                if let MlCodec::Enum { egg_sort, .. } = &c.codec {
                    enums_needed.insert(egg_sort.clone());
                }
            }
            if let MergeKind::Prim(p) = &tbl.merge {
                calls_used.insert(p.clone());
            }
            tables.push(tbl.clone());
        } else if memo_local_tables.contains(eng) && rule_tables_used.contains(eng) {
            for c in &tbl.cols {
                if let MlCodec::Enum { egg_sort, .. } = &c.codec {
                    enums_needed.insert(egg_sort.clone());
                }
            }
            if let MergeKind::Prim(p) = &tbl.merge {
                calls_used.insert(p.clone());
            }
            code_only_tables.push(tbl.clone());
        }
    }

    // Transitively close enum field types
    let mut changed = true;
    while changed {
        changed = false;
        let snapshot: Vec<String> = enums_needed.iter().cloned().collect();
        for s in snapshot {
            if let Some(en) = all_enums_by_sort.get(&s) {
                for v in &en.variants {
                    for fc in &v.fields {
                        if let MlCodec::Enum { egg_sort, .. } = fc
                            && enums_needed.insert(egg_sort.clone())
                        {
                            changed = true;
                        }
                    }
                }
            }
        }
    }

    let enums: Vec<MlEqEnum> = enums_needed
        .iter()
        .filter_map(|s| all_enums_by_sort.get(s).cloned())
        .collect();

    // Deduplicate rulesets by name (e.g. shared `RepeatNamed("eqsat::global", ...)`).
    let mut seen_rs = BTreeSet::new();
    all_rulesets.retain(|rs| seen_rs.insert(rs.name.clone()));

    let externs: Vec<MlExtern> = all_externs_by_engine
        .iter()
        .filter(|(eng, ext)| {
            ext.exported
                || calls_used.contains(*eng)
                || (module_prefix == Some("ir")
                    && sig.interner.resolve(ext.qname).starts_with("std::ir::"))
        })
        .map(|(_, ext)| ext.clone())
        .collect();

    let linked_ops: Vec<MlLinkedOp> = decls_used
        .iter()
        .map(|sym| {
            all_ops_by_symbol.get(sym).cloned().unwrap_or_else(|| {
                let tag = sym.strip_prefix("@cdsl:decl:").unwrap_or(sym);
                let (d, n) = tag.split_once('.').unwrap_or(("", tag));
                MlLinkedOp {
                    dialect: d.to_owned(),
                    name: n.to_owned(),
                    symbol: sym.clone(),
                    ports: None,
                }
            })
        })
        .collect();

    // Build TEGG schema (sorts, externs, tables)
    let (ir_sorts, ir_externs, ir_tables) = build_ir_schema(&enums, &externs, &tables, &linked_ops);

    let catalog_ir = IrProgram {
        sorts: ir_sorts.clone(),
        externs: ir_externs.clone(),
        tables: ir_tables.clone(),
        rulesets: Vec::new(),
        schedule: IrSchedule::Seq(Vec::new()),
    };
    let catalog_bytes = tegg::compile(&catalog_ir)?;

    let code_bytes = if !catalog_only && !all_rulesets.is_empty() {
        let mut all_code_tables = tables.clone();
        all_code_tables.extend(code_only_tables);
        let (code_sorts, code_externs, code_ir_tables) =
            build_ir_schema(&enums, &externs, &all_code_tables, &linked_ops);
        let code_ir = IrProgram {
            sorts: code_sorts,
            externs: code_externs,
            tables: code_ir_tables,
            rulesets: all_rulesets,
            schedule: IrSchedule::Seq(Vec::new()),
        };
        Some(tegg::compile(&code_ir)?)
    } else {
        None
    };

    Ok(MlProgramBundle {
        enums,
        tables,
        externs,
        linked_ops,
        schedules,
        collections,
        memos,
        fns,
        catalog_bytes,
        code_bytes,
    })
}

struct DriverSink<'a> {
    rulesets: &'a mut Vec<IrRuleset>,
    tables_used: &'a mut BTreeSet<String>,
    calls_used: &'a mut BTreeSet<String>,
    decls_used: &'a mut BTreeSet<String>,
    stage_counter: usize,
}

impl DriverSink<'_> {
    fn compile_schedule(
        &mut self,
        sig: &mut Signature,
        ctx: &Ctx,
        hint: &str,
        t: &Term,
    ) -> Result<IrSchedule, Error> {
        self.stage_counter += 1;
        let stage_name = format!("{hint}_s{}", self.stage_counter);
        let v = nbe::eval(&sig.values, &sig.interner, &ctx.env, t)?;
        let norm = nbe::quote(&sig.values, &mut sig.interner, 0, &v)?;
        let (rss, sched) = decode_schedule(sig, &stage_name, &norm)?;
        let mut dummy_w = BTreeSet::new();
        let mut dummy_r = BTreeSet::new();
        for rs in &rss {
            for r in &rs.rules {
                collect_refs_in_rule(
                    r,
                    self.tables_used,
                    self.calls_used,
                    self.decls_used,
                    &mut dummy_w,
                    &mut dummy_r,
                );
            }
        }
        self.rulesets.extend(rss);
        Ok(sched)
    }
}

fn find_def_body(sig: &Signature, target: Sym) -> Option<Term> {
    sig.decl_order.iter().find_map(|d| match d {
        Decl::Def(n, _, body) if *n == target => Some(body.clone()),
        _ => None,
    })
}

#[allow(clippy::cognitive_complexity)]
fn erase_driver_check(
    sig: &mut Signature,
    ctx: &mut Ctx,
    hint: &str,
    t: &Term,
    expected: &Rc<Value>,
    sink: &mut DriverSink<'_>,
) -> Result<MlExpr, Error> {
    if is_erased_type(sig, ctx.lvl, expected) {
        return Ok(MlExpr::Erased);
    }
    if is_schedule_type(sig, expected) && !matches!(t, Term::Var(_)) {
        let sched = sink.compile_schedule(sig, ctx, hint, t)?;
        return Ok(MlExpr::Schedule(sched));
    }
    match (t, expected.as_ref()) {
        (Term::Lam(x, ann, body), Value::Pi(_, dom, cod)) => {
            let dom_val = match ann {
                Some(a) => nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?,
                None => dom.clone(),
            };
            let dom_erased = is_erased_type(sig, ctx.lvl, &dom_val);
            let mark = ctx.mark();
            let v = ctx.push_var(*x, dom_val);
            let exp2 = nbe::apply_closure(&sig.values, &sig.interner, cod, v);
            let ebody = match exp2 {
                Ok(exp2) => erase_driver_check(sig, ctx, hint, body, &exp2, sink),
                Err(e) => Err(e),
            };
            ctx.restore(mark);
            let ebody = ebody?;
            if dom_erased {
                Ok(ebody)
            } else {
                Ok(MlExpr::Lam(*x, Box::new(ebody)))
            }
        }
        (Term::Let(x, ann, rhs, body), _) => {
            let (ty_rhs, erhs_opt) = match ann {
                Some(a) => (nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?, None),
                None => {
                    let x_name = sig.interner.resolve(*x).to_owned();
                    let sub_hint = format!("{hint}_{x_name}");
                    let (er, tr) = erase_driver_infer(sig, ctx, &sub_hint, rhs, sink)?;
                    (tr, Some(er))
                }
            };
            let is_sched = is_schedule_type(sig, &ty_rhs);
            let skip =
                is_erased_type(sig, ctx.lvl, &ty_rhs) || (has_ct_type(sig, &ty_rhs) && !is_sched);
            let vrhs = if *x == sig.sym_wild || (!is_sched && !skip) {
                sig.val_unit.clone()
            } else {
                nbe::eval(&sig.values, &sig.interner, &ctx.env, rhs)?
            };
            let erhs = if skip {
                None
            } else if let Some(er) = erhs_opt {
                Some(er)
            } else {
                let x_name = sig.interner.resolve(*x).to_owned();
                let sub_hint = format!("{hint}_{x_name}");
                Some(erase_driver_check(sig, ctx, &sub_hint, rhs, &ty_rhs, sink)?)
            };
            let mark = ctx.mark();
            ctx.push_local(*x, ty_rhs, vrhs);
            let ebody = erase_driver_check(sig, ctx, hint, body, expected, sink);
            ctx.restore(mark);
            let ebody = ebody?;
            match erhs {
                None => Ok(ebody),
                Some(er) => Ok(MlExpr::Let(*x, Box::new(er), Box::new(ebody))),
            }
        }
        _ => {
            let (e, _) = erase_driver_infer(sig, ctx, hint, t, sink)?;
            Ok(e)
        }
    }
}

#[allow(clippy::cognitive_complexity)]
fn erase_driver_infer(
    sig: &mut Signature,
    ctx: &mut Ctx,
    hint: &str,
    t: &Term,
    sink: &mut DriverSink<'_>,
) -> Result<(MlExpr, Rc<Value>), Error> {
    match t {
        Term::Var(x) => {
            let ty = ctx
                .lookup_local(*x)
                .or_else(|| sig.types.get(x).cloned())
                .ok_or_else(|| Error(format!("unbound variable: {}", sig.interner.resolve(*x))))?;
            if is_erased_type(sig, ctx.lvl, &ty) {
                Ok((MlExpr::Erased, ty))
            } else {
                Ok((MlExpr::Var(*x), ty))
            }
        }
        Term::Global(g) => {
            let ty =
                sig.types.get(g).cloned().ok_or_else(|| {
                    Error(format!("unbound global: {}", sig.interner.resolve(*g)))
                })?;
            if is_erased_type(sig, ctx.lvl, &ty) {
                Ok((MlExpr::Erased, ty))
            } else if is_schedule_type(sig, &ty) {
                let sched = sink.compile_schedule(sig, ctx, hint, t)?;
                Ok((MlExpr::Schedule(sched), ty))
            } else {
                Ok((MlExpr::Global(*g), ty))
            }
        }
        Term::Lit(l) => Ok((MlExpr::Lit(l.clone()), sig.lit_type(l))),
        Term::App(_, _) => {
            let mut head = t;
            let mut args = Vec::new();
            while let Term::App(f, a) = head {
                args.push(a.as_ref());
                head = f.as_ref();
            }
            args.reverse();

            // Higher-order compile-time pass template specialization (preserves effect order!)
            if let Term::Global(g) = head
                && let Some(gty) = sig.types.get(g).cloned()
                && has_ct_type(sig, &gty)
                && let Some(def_body) = find_def_body(sig, *g)
            {
                let mut cur_body = &def_body;
                let mut cur_ty = gty;
                let mut bindings = Vec::new();
                let mut idx = 0usize;
                while idx < args.len() {
                    let Term::Lam(param, _, inner) = cur_body else {
                        break;
                    };
                    let Value::Pi(_, dom, cod) = cur_ty.as_ref().clone() else {
                        break;
                    };
                    let a = args[idx];
                    idx += 1;
                    let is_sched = is_schedule_type(sig, &dom);
                    if is_erased_type(sig, ctx.lvl, &dom) || (has_ct_type(sig, &dom) && !is_sched) {
                        let va = nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?;
                        let next_ty =
                            nbe::apply_closure(&sig.values, &sig.interner, &cod, va.clone())?;
                        bindings.push((*param, None, dom, va));
                        cur_ty = next_ty;
                    } else {
                        let ea = erase_driver_check(sig, ctx, hint, a, &dom, sink)?;
                        let va = if is_sched {
                            nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?
                        } else {
                            Rc::new(Value::Neutral(Neutral::Global(*param)))
                        };
                        let next_ty =
                            nbe::apply_closure(&sig.values, &sig.interner, &cod, va.clone())?;
                        bindings.push((*param, Some(ea), dom, va));
                        cur_ty = next_ty;
                    }
                    cur_body = inner.as_ref();
                }
                if idx == args.len() {
                    let mark = ctx.mark();
                    for (p, _, dom, va) in &bindings {
                        ctx.push_local(*p, dom.clone(), va.clone());
                    }
                    let mut ebody = erase_driver_check(sig, ctx, hint, cur_body, &cur_ty, sink)?;
                    ctx.restore(mark);
                    for (p, ea, _, _) in bindings.into_iter().rev() {
                        if let Some(ea) = ea {
                            ebody = MlExpr::Let(p, Box::new(ea), Box::new(ebody));
                        }
                    }
                    return Ok((ebody, cur_ty));
                }
            }

            let (mut res_expr, mut res_ty) = erase_driver_infer(sig, ctx, hint, head, sink)?;
            for a in args {
                match res_ty.as_ref().clone() {
                    Value::Pi(_, dom, cod) => {
                        let is_sched = is_schedule_type(sig, &dom);
                        let va = if is_erased_type(sig, ctx.lvl, &dom)
                            || is_sched
                            || has_ct_type(sig, &dom)
                        {
                            nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?
                        } else {
                            sig.val_unit.clone()
                        };
                        let next_ty = nbe::apply_closure(&sig.values, &sig.interner, &cod, va)?;
                        if !is_erased_type(sig, ctx.lvl, &dom) {
                            let ea = erase_driver_check(sig, ctx, hint, a, &dom, sink)?;
                            res_expr = MlExpr::App(Box::new(res_expr), Box::new(ea));
                        }
                        res_ty = next_ty;
                    }
                    _ => {
                        let (ea, _) = erase_driver_infer(sig, ctx, hint, a, sink)?;
                        res_expr = MlExpr::App(Box::new(res_expr), Box::new(ea));
                    }
                }
            }
            Ok((res_expr, res_ty))
        }
        Term::Let(x, ann, rhs, body) => {
            let (ty_rhs, erhs_opt) = match ann {
                Some(a) => (nbe::eval(&sig.values, &sig.interner, &ctx.env, a)?, None),
                None => {
                    let x_name = sig.interner.resolve(*x).to_owned();
                    let sub_hint = format!("{hint}_{x_name}");
                    let (er, tr) = erase_driver_infer(sig, ctx, &sub_hint, rhs, sink)?;
                    (tr, Some(er))
                }
            };
            let is_sched = is_schedule_type(sig, &ty_rhs);
            let skip =
                is_erased_type(sig, ctx.lvl, &ty_rhs) || (has_ct_type(sig, &ty_rhs) && !is_sched);
            let vrhs = if *x == sig.sym_wild || (!is_sched && !skip) {
                sig.val_unit.clone()
            } else {
                nbe::eval(&sig.values, &sig.interner, &ctx.env, rhs)?
            };
            let erhs = if skip {
                None
            } else if let Some(er) = erhs_opt {
                Some(er)
            } else {
                let x_name = sig.interner.resolve(*x).to_owned();
                let sub_hint = format!("{hint}_{x_name}");
                Some(erase_driver_check(sig, ctx, &sub_hint, rhs, &ty_rhs, sink)?)
            };
            let mark = ctx.mark();
            ctx.push_local(*x, ty_rhs, vrhs);
            let res = erase_driver_infer(sig, ctx, hint, body, sink);
            ctx.restore(mark);
            let (ebody, tb) = res?;
            match erhs {
                None => Ok((ebody, tb)),
                Some(er) => Ok((MlExpr::Let(*x, Box::new(er), Box::new(ebody)), tb)),
            }
        }
        Term::Prim(op, args) => {
            let mut eargs = Vec::with_capacity(args.len());
            for a in args {
                let (ea, _) = erase_driver_infer(sig, ctx, hint, a, sink)?;
                eargs.push(ea);
            }
            let rty = match op {
                PrimOp::Add | PrimOp::Sub | PrimOp::Mul | PrimOp::Div | PrimOp::Rem => {
                    sig.val_int.clone()
                }
                PrimOp::StrCat => sig.val_str.clone(),
                _ => sig.val_bool.clone(),
            };
            Ok((MlExpr::Prim(*op, eargs), rty))
        }
        _ => Ok((MlExpr::Erased, sig.val_unit.clone())),
    }
}

fn build_ir_schema(
    enums: &[MlEqEnum],
    externs: &[MlExtern],
    tables: &[MlTable],
    linked_ops: &[MlLinkedOp],
) -> (Vec<IrSort>, Vec<IrExtern>, Vec<IrTable>) {
    let mut sorts = Vec::new();
    let mut seen_sorts = BTreeSet::new();
    let mut add_sort = |name: &str, class: SortClass| {
        if seen_sorts.insert(name.to_owned()) {
            sorts.push(IrSort {
                name: name.to_owned(),
                class,
            });
        }
    };
    add_sort("i64", SortClass::BaseSort);
    add_sort("bool", SortClass::BaseSort);
    add_sort("String", SortClass::BaseSort);
    add_sort("BigInt", SortClass::BaseSort);
    add_sort("NativeIdentity", SortClass::BaseSort);
    for en in enums {
        add_sort(&en.egg_sort, SortClass::EqSort);
    }

    let mut ir_externs = Vec::new();
    for ext in externs {
        ir_externs.push(IrExtern {
            name: ext.engine_name.clone(),
            inputs: ext.inputs.iter().map(|(_, s)| s.clone()).collect(),
            output: ext.output.1.clone(),
        });
    }
    for op in linked_ops {
        ir_externs.push(IrExtern {
            name: op.symbol.clone(),
            inputs: Vec::new(),
            output: "NativeIdentity".to_owned(),
        });
    }

    let mut ir_tables = Vec::new();
    for en in enums {
        for v in &en.variants {
            let mut cols = v.egg_sorts.clone();
            cols.push(en.egg_sort.clone());
            ir_tables.push(IrTable {
                name: v.egg_ctor.clone(),
                kind: TableKind::Constructor,
                cols,
                keys: None,
                merge: MergeKind::Union,
            });
        }
    }
    for tbl in tables {
        ir_tables.push(IrTable {
            name: tbl.engine_name.clone(),
            kind: tbl.kind,
            cols: tbl.cols.iter().map(|c| c.egg_sort.clone()).collect(),
            keys: None,
            merge: tbl.merge.clone(),
        });
    }
    (sorts, ir_externs, ir_tables)
}

fn unwrap_unit_fn_ty(sig: &Signature, vty: &Rc<Value>) -> Rc<Value> {
    if let Value::Pi(_, dom, cod) = vty.as_ref()
        && matches!(dom.as_ref(), Value::Ind(s, _) if *s == sig.sym_unit)
        && let Ok(ret) = nbe::apply_closure(&sig.values, &sig.interner, cod, sig.val_unit.clone())
    {
        return ret;
    }
    vty.clone()
}

fn peel_unit_fn(sig: &Signature, vty: &Rc<Value>, body: &Term) -> (bool, Rc<Value>, Term) {
    if let Value::Pi(_, dom, cod) = vty.as_ref()
        && matches!(dom.as_ref(), Value::Ind(s, _) if *s == sig.sym_unit)
        && let Ok(ret) = nbe::apply_closure(&sig.values, &sig.interner, cod, sig.val_unit.clone())
    {
        let applied = Term::App(Box::new(body.clone()), Box::new(Term::Lit(Lit::Unit)));
        return (true, ret, applied);
    }
    (false, vty.clone(), body.clone())
}

#[allow(clippy::too_many_arguments)]
fn decode_program_memo(
    sig: &mut Signature,
    name: Sym,
    rust_name: &str,
    lam_term: &Term,
    vty: &Rc<Value>,
    module_prefix: Option<&str>,
    all_tables: &BTreeMap<String, MlTable>,
    all_externs: &BTreeMap<String, MlExtern>,
    all_enums: &BTreeMap<String, MlEqEnum>,
    outer_tables_used: &mut BTreeSet<String>,
    outer_calls_used: &mut BTreeSet<String>,
    outer_decls_used: &mut BTreeSet<String>,
    memo_local_tables: &mut BTreeSet<String>,
) -> Result<MlProgramMemo, Error> {
    // Peel outer parameter lambdas: `memo |p0: T0, p1: T1| { ... }` or `memo || { ... }`
    let mut params: Vec<(Sym, MlCodec)> = Vec::new();
    let mut cur_term = lam_term;
    let mut cur_ty = vty.clone();
    let mut param_env = None;
    let mut ctx = Ctx::empty();
    let mut param_idx = 0i64;
    while let Term::Lam(px, _, inner) = cur_term {
        let Value::Pi(_, dom, cod) = cur_ty.as_ref().clone() else {
            break;
        };
        if matches!(dom.as_ref(), Value::Ind(s, _) if *s == sig.sym_unit) {
            param_env = env_extend(&param_env, *px, sig.val_unit.clone());
            cur_ty = nbe::apply_closure(&sig.values, &sig.interner, &cod, sig.val_unit.clone())?;
            cur_term = inner.as_ref();
            break;
        }
        let (codec, _) = value_to_codec(sig, &dom, module_prefix)?;
        params.push((*px, codec));
        // Bind parameter to `Atom::Arg(param_idx)` for schedule evaluation AND as runtime Var for MlExpr!
        let v_dummy = ctx.push_var(*px, dom.clone());
        let _ = v_dummy;
        let arg_val = Rc::new(Value::Lit(Lit::Int(param_idx)));
        param_env = env_extend(&param_env, *px, arg_val);
        let next_v = Rc::new(Value::Neutral(Neutral::Global(*px)));
        cur_ty = nbe::apply_closure(&sig.values, &sig.interner, &cod, next_v)?;
        param_idx += 1;
        cur_term = inner.as_ref();
    }

    // Check if the memo body is a tuple `(sched, { outputs })` or `(sched, { outputs }, |q...| ...)`
    // or `(sched, value_expr)` or pure `value_expr`!
    let mut bc_scratch = Vec::new();
    let mut bc_ctr = 0usize;

    // Evaluate with dummy int params to see if it normalizes to a tuple whose `_0` is a Schedule!
    let eval_check = nbe::eval(&sig.values, &sig.interner, &param_env, cur_term)
        .and_then(|v| nbe::quote(&sig.values, &mut sig.interner, 0, &v));

    let mut local_tables_used = BTreeSet::new();
    let mut local_calls_used = BTreeSet::new();
    let mut local_decls_used = BTreeSet::new();
    let mut written_tables = BTreeSet::new();
    let mut read_tables = BTreeSet::new();

    let (rulesets, schedule, body) = match eval_check {
        Ok(Term::Rec(ref fs)) if fs.len() >= 2 => {
            let s0 = sig.interner.intern("_0");
            let s1 = sig.interner.intern("_1");
            let s2 = sig.interner.intern("_2");
            let t0 = fs.iter().find(|(k, _)| *k == s0).map(|(_, v)| v.clone());
            let t1 = fs.iter().find(|(k, _)| *k == s1).map(|(_, v)| v.clone());
            let t2 = fs.iter().find(|(k, _)| *k == s2).map(|(_, v)| v.clone());

            let is_sched_0 = t0.as_ref().is_some_and(|t| {
                matches!(t, Term::Ctor(tn, _, _) if matches!(egg_tag(sig, *tn), Some(("schedule", _))))
            });
            if is_sched_0 {
                let (rss, sched) = decode_schedule(sig, rust_name, t0.as_ref().unwrap())?;
                for rs in &rss {
                    for r in &rs.rules {
                        collect_refs_in_rule(
                            r,
                            &mut local_tables_used,
                            &mut local_calls_used,
                            &mut local_decls_used,
                            &mut written_tables,
                            &mut read_tables,
                        );
                    }
                }
                if let (Some(Term::Rec(ofs)), Some(_)) = (&t1, &t2) {
                    // `(sched, { tbl_field: "TableName", ... }, |qparams...| expr)` -> Query memo!
                    let mut out_tbls = Vec::new();
                    for (fk, fv) in ofs {
                        let tname = decode_str(sig, fv)?;
                        let codecs = all_tables
                            .get(&tname)
                            .map(|t| t.cols.iter().map(|c| c.codec.clone()).collect())
                            .unwrap_or_default();
                        written_tables.insert(tname.clone());
                        local_tables_used.insert(tname.clone());
                        out_tbls.push((sig.interner.resolve(*fk).to_owned(), tname, codecs));
                    }
                    // Extract the query closure from the UN-NORMALIZED AST (preserving parameter variables!)
                    let q_term = extract_tuple_elem(cur_term, 2).unwrap_or(cur_term);
                    let q_ty = extract_rec_ty_field(sig, &cur_ty, "_2");
                    let mut q_params = Vec::new();
                    let mut qt = q_term;
                    let mut qty = q_ty;
                    while let Term::Lam(qx, _, qinner) = qt {
                        let Value::Pi(_, qdom, qcod) = qty.as_ref().clone() else {
                            break;
                        };
                        let (qc, _) = value_to_codec(sig, &qdom, module_prefix)?;
                        q_params.push((*qx, qc));
                        let qv = ctx.push_var(*qx, qdom);
                        qty = nbe::apply_closure(&sig.values, &sig.interner, &qcod, qv)?;
                        qt = qinner.as_ref();
                    }
                    let q_expr = erase_check(
                        sig,
                        &mut ctx,
                        rust_name,
                        qt,
                        &qty,
                        &mut bc_scratch,
                        &mut bc_ctr,
                    )?;
                    (
                        rss,
                        sched,
                        MlMemoBody::Query {
                            tables: out_tbls,
                            query_params: q_params,
                            query_expr: q_expr,
                        },
                    )
                } else if let Some(Term::Rec(ofs)) = &t1
                    && ofs.iter().all(|(_, v)| matches!(v, Term::Lit(Lit::Str(_))))
                {
                    // `(sched, { field: "TableName", ... })` -> Table-output memo!
                    let mut outputs = Vec::new();
                    for (fk, fv) in ofs {
                        let tname = decode_str(sig, fv)?;
                        let codecs = all_tables
                            .get(&tname)
                            .map(|t| t.cols.iter().map(|c| c.codec.clone()).collect())
                            .unwrap_or_default();
                        written_tables.insert(tname.clone());
                        local_tables_used.insert(tname.clone());
                        outputs.push((sig.interner.resolve(*fk).to_owned(), tname, codecs));
                    }
                    (rss, sched, MlMemoBody::Tables { outputs })
                } else {
                    // `(sched, value_expr)` -> Scalar/Tuple value memo!
                    let val_term = extract_tuple_elem(cur_term, 1).unwrap_or(cur_term);
                    let val_ty = extract_rec_ty_field(sig, &cur_ty, "_1");
                    let ml_ret = value_to_ml_val_ty(sig, &val_ty, module_prefix)?;
                    let expr = erase_check(
                        sig,
                        &mut ctx,
                        rust_name,
                        val_term,
                        &val_ty,
                        &mut bc_scratch,
                        &mut bc_ctr,
                    )?;
                    (
                        rss,
                        sched,
                        MlMemoBody::Value {
                            expr,
                            ret_ty: ml_ret,
                        },
                    )
                }
            } else {
                // Pure tuple value memo (e.g. `scoped_projection` / `scoped_pair` without schedule)
                let ml_ret = value_to_ml_val_ty(sig, &cur_ty, module_prefix)?;
                let expr = erase_check(
                    sig,
                    &mut ctx,
                    rust_name,
                    cur_term,
                    &cur_ty,
                    &mut bc_scratch,
                    &mut bc_ctr,
                )?;
                (
                    Vec::new(),
                    IrSchedule::Seq(Vec::new()),
                    MlMemoBody::Value {
                        expr,
                        ret_ty: ml_ret,
                    },
                )
            }
        }
        _ => {
            // Pure scalar value memo (no schedule)
            let ml_ret = value_to_ml_val_ty(sig, &cur_ty, module_prefix)?;
            let expr = erase_check(
                sig,
                &mut ctx,
                rust_name,
                cur_term,
                &cur_ty,
                &mut bc_scratch,
                &mut bc_ctr,
            )?;
            (
                Vec::new(),
                IrSchedule::Seq(Vec::new()),
                MlMemoBody::Value {
                    expr,
                    ret_ty: ml_ret,
                },
            )
        }
    };

    // Tables written by this memo are its `locals`; tables only read are its `reads`!
    let locals: Vec<String> = written_tables.iter().cloned().collect();
    let reads: Vec<String> = read_tables.difference(&written_tables).cloned().collect();
    for l in &locals {
        memo_local_tables.insert(l.clone());
    }
    outer_tables_used.extend(reads.iter().cloned());
    outer_calls_used.extend(local_calls_used.iter().cloned());
    outer_decls_used.extend(local_decls_used.iter().cloned());

    // Compile standalone TEGG frame bytecode for this memo!
    let memo_tables: Vec<MlTable> = local_tables_used
        .iter()
        .filter_map(|t| all_tables.get(t).cloned())
        .collect();
    let mut memo_enum_sorts = BTreeSet::new();
    for t in &memo_tables {
        for c in &t.cols {
            if let MlCodec::Enum { egg_sort, .. } = &c.codec {
                memo_enum_sorts.insert(egg_sort.clone());
            }
        }
    }
    for c in &local_calls_used {
        if let Some((s, _)) = c.rsplit_once("::")
            && all_enums.contains_key(s)
        {
            memo_enum_sorts.insert(s.to_owned());
        }
    }
    let memo_enums: Vec<MlEqEnum> = memo_enum_sorts
        .iter()
        .filter_map(|s| all_enums.get(s).cloned())
        .collect();
    let memo_externs: Vec<MlExtern> = local_calls_used
        .iter()
        .filter_map(|c| all_externs.get(c).cloned())
        .collect();
    let memo_ops: Vec<MlLinkedOp> = local_decls_used
        .iter()
        .map(|sym| {
            let tag = sym.strip_prefix("@cdsl:decl:").unwrap_or(sym);
            let (d, n) = tag.split_once('.').unwrap_or(("", tag));
            MlLinkedOp {
                dialect: d.to_owned(),
                name: n.to_owned(),
                symbol: sym.clone(),
                ports: None,
            }
        })
        .collect();

    let (ir_sorts, ir_externs, ir_tables) =
        build_ir_schema(&memo_enums, &memo_externs, &memo_tables, &memo_ops);
    let memo_ir = IrProgram {
        sorts: ir_sorts,
        externs: ir_externs,
        tables: ir_tables,
        rulesets,
        schedule: schedule.clone(),
    };
    let bytes = tegg::compile(&memo_ir)?;

    Ok(MlProgramMemo {
        qname: name,
        rust_name: rust_name.to_owned(),
        params,
        locals,
        reads,
        bytes,
        schedule,
        body,
    })
}

fn extract_tuple_elem(t: &Term, idx: usize) -> Option<&Term> {
    match t {
        Term::Rec(fs) => fs.get(idx).map(|(_, v)| v),
        Term::Let(_, _, _, body) => extract_tuple_elem(body, idx),
        _ => None,
    }
}

fn extract_rec_ty_field(sig: &Signature, vty: &Rc<Value>, field: &str) -> Rc<Value> {
    if let Value::RecTy(fs) = vty.as_ref()
        && let Some(sym) = sig.interner.lookup(field)
        && let Some((_, ft)) = fs.iter().find(|(k, _)| *k == sym)
    {
        return ft.clone();
    }
    sig.val_unit.clone()
}

// ── Top-Level Erasure Entry Points ───────────────────────────────────────────

pub fn erase_program(sig: &mut Signature) -> Result<MlProgram, Error> {
    erase_program_mode(sig, None, EraseMode::Standalone)
}

pub fn erase_program_mode(
    sig: &mut Signature,
    module_prefix: Option<&str>,
    mode: EraseMode,
) -> Result<MlProgram, Error> {
    match mode {
        EraseMode::Program | EraseMode::Catalog => {
            let bundle = build_program_bundle(sig, module_prefix, mode == EraseMode::Catalog)?;
            return Ok(MlProgram {
                decls: vec![MlDecl::ProgramBundle(Box::new(bundle))],
                bytecodes: Vec::new(),
            });
        }
        EraseMode::OpDef => return erase_opdef_mode(sig, module_prefix),
        EraseMode::Standalone => {}
    }

    let mut bytecodes: Vec<(String, Vec<u8>, String)> = Vec::new();
    let mut bc_counter = 0usize;
    let mut decls_out: Vec<MlDecl> = Vec::with_capacity(sig.decl_order.len());
    let decl_order = sig.decl_order.clone();
    let mut ctx = Ctx::empty();

    for d in decl_order {
        match d {
            Decl::Ind(ind) if ind.isort == Sort::Type => {
                if !matches_module_prefix(sig.interner.resolve(ind.iname), module_prefix) {
                    continue;
                }
                let has_rust = sig.extracts.contains_key(&(ExtractTarget::Rust, ind.iname));
                let has_egg = sig
                    .extracts
                    .contains_key(&(ExtractTarget::Egglog, ind.iname));
                if !has_rust && !has_egg {
                    decls_out.push(MlDecl::Ind(ind));
                }
            }
            Decl::Def(name, Some(_), body) => {
                if !matches_module_prefix(sig.interner.resolve(name), module_prefix) {
                    continue;
                }
                let vty = match sig.types.get(&name) {
                    Some(v) => v.clone(),
                    None => continue,
                };
                if is_erased_type(sig, 0, &vty) {
                    continue;
                }
                if is_op_def_result_type(sig, &vty) {
                    if let Some(op) = decode_op_def(sig, name, &body)? {
                        decls_out.push(MlDecl::OpDef(Box::new(op)));
                    }
                    continue;
                }

                // Standalone `@memo`: `memo(|root| (prog, { outputs }))`
                if let Term::App(f, a) = &body
                    && let Term::Global(mfn) = f.as_ref()
                    && rust_tag(sig, *mfn) == Some("@memo")
                    && let Term::Lam(param, _, mbody) = a.as_ref()
                {
                    let let_term = Term::Let(
                        *param,
                        None,
                        Box::new(Term::Lit(Lit::Int(0))),
                        Box::new((**mbody).clone()),
                    );
                    let norm = nbe::normalize(&sig.values, &mut sig.interner, &let_term)?;
                    let name_str = sig.interner.resolve(name).to_owned();
                    match norm {
                        Term::Rec(fs) => {
                            let s0 = sig.interner.intern("_0");
                            let s1 = sig.interner.intern("_1");
                            let prog_t = fs
                                .iter()
                                .find(|(k, _)| *k == s0)
                                .map(|(_, v)| v.clone())
                                .ok_or_else(|| Error(format!("@memo missing _0 in {name_str}")))?;
                            let outs_t = fs
                                .iter()
                                .find(|(k, _)| *k == s1)
                                .map(|(_, v)| v.clone())
                                .ok_or_else(|| Error(format!("@memo missing _1 in {name_str}")))?;
                            let ir = decode_egglog_program(sig, &name_str, &prog_t)?;
                            let egg_txt = tegg::pp_egg(&ir);
                            let bytes = tegg::compile(&ir)?;
                            let bc_id = sanitize_bc_id(&name_str, &mut bc_counter);
                            bytecodes.push((bc_id.clone(), bytes.clone(), egg_txt.clone()));
                            let mut outputs = Vec::new();
                            if let Term::Rec(ofs) = outs_t {
                                for (k, v) in ofs {
                                    outputs.push((k, decode_str(sig, &v)?));
                                }
                            }
                            decls_out.push(MlDecl::Memo {
                                name,
                                param: *param,
                                bc_id,
                                bytes,
                                egg: egg_txt,
                                schedule: ir.schedule,
                                outputs,
                            });
                        }
                        _ => {
                            return Err(Error(format!(
                                "@memo body must evaluate to (Program, {{ outputs }}) in {name_str}"
                            )));
                        }
                    }
                    continue;
                }

                if rust_tag(sig, name) != Some("@ct")
                    && (!has_ct_type(sig, &vty) || is_egg_program_type(sig, &vty))
                {
                    let hint = sig.interner.resolve(name).to_owned();
                    let em = erase_check(
                        sig,
                        &mut ctx,
                        &hint,
                        &body,
                        &vty,
                        &mut bytecodes,
                        &mut bc_counter,
                    )?;
                    if !matches!(em, MlExpr::Erased) {
                        decls_out.push(MlDecl::Def(name, em));
                    }
                }
            }
            _ => {}
        }
    }

    Ok(MlProgram {
        decls: decls_out,
        bytecodes,
    })
}

fn erase_opdef_mode(sig: &mut Signature, module_prefix: Option<&str>) -> Result<MlProgram, Error> {
    let decl_order = sig.decl_order.clone();
    let mut decls_out = Vec::new();

    // Check if the selected module IS `std::ir::interfaces` (`@tir_interfaces`)
    for d in &decl_order {
        if let Decl::Extract(ExtractTarget::Rust, sym, ExtractSpec::Const(tag)) = d
            && matches_module_prefix(sig.interner.resolve(*sym), module_prefix)
        {
            if tag == "@tir_interfaces" {
                return Ok(MlProgram {
                    decls: vec![MlDecl::TirInterfaces],
                    bytecodes: Vec::new(),
                });
            }
            if tag == "@reexport_interfaces" {
                decls_out.push(MlDecl::ReexportInterfaces);
            }
        }
    }

    for d in decl_order {
        match d {
            Decl::Ind(ind) if ind.isort == Sort::Type && ind.iparams.is_empty() => {
                if !matches_module_prefix(sig.interner.resolve(ind.iname), module_prefix) {
                    continue;
                }
                let has_rust = sig.extracts.contains_key(&(ExtractTarget::Rust, ind.iname));
                let has_egg = sig
                    .extracts
                    .contains_key(&(ExtractTarget::Egglog, ind.iname));
                if !has_rust && !has_egg {
                    decls_out.push(MlDecl::Ind(ind));
                }
            }
            Decl::Def(name, Some(_), body) => {
                if !matches_module_prefix(sig.interner.resolve(name), module_prefix) {
                    continue;
                }
                let Some(vty) = sig.types.get(&name).cloned() else {
                    continue;
                };
                if is_op_def_result_type(sig, &vty)
                    && let Some(op) = decode_op_def(sig, name, &body)?
                {
                    decls_out.push(MlDecl::OpDef(Box::new(op)));
                }
            }
            _ => {}
        }
    }

    Ok(MlProgram {
        decls: decls_out,
        bytecodes: Vec::new(),
    })
}
