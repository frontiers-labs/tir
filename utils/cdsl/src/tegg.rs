//! Direct TEGG v15 Binary Bytecode Compiler & Readable `.egg` Printer.
//!
//! Matches `libs/egglog/src/vm/file.rs` (`MAGIC = "TEGG"`, `VERSION = 15`) exactly,
//! so emitted bytes load directly via `tir_egglog::Program::from_bytes`.

use std::{collections::HashMap, fmt::Write};

use crate::{
    error::Error,
    tegg_core::{self, Bind, ColReq, VmNode},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortClass {
    EqSort,
    BaseSort,
    ContainerSort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TableKind {
    Relation,
    Function,
    Constructor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeKind {
    None,
    AssertEq,
    Union,
    Old,
    New,
    Min,
    Max,
    Prim(String),
}

#[derive(Clone, Debug)]
pub struct IrSort {
    pub name: String,
    pub class: SortClass,
}

#[derive(Clone, Debug)]
pub struct IrExtern {
    pub name: String,
    pub inputs: Vec<String>,
    pub output: String,
}

#[derive(Clone, Debug)]
pub struct IrTable {
    pub name: String,
    pub kind: TableKind,
    pub cols: Vec<String>,
    pub keys: Option<usize>,
    pub merge: MergeKind,
}

#[derive(Clone, Debug)]
pub enum IrAtom {
    Var(String),
    Int(i64),
    Bool(bool),
    Str(String),
    Unit,
    Arg(u32),
    Call(String, Vec<IrAtom>),
}

#[derive(Clone, Debug)]
pub enum IrFact {
    Row(String, Vec<IrAtom>),
    NotRow(String, Vec<IrAtom>),
    Eq(IrAtom, IrAtom),
    Neq(IrAtom, IrAtom),
    Lt(IrAtom, IrAtom),
    Le(IrAtom, IrAtom),
    Gt(IrAtom, IrAtom),
    Ge(IrAtom, IrAtom),
}

#[derive(Clone, Debug)]
pub enum IrAction {
    Insert(String, Vec<IrAtom>),
    Union(IrAtom, IrAtom),
    Delete(String, Vec<IrAtom>),
    Subsume(String, Vec<IrAtom>),
    Call(String, Vec<IrAtom>),
    Panic(String),
}

#[derive(Clone, Debug)]
pub struct IrRule {
    pub name: String,
    pub facts: Vec<IrFact>,
    pub actions: Vec<IrAction>,
}

#[derive(Clone, Debug)]
pub struct IrRuleset {
    pub name: String,
    pub rules: Vec<IrRule>,
    pub members: Vec<String>,
    pub once: bool,
    pub colored: bool,
}

#[derive(Clone, Debug)]
pub enum IrSchedule {
    Run(String, usize), // ruleset_name, repeat_count (0 = saturate)
    Seq(Vec<IrSchedule>),
    Repeat(usize, Box<IrSchedule>), // 0 = saturate
}

#[derive(Clone, Debug)]
pub struct IrProgram {
    pub sorts: Vec<IrSort>,
    pub externs: Vec<IrExtern>,
    pub tables: Vec<IrTable>,
    pub rulesets: Vec<IrRuleset>,
    pub schedule: IrSchedule,
}

// ── Readable `.egg` text dump for debugging ──────────────────────────────────

fn pp_atom(a: &IrAtom, out: &mut String) {
    match a {
        IrAtom::Var(v) => out.push_str(v),
        IrAtom::Int(n) => {
            let _ = write!(out, "{n}");
        }
        IrAtom::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        IrAtom::Str(s) => {
            let _ = write!(out, "{s:?}");
        }
        IrAtom::Unit => out.push_str("()"),
        IrAtom::Arg(n) => {
            let _ = write!(out, "${n}");
        }
        IrAtom::Call(f, xs) => {
            out.push('(');
            out.push_str(f);
            for x in xs {
                out.push(' ');
                pp_atom(x, out);
            }
            out.push(')');
        }
    }
}

fn pp_fact(f: &IrFact, out: &mut String) {
    match f {
        IrFact::Row(t, xs) => {
            out.push('(');
            out.push_str(t);
            for x in xs {
                out.push(' ');
                pp_atom(x, out);
            }
            out.push(')');
        }
        IrFact::NotRow(t, xs) => {
            out.push_str("(not (");
            out.push_str(t);
            for x in xs {
                out.push(' ');
                pp_atom(x, out);
            }
            out.push_str("))");
        }
        IrFact::Eq(a, b) => pp_bin_fact("=", a, b, out),
        IrFact::Neq(a, b) => pp_bin_fact("!=", a, b, out),
        IrFact::Lt(a, b) => pp_bin_fact("<", a, b, out),
        IrFact::Le(a, b) => pp_bin_fact("<=", a, b, out),
        IrFact::Gt(a, b) => pp_bin_fact(">", a, b, out),
        IrFact::Ge(a, b) => pp_bin_fact(">=", a, b, out),
    }
}

fn pp_bin_fact(op: &str, a: &IrAtom, b: &IrAtom, out: &mut String) {
    out.push('(');
    out.push_str(op);
    out.push(' ');
    pp_atom(a, out);
    out.push(' ');
    pp_atom(b, out);
    out.push(')');
}

fn pp_action(a: &IrAction, out: &mut String) {
    match a {
        IrAction::Insert(t, xs) => {
            out.push('(');
            out.push_str(t);
            for x in xs {
                out.push(' ');
                pp_atom(x, out);
            }
            out.push(')');
        }
        IrAction::Union(a, b) => {
            out.push_str("(union ");
            pp_atom(a, out);
            out.push(' ');
            pp_atom(b, out);
            out.push(')');
        }
        IrAction::Delete(t, xs) => {
            out.push_str("(delete (");
            out.push_str(t);
            for x in xs {
                out.push(' ');
                pp_atom(x, out);
            }
            out.push_str("))");
        }
        IrAction::Subsume(t, xs) => {
            out.push_str("(subsume (");
            out.push_str(t);
            for x in xs {
                out.push(' ');
                pp_atom(x, out);
            }
            out.push_str("))");
        }
        IrAction::Call(f, xs) => {
            out.push('(');
            out.push_str(f);
            for x in xs {
                out.push(' ');
                pp_atom(x, out);
            }
            out.push(')');
        }
        IrAction::Panic(msg) => {
            let _ = write!(out, "(panic {msg:?})");
        }
    }
}

pub fn pp_egg(prog: &IrProgram) -> String {
    let mut out = String::with_capacity(512);
    for s in &prog.sorts {
        if s.class == SortClass::EqSort {
            let _ = writeln!(out, "(sort {})", s.name);
        }
    }
    for t in &prog.tables {
        match t.kind {
            TableKind::Relation => {
                let _ = writeln!(out, "(relation {} ({}))", t.name, t.cols.join(" "));
            }
            TableKind::Constructor => {
                let (out_s, ins) = t.cols.split_last().unwrap();
                let _ = writeln!(out, "(constructor {} ({}) {out_s})", t.name, ins.join(" "));
            }
            TableKind::Function => {
                let (out_s, ins) = t.cols.split_last().unwrap();
                let m = match &t.merge {
                    MergeKind::Max => " :merge (max old new)".to_owned(),
                    MergeKind::Min => " :merge (min old new)".to_owned(),
                    MergeKind::Union => " :merge (union old new)".to_owned(),
                    MergeKind::Old => " :merge old".to_owned(),
                    MergeKind::New => " :merge new".to_owned(),
                    MergeKind::Prim(p) => format!(" :merge ({p} old new)"),
                    MergeKind::None | MergeKind::AssertEq => String::new(),
                };
                let _ = writeln!(out, "(function {} ({}) {out_s}{m})", t.name, ins.join(" "));
            }
        }
    }
    for rs in &prog.rulesets {
        let rs_name = &rs.name;
        if !rs.members.is_empty() {
            let _ = writeln!(
                out,
                "(unstable-combined-ruleset {rs_name} {})",
                rs.members.join(" ")
            );
            continue;
        }
        let _ = writeln!(out, "(ruleset {rs_name})");
        for r in &rs.rules {
            out.push_str("(rule (");
            for (i, f) in r.facts.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                pp_fact(f, &mut out);
            }
            out.push_str(")\n      (");
            for (i, a) in r.actions.iter().enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                pp_action(a, &mut out);
            }
            let _ = writeln!(out, ")\n      :ruleset {rs_name} :name {:?})", r.name);
        }
    }
    out
}

// ── TEGG v15 VM Types & Serializer ───────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum VmLit {
    Int(i64),
    Str(String),
    Bool(bool),
    Unit,
    Arg(u32),
}

#[derive(Clone, Debug)]
enum VmStep {
    Scan(usize, usize, Vec<Bind>),
    Lookup(usize, usize, Vec<usize>, Option<Bind>),
    Expand(usize, usize, Vec<usize>, Vec<Bind>),
    Check(usize, Vec<usize>),
    Absent(usize, Vec<usize>),
    Same(usize, usize),
    Prim(usize, usize, Vec<usize>, Bind),
}

#[derive(Clone, Debug)]
enum VmAction {
    Entry(usize, Vec<usize>, usize),
    Set(usize, Vec<usize>),
    Union(usize, usize),
    Delete(usize, Vec<usize>),
    Subsume(usize, Vec<usize>),
    Call(usize, usize, Vec<usize>),
    Panic(String),
}

#[derive(Clone, Debug)]
struct NodePayload {
    step: VmStep,
    actions: Vec<VmAction>,
    rules: Vec<usize>,
}

#[derive(Clone, Debug)]
enum VmMerge {
    AssertEq,
    Union,
    Old,
    New,
    Prim(usize, Vec<VmMerge>),
}

struct VmTable {
    name: String,
    kind: u32,
    cols: Vec<usize>,
    keys: usize,
    merge: Option<VmMerge>,
    shapes: Vec<usize>,
}

struct VmExtern {
    name: String,
    inputs: Vec<usize>,
    output: usize,
}

struct VmIndex {
    table: usize,
    order: Vec<usize>,
}

struct VmRuleset {
    name: String,
    roots: Vec<usize>,
    first: Vec<usize>,
    used: Vec<usize>,
    rules: Vec<String>,
    consts: Vec<VmLit>,
    id_consts: Vec<usize>,
    members: Vec<String>,
    once: bool,
    colored: bool,
}

struct Writer {
    words: Vec<u32>,
    strings: HashMap<String, u32>,
    str_order: Vec<String>,
}

impl Writer {
    fn new() -> Self {
        Self {
            words: Vec::with_capacity(256),
            strings: HashMap::with_capacity(64),
            str_order: Vec::with_capacity(64),
        }
    }

    #[inline]
    fn u32(&mut self, v: u32) {
        self.words.push(v);
    }

    #[inline]
    fn usize(&mut self, v: usize) {
        self.words.push(v as u32);
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.strings.get(s) {
            return id;
        }
        let id = self.str_order.len() as u32;
        self.strings.insert(s.to_owned(), id);
        self.str_order.push(s.to_owned());
        id
    }

    fn str(&mut self, s: &str) {
        let id = self.intern(s);
        self.u32(id);
    }

    fn words(&mut self, xs: &[usize]) {
        self.usize(xs.len());
        for &x in xs {
            self.usize(x);
        }
    }

    fn bind(&mut self, b: Bind) {
        match b {
            Bind::New(r) => self.usize(r * 2),
            Bind::Same(r) => self.usize(r * 2 + 1),
        }
    }

    fn merge(&mut self, m: &VmMerge) {
        match m {
            VmMerge::AssertEq => {
                self.u32(0);
                self.u32(0);
                self.u32(0);
            }
            VmMerge::Union => {
                self.u32(1);
                self.u32(0);
                self.u32(0);
            }
            VmMerge::Old => {
                self.u32(2);
                self.u32(0);
                self.u32(0);
            }
            VmMerge::New => {
                self.u32(3);
                self.u32(0);
                self.u32(0);
            }
            VmMerge::Prim(p, args) => {
                self.u32(5);
                self.usize(*p);
                self.usize(args.len());
                for a in args {
                    self.merge(a);
                }
            }
        }
    }

    fn lit(&mut self, l: &VmLit) {
        match l {
            VmLit::Int(n) => {
                let u = *n as u64;
                self.u32(0);
                self.u32(u as u32);
                self.u32((u >> 32) as u32);
            }
            VmLit::Str(s) => {
                self.u32(2);
                self.str(s);
            }
            VmLit::Bool(b) => {
                self.u32(3);
                self.u32(u32::from(*b));
            }
            VmLit::Unit => self.u32(4),
            VmLit::Arg(n) => {
                self.u32(5);
                self.u32(*n);
            }
        }
    }

    fn step(&mut self, s: &VmStep) {
        match s {
            VmStep::Scan(tbl, sl, cols) => {
                self.u32(0);
                self.usize(*tbl);
                self.usize(*sl);
                self.usize(cols.len());
                for &b in cols {
                    self.bind(b);
                }
            }
            VmStep::Lookup(tbl, sl, key, out) => {
                self.u32(1);
                self.usize(*tbl);
                self.usize(*sl);
                self.words(key);
                match out {
                    None => self.u32(0),
                    Some(b) => {
                        self.u32(1);
                        self.bind(*b);
                    }
                }
            }
            VmStep::Expand(idx, sl, key, outs) => {
                self.u32(2);
                self.usize(*idx);
                self.usize(*sl);
                self.words(key);
                self.usize(outs.len());
                for &b in outs {
                    self.bind(b);
                }
            }
            VmStep::Check(idx, key) => {
                self.u32(3);
                self.usize(*idx);
                self.u32(0);
                self.words(key);
            }
            VmStep::Same(a, b) => {
                self.u32(4);
                self.usize(*a);
                self.usize(*b);
            }
            VmStep::Prim(p, ctx, args, out) => {
                self.u32(5);
                self.usize(*p);
                self.usize(*ctx);
                self.words(args);
                self.bind(*out);
            }
            VmStep::Absent(idx, key) => {
                self.u32(6);
                self.usize(*idx);
                self.u32(0);
                self.words(key);
            }
        }
    }

    fn action(&mut self, a: &VmAction) {
        match a {
            VmAction::Entry(tbl, args, out) => {
                self.u32(0);
                self.usize(*tbl);
                self.usize(*out);
                self.words(args);
            }
            VmAction::Set(tbl, args) => {
                self.u32(1);
                self.usize(*tbl);
                self.u32(0);
                self.words(args);
            }
            VmAction::Union(a, b) => {
                self.u32(2);
                self.usize(*a);
                self.usize(*b);
            }
            VmAction::Delete(tbl, args) => {
                self.u32(3);
                self.usize(*tbl);
                self.u32(0);
                self.words(args);
            }
            VmAction::Subsume(tbl, args) => {
                self.u32(4);
                self.usize(*tbl);
                self.u32(0);
                self.words(args);
            }
            VmAction::Call(prim, ctx, args) => {
                self.u32(5);
                self.usize(*prim);
                self.usize(*ctx);
                self.words(args);
            }
            VmAction::Panic(msg) => {
                self.u32(6);
                self.str(msg);
                self.u32(0);
            }
        }
    }
}

enum CmpItem {
    Eq(IrAtom, IrAtom),
    Cmp(&'static str, IrAtom, IrAtom),
    Prim(String, Vec<IrAtom>, IrAtom),
}

struct SchemaEnv<'a> {
    sort_ids: HashMap<String, usize>,
    sorts: Vec<(String, u32)>,
    s_i64: usize,
    s_bool: usize,
    s_str: usize,
    s_unit: usize,
    s_bigint: Option<usize>,
    declared_externs: Vec<(String, Vec<usize>, usize)>,
    externs: Vec<VmExtern>,
    extern_ids: HashMap<(String, Vec<usize>, usize), usize>,
    table_ids: HashMap<String, usize>,
    table_meta: HashMap<String, &'a IrTable>,
    vtables: Vec<VmTable>,
    indexes: Vec<VmIndex>,
    index_ids: HashMap<(usize, Vec<usize>), usize>,
}

impl<'a> SchemaEnv<'a> {
    fn new(prog: &'a IrProgram) -> Self {
        let mut env = Self {
            sort_ids: HashMap::with_capacity(16),
            sorts: Vec::with_capacity(16),
            s_i64: 0,
            s_bool: 0,
            s_str: 0,
            s_unit: 0,
            s_bigint: None,
            declared_externs: Vec::with_capacity(prog.externs.len()),
            externs: Vec::new(),
            extern_ids: HashMap::new(),
            table_ids: HashMap::with_capacity(prog.tables.len()),
            table_meta: HashMap::with_capacity(prog.tables.len()),
            vtables: Vec::with_capacity(prog.tables.len()),
            indexes: Vec::new(),
            index_ids: HashMap::new(),
        };
        env.s_i64 = env.add_sort("i64", 1);
        env.s_bool = env.add_sort("bool", 1);
        env.s_str = env.add_sort("String", 1);
        env.s_unit = env.add_sort("Unit", 1);
        for s in &prog.sorts {
            let c = match s.class {
                SortClass::EqSort => 0,
                SortClass::BaseSort => 1,
                SortClass::ContainerSort => 2,
            };
            let id = env.add_sort(&s.name, c);
            if s.name == "BigInt" {
                env.s_bigint = Some(id);
            }
        }
        for e in &prog.externs {
            let inputs: Vec<usize> = e.inputs.iter().map(|s| env.ensure_sort(s)).collect();
            let output = env.ensure_sort(&e.output);
            env.declared_externs.push((e.name.clone(), inputs, output));
        }
        for t in &prog.tables {
            let tid = env.vtables.len();
            env.table_ids.insert(t.name.clone(), tid);
            env.table_meta.insert(t.name.clone(), t);
            let cols: Vec<usize> = t.cols.iter().map(|s| env.ensure_sort(s)).collect();
            let shapes: Vec<usize> = cols
                .iter()
                .map(|&s| if env.sorts[s].1 == 0 { 0 } else { 1 })
                .collect();
            let (kind, keys, merge) = match t.kind {
                TableKind::Relation => (0, cols.len(), None),
                TableKind::Constructor => (2, cols.len().saturating_sub(1), Some(VmMerge::Union)),
                TableKind::Function => {
                    let out_sort = *cols.last().unwrap_or(&env.s_i64);
                    let m = match &t.merge {
                        MergeKind::AssertEq | MergeKind::None => VmMerge::AssertEq,
                        MergeKind::Union => VmMerge::Union,
                        MergeKind::Old => VmMerge::Old,
                        MergeKind::New => VmMerge::New,
                        MergeKind::Max => {
                            let p = env.get_extern("max", vec![out_sort, out_sort], out_sort);
                            VmMerge::Prim(p, vec![VmMerge::Old, VmMerge::New])
                        }
                        MergeKind::Min => {
                            let p = env.get_extern("min", vec![out_sort, out_sort], out_sort);
                            VmMerge::Prim(p, vec![VmMerge::Old, VmMerge::New])
                        }
                        MergeKind::Prim(pname) => {
                            let p = env.get_extern(pname, vec![out_sort, out_sort], out_sort);
                            VmMerge::Prim(p, vec![VmMerge::Old, VmMerge::New])
                        }
                    };
                    let k = t.keys.unwrap_or_else(|| cols.len().saturating_sub(1));
                    (1, k, Some(m))
                }
            };
            env.vtables.push(VmTable {
                name: t.name.clone(),
                kind,
                cols,
                keys,
                merge,
                shapes,
            });
        }
        env
    }

    fn add_sort(&mut self, name: &str, cls: u32) -> usize {
        if let Some(&id) = self.sort_ids.get(name) {
            return id;
        }
        let id = self.sorts.len();
        self.sort_ids.insert(name.to_owned(), id);
        self.sorts.push((name.to_owned(), cls));
        id
    }

    fn ensure_sort(&mut self, name: &str) -> usize {
        if let Some(&id) = self.sort_ids.get(name) {
            return id;
        }
        let cls = match name {
            "i64" | "f64" | "bool" | "String" | "Unit" | "BigInt" | "BigRat" | "NativeIdentity" => {
                1
            }
            _ => 0,
        };
        let id = self.add_sort(name, cls);
        if name == "BigInt" {
            self.s_bigint = Some(id);
        }
        id
    }

    fn get_extern(&mut self, name: &str, inputs: Vec<usize>, output: usize) -> usize {
        let key = (name.to_owned(), inputs.clone(), output);
        if let Some(&id) = self.extern_ids.get(&key) {
            return id;
        }
        let id = self.externs.len();
        self.extern_ids.insert(key, id);
        self.externs.push(VmExtern {
            name: name.to_owned(),
            inputs,
            output,
        });
        id
    }

    fn resolve_prim(&mut self, name: &str, arg_sorts: &[usize]) -> (usize, usize) {
        // 1. Exact match in explicitly declared externs
        for (en, ins, out) in &self.declared_externs {
            if en == name && ins.as_slice() == arg_sorts {
                let ins = ins.clone();
                let out = *out;
                return (self.get_extern(name, ins, out), out);
            }
        }
        // 2. Single declared extern with matching name and arity
        let matches: Vec<_> = self
            .declared_externs
            .iter()
            .filter(|(en, ins, _)| en == name && ins.len() == arg_sorts.len())
            .cloned()
            .collect();
        if matches.len() == 1 {
            let (_, ins, out) = matches.into_iter().next().unwrap();
            return (self.get_extern(name, ins, out), out);
        }
        // 3. Built-in tir_egglog primitives
        let s_i64 = self.s_i64;
        let s_bool = self.s_bool;
        let s_str = self.s_str;
        let s_unit = self.s_unit;
        let first = arg_sorts.first().copied().unwrap_or(s_i64);
        let (ins, out) = match name {
            "!=" | "value-eq" => (vec![first, first], s_unit),
            "bool-!=" => (vec![first, first], s_bool),
            "<" | "<=" | ">" | ">=" => (vec![first, first], s_unit),
            "equal" | "less" | "bool-<" | "bool->" | "bool-<=" | "bool->=" | "le" | "ge" => {
                (vec![first, first], s_bool)
            }
            "min" | "max" | "ordering-min" | "ordering-max" => (vec![first, first], first),
            "+" | "-" | "*" | "/" | "%" | "&" | "|" | "^" | "bit_and" | "bit_or" => {
                (vec![first, first], first)
            }
            "<<" | "shl" | ">>" => (vec![first, s_i64], first),
            "not-i64" | "abs" | "log2" => (vec![s_i64], s_i64),
            "not-Z" | "bit_not" | "bits" => {
                let z = self.ensure_sort("BigInt");
                (vec![z], z)
            }
            "bigint" => {
                let z = self.ensure_sort("BigInt");
                (vec![first], z)
            }
            "from-string" | "from_string" => {
                let z = self.ensure_sort("BigInt");
                (vec![s_str], z)
            }
            "to-string" | "to_string" => (vec![first], s_str),
            "not" => (vec![s_bool], s_bool),
            "and" | "xor" | "=>" => (vec![s_bool, s_bool], s_bool),
            "or" => (vec![s_bool; arg_sorts.len()], s_bool),
            "guard" => (vec![s_bool], s_unit),
            "native-ports" => {
                let nid = self.ensure_sort("NativeIdentity");
                (vec![nid, s_i64, s_i64], s_unit)
            }
            "fits-unsigned-hex" => {
                let z = self.ensure_sort("BigInt");
                (vec![s_str, z], s_bool)
            }
            "parse-hex-i64" => (vec![s_str], s_i64),
            "hex-power-of-two" => (vec![s_str], s_bool),
            _ if arg_sorts.is_empty() && name.starts_with("@cdsl:decl:") => {
                let nid = self.ensure_sort("NativeIdentity");
                (Vec::new(), nid)
            }
            _ => (arg_sorts.to_vec(), s_i64),
        };
        (self.get_extern(name, ins, out), out)
    }

    fn get_index(&mut self, tid: usize, order: Vec<usize>) -> usize {
        let key = (tid, order.clone());
        if let Some(&id) = self.index_ids.get(&key) {
            return id;
        }
        let id = self.indexes.len();
        self.index_ids.insert(key, id);
        self.indexes.push(VmIndex { table: tid, order });
        id
    }
}

fn scan_atom_consts(a: &IrAtom, intern: &mut impl FnMut(VmLit) -> usize) {
    match a {
        IrAtom::Int(n) => {
            intern(VmLit::Int(*n));
        }
        IrAtom::Bool(b) => {
            intern(VmLit::Bool(*b));
        }
        IrAtom::Str(s) => {
            intern(VmLit::Str(s.clone()));
        }
        IrAtom::Unit => {
            intern(VmLit::Unit);
        }
        IrAtom::Arg(n) => {
            intern(VmLit::Arg(*n));
        }
        IrAtom::Call(_, xs) => {
            for x in xs {
                scan_atom_consts(x, intern);
            }
        }
        IrAtom::Var(_) => {}
    }
}

fn collect_ruleset_consts(rules: &[IrRule]) -> (Vec<VmLit>, HashMap<VmLit, usize>) {
    let mut consts: Vec<VmLit> = Vec::new();
    let mut const_map: HashMap<VmLit, usize> = HashMap::new();
    let mut intern = |lit: VmLit| -> usize {
        if let Some(&r) = const_map.get(&lit) {
            return r;
        }
        let r = consts.len();
        const_map.insert(lit.clone(), r);
        consts.push(lit);
        r
    };
    let needs_unit = rules.iter().any(|r| r.facts.is_empty());
    if needs_unit {
        intern(VmLit::Unit);
    }
    for r in rules {
        for f in &r.facts {
            match f {
                IrFact::Row(_, xs) | IrFact::NotRow(_, xs) => {
                    for x in xs {
                        scan_atom_consts(x, &mut intern);
                    }
                }
                IrFact::Eq(a, b)
                | IrFact::Neq(a, b)
                | IrFact::Lt(a, b)
                | IrFact::Le(a, b)
                | IrFact::Gt(a, b)
                | IrFact::Ge(a, b) => {
                    scan_atom_consts(a, &mut intern);
                    scan_atom_consts(b, &mut intern);
                }
            }
        }
        for act in &r.actions {
            match act {
                IrAction::Insert(_, xs)
                | IrAction::Delete(_, xs)
                | IrAction::Subsume(_, xs)
                | IrAction::Call(_, xs) => {
                    for x in xs {
                        scan_atom_consts(x, &mut intern);
                    }
                }
                IrAction::Union(a, b) => {
                    scan_atom_consts(a, &mut intern);
                    scan_atom_consts(b, &mut intern);
                }
                IrAction::Panic(_) => {}
            }
        }
    }
    (consts, const_map)
}

fn flatten_fact_atom(
    a: &IrAtom,
    table_ids: &HashMap<String, usize>,
    flat_pos: &mut Vec<(String, Vec<IrAtom>)>,
    flat_cmp: &mut Vec<CmpItem>,
    tmp_n: &mut usize,
) -> IrAtom {
    match a {
        IrAtom::Call(f, args) if table_ids.contains_key(f) => {
            let mut fargs: Vec<IrAtom> = args
                .iter()
                .map(|x| flatten_fact_atom(x, table_ids, flat_pos, flat_cmp, tmp_n))
                .collect();
            *tmp_n += 1;
            let out = IrAtom::Var(format!("__t{}", *tmp_n));
            fargs.push(out.clone());
            flat_pos.push((f.clone(), fargs));
            out
        }
        IrAtom::Call(op, args) => {
            let fargs: Vec<IrAtom> = args
                .iter()
                .map(|x| flatten_fact_atom(x, table_ids, flat_pos, flat_cmp, tmp_n))
                .collect();
            *tmp_n += 1;
            let out = IrAtom::Var(format!("__t{}", *tmp_n));
            flat_cmp.push(CmpItem::Prim(op.clone(), fargs, out.clone()));
            out
        }
        other => other.clone(),
    }
}

struct FlatFacts {
    pos: Vec<(String, Vec<IrAtom>)>,
    neg: Vec<(String, Vec<IrAtom>)>,
    cmp: Vec<CmpItem>,
}

fn flatten_facts(facts: &[IrFact], table_ids: &HashMap<String, usize>) -> FlatFacts {
    let mut flat_pos = Vec::new();
    let mut flat_neg = Vec::new();
    let mut flat_cmp = Vec::new();
    let mut tmp_n = 0usize;
    for f in facts {
        match f {
            IrFact::Row(tbl, args) => {
                let fargs = args
                    .iter()
                    .map(|x| {
                        flatten_fact_atom(x, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n)
                    })
                    .collect();
                if table_ids.contains_key(tbl) {
                    flat_pos.push((tbl.clone(), fargs));
                } else {
                    tmp_n += 1;
                    let out = IrAtom::Var(format!("__t{tmp_n}"));
                    flat_cmp.push(CmpItem::Prim(tbl.clone(), fargs, out));
                }
            }
            IrFact::NotRow(tbl, args) => {
                let fargs = args
                    .iter()
                    .map(|x| {
                        flatten_fact_atom(x, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n)
                    })
                    .collect();
                flat_neg.push((tbl.clone(), fargs));
            }
            IrFact::Eq(IrAtom::Var(v), IrAtom::Call(tbl, args))
            | IrFact::Eq(IrAtom::Call(tbl, args), IrAtom::Var(v))
                if table_ids.contains_key(tbl) =>
            {
                let mut fargs: Vec<IrAtom> = args
                    .iter()
                    .map(|x| {
                        flatten_fact_atom(x, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n)
                    })
                    .collect();
                fargs.push(IrAtom::Var(v.clone()));
                flat_pos.push((tbl.clone(), fargs));
            }
            IrFact::Eq(a, b) => {
                let fa = flatten_fact_atom(a, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                let fb = flatten_fact_atom(b, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                flat_cmp.push(CmpItem::Eq(fa, fb));
            }
            IrFact::Neq(a, b) => {
                let fa = flatten_fact_atom(a, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                let fb = flatten_fact_atom(b, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                flat_cmp.push(CmpItem::Cmp("!=", fa, fb));
            }
            IrFact::Lt(a, b) => {
                let fa = flatten_fact_atom(a, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                let fb = flatten_fact_atom(b, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                flat_cmp.push(CmpItem::Cmp("<", fa, fb));
            }
            IrFact::Le(a, b) => {
                let fa = flatten_fact_atom(a, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                let fb = flatten_fact_atom(b, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                flat_cmp.push(CmpItem::Cmp("<=", fa, fb));
            }
            IrFact::Gt(a, b) => {
                let fa = flatten_fact_atom(a, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                let fb = flatten_fact_atom(b, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                flat_cmp.push(CmpItem::Cmp(">", fa, fb));
            }
            IrFact::Ge(a, b) => {
                let fa = flatten_fact_atom(a, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                let fb = flatten_fact_atom(b, table_ids, &mut flat_pos, &mut flat_cmp, &mut tmp_n);
                flat_cmp.push(CmpItem::Cmp(">=", fa, fb));
            }
        }
    }
    FlatFacts {
        pos: flat_pos,
        neg: flat_neg,
        cmp: flat_cmp,
    }
}

struct RuleCompiler<'a, 'b> {
    env: &'b mut SchemaEnv<'a>,
    const_map: &'b HashMap<VmLit, usize>,
    vars: HashMap<String, usize>,
    reg_sorts: Vec<usize>,
    next_reg: usize,
    steps: Vec<VmStep>,
    actions: Vec<VmAction>,
    used_idxs: &'b mut Vec<usize>,
}

impl<'a, 'b> RuleCompiler<'a, 'b> {
    fn new(
        env: &'b mut SchemaEnv<'a>,
        consts: &[VmLit],
        const_map: &'b HashMap<VmLit, usize>,
        used_idxs: &'b mut Vec<usize>,
    ) -> Self {
        let reg_sorts: Vec<usize> = consts
            .iter()
            .map(|c| match c {
                VmLit::Int(_) | VmLit::Arg(_) => env.s_i64,
                VmLit::Str(_) => env.s_str,
                VmLit::Bool(_) => env.s_bool,
                VmLit::Unit => env.s_unit,
            })
            .collect();
        let next_reg = consts.len();
        Self {
            env,
            const_map,
            vars: HashMap::new(),
            reg_sorts,
            next_reg,
            steps: Vec::new(),
            actions: Vec::new(),
            used_idxs,
        }
    }

    fn alloc_reg(&mut self, sort: usize) -> usize {
        let r = self.next_reg;
        self.next_reg += 1;
        self.reg_sorts.push(sort);
        r
    }

    fn known_reg(&self, atom: &IrAtom) -> Option<usize> {
        match atom {
            IrAtom::Int(n) => Some(self.const_map[&VmLit::Int(*n)]),
            IrAtom::Bool(b) => Some(self.const_map[&VmLit::Bool(*b)]),
            IrAtom::Str(s) => Some(self.const_map[&VmLit::Str(s.clone())]),
            IrAtom::Unit => Some(self.const_map[&VmLit::Unit]),
            IrAtom::Arg(n) => Some(self.const_map[&VmLit::Arg(*n)]),
            IrAtom::Var(v) if v != "_" => self.vars.get(v).copied(),
            _ => None,
        }
    }

    fn resolve_bound(&self, atom: &IrAtom) -> Result<usize, Error> {
        self.known_reg(atom).ok_or_else(|| match atom {
            IrAtom::Var(v) => Error(format!("TEGG planner: unbound variable {v}")),
            _ => Error("TEGG planner: nested call in guard".into()),
        })
    }

    fn bind_atoms(&mut self, atoms: &[IrAtom], col_sorts: &[usize]) -> Result<Vec<Bind>, Error> {
        let mut cur_w = self.next_reg;
        let mut reqs = Vec::with_capacity(atoms.len());
        let mut new_sorts = Vec::new();
        for (i, atom) in atoms.iter().enumerate() {
            let sort = col_sorts.get(i).copied().unwrap_or(self.env.s_i64);
            let req = match atom {
                IrAtom::Int(n) => ColReq::Known(self.const_map[&VmLit::Int(*n)]),
                IrAtom::Bool(b) => ColReq::Known(self.const_map[&VmLit::Bool(*b)]),
                IrAtom::Str(s) => ColReq::Known(self.const_map[&VmLit::Str(s.clone())]),
                IrAtom::Unit => ColReq::Known(self.const_map[&VmLit::Unit]),
                IrAtom::Arg(n) => ColReq::Known(self.const_map[&VmLit::Arg(*n)]),
                IrAtom::Var(v) if v == "_" => {
                    cur_w += 1;
                    new_sorts.push(sort);
                    ColReq::Fresh
                }
                IrAtom::Var(v) => {
                    if let Some(&reg) = self.vars.get(v) {
                        ColReq::Known(reg)
                    } else {
                        let reg = cur_w;
                        cur_w += 1;
                        self.vars.insert(v.clone(), reg);
                        new_sorts.push(sort);
                        ColReq::Fresh
                    }
                }
                IrAtom::Call(_, _) => {
                    return Err(Error(
                        "TEGG planner: nested call in Scan column must be flattened".into(),
                    ));
                }
            };
            reqs.push(req);
        }
        let (binds, w_after) = tegg_core::alloc_binds(self.next_reg, &reqs);
        self.next_reg = w_after;
        self.reg_sorts.extend(new_sorts);
        Ok(binds)
    }

    fn plan_positive_row(&mut self, tbl: &str, args: &[IrAtom], slice: usize) -> Result<(), Error> {
        let tid = *self
            .env
            .table_ids
            .get(tbl)
            .ok_or_else(|| Error(format!("TEGG planner: unknown table {tbl}")))?;
        let cols = self.env.vtables[tid].cols.clone();
        let kind = self.env.vtables[tid].kind;
        let keys = self.env.vtables[tid].keys;
        let arity = cols.len();
        if args.len() != arity {
            return Err(Error(format!(
                "TEGG planner: table {tbl} expects {arity} columns, got {}",
                args.len()
            )));
        }

        // Partition known vs fresh columns
        let mut known_cols = Vec::new();
        let mut known_regs = Vec::new();
        let mut fresh_cols = Vec::new();
        for (c, atom) in args.iter().enumerate() {
            if let Some(reg) = self.known_reg(atom) {
                known_cols.push(c);
                known_regs.push(reg);
            } else {
                fresh_cols.push(c);
            }
        }

        // Case 1: Function/Constructor with all key columns known -> Lookup (no extra index)
        if kind != 0 && keys + 1 == arity && (0..keys).all(|c| self.known_reg(&args[c]).is_some()) {
            let key_regs: Vec<usize> = (0..keys)
                .map(|c| self.known_reg(&args[c]).unwrap())
                .collect();
            let out_bind = self.bind_atoms(std::slice::from_ref(&args[keys]), &[cols[keys]])?[0];
            self.steps
                .push(VmStep::Lookup(tid, slice, key_regs, Some(out_bind)));
            return Ok(());
        }

        // Case 2: All columns known & Slice::All -> Check on full-row index
        if fresh_cols.is_empty() && slice == 2 {
            let order: Vec<usize> = (0..arity).collect();
            let idx = self.env.get_index(tid, order);
            if !self.used_idxs.contains(&idx) {
                self.used_idxs.push(idx);
            }
            self.steps.push(VmStep::Check(idx, known_regs));
            return Ok(());
        }

        // Case 3: Some (non-empty) columns known and some fresh -> Expand via index on known_cols
        if !known_cols.is_empty() && !fresh_cols.is_empty() {
            let idx = self.env.get_index(tid, known_cols);
            if !self.used_idxs.contains(&idx) {
                self.used_idxs.push(idx);
            }
            let fresh_atoms: Vec<IrAtom> = fresh_cols.iter().map(|&c| args[c].clone()).collect();
            let fresh_sorts: Vec<usize> = fresh_cols.iter().map(|&c| cols[c]).collect();
            let outs = self.bind_atoms(&fresh_atoms, &fresh_sorts)?;
            self.steps
                .push(VmStep::Expand(idx, slice, known_regs, outs));
            return Ok(());
        }

        // Case 4: Full Scan
        let binds = self.bind_atoms(args, &cols)?;
        self.steps.push(VmStep::Scan(tid, slice, binds));
        Ok(())
    }

    fn hoist_act_prims(
        &mut self,
        atom: &IrAtom,
        cache: &mut HashMap<*const IrAtom, usize>,
    ) -> Result<(), Error> {
        match atom {
            IrAtom::Call(f, args) if self.env.table_ids.contains_key(f) => {
                for x in args {
                    self.hoist_act_prims(x, cache)?;
                }
                Ok(())
            }
            IrAtom::Call(op, args) => {
                let mut rargs = Vec::with_capacity(args.len());
                for x in args {
                    self.hoist_act_prims(x, cache)?;
                    let r = if let Some(&reg) = cache.get(&(x as *const IrAtom)) {
                        reg
                    } else {
                        self.resolve_bound(x)?
                    };
                    rargs.push(r);
                }
                let arg_sorts: Vec<usize> = rargs
                    .iter()
                    .map(|&r| self.reg_sorts.get(r).copied().unwrap_or(self.env.s_i64))
                    .collect();
                let (p, out_sort) = self.env.resolve_prim(op, &arg_sorts);
                let out = self.alloc_reg(out_sort);
                self.steps.push(VmStep::Prim(p, 0, rargs, Bind::New(out)));
                cache.insert(atom as *const IrAtom, out);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn eval_act_atom(
        &mut self,
        atom: &IrAtom,
        cache: &HashMap<*const IrAtom, usize>,
    ) -> Result<usize, Error> {
        if let Some(&reg) = cache.get(&(atom as *const IrAtom)) {
            return Ok(reg);
        }
        match atom {
            IrAtom::Int(n) => Ok(self.const_map[&VmLit::Int(*n)]),
            IrAtom::Bool(b) => Ok(self.const_map[&VmLit::Bool(*b)]),
            IrAtom::Str(s) => Ok(self.const_map[&VmLit::Str(s.clone())]),
            IrAtom::Unit => Ok(self.const_map[&VmLit::Unit]),
            IrAtom::Arg(n) => Ok(self.const_map[&VmLit::Arg(*n)]),
            IrAtom::Var(v) => self
                .vars
                .get(v)
                .copied()
                .ok_or_else(|| Error(format!("TEGG planner: unbound action var {v}"))),
            IrAtom::Call(f, args) if self.env.table_ids.contains_key(f) => {
                let tid = self.env.table_ids[f];
                let out_sort = *self.env.vtables[tid].cols.last().unwrap_or(&self.env.s_i64);
                let mut rargs = Vec::with_capacity(args.len());
                for x in args {
                    rargs.push(self.eval_act_atom(x, cache)?);
                }
                let out = self.alloc_reg(out_sort);
                self.actions.push(VmAction::Entry(tid, rargs, out));
                Ok(out)
            }
            IrAtom::Call(op, _) => Err(Error(format!(
                "TEGG planner: unhoisted action primitive {op}"
            ))),
        }
    }

    #[allow(clippy::cognitive_complexity)]
    fn compile_variant(
        mut self,
        rule_idx: usize,
        rule_name: &str,
        flat: &FlatFacts,
        rule_actions: &[IrAction],
        slices: &[usize],
        nodes: &mut Vec<VmNode<NodePayload>>,
    ) -> Result<usize, Error> {
        let flat_pos = &flat.pos;
        let flat_neg = &flat.neg;
        let flat_cmp = &flat.cmp;
        // 1. Evaluate 0-ary primitives first (e.g. NativeIdentity declarations) so they can serve as lookup keys!
        let mut remaining_cmp = Vec::with_capacity(flat_cmp.len());
        for item in flat_cmp {
            if let CmpItem::Prim(op, args, out) = item
                && args.is_empty()
            {
                let (p, out_sort) = self.env.resolve_prim(op, &[]);
                let b = self.bind_atoms(std::slice::from_ref(out), &[out_sort])?[0];
                self.steps.push(VmStep::Prim(p, 0, Vec::new(), b));
            } else {
                remaining_cmp.push(item);
            }
        }

        // 2. Plan positive table joins in greedy bound-first order (starting with Slice::New fact if any)
        let mut done = vec![false; flat_pos.len()];
        for step_i in 0..flat_pos.len() {
            let pick = if step_i == 0
                && let Some(new_idx) = slices.iter().position(|&s| s == 0)
            {
                new_idx
            } else {
                // Prefer a fact with the most already-known columns
                let mut best = None;
                let mut best_score = 0usize;
                for (i, (_, args)) in flat_pos.iter().enumerate() {
                    if done[i] {
                        continue;
                    }
                    let known = args.iter().filter(|a| self.known_reg(a).is_some()).count();
                    let score = if known == args.len() {
                        1000 + known
                    } else if known > 0 {
                        100 + known
                    } else {
                        1
                    };
                    if best.is_none() || score > best_score {
                        best = Some(i);
                        best_score = score;
                    }
                }
                best.unwrap()
            };
            done[pick] = true;
            let (tbl, args) = &flat_pos[pick];
            self.plan_positive_row(tbl, args, slices[pick])?;
        }

        // 3. Comparisons & remaining primitives (fixed-point loop so chained prims/eqs resolve in dependency order)
        let mut cmp_done = vec![false; remaining_cmp.len()];
        loop {
            let mut progressed = false;
            for (i, item) in remaining_cmp.iter().enumerate() {
                if cmp_done[i] {
                    continue;
                }
                match item {
                    CmpItem::Eq(a, b) => {
                        if let (Some(ra), Some(rb)) = (self.known_reg(a), self.known_reg(b)) {
                            self.steps.push(VmStep::Same(ra, rb));
                            cmp_done[i] = true;
                            progressed = true;
                        } else if let (IrAtom::Var(v), Some(rb)) = (a, self.known_reg(b)) {
                            self.vars.insert(v.clone(), rb);
                            cmp_done[i] = true;
                            progressed = true;
                        } else if let (Some(ra), IrAtom::Var(v)) = (self.known_reg(a), b) {
                            self.vars.insert(v.clone(), ra);
                            cmp_done[i] = true;
                            progressed = true;
                        }
                    }
                    CmpItem::Cmp(op, a, b) => {
                        if let (Some(ra), Some(rb)) = (self.known_reg(a), self.known_reg(b)) {
                            let sa = self.reg_sorts.get(ra).copied().unwrap_or(self.env.s_i64);
                            let sb = self.reg_sorts.get(rb).copied().unwrap_or(sa);
                            let (p, out_sort) = self.env.resolve_prim(op, &[sa, sb]);
                            let out = self.alloc_reg(out_sort);
                            self.steps
                                .push(VmStep::Prim(p, 0, vec![ra, rb], Bind::New(out)));
                            cmp_done[i] = true;
                            progressed = true;
                        }
                    }
                    CmpItem::Prim(op, args, out) => {
                        if args.iter().all(|x| self.known_reg(x).is_some()) {
                            let rargs: Vec<usize> =
                                args.iter().map(|x| self.known_reg(x).unwrap()).collect();
                            let arg_sorts: Vec<usize> = rargs
                                .iter()
                                .map(|&r| self.reg_sorts.get(r).copied().unwrap_or(self.env.s_i64))
                                .collect();
                            let (p, out_sort) = self.env.resolve_prim(op, &arg_sorts);
                            let b = self.bind_atoms(std::slice::from_ref(out), &[out_sort])?[0];
                            self.steps.push(VmStep::Prim(p, 0, rargs, b));
                            cmp_done[i] = true;
                            progressed = true;
                        }
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        if let Some(idx) = cmp_done.iter().position(|&d| !d) {
            return Err(Error(format!(
                "TEGG planner: unresolved comparison/primitive #{idx} in rule {rule_name}"
            )));
        }

        // 4. Negated table checks (`Absent`)
        for (tbl, args) in flat_neg {
            let tid = *self
                .env
                .table_ids
                .get(tbl)
                .ok_or_else(|| Error(format!("TEGG planner: unknown negated table {tbl}")))?;
            let order: Vec<usize> = (0..args.len()).collect();
            let idx = self.env.get_index(tid, order);
            if !self.used_idxs.contains(&idx) {
                self.used_idxs.push(idx);
            }
            let key: Vec<usize> = args
                .iter()
                .map(|x| self.resolve_bound(x))
                .collect::<Result<_, _>>()?;
            self.steps.push(VmStep::Absent(idx, key));
        }

        // 5a. Hoist all action-side primitive calls into query steps before any
        // `VmAction::Entry` allocates an action register, preserving strict
        // sequential register width (`Bind::New(w)`) required by the TEGG loader.
        let mut act_prim_cache: HashMap<*const IrAtom, usize> = HashMap::new();
        for act in rule_actions {
            match act {
                IrAction::Insert(_, args)
                | IrAction::Delete(_, args)
                | IrAction::Subsume(_, args)
                | IrAction::Call(_, args) => {
                    for x in args {
                        self.hoist_act_prims(x, &mut act_prim_cache)?;
                    }
                }
                IrAction::Union(a, b) => {
                    self.hoist_act_prims(a, &mut act_prim_cache)?;
                    self.hoist_act_prims(b, &mut act_prim_cache)?;
                }
                IrAction::Panic(_) => {}
            }
        }

        // 5b. Compile actions
        for act in rule_actions {
            match act {
                IrAction::Insert(tbl, args) => {
                    let tid = *self.env.table_ids.get(tbl).ok_or_else(|| {
                        Error(format!("TEGG planner: unknown action table {tbl}"))
                    })?;
                    let tm = self.env.table_meta[tbl];
                    let mut rargs = Vec::with_capacity(args.len());
                    for x in args {
                        rargs.push(self.eval_act_atom(x, &act_prim_cache)?);
                    }
                    if tm.kind == TableKind::Constructor && rargs.len() + 1 == tm.cols.len() {
                        let out_sort =
                            *self.env.vtables[tid].cols.last().unwrap_or(&self.env.s_i64);
                        let out = self.alloc_reg(out_sort);
                        self.actions.push(VmAction::Entry(tid, rargs, out));
                    } else {
                        self.actions.push(VmAction::Set(tid, rargs));
                    }
                }
                IrAction::Union(a, b) => {
                    let ra = self.eval_act_atom(a, &act_prim_cache)?;
                    let rb = self.eval_act_atom(b, &act_prim_cache)?;
                    self.actions.push(VmAction::Union(ra, rb));
                }
                IrAction::Delete(tbl, args) => {
                    let tid = self.env.table_ids[tbl];
                    let mut rargs = Vec::with_capacity(args.len());
                    for x in args {
                        rargs.push(self.eval_act_atom(x, &act_prim_cache)?);
                    }
                    self.actions.push(VmAction::Delete(tid, rargs));
                }
                IrAction::Subsume(tbl, args) => {
                    let tid = self.env.table_ids[tbl];
                    let mut rargs = Vec::with_capacity(args.len());
                    for x in args {
                        rargs.push(self.eval_act_atom(x, &act_prim_cache)?);
                    }
                    self.actions.push(VmAction::Subsume(tid, rargs));
                }
                IrAction::Call(op, args) => {
                    let mut rargs = Vec::with_capacity(args.len());
                    for x in args {
                        rargs.push(self.eval_act_atom(x, &act_prim_cache)?);
                    }
                    let arg_sorts: Vec<usize> = rargs
                        .iter()
                        .map(|&r| self.reg_sorts.get(r).copied().unwrap_or(self.env.s_i64))
                        .collect();
                    let (p, _) = self.env.resolve_prim(op, &arg_sorts);
                    // Context::Write = 1
                    self.actions.push(VmAction::Call(p, 1, rargs));
                }
                IrAction::Panic(msg) => {
                    self.actions.push(VmAction::Panic(msg.clone()));
                }
            }
        }

        // Empty-fact `once` rule: synthesize a trivial valid query step `Same(unit_reg, unit_reg)`.
        if self.steps.is_empty() {
            let ureg = self.const_map[&VmLit::Unit];
            self.steps.push(VmStep::Same(ureg, ureg));
        }

        // 6. Build bottom-up chain via formally verified `tegg_core::build_chain`
        let leaf_step = self.steps.pop().unwrap();
        self.steps.reverse();
        let leaf_payload = NodePayload {
            step: leaf_step,
            actions: self.actions,
            rules: vec![rule_idx],
        };
        let parent_payloads: Vec<NodePayload> = self
            .steps
            .into_iter()
            .map(|st| NodePayload {
                step: st,
                actions: Vec::new(),
                rules: Vec::new(),
            })
            .collect();
        let taken = std::mem::take(nodes);
        let (next_nodes, root_id) = tegg_core::build_chain(taken, leaf_payload, parent_payloads);
        *nodes = next_nodes;
        Ok(root_id)
    }
}

/// Compiles a ground `IrProgram` into TEGG v15 binary bytecode loadable by
/// `tir_egglog::Program::from_bytes`.
pub fn compile(prog: &IrProgram) -> Result<Vec<u8>, Error> {
    let mut env = SchemaEnv::new(prog);
    let mut nodes: Vec<VmNode<NodePayload>> = Vec::new();
    let mut vrulesets: Vec<VmRuleset> = Vec::with_capacity(prog.rulesets.len());

    for rs in &prog.rulesets {
        if !rs.members.is_empty() {
            vrulesets.push(VmRuleset {
                name: rs.name.clone(),
                roots: Vec::new(),
                first: Vec::new(),
                used: Vec::new(),
                rules: Vec::new(),
                consts: Vec::new(),
                id_consts: Vec::new(),
                members: rs.members.clone(),
                once: false,
                colored: rs.colored,
            });
            continue;
        }

        let (consts, const_map) = collect_ruleset_consts(&rs.rules);
        let mut used_idxs: Vec<usize> = Vec::new();
        let mut roots = Vec::with_capacity(rs.rules.len());
        let mut first = Vec::with_capacity(rs.rules.len());
        let mut all_factless = !rs.rules.is_empty();

        for (i, r) in rs.rules.iter().enumerate() {
            let flat = flatten_facts(&r.facts, &env.table_ids);
            if !flat.pos.is_empty() || !flat.neg.is_empty() {
                all_factless = false;
            }
            // Seminaive variants for `roots`
            if flat.pos.is_empty() {
                let rc = RuleCompiler::new(&mut env, &consts, &const_map, &mut used_idxs);
                roots.push(rc.compile_variant(i, &r.name, &flat, &r.actions, &[], &mut nodes)?);
            } else {
                for v in 0..flat.pos.len() {
                    let slices: Vec<usize> = (0..flat.pos.len())
                        .map(|j| {
                            if j < v {
                                1
                            } else if j == v {
                                0
                            } else {
                                2
                            }
                        })
                        .collect();
                    let rc = RuleCompiler::new(&mut env, &consts, &const_map, &mut used_idxs);
                    roots.push(
                        rc.compile_variant(i, &r.name, &flat, &r.actions, &slices, &mut nodes)?,
                    );
                }
            }
            // Naive variant (`Slice::All`) for `first`
            let all_slices = vec![2usize; flat.pos.len()];
            let rc = RuleCompiler::new(&mut env, &consts, &const_map, &mut used_idxs);
            first.push(rc.compile_variant(
                i,
                &r.name,
                &flat,
                &r.actions,
                &all_slices,
                &mut nodes,
            )?);
        }
        used_idxs.sort_unstable();
        let rule_names: Vec<String> = rs.rules.iter().map(|r| r.name.clone()).collect();
        vrulesets.push(VmRuleset {
            name: rs.name.clone(),
            roots,
            first,
            used: used_idxs,
            rules: rule_names,
            consts,
            id_consts: Vec::new(),
            members: Vec::new(),
            once: rs.once || all_factless,
            colored: rs.colored,
        });
    }

    serialize_tegg(&env, &nodes, &vrulesets)
}

fn serialize_tegg(
    env: &SchemaEnv<'_>,
    nodes: &[VmNode<NodePayload>],
    vrulesets: &[VmRuleset],
) -> Result<Vec<u8>, Error> {
    let mut w = Writer::new();
    // sorts
    w.usize(env.sorts.len());
    for (name, cls) in &env.sorts {
        w.str(name);
        w.u32(*cls);
    }
    // consts (default values)
    w.u32(0);
    // tables
    w.usize(env.vtables.len());
    for t in &env.vtables {
        w.str(&t.name);
        w.u32(t.kind);
        w.words(&t.cols);
        w.usize(t.keys);
        match &t.merge {
            None => w.u32(0),
            Some(m) => {
                w.u32(1);
                w.merge(m);
            }
        }
        w.words(&t.shapes);
        w.u32(0); // symmetries
        w.u32(u32::MAX);
        w.u32(u32::MAX); // cost = None
        w.u32(0); // unextractable = false
        w.u32(0); // has_default = false
        w.u32(0); // default = 0
    }
    // groups
    w.u32(0);
    // externs
    w.usize(env.externs.len());
    for e in &env.externs {
        w.str(&e.name);
        w.words(&e.inputs);
        w.usize(e.output);
    }
    // indexes
    w.usize(env.indexes.len());
    for idx in &env.indexes {
        w.usize(idx.table);
        w.words(&idx.order);
    }
    // nodes count
    let num_nodes = nodes.len();
    w.usize(num_nodes);
    // rulesets
    w.usize(vrulesets.len());
    for rs in vrulesets {
        w.str(&rs.name);
        w.words(&rs.roots);
        w.words(&rs.first);
        w.words(&rs.used);
        w.usize(rs.rules.len());
        for rn in &rs.rules {
            w.str(rn);
        }
        w.usize(rs.consts.len());
        for c in &rs.consts {
            w.lit(c);
        }
        w.u32(0); // calls
        w.words(&rs.id_consts);
        w.usize(rs.members.len());
        for m in &rs.members {
            w.str(m);
        }
        w.u32(u32::from(rs.once));
        w.u32(u32::from(rs.colored));
    }

    let prefix_words = std::mem::take(&mut w.words);
    let table_start = prefix_words.len();
    let mut offsets: Vec<u32> = Vec::with_capacity(num_nodes);
    let mut cur_off = table_start + num_nodes;
    for node in nodes {
        offsets.push(cur_off as u32);
        let before = w.words.len();
        w.step(&node.payload.step);
        w.usize(node.payload.actions.len());
        for a in &node.payload.actions {
            w.action(a);
        }
        w.words(&node.children);
        w.words(&node.payload.rules);
        let added = w.words.len() - before;
        cur_off += added;
    }
    let node_words = w.words;

    let str_lens: Vec<usize> = w.str_order.iter().map(String::len).collect();
    let total_words = prefix_words.len() + offsets.len() + node_words.len();
    let expected_len = tegg_core::tegg_file_bytes(&str_lens, total_words);

    let mut out = Vec::with_capacity(expected_len);
    out.extend_from_slice(b"TEGG");
    out.extend_from_slice(&15u32.to_le_bytes());
    out.extend_from_slice(&(w.str_order.len() as u32).to_le_bytes());
    for s in &w.str_order {
        let bytes = s.as_bytes();
        out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        out.extend_from_slice(bytes);
        let pad = tegg_core::pad4(bytes.len());
        out.extend(std::iter::repeat_n(0, pad));
    }
    for &word in prefix_words.iter().chain(&offsets).chain(&node_words) {
        out.extend_from_slice(&word.to_le_bytes());
    }
    if out.len() != expected_len {
        return Err(Error(format!(
            "TEGG internal alignment mismatch: {} != {expected_len}",
            out.len()
        )));
    }
    Ok(out)
}
