//! Project checked dialect methods into the CDSL instruction selector.
use std::fmt::Write;

use std::path::{Path, PathBuf};
use tir_cdsl::ast::{BinaryOp, Declaration, Expr, Expression as E, Literal, OpField, PatternKind};
use tir_cdsl::{CompiledModule, Error, lexer::TokenStream, parser};

fn path(expr: &Expr) -> Option<String> {
    match &expr.value {
        E::Path(parts) => Some(parts.join("::")),
        _ => None,
    }
}

fn term(expr: &Expr, parameters: &[&str]) -> Option<String> {
    let binary = |name: &str, left, right| {
        Some(format!(
            "Expr::Binary({name:?},{},{})",
            term(left, parameters)?,
            term(right, parameters)?
        ))
    };
    match &expr.value {
        E::Path(parts) if parts.len() == 1 => parameters
            .iter()
            .position(|&s| s == parts[0])
            .map(|i| format!("Expr::Arg({i})")),
        E::Literal(Literal::Bool(false)) => Some("Expr::False".into()),
        E::Unit => Some("Expr::UnitValue".into()),
        E::Member { value, name } if path(value).as_deref() == Some("self") => {
            Some(format!("Expr::Field({name:?})"))
        }
        E::Binary {
            op: BinaryOp::And,
            left,
            right,
        } => binary("and", left, right),
        E::Binary {
            op: BinaryOp::Or,
            left,
            right,
        } => binary("or", left, right),
        E::Call { value, arguments } => match (&value.value, arguments.as_slice()) {
            (E::Generic { value, arguments }, [])
                if path(value).as_deref() == Some("std::bits::zero")
                    && matches!(arguments.as_slice(),[n] if matches!(&n.value,E::Literal(Literal::Natural(s)) if s=="1")) =>
            {
                Some("Expr::ZeroBit".into())
            }
            (_, [left, right]) => binary(&path(value)?, left, right),
            _ => None,
        },
        _ => None,
    }
}

fn variants(expr: &Expr, parameters: &[&str]) -> Option<Vec<(String, String, String)>> {
    if let E::Call { value, arguments } = &expr.value
        && path(value).as_deref() == Some("std::bits::from_bool")
        && let [arg] = arguments.as_slice()
        && let E::Match { value, arms } = &arg.value
        && let E::Member { value, name } = &value.value
        && path(value).as_deref() == Some("self")
    {
        return arms
            .iter()
            .map(|(pattern, body)| {
                let PatternKind::Path(case) = &pattern.value else {
                    return None;
                };
                Some((
                    name.clone(),
                    case.last()?.to_ascii_lowercase(),
                    term(body, parameters)?,
                ))
            })
            .collect();
    }
    Some(vec![(
        String::new(),
        String::new(),
        term(expr, parameters)?,
    )])
}

fn contracts(module: &CompiledModule) -> String {
    let mut out = String::new();
    for decl in &module.ast.declarations {
        let Declaration::Let { binding, .. } = &decl.value else {
            continue;
        };
        let E::Op(fields) = &binding.value.value else {
            continue;
        };
        let [name] = binding.path.as_slice() else {
            continue;
        };
        let mut facts = Vec::new();
        let mut complete = true;
        for role in ["exec", "poison", "requires"] {
            let method = fields.iter().find_map(|field| match &field.value {
                OpField::Field { name, value } if name == role => Some(value),
                _ => None,
            });
            let Some(method) = method else {
                facts.push((
                    role,
                    String::new(),
                    String::new(),
                    String::from("Expr::Absent"),
                ));
                continue;
            };
            let E::Function(f) = &method.value else {
                complete = false;
                break;
            };
            let parameters = f
                .parameters
                .iter()
                .map(|p| p.name.as_str())
                .filter(|&p| p != "self")
                .collect::<Vec<_>>();
            let Some(forms) =
                variants(&f.body.value, &parameters).filter(|_| f.body.bindings.is_empty())
            else {
                complete = false;
                break;
            };
            facts.extend(
                forms
                    .into_iter()
                    .map(|(field, case, body)| (role, field, case, body)),
            );
        }
        if complete {
            for (i, (role, field, case, body)) in facts.iter().enumerate() {
                writeln!(out,"pub let method_{name}_{i}: Method(dialects::builtin::{name}.kind,{role:?},{field:?},{case:?},{body});").unwrap();
            }
        }
    }
    out
}

pub fn compile() -> Result<(), Box<dyn std::error::Error>> {
    let mut package = tir_cdsl::compile_file(Path::new("defs/mod.cdsl"))?;
    let builtin = package
        .modules
        .iter()
        .find(|m| m.path == ["dialects", "builtin"])
        .ok_or_else(|| Error("missing builtin dialect".into()))?;
    let tokens = TokenStream::from_source(&contracts(builtin))
        .map_err(|errors| Error(format!("{errors:?}")))?;
    let facts = parser::parse(tokens).map_err(|errors| Error(format!("{errors:?}")))?;
    let contracts = package
        .modules
        .iter_mut()
        .find(|m| m.path == ["interp", "contracts"])
        .ok_or_else(|| Error("missing interpreter contracts module".into()))?;
    contracts.ast.declarations.extend(facts.declarations);
    let rust = package.lower()?.render_program(&["interp".into()])?;
    std::fs::write(
        PathBuf::from(std::env::var("OUT_DIR")?).join("interp.rs"),
        rust,
    )?;
    Ok(())
}
