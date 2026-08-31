use std::collections::HashMap;
use std::mem;

use chumsky::error::Rich;

use crate::{
    Span, Substitution, Type, TypeEnv, TypeScheme, TypeVar, TypeVarGen, ast, unify, utils,
};

type Diag = Rich<'static, String, Span>;
type TypeCache<'a> = HashMap<&'a ast::Expr, Type>;

/// Maps register class names to their resolved bit type (may contain TypeVars).
type SynonymTable = HashMap<String, Type>;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn check<'a>(files: &'a [ast::File]) -> (TypeCache<'a>, Vec<(String, Diag)>) {
    let mut tvg = TypeVarGen::new();
    let mut diags = vec![];
    let mut cache = TypeCache::new();

    let isa_param_vars = build_isa_param_vars(files, &mut tvg);
    let synonyms = build_synonym_table(files, &isa_param_vars, &mut tvg);
    let encoding_lens = crate::encoding::register_class_widths(files);

    let item_cache: HashMap<&str, &ast::Item> = files
        .iter()
        .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
        .collect();

    for file in files {
        for instr in file.instructions() {
            let env = build_instr_env(
                instr,
                &item_cache,
                &synonyms,
                &isa_param_vars,
                &encoding_lens,
                &mut tvg,
            );
            let mut subst = Substitution::new();
            infer(
                &instr.behavior,
                &env,
                &mut tvg,
                &mut subst,
                &mut cache,
                &mut diags,
                &file.file_name,
            );
        }
        for item in &file.items {
            let ast::Item::Isa(isa) = item else { continue };
            let Some(trap) = &isa.trap_handler else {
                continue;
            };
            let mut env = TypeEnv::new();
            for (name, ty) in &synonyms {
                env.bind(name.clone(), TypeScheme::mono(ty.clone()));
            }
            for name in isa_param_vars.keys() {
                env.bind(name.clone(), TypeScheme::mono(Type::Integer));
            }
            // Trap parameters carry exception payloads: bits of some width.
            for param in &trap.params {
                env.bind(
                    param.clone(),
                    TypeScheme::mono(Type::Con("bits".into(), vec![Type::Var(tvg.fresh())])),
                );
            }
            let mut subst = Substitution::new();
            infer(
                &trap.body,
                &env,
                &mut tvg,
                &mut subst,
                &mut cache,
                &mut diags,
                &file.file_name,
            );
        }
    }

    (cache, diags)
}

// ---------------------------------------------------------------------------
// Environment setup
// ---------------------------------------------------------------------------

fn build_isa_param_vars(files: &[ast::File], tvg: &mut TypeVarGen) -> HashMap<String, TypeVar> {
    let mut vars: HashMap<String, TypeVar> = HashMap::new();
    for file in files {
        for item in &file.items {
            if let ast::Item::Isa(isa) = item {
                for param_name in isa.parameters.keys() {
                    vars.entry(param_name.clone())
                        .or_insert_with(|| tvg.fresh());
                }
            }
        }
    }
    vars
}

fn reg_class_type(
    rc: &ast::RegisterClass,
    isa_param_vars: &HashMap<String, TypeVar>,
    tvg: &mut TypeVarGen,
) -> Type {
    // A static-width class fixes its width to an ISA parameter (e.g. `XLEN`); the
    // operand is `bits<XLEN>`. A class whose `WIDTH` is any other expression is
    // dynamically sized: its width is an architectural quantity not known at
    // spec time (RVV's `VLEN`, reported at runtime by `vlenb`), so the operand is
    // `bits<?>` — a bitvector of unknown width. The element structure such a
    // register carries is imposed by the instructions that read it, not by the
    // register type, matching hardware where one physical file is reused across
    // element widths.
    if let Some((_ty, Some(default))) = rc.parameters.get("WIDTH")
        && let ast::Expr::Field(field) = default
        && let Some(&tv) = isa_param_vars.get(&field.member)
    {
        return Type::Con("bits".into(), vec![Type::Var(tv)]);
    }
    Type::Con("bits".into(), vec![Type::Var(tvg.fresh())])
}

fn build_synonym_table(
    files: &[ast::File],
    isa_param_vars: &HashMap<String, TypeVar>,
    tvg: &mut TypeVarGen,
) -> SynonymTable {
    let mut synonyms = SynonymTable::new();
    for file in files {
        for rc in file.register_classes() {
            synonyms.insert(rc.name.clone(), reg_class_type(rc, isa_param_vars, tvg));
        }
    }
    synonyms
}

fn normalize(ty: &Type, synonyms: &SynonymTable) -> Type {
    match ty {
        Type::Struct(name) => synonyms.get(name).cloned().unwrap_or_else(|| ty.clone()),
        other => other.clone(),
    }
}

fn build_instr_env<'a>(
    instr: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
    synonyms: &SynonymTable,
    isa_param_vars: &HashMap<String, TypeVar>,
    encoding_lens: &HashMap<String, u16>,
    tvg: &mut TypeVarGen,
) -> TypeEnv {
    let mut env = TypeEnv::new();
    for (name, ty) in utils::resolve_operands_for_instruction(instr, item_cache) {
        // A register operand carries two things a spec can ask for: the bits it
        // holds and the architectural number the encoding spells. Both are in
        // scope as projections; the bare name is the value, which is what a
        // behavior means by an operand.
        if let Type::Struct(class) = &ty {
            env.bind(
                format!("{name}.value"),
                TypeScheme::mono(normalize(&ty, synonyms)),
            );
            if let Some(&len) = encoding_lens.get(class) {
                env.bind(format!("{name}.index"), TypeScheme::mono(Type::Bits(len)));
            }
        }
        // A `bits<expr>` width is ISA-dependent, so across ISAs the operand is
        // "bits of some width", like a register class with symbolic WIDTH.
        let ty = match ty {
            Type::BitsExpr(_) => Type::Con("bits".into(), vec![Type::Var(tvg.fresh())]),
            other => other,
        };
        env.bind(name, TypeScheme::mono(normalize(&ty, synonyms)));
    }
    for (name, ty) in synonyms {
        env.bind(name.clone(), TypeScheme::mono(ty.clone()));
    }
    for name in isa_param_vars.keys() {
        env.bind(name.clone(), TypeScheme::mono(Type::Integer));
    }
    env
}

// ---------------------------------------------------------------------------
// Type inference
// ---------------------------------------------------------------------------

/// The concrete width of a resolved bit type, or `None` while it is still a
/// variable (a register class whose `WIDTH` is an ISA parameter, say).
fn bit_width(ty: &Type) -> Option<u16> {
    match ty {
        Type::Bits(n) => Some(*n),
        Type::Con(name, args) if name == "bits" && args.len() == 1 => bit_width(&args[0]),
        _ => None,
    }
}

/// Whether `ty` is a bitvector, whatever its width. A register operand whose
/// class takes its `WIDTH` from an ISA parameter is one of these: known to be
/// bits, not known to be any particular number of them.
fn is_bits(ty: &Type) -> bool {
    matches!(ty, Type::Bits(_))
        || matches!(ty, Type::Con(name, args) if name == "bits" && args.len() == 1)
}

/// Constrain `from` to be usable where `to` is expected. Widening is implicit
/// (zero-extension); narrowing has to be written `as bits<N>`.
fn coerce(
    from: &Type,
    to: &Type,
    subst: &mut Substitution,
    span: Span,
    diags: &mut Vec<(String, Diag)>,
    file_name: &str,
) {
    let (from, to) = (from.apply(subst), to.apply(subst));
    let message = match (bit_width(&from), bit_width(&to)) {
        (Some(from_width), Some(to_width)) if from_width > to_width => format!(
            "implicit narrowing from bits<{from_width}> to bits<{to_width}>; \
             write the truncation as `as bits<{to_width}>`"
        ),
        (Some(_), Some(_)) => return,
        // The value is bits of a width this spec never pins down (an
        // ISA-parameter register width): whether it fits cannot be decided, so
        // the truncation has to be written.
        (None, Some(to_width)) if is_bits(&from) => format!(
            "value of unknown width used as bits<{to_width}>; \
             write the truncation as `as bits<{to_width}>`"
        ),
        _ => return constrain(&from, &to, subst, span, diags, file_name),
    };
    diags.push((file_name.to_string(), Rich::custom(span, message)));
}

/// The type of two operands used together: the wider of the two, since the
/// narrower zero-extends. Widths still unresolved unify instead.
fn join(
    lhs: &Type,
    rhs: &Type,
    subst: &mut Substitution,
    span: Span,
    diags: &mut Vec<(String, Diag)>,
    file_name: &str,
) -> Type {
    let (lhs, rhs) = (lhs.apply(subst), rhs.apply(subst));
    match (bit_width(&lhs), bit_width(&rhs)) {
        (Some(l), Some(r)) => {
            if l >= r {
                lhs
            } else {
                rhs
            }
        }
        _ => {
            constrain(&lhs, &rhs, subst, span, diags, file_name);
            lhs.apply(subst)
        }
    }
}

fn constrain(
    t1: &Type,
    t2: &Type,
    subst: &mut Substitution,
    span: Span,
    diags: &mut Vec<(String, Diag)>,
    file_name: &str,
) {
    match unify(&t1.apply(subst), &t2.apply(subst)) {
        Ok(s) => {
            let old = mem::take(subst);
            *subst = old.compose(&s);
        }
        Err(e) => {
            diags.push((file_name.to_string(), Rich::custom(span, e.to_string())));
        }
    }
}

fn infer<'a>(
    expr: &'a ast::Expr,
    env: &TypeEnv,
    tvg: &mut TypeVarGen,
    subst: &mut Substitution,
    cache: &mut TypeCache<'a>,
    diags: &mut Vec<(String, Diag)>,
    file_name: &str,
) -> Type {
    let ty = match expr {
        ast::Expr::Ident(id) => match env.get(&id.name) {
            Some(scheme) => scheme.ty.apply(subst),
            None => {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(id.span, format!("unbound variable '{}'", id.name)),
                ));
                Type::Var(tvg.fresh())
            }
        },

        // A literal has no width of its own; unification with the use site
        // gives it one (or leaves it a spec-time `Integer`).
        ast::Expr::Lit(ast::Lit::Int(_)) => Type::Num(tvg.fresh()),
        ast::Expr::Lit(ast::Lit::Str(_)) => Type::String,

        ast::Expr::Binary(bin) => {
            let lhs_ty = infer(&bin.lhs, env, tvg, subst, cache, diags, file_name);
            let rhs_ty = infer(&bin.rhs, env, tvg, subst, cache, diags, file_name);
            match bin.op {
                ast::BinOp::Add
                | ast::BinOp::Sub
                | ast::BinOp::Mul
                | ast::BinOp::Div
                | ast::BinOp::UnsignedDiv
                | ast::BinOp::BitwiseAnd
                | ast::BinOp::BitwiseOr
                | ast::BinOp::BitwiseXor => {
                    join(&lhs_ty, &rhs_ty, subst, bin.span, diags, file_name)
                }
                // Shifts: result is the LHS type; RHS is unconstrained so that
                // both register operands (bits<N>) and clamp results (Integer) are accepted.
                ast::BinOp::ShiftLeftLogical
                | ast::BinOp::ShiftRightLogical
                | ast::BinOp::ShiftRightArithmetic => lhs_ty.apply(subst),
                ast::BinOp::Equal
                | ast::BinOp::NotEqual
                | ast::BinOp::LessThan
                | ast::BinOp::GreaterThan
                | ast::BinOp::LessThenEqual
                | ast::BinOp::GreaterThanEqual
                | ast::BinOp::UnsignedLessThan
                | ast::BinOp::UnsignedGreaterThan
                | ast::BinOp::UnsignedLessThenEqual
                | ast::BinOp::UnsignedGreaterThanEqual => {
                    join(&lhs_ty, &rhs_ty, subst, bin.span, diags, file_name);
                    Type::Bits(1)
                }
            }
        }

        ast::Expr::Assign(asgn) => {
            let dest_ty = infer(&asgn.dest, env, tvg, subst, cache, diags, file_name);
            let val_ty = infer(&asgn.value, env, tvg, subst, cache, diags, file_name);
            coerce(&val_ty, &dest_ty, subst, asgn.span, diags, file_name);
            val_ty.apply(subst)
        }

        // A `let` outside a block introduces no visible scope; its type is the
        // bound value's. Blocks bind the name (see the `Block` arm below).
        ast::Expr::Let(binding) => infer_let(binding, env, tvg, subst, cache, diags, file_name),

        ast::Expr::Path(path) => {
            // `Ordering::<member>` is a memory-ordering constant of type `bits<3>`,
            // resolved before register-class lookup since `Ordering` is not a class.
            if path.base == "Ordering" {
                return Type::Bits(3);
            }
            if path.remainder.len() != 1 {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        path.span,
                        format!(
                            "path '{}' must have exactly one register component",
                            format_args!("{}::{}", path.base, path.remainder.join("::"))
                        ),
                    ),
                ));
                Type::Var(tvg.fresh())
            } else {
                match env.get(&path.base) {
                    Some(scheme) => scheme.ty.apply(subst),
                    None => {
                        diags.push((
                            file_name.to_string(),
                            Rich::custom(
                                path.span,
                                format!("unknown register class '{}'", path.base),
                            ),
                        ));
                        Type::Var(tvg.fresh())
                    }
                }
            }
        }

        ast::Expr::Block(block) => {
            // Statements see a block-local environment: an assignment to a
            // fresh name introduces a behavior-local binding visible to the
            // later statements of the same block. The binding is the value's
            // type; reads of it are just reads of the defining expression
            // (the lowering substitutes it).
            let mut block_env = env.clone();
            let mut ty = Type::Integer;
            for stmt in &block.stmts {
                if let ast::Expr::Let(binding) = stmt {
                    let val_ty =
                        infer_let(binding, &block_env, tvg, subst, cache, diags, file_name);
                    block_env.bind(binding.name.clone(), TypeScheme::mono(val_ty.clone()));
                    cache.insert(stmt, val_ty.clone());
                    ty = val_ty;
                    continue;
                }
                if let ast::Expr::Assign(asgn) = stmt
                    && let ast::Expr::Ident(id) = &*asgn.dest
                    && block_env.get(&id.name).is_none()
                {
                    let val_ty =
                        infer(&asgn.value, &block_env, tvg, subst, cache, diags, file_name);
                    block_env.bind(id.name.clone(), TypeScheme::mono(val_ty.clone()));
                    cache.insert(stmt, val_ty.clone());
                    ty = val_ty;
                    continue;
                }
                ty = infer(stmt, &block_env, tvg, subst, cache, diags, file_name);
            }
            if block.last_expr_return {
                ty
            } else {
                Type::Integer
            }
        }

        ast::Expr::Field(field) => {
            // An operand projection (`rs1.index`, `rs1.value`) is in scope under
            // its spelled name.
            if let ast::Expr::Ident(base) = &*field.base
                && matches!(field.member.as_str(), "value" | "index")
            {
                return match env.get(format!("{}.{}", base.name, field.member)) {
                    Some(scheme) => scheme.ty.apply(subst),
                    None => {
                        diags.push((
                            file_name.to_string(),
                            Rich::custom(
                                field.span,
                                format!(
                                    "'{}' is not a register operand, so it has no \
                                     '.{}' projection",
                                    base.name, field.member
                                ),
                            ),
                        ));
                        Type::Var(tvg.fresh())
                    }
                };
            }
            // Otherwise only `self.MEMBER` is supported; the member is resolved
            // as an ISA parameter.
            let is_self = matches!(&*field.base, ast::Expr::Ident(id) if id.name == "self");
            if is_self {
                match env.get(&field.member) {
                    Some(scheme) => scheme.ty.apply(subst),
                    None => {
                        diags.push((
                            file_name.to_string(),
                            Rich::custom(
                                field.span,
                                format!("unknown ISA parameter 'self.{}'", field.member),
                            ),
                        ));
                        Type::Var(tvg.fresh())
                    }
                }
            } else {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(field.span, "unsupported field access".to_string()),
                ));
                Type::Var(tvg.fresh())
            }
        }

        ast::Expr::Call(call) => match &*call.callee {
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Clamp) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Integer
            }
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Extract) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Bitcast) => {
                let Some(input) = call.arguments.first() else {
                    return Type::Var(tvg.fresh());
                };
                let ty = infer(input, env, tvg, subst, cache, diags, file_name);
                for arg in &call.arguments[1..] {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                ty.apply(subst)
            }
            // `todo()` stands in for unmodeled semantics; it takes no arguments
            // and unifies with whatever context uses it.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Todo) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Log2Ceil) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                // A count: Integer in spec-time expressions (widths), but a
                // small bitvector when computed from a runtime register value.
                Type::Var(tvg.fresh())
            }
            // `regnum(op)` -> bits<ENCODING_LEN>: the operand's encoding index.
            // The width is the operand class's `ENCODING_LEN`, not tracked here;
            // a fresh var lets the surrounding comparison fix it.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Regnum) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::SExt)
            | ast::Expr::BuiltinFunction(ast::BuiltinFunction::ZExt)
            | ast::Expr::BuiltinFunction(ast::BuiltinFunction::Load)
            | ast::Expr::BuiltinFunction(ast::BuiltinFunction::LoadReserved) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            // `store_conditional(addr, bytes, value, ordering)` -> bits<1>: the
            // success flag. Arguments carry the address/value/ordering.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::StoreConditional) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Bits(1)
            }
            // `atomic_rmw(op, addr, bytes, value, ordering)` -> bits<_>: the old
            // memory word. `op` is a bare op-selector identifier, not a value, so
            // it is validated but never run through inference.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::AtomicRmw) => {
                match call.arguments.first() {
                    Some(ast::Expr::Ident(id)) if ast::atomic_rmw_op_code(&id.name).is_some() => {}
                    _ => {
                        diags.push((
                            file_name.to_string(),
                            Rich::custom(
                                call.span,
                                format!(
                                    "atomic_rmw op must be one of: {}",
                                    ast::ATOMIC_RMW_OPS.join(", ")
                                ),
                            ),
                        ));
                    }
                }
                for arg in call.arguments.iter().skip(1) {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            // `fence(pred, succ)` / `fence_i()` are effect-only statements.
            ast::Expr::BuiltinFunction(
                ast::BuiltinFunction::Fence | ast::BuiltinFunction::FenceI,
            ) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Integer
            }
            // Float arithmetic: both operands and the result share one type
            // (the register bits reinterpreted as the width's binary format).
            ast::Expr::BuiltinFunction(
                ast::BuiltinFunction::FAdd
                | ast::BuiltinFunction::FSub
                | ast::BuiltinFunction::FMul
                | ast::BuiltinFunction::FDiv
                | ast::BuiltinFunction::FMin
                | ast::BuiltinFunction::FMax,
            ) => {
                let lhs_ty = infer(&call.arguments[0], env, tvg, subst, cache, diags, file_name);
                for arg in &call.arguments[1..] {
                    let arg_ty = infer(arg, env, tvg, subst, cache, diags, file_name);
                    constrain(&arg_ty, &lhs_ty, subst, call.span, diags, file_name);
                }
                lhs_ty.apply(subst)
            }
            ast::Expr::BuiltinFunction(
                ast::BuiltinFunction::SIToFP
                | ast::BuiltinFunction::UIToFP
                | ast::BuiltinFunction::AsFloat
                | ast::BuiltinFunction::FCvt
                | ast::BuiltinFunction::Fma
                | ast::BuiltinFunction::Sqrt,
            ) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            ast::Expr::BuiltinFunction(
                ast::BuiltinFunction::FPToSI | ast::BuiltinFunction::FPToUI,
            ) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store)
            | ast::Expr::BuiltinFunction(ast::BuiltinFunction::Trap) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Integer
            }
            // `split(bits, n)` -> vec<bits<_>>: the input is some bitvector; each
            // lane is a bitvector whose width (input / n) is not tracked here.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Split) => {
                let bits_ty = infer(&call.arguments[0], env, tvg, subst, cache, diags, file_name);
                for arg in &call.arguments[1..] {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                constrain(
                    &bits_ty,
                    &Type::Con("bits".into(), vec![Type::Var(tvg.fresh())]),
                    subst,
                    call.span,
                    diags,
                    file_name,
                );
                vec_ty(Type::Con("bits".into(), vec![Type::Var(tvg.fresh())]))
            }
            // `concat(iter)` -> bits<_>: joins an iterator's lanes into a bitvector
            // whose width is the sum of the lane widths, not tracked here.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Concat) => {
                let iter_ty = infer(&call.arguments[0], env, tvg, subst, cache, diags, file_name);
                constrain(
                    &iter_ty,
                    &vec_ty(Type::Var(tvg.fresh())),
                    subst,
                    call.span,
                    diags,
                    file_name,
                );
                Type::Con("bits".into(), vec![Type::Var(tvg.fresh())])
            }
            // `iota(n, w)` -> vec<bits<w>>: lane indices 0..n-1. The lane width
            // comes from the second argument, not tracked here.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Iota) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                vec_ty(Type::Con("bits".into(), vec![Type::Var(tvg.fresh())]))
            }
            // `zip(a, b, ...)` -> vec<pair<A, B, ...>>: combines iterators
            // lane-wise; a `map` lambda with one parameter per zipped iterator
            // destructures each lane.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Zip) => {
                let mut components = Vec::with_capacity(call.arguments.len());
                for arg in &call.arguments {
                    let arg_ty = infer(arg, env, tvg, subst, cache, diags, file_name);
                    let component = Type::Var(tvg.fresh());
                    constrain(
                        &arg_ty,
                        &vec_ty(component.clone()),
                        subst,
                        call.span,
                        diags,
                        file_name,
                    );
                    components.push(component);
                }
                vec_ty(Type::Con("pair".into(), components))
            }
            // `map(iter, |x| ...)` -> vec<R>: applies the lambda to each lane. A
            // two-parameter lambda destructures a zipped pair element.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Map) => {
                let iter_ty = infer(&call.arguments[0], env, tvg, subst, cache, diags, file_name);
                let elem = Type::Var(tvg.fresh());
                constrain(
                    &iter_ty,
                    &vec_ty(elem.clone()),
                    subst,
                    call.span,
                    diags,
                    file_name,
                );
                let param_tys = map_param_tys(&elem.apply(subst), &call.arguments[1], tvg, subst);
                let ret = infer_lambda(
                    &call.arguments[1],
                    &param_tys,
                    env,
                    tvg,
                    subst,
                    cache,
                    diags,
                    file_name,
                );
                vec_ty(ret)
            }
            // `reduce(iter, |acc, x| ...)` -> R: left-folds the lambda over the
            // lanes; the accumulator, each lane and the result share one type.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Reduce) => {
                let iter_ty = infer(&call.arguments[0], env, tvg, subst, cache, diags, file_name);
                let elem = Type::Var(tvg.fresh());
                constrain(
                    &iter_ty,
                    &vec_ty(elem.clone()),
                    subst,
                    call.span,
                    diags,
                    file_name,
                );
                let elem = elem.apply(subst);
                let ret = infer_lambda(
                    &call.arguments[1],
                    &[elem.clone(), elem.clone()],
                    env,
                    tvg,
                    subst,
                    cache,
                    diags,
                    file_name,
                );
                constrain(&ret, &elem, subst, call.span, diags, file_name);
                elem.apply(subst)
            }
            callee => {
                let callee_ty = infer(callee, env, tvg, subst, cache, diags, file_name);
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                match callee_ty.apply(subst) {
                    Type::Fn(_, ret) => *ret,
                    _ => Type::Var(tvg.fresh()),
                }
            }
        },

        // base[hi..lo] → bits<hi - lo + 1>  (inclusive on both ends)
        ast::Expr::Slice(slc) => {
            let base_ty = infer(&slc.base, env, tvg, subst, cache, diags, file_name);
            let width_var = tvg.fresh();
            constrain(
                &base_ty,
                &Type::Con("bits".into(), vec![Type::Var(width_var)]),
                subst,
                slc.span,
                diags,
                file_name,
            );
            Type::Bits(slc.hi - slc.lo + 1)
        }

        // `x as bits<W>` → bits<W>: the low W bits of x. Widening is implicit,
        // so a cast only ever narrows.
        ast::Expr::Cast(cast) => {
            let from = infer(&cast.x, env, tvg, subst, cache, diags, file_name).apply(subst);
            let to = match declared_width(&cast.width) {
                Ok(to) => to,
                Err(message) => {
                    diags.push((file_name.to_string(), Rich::custom(cast.span, message)));
                    return Type::Var(tvg.fresh());
                }
            };
            if let Some(from) = bit_width(&from)
                && from < to
            {
                diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        cast.span,
                        format!(
                            "cast of bits<{from}> to the wider bits<{to}>; \
                             widening is implicit"
                        ),
                    ),
                ));
            }
            Type::Bits(to)
        }

        // base[i] → bits<1>
        ast::Expr::IndexAccess(idx) => {
            let base_ty = infer(&idx.base, env, tvg, subst, cache, diags, file_name);
            let width_var = tvg.fresh();
            constrain(
                &base_ty,
                &Type::Con("bits".into(), vec![Type::Var(width_var)]),
                subst,
                idx.span,
                diags,
                file_name,
            );
            Type::Bits(1)
        }

        ast::Expr::If(if_) => {
            // `bits<1>` is the condition type: there is no bool, so anything
            // wider has to say what it is being tested against.
            let cond_ty = infer(&if_.cond, env, tvg, subst, cache, diags, file_name).apply(subst);
            match bit_width(&cond_ty) {
                Some(1) => {}
                Some(width) => diags.push((
                    file_name.to_string(),
                    Rich::custom(
                        if_.span,
                        format!("condition must be bits<1>, found bits<{width}>"),
                    ),
                )),
                // A lane of a split vector is one bit wide without the checker
                // knowing it, so an unresolved width takes bits<1> here.
                None => constrain(&cond_ty, &Type::Bits(1), subst, if_.span, diags, file_name),
            }
            let then_ty = infer(&if_.then, env, tvg, subst, cache, diags, file_name);
            if let Some(else_) = &if_.else_ {
                let else_ty = infer(else_, env, tvg, subst, cache, diags, file_name);
                constrain(&then_ty, &else_ty, subst, if_.span, diags, file_name);
            }
            then_ty.apply(subst)
        }

        ast::Expr::Unary(u) => {
            let ty = infer(&u.x, env, tvg, subst, cache, diags, file_name);
            // A literal operand like `~1` takes its width from context, so it
            // must not pin the result to Integer.
            if ty == Type::Integer {
                Type::Var(tvg.fresh())
            } else {
                ty
            }
        }

        ast::Expr::Try(t) => {
            infer(&t.body, env, tvg, subst, cache, diags, file_name);
            for handler in &t.handlers {
                // The binding carries the faulting address: XLEN-wide bits,
                // like a register value.
                let mut handler_env = env.clone();
                if let Some(binding) = &handler.binding {
                    handler_env.bind(
                        binding.clone(),
                        TypeScheme::mono(Type::Con("bits".into(), vec![Type::Var(tvg.fresh())])),
                    );
                }
                infer(
                    &handler.body,
                    &handler_env,
                    tvg,
                    subst,
                    cache,
                    diags,
                    file_name,
                );
            }
            Type::Integer
        }

        // A bare lambda outside `map`/`reduce` is invalid, but inferring it (with
        // fresh parameter types) keeps the checker total and reports no spurious
        // error; the lowering rejects it.
        ast::Expr::Lambda(lambda) => {
            let param_tys: Vec<Type> = lambda
                .params
                .iter()
                .map(|_| Type::Var(tvg.fresh()))
                .collect();
            let ret = infer_lambda(expr, &param_tys, env, tvg, subst, cache, diags, file_name);
            let mut fields = param_tys;
            fields.push(ret);
            Type::Con("fn".into(), fields)
        }

        // `(a, b, c)` concatenates: bits<w(a) + w(b) + w(c)>. An element whose
        // width is not yet known leaves the whole width open, which is what a
        // per-shape `if` does until the encoding is expanded.
        ast::Expr::Tuple(tuple) => {
            let widths: Option<u16> = tuple
                .elements
                .iter()
                .map(|element| {
                    let ty = infer(element, env, tvg, subst, cache, diags, file_name);
                    bit_width(&ty.apply(subst))
                })
                .sum();
            match widths {
                Some(width) => Type::Bits(width),
                None => Type::Con("bits".into(), vec![Type::Var(tvg.fresh())]),
            }
        }

        ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => Type::Var(tvg.fresh()),
    };

    cache.insert(expr, ty.clone());
    ty
}

/// Infer a `let` binding's type: its annotation when it carries one, and then
/// the bound value must fit in it.
#[allow(clippy::too_many_arguments)]
fn infer_let<'a>(
    binding: &'a ast::Let,
    env: &TypeEnv,
    tvg: &mut TypeVarGen,
    subst: &mut Substitution,
    cache: &mut TypeCache<'a>,
    diags: &mut Vec<(String, Diag)>,
    file_name: &str,
) -> Type {
    let val_ty = infer(&binding.value, env, tvg, subst, cache, diags, file_name);
    let Some(width) = &binding.width else {
        return val_ty;
    };
    let width = match declared_width(width) {
        Ok(width) => width,
        Err(message) => {
            diags.push((file_name.to_string(), Rich::custom(binding.span, message)));
            return val_ty;
        }
    };
    if let ast::Expr::Lit(ast::Lit::Int(lit)) = &*binding.value {
        let value = utils::parse_literal_value(lit);
        if width < 64 && value >= 1 << width {
            diags.push((
                file_name.to_string(),
                Rich::custom(
                    binding.span,
                    format!("{value} does not fit in bits<{width}>"),
                ),
            ));
        }
    }
    let ty = Type::Bits(width);
    coerce(&val_ty, &ty, subst, binding.span, diags, file_name);
    ty
}

/// The number of bits a `bits<W>` annotation names. `W` has to be a literal:
/// a cast keeps the low bits and an annotation is checked against them, so a
/// width the spec never pins down would say nothing. `fn` parameters are
/// substituted before type checking, so `bits<n>` in a helper is a literal here.
fn declared_width(width: &ast::Expr) -> Result<u16, String> {
    let ast::Expr::Lit(ast::Lit::Int(lit)) = width else {
        return Err("width must be a literal number of bits".to_string());
    };
    let value = utils::parse_literal_value(lit);
    u16::try_from(value).map_err(|_| format!("width bits<{value}> is out of range"))
}

/// An iterator (vector) type carrying elements of `elem`.
fn vec_ty(elem: Type) -> Type {
    Type::Con("vec".into(), vec![elem])
}

/// Parameter types for a `map` lambda over elements of type `elem`. A unary
/// lambda takes the element; a lambda with two or more parameters destructures
/// a zipped `pair` element into its components (best-effort, leaving them free
/// if `elem` is not yet known to be a pair).
fn map_param_tys(
    elem: &Type,
    lambda_arg: &ast::Expr,
    tvg: &mut TypeVarGen,
    subst: &mut Substitution,
) -> Vec<Type> {
    let arity = match lambda_arg {
        ast::Expr::Lambda(l) => l.params.len(),
        _ => 1,
    };
    if arity < 2 {
        return vec![elem.clone()];
    }
    let tys: Vec<Type> = (0..arity).map(|_| Type::Var(tvg.fresh())).collect();
    let pair = Type::Con("pair".into(), tys.clone());
    if let Ok(s) = unify(&elem.apply(subst), &pair) {
        let old = mem::take(subst);
        *subst = old.compose(&s);
        tys.iter().map(|ty| ty.apply(subst)).collect()
    } else {
        tys
    }
}

/// Infer a `map`/`reduce` lambda's body with its parameters bound to `param_tys`,
/// returning the body's (result) type and recording the lambda's `fn` type.
#[allow(clippy::too_many_arguments)]
fn infer_lambda<'a>(
    lambda_arg: &'a ast::Expr,
    param_tys: &[Type],
    env: &TypeEnv,
    tvg: &mut TypeVarGen,
    subst: &mut Substitution,
    cache: &mut TypeCache<'a>,
    diags: &mut Vec<(String, Diag)>,
    file_name: &str,
) -> Type {
    let ast::Expr::Lambda(lambda) = lambda_arg else {
        // Not a lambda where one is required; infer generically so the body is
        // still checked. The lowering reports the misuse.
        return infer(lambda_arg, env, tvg, subst, cache, diags, file_name);
    };
    let mut body_env = env.clone();
    for (name, ty) in lambda.params.iter().zip(param_tys) {
        body_env.bind(name.clone(), TypeScheme::mono(ty.clone()));
    }
    let ret = infer(&lambda.body, &body_env, tvg, subst, cache, diags, file_name);
    let mut fields: Vec<Type> = param_tys.to_vec();
    fields.push(ret.clone());
    cache.insert(lambda_arg, Type::Con("fn".into(), fields));
    ret
}
