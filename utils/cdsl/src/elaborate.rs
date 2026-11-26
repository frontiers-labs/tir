//! Surface -> Kernel Elaborator.
//!
//! Resolves modules (`mod`, `use`), elaborates inductive types and constructor
//! applications, and desugars lightweight surface syntax (`List(...)`,
//! `memo |x| ...`, `op { ... }`, `Rewrite(...)`, `Refine(...)`, `Rule(...)`,
//! `if/else`) into ordinary kernel terms referencing whatever library types are
//! in scope.
//!
//! ZERO target codegen logic and ZERO hardcoded domain semantics.

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    hash::BuildHasherDefault,
    path::{Path, PathBuf},
    rc::Rc,
};

use crate::{
    ast::{
        Ast, BinOp, Decl as SDecl, DeclId, Expr as SExpr, ExprId, ExtractTarget, Lit as SLit,
        Pattern as SPattern, Slice, Stmt as SStmt, UseItems,
    },
    core::{
        CtorDecl, Decl, ExtractSpec, FxHashMap, IndDecl, Interner, Lit, Pattern, PrimOp, Sym, Term,
    },
    error::Error,
    lexer::{TokenStream, decode_string_inner, parse_natural},
    parser,
};

#[derive(Clone, Debug)]
enum OpTParamKind {
    Type,
    Int,
    Enum(String),
}

#[derive(Clone, Debug)]
struct OpSugarMeta {
    dialect: String,
    op_name: String,
    tparam_kinds: Vec<OpTParamKind>,
    fixed_operands: Option<usize>,
    is_constant_like: bool,
}

/// Global inductive/constructor registry shared across all modules without cloning.
struct GlobalTypes {
    ctor_owner: FxHashMap<Sym, Sym>,
    ctor_arity: FxHashMap<Sym, usize>,
    ind_params: FxHashMap<Sym, usize>,
    def_tparams: FxHashMap<Sym, usize>,
    type_names: FxHashMap<Sym, bool>,
    axioms: FxHashMap<Sym, String>,
    op_defs: FxHashMap<Sym, OpSugarMeta>,
    mod_exports: HashMap<String, Vec<(String, Sym)>>,
}

impl GlobalTypes {
    fn new() -> Self {
        Self {
            ctor_owner: FxHashMap::with_capacity_and_hasher(128, BuildHasherDefault::default()),
            ctor_arity: FxHashMap::with_capacity_and_hasher(128, BuildHasherDefault::default()),
            ind_params: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()),
            def_tparams: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()),
            type_names: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()),
            axioms: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()),
            op_defs: FxHashMap::with_capacity_and_hasher(64, BuildHasherDefault::default()),
            mod_exports: HashMap::with_capacity(32),
        }
    }
}

/// Module-level scope (never cloned on local variable bindings!).
#[derive(Clone)]
struct ModScope {
    prefix: String,
    imports: FxHashMap<String, Sym>,
}

impl ModScope {
    fn new() -> Self {
        Self {
            prefix: String::new(),
            imports: FxHashMap::with_capacity_and_hasher(128, BuildHasherDefault::default()),
        }
    }

    fn child(&self, name: &str) -> Self {
        let mut prefix = String::with_capacity(self.prefix.len() + name.len() + 2);
        if !self.prefix.is_empty() {
            prefix.push_str(&self.prefix);
            prefix.push_str("::");
        }
        prefix.push_str(name);
        Self {
            prefix,
            imports: self.imports.clone(),
        }
    }

    #[inline]
    fn qualify_intern(&self, interner: &mut Interner, buf: &mut String, name: &str) -> Sym {
        if self.prefix.is_empty() {
            interner.intern(name)
        } else {
            buf.clear();
            buf.push_str(&self.prefix);
            buf.push_str("::");
            buf.push_str(name);
            interner.intern(buf)
        }
    }
}

/// Mutable elaborator state with O(1) push/truncate local binder stack (zero HashMap clones).
struct ElabCtx<'a> {
    interner: Interner,
    globals: GlobalTypes,
    locals: Vec<&'a str>,
    scratch: String,
    sym_int: Sym,
    sym_bool: Sym,
    sym_str: Sym,
    sym_unit: Sym,
    sym_wild: Sym,
    sym_nil: Sym,
    sym_cons: Sym,
    sym_forall: Sym,
}

impl<'a> ElabCtx<'a> {
    fn new() -> Self {
        let mut interner = Interner::new();
        let sym_int = interner.intern("Int");
        let sym_bool = interner.intern("Bool");
        let sym_str = interner.intern("String");
        let sym_unit = interner.intern("Unit");
        let sym_wild = interner.intern("_");
        let sym_nil = interner.intern("Nil");
        let sym_cons = interner.intern("Cons");
        let sym_forall = interner.intern("Forall");
        Self {
            interner,
            globals: GlobalTypes::new(),
            locals: Vec::with_capacity(32),
            scratch: String::with_capacity(128),
            sym_int,
            sym_bool,
            sym_str,
            sym_unit,
            sym_wild,
            sym_nil,
            sym_cons,
            sym_forall,
        }
    }

    #[inline]
    fn is_local(&self, name: &str) -> bool {
        self.locals.iter().rev().any(|&l| l == name)
    }

    #[inline]
    fn resolve_sym(&mut self, sc: &ModScope, segs: &[&str]) -> Result<Sym, Error> {
        match segs {
            [] => Err(Error("empty path".into())),
            [x] => {
                if self.is_local(x) {
                    return Ok(self.interner.intern(x));
                }
                if let Some(&q) = sc.imports.get(*x) {
                    return Ok(q);
                }
                match *x {
                    "Int" => Ok(self.sym_int),
                    "Bool" => Ok(self.sym_bool),
                    "String" => Ok(self.sym_str),
                    "Unit" => Ok(self.sym_unit),
                    _ => Ok(sc.qualify_intern(&mut self.interner, &mut self.scratch, x)),
                }
            }
            [head, rest @ ..] => {
                self.scratch.clear();
                if let Some(&qhead) = sc.imports.get(*head) {
                    self.scratch.push_str(self.interner.resolve(qhead));
                } else {
                    self.scratch.push_str(head);
                }
                for r in rest {
                    self.scratch.push_str("::");
                    self.scratch.push_str(r);
                }
                Ok(self.interner.intern(&self.scratch))
            }
        }
    }

    #[inline]
    fn resolve_single(&mut self, sc: &ModScope, name: &str) -> Sym {
        if let Some(&q) = sc.imports.get(name) {
            q
        } else {
            sc.qualify_intern(&mut self.interner, &mut self.scratch, name)
        }
    }

    #[inline]
    fn is_known_def(&self, sc: &ModScope, segs: &[&str]) -> bool {
        match segs {
            [name] => {
                if self.is_local(name) {
                    return true;
                }
                if let Some(&q) = sc.imports.get(*name) {
                    return !self.globals.axioms.contains_key(&q);
                }
                self.globals
                    .mod_exports
                    .get(&sc.prefix)
                    .is_some_and(|exps| {
                        exps.iter()
                            .any(|(k, q)| k == name && !self.globals.axioms.contains_key(q))
                    })
            }
            [head, rest @ ..] => {
                let Some((leaf, mid)) = rest.split_last() else {
                    return false;
                };
                let mut prefix = if let Some(&qh) = sc.imports.get(*head) {
                    self.interner.resolve(qh).to_owned()
                } else {
                    (*head).to_owned()
                };
                for m in mid {
                    prefix.push_str("::");
                    prefix.push_str(m);
                }
                self.globals.mod_exports.get(&prefix).is_some_and(|exps| {
                    exps.iter()
                        .any(|(k, q)| k == leaf && !self.globals.axioms.contains_key(q))
                })
            }
            [] => false,
        }
    }
}

fn qualify_prefix(prefix: &str, name: &str, buf: &mut String) {
    buf.clear();
    if !prefix.is_empty() {
        buf.push_str(prefix);
        buf.push_str("::");
    }
    buf.push_str(name);
}

fn parse_text<'src>(source: &'src str, label: &str) -> Result<Ast<'src>, Error> {
    let stream = TokenStream::from_source(source).map_err(|errs| {
        Error(
            errs.into_iter()
                .map(|e| format!("{label}: {}", e.message))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })?;
    parser::parse(&stream).map_err(|errs| {
        Error(
            errs.into_iter()
                .map(|e| {
                    format!(
                        "{label}: unexpected {:?}, expected {}",
                        e.found,
                        e.expected.join(", ")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        )
    })
}

/// Tree of parsed module files ready for two-pass elaboration.
pub(crate) enum ModuleNode {
    File {
        source: String,
        label: String,
        children: HashMap<String, ModuleNode>,
    },
}

struct ParsedNode<'a> {
    ast: Ast<'a>,
    children: HashMap<String, ParsedNode<'a>>,
}

fn has_root_mod_std(ast: &Ast<'_>) -> bool {
    ast.root_decls()
        .iter()
        .any(|&id| matches!(ast.decl(id), SDecl::Mod { name: "std", .. }))
}

fn ast_references_std(ast: &Ast<'_>) -> bool {
    ast.idents.contains(&"std")
}

fn parsed_tree_references_std(node: &ParsedNode<'_>) -> bool {
    ast_references_std(&node.ast) || node.children.values().any(parsed_tree_references_std)
}

fn parse_module_tree<'a>(node: &'a ModuleNode) -> Result<ParsedNode<'a>, Error> {
    let ModuleNode::File {
        source,
        label,
        children,
    } = node;
    let ast = parse_text(source, label)?;
    let mut parsed_children = HashMap::with_capacity(children.len());
    for (k, v) in children {
        parsed_children.insert(k.clone(), parse_module_tree(v)?);
    }
    Ok(ParsedNode {
        ast,
        children: parsed_children,
    })
}

fn load_module_tree(file: &Path, inputs: &mut BTreeSet<PathBuf>) -> Result<ModuleNode, Error> {
    let canonical =
        fs::canonicalize(file).map_err(|e| Error(format!("{}: {e}", file.display())))?;
    inputs.insert(canonical);
    let source = fs::read_to_string(file).map_err(|e| Error(format!("{}: {e}", file.display())))?;
    let dir = file.parent().unwrap_or(Path::new("."));
    let label = file.display().to_string();
    let ast = parse_text(&source, &label)?;
    let mut external_mods = Vec::new();
    collect_external_mods(&ast, ast.root_decls(), &mut external_mods);
    let mut children = HashMap::new();
    for mod_name in external_mods {
        let f1 = dir.join(format!("{mod_name}.cdsl"));
        let f2 = dir.join(&mod_name).join("mod.cdsl");
        let sub_file = if f1.is_file() {
            f1
        } else if f2.is_file() {
            f2
        } else {
            return Err(Error(format!(
                "cannot find module file for `mod {mod_name};` in {}",
                dir.display()
            )));
        };
        let child = load_module_tree(&sub_file, inputs)?;
        children.insert(mod_name, child);
    }
    Ok(ModuleNode::File {
        source,
        label,
        children,
    })
}

fn collect_external_mods(ast: &Ast<'_>, decls: &[DeclId], out: &mut Vec<String>) {
    for &id in decls {
        if let SDecl::Mod { name, body, .. } = ast.decl(id) {
            match body {
                None => out.push(name.to_owned()),
                Some(slice) => collect_external_mods(ast, ast.decl_slice(slice), out),
            }
        }
    }
}

/// Elaborates a CDSL root file (recursively loading `mod foo;` files) into kernel declarations.
pub fn elaborate_file(file: &Path) -> Result<(Interner, Vec<Decl>, BTreeSet<PathBuf>), Error> {
    let mut inputs = BTreeSet::new();
    let tree = load_module_tree(file, &mut inputs)?;
    let (interner, decls) = elaborate_tree(&tree)?;
    Ok((interner, decls, inputs))
}

/// Elaborates standalone CDSL source text into kernel declarations.
pub fn elaborate_source(source: &str, label: &str) -> Result<(Interner, Vec<Decl>), Error> {
    let ast = parse_text(source, label)?;
    elaborate_ast(&ast)
}

/// Elaborates an already-parsed single-file AST (zero re-parsing).
pub fn elaborate_ast<'src>(ast: &Ast<'src>) -> Result<(Interner, Vec<Decl>), Error> {
    let mut ctx = ElabCtx::new();
    let empty_children = HashMap::new();
    let mut out = Vec::with_capacity(ast.decls.len());
    let mut scope = ModScope::new();
    if ast_references_std(ast) && !has_root_mod_std(ast) {
        let std_node = crate::stdlib::embedded_std();
        let std_parsed = parse_module_tree(&std_node)?;
        let mut std_ctx = ElabCtx::new();
        register_ast_types(
            &mut std_ctx,
            "std",
            &std_parsed.ast,
            std_parsed.ast.root_decls(),
            &std_parsed.children,
        )?;
        let std_scope = ModScope {
            prefix: "std".to_owned(),
            imports: FxHashMap::default(),
        };
        elab_ast_decls(
            &mut std_ctx,
            &std_scope,
            &std_parsed.ast,
            std_parsed.ast.root_decls(),
            &std_parsed.children,
            &mut out,
        )?;
        // Transfer std interner and registered types into `ctx` before interning `std`.
        ctx.interner = std_ctx.interner;
        ctx.globals = std_ctx.globals;
        ctx.sym_int = std_ctx.sym_int;
        ctx.sym_bool = std_ctx.sym_bool;
        ctx.sym_str = std_ctx.sym_str;
        ctx.sym_unit = std_ctx.sym_unit;
        ctx.sym_wild = std_ctx.sym_wild;
        ctx.sym_nil = std_ctx.sym_nil;
        ctx.sym_cons = std_ctx.sym_cons;
        ctx.sym_forall = std_ctx.sym_forall;
        let std_sym = ctx.interner.intern("std");
        scope.imports.insert("std".to_owned(), std_sym);
    }
    register_ast_types(&mut ctx, "", ast, ast.root_decls(), &empty_children)?;
    elab_ast_decls(
        &mut ctx,
        &scope,
        ast,
        ast.root_decls(),
        &empty_children,
        &mut out,
    )?;
    Ok((ctx.interner, out))
}

fn elaborate_tree(tree: &ModuleNode) -> Result<(Interner, Vec<Decl>), Error> {
    let parsed = parse_module_tree(tree)?;
    let std_node = if parsed_tree_references_std(&parsed) && !has_root_mod_std(&parsed.ast) {
        Some(crate::stdlib::embedded_std())
    } else {
        None
    };
    let std_parsed = match &std_node {
        Some(n) => Some(parse_module_tree(n)?),
        None => None,
    };
    let mut ctx = ElabCtx::new();
    let mut scope = ModScope::new();
    let mut out = Vec::with_capacity(parsed.ast.decls.len());
    if let Some(sp) = &std_parsed {
        let std_sym = ctx.interner.intern("std");
        scope.imports.insert("std".to_owned(), std_sym);
        register_ast_types(&mut ctx, "std", &sp.ast, sp.ast.root_decls(), &sp.children)?;
        let std_scope = ModScope {
            prefix: "std".to_owned(),
            imports: FxHashMap::default(),
        };
        elab_ast_decls(
            &mut ctx,
            &std_scope,
            &sp.ast,
            sp.ast.root_decls(),
            &sp.children,
            &mut out,
        )?;
    }
    register_ast_types(
        &mut ctx,
        "",
        &parsed.ast,
        parsed.ast.root_decls(),
        &parsed.children,
    )?;
    elab_ast_decls(
        &mut ctx,
        &scope,
        &parsed.ast,
        parsed.ast.root_decls(),
        &parsed.children,
        &mut out,
    )?;
    Ok((ctx.interner, out))
}

fn register_ast_types<'src>(
    ctx: &mut ElabCtx<'src>,
    prefix: &str,
    ast: &Ast<'src>,
    decls: &[DeclId],
    children: &HashMap<String, ParsedNode<'src>>,
) -> Result<(), Error> {
    let mut buf = String::new();
    let mut exports = Vec::new();
    for &id in decls {
        match ast.decl(id) {
            SDecl::Mod { name, body, .. } => {
                qualify_prefix(prefix, name, &mut buf);
                let qmod = ctx.interner.intern(&buf);
                exports.push(((*name).to_owned(), qmod));
                let sub = buf.clone();
                match body {
                    Some(slice) => {
                        register_ast_types(ctx, &sub, ast, ast.decl_slice(slice), children)?;
                    }
                    None => {
                        if let Some(child) = children.get(name) {
                            register_ast_types(
                                ctx,
                                &sub,
                                &child.ast,
                                child.ast.root_decls(),
                                &child.children,
                            )?;
                        }
                    }
                }
            }
            SDecl::Enum {
                name,
                params,
                ctors,
                ..
            } => {
                qualify_prefix(prefix, name, &mut buf);
                let qind = ctx.interner.intern(&buf);
                exports.push(((*name).to_owned(), qind));
                ctx.globals.type_names.insert(qind, true);
                ctx.globals
                    .ind_params
                    .insert(qind, ast.field_slice(params).len());
                let enum_key = buf.clone();
                let mut ctor_exports = Vec::new();
                let base_len = buf.len();
                for c in ast.ctor_slice(ctors) {
                    buf.truncate(base_len);
                    buf.push_str("::");
                    buf.push_str(c.name);
                    let qctor = ctx.interner.intern(&buf);
                    ctx.globals.ctor_owner.insert(qctor, qind);
                    ctx.globals
                        .ctor_arity
                        .insert(qctor, ast.field_slice(c.fields).len());
                    ctor_exports.push((c.name.to_owned(), qctor));
                }
                ctx.globals.mod_exports.insert(enum_key, ctor_exports);
            }
            SDecl::Let {
                name,
                tparams,
                body,
                ..
            } => {
                qualify_prefix(prefix, name, &mut buf);
                let q = ctx.interner.intern(&buf);
                exports.push(((*name).to_owned(), q));
                if !tparams.is_empty() {
                    ctx.globals.def_tparams.insert(q, tparams.len as usize);
                }
                match body {
                    None => {
                        ctx.globals.axioms.insert(q, (*name).to_owned());
                    }
                    Some(body_id) => {
                        if let SExpr::OpSugar(fs) = ast.expr(body_id) {
                            let meta = extract_op_sugar_meta(prefix, name, tparams, fs, ast);
                            ctx.globals.op_defs.insert(q, meta);
                        }
                    }
                }
            }
            SDecl::Type { name, .. } => {
                qualify_prefix(prefix, name, &mut buf);
                let q = ctx.interner.intern(&buf);
                exports.push(((*name).to_owned(), q));
                ctx.globals.type_names.insert(q, true);
            }
            SDecl::Rule { name, .. } => {
                qualify_prefix(prefix, name, &mut buf);
                let q = ctx.interner.intern(&buf);
                exports.push(((*name).to_owned(), q));
            }
            SDecl::Use { .. } | SDecl::Extract { .. } => {}
        }
    }
    if !prefix.is_empty() {
        ctx.globals
            .mod_exports
            .entry(prefix.to_owned())
            .or_default()
            .extend(exports);
    }
    Ok(())
}

#[allow(clippy::cognitive_complexity)]
fn elab_ast_decls<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    decls: &[DeclId],
    children: &HashMap<String, ParsedNode<'src>>,
    out: &mut Vec<Decl>,
) -> Result<(), Error> {
    let mut cur_sc = sc.clone();
    // Register sibling names in this module scope.
    for &id in decls {
        let name = match ast.decl(id) {
            SDecl::Type { name, .. }
            | SDecl::Enum { name, .. }
            | SDecl::Let { name, .. }
            | SDecl::Rule { name, .. }
            | SDecl::Mod { name, .. } => Some(name),
            SDecl::Use { .. } | SDecl::Extract { .. } => None,
        };
        if let Some(n) = name {
            let sym = cur_sc.qualify_intern(&mut ctx.interner, &mut ctx.scratch, n);
            cur_sc.imports.insert(n.to_owned(), sym);
        }
    }

    for &id in decls {
        match ast.decl(id) {
            SDecl::Use {
                public,
                path,
                items,
            } => {
                let segs = ast.ident_slice(path);
                let full = segs.join("::");
                match items {
                    UseItems::Single => {
                        if let Some(&short) = segs.last() {
                            let sym = ctx.interner.intern(&full);
                            cur_sc.imports.insert(short.to_owned(), sym);
                        }
                    }
                    UseItems::Alias(alias) => {
                        let sym = ctx.interner.intern(&full);
                        cur_sc.imports.insert(alias.to_owned(), sym);
                    }
                    UseItems::Glob => {
                        let resolved_mod = match segs {
                            [head, rest @ ..] => {
                                if let Some(&qhead) = cur_sc.imports.get(*head) {
                                    let mut s = ctx.interner.resolve(qhead).to_owned();
                                    for r in rest {
                                        s.push_str("::");
                                        s.push_str(r);
                                    }
                                    s
                                } else {
                                    full.clone()
                                }
                            }
                            [] => full.clone(),
                        };
                        if public && resolved_mod == "std::ir::interfaces" {
                            let marker = cur_sc.qualify_intern(
                                &mut ctx.interner,
                                &mut ctx.scratch,
                                "__reexport_interfaces",
                            );
                            out.push(Decl::Extract(
                                ExtractTarget::Rust,
                                marker,
                                ExtractSpec::Const("@reexport_interfaces".to_owned()),
                            ));
                        }
                        if let Some(items) = ctx.globals.mod_exports.get(&resolved_mod) {
                            for (short, sym) in items {
                                cur_sc.imports.insert(short.clone(), *sym);
                            }
                        }
                    }
                    UseItems::Group(group) => {
                        let mut item_buf = String::with_capacity(full.len() + 24);
                        for &item in ast.ident_slice(group) {
                            item_buf.clear();
                            item_buf.push_str(&full);
                            item_buf.push_str("::");
                            item_buf.push_str(item);
                            let sym = ctx.interner.intern(&item_buf);
                            cur_sc.imports.insert(item.to_owned(), sym);
                        }
                    }
                }
            }
            SDecl::Mod { name, body, .. } => {
                let sub = cur_sc.child(name);
                match body {
                    Some(slice) => {
                        elab_ast_decls(ctx, &sub, ast, ast.decl_slice(slice), children, out)?;
                    }
                    None => {
                        let child = children
                            .get(name)
                            .ok_or_else(|| Error(format!("unexpanded external mod {name}")))?;
                        elab_ast_decls(
                            ctx,
                            &sub,
                            &child.ast,
                            child.ast.root_decls(),
                            &child.children,
                            out,
                        )?;
                    }
                }
            }
            SDecl::Type {
                name, params, sort, ..
            } => {
                let q = cur_sc.qualify_intern(&mut ctx.interner, &mut ctx.scratch, name);
                let pslice = ast.field_slice(params);
                let saved = ctx.locals.len();
                let mut ps = Vec::with_capacity(pslice.len());
                for &(x, t) in pslice {
                    let tx = elab_type(ctx, &cur_sc, ast, t)?;
                    ps.push((ctx.interner.intern(x), tx));
                    ctx.locals.push(x);
                }
                ctx.locals.truncate(saved);
                out.push(Decl::Type(q, ps, sort));
            }
            SDecl::Enum {
                name,
                params,
                sort,
                ctors,
                ..
            } => {
                let q = cur_sc.qualify_intern(&mut ctx.interner, &mut ctx.scratch, name);
                let pslice = ast.field_slice(params);
                let saved = ctx.locals.len();
                let mut ps = Vec::with_capacity(pslice.len());
                for &(x, t) in pslice {
                    let tx = elab_type(ctx, &cur_sc, ast, t)?;
                    ps.push((ctx.interner.intern(x), tx));
                    ctx.locals.push(x);
                }
                let def_ret = Term::Ind(
                    q,
                    pslice
                        .iter()
                        .map(|&(x, _)| Term::Var(ctx.interner.intern(x)))
                        .collect(),
                );
                let mut kctors = Vec::with_capacity(ctors.len as usize);
                for c in ast.ctor_slice(ctors) {
                    let saved_c = ctx.locals.len();
                    let mut fields = Vec::with_capacity(c.fields.len as usize);
                    for &(fnm, ft) in ast.field_slice(c.fields) {
                        fields.push((ctx.interner.intern(fnm), elab_type(ctx, &cur_sc, ast, ft)?));
                        if !fnm.starts_with('_') {
                            ctx.locals.push(fnm);
                        }
                    }
                    let ret = match c.ret {
                        Some(r) => elab_type(ctx, &cur_sc, ast, r)?,
                        None => def_ret.clone(),
                    };
                    ctx.locals.truncate(saved_c);
                    kctors.push(CtorDecl {
                        cname: ctx.interner.intern(c.name),
                        cparams: fields,
                        cret: ret,
                    });
                }
                ctx.locals.truncate(saved);
                out.push(Decl::Ind(IndDecl {
                    iname: q,
                    iparams: ps,
                    isort: sort,
                    ictors: kctors,
                }));
            }
            SDecl::Let {
                name,
                tparams,
                ty,
                body: None,
                ..
            } => {
                let q = cur_sc.qualify_intern(&mut ctx.interner, &mut ctx.scratch, name);
                let tps = ast.field_slice(tparams);
                let ann = ty.ok_or_else(|| {
                    Error(format!(
                        "bodyless let {} requires type annotation",
                        ctx.interner.resolve(q)
                    ))
                })?;
                let saved = ctx.locals.len();
                for &(x, _) in tps {
                    ctx.locals.push(x);
                }
                let mut full_ty = elab_type(ctx, &cur_sc, ast, ann)?;
                ctx.locals.truncate(saved);
                for &(x, t) in tps.iter().rev() {
                    let tx = elab_type(ctx, &cur_sc, ast, t)?;
                    full_ty = Term::Pi(ctx.interner.intern(x), Box::new(tx), Rc::new(full_ty));
                }
                out.push(Decl::Axiom(q, full_ty));
            }
            SDecl::Let {
                name,
                tparams,
                ty,
                body: Some(body_id),
                ..
            } => {
                let q = cur_sc.qualify_intern(&mut ctx.interner, &mut ctx.scratch, name);
                let tps = ast.field_slice(tparams);
                let saved = ctx.locals.len();
                for &(x, _) in tps {
                    ctx.locals.push(x);
                }
                let body_expr = ast.expr(body_id);
                let mut ebody = match body_expr {
                    SExpr::OpSugar(fs) => {
                        let def_dialect = cur_sc.prefix.rsplit("::").next().unwrap_or("");
                        let def_name = name.strip_prefix("r#").unwrap_or(name);
                        elab_op_sugar(ctx, &cur_sc, ast, fs, def_dialect, def_name)?
                    }
                    SExpr::RecFix { params, ret, body } => {
                        let ps = ast.opt_param_slice(params);
                        let rty_id = ret.ok_or_else(|| {
                            Error(format!(
                                "rec definition {} needs return type",
                                ctx.interner.resolve(q)
                            ))
                        })?;
                        let rty = elab_type(ctx, &cur_sc, ast, rty_id)?;
                        let mut fty = rty;
                        for &(px, pty_opt) in ps.iter().rev() {
                            let pty_id = pty_opt.ok_or_else(|| {
                                Error(format!("rec param {px} needs type annotation"))
                            })?;
                            let pty = elab_type(ctx, &cur_sc, ast, pty_id)?;
                            fty = Term::Pi(ctx.interner.intern(px), Box::new(pty), Rc::new(fty));
                        }
                        let saved_r = ctx.locals.len();
                        ctx.locals.push(name);
                        let lam = elab_lam(ctx, &cur_sc, ast, ps, body)?;
                        ctx.locals.truncate(saved_r);
                        Term::Fix(ctx.interner.intern(name), Box::new(fty), Rc::new(lam))
                    }
                    _ if ty.is_some_and(|tid| {
                        matches!(ast.expr(tid), SExpr::Sort(crate::ast::Sort::Type))
                    }) =>
                    {
                        elab_type(ctx, &cur_sc, ast, body_id)?
                    }
                    _ => elab_expr(ctx, &cur_sc, ast, body_id)?,
                };
                let ty_opt = match ty {
                    Some(ann) => {
                        let mut full_ty = elab_type(ctx, &cur_sc, ast, ann)?;
                        ctx.locals.truncate(saved);
                        for &(x, t) in tps.iter().rev() {
                            let tx = elab_type(ctx, &cur_sc, ast, t)?;
                            full_ty =
                                Term::Pi(ctx.interner.intern(x), Box::new(tx), Rc::new(full_ty));
                        }
                        Some(full_ty)
                    }
                    None => {
                        let inferred = match (&ebody, body_expr) {
                            (Term::Fix(_, fty, _), _) => {
                                let mut fn_ty = (**fty).clone();
                                ctx.locals.truncate(saved);
                                for &(x, t) in tps.iter().rev() {
                                    let tx = elab_type(ctx, &cur_sc, ast, t)?;
                                    fn_ty = Term::Pi(
                                        ctx.interner.intern(x),
                                        Box::new(tx),
                                        Rc::new(fn_ty),
                                    );
                                }
                                Some(fn_ty)
                            }
                            (
                                _,
                                SExpr::Lam {
                                    params,
                                    ret: Some(ret_id),
                                    ..
                                },
                            ) => {
                                let ps = ast.opt_param_slice(params);
                                if ps.iter().all(|(_, t)| t.is_some()) {
                                    let mut fn_ty = elab_type(ctx, &cur_sc, ast, ret_id)?;
                                    for &(px, pty_opt) in ps.iter().rev() {
                                        let pty = elab_type(ctx, &cur_sc, ast, pty_opt.unwrap())?;
                                        fn_ty = Term::Pi(
                                            ctx.interner.intern(px),
                                            Box::new(pty),
                                            Rc::new(fn_ty),
                                        );
                                    }
                                    ctx.locals.truncate(saved);
                                    for &(x, t) in tps.iter().rev() {
                                        let tx = elab_type(ctx, &cur_sc, ast, t)?;
                                        fn_ty = Term::Pi(
                                            ctx.interner.intern(x),
                                            Box::new(tx),
                                            Rc::new(fn_ty),
                                        );
                                    }
                                    Some(fn_ty)
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        ctx.locals.truncate(saved);
                        inferred
                    }
                };
                for &(x, t) in tps.iter().rev() {
                    let tx = elab_type(ctx, &cur_sc, ast, t)?;
                    ebody = Term::Lam(ctx.interner.intern(x), Some(Box::new(tx)), Rc::new(ebody));
                }
                out.push(Decl::Def(q, ty_opt, ebody));
            }
            SDecl::Rule {
                name,
                vars,
                kind,
                lhs,
                rhs,
                ..
            } => {
                let q = cur_sc.qualify_intern(&mut ctx.interner, &mut ctx.scratch, name);
                let rterm = elab_rule_sugar(ctx, &cur_sc, ast, name, vars, kind, lhs, rhs)?;
                let rty_sym = match kind {
                    "Rewrite" | "Refine" => ctx.resolve_single(&cur_sc, "Rewrite"),
                    "Rule" => ctx.resolve_single(&cur_sc, "Rule"),
                    other => return Err(Error(format!("unknown rule kind {other}"))),
                };
                let rty = Term::Ind(rty_sym, Vec::new());
                out.push(Decl::Def(q, Some(rty), rterm));
            }
            SDecl::Extract {
                target,
                path,
                rhs,
                ctors,
            } => {
                let q = ctx.resolve_sym(&cur_sc, ast.ident_slice(path))?;
                let rhs_decoded = decode_string_inner(rhs).into_owned();
                let spec = match ctors {
                    None => ExtractSpec::Const(rhs_decoded),
                    Some(cs) => {
                        let mapped: Vec<(Sym, String)> = ast
                            .ctor_map_slice(cs)
                            .iter()
                            .map(|&(c, t)| {
                                (ctx.interner.intern(c), decode_string_inner(t).into_owned())
                            })
                            .collect();
                        if target == ExtractTarget::Egglog {
                            ExtractSpec::Tag(rhs_decoded, mapped)
                        } else {
                            ExtractSpec::Inductive(rhs_decoded, mapped)
                        }
                    }
                };
                out.push(Decl::Extract(target, q, spec));
            }
        }
    }
    Ok(())
}

#[inline]
fn elab_lit(l: SLit<'_>) -> Result<Lit, Error> {
    match l {
        SLit::Int(s) => Ok(Lit::Int(parse_natural(s).map_err(Error)?)),
        SLit::Bool(b) => Ok(Lit::Bool(b)),
        SLit::Str(s) => Ok(Lit::Str(Rc::from(decode_string_inner(s).as_ref()))),
        SLit::Unit => Ok(Lit::Unit),
    }
}

#[inline]
fn elab_binop(op: BinOp) -> PrimOp {
    match op {
        BinOp::Add => PrimOp::Add,
        BinOp::Sub => PrimOp::Sub,
        BinOp::Mul => PrimOp::Mul,
        BinOp::Div => PrimOp::Div,
        BinOp::Rem => PrimOp::Rem,
        BinOp::Eq => PrimOp::Eq,
        BinOp::Neq => PrimOp::Neq,
        BinOp::Lt => PrimOp::Lt,
        BinOp::Le => PrimOp::Le,
        BinOp::Gt => PrimOp::Gt,
        BinOp::Ge => PrimOp::Ge,
        BinOp::And => PrimOp::And,
        BinOp::Or => PrimOp::Or,
        BinOp::StrCat => PrimOp::StrCat,
    }
}

fn elab_type<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    id: ExprId,
) -> Result<Term, Error> {
    match ast.expr(id) {
        SExpr::Sort(s) => Ok(Term::Sort(s)),
        SExpr::Pi { param, dom, cod } => {
            let ta = elab_type(ctx, sc, ast, dom)?;
            let saved = ctx.locals.len();
            if param != "_" {
                ctx.locals.push(param);
            }
            let tb = elab_type(ctx, sc, ast, cod)?;
            ctx.locals.truncate(saved);
            Ok(Term::Pi(
                ctx.interner.intern(param),
                Box::new(ta),
                Rc::new(tb),
            ))
        }
        SExpr::RecordTy(fs) => {
            let fslice = ast.field_slice(fs);
            let mut fields = Vec::with_capacity(fslice.len());
            for &(k, t) in fslice {
                fields.push((ctx.interner.intern(k), elab_type(ctx, sc, ast, t)?));
            }
            Ok(Term::RecTy(fields))
        }
        SExpr::Lit(l) => Ok(Term::Lit(elab_lit(l)?)),
        SExpr::Bin { op, lhs, rhs } => {
            let a = elab_type(ctx, sc, ast, lhs)?;
            let b = elab_type(ctx, sc, ast, rhs)?;
            Ok(Term::Prim(elab_binop(op), vec![a, b]))
        }
        SExpr::Path { path, ty_args } => {
            let segs = ast.ident_slice(path);
            if let [x] = segs
                && ty_args.is_empty()
                && ctx.is_local(x)
            {
                return Ok(Term::Var(ctx.interner.intern(x)));
            }
            let q = ctx.resolve_sym(sc, segs)?;
            if let Some(&ind) = ctx.globals.ctor_owner.get(&q) {
                let cname = *segs.last().unwrap();
                let nparams = ctx.globals.ind_params.get(&ind).copied().unwrap_or(0);
                let mut cargs = Vec::with_capacity(nparams + ty_args.len as usize);
                cargs.resize(nparams, Term::Hole);
                for &a in ast.expr_slice(ty_args) {
                    cargs.push(elab_type(ctx, sc, ast, a)?);
                }
                return Ok(Term::Ctor(ind, ctx.interner.intern(cname), cargs));
            }
            let is_nominal = q == ctx.sym_int
                || q == ctx.sym_bool
                || q == ctx.sym_str
                || q == ctx.sym_unit
                || ctx.globals.type_names.contains_key(&q);
            if !is_nominal {
                let mut acc = Term::Global(q);
                for &a in ast.expr_slice(ty_args) {
                    acc = Term::App(Box::new(acc), Box::new(elab_type(ctx, sc, ast, a)?));
                }
                return Ok(acc);
            }
            if ty_args.is_empty() {
                Ok(Term::Ind(q, Vec::new()))
            } else {
                let aslice = ast.expr_slice(ty_args);
                let mut args = Vec::with_capacity(aslice.len());
                for &a in aslice {
                    args.push(elab_type(ctx, sc, ast, a)?);
                }
                Ok(Term::Ind(q, args))
            }
        }
        _ => Err(Error("invalid type expression".into())),
    }
}

fn mk_list(ctx: &mut ElabCtx<'_>, sc: &ModScope, items: Vec<Term>) -> Term {
    let list_ty = ctx.resolve_single(sc, "List");
    let nil = ctx.sym_nil;
    let cons = ctx.sym_cons;
    let mut acc = Term::Ctor(list_ty, nil, vec![Term::Hole]);
    for item in items.into_iter().rev() {
        acc = Term::Ctor(list_ty, cons, vec![Term::Hole, item, acc]);
    }
    acc
}

#[allow(clippy::too_many_arguments)]
fn elab_rule_sugar<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    rname: &str,
    vars_slice: Slice,
    kind: &str,
    lhs_slice: Slice,
    rhs_slice: Slice,
) -> Result<Term, Error> {
    let vars = ast.ident_slice(vars_slice);
    let saved = ctx.locals.len();
    for &v in vars {
        ctx.locals.push(v);
    }
    let lhs = ast.expr_slice(lhs_slice);
    let rhs = ast.expr_slice(rhs_slice);
    let res = match kind {
        "Rewrite" | "Refine" => {
            let rw_ty = ctx.resolve_single(sc, "Rewrite");
            let term_ty = ctx.resolve_single(sc, "Term");
            let guard_ty = ctx.resolve_single(sc, "Guard");
            let (&first, rest) = lhs
                .split_first()
                .ok_or_else(|| Error(format!("empty pattern in {kind}")))?;
            let pat = elab_rewrite_term(ctx, sc, ast, first)?;
            let mut guards = Vec::with_capacity(rest.len());
            for &g in rest {
                guards.push(elab_guard(ctx, sc, ast, guard_ty, g)?);
            }
            let guard = if guards.is_empty() {
                Term::Ctor(guard_ty, ctx.interner.intern("True"), Vec::new())
            } else {
                let and_sym = ctx.interner.intern("And");
                guards
                    .into_iter()
                    .reduce(|a, b| Term::Ctor(guard_ty, and_sym, vec![a, b]))
                    .unwrap()
            };
            let [rep_id] = rhs else {
                ctx.locals.truncate(saved);
                return Err(Error(format!(
                    "{kind} expects a single replacement expression"
                )));
            };
            let replacement = elab_rewrite_term(ctx, sc, ast, *rep_id)?;
            let ctor = ctx
                .interner
                .intern(if kind == "Rewrite" { "Equiv" } else { "Refine" });
            let forall_sym = ctx.sym_forall;
            let mut acc = Term::Ctor(rw_ty, ctor, vec![pat, guard, replacement]);
            for &v in vars.iter().rev() {
                let vs = ctx.interner.intern(v);
                let lam = Term::Lam(
                    vs,
                    Some(Box::new(Term::Ind(term_ty, Vec::new()))),
                    Rc::new(acc),
                );
                acc = Term::Ctor(rw_ty, forall_sym, vec![lam]);
            }
            Ok(acc)
        }
        "Rule" => {
            let rule_ty = ctx.resolve_single(sc, "Rule");
            let atom_ty = ctx.resolve_single(sc, "Atom");
            let mut facts = Vec::with_capacity(lhs.len());
            for &f in lhs {
                facts.push(elab_fact(ctx, sc, ast, f)?);
            }
            let mut actions = Vec::with_capacity(rhs.len());
            for &a in rhs {
                actions.push(elab_action(ctx, sc, ast, a)?);
            }
            let facts_list = mk_list(ctx, sc, facts);
            let actions_list = mk_list(ctx, sc, actions);
            let clause = Term::Ctor(
                rule_ty,
                ctx.interner.intern("Clause"),
                vec![
                    Term::Lit(Lit::Str(Rc::from(rname))),
                    facts_list,
                    actions_list,
                ],
            );
            let forall_sym = ctx.sym_forall;
            let mut acc = clause;
            for &v in vars.iter().rev() {
                let vs = ctx.interner.intern(v);
                let lam = Term::Lam(
                    vs,
                    Some(Box::new(Term::Ind(atom_ty, Vec::new()))),
                    Rc::new(acc),
                );
                acc = Term::Ctor(rule_ty, forall_sym, vec![lam]);
            }
            Ok(acc)
        }
        other => Err(Error(format!("unknown rule sugar kind: {other}"))),
    };
    ctx.locals.truncate(saved);
    res
}

fn instantiate_op_def(ctx: &ElabCtx<'_>, q: Sym, meta: &OpSugarMeta) -> Term {
    let mut acc = Term::Global(q);
    for kind in &meta.tparam_kinds {
        let arg = match kind {
            OpTParamKind::Type => Term::Ind(ctx.sym_int, Vec::new()),
            OpTParamKind::Int => Term::Lit(Lit::Int(0)),
            OpTParamKind::Enum(qtype) => {
                if let Some(ind_sym) = ctx.interner.lookup(qtype)
                    && let Some((&ctor_sym, _)) = ctx.globals.ctor_owner.iter().find(|&(c, &o)| {
                        o == ind_sym && ctx.globals.ctor_arity.get(c).copied().unwrap_or(1) == 0
                    })
                {
                    let cname = ctx
                        .interner
                        .resolve(ctor_sym)
                        .rsplit("::")
                        .next()
                        .unwrap_or("");
                    if let Some(leaf) = ctx.interner.lookup(cname) {
                        Term::Ctor(ind_sym, leaf, Vec::new())
                    } else {
                        Term::Lit(Lit::Int(0))
                    }
                } else {
                    Term::Lit(Lit::Int(0))
                }
            }
        };
        acc = Term::App(Box::new(acc), Box::new(arg));
    }
    acc
}

fn elab_rewrite_term<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    id: ExprId,
) -> Result<Term, Error> {
    if let SExpr::App { func, args } = ast.expr(id)
        && let SExpr::Path { path, ty_args } = ast.expr(func)
        && ty_args.is_empty()
    {
        let segs = ast.ident_slice(path);
        let q = ctx.resolve_sym(sc, segs)?;
        if let Some(meta) = ctx.globals.op_defs.get(&q).cloned() {
            let term_ty = ctx.resolve_single(sc, "Term");
            let op_val = instantiate_op_def(ctx, q, &meta);
            let aslice = ast.expr_slice(args);
            if meta.is_constant_like && aslice.len() == 1 {
                let val = elab_expr(ctx, sc, ast, aslice[0])?;
                let csym = ctx.interner.intern("TConst");
                return Ok(Term::Ctor(term_ty, csym, vec![op_val, val]));
            }
            if let Some(n) = meta.fixed_operands
                && aslice.len() != n
            {
                return Err(Error(format!(
                    "op {}.{} expects {n} operand(s) in Rewrite, got {}",
                    meta.dialect,
                    meta.op_name,
                    aslice.len()
                )));
            }
            let mut targs = Vec::with_capacity(aslice.len());
            for &a in aslice {
                targs.push(elab_rewrite_term(ctx, sc, ast, a)?);
            }
            let list = mk_list(ctx, sc, targs);
            let osym = ctx.interner.intern("TOp");
            return Ok(Term::Ctor(term_ty, osym, vec![op_val, list]));
        }
    }
    elab_expr(ctx, sc, ast, id)
}

fn elab_guard<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    guard_ty: Sym,
    id: ExprId,
) -> Result<Term, Error> {
    match ast.expr(id) {
        SExpr::Not(inner) => {
            let f = elab_fact(ctx, sc, ast, inner)?;
            Ok(Term::Ctor(guard_ty, ctx.interner.intern("Not"), vec![f]))
        }
        SExpr::Path { path, ty_args } if ty_args.is_empty() => {
            let segs = ast.ident_slice(path);
            if segs
                .last()
                .and_then(|s| s.as_bytes().first())
                .is_some_and(u8::is_ascii_lowercase)
                && ctx.is_known_def(sc, segs)
            {
                return elab_expr(ctx, sc, ast, id);
            }
            let f = elab_fact(ctx, sc, ast, id)?;
            Ok(Term::Ctor(guard_ty, ctx.interner.intern("Fact"), vec![f]))
        }
        SExpr::App { func, args } => {
            if let SExpr::Path { path, ty_args } = ast.expr(func)
                && ty_args.is_empty()
            {
                let segs = ast.ident_slice(path);
                let q = ctx.resolve_sym(sc, segs)?;
                if let Some(rel_leaf) = ctx.globals.axioms.get(&q).cloned() {
                    let fact_ty = ctx.resolve_single(sc, "Fact");
                    let t2a = ctx.resolve_single(sc, "term_to_atom");
                    let aslice = ast.expr_slice(args);
                    let mut eargs = Vec::with_capacity(aslice.len());
                    for &a in aslice {
                        let ta = elab_rewrite_term(ctx, sc, ast, a)?;
                        eargs.push(Term::App(Box::new(Term::Global(t2a)), Box::new(ta)));
                    }
                    let list = mk_list(ctx, sc, eargs);
                    let row = Term::Ctor(
                        fact_ty,
                        ctx.interner.intern("Row"),
                        vec![Term::Lit(Lit::Str(Rc::from(rel_leaf.as_str()))), list],
                    );
                    return Ok(Term::Ctor(guard_ty, ctx.interner.intern("Fact"), vec![row]));
                }
                if segs
                    .last()
                    .and_then(|s| s.as_bytes().first())
                    .is_some_and(u8::is_ascii_lowercase)
                    && ctx.is_known_def(sc, segs)
                {
                    if matches!(
                        segs.last().copied(),
                        Some("arg" | "int_lit" | "bool_lit" | "str_lit" | "bigint" | "op_decl")
                    ) {
                        return elab_expr(ctx, sc, ast, id);
                    }
                    let q = ctx.resolve_sym(sc, segs)?;
                    let mut acc = Term::Global(q);
                    if let Some(&nt) = ctx.globals.def_tparams.get(&q) {
                        for _ in 0..nt {
                            acc = Term::App(Box::new(acc), Box::new(Term::Hole));
                        }
                    }
                    let aslice = ast.expr_slice(args);
                    if aslice.is_empty() {
                        return Ok(Term::App(Box::new(acc), Box::new(Term::Lit(Lit::Unit))));
                    }
                    for &a in aslice {
                        acc =
                            Term::App(Box::new(acc), Box::new(elab_rewrite_term(ctx, sc, ast, a)?));
                    }
                    return Ok(acc);
                }
            }
            let f = elab_fact(ctx, sc, ast, id)?;
            Ok(Term::Ctor(guard_ty, ctx.interner.intern("Fact"), vec![f]))
        }
        _ => {
            let f = elab_fact(ctx, sc, ast, id)?;
            Ok(Term::Ctor(guard_ty, ctx.interner.intern("Fact"), vec![f]))
        }
    }
}

fn elab_atom<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    id: ExprId,
) -> Result<Term, Error> {
    let atom_ty = ctx.resolve_single(sc, "Atom");
    match ast.expr(id) {
        SExpr::Lit(SLit::Int(s)) => Ok(Term::Ctor(
            atom_ty,
            ctx.interner.intern("IntLit"),
            vec![Term::Lit(Lit::Int(parse_natural(s).map_err(Error)?))],
        )),
        SExpr::Neg(inner) => {
            if let SExpr::Lit(SLit::Int(s)) = ast.expr(inner) {
                let n = parse_natural(s).map_err(Error)?;
                return Ok(Term::Ctor(
                    atom_ty,
                    ctx.interner.intern("IntLit"),
                    vec![Term::Lit(Lit::Int(n.wrapping_neg()))],
                ));
            }
            let zero = Term::Ctor(
                atom_ty,
                ctx.interner.intern("IntLit"),
                vec![Term::Lit(Lit::Int(0))],
            );
            let rhs = elab_atom(ctx, sc, ast, inner)?;
            let args = mk_list(ctx, sc, vec![zero, rhs]);
            Ok(Term::Ctor(
                atom_ty,
                ctx.interner.intern("Call"),
                vec![Term::Lit(Lit::Str(Rc::from("-"))), args],
            ))
        }
        SExpr::Lit(SLit::Bool(b)) => Ok(Term::Ctor(
            atom_ty,
            ctx.interner.intern("BoolLit"),
            vec![Term::Lit(Lit::Bool(b))],
        )),
        SExpr::Lit(SLit::Str(s)) => Ok(Term::Ctor(
            atom_ty,
            ctx.interner.intern("StrLit"),
            vec![Term::Lit(Lit::Str(Rc::from(
                decode_string_inner(s).as_ref(),
            )))],
        )),
        SExpr::Path { path, ty_args } if ty_args.is_empty() => {
            let segs = ast.ident_slice(path);
            if let [x] = segs
                && ctx.is_local(x)
            {
                return Ok(Term::Var(ctx.interner.intern(x)));
            }
            if let Ok(q) = ctx.resolve_sym(sc, segs)
                && ctx.globals.ctor_owner.contains_key(&q)
            {
                let cname: Rc<str> = Rc::from(segs.join("::"));
                let empty = mk_list(ctx, sc, Vec::new());
                return Ok(Term::Ctor(
                    atom_ty,
                    ctx.interner.intern("Call"),
                    vec![Term::Lit(Lit::Str(cname)), empty],
                ));
            }
            elab_expr(ctx, sc, ast, id)
        }
        SExpr::Bin { op, lhs, rhs } => {
            let op_name = match op {
                BinOp::Add => "+",
                BinOp::Sub => "-",
                BinOp::Mul => "*",
                BinOp::Div => "/",
                BinOp::Rem => "%",
                BinOp::Lt => "<",
                BinOp::Le => "<=",
                BinOp::Gt => ">",
                BinOp::Ge => ">=",
                BinOp::Eq => "==",
                BinOp::Neq => "!=",
                _ => return Err(Error("unsupported operator in egglog atom".into())),
            };
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            let args = mk_list(ctx, sc, vec![a, b]);
            Ok(Term::Ctor(
                atom_ty,
                ctx.interner.intern("Call"),
                vec![Term::Lit(Lit::Str(Rc::from(op_name))), args],
            ))
        }
        SExpr::Proj {
            obj,
            field: field @ ("tag" | "kind" | "typed_kind"),
        } => {
            if let SExpr::Path { path, ty_args } = ast.expr(obj)
                && ty_args.is_empty()
            {
                let q = ctx.resolve_sym(sc, ast.ident_slice(path))?;
                if let Some(meta) = ctx.globals.op_defs.get(&q).cloned() {
                    if field == "tag" {
                        let tag = format!("{}.{}", meta.dialect, meta.op_name);
                        return Ok(Term::Ctor(
                            atom_ty,
                            ctx.interner.intern("StrLit"),
                            vec![Term::Lit(Lit::Str(Rc::from(tag.as_str())))],
                        ));
                    }
                    let decl_sym = format!("@cdsl:decl:{}.{}", meta.dialect, meta.op_name);
                    let empty = mk_list(ctx, sc, Vec::new());
                    return Ok(Term::Ctor(
                        atom_ty,
                        ctx.interner.intern("Call"),
                        vec![Term::Lit(Lit::Str(Rc::from(decl_sym.as_str()))), empty],
                    ));
                }
                if field == "tag" {
                    let tag_sym = ctx.interner.intern("tag");
                    return Ok(Term::Ctor(
                        atom_ty,
                        ctx.interner.intern("StrLit"),
                        vec![Term::Proj(Box::new(Term::Global(q)), tag_sym)],
                    ));
                }
            }
            elab_expr(ctx, sc, ast, id)
        }
        SExpr::App { func, args } => {
            if let SExpr::Path { path, ty_args } = ast.expr(func)
                && ty_args.is_empty()
            {
                let segs = ast.ident_slice(path);
                if segs
                    .last()
                    .and_then(|s| s.as_bytes().first())
                    .is_some_and(u8::is_ascii_lowercase)
                    && ctx.is_known_def(sc, segs)
                {
                    if matches!(
                        segs.last().copied(),
                        Some("arg" | "int_lit" | "bool_lit" | "str_lit" | "bigint" | "op_decl")
                    ) {
                        return elab_expr(ctx, sc, ast, id);
                    }
                    let q = ctx.resolve_sym(sc, segs)?;
                    let mut acc = Term::Global(q);
                    if let Some(&nt) = ctx.globals.def_tparams.get(&q) {
                        for _ in 0..nt {
                            acc = Term::App(Box::new(acc), Box::new(Term::Hole));
                        }
                    }
                    let aslice = ast.expr_slice(args);
                    if aslice.is_empty() {
                        return Ok(Term::App(Box::new(acc), Box::new(Term::Lit(Lit::Unit))));
                    }
                    for &a in aslice {
                        acc = Term::App(Box::new(acc), Box::new(elab_atom(ctx, sc, ast, a)?));
                    }
                    return Ok(acc);
                }
                let q: Rc<str> = if let Ok(sym) = ctx.resolve_sym(sc, segs)
                    && let Some(leaf) = ctx.globals.axioms.get(&sym)
                {
                    Rc::from(leaf.as_str())
                } else {
                    match segs {
                        [name] => Rc::from(*name),
                        many => Rc::from(many.join("::")),
                    }
                };
                let aslice = ast.expr_slice(args);
                let mut eargs = Vec::with_capacity(aslice.len());
                for &a in aslice {
                    eargs.push(elab_atom(ctx, sc, ast, a)?);
                }
                let list = mk_list(ctx, sc, eargs);
                return Ok(Term::Ctor(
                    atom_ty,
                    ctx.interner.intern("Call"),
                    vec![Term::Lit(Lit::Str(q)), list],
                ));
            }
            elab_expr(ctx, sc, ast, id)
        }
        _ => elab_expr(ctx, sc, ast, id),
    }
}

fn elab_rel_name<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    id: ExprId,
) -> Result<Term, Error> {
    match ast.expr(id) {
        SExpr::Path { path, ty_args } if ty_args.is_empty() => {
            let segs = ast.ident_slice(path);
            let name: Rc<str> = Rc::from(*segs.last().unwrap_or(&""));
            Ok(Term::Lit(Lit::Str(name)))
        }
        SExpr::Proj { obj, field } => {
            let e = elab_expr(ctx, sc, ast, obj)?;
            Ok(Term::Proj(Box::new(e), ctx.interner.intern(field)))
        }
        _ => Err(Error("expected relation name in fact/action".into())),
    }
}

fn elab_fact<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    id: ExprId,
) -> Result<Term, Error> {
    let fact_ty = ctx.resolve_single(sc, "Fact");
    match ast.expr(id) {
        SExpr::Not(inner) => {
            if let SExpr::App { func, args } = ast.expr(inner) {
                let rel = elab_rel_name(ctx, sc, ast, func)?;
                let aslice = ast.expr_slice(args);
                let mut eargs = Vec::with_capacity(aslice.len());
                for &a in aslice {
                    eargs.push(elab_atom(ctx, sc, ast, a)?);
                }
                let list = mk_list(ctx, sc, eargs);
                return Ok(Term::Ctor(
                    fact_ty,
                    ctx.interner.intern("NotRow"),
                    vec![rel, list],
                ));
            }
            elab_expr(ctx, sc, ast, id)
        }
        SExpr::App { func, args } => {
            if let SExpr::Path { path, ty_args } = ast.expr(func)
                && ty_args.is_empty()
            {
                let segs = ast.ident_slice(path);
                if segs
                    .last()
                    .and_then(|s| s.as_bytes().first())
                    .is_some_and(u8::is_ascii_lowercase)
                    && ctx.is_known_def(sc, segs)
                {
                    if matches!(
                        segs.last().copied(),
                        Some("arg" | "int_lit" | "bool_lit" | "str_lit" | "bigint" | "op_decl")
                    ) {
                        return elab_expr(ctx, sc, ast, id);
                    }
                    let q = ctx.resolve_sym(sc, segs)?;
                    let mut acc = Term::Global(q);
                    if let Some(&nt) = ctx.globals.def_tparams.get(&q) {
                        for _ in 0..nt {
                            acc = Term::App(Box::new(acc), Box::new(Term::Hole));
                        }
                    }
                    let aslice = ast.expr_slice(args);
                    if aslice.is_empty() {
                        return Ok(Term::App(Box::new(acc), Box::new(Term::Lit(Lit::Unit))));
                    }
                    for &a in aslice {
                        acc = Term::App(Box::new(acc), Box::new(elab_atom(ctx, sc, ast, a)?));
                    }
                    return Ok(acc);
                }
            }
            let rel = elab_rel_name(ctx, sc, ast, func)?;
            let aslice = ast.expr_slice(args);
            let mut eargs = Vec::with_capacity(aslice.len());
            for &a in aslice {
                eargs.push(elab_atom(ctx, sc, ast, a)?);
            }
            let list = mk_list(ctx, sc, eargs);
            Ok(Term::Ctor(
                fact_ty,
                ctx.interner.intern("Row"),
                vec![rel, list],
            ))
        }
        SExpr::Bin {
            op: BinOp::Eq,
            lhs,
            rhs,
        } => {
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            Ok(Term::Ctor(fact_ty, ctx.interner.intern("Eq"), vec![a, b]))
        }
        SExpr::Bin {
            op: BinOp::Neq,
            lhs,
            rhs,
        } => {
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            Ok(Term::Ctor(fact_ty, ctx.interner.intern("Neq"), vec![a, b]))
        }
        SExpr::Bin {
            op: BinOp::Lt,
            lhs,
            rhs,
        } => {
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            Ok(Term::Ctor(fact_ty, ctx.interner.intern("Lt"), vec![a, b]))
        }
        SExpr::Bin {
            op: BinOp::Le,
            lhs,
            rhs,
        } => {
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            Ok(Term::Ctor(fact_ty, ctx.interner.intern("Le"), vec![a, b]))
        }
        SExpr::Bin {
            op: BinOp::Gt,
            lhs,
            rhs,
        } => {
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            Ok(Term::Ctor(fact_ty, ctx.interner.intern("Gt"), vec![a, b]))
        }
        SExpr::Bin {
            op: BinOp::Ge,
            lhs,
            rhs,
        } => {
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            Ok(Term::Ctor(fact_ty, ctx.interner.intern("Ge"), vec![a, b]))
        }
        _ => elab_expr(ctx, sc, ast, id),
    }
}

fn elab_action<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    id: ExprId,
) -> Result<Term, Error> {
    let act_ty = ctx.resolve_single(sc, "Action");
    match ast.expr(id) {
        SExpr::App { func, args } => {
            let arg_ids = ast.expr_slice(args);
            if let SExpr::Path { path, ty_args } = ast.expr(func)
                && ty_args.is_empty()
            {
                match ast.ident_slice(path) {
                    ["union"] if arg_ids.len() == 2 => {
                        let a = elab_atom(ctx, sc, ast, arg_ids[0])?;
                        let b = elab_atom(ctx, sc, ast, arg_ids[1])?;
                        return Ok(Term::Ctor(act_ty, ctx.interner.intern("Union"), vec![a, b]));
                    }
                    ["delete"] if arg_ids.len() == 1 => {
                        if let SExpr::App {
                            func: inner_f,
                            args: inner_a,
                        } = ast.expr(arg_ids[0])
                        {
                            let rel = elab_rel_name(ctx, sc, ast, inner_f)?;
                            let aslice = ast.expr_slice(inner_a);
                            let mut eargs = Vec::with_capacity(aslice.len());
                            for &a in aslice {
                                eargs.push(elab_atom(ctx, sc, ast, a)?);
                            }
                            let list = mk_list(ctx, sc, eargs);
                            return Ok(Term::Ctor(
                                act_ty,
                                ctx.interner.intern("Delete"),
                                vec![rel, list],
                            ));
                        }
                    }
                    ["subsume"] if arg_ids.len() == 1 => {
                        if let SExpr::App {
                            func: inner_f,
                            args: inner_a,
                        } = ast.expr(arg_ids[0])
                        {
                            let rel = elab_rel_name(ctx, sc, ast, inner_f)?;
                            let aslice = ast.expr_slice(inner_a);
                            let mut eargs = Vec::with_capacity(aslice.len());
                            for &a in aslice {
                                eargs.push(elab_atom(ctx, sc, ast, a)?);
                            }
                            let list = mk_list(ctx, sc, eargs);
                            return Ok(Term::Ctor(
                                act_ty,
                                ctx.interner.intern("Subsume"),
                                vec![rel, list],
                            ));
                        }
                    }
                    ["panic"] if arg_ids.len() == 1 => {
                        let msg = elab_expr(ctx, sc, ast, arg_ids[0])?;
                        return Ok(Term::Ctor(act_ty, ctx.interner.intern("Panic"), vec![msg]));
                    }
                    segs if segs
                        .last()
                        .and_then(|s| s.as_bytes().first())
                        .is_some_and(u8::is_ascii_lowercase)
                        && ctx.is_known_def(sc, segs) =>
                    {
                        return elab_expr(ctx, sc, ast, id);
                    }
                    _ => {}
                }
            }
            let rel = elab_rel_name(ctx, sc, ast, func)?;
            let mut eargs = Vec::with_capacity(arg_ids.len());
            for &a in arg_ids {
                eargs.push(elab_atom(ctx, sc, ast, a)?);
            }
            let list = mk_list(ctx, sc, eargs);
            Ok(Term::Ctor(
                act_ty,
                ctx.interner.intern("Insert"),
                vec![rel, list],
            ))
        }
        SExpr::Bin {
            op: BinOp::Eq,
            lhs,
            rhs,
        } => {
            let a = elab_atom(ctx, sc, ast, lhs)?;
            let b = elab_atom(ctx, sc, ast, rhs)?;
            Ok(Term::Ctor(act_ty, ctx.interner.intern("Union"), vec![a, b]))
        }
        _ => elab_expr(ctx, sc, ast, id),
    }
}

fn elab_lam<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    params: &[(&'src str, Option<ExprId>)],
    body: ExprId,
) -> Result<Term, Error> {
    let Some((&(x, ty_opt), rest)) = params.split_first() else {
        return elab_expr(ctx, sc, ast, body);
    };
    let ann = match ty_opt {
        Some(t) => Some(Box::new(elab_type(ctx, sc, ast, t)?)),
        None => None,
    };
    let saved = ctx.locals.len();
    ctx.locals.push(x);
    let inner = elab_lam(ctx, sc, ast, rest, body);
    ctx.locals.truncate(saved);
    Ok(Term::Lam(ctx.interner.intern(x), ann, Rc::new(inner?)))
}

fn elab_expr<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    id: ExprId,
) -> Result<Term, Error> {
    match ast.expr(id) {
        SExpr::Lit(l) => Ok(Term::Lit(elab_lit(l)?)),
        SExpr::Sort(s) => Ok(Term::Sort(s)),
        SExpr::Pi { .. } | SExpr::RecordTy(_) => elab_type(ctx, sc, ast, id),
        SExpr::Record(fs) => {
            let fslice = ast.field_slice(fs);
            let mut fields = Vec::with_capacity(fslice.len());
            for &(k, v) in fslice {
                fields.push((ctx.interner.intern(k), elab_expr(ctx, sc, ast, v)?));
            }
            Ok(Term::Rec(fields))
        }
        SExpr::Proj { obj, field } => {
            let e = elab_expr(ctx, sc, ast, obj)?;
            Ok(Term::Proj(Box::new(e), ctx.interner.intern(field)))
        }
        SExpr::Not(inner) => {
            let e = elab_expr(ctx, sc, ast, inner)?;
            Ok(Term::Prim(PrimOp::Not, vec![e]))
        }
        SExpr::Neg(inner) => {
            let e = elab_expr(ctx, sc, ast, inner)?;
            Ok(Term::Prim(PrimOp::Sub, vec![Term::Lit(Lit::Int(0)), e]))
        }
        SExpr::Bin { op, lhs, rhs } => {
            let a = elab_expr(ctx, sc, ast, lhs)?;
            let b = elab_expr(ctx, sc, ast, rhs)?;
            Ok(Term::Prim(elab_binop(op), vec![a, b]))
        }
        SExpr::Try(inner) => {
            let try_fn = ctx.resolve_single(sc, "try_unwrap");
            let e = elab_expr(ctx, sc, ast, inner)?;
            Ok(Term::App(Box::new(Term::Global(try_fn)), Box::new(e)))
        }
        SExpr::Memo { params, body } => {
            let memo_fn = ctx.resolve_single(sc, "memo");
            let ps = ast.opt_param_slice(params);
            let lam = if ps.is_empty() {
                Term::Lam(
                    ctx.sym_wild,
                    Some(Box::new(Term::Ind(ctx.sym_unit, Vec::new()))),
                    Rc::new(elab_expr(ctx, sc, ast, body)?),
                )
            } else {
                elab_lam(ctx, sc, ast, ps, body)?
            };
            Ok(Term::App(Box::new(Term::Global(memo_fn)), Box::new(lam)))
        }
        SExpr::Lam { params, body, .. } => {
            elab_lam(ctx, sc, ast, ast.opt_param_slice(params), body)
        }
        SExpr::RecFix { params, ret, body } => {
            let ps = ast.opt_param_slice(params);
            let rty_id = ret.ok_or_else(|| Error("rec lambda needs return type".into()))?;
            let mut fty = elab_type(ctx, sc, ast, rty_id)?;
            for &(px, pty_opt) in ps.iter().rev() {
                let pty_id =
                    pty_opt.ok_or_else(|| Error(format!("rec lambda param {px} needs type")))?;
                let pty = elab_type(ctx, sc, ast, pty_id)?;
                fty = Term::Pi(ctx.interner.intern(px), Box::new(pty), Rc::new(fty));
            }
            let self_name = "__rec_self";
            let saved = ctx.locals.len();
            ctx.locals.push(self_name);
            let lam = elab_lam(ctx, sc, ast, ps, body);
            ctx.locals.truncate(saved);
            Ok(Term::Fix(
                ctx.interner.intern(self_name),
                Box::new(fty),
                Rc::new(lam?),
            ))
        }
        SExpr::Block { stmts, final_expr } => {
            elab_stmts(ctx, sc, ast, ast.stmt_slice(stmts), final_expr)
        }
        SExpr::Match { scrut, arms } => {
            let es = elab_expr(ctx, sc, ast, scrut)?;
            let aslice = ast.arm_slice(arms);
            let mut bs = Vec::with_capacity(aslice.len());
            for &(pat, rhs) in aslice {
                let saved = ctx.locals.len();
                let kp = match pat {
                    SPattern::Wild => Pattern::Wild,
                    SPattern::Var(x) => {
                        ctx.locals.push(x);
                        Pattern::Var(ctx.interner.intern(x))
                    }
                    SPattern::Lit(l) => Pattern::Lit(elab_lit(l)?),
                    SPattern::Ctor { path, fields } => {
                        let cp = ast.ident_slice(path);
                        let cname = *cp
                            .last()
                            .ok_or_else(|| Error("empty ctor pattern".into()))?;
                        let fnames = ast.ident_slice(fields);
                        let mut syms = Vec::with_capacity(fnames.len());
                        for &s in fnames {
                            ctx.locals.push(s);
                            syms.push(ctx.interner.intern(s));
                        }
                        Pattern::Ctor(ctx.interner.intern(cname), syms)
                    }
                };
                let erhs = elab_expr(ctx, sc, ast, rhs);
                ctx.locals.truncate(saved);
                bs.push((kp, erhs?));
            }
            Ok(Term::Match(Box::new(es), Rc::new(bs)))
        }
        SExpr::If {
            cond,
            then_expr,
            else_expr,
        } => {
            let c = elab_expr(ctx, sc, ast, cond)?;
            let t = elab_expr(ctx, sc, ast, then_expr)?;
            let e = elab_expr(ctx, sc, ast, else_expr)?;
            Ok(Term::Match(
                Box::new(c),
                Rc::new(vec![
                    (Pattern::Lit(Lit::Bool(true)), t),
                    (Pattern::Lit(Lit::Bool(false)), e),
                ]),
            ))
        }
        SExpr::Path { path, ty_args } => {
            let segs = ast.ident_slice(path);
            if let [x] = segs
                && ctx.is_local(x)
            {
                return Ok(Term::Var(ctx.interner.intern(x)));
            }
            let q = ctx.resolve_sym(sc, segs)?;
            if let Some(&ind) = ctx.globals.ctor_owner.get(&q) {
                let cname = *segs.last().unwrap();
                let nparams = ctx.globals.ind_params.get(&ind).copied().unwrap_or(0);
                let holes = vec![Term::Hole; nparams];
                Ok(Term::Ctor(ind, ctx.interner.intern(cname), holes))
            } else {
                let mut head = Term::Global(q);
                let targs = ast.expr_slice(ty_args);
                if !targs.is_empty() {
                    for &ta in targs {
                        head = Term::App(Box::new(head), Box::new(elab_type(ctx, sc, ast, ta)?));
                    }
                } else if let Some(&nt) = ctx.globals.def_tparams.get(&q) {
                    for _ in 0..nt {
                        head = Term::App(Box::new(head), Box::new(Term::Hole));
                    }
                }
                Ok(head)
            }
        }
        SExpr::App { func, args } => {
            let arg_ids = ast.expr_slice(args);
            if let SExpr::Path { path, ty_args } = ast.expr(func)
                && ty_args.is_empty()
            {
                let segs = ast.ident_slice(path);
                if segs == ["List"] {
                    let mut items = Vec::with_capacity(arg_ids.len());
                    for &a in arg_ids {
                        items.push(elab_expr(ctx, sc, ast, a)?);
                    }
                    return Ok(mk_list(ctx, sc, items));
                }
                if let [x] = segs
                    && ctx.is_local(x)
                {
                    let head = Term::Var(ctx.interner.intern(x));
                    if arg_ids.is_empty() {
                        return Ok(Term::App(Box::new(head), Box::new(Term::Lit(Lit::Unit))));
                    }
                    let mut acc = head;
                    for &a in arg_ids {
                        acc = Term::App(Box::new(acc), Box::new(elab_expr(ctx, sc, ast, a)?));
                    }
                    return Ok(acc);
                }
                let q = ctx.resolve_sym(sc, segs)?;
                if let Some(&ind) = ctx.globals.ctor_owner.get(&q) {
                    let cname = *segs.last().unwrap();
                    let nparams = ctx.globals.ind_params.get(&ind).copied().unwrap_or(0);
                    let mut cargs = Vec::with_capacity(nparams + arg_ids.len());
                    cargs.resize(nparams, Term::Hole);
                    for &a in arg_ids {
                        cargs.push(elab_expr(ctx, sc, ast, a)?);
                    }
                    return Ok(Term::Ctor(ind, ctx.interner.intern(cname), cargs));
                }
                let mut acc = Term::Global(q);
                if let Some(&nt) = ctx.globals.def_tparams.get(&q) {
                    for _ in 0..nt {
                        acc = Term::App(Box::new(acc), Box::new(Term::Hole));
                    }
                }
                if arg_ids.is_empty() {
                    return Ok(Term::App(Box::new(acc), Box::new(Term::Lit(Lit::Unit))));
                }
                for &a in arg_ids {
                    acc = Term::App(Box::new(acc), Box::new(elab_expr(ctx, sc, ast, a)?));
                }
                return Ok(acc);
            }
            let ef = elab_expr(ctx, sc, ast, func)?;
            if arg_ids.is_empty() {
                return Ok(Term::App(Box::new(ef), Box::new(Term::Lit(Lit::Unit))));
            }
            let mut acc = ef;
            for &a in arg_ids {
                acc = Term::App(Box::new(acc), Box::new(elab_expr(ctx, sc, ast, a)?));
            }
            Ok(acc)
        }
        SExpr::OpSugar(fs) => elab_op_sugar(ctx, sc, ast, fs, "", ""),
    }
}

fn to_pascal_op(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    let mut cap = true;
    for c in name.chars() {
        if c == '_' {
            cap = true;
        } else if cap {
            out.push(c.to_ascii_uppercase());
            cap = false;
        } else {
            out.push(c);
        }
    }
    out.push_str("Op");
    out
}

fn extract_op_sugar_meta<'src>(
    prefix: &str,
    name: &str,
    tparams: Slice,
    fs: Slice,
    ast: &Ast<'src>,
) -> OpSugarMeta {
    let field_map = ast.field_slice(fs);
    let get_lit_str = |k: &str| -> Option<String> {
        field_map
            .iter()
            .find(|&&(fk, _)| fk == k)
            .and_then(|&(_, v)| match ast.expr(v) {
                SExpr::Lit(SLit::Str(s)) => Some(decode_string_inner(s).into_owned()),
                _ => None,
            })
    };
    let dialect = get_lit_str("dialect")
        .unwrap_or_else(|| prefix.rsplit("::").next().unwrap_or("").to_owned());
    let op_name =
        get_lit_str("name").unwrap_or_else(|| name.strip_prefix("r#").unwrap_or(name).to_owned());
    let tparam_kinds = ast
        .field_slice(tparams)
        .iter()
        .map(|&(_, ty_id)| match ast.expr(ty_id) {
            SExpr::Sort(crate::ast::Sort::Type) => OpTParamKind::Type,
            SExpr::Path { path, .. } => {
                let segs = ast.ident_slice(path);
                if matches!(segs, ["Nat"] | ["Int"] | ["std", "core", "Nat"]) {
                    OpTParamKind::Int
                } else if segs.len() == 1 && !prefix.is_empty() {
                    OpTParamKind::Enum(format!("{prefix}::{}", segs[0]))
                } else {
                    OpTParamKind::Enum(segs.join("::"))
                }
            }
            _ => OpTParamKind::Int,
        })
        .collect();
    let fixed_operands = if let Some(&(_, ops_id)) =
        field_map.iter().find(|&&(fk, _)| fk == "sig_operands")
        && let SExpr::RecordTy(pfs) = ast.expr(ops_id)
    {
        let ports = ast.field_slice(pfs);
        let all_fixed = ports.iter().all(|&(_, pt)| match ast.expr(pt) {
            SExpr::Path { path, .. } => !matches!(
                ast.ident_slice(path).last().copied(),
                Some("Option" | "Optional" | "Variadic")
            ),
            _ => true,
        });
        all_fixed.then_some(ports.len())
    } else {
        None
    };
    let is_constant_like = field_map
        .iter()
        .find(|&&(fk, _)| fk == "interfaces")
        .is_some_and(|&(_, iv)| {
            if let SExpr::App { args, .. } = ast.expr(iv) {
                ast.expr_slice(args).iter().any(|&a| {
                    if let SExpr::Path { path, .. } = ast.expr(a) {
                        matches!(
                            ast.ident_slice(path).last().copied(),
                            Some("ConstantLike" | "ConstantLikeExternal")
                        )
                    } else {
                        false
                    }
                })
            } else {
                false
            }
        });
    OpSugarMeta {
        dialect,
        op_name,
        tparam_kinds,
        fixed_operands,
        is_constant_like,
    }
}

fn elab_typed_ports<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    port_ty: Sym,
    port_ctor: Sym,
    rec_id: ExprId,
) -> Result<Vec<(String, Term)>, Error> {
    let mut out = Vec::new();
    match ast.expr(rec_id) {
        SExpr::RecordTy(pfs) => {
            for &(pname, pty) in ast.field_slice(pfs) {
                let kt = elab_type(ctx, sc, ast, pty)?;
                let _ = (port_ty, port_ctor);
                out.push((pname.to_owned(), kt));
            }
        }
        _ => {
            // Named record type alias (e.g. `attributes: ConstantAttributes`)
            let kt = elab_type(ctx, sc, ast, rec_id)?;
            out.push(("__record".to_owned(), kt));
        }
    }
    Ok(out)
}

fn elab_interface_list<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    list_expr: ExprId,
) -> Result<Term, Error> {
    let iface_ty = ctx.resolve_single(sc, "Interface");
    let SExpr::App { args, .. } = ast.expr(list_expr) else {
        return elab_expr(ctx, sc, ast, list_expr);
    };
    let mut items = Vec::new();
    for &item_id in ast.expr_slice(args) {
        let term = match ast.expr(item_id) {
            SExpr::Path { path, ty_args } if ty_args.is_empty() => {
                let segs = ast.ident_slice(path);
                let q = ctx.resolve_sym(sc, segs)?;
                if ctx.globals.ctor_owner.contains_key(&q) {
                    elab_expr(ctx, sc, ast, item_id)?
                } else if let [bare] = segs {
                    Term::Ctor(iface_ty, ctx.interner.intern(bare), Vec::new())
                } else {
                    elab_expr(ctx, sc, ast, item_id)?
                }
            }
            SExpr::App { func, args: iargs } => {
                if let SExpr::Path { path, ty_args } = ast.expr(func)
                    && ty_args.is_empty()
                    && let [bare] = ast.ident_slice(path)
                {
                    let q = ctx.resolve_sym(sc, &[bare])?;
                    if !ctx.globals.ctor_owner.contains_key(&q) {
                        let mut cargs = Vec::new();
                        for &ia in ast.expr_slice(iargs) {
                            cargs.push(elab_expr(ctx, sc, ast, ia)?);
                        }
                        Term::Ctor(iface_ty, ctx.interner.intern(bare), cargs)
                    } else {
                        elab_expr(ctx, sc, ast, item_id)?
                    }
                } else {
                    elab_expr(ctx, sc, ast, item_id)?
                }
            }
            _ => elab_expr(ctx, sc, ast, item_id)?,
        };
        items.push(term);
    }
    Ok(mk_list(ctx, sc, items))
}

#[allow(clippy::too_many_arguments)]
fn elab_sem_field<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    sem_ty: Sym,
    expr_id: Option<ExprId>,
    attr_rec_ty: Option<&Term>,
    operand_tys: &[(String, Term)],
    ret_ty: Term,
) -> Result<Term, Error> {
    let Some(eid) = expr_id else {
        return Ok(Term::Ctor(sem_ty, ctx.interner.intern("None"), Vec::new()));
    };
    let e = elab_expr(ctx, sc, ast, eid)?;
    // Derive the expected Pi type from `signature` + `attributes` so `check.rs`
    // typechecks unannotated lambdas like `|self| ...` and `|lhs, rhs| ...`!
    let mut param_tys: Vec<Term> = Vec::new();
    if let SExpr::Lam { params, .. } = ast.expr(eid) {
        let ps = ast.opt_param_slice(params);
        let mut op_idx = 0usize;
        for &(pname, pty_opt) in ps {
            if let Some(pty) = pty_opt {
                param_tys.push(elab_type(ctx, sc, ast, pty)?);
            } else if pname == "self" {
                param_tys.push(
                    attr_rec_ty
                        .cloned()
                        .unwrap_or_else(|| Term::RecTy(Vec::new())),
                );
            } else if let Some((_, oty)) = operand_tys.get(op_idx) {
                param_tys.push(oty.clone());
                op_idx += 1;
            } else {
                param_tys.push(Term::Ind(ctx.sym_unit, Vec::new()));
            }
        }
    }
    let mut fn_ty = ret_ty;
    for pty in param_tys.into_iter().rev() {
        fn_ty = Term::Pi(ctx.sym_wild, Box::new(pty), Rc::new(fn_ty));
    }
    Ok(Term::Ctor(
        sem_ty,
        ctx.interner.intern("Body"),
        vec![fn_ty, e],
    ))
}

fn elab_op_sugar<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    fs: Slice,
    default_dialect: &str,
    default_name: &str,
) -> Result<Term, Error> {
    let op_ty = ctx.resolve_single(sc, "OpDef");
    let def_ctor = ctx.interner.intern("Define");
    let q_def_ctor = {
        let mut s = ctx.interner.resolve(op_ty).to_owned();
        s.push_str("::Define");
        ctx.interner.intern(&s)
    };
    let arity = ctx
        .globals
        .ctor_arity
        .get(&q_def_ctor)
        .copied()
        .unwrap_or(9);
    let field_map = ast.field_slice(fs);

    if arity <= 9 {
        // Legacy 9-field `OpDef::Define` (used by standalone prototype tests/benches).
        let get_str = |ctx: &mut ElabCtx<'src>, k: &str, def: &str| -> Result<Term, Error> {
            if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == k) {
                elab_expr(ctx, sc, ast, v)
            } else {
                Ok(Term::Lit(Lit::Str(Rc::from(def))))
            }
        };
        let get_list = |ctx: &mut ElabCtx<'src>, k: &str| -> Result<Term, Error> {
            if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == k) {
                if k == "interfaces" {
                    elab_interface_list(ctx, sc, ast, v)
                } else {
                    elab_expr(ctx, sc, ast, v)
                }
            } else {
                Ok(mk_list(ctx, sc, Vec::new()))
            }
        };
        let args = vec![
            get_str(ctx, "dialect", default_dialect)?,
            get_str(ctx, "name", default_name)?,
            get_str(ctx, "rust_name", &to_pascal_op(default_name))?,
            get_list(ctx, "operands")?,
            get_list(ctx, "results")?,
            get_list(ctx, "attributes")?,
            get_list(ctx, "regions")?,
            get_list(ctx, "successors")?,
            get_list(ctx, "interfaces")?,
        ];
        return Ok(Term::Ctor(op_ty, def_ctor, args));
    }

    // Full 14-field typed `OpDef::Define` using CDSL's dependent type system!
    let port_ty = ctx.resolve_single(sc, "Port");
    let port_ctor = ctx.interner.intern("Port");
    let sem_ty = ctx.resolve_single(sc, "Sem");
    let bind_ty = ctx.resolve_single(sc, "BindSpec");
    let counted_ty = ctx.resolve_single(sc, "CountedSpec");

    let dialect_t = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "dialect") {
        elab_expr(ctx, sc, ast, v)?
    } else {
        Term::Lit(Lit::Str(Rc::from(default_dialect)))
    };
    let name_str = field_map
        .iter()
        .find(|&&(fk, _)| fk == "name")
        .and_then(|&(_, v)| match ast.expr(v) {
            SExpr::Lit(SLit::Str(s)) => Some(decode_string_inner(s).into_owned()),
            _ => None,
        })
        .unwrap_or_else(|| default_name.to_owned());
    let name_t = Term::Lit(Lit::Str(Rc::from(name_str.as_str())));
    let rust_name_t = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "rust_name") {
        elab_expr(ctx, sc, ast, v)?
    } else {
        Term::Lit(Lit::Str(Rc::from(to_pascal_op(&name_str).as_str())))
    };

    let to_port_list = |ctx: &mut ElabCtx<'src>, items: &[(String, Term)]| -> Term {
        let ps = items
            .iter()
            .map(|(n, ty)| {
                Term::Ctor(
                    port_ty,
                    port_ctor,
                    vec![Term::Lit(Lit::Str(Rc::from(n.as_str()))), ty.clone()],
                )
            })
            .collect();
        mk_list(ctx, sc, ps)
    };

    let operand_pairs =
        if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "sig_operands") {
            elab_typed_ports(ctx, sc, ast, port_ty, port_ctor, v)?
        } else {
            Vec::new()
        };
    let result_pairs = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "sig_results")
    {
        elab_typed_ports(ctx, sc, ast, port_ty, port_ctor, v)?
    } else {
        Vec::new()
    };
    let (attr_pairs, attr_rec_ty) =
        if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "sig_attributes") {
            let pairs = elab_typed_ports(ctx, sc, ast, port_ty, port_ctor, v)?;
            let rty = elab_type(ctx, sc, ast, v)?;
            (pairs, Some(rty))
        } else {
            (Vec::new(), None)
        };
    let region_pairs = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "sig_regions")
    {
        elab_typed_ports(ctx, sc, ast, port_ty, port_ctor, v)?
    } else {
        Vec::new()
    };
    let succ_pairs =
        if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "sig_successors") {
            elab_typed_ports(ctx, sc, ast, port_ty, port_ctor, v)?
        } else {
            Vec::new()
        };

    let interfaces_t = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "interfaces")
    {
        elab_interface_list(ctx, sc, ast, v)?
    } else {
        mk_list(ctx, sc, Vec::new())
    };

    let exec_ret = result_pairs
        .first()
        .map(|(_, t)| t.clone())
        .unwrap_or_else(|| Term::Ind(ctx.sym_unit, Vec::new()));
    let exec_id = field_map
        .iter()
        .find(|&&(fk, _)| fk == "exec")
        .map(|&(_, v)| v);
    let exec_t = elab_sem_field(
        ctx,
        sc,
        ast,
        sem_ty,
        exec_id,
        attr_rec_ty.as_ref(),
        &operand_pairs,
        exec_ret,
    )?;

    let poison_id = field_map
        .iter()
        .find(|&&(fk, _)| fk == "poison")
        .map(|&(_, v)| v);
    let poison_t = elab_sem_field(
        ctx,
        sc,
        ast,
        sem_ty,
        poison_id,
        attr_rec_ty.as_ref(),
        &operand_pairs,
        Term::Ind(ctx.sym_bool, Vec::new()),
    )?;

    let binds_t = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "binds") {
        elab_expr(ctx, sc, ast, v)?
    } else {
        Term::Ctor(bind_ty, ctx.interner.intern("None"), Vec::new())
    };
    let counted_t = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "counted") {
        elab_expr(ctx, sc, ast, v)?
    } else {
        Term::Ctor(counted_ty, ctx.interner.intern("None"), Vec::new())
    };
    let imms_t = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "immediates") {
        elab_expr(ctx, sc, ast, v)?
    } else {
        mk_list(ctx, sc, Vec::new())
    };
    let asm_pairs = if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == "sig_assembly") {
        elab_typed_ports(ctx, sc, ast, port_ty, port_ctor, v)?
    } else {
        Vec::new()
    };
    let flag_field = |ctx: &mut ElabCtx<'src>, key: &str| -> Result<Term, Error> {
        if let Some(&(_, v)) = field_map.iter().find(|&&(fk, _)| fk == key) {
            elab_expr(ctx, sc, ast, v)
        } else {
            Ok(Term::Lit(Lit::Bool(false)))
        }
    };
    let flags_ty = ctx.resolve_single(sc, "OpFlags");
    let flags_t = Term::Ctor(
        flags_ty,
        ctx.interner.intern("Flags"),
        vec![
            flag_field(ctx, "macro_def")?,
            flag_field(ctx, "custom_format")?,
            flag_field(ctx, "widened")?,
            flag_field(ctx, "format")?,
            flag_field(ctx, "machine")?,
        ],
    );

    let operands_t = to_port_list(ctx, &operand_pairs);
    let results_t = to_port_list(ctx, &result_pairs);
    let attrs_t = to_port_list(ctx, &attr_pairs);
    let regions_t = to_port_list(ctx, &region_pairs);
    let succs_t = to_port_list(ctx, &succ_pairs);
    let asm_t = to_port_list(ctx, &asm_pairs);

    Ok(Term::Ctor(
        op_ty,
        def_ctor,
        vec![
            dialect_t,
            name_t,
            rust_name_t,
            operands_t,
            results_t,
            attrs_t,
            regions_t,
            succs_t,
            interfaces_t,
            exec_t,
            poison_t,
            binds_t,
            counted_t,
            imms_t,
            asm_t,
            flags_t,
        ],
    ))
}

fn elab_stmts<'src>(
    ctx: &mut ElabCtx<'src>,
    sc: &ModScope,
    ast: &Ast<'src>,
    stmts: &[SStmt<'src>],
    final_expr: ExprId,
) -> Result<Term, Error> {
    let Some((&first, rest)) = stmts.split_first() else {
        return elab_expr(ctx, sc, ast, final_expr);
    };
    match first {
        SStmt::Let { name, ty, rhs } => {
            let rhs_t = elab_expr(ctx, sc, ast, rhs)?;
            let ann_t = match ty {
                Some(t) => Some(Box::new(elab_type(ctx, sc, ast, t)?)),
                None => None,
            };
            let saved = ctx.locals.len();
            ctx.locals.push(name);
            let body_t = elab_stmts(ctx, sc, ast, rest, final_expr);
            ctx.locals.truncate(saved);
            Ok(Term::Let(
                ctx.interner.intern(name),
                ann_t,
                Box::new(rhs_t),
                Box::new(body_t?),
            ))
        }
        SStmt::Rule {
            name,
            vars,
            kind,
            lhs,
            rhs,
        } => {
            let r = elab_rule_sugar(ctx, sc, ast, name, vars, kind, lhs, rhs)?;
            let saved = ctx.locals.len();
            ctx.locals.push(name);
            let body_t = elab_stmts(ctx, sc, ast, rest, final_expr);
            ctx.locals.truncate(saved);
            Ok(Term::Let(
                ctx.interner.intern(name),
                None,
                Box::new(r),
                Box::new(body_t?),
            ))
        }
        SStmt::Expr(ex) => {
            let rhs_t = elab_expr(ctx, sc, ast, ex)?;
            let body_t = elab_stmts(ctx, sc, ast, rest, final_expr)?;
            Ok(Term::Let(
                ctx.sym_wild,
                None,
                Box::new(rhs_t),
                Box::new(body_t),
            ))
        }
    }
}
