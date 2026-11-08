use quote::format_ident;
use syn::Ident;

pub fn op_fn_ident(name: &str) -> Ident {
    let mut sanitized = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        sanitized.push_str("op");
    }
    let first = sanitized.chars().next().unwrap();
    if !(first.is_ascii_alphabetic() || first == '_') {
        sanitized.insert_str(0, "op_");
    }
    format_ident!("r#{}", sanitized)
}
