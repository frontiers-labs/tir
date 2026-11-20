//! Syntactic well-foundedness checks run before HM elaboration.
use crate::{Error, ast::*, compiler::Program};
use std::collections::{BTreeMap, BTreeSet};

pub fn check(program: &Program) -> Result<(), Error> {
    let mut definitions = BTreeMap::new();
    for module in program.all_modules() {
        for declaration in &module.ast.declarations {
            if let Declaration::Let { binding, .. } | Declaration::Interface { binding, .. } =
                &declaration.value
            {
                let mut path = module.path.clone();
                path.extend(binding.path.clone());
                definitions.insert(path, binding);
            }
        }
    }
    let mut dependencies = BTreeMap::new();
    for (path, binding) in &definitions {
        let mut reads = free_paths(&binding.value)?;
        if let Some(ty) = &binding.ty {
            reads.extend(free_paths(ty)?);
        }
        reads.retain(|name| {
            definitions.contains_key(name)
                && !(name.len() == 1
                    && binding.generics.iter().any(|binder| binder.name == name[0]))
        });
        if reads.remove(path) {
            let Expression::Function(function) = &binding.value.value else {
                return Err(Error("recursive values are not total".into()));
            };
            let Some(measure) = &function.decreases else {
                return Err(Error(
                    "recursive functions require a structural decreases clause".into(),
                ));
            };
            let Expression::Path(measure) = &measure.value else {
                return Err(Error("decreases must name an inductive parameter".into()));
            };
            if measure.len() != 1 {
                return Err(Error("decreases must name a parameter".into()));
            }
            let index = function
                .parameters
                .iter()
                .position(|p| p.name == measure[0])
                .ok_or_else(|| Error("decreases must name a parameter".into()))?;
            if function.result.is_none() {
                return Err(Error("recursive functions require a result type".into()));
            }
            recursive_block(&function.body, path, &measure[0], index, &BTreeSet::new())?;
        } else if let Expression::Function(function) = &binding.value.value
            && function.decreases.is_some()
        {
            return Err(Error("decreases on a nonrecursive function".into()));
        }
        dependencies.insert(path.clone(), reads);
    }
    let mut done = BTreeSet::new();
    let mut pending = BTreeSet::new();
    for path in definitions.keys() {
        dependency(path, &dependencies, &mut pending, &mut done)?;
    }
    Ok(())
}
fn dependency(
    path: &Path,
    edges: &BTreeMap<Path, BTreeSet<Path>>,
    pending: &mut BTreeSet<Path>,
    done: &mut BTreeSet<Path>,
) -> Result<(), Error> {
    if done.contains(path) {
        return Ok(());
    }
    if !pending.insert(path.clone()) {
        return Err(Error(
            "mutual recursion needs a joint well-founded measure; none was supplied".into(),
        ));
    }
    if let Some(reads) = edges.get(path) {
        for read in reads {
            dependency(read, edges, pending, done)?;
        }
    }
    pending.remove(path);
    done.insert(path.clone());
    Ok(())
}
fn recursive_body(
    expr: &Expr,
    owner: &Path,
    measure: &str,
    index: usize,
    smaller: &BTreeSet<String>,
) -> Result<(), Error> {
    match &expr.value {
        Expression::Function(function) => {
            let mut nested = smaller.clone();
            let mut root = measure;
            for binder in function.parameters.iter().chain(&function.generics) {
                nested.remove(&binder.name);
                if binder.name == measure {
                    root = "";
                }
            }
            return recursive_block(&function.body, owner, root, index, &nested);
        }
        Expression::Block(block) => return recursive_block(block, owner, measure, index, smaller),
        Expression::Ir { bindings, value } => {
            return recursive_block(
                &Block {
                    bindings: bindings.clone(),
                    value: value.clone(),
                },
                owner,
                measure,
                index,
                smaller,
            );
        }
        Expression::If {
            condition,
            then,
            otherwise,
        } => {
            recursive_body(condition, owner, measure, index, smaller)?;
            recursive_block(then, owner, measure, index, smaller)?;
            return recursive_body(otherwise, owner, measure, index, smaller);
        }
        Expression::Path(p) if p == owner => {
            return Err(Error("recursive function cannot escape as a value".into()));
        }
        Expression::Call { value, arguments } if matches!(&value.value,Expression::Path(p) if p==owner) =>
        {
            if !arguments.get(index).is_some_and(
                |a| matches!(&a.value,Expression::Path(p) if p.len()==1 && smaller.contains(&p[0])),
            ) {
                return Err(Error(
                    "recursive call does not structurally decrease".into(),
                ));
            }
            for argument in arguments {
                recursive_body(argument, owner, measure, index, smaller)?;
            }
            return Ok(());
        }
        Expression::Match { value, arms } => {
            recursive_body(value, owner, measure, index, smaller)?;
            let decreases = matches!(&value.value,Expression::Path(p) if p.len()==1 && (p[0]==measure || smaller.contains(&p[0])));
            for (pattern, body) in arms {
                let mut nested = smaller.clone();
                remove_pattern_binders(pattern, &mut nested);
                let mut bound = BTreeSet::from([measure.to_owned()]);
                remove_pattern_binders(pattern, &mut bound);
                let root = if bound.contains(measure) { measure } else { "" };
                if decreases {
                    subpatterns(pattern, false, &mut nested);
                }
                recursive_body(body, owner, root, index, &nested)?;
            }
            return Ok(());
        }
        _ => {}
    }
    children(expr, &mut |child| {
        recursive_body(child, owner, measure, index, smaller)
    })
}
fn recursive_block(
    block: &Block,
    owner: &Path,
    measure: &str,
    index: usize,
    smaller: &BTreeSet<String>,
) -> Result<(), Error> {
    let mut nested = smaller.clone();
    let mut root = measure;
    for binding in &block.bindings {
        recursive_body(&binding.value, owner, root, index, &nested)?;
        if binding.path.len() == 1 {
            nested.remove(&binding.path[0]);
            if binding.path[0] == root {
                root = "";
            }
        }
    }
    recursive_body(&block.value, owner, root, index, &nested)
}
fn remove_pattern_binders(pattern: &Pattern, names: &mut BTreeSet<String>) {
    match &pattern.value {
        PatternKind::Path(p) if p.len() == 1 => {
            names.remove(&p[0]);
        }
        PatternKind::Constructor { arguments, .. } | PatternKind::Tuple(arguments) => {
            for p in arguments {
                remove_pattern_binders(p, names);
            }
        }
        _ => {}
    }
}
fn subpatterns(pattern: &Pattern, strict: bool, smaller: &mut BTreeSet<String>) {
    match &pattern.value {
        PatternKind::Path(p) if strict && p.len() == 1 => {
            smaller.insert(p[0].clone());
        }
        PatternKind::Constructor { arguments, .. } => {
            for p in arguments {
                subpatterns(p, true, smaller);
            }
        }
        PatternKind::Tuple(items) => {
            for p in items {
                subpatterns(p, strict, smaller);
            }
        }
        _ => {}
    }
}
pub(crate) fn visit(
    expr: &Expr,
    f: &mut impl FnMut(&Expr) -> Result<(), Error>,
) -> Result<(), Error> {
    f(expr)?;
    children(expr, &mut |e| visit(e, f))
}
fn block(block: &Block, f: &mut impl FnMut(&Expr) -> Result<(), Error>) -> Result<(), Error> {
    for b in &block.bindings {
        for generic in &b.generics {
            f(&generic.ty)?;
        }
        if let Some(ty) = &b.ty {
            f(ty)?;
        }
        f(&b.value)?;
    }
    f(&block.value)
}
fn children(expr: &Expr, f: &mut impl FnMut(&Expr) -> Result<(), Error>) -> Result<(), Error> {
    match &expr.value {
        Expression::Literal(_) | Expression::Path(_) | Expression::Unit => {}
        Expression::Tuple(es) | Expression::Array(es) => {
            for e in es {
                f(e)?;
            }
        }
        Expression::Record(fs) | Expression::Limit(fs) => {
            for (_, e) in fs {
                f(e)?;
            }
        }
        Expression::Block(b) => block(b, f)?,
        Expression::Function(fun) => {
            for binder in fun.generics.iter().chain(&fun.parameters) {
                f(&binder.ty)?;
            }
            if let Some(result) = &fun.result {
                f(result)?;
            }
            block(&fun.body, f)?;
        }
        Expression::If {
            condition,
            then,
            otherwise,
        } => {
            f(condition)?;
            block(then, f)?;
            f(otherwise)?;
        }
        Expression::Match { value, arms } => {
            f(value)?;
            for (_, e) in arms {
                f(e)?;
            }
        }
        Expression::Op(fields) => {
            for field in fields {
                match &field.value {
                    OpField::Field { value, .. } => f(value)?,
                    OpField::Interfaces { entries, .. } => {
                        for (_, _, methods) in entries {
                            f(methods)?;
                        }
                    }
                    OpField::Signature { .. } => {}
                }
            }
        }
        Expression::Ir { bindings, value } => {
            for b in bindings {
                f(&b.value)?;
            }
            f(value)?;
        }
        Expression::Unary { value, .. } | Expression::Member { value, .. } => f(value)?,
        Expression::Binary { left, right, .. } => {
            f(left)?;
            f(right)?;
        }
        Expression::Call { value, arguments } | Expression::Generic { value, arguments } => {
            f(value)?;
            for e in arguments {
                f(e)?;
            }
        }
        Expression::RecordCall {
            value,
            fields,
            arguments,
        } => {
            f(value)?;
            for (_, e) in fields {
                f(e)?;
            }
            for e in arguments {
                f(e)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn free_paths(expr: &Expr) -> Result<BTreeSet<Path>, Error> {
    let mut paths = BTreeSet::new();
    free(expr, &BTreeSet::new(), &mut paths)?;
    Ok(paths)
}
fn free_block(
    body: &Block,
    shadowed: &BTreeSet<String>,
    paths: &mut BTreeSet<Path>,
) -> Result<(), Error> {
    let mut scope = shadowed.clone();
    for binding in &body.bindings {
        free(&binding.value, &scope, paths)?;
        if binding.path.len() == 1 {
            scope.insert(binding.path[0].clone());
        }
    }
    free(&body.value, &scope, paths)
}
fn pattern_names(pattern: &Pattern, names: &mut BTreeSet<String>) {
    match &pattern.value {
        PatternKind::Path(p) if p.len() == 1 => {
            names.insert(p[0].clone());
        }
        PatternKind::Constructor { arguments, .. } | PatternKind::Tuple(arguments) => {
            for p in arguments {
                pattern_names(p, names);
            }
        }
        _ => {}
    }
}
fn free(expr: &Expr, shadowed: &BTreeSet<String>, paths: &mut BTreeSet<Path>) -> Result<(), Error> {
    match &expr.value {
        Expression::Path(path) => {
            if path.len() != 1 || !shadowed.contains(&path[0]) {
                paths.insert(path.clone());
            }
            Ok(())
        }
        Expression::Function(function) => {
            let mut scope = shadowed.clone();
            for binder in function.generics.iter().chain(&function.parameters) {
                free(&binder.ty, &scope, paths)?;
                scope.insert(binder.name.clone());
            }
            if let Some(result) = &function.result {
                free(result, &scope, paths)?;
            }
            free_block(&function.body, &scope, paths)
        }
        Expression::Block(body) => free_block(body, shadowed, paths),
        Expression::Op(fields) => {
            for field in fields {
                match &field.value {
                    OpField::Interfaces { receiver, entries } => {
                        let mut scope = shadowed.clone();
                        scope.insert(receiver.clone());
                        for (_, _, methods) in entries {
                            free(methods, &scope, paths)?;
                        }
                    }
                    OpField::Field { value, .. } => free(value, shadowed, paths)?,
                    OpField::Signature { .. } => {}
                }
            }
            Ok(())
        }
        Expression::Ir { bindings, value } => free_block(
            &Block {
                bindings: bindings.clone(),
                value: value.clone(),
            },
            shadowed,
            paths,
        ),
        Expression::If {
            condition,
            then,
            otherwise,
        } => {
            free(condition, shadowed, paths)?;
            free_block(then, shadowed, paths)?;
            free(otherwise, shadowed, paths)
        }
        Expression::Match { value, arms } => {
            free(value, shadowed, paths)?;
            for (pattern, body) in arms {
                let mut scope = shadowed.clone();
                pattern_names(pattern, &mut scope);
                free(body, &scope, paths)?;
            }
            Ok(())
        }
        _ => children(expr, &mut |child| free(child, shadowed, paths)),
    }
}
