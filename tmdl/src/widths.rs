//! `width(x)`: the declared width of a value, substituted before lowering.
//!
//! A width is spec-time information: it comes from the type an operand,
//! parameter or cast declares, never from a runtime value. Resolving the calls
//! here — after `fn` inlining, before semantic lowering — leaves the rest of
//! the pipeline with ordinary width expressions over ISA parameters.

use std::collections::HashMap;

use chumsky::error::Rich;

use crate::Span;
use crate::ast;
use crate::expander::Diag;
use crate::types::Type;
use crate::utils;

/// Replace every `width(x)` in a behavior with the width `x` declares.
pub fn resolve_width_calls(files: &mut [ast::File]) -> Vec<Diag> {
    let item_cache: HashMap<&str, &ast::Item> = files
        .iter()
        .flat_map(|f| f.items.iter().map(|i| (i.name(), i)))
        .collect();
    let classes = class_widths(files);

    let mut contexts: Vec<(usize, usize, HashMap<String, Type>)> = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        for (item_index, item) in file.items.iter().enumerate() {
            let ast::Item::Instruction(instr) = item else {
                continue;
            };
            let mut types: HashMap<String, Type> =
                utils::resolve_operands_for_instruction(instr, &item_cache)
                    .into_iter()
                    .collect();
            for (name, (ty, _)) in utils::resolve_params_for_instruction(instr, &item_cache) {
                types.entry(name).or_insert(ty);
            }
            contexts.push((file_index, item_index, types));
        }
    }

    let mut diags = Vec::new();
    for (file_index, item_index, types) in contexts {
        let file_name = files[file_index].file_name.clone();
        let ast::Item::Instruction(instr) = &mut files[file_index].items[item_index] else {
            continue;
        };
        let ctx = WidthContext {
            types: &types,
            classes: &classes,
        };
        instr.behavior = ctx.resolve(&instr.behavior, &mut Vec::new(), &file_name, &mut diags);
    }

    // A trap handler names no operands, so only self-describing values (a
    // literal, a slice, a cast) have a width there.
    let empty = HashMap::new();
    for file in files.iter_mut() {
        let file_name = file.file_name.clone();
        for item in &mut file.items {
            let ast::Item::Isa(isa) = item else { continue };
            let Some(trap) = &mut isa.trap_handler else {
                continue;
            };
            let ctx = WidthContext {
                types: &empty,
                classes: &classes,
            };
            trap.body = ctx.resolve(&trap.body, &mut Vec::new(), &file_name, &mut diags);
        }
    }
    diags
}

/// The two widths a register class gives an operand: `WIDTH` for the value it
/// holds, `ENCODING_LEN` for the number an encoding spells.
struct ClassWidths {
    value: Option<ast::Expr>,
    index: Option<u16>,
}

fn class_widths(files: &[ast::File]) -> HashMap<String, ClassWidths> {
    let encoding_lens = crate::encoding::register_class_widths(files);
    files
        .iter()
        .flat_map(|f| &f.items)
        .filter_map(|item| match item {
            ast::Item::RegisterClass(class) => Some((
                class.name.clone(),
                ClassWidths {
                    value: class
                        .parameters
                        .get("WIDTH")
                        .and_then(|(_, value)| value.clone()),
                    index: encoding_lens.get(&class.name).copied(),
                },
            )),
            _ => None,
        })
        .collect()
}

struct WidthContext<'a> {
    types: &'a HashMap<String, Type>,
    classes: &'a HashMap<String, ClassWidths>,
}

impl WidthContext<'_> {
    /// `lets` carries the annotated bindings in scope, innermost last: a
    /// `let x: bits<8>` declares a width like an operand's type does.
    fn resolve(
        &self,
        expr: &ast::Expr,
        lets: &mut Vec<(String, ast::Expr)>,
        file_name: &str,
        diags: &mut Vec<Diag>,
    ) -> ast::Expr {
        if let ast::Expr::Call(call) = expr
            && matches!(
                &*call.callee,
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Width)
            )
        {
            let resolved = match call.arguments.first() {
                Some(argument) => self.width_of(argument, lets, call.span),
                None => Err("width requires 1 argument".to_string()),
            };
            return match resolved {
                Ok(width) => width,
                Err(message) => {
                    diags.push((file_name.to_string(), Rich::custom(call.span, message)));
                    ast::Expr::Invalid
                }
            };
        }
        if let ast::Expr::Block(block) = expr {
            let outer = lets.len();
            let stmts = block
                .stmts
                .iter()
                .map(|stmt| {
                    let resolved = self.resolve(stmt, lets, file_name, diags);
                    if let ast::Expr::Let(binding) = stmt
                        && let Some(width) = &binding.width
                    {
                        lets.push((binding.name.clone(), (**width).clone()));
                    }
                    resolved
                })
                .collect();
            lets.truncate(outer);
            return ast::Expr::Block(ast::Block {
                stmts,
                last_expr_return: block.last_expr_return,
                span: block.span,
            });
        }
        utils::map_child_exprs(expr, &mut |child| {
            self.resolve(child, lets, file_name, diags)
        })
    }

    /// The width `value` declares, as a spec-time expression.
    fn width_of(
        &self,
        value: &ast::Expr,
        lets: &[(String, ast::Expr)],
        span: Span,
    ) -> Result<ast::Expr, String> {
        match value {
            ast::Expr::Lit(ast::Lit::Int(lit)) => {
                crate::encoding::literal_width(lit.value()).map(|width| literal(width, span))
            }
            ast::Expr::Ident(id) => match lets.iter().rev().find(|(name, _)| *name == id.name) {
                Some((_, width)) => Ok(width.clone()),
                None => self.named_width(&id.name, span),
            },
            // A register operand's projections: the bits it holds, and the
            // number the encoding spells.
            ast::Expr::Field(field) => match (&*field.base, field.member.as_str()) {
                (ast::Expr::Ident(base), "value") => self.named_width(&base.name, span),
                (ast::Expr::Ident(base), "index") => self.index_width(&base.name, span),
                _ => Err(unresolved()),
            },
            ast::Expr::Cast(cast) => Ok((*cast.width).clone()),
            ast::Expr::Slice(slc) => Ok(literal(slc.hi - slc.lo + 1, span)),
            ast::Expr::IndexAccess(_) => Ok(literal(1, span)),
            _ => Err(unresolved()),
        }
    }

    fn named_width(&self, name: &str, span: Span) -> Result<ast::Expr, String> {
        match self.types.get(name) {
            Some(Type::Bits(width)) => Ok(literal(*width, span)),
            Some(Type::BitsExpr(width)) => Ok((**width).clone()),
            Some(Type::Struct(class)) => {
                match self.classes.get(class).and_then(|c| c.value.as_ref()) {
                    Some(width) => Ok(width.clone()),
                    None => Err(format!("register class '{class}' declares no WIDTH")),
                }
            }
            _ => Err(format!("'{name}' has no declared width")),
        }
    }

    fn index_width(&self, name: &str, span: Span) -> Result<ast::Expr, String> {
        match self.types.get(name) {
            Some(Type::Struct(class)) => match self.classes.get(class).and_then(|c| c.index) {
                Some(len) => Ok(literal(len, span)),
                None => Err(format!("register class '{class}' declares no ENCODING_LEN")),
            },
            _ => Err(format!("'{name}' is not a register operand")),
        }
    }
}

fn literal(width: u16, span: Span) -> ast::Expr {
    ast::Expr::Lit(ast::Lit::Int(ast::LitInt::new(width.to_string(), span)))
}

fn unresolved() -> String {
    "width of this value is not declared anywhere; \
     give it one with `as bits<N>`"
        .to_string()
}
