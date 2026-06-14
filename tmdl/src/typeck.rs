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

    let item_cache: HashMap<&str, &ast::Item> = files
        .iter()
        .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
        .collect();

    for file in files {
        for instr in file.instructions() {
            let env = build_instr_env(instr, &item_cache, &synonyms, &isa_param_vars, &mut tvg);
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
    tvg: &mut TypeVarGen,
) -> TypeEnv {
    let mut env = TypeEnv::new();
    for (name, ty) in utils::resolve_operands_for_instruction(instr, item_cache) {
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

        ast::Expr::Lit(ast::Lit::Int(_)) => Type::Integer,
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
                    constrain(&lhs_ty, &rhs_ty, subst, bin.span, diags, file_name);
                    lhs_ty.apply(subst)
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
                    constrain(&lhs_ty, &rhs_ty, subst, bin.span, diags, file_name);
                    Type::Bits(1)
                }
            }
        }

        ast::Expr::Assign(asgn) => {
            let dest_ty = infer(&asgn.dest, env, tvg, subst, cache, diags, file_name);
            let val_ty = infer(&asgn.value, env, tvg, subst, cache, diags, file_name);
            constrain(&dest_ty, &val_ty, subst, asgn.span, diags, file_name);
            val_ty.apply(subst)
        }

        ast::Expr::Path(path) => {
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
            let mut ty = Type::Integer;
            for stmt in &block.stmts {
                ty = infer(stmt, env, tvg, subst, cache, diags, file_name);
            }
            if block.last_expr_return {
                ty
            } else {
                Type::Integer
            }
        }

        ast::Expr::Field(field) => {
            // Only `self.MEMBER` is supported; the member is resolved as an ISA parameter.
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
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Log2Ceil) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Integer
            }
            // `lane(vector, index)` reads one element; its width is the vector's
            // element width, which is not tracked, so it stays a free variable.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Lane) => {
                for arg in &call.arguments {
                    infer(arg, env, tvg, subst, cache, diags, file_name);
                }
                Type::Var(tvg.fresh())
            }
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::SExt)
            | ast::Expr::BuiltinFunction(ast::BuiltinFunction::ZExt)
            | ast::Expr::BuiltinFunction(ast::BuiltinFunction::Load) => {
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
            // `zip(a, b)` -> vec<pair<A, B>>: pairs two iterators lane-wise.
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Zip) => {
                let lhs_ty = infer(&call.arguments[0], env, tvg, subst, cache, diags, file_name);
                let rhs_ty = infer(&call.arguments[1], env, tvg, subst, cache, diags, file_name);
                let a = Type::Var(tvg.fresh());
                let b = Type::Var(tvg.fresh());
                constrain(
                    &lhs_ty,
                    &vec_ty(a.clone()),
                    subst,
                    call.span,
                    diags,
                    file_name,
                );
                constrain(
                    &rhs_ty,
                    &vec_ty(b.clone()),
                    subst,
                    call.span,
                    diags,
                    file_name,
                );
                vec_ty(Type::Con("pair".into(), vec![a, b]))
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

        // base[lo..hi] → bits<hi - lo + 1>  (inclusive on both ends)
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
            Type::Bits(slc.end - slc.start + 1)
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
            infer(&if_.cond, env, tvg, subst, cache, diags, file_name);
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

        ast::Expr::For(f) => {
            infer(&f.start, env, tvg, subst, cache, diags, file_name);
            infer(&f.end, env, tvg, subst, cache, diags, file_name);
            let mut body_env = env.clone();
            body_env.bind(f.var.clone(), TypeScheme::mono(Type::Integer));
            infer(&f.body, &body_env, tvg, subst, cache, diags, file_name);
            // A value-producing (map) loop yields a vector assigned to a register,
            // so its type must unify with `bits<N>` as well as the integer the
            // statement and accumulator forms produce; a free variable does both.
            Type::Var(tvg.fresh())
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

        ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => Type::Var(tvg.fresh()),
    };

    cache.insert(expr, ty.clone());
    ty
}

/// An iterator (vector) type carrying elements of `elem`.
fn vec_ty(elem: Type) -> Type {
    Type::Con("vec".into(), vec![elem])
}

/// Parameter types for a `map` lambda over elements of type `elem`. A unary
/// lambda takes the element; a binary lambda destructures a zipped `pair`
/// element into its two components (best-effort, leaving them free if `elem` is
/// not yet known to be a pair).
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
    match arity {
        2 => {
            let a = Type::Var(tvg.fresh());
            let b = Type::Var(tvg.fresh());
            let pair = Type::Con("pair".into(), vec![a.clone(), b.clone()]);
            if let Ok(s) = unify(&elem.apply(subst), &pair) {
                let old = mem::take(subst);
                *subst = old.compose(&s);
                vec![a.apply(subst), b.apply(subst)]
            } else {
                vec![a, b]
            }
        }
        n => {
            let mut tys = vec![elem.clone()];
            tys.extend((1..n).map(|_| Type::Var(tvg.fresh())));
            tys
        }
    }
}

#[cfg(test)]
mod tests {
    use super::check;

    fn type_check_source(src: &str) -> Vec<(String, String)> {
        let (tokens, _lex_errs) = crate::lexer::lex(src);
        let (file, parse_errs) = crate::parser::parse(src, &tokens, "test.tmdl");
        assert!(parse_errs.is_empty(), "parse errors: {parse_errs:?}");
        let files = vec![file.expect("file parses")];
        let (_cache, diags) = check(&files);
        diags.into_iter().map(|(f, d)| (f, d.to_string())).collect()
    }

    const VECTOR_PRELUDE: &str = "
        isa RV32I { param XLEN: Integer = 32; }
        isa RVV requires [RV32I] {
            param VLEN: Integer = 128;
            param SEW: Integer = 32;
        }
        register_class VR for [RVV] {
            param ENCODING_LEN: Integer = 5;
            param WIDTH: Integer = self.VLEN;
            registers { v0..v31 => { traits = [vector] }, }
        }
        template VArithVV for [RVV] {
            param MNEMONIC: String;
            operands { vd: VR, vs2: VR, vs1: VR, }
        }
    ";

    #[test]
    fn functional_vector_add_type_checks() {
        let src = format!(
            "{VECTOR_PRELUDE}
            instruction VAdd for [RVV] : VArithVV {{
                param MNEMONIC: String = \"vadd.vv\";
                behavior {{
                    vd = concat(map(zip(split(vs2, 4), split(vs1, 4)), |a, b| a + b));
                }}
            }}"
        );
        assert!(type_check_source(&src).is_empty());
    }

    #[test]
    fn functional_reduce_type_checks() {
        let src = format!(
            "{VECTOR_PRELUDE}
            instruction VRedSum for [RVV] : VArithVV {{
                param MNEMONIC: String = \"vredsum.vs\";
                behavior {{
                    vd = reduce(split(vs2, 4), |acc, x| acc + x);
                }}
            }}"
        );
        assert!(type_check_source(&src).is_empty());
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
