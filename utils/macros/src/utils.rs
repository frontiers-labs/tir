use syn::{Expr, Ident, Lit};

pub fn expr_as_string(expr: &Expr) -> String {
    match &expr {
        Expr::Lit(lit) => match &lit.lit {
            Lit::Str(str) => str.value(),
            _ => unreachable!(),
        },
        Expr::Path(p) => p.path.get_ident().unwrap().to_string(),
        _ => unreachable!(),
    }
}

pub fn expr_as_ident_vec(expr: &Expr) -> Vec<Ident> {
    if let Expr::Array(arr) = expr {
        arr.elems
            .iter()
            .map(|e| {
                if let Expr::Path(p) = e {
                    p.path.get_ident().unwrap().clone()
                } else {
                    unreachable!()
                }
            })
            .collect()
    } else {
        unreachable!()
    }
}
