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
    let synonyms = build_synonym_table(files, &isa_param_vars);

    let item_cache: HashMap<&str, &ast::Item> = files
        .iter()
        .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
        .collect();

    for file in files {
        for instr in file.instructions() {
            let env = build_instr_env(instr, &item_cache, &synonyms, &isa_param_vars);
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

fn reg_class_type(rc: &ast::RegisterClass, isa_param_vars: &HashMap<String, TypeVar>) -> Type {
    if let Some((_ty, Some(default))) = rc.parameters.get("WIDTH")
        && let ast::Expr::Field(field) = default
        && let Some(&tv) = isa_param_vars.get(&field.member)
    {
        return Type::Con("bits".into(), vec![Type::Var(tv)]);
    }
    unreachable!("All register classes must have WIDTH parameter")
}

fn build_synonym_table(
    files: &[ast::File],
    isa_param_vars: &HashMap<String, TypeVar>,
) -> SynonymTable {
    let mut synonyms = SynonymTable::new();
    for file in files {
        for rc in file.register_classes() {
            synonyms.insert(rc.name.clone(), reg_class_type(rc, isa_param_vars));
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
) -> TypeEnv {
    let mut env = TypeEnv::new();
    for (name, ty) in utils::resolve_operands_for_instruction(instr, item_cache) {
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

        ast::Expr::BuiltinFunction(_) | ast::Expr::Invalid => Type::Var(tvg.fresh()),
    };

    cache.insert(expr, ty.clone());
    ty
}
