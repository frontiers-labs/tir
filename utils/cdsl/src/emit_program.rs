//! Direct Rust printer for TIR relational programs & catalogs
//! (`OutputKind::Program` and `OutputKind::Catalog`).
//!
//! Emits `OperationId`, `ValueId`, `ValueRef<T>`, `OperationKind`, `Class<T>`,
//! `Index<K, R>`, eq-sort enums + TermDag decoders + `lookup_*`/`rows_*`/`index_*_by_result`,
//! auto-linked `NativeDeclaration`s, `bind_*` extern binders, `table_*` getters,
//! `install_schema`, `attach`, `Program<'db>`, schedules, `RuleCollection`s,
//! and cached `memo` functions (table-returning, query-returning, and scalar-returning) —
//! with ZERO string templates in `.cdsl` files!

use std::fmt::Write;

use crate::{
    check::Signature,
    core::{Lit, PrimOp},
    erase::{
        MlCodec, MlDecl, MlEqEnum, MlExpr, MlExtern, MlLinkedOp, MlMemoBody, MlProgram,
        MlProgramBundle, MlProgramMemo, MlRuleCollection, MlScheduleDecl, MlTable, MlValueTy,
        to_snake,
    },
    tegg::IrSchedule,
};

fn pascal_from_snake(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut cap = true;
    for c in s.chars() {
        if c == '_' {
            cap = true;
        } else if cap {
            out.push(c.to_ascii_uppercase());
            cap = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn escape_rust_ident(s: &str) -> String {
    match s {
        "as" | "async" | "await" | "break" | "const" | "continue" | "crate" | "dyn" | "else"
        | "enum" | "extern" | "false" | "fn" | "for" | "if" | "in" | "let" | "loop" | "match"
        | "mod" | "move" | "mut" | "pub" | "ref" | "return" | "self" | "Self" | "static"
        | "struct" | "super" | "trait" | "true" | "type" | "unsafe" | "use" | "where" | "while"
        | "yield" => format!("r#{s}"),
        _ => s.to_owned(),
    }
}

fn emit_byte_literal(bytes: &[u8], out: &mut String) {
    out.push_str("b\"");
    for &b in bytes {
        let _ = write!(out, "\\x{b:02x}");
    }
    out.push('"');
}

fn codec_rust_ty(c: &MlCodec) -> String {
    match c {
        MlCodec::Int => "i64".to_owned(),
        MlCodec::Bool => "bool".to_owned(),
        MlCodec::Str => "String".to_owned(),
        MlCodec::BigInt => "::tir_egglog::sort::BigInt".to_owned(),
        MlCodec::OpId => "OperationId".to_owned(),
        MlCodec::ValueId => "ValueId".to_owned(),
        MlCodec::ValueRef => "ValueRef<()>".to_owned(),
        MlCodec::Kind => "OperationKind".to_owned(),
        MlCodec::Enum { rust_name, .. } => rust_name.clone(),
    }
}

fn codec_class_ty(c: &MlCodec) -> String {
    match c {
        MlCodec::Enum { rust_name, .. } => format!("Class<{rust_name}>"),
        other => codec_rust_ty(other),
    }
}

fn codec_field_ty(c: &MlCodec) -> String {
    match c {
        MlCodec::Enum { rust_name, .. } => format!("Box<{rust_name}>"),
        other => codec_rust_ty(other),
    }
}

fn codec_encode(c: &MlCodec, db: &str, expr: &str) -> String {
    match c {
        MlCodec::Int => format!("{db}.base({expr})"),
        MlCodec::Bool => format!("{db}.base({expr})"),
        MlCodec::Str => format!("{db}.base({expr})"),
        MlCodec::BigInt => format!("{db}.base({expr})"),
        MlCodec::OpId | MlCodec::ValueId | MlCodec::ValueRef => {
            format!("{db}.base(i64::from(({expr}).raw()))")
        }
        MlCodec::Kind => format!("{db}.base(({expr}).declaration())"),
        MlCodec::Enum { .. } => format!("({expr}).value_for({db}.id())?"),
    }
}

fn codec_encode_prim(c: &MlCodec, p: &str, expr: &str) -> String {
    match c {
        MlCodec::Int | MlCodec::Bool | MlCodec::Str | MlCodec::BigInt => {
            format!("{p}.base({expr})")
        }
        MlCodec::OpId | MlCodec::ValueId | MlCodec::ValueRef => {
            format!("{p}.base(i64::from(({expr}).raw()))")
        }
        MlCodec::Kind => format!("{p}.base(({expr}).declaration())"),
        MlCodec::Enum { .. } => format!("({expr}).value"),
    }
}

fn codec_decode_val(c: &MlCodec, db: &str, val: &str, prim: bool) -> String {
    let v = if prim {
        format!("*{val}")
    } else {
        val.to_owned()
    };
    let try_sfx = if prim { ".ok()?" } else { "?" };
    match c {
        MlCodec::Int => format!("{db}.base_value::<i64>({v})"),
        MlCodec::Bool => format!("{db}.base_value::<bool>({v})"),
        MlCodec::Str => format!("{db}.base_value::<String>({v})"),
        MlCodec::BigInt => format!("{db}.base_value::<::tir_egglog::sort::BigInt>({v})"),
        MlCodec::OpId => {
            format!("OperationId::from_raw(u32::try_from({db}.base_value::<i64>({v})){try_sfx})")
        }
        MlCodec::ValueId => {
            format!("ValueId::from_raw(u32::try_from({db}.base_value::<i64>({v})){try_sfx})")
        }
        MlCodec::ValueRef => {
            format!("ValueRef::from_raw(u32::try_from({db}.base_value::<i64>({v})){try_sfx})")
        }
        MlCodec::Kind if prim => {
            format!("OperationKind::from({db}.base_value::<::tir_egglog::NativeIdentity>({v}))")
        }
        MlCodec::Kind => format!(
            "OperationKind::decode({db}, {db}.base_value::<::tir_egglog::NativeIdentity>({v}))?"
        ),
        MlCodec::Enum { .. } if prim => unreachable!("enum not supported in extern prim"),
        MlCodec::Enum { .. } => format!("Class::new({db}, {v})"),
    }
}

fn codec_decode_base(c: &MlCodec, db: &str, val: &str) -> String {
    codec_decode_val(c, db, val, false)
}

fn codec_decode_prim(c: &MlCodec, p: &str, val: &str) -> String {
    codec_decode_val(c, p, val, true)
}

fn codec_decode_cell(c: &MlCodec, db: &str, val: &str, captured: Option<&str>) -> String {
    match c {
        MlCodec::Enum { rust_name, .. } => {
            if let Some(cap) = captured {
                format!(
                    "{{ let (__dag, __term) = {cap}.best({val})?; {rust_name}::decode({db}, __dag, __term)? }}"
                )
            } else {
                format!(
                    "{{ let (dag, term) = extractor.get_or_init(|| ::tir_egglog::Extractor::new_in({db}, color)).best({val})?; {rust_name}::decode({db}, &dag, term)? }}"
                )
            }
        }
        other => codec_decode_base(other, db, val),
    }
}

fn codec_decode_term(c: &MlCodec, arg: &str) -> String {
    match c {
        MlCodec::Int => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::Int(v)) => *v, _ => return Err(invalid()) }}"
        ),
        MlCodec::Bool => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::Bool(v)) => *v, _ => return Err(invalid()) }}"
        ),
        MlCodec::Str => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::String(v)) => v.clone(), _ => return Err(invalid()) }}"
        ),
        MlCodec::BigInt => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::App(tag, xs) if (tag == \"from-string\" || tag == \"from_string\") && xs.len() == 1 => match terms.get(xs[0]) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::String(s)) => s.parse::<::tir_egglog::sort::BigInt>().map_err(|_| invalid())?, _ => return Err(invalid()) }}, ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::Int(v)) => ::tir_egglog::sort::BigInt::new(*v), _ => return Err(invalid()) }}"
        ),
        MlCodec::OpId => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::Int(v)) => OperationId::from_raw(u32::try_from(*v)?), _ => return Err(invalid()) }}"
        ),
        MlCodec::ValueId => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::Int(v)) => ValueId::from_raw(u32::try_from(*v)?), _ => return Err(invalid()) }}"
        ),
        MlCodec::ValueRef => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::Int(v)) => ValueRef::from_raw(u32::try_from(*v)?), _ => return Err(invalid()) }}"
        ),
        MlCodec::Kind => format!(
            "match terms.get(*{arg}) {{ ::tir_egglog::Term::Lit(::tir_egglog::prelude::Literal::Native(v)) => OperationKind::decode(_db, *v)?, _ => return Err(invalid()) }}"
        ),
        MlCodec::Enum { rust_name, .. } => {
            format!("Box::new({rust_name}::decode(_db, terms, *{arg})?)")
        }
    }
}

pub(crate) fn emit_schedule_expr(sched: &IrSchedule, out: &mut String) {
    const SYN: crate::emit_rust::ScheduleSyntax = crate::emit_rust::ScheduleSyntax {
        run_open: "::tir_egglog::Schedule::run(",
        sat_run_open: "::tir_egglog::Schedule::saturate(::tir_egglog::Schedule::run(",
        sat_run_close: "))",
        rep_run_open: "::tir_egglog::Schedule::repeat(",
        rep_run_mid: ", ::tir_egglog::Schedule::run(",
        rep_run_close: "))",
        seq_open: "::tir_egglog::Schedule::seq([",
        seq_close: "])",
        sat_sched_open: "::tir_egglog::Schedule::saturate(",
        rep_sched_open: "::tir_egglog::Schedule::repeat(",
    };
    crate::emit_rust::emit_schedule_with(sched, &SYN, out);
}

fn emit_prelude(out: &mut String) {
    out.push_str(
        "type DecodeResult<T> = Result<T, Box<dyn std::error::Error>>;\n\n\
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
pub struct OperationId(u32);\n\
impl OperationId {\n\
    pub const fn from_raw(raw: u32) -> Self { Self(raw) }\n\
    pub const fn raw(self) -> u32 { self.0 }\n\
}\n\n\
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
pub struct ValueId(u32);\n\
impl ValueId {\n\
    pub const fn from_raw(raw: u32) -> Self { Self(raw) }\n\
    pub const fn raw(self) -> u32 { self.0 }\n\
}\n\n\
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
pub struct ValueRef<T>(u32, std::marker::PhantomData<fn(T) -> T>);\n\
impl<T> ValueRef<T> {\n\
    pub fn from_raw(raw: u32) -> Self { Self(raw, std::marker::PhantomData) }\n\
    pub fn raw(self) -> u32 { self.0 }\n\
}\n\n\
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]\n\
pub struct OperationKind(::tir_egglog::NativeIdentity);\n\
impl From<::tir_egglog::NativeIdentity> for OperationKind {\n\
    fn from(declaration: ::tir_egglog::NativeIdentity) -> Self { Self(declaration) }\n\
}\n\
impl OperationKind {\n\
    fn decode(_: &::tir_egglog::Database, identity: ::tir_egglog::NativeIdentity) -> DecodeResult<Self> {\n\
        Ok(Self(identity))\n\
    }\n\
    pub fn declaration(&self) -> ::tir_egglog::NativeIdentity { self.0 }\n\
    pub fn as_str(&self) -> &str { self.0.as_str() }\n\
}\n\n\
pub struct Class<T> {\n\
    value: ::tir_egglog::Value,\n\
    database: ::tir_egglog::DatabaseId,\n\
    sort: std::marker::PhantomData<fn(T) -> T>,\n\
}\n\
impl<T> Clone for Class<T> {\n\
    fn clone(&self) -> Self { *self }\n\
}\n\
impl<T> Copy for Class<T> {}\n\
impl<T> PartialEq for Class<T> {\n\
    fn eq(&self, other: &Self) -> bool { self.value == other.value && self.database == other.database }\n\
}\n\
impl<T> Eq for Class<T> {}\n\
impl<T> PartialOrd for Class<T> {\n\
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(other)) }\n\
}\n\
impl<T> Ord for Class<T> {\n\
    fn cmp(&self, other: &Self) -> std::cmp::Ordering { self.value.cmp(&other.value).then(self.database.cmp(&other.database)) }\n\
}\n\
impl<T> std::hash::Hash for Class<T> {\n\
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) { self.value.hash(state); }\n\
}\n\
impl<T> std::fmt::Debug for Class<T> {\n\
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, \"Class({:?})\", self.value) }\n\
}\n\
impl<T> Class<T> {\n\
    fn new(db: &::tir_egglog::Database, value: ::tir_egglog::Value) -> Self {\n\
        Self { value, database: db.id(), sort: std::marker::PhantomData }\n\
    }\n\
    fn value_for(self, database: ::tir_egglog::DatabaseId) -> Result<::tir_egglog::Value, ::tir_egglog::Error> {\n\
        if self.database == database { Ok(self.value) } else { Err(::tir_egglog::Error::Unsupported(\"class belongs to a different database\".into())) }\n\
    }\n\
}\n\n\
pub struct Index<K, R> {\n\
    index: ::tir_egglog::IndexId,\n\
    database: ::tir_egglog::DatabaseId,\n\
    key: fn(&::tir_egglog::Database, K) -> Result<::tir_egglog::Value, ::tir_egglog::Error>,\n\
    decode: fn(&::tir_egglog::Database, ::tir_egglog::Row<'_>) -> DecodeResult<R>,\n\
}\n\n",
    );
}

fn emit_eq_enum(en: &MlEqEnum, out: &mut String) {
    if en.rust_name.contains('_') {
        out.push_str("#[allow(non_camel_case_types)]\n");
    }
    let _ = writeln!(
        out,
        "#[allow(clippy::type_complexity)]\n#[derive(Clone, Debug, PartialEq, Eq)]\npub enum {} {{",
        en.rust_name
    );
    for v in &en.variants {
        if v.fields.is_empty() {
            let _ = writeln!(out, "    {},", v.name);
        } else {
            let _ = write!(out, "    {}(", v.name);
            for (i, fc) in v.fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&codec_field_ty(fc));
            }
            out.push_str("),\n");
        }
    }
    out.push_str("}\n\n");

    let _ = writeln!(out, "impl {} {{", en.rust_name);
    let _ = writeln!(
        out,
        "    #[allow(clippy::too_many_lines)]\n\
         \x20   fn decode(_db: &::tir_egglog::Database, terms: &::tir_egglog::TermDag, term: ::tir_egglog::TermId) -> DecodeResult<Self> {{\n\
         \x20       let invalid = || -> Box<dyn std::error::Error> {{ concat!(\"invalid engine term for \", stringify!({})).into() }};\n\
         \x20       match terms.get(term) {{\n\
         \x20           ::tir_egglog::Term::App(tag, args) => match (tag.as_str(), args.as_slice()) {{",
        en.rust_name
    );
    for v in &en.variants {
        if v.fields.is_empty() {
            let _ = writeln!(
                out,
                "                ({:?}, []) => Ok(Self::{}),",
                v.egg_ctor, v.name
            );
        } else {
            let _ = write!(out, "                ({:?}, [", v.egg_ctor);
            for i in 0..v.fields.len() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "arg_{i}");
            }
            let _ = write!(out, "]) => Ok(Self::{}(", v.name);
            for (i, fc) in v.fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&codec_decode_term(fc, &format!("arg_{i}")));
            }
            out.push_str(")),\n");
        }
    }
    out.push_str("                _ => Err(invalid()),\n            },\n            _ => Err(invalid()),\n        }\n    }\n}\n\n");
}

fn emit_enum_program_methods(en: &MlEqEnum, out: &mut String) {
    let ename = &en.rust_name;
    let snake_en = to_snake(ename);
    for v in &en.variants {
        let short = format!("{snake_en}_{}", to_snake(&v.name));
        let lookup = format!("lookup_{short}");
        let rows = format!("rows_{short}");
        let by_res = format!("index_{short}_by_result");
        let res_col = v.fields.len();

        let mut field_tys = String::new();
        let mut params = String::new();
        let mut enc_args = String::new();
        let mut dec_args = String::new();
        let mut row_binds = String::new();
        for (i, fc) in v.fields.iter().enumerate() {
            let ty = codec_class_ty(fc);
            let _ = write!(field_tys, "{ty}, ");
            if i > 0 {
                params.push_str(", ");
                enc_args.push_str(", ");
            }
            let _ = write!(params, "arg_{i}: {ty}");
            enc_args.push_str(&codec_encode(fc, "db", &format!("arg_{i}")));
            let _ = write!(row_binds, "let arg_{i} = row[{i}]; ");
            let _ = write!(
                dec_args,
                "{}, ",
                codec_decode_base(fc, "db", &format!("arg_{i}"))
            );
        }

        let _ = writeln!(
            out,
            "    pub fn {by_res}(&mut self) -> Result<Index<Class<{ename}>, ({field_tys}Class<{ename}>,)>, ::tir_egglog::Error> {{\n\
             \x20       let table = self.db.table({:?})?;\n\
             \x20       Ok(Index {{\n\
             \x20           index: self.db.add_kept_index(table, &[{res_col}], &[]),\n\
             \x20           database: self.db.id(),\n\
             \x20           key: |db, key| key.value_for(db.id()),\n\
             \x20           decode: |db, row| {{ {row_binds}Ok(({dec_args}Class::new(db, row[{res_col}]),)) }},\n\
             \x20       }})\n\
             \x20   }}",
            v.egg_ctor
        );
        let _ = writeln!(
            out,
            "    pub fn {lookup}(&self, {params}) -> Result<Option<Class<{ename}>>, ::tir_egglog::Error> {{\n\
             \x20       let db = &*self.db;\n\
             \x20       let table = db.table({:?})?;\n\
             \x20       Ok(db.lookup(table, &[{enc_args}]).map(|value| Class::new(db, value)))\n\
             \x20   }}",
            v.egg_ctor
        );
        let _ = writeln!(
            out,
            "    pub fn {rows}(&self) -> Result<impl Iterator<Item = DecodeResult<({field_tys}Class<{ename}>,)>> + '_, ::tir_egglog::Error> {{\n\
             \x20       let db = &*self.db;\n\
             \x20       let table = db.table({:?})?;\n\
             \x20       Ok(db.rows(table).map(move |row| {{ {row_binds}Ok(({dec_args}Class::new(db, row[{res_col}]),)) }}))\n\
             \x20   }}",
            v.egg_ctor
        );
    }
}

fn emit_extern_binder(ext: &MlExtern, out: &mut String) {
    if ext
        .inputs
        .iter()
        .chain(std::iter::once(&ext.output))
        .any(|(c, _)| matches!(c, MlCodec::Enum { .. }))
    {
        return;
    }
    let fn_name = format!("bind_{}", ext.rust_name);
    let mut in_tys = String::new();
    let mut in_sorts = String::new();
    let mut arg_pats = String::new();
    let mut dec_args = String::new();
    for (i, (c, s)) in ext.inputs.iter().enumerate() {
        if i > 0 {
            in_tys.push_str(", ");
            in_sorts.push_str(", ");
            arg_pats.push_str(", ");
            dec_args.push_str(", ");
        }
        in_tys.push_str(&codec_rust_ty(c));
        let _ = write!(in_sorts, "db.sort({s:?})?");
        let _ = write!(arg_pats, "arg_{i}");
        dec_args.push_str(&codec_decode_prim(c, "p", &format!("arg_{i}")));
    }
    let out_ty = codec_rust_ty(&ext.output.0);
    let out_sort = &ext.output.1;
    let enc_out = codec_encode_prim(&ext.output.0, "p", "output");
    let _ = writeln!(
        out,
        "#[allow(clippy::type_complexity)]\n\
         pub fn {fn_name}(\n\
         \x20   db: &mut ::tir_egglog::Database,\n\
         \x20   implementation: impl Fn(&::tir_egglog::PrimCtx<'_>, {in_tys}) -> Option<{out_ty}> + Send + Sync + 'static,\n\
         ) -> Result<(), ::tir_egglog::Error> {{\n\
         \x20   let signature = ::tir_egglog::Signature {{ inputs: vec![{in_sorts}], output: db.sort({out_sort:?})? }};\n\
         \x20   db.add_primitive({:?}, signature, move |p, values| {{\n\
         \x20       let [{arg_pats}] = values else {{ return None; }};\n\
         \x20       let output = implementation(p, {dec_args})?;\n\
         \x20       Some({enc_out})\n\
         \x20   }});\n\
         \x20   Ok(())\n\
         }}\n",
        ext.engine_name
    );
}

fn emit_linked_ops(ops: &[MlLinkedOp], out: &mut String) {
    for (i, op) in ops.iter().enumerate() {
        let sname = format!("__CdslLinkedOp{i}");
        let ports = match op.ports {
            Some((a, b)) => format!("Some(({a}usize, {b}usize))"),
            None => "None".to_owned(),
        };
        let _ = writeln!(
            out,
            "struct {sname};\n\
             impl ::tir_egglog::NativeDeclaration for {sname} {{\n\
             \x20   const NAME: &'static str = \"{}.{}\";\n\
             \x20   const SYMBOL: &'static str = {:?};\n\
             \x20   const PORTS: Option<(usize, usize)> = {ports};\n\
             }}",
            op.dialect, op.name, op.symbol
        );
    }
}

fn emit_table_aliases_and_methods(
    tbl: &MlTable,
    field_idx: usize,
    aliases_out: &mut String,
    methods_out: &mut String,
) {
    let tname = &tbl.rust_name;
    let pascal = pascal_from_snake(tname);
    let row_ty = format!("{pascal}Row");
    let class_row_ty = format!("{pascal}ClassRow");
    let has_enum = tbl
        .cols
        .iter()
        .any(|c| matches!(c.codec, MlCodec::Enum { .. }));
    let mut row_cols = String::new();
    let mut class_cols = String::new();
    for c in &tbl.cols {
        let _ = write!(row_cols, "{}, ", codec_rust_ty(&c.codec));
        let _ = write!(class_cols, "{}, ", codec_class_ty(&c.codec));
    }
    let _ = writeln!(
        aliases_out,
        "#[allow(clippy::type_complexity)]\npub type {row_ty} = ({row_cols});"
    );
    if has_enum {
        let _ = writeln!(
            aliases_out,
            "#[allow(clippy::type_complexity)]\npub type {class_row_ty} = ({class_cols});"
        );
    }
    let effective_class_row = if has_enum { &class_row_ty } else { &row_ty };

    let tf = format!("table_{field_idx}");
    let reader = escape_rust_ident(tname);
    let in_color = format!("{tname}_in");
    let insert = format!("insert_{tname}");
    let insert_in = format!("insert_{tname}_in");

    let mut arg_names = String::new();
    let mut row_binds = String::new();
    let mut dec_cells = String::new();
    let mut dec_classes = String::new();
    let mut enc_vals = String::new();
    for (i, c) in tbl.cols.iter().enumerate() {
        let _ = write!(arg_names, "col_{i}, ");
        let _ = write!(row_binds, "let col_{i} = row[{i}]; ");
        let _ = write!(
            dec_cells,
            "{}, ",
            codec_decode_cell(&c.codec, "db", &format!("col_{i}"), None)
        );
        let _ = write!(
            dec_classes,
            "{}, ",
            codec_decode_base(&c.codec, "db", &format!("col_{i}"))
        );
        let _ = write!(
            enc_vals,
            "{}, ",
            codec_encode(&c.codec, "db", &format!("col_{i}"))
        );
    }

    // Indexes for every named column
    for (ci, c) in tbl.cols.iter().enumerate() {
        if let Some(cname) = &c.name {
            let imethod = format!("index_{tname}_by_{}", to_snake(cname));
            let kty = codec_class_ty(&c.codec);
            let kenc = match &c.codec {
                MlCodec::Enum { .. } => "key.value_for(db.id())".to_owned(),
                other => format!("Ok({})", codec_encode(other, "db", "key")),
            };
            let _ = writeln!(
                methods_out,
                "    pub fn {imethod}(&mut self) -> Index<{kty}, {effective_class_row}> {{\n\
                 \x20       Index {{\n\
                 \x20           index: self.db.add_kept_index(self.{tf}, &[{ci}], &[]),\n\
                 \x20           database: self.db.id(),\n\
                 \x20           key: |db, key| {kenc},\n\
                 \x20           decode: |db, row| {{ {row_binds}Ok(({dec_classes})) }},\n\
                 \x20       }}\n\
                 \x20   }}"
            );
        }
    }

    let extractor_decl = if has_enum {
        "let extractor = std::cell::OnceCell::new(); "
    } else {
        ""
    };
    let _ = writeln!(
        methods_out,
        "    pub fn {reader}(&self) -> impl Iterator<Item = DecodeResult<{row_ty}>> + '_ {{ self.{in_color}(::tir_egglog::Color::BASE) }}"
    );
    let _ = writeln!(
        methods_out,
        "    pub fn {in_color}(&self, color: ::tir_egglog::Color) -> impl Iterator<Item = DecodeResult<{row_ty}>> + '_ {{\n\
         \x20       let db = &*self.db;\n\
         \x20       {extractor_decl}db.rows_in(color, self.{tf}).filter(|r| !r.is_subsumed()).map(move |row| {{ {row_binds}Ok(({dec_cells})) }})\n\
         \x20   }}"
    );
    if has_enum {
        let handles = format!("{tname}_classes");
        let _ = writeln!(
            methods_out,
            "    pub fn {handles}(&self, color: ::tir_egglog::Color) -> impl Iterator<Item = DecodeResult<{class_row_ty}>> + '_ {{\n\
             \x20       let db = &*self.db;\n\
             \x20       db.rows_in(color, self.{tf}).filter(|r| !r.is_subsumed()).map(move |row| {{ {row_binds}Ok(({dec_classes})) }})\n\
             \x20   }}"
        );
    }
    let _ = writeln!(
        methods_out,
        "    pub fn {insert}(&mut self, row: {effective_class_row}) -> Result<(), ::tir_egglog::Error> {{ self.{insert_in}(::tir_egglog::Color::BASE, row) }}"
    );
    let _ = writeln!(
        methods_out,
        "    pub fn {insert_in}(&mut self, color: ::tir_egglog::Color, ({arg_names}): {effective_class_row}) -> Result<(), ::tir_egglog::Error> {{\n\
         \x20       let db = &mut *self.db;\n\
         \x20       let row = [{enc_vals}];\n\
         \x20       db.insert_in(color, self.{tf}, &row)\n\
         \x20   }}"
    );
}

fn emit_ml_val_ty(ty: &MlValueTy) -> String {
    match ty {
        MlValueTy::Unit => "()".to_owned(),
        MlValueTy::Scalar(c) => codec_class_ty(c),
        MlValueTy::Tuple(ts) => {
            let mut s = String::from("(");
            for t in ts {
                s.push_str(&emit_ml_val_ty(t));
                s.push_str(", ");
            }
            s.push(')');
            s
        }
    }
}

fn emit_memo_expr(
    sig: &Signature,
    e: &MlExpr,
    query_tables: &[(String, String, Vec<MlCodec>)],
    out: &mut String,
) {
    match e {
        MlExpr::Erased | MlExpr::Lit(Lit::Unit) => out.push_str("()"),
        MlExpr::Var(x) => out.push_str(&escape_rust_ident(sig.interner.resolve(*x))),
        MlExpr::Global(g) => {
            let name = sig.interner.resolve(*g);
            let leaf = name.rsplit("::").next().unwrap_or(name);
            out.push_str(&escape_rust_ident(leaf));
        }
        MlExpr::Lit(Lit::Int(n)) => {
            let _ = write!(out, "{n}i64");
        }
        MlExpr::Lit(Lit::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
        MlExpr::Lit(Lit::Str(s)) => {
            let _ = write!(out, "String::from({s:?})");
        }
        MlExpr::Prim(op, args) => match (op, args.as_slice()) {
            (PrimOp::Not, [a]) => {
                out.push_str("(!(");
                emit_memo_expr(sig, a, query_tables, out);
                out.push_str("))");
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
                    _ => "==",
                };
                out.push('(');
                emit_memo_expr(sig, a, query_tables, out);
                let _ = write!(out, " {sym} ");
                emit_memo_expr(sig, b, query_tables, out);
                out.push(')');
            }
            _ => out.push_str("()"),
        },
        MlExpr::Let(x, rhs, body) => {
            let xs = sig.interner.resolve(*x);
            out.push_str("{ let ");
            out.push_str(&escape_rust_ident(xs));
            out.push_str(" = ");
            emit_memo_expr(sig, rhs, query_tables, out);
            out.push_str("; ");
            emit_memo_expr(sig, body, query_tables, out);
            out.push_str(" }");
        }
        MlExpr::Rec(fs) => {
            out.push('(');
            for (_, v) in fs {
                emit_memo_expr(sig, v, query_tables, out);
                out.push_str(", ");
            }
            out.push(')');
        }
        MlExpr::Proj(obj, f) => {
            let fs = sig.interner.resolve(*f);
            out.push('(');
            emit_memo_expr(sig, obj, query_tables, out);
            out.push(')');
            if let Some(idx) = fs.strip_prefix('_').and_then(|r| r.parse::<usize>().ok()) {
                let _ = write!(out, ".{idx}");
            } else {
                let _ = write!(out, ".{fs}");
            }
        }
        MlExpr::Match(scrut, branches) => {
            out.push_str("match ");
            emit_memo_expr(sig, scrut, query_tables, out);
            out.push_str(" { ");
            for (pat, rhs) in branches {
                match pat {
                    crate::core::Pattern::Wild => out.push('_'),
                    crate::core::Pattern::Var(x) => {
                        out.push_str(&escape_rust_ident(sig.interner.resolve(*x)))
                    }
                    crate::core::Pattern::Lit(Lit::Int(n)) => {
                        let _ = write!(out, "{n}i64");
                    }
                    crate::core::Pattern::Lit(Lit::Bool(b)) => {
                        out.push_str(if *b { "true" } else { "false" })
                    }
                    crate::core::Pattern::Lit(Lit::Str(s)) => {
                        let _ = write!(out, "{s:?}");
                    }
                    _ => out.push('_'),
                }
                out.push_str(" => ");
                emit_memo_expr(sig, rhs, query_tables, out);
                out.push_str(", ");
            }
            out.push('}');
        }
        MlExpr::App(_, _) => {
            // Check if this is a captured query table lookup `Scratch(arg0, arg1)`
            let mut head = e;
            let mut args = Vec::new();
            while let MlExpr::App(f, a) = head {
                args.push(a.as_ref());
                head = f.as_ref();
            }
            args.reverse();
            let head_name = match head {
                MlExpr::Global(g) => Some(sig.interner.resolve(*g)),
                MlExpr::Var(v) => Some(sig.interner.resolve(*v)),
                _ => None,
            };
            if let Some(hn) = head_name
                && (hn == "has_row" || hn.ends_with("::has_row"))
            {
                let live_args: Vec<&MlExpr> = args
                    .iter()
                    .copied()
                    .filter(|a| !matches!(a, MlExpr::Erased))
                    .collect();
                if let [rel_arg, tup_arg] = live_args.as_slice() {
                    let rel_name = match rel_arg {
                        MlExpr::Global(g) => Some(sig.interner.resolve(*g)),
                        MlExpr::Var(v) => Some(sig.interner.resolve(*v)),
                        _ => None,
                    };
                    if let Some(rn) = rel_name
                        && let Some((field, _, codecs)) = query_tables.iter().find(|(f, eng, _)| {
                            eng == rn || rn.ends_with(&format!("::{eng}")) || f == rn
                        })
                    {
                        let elem_exprs: Vec<&MlExpr> = match tup_arg {
                            MlExpr::Rec(fs) => fs.iter().map(|(_, v)| v).collect(),
                            other => vec![other],
                        };
                        let fident = escape_rust_ident(&to_snake(field));
                        out.push_str("{ let __row = [");
                        for (i, (a, c)) in elem_exprs.iter().zip(codecs).enumerate() {
                            if i > 0 {
                                out.push_str(", ");
                            }
                            let mut a_s = String::new();
                            emit_memo_expr(sig, a, query_tables, &mut a_s);
                            out.push_str(&codec_encode(c, "db", &a_s));
                        }
                        let _ = write!(
                            out,
                            "]; self.{fident}.rows().any(|__r| !__r.is_subsumed() && __r.values().zip(__row.iter()).all(|(__a, __b)| self.{fident}.find(__a) == self.{fident}.find(*__b))) }}"
                        );
                        return;
                    }
                }
            }
            if let Some(hn) = head_name
                && let Some((field, _, codecs)) = query_tables
                    .iter()
                    .find(|(f, eng, _)| eng == hn || hn.ends_with(&format!("::{eng}")) || f == hn)
            {
                let fident = escape_rust_ident(&to_snake(field));
                out.push_str("{ let __row = [");
                for (i, (a, c)) in args.iter().zip(codecs).enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let mut a_s = String::new();
                    emit_memo_expr(sig, a, query_tables, &mut a_s);
                    out.push_str(&codec_encode(c, "db", &a_s));
                }
                let _ = write!(
                    out,
                    "]; self.{fident}.rows().any(|__r| !__r.is_subsumed() && __r.values().zip(__row.iter()).all(|(__a, __b)| self.{fident}.find(__a) == self.{fident}.find(*__b))) }}"
                );
                return;
            }
            emit_memo_expr(sig, head, query_tables, out);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_memo_expr(sig, a, query_tables, out);
            }
            out.push(')');
        }
        _ => out.push_str("()"),
    }
}

#[allow(clippy::cognitive_complexity)]
fn emit_program_memo(sig: &Signature, memo: &MlProgramMemo, out: &mut String) {
    let fname = &memo.rust_name;
    let fname_in = format!("{fname}_in");
    let pascal = pascal_from_snake(fname);
    let res_struct = format!("{pascal}Result");
    let sched_fn = format!("frame_schedule_{fname}");
    let memo_key = format!("@cdsl:memo:{fname}");
    let scoped_root = matches!(memo.params.first(), Some((_, MlCodec::OpId)));

    // Schedule helper
    let _ = write!(out, "fn {sched_fn}() -> ::tir_egglog::Schedule {{ ");
    emit_schedule_expr(&memo.schedule, out);
    out.push_str(" }\n\n");

    let ret_ty = match &memo.body {
        MlMemoBody::Tables { outputs } => {
            if outputs.is_empty() {
                "()".to_owned()
            } else {
                // Emit result struct + row types + bindings() + table_*() + readers
                for (field, _, codecs) in outputs {
                    let row_nm = format!("{res_struct}{}Row", pascal_from_snake(&to_snake(field)));
                    let mut ctys = String::new();
                    for c in codecs {
                        let _ = write!(ctys, "{}, ", codec_rust_ty(c));
                    }
                    let _ = writeln!(out, "pub type {row_nm} = ({ctys});");
                }
                let _ = writeln!(out, "#[derive(Clone)]\npub struct {res_struct} {{");
                let _ = writeln!(out, "    color: ::tir_egglog::Color,");
                for (field, _, _) in outputs {
                    let fi = escape_rust_ident(&to_snake(field));
                    let _ = writeln!(out, "    {fi}: ::tir_egglog::LocalTable,");
                }
                out.push_str("}\n\n");
                let _ = writeln!(
                    out,
                    "#[allow(unused_parens, clippy::double_parens)]\nimpl {res_struct} {{"
                );
                let _ = writeln!(
                    out,
                    "    pub fn bindings(&self) -> [(&'static str, &::tir_egglog::LocalTable); {}] {{",
                    outputs.len()
                );
                out.push_str("        [");
                for (i, (field, eng, _)) in outputs.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let fi = escape_rust_ident(&to_snake(field));
                    let _ = write!(out, "({eng:?}, &self.{fi})");
                }
                out.push_str("]\n    }\n");
                for (field, _, codecs) in outputs {
                    let fi = escape_rust_ident(&to_snake(field));
                    let row_nm = format!("{res_struct}{}Row", pascal_from_snake(&to_snake(field)));
                    let _ = writeln!(
                        out,
                        "    pub fn table_{fi}(&self) -> &::tir_egglog::LocalTable {{ &self.{fi} }}"
                    );
                    let has_enum = codecs.iter().any(|c| matches!(c, MlCodec::Enum { .. }));
                    let ext_decl = if has_enum {
                        "let extractor = std::cell::OnceCell::new(); "
                    } else {
                        ""
                    };
                    let mut binds = String::new();
                    let mut decs = String::new();
                    for (ci, c) in codecs.iter().enumerate() {
                        let _ = write!(binds, "let col_{ci} = row[{ci}]; ");
                        let _ = write!(
                            decs,
                            "{}, ",
                            codec_decode_cell(c, "db", &format!("col_{ci}"), None)
                        );
                    }
                    let _ = writeln!(
                        out,
                        "    pub fn {fi}<'a>(&self, db: &'a ::tir_egglog::Database) -> Result<impl Iterator<Item = DecodeResult<{row_nm}>> + use<'a>, ::tir_egglog::Error> {{\n\
                         \x20       let table = self.{fi}.id(db)?;\n\
                         \x20       let color = self.color;\n\
                         \x20       Ok({{ {ext_decl}db.rows_in(color, table).filter(|r| !r.is_subsumed()).map(move |row| {{ {binds}Ok(({decs})) }}) }})\n\
                         \x20   }}"
                    );
                }
                out.push_str("}\n\n");
                res_struct.clone()
            }
        }
        MlMemoBody::Query {
            tables,
            query_params,
            query_expr,
        } => {
            for (field, _, codecs) in tables {
                let row_nm = format!("{res_struct}{}Row", pascal_from_snake(&to_snake(field)));
                let mut ctys = String::new();
                for c in codecs {
                    let _ = write!(ctys, "{}, ", codec_rust_ty(c));
                }
                let _ = writeln!(out, "pub type {row_nm} = ({ctys});");
            }
            let _ = writeln!(out, "#[derive(Clone)]\npub struct {res_struct} {{");
            for (field, _, _) in tables {
                let fi = escape_rust_ident(&to_snake(field));
                let _ = writeln!(out, "    {fi}: ::tir_egglog::CapturedTable,");
            }
            for (px, pc) in &memo.params {
                let pn = escape_rust_ident(sig.interner.resolve(*px));
                let _ = writeln!(out, "    __cap_{pn}: {},", codec_class_ty(pc));
            }
            out.push_str("}\n\n");
            let _ = writeln!(
                out,
                "#[allow(unused_parens, clippy::double_parens)]\nimpl {res_struct} {{"
            );
            for (field, _, codecs) in tables {
                let fi = escape_rust_ident(&to_snake(field));
                let row_nm = format!("{res_struct}{}Row", pascal_from_snake(&to_snake(field)));
                let mut binds = String::new();
                let mut decs = String::new();
                for (ci, c) in codecs.iter().enumerate() {
                    let _ = write!(binds, "let col_{ci} = row[{ci}]; ");
                    let _ = write!(
                        decs,
                        "{}, ",
                        codec_decode_cell(
                            c,
                            "db",
                            &format!("col_{ci}"),
                            Some(&format!("self.{fi}"))
                        )
                    );
                }
                let _ = writeln!(
                    out,
                    "    pub fn {fi}<'a>(&'a self, db: &'a ::tir_egglog::Database) -> Result<impl Iterator<Item = DecodeResult<{row_nm}>> + use<'a>, ::tir_egglog::Error> {{\n\
                     \x20       self.{fi}.table().id(db)?;\n\
                     \x20       Ok(self.{fi}.rows().filter(|r| !r.is_subsumed()).map(move |row| {{ {binds}Ok(({decs})) }}))\n\
                     \x20   }}"
                );
            }
            // If first table is named `rows` or there is a single/first table, also provide `.rows(db)` if not already named `rows`
            if let Some((first_f, _, _)) = tables.first()
                && to_snake(first_f) != "rows"
            {
                let fi = escape_rust_ident(&to_snake(first_f));
                let row_nm = format!("{res_struct}{}Row", pascal_from_snake(&to_snake(first_f)));
                let _ = writeln!(
                    out,
                    "    pub fn rows<'a>(&'a self, db: &'a ::tir_egglog::Database) -> Result<impl Iterator<Item = DecodeResult<{row_nm}>> + use<'a>, ::tir_egglog::Error> {{ self.{fi}(db) }}"
                );
            }
            // `.contains(db, qparams...)`
            out.push_str("    pub fn contains(&self, db: &::tir_egglog::Database");
            for (qx, qc) in query_params {
                let qn = escape_rust_ident(sig.interner.resolve(*qx));
                let _ = write!(out, ", {qn}: {}", codec_class_ty(qc));
            }
            out.push_str(") -> Result<bool, ::tir_egglog::Error> {\n");
            for (px, _) in &memo.params {
                let pn = escape_rust_ident(sig.interner.resolve(*px));
                let _ = writeln!(out, "        let {pn} = self.__cap_{pn};");
            }
            out.push_str("        Ok(");
            emit_memo_expr(sig, query_expr, tables, out);
            out.push_str(")\n    }\n}\n\n");
            res_struct.clone()
        }
        MlMemoBody::Value { ret_ty, .. } => emit_ml_val_ty(ret_ty),
    };

    // Emit both `pub fn <fname>(db, ...)` and `pub fn <fname>_in(db, color, ...)`
    let mut param_sig = String::new();
    let mut param_fwd = String::new();
    let mut arg_enc = String::new();
    if scoped_root {
        param_sig.push_str(", revision: u64");
        param_fwd.push_str(", revision");
    }
    for (px, pc) in &memo.params {
        let pn = escape_rust_ident(sig.interner.resolve(*px));
        let pty = codec_class_ty(pc);
        let _ = write!(param_sig, ", {pn}: {pty}");
        let _ = write!(param_fwd, ", {pn}");
        let _ = write!(arg_enc, "{}, ", codec_encode(pc, "db", &pn));
    }

    let _ = writeln!(
        out,
        "pub fn {fname}(db: &mut ::tir_egglog::Database{param_sig}) -> DecodeResult<{ret_ty}> {{\n\
         \x20   {fname_in}(db, ::tir_egglog::Color::BASE{param_fwd})\n\
         }}\n"
    );

    let _ = writeln!(
        out,
        "#[allow(unused_parens, clippy::clone_on_copy, clippy::double_parens)]\npub fn {fname_in}(db: &mut ::tir_egglog::Database, color: ::tir_egglog::Color{param_sig}) -> DecodeResult<{ret_ty}> {{"
    );
    let _ = writeln!(out, "    install_schema(db)?;");
    let _ = writeln!(out, "    db.rebuild()?;");
    let _ = writeln!(out, "    let args = [{arg_enc}];");
    let stamp_mut = if memo.reads.is_empty() { "" } else { "mut " };
    if scoped_root {
        let _ = writeln!(
            out,
            "    let {stamp_mut}stamp = vec![revision, db.equality_revision(color)];"
        );
    } else {
        let _ = writeln!(
            out,
            "    let {stamp_mut}stamp = vec![db.equality_revision(color)];"
        );
    }
    if !memo.reads.is_empty() {
        out.push_str("    stamp.extend([");
        for (i, r) in memo.reads.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{r:?}");
        }
        out.push_str(
            "].into_iter().map(|name| db.table(name).map_or(0, |t| db.table_version(t))));\n",
        );
    }
    let _ = writeln!(
        out,
        "    if let Some(cached) = db.memo_result::<{ret_ty}>({memo_key:?}, &args, color, &stamp) {{\n\
         \x20       return Ok(cached);\n\
         \x20   }}"
    );

    match &memo.body {
        MlMemoBody::Tables { outputs } => {
            out.push_str("    static FRAME_BYTES: &[u8] = ");
            emit_byte_literal(&memo.bytes, out);
            out.push_str(";\n");
            let locals_lit: Vec<String> = memo.locals.iter().map(|s| format!("{s:?}")).collect();
            let _ = writeln!(
                out,
                "    let (mut prog, tables) = ::tir_egglog::Program::from_static_with_tables(FRAME_BYTES, db, &[{}], &[])\n\
                 \x20       .map_err(|e| ::tir_egglog::Error::Unsupported(e.to_string()))?;",
                locals_lit.join(", ")
            );
            let _ = writeln!(out, "    prog.run_in(db, color, &{sched_fn}(), &args)?;");
            if outputs.is_empty() {
                let _ = writeln!(
                    out,
                    "    drop(prog);\n    drop(tables);\n    db.collect_local_tables();"
                );
                let _ = writeln!(
                    out,
                    "    db.store_memo_result({memo_key:?}, &args, color, stamp, ());\n    Ok(())"
                );
            } else {
                let _ = writeln!(out, "    let result = {res_struct} {{ color,");
                for (field, eng, _) in outputs {
                    let fi = escape_rust_ident(&to_snake(field));
                    let slot = memo.locals.iter().position(|l| l == eng).unwrap_or(0);
                    let _ = writeln!(out, "        {fi}: tables[{slot}usize].clone(),");
                }
                out.push_str("    };\n");
                let _ = writeln!(
                    out,
                    "    db.store_memo_result({memo_key:?}, &args, color, stamp, result.clone());\n    Ok(result)"
                );
            }
        }
        MlMemoBody::Query { tables, .. } => {
            out.push_str("    static FRAME_BYTES: &[u8] = ");
            emit_byte_literal(&memo.bytes, out);
            out.push_str(";\n");
            let locals_lit: Vec<String> = memo.locals.iter().map(|s| format!("{s:?}")).collect();
            let _ = writeln!(
                out,
                "    let (mut prog, tables) = ::tir_egglog::Program::from_static_with_tables(FRAME_BYTES, db, &[{}], &[])\n\
                 \x20       .map_err(|e| ::tir_egglog::Error::Unsupported(e.to_string()))?;",
                locals_lit.join(", ")
            );
            let _ = writeln!(out, "    prog.run_in(db, color, &{sched_fn}(), &args)?;");
            let _ = writeln!(out, "    let result = {res_struct} {{");
            for (field, eng, codecs) in tables {
                let fi = escape_rust_ident(&to_snake(field));
                let slot = memo.locals.iter().position(|l| l == eng).unwrap_or(0);
                let enum_cols: Vec<String> = codecs
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| matches!(c, MlCodec::Enum { .. }))
                    .map(|(i, _)| format!("{i}usize"))
                    .collect();
                let _ = writeln!(
                    out,
                    "        {fi}: tables[{slot}usize].capture_with_terms(db, color, &[{}])?,",
                    enum_cols.join(", ")
                );
            }
            for (px, _) in &memo.params {
                let pn = escape_rust_ident(sig.interner.resolve(*px));
                let _ = writeln!(out, "        __cap_{pn}: {pn},");
            }
            out.push_str("    };\n");
            let _ = writeln!(
                out,
                "    db.store_memo_result({memo_key:?}, &args, color, stamp, result.clone());\n    Ok(result)"
            );
        }
        MlMemoBody::Value { expr, .. } => {
            if !memo.locals.is_empty() {
                out.push_str("    static FRAME_BYTES: &[u8] = ");
                emit_byte_literal(&memo.bytes, out);
                out.push_str(";\n");
                let locals_lit: Vec<String> =
                    memo.locals.iter().map(|s| format!("{s:?}")).collect();
                let _ = writeln!(
                    out,
                    "    let (mut prog, tables) = ::tir_egglog::Program::from_static_with_tables(FRAME_BYTES, db, &[{}], &[])\n\
                     \x20       .map_err(|e| ::tir_egglog::Error::Unsupported(e.to_string()))?;",
                    locals_lit.join(", ")
                );
                let _ = writeln!(out, "    prog.run_in(db, color, &{sched_fn}(), &args)?;");
                let _ = writeln!(
                    out,
                    "    drop(prog);\n    drop(tables);\n    db.collect_local_tables();"
                );
            }
            out.push_str("    let result = ");
            emit_memo_expr(sig, expr, &[], out);
            out.push_str(";\n");
            let _ = writeln!(
                out,
                "    db.store_memo_result({memo_key:?}, &args, color, stamp, result.clone());\n    Ok(result)"
            );
        }
    }
    out.push_str("}\n\n");
}

fn emit_schedule_items(s: &MlScheduleDecl, top_out: &mut String, methods_out: &mut String) {
    let sname = &s.rust_name;
    let sched_fn = format!("schedule_{sname}");
    let run_fn = format!("run_{sname}");
    let method_name = escape_rust_ident(sname);

    let _ = write!(top_out, "pub fn {sched_fn}() -> ::tir_egglog::Schedule {{ ");
    emit_schedule_expr(&s.schedule, top_out);
    top_out.push_str(" }\n\n");

    let _ = writeln!(
        top_out,
        "pub fn {run_fn}(engine: &mut ::tir_egglog::EGraph) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error> {{\n\
         \x20   engine.run_schedule(&{sched_fn}())\n\
         }}\n"
    );

    let _ = writeln!(
        methods_out,
        "    pub fn {method_name}(&mut self) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error> {{\n\
         \x20       self.code.run(self.db, &{sched_fn}(), &[])\n\
         \x20   }}"
    );
}

fn emit_collection(c: &MlRuleCollection, out: &mut String) {
    let _ = writeln!(
        out,
        "pub fn {}() -> ::tir_egglog::RuleCollection {{\n\
         \x20   ::tir_egglog::RuleCollection::from_static(CODE, {:?}, __install_schema_load)\n\
         }}\n",
        c.rust_name, c.ruleset_name
    );
}

pub fn emit_program_module(
    sig: &Signature,
    ml: &MlProgram,
    _strip_prefix: Option<&str>,
    catalog_only: bool,
) -> String {
    let Some(MlDecl::ProgramBundle(bundle)) = ml
        .decls
        .iter()
        .find(|d| matches!(d, MlDecl::ProgramBundle(_)))
    else {
        return String::new();
    };
    emit_bundle(sig, bundle, catalog_only)
}

fn emit_bundle(sig: &Signature, b: &MlProgramBundle, catalog_only: bool) -> String {
    let mut out = String::with_capacity(8192);
    emit_prelude(&mut out);

    // 1. Eq-sort enums
    for en in &b.enums {
        emit_eq_enum(en, &mut out);
    }

    // 2. Linked native operation declarations
    emit_linked_ops(&b.linked_ops, &mut out);

    // 3. Static CATALOG bytes + install_schema
    out.push_str("static CATALOG: &[u8] = ");
    emit_byte_literal(&b.catalog_bytes, &mut out);
    out.push_str(";\n\n");
    out.push_str(
        "#[allow(clippy::cognitive_complexity)]\npub fn install_schema(db: &mut ::tir_egglog::Database) -> DecodeResult<()> {\n\
         \x20   let _ = ::tir_egglog::Program::from_static(CATALOG, db)?;\n",
    );
    for (i, op) in b.linked_ops.iter().enumerate() {
        let _ = writeln!(
            out,
            "    if db.declaration({:?}).is_none() {{ db.bind_declaration::<__CdslLinkedOp{i}>()?; }}",
            op.symbol
        );
    }
    out.push_str("    Ok(())\n}\n\n");
    out.push_str(
        "#[allow(dead_code, clippy::cognitive_complexity)]\nfn __install_schema_load(db: &mut ::tir_egglog::Database) -> Result<(), ::tir_egglog::LoadError> {\n\
         \x20   let _ = ::tir_egglog::Program::from_static(CATALOG, db)?;\n",
    );
    for (i, op) in b.linked_ops.iter().enumerate() {
        let _ = writeln!(
            out,
            "    if db.declaration({:?}).is_none() {{ db.bind_declaration::<__CdslLinkedOp{i}>().map_err(|_| ::tir_egglog::LoadError::Malformed(\"native declaration binding failed\"))?; }}",
            op.symbol
        );
    }
    out.push_str("    Ok(())\n}\n\n");

    // 4. Static CODE bytes + attach
    let has_code = !catalog_only && b.code_bytes.is_some();
    if let Some(code_bytes) = &b.code_bytes
        && !catalog_only
    {
        out.push_str("static CODE: &[u8] = ");
        emit_byte_literal(code_bytes, &mut out);
        out.push_str(";\n\n");
        out.push_str(
            "pub fn attach(engine: &mut ::tir_egglog::EGraph) -> Result<(), Box<dyn std::error::Error>> {\n\
             \x20   install_schema(engine.database_mut())?;\n\
             \x20   engine.attach_program(CODE)?;\n\
             \x20   Ok(())\n\
             }\n\n",
        );
    }

    // 5. Extern binders
    for ext in &b.externs {
        emit_extern_binder(ext, &mut out);
    }

    // 6. Table accessors + row aliases + Program methods
    let mut row_aliases = String::new();
    let mut prog_methods = String::new();
    let exported_tables: Vec<&MlTable> = b.tables.iter().filter(|t| t.exported).collect();

    for (idx, tbl) in exported_tables.iter().enumerate() {
        let _ = writeln!(
            out,
            "pub fn table_{}(db: &::tir_egglog::Database) -> Result<::tir_egglog::TableId, ::tir_egglog::Error> {{ db.table({:?}) }}",
            tbl.rust_name, tbl.engine_name
        );
        emit_table_aliases_and_methods(tbl, idx, &mut row_aliases, &mut prog_methods);
    }
    out.push('\n');
    out.push_str(&row_aliases);
    out.push('\n');

    // 7. Rule collections
    if has_code {
        for c in &b.collections {
            emit_collection(c, &mut out);
        }
    }

    // 8. Schedules
    let mut top_schedules = String::new();
    if has_code {
        for s in &b.schedules {
            emit_schedule_items(s, &mut top_schedules, &mut prog_methods);
        }
    }
    out.push_str(&top_schedules);

    // 9. Memos
    if !catalog_only {
        for memo in &b.memos {
            emit_program_memo(sig, memo, &mut out);
        }
        // 9b. CDSL pass driver functions
        for f in &b.fns {
            crate::emit_rust::emit_driver_fn(sig, f.qname, &f.rust_name, &f.body, &mut out);
        }
    }

    // 10. Program<'db> struct (emitted when not catalog_only)
    if !catalog_only {
        out.push_str("pub struct Program<'db> {\n    db: &'db mut ::tir_egglog::Database,\n");
        if has_code {
            out.push_str("    code: ::tir_egglog::Program,\n");
        }
        for idx in 0..exported_tables.len() {
            let _ = writeln!(out, "    table_{idx}: ::tir_egglog::TableId,");
        }
        out.push_str("}\n\n");

        out.push_str(
            "#[allow(clippy::type_complexity, clippy::cognitive_complexity)]\nimpl<'db> Program<'db> {\n",
        );
        out.push_str(
            "    pub fn load(db: &'db mut ::tir_egglog::Database) -> DecodeResult<Self> {\n\
             \x20       Self::load_with_tables(db, &[])\n\
             \x20   }\n",
        );
        out.push_str(
            "    pub fn load_with_tables(\n\
             \x20       db: &'db mut ::tir_egglog::Database,\n\
             \x20       inputs: &[(&str, &::tir_egglog::LocalTable)],\n\
             \x20   ) -> DecodeResult<Self> {\n\
             \x20       let _ = inputs;\n\
             \x20       install_schema(db)?;\n",
        );
        if has_code {
            out.push_str(
                "        let (code, _) = ::tir_egglog::Program::from_static_with_tables(CODE, db, &[], inputs)?;\n",
            );
        }
        if !exported_tables.is_empty() {
            out.push_str(
                "        let resolve = |name: &str| -> DecodeResult<::tir_egglog::TableId> {\n\
                 \x20           match inputs.iter().find(|(alias, _)| *alias == name) {\n\
                 \x20               Some((_, handle)) => Ok(handle.id(db)?),\n\
                 \x20               None => Ok(db.table(name)?),\n\
                 \x20           }\n\
                 \x20       };\n",
            );
            for (idx, tbl) in exported_tables.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "        let table_{idx} = resolve({:?})?;",
                    tbl.engine_name
                );
            }
        }
        out.push_str("        Ok(Self { db");
        if has_code {
            out.push_str(", code");
        }
        for idx in 0..exported_tables.len() {
            let _ = write!(out, ", table_{idx}");
        }
        out.push_str(" })\n    }\n");

        // Common utility methods
        out.push_str(
            "    pub fn rebuild(&mut self) -> Result<(), ::tir_egglog::Error> { self.db.rebuild() }\n\
             \x20   pub fn color(&mut self, parents: &[::tir_egglog::Color]) -> ::tir_egglog::Color { self.db.color(parents) }\n\
             \x20   pub fn union_in<T>(&mut self, color: ::tir_egglog::Color, a: Class<T>, b: Class<T>) -> Result<(), ::tir_egglog::Error> {\n\
             \x20       let av = a.value_for(self.db.id())?;\n\
             \x20       let bv = b.value_for(self.db.id())?;\n\
             \x20       self.db.union_in(color, av, bv);\n\
             \x20       Ok(())\n\
             \x20   }\n\
             \x20   pub fn listed<'a, K, R>(&'a self, index: &Index<K, R>, key: K) -> Result<impl Iterator<Item = DecodeResult<R>> + use<'a, K, R>, ::tir_egglog::Error> {\n\
             \x20       if index.database != self.db.id() { return Err(::tir_egglog::Error::Unsupported(\"index belongs to a different database\".into())); }\n\
             \x20       let db = &*self.db;\n\
             \x20       let kval = (index.key)(db, key)?;\n\
             \x20       let decode = index.decode;\n\
             \x20       Ok(db.listed(index.index, &[kval]).map(move |row| decode(db, row)))\n\
             \x20   }\n",
        );

        if has_code {
            out.push_str(
                "    pub fn run_schedule(&mut self, schedule: &::tir_egglog::Schedule) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error> {\n\
                 \x20       self.code.run(self.db, schedule, &[])\n\
                 \x20   }\n\
                 \x20   pub fn load_rules(&mut self, collection: &::tir_egglog::RuleCollection) -> Result<(::tir_egglog::Program, &'static str), ::tir_egglog::LoadError> {\n\
                 \x20       collection.load(self.db)\n\
                 \x20   }\n\
                 \x20   pub fn run_rules(&mut self, program: &mut ::tir_egglog::Program, schedule: &::tir_egglog::Schedule) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error> {\n\
                 \x20       program.run(self.db, schedule, &[])\n\
                 \x20   }\n\
                 \x20   pub fn run_schedule_with_after(\n\
                 \x20       &mut self,\n\
                 \x20       schedule: &::tir_egglog::Schedule,\n\
                 \x20       phase: &str,\n\
                 \x20       extras: &mut [(&mut ::tir_egglog::Program, &str)],\n\
                 \x20       after: impl FnMut(&mut ::tir_egglog::Database) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error>,\n\
                 \x20   ) -> Result<::tir_egglog::RunReport, ::tir_egglog::Error> {\n\
                 \x20       self.code.run_with_after(self.db, schedule, &[], phase, extras, after)\n\
                 \x20   }\n",
            );
        }

        // Enum constructor lookup/rows/index_by_result methods
        for en in &b.enums {
            emit_enum_program_methods(en, &mut out);
        }

        out.push_str(&prog_methods);
        out.push_str("}\n");
    }

    out
}
