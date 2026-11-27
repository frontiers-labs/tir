//! Fast zero-copy Chumsky parser for CDSL v3 (plus lossless CST parser mode).

use chumsky::{
    error::{EmptyErr, Rich},
    extra,
    input::InputRef,
    prelude::*,
};

use crate::{
    ast::{
        Ast, BinOp, Ctor, Decl, DeclId, Expr, ExprId, ExtractTarget, Lit, Pattern, Slice, Sort,
        Stmt, UseItems,
    },
    lexer::{Span, Token, TokenStream},
};

#[derive(Clone, Debug)]
pub struct ParseError {
    pub found: Option<String>,
    pub spans: Vec<Span>,
    pub expected: Vec<String>,
}

/// Fast-path zero-copy parse into flat-arena `Ast<'src>`.
///
/// Uses zero-sized `EmptyErr` on the hot path so valid CDSL parses with zero
/// error-tracking allocations; only on syntax error does it re-run with `Rich`
/// to build detailed diagnostics.
pub fn parse<'src>(stream: &TokenStream<'src>) -> Result<Ast<'src>, Vec<ParseError>> {
    let mut state = extra::SimpleState(Ast::with_token_capacity(stream.tokens.len()));
    let parser = fast_module::<EmptyErr>();
    if let Some(root) = parser
        .parse_with_state(&stream.tokens, &mut state)
        .into_output()
    {
        state.0.root = root;
        return Ok(state.0);
    }
    let mut diag_state = extra::SimpleState(Ast::with_token_capacity(stream.tokens.len()));
    let errors = fast_module::<Rich<'_, Token<'src>>>()
        .parse_with_state(&stream.tokens, &mut diag_state)
        .into_errors();
    Err(diagnostics(stream, errors))
}

pub(crate) fn parse_lossless<'a, 'src: 'a>(
    stream: &'a TokenStream<'src>,
    ctx: &'a crate::parser_syntax::Context<'a>,
) -> Result<Vec<crate::parser_syntax::DeclNode>, Vec<ParseError>> {
    lossless::module(ctx)
        .parse(&stream.tokens)
        .into_result()
        .map_err(|errors| diagnostics(stream, errors))
}

fn diagnostics<'src>(
    stream: &TokenStream<'src>,
    errors: Vec<Rich<'_, Token<'src>>>,
) -> Vec<ParseError> {
    errors
        .into_iter()
        .map(|error| {
            let range = error.span().into_range();
            let mut spans: Vec<Span> = stream
                .spans
                .get(range)
                .unwrap_or(&[])
                .iter()
                .map(|&(s, e)| Span::Source(s as usize..e as usize))
                .collect();
            if spans.is_empty() {
                let end = stream.end as usize;
                spans.push(Span::Source(end..end));
            }
            ParseError {
                spans,
                found: error.found().map(|tok| format!("{tok:?}")),
                expected: error
                    .expected()
                    .map(|expected| format!("{expected:?}"))
                    .collect(),
            }
        })
        .collect()
}

type FastExtra<'src, E> = extra::Full<E, extra::SimpleState<Ast<'src>>, ()>;

const TUPLE_FIELDS: [&str; 16] = [
    "_0", "_1", "_2", "_3", "_4", "_5", "_6", "_7", "_8", "_9", "_10", "_11", "_12", "_13", "_14",
    "_15",
];

#[inline(always)]
fn as_ident<'src>(tok: Token<'src>) -> Option<&'src str> {
    match tok {
        Token::Ident(s) | Token::RawIdent(s) => Some(s),
        Token::Wildcard => Some("_"),
        Token::Prop => Some("Prop"),
        Token::Memo => Some("memo"),
        Token::Op => Some("op"),
        _ => None,
    }
}

#[inline(always)]
fn as_binop(tok: Token<'_>) -> Option<BinOp> {
    match tok {
        Token::Star => Some(BinOp::Mul),
        Token::Slash => Some(BinOp::Div),
        Token::Percent => Some(BinOp::Rem),
        Token::Plus => Some(BinOp::Add),
        Token::Minus => Some(BinOp::Sub),
        Token::PlusPlus => Some(BinOp::StrCat),
        Token::EqEq => Some(BinOp::Eq),
        Token::NotEq => Some(BinOp::Neq),
        Token::Less => Some(BinOp::Lt),
        Token::LessEq => Some(BinOp::Le),
        Token::Greater => Some(BinOp::Gt),
        Token::GreaterEq => Some(BinOp::Ge),
        Token::AndAnd => Some(BinOp::And),
        Token::OrOr => Some(BinOp::Or),
        _ => None,
    }
}

/// Zero-backtracking LL(1) + Pratt fast parser running inside Chumsky `custom`.
///
/// Uses mark/truncate scratch buffers instead of per-call stack arrays so parsing
/// executes with zero per-node heap allocations, zero stack memsets, and zero
/// dynamic `Recursive` dispatch.
struct FastParser<'a, 'b, 'c, 'src, E>
where
    'src: 'a,
    'a: 'b,
    E: chumsky::error::Error<'a, &'a [Token<'src>]> + 'a,
{
    inp: &'c mut InputRef<'a, 'b, &'a [Token<'src>], FastExtra<'src, E>>,
    s_idents: Vec<&'src str>,
    s_exprs: Vec<ExprId>,
    s_fields: Vec<(&'src str, ExprId)>,
    s_params: Vec<(&'src str, Option<ExprId>)>,
    s_stmts: Vec<Stmt<'src>>,
    s_arms: Vec<(Pattern<'src>, ExprId)>,
    s_ctors: Vec<Ctor<'src>>,
    s_ctor_maps: Vec<(&'src str, &'src str)>,
    s_decls: Vec<DeclId>,
}

impl<'a, 'b, 'c, 'src, E> FastParser<'a, 'b, 'c, 'src, E>
where
    'src: 'a,
    'a: 'b,
    E: chumsky::error::Error<'a, &'a [Token<'src>]> + 'a,
{
    fn new(inp: &'c mut InputRef<'a, 'b, &'a [Token<'src>], FastExtra<'src, E>>) -> Self {
        Self {
            inp,
            s_idents: Vec::with_capacity(32),
            s_exprs: Vec::with_capacity(32),
            s_fields: Vec::with_capacity(32),
            s_params: Vec::with_capacity(16),
            s_stmts: Vec::with_capacity(32),
            s_arms: Vec::with_capacity(16),
            s_ctors: Vec::with_capacity(16),
            s_ctor_maps: Vec::with_capacity(16),
            s_decls: Vec::with_capacity(64),
        }
    }

    #[inline(always)]
    fn ast(&mut self) -> &mut Ast<'src> {
        &mut self.inp.state().0
    }

    #[inline(always)]
    fn eat(&mut self, tok: Token<'src>) -> bool {
        if self.inp.peek() == Some(tok) {
            let _ = self.inp.next();
            true
        } else {
            false
        }
    }

    #[inline(always)]
    fn expect(&mut self, tok: Token<'src>) -> Result<(), E> {
        if self.inp.peek() == Some(tok) {
            let _ = self.inp.next();
            Ok(())
        } else {
            self.inp.parse(just(tok)).map(|_| ())
        }
    }

    #[inline(always)]
    fn fail_expected(&mut self, tok: Token<'src>) -> E {
        self.inp.parse(just(tok)).unwrap_err()
    }

    #[inline(always)]
    fn parse_ident(&mut self) -> Result<&'src str, E> {
        if let Some(tok) = self.inp.peek()
            && let Some(s) = as_ident(tok)
        {
            let _ = self.inp.next();
            return Ok(s);
        }
        Err(self.fail_expected(Token::Ident("identifier")))
    }

    #[inline(always)]
    fn parse_str(&mut self) -> Result<&'src str, E> {
        if let Some(Token::Str(s)) = self.inp.peek() {
            let _ = self.inp.next();
            return Ok(s);
        }
        Err(self.fail_expected(Token::Str("")))
    }

    #[inline]
    fn parse_path(&mut self) -> Result<Slice, E> {
        let first = self.parse_ident()?;
        if self.inp.peek() != Some(Token::PathSep) {
            return Ok(self.ast().push_ident_one(first));
        }
        let mark = self.s_idents.len();
        self.s_idents.push(first);
        while self.inp.peek() == Some(Token::PathSep) {
            let saved = self.inp.save();
            let _ = self.inp.next();
            if matches!(
                self.inp.peek(),
                Some(Token::LBrace | Token::Star | Token::Less)
            ) {
                self.inp.rewind(saved);
                break;
            }
            let id = self.parse_ident()?;
            self.s_idents.push(id);
        }
        let slice = self.inp.state().0.push_idents(&self.s_idents[mark..]);
        self.s_idents.truncate(mark);
        Ok(slice)
    }

    #[inline]
    fn parse_use_path_and_items(&mut self) -> Result<(Slice, UseItems<'src>), E> {
        let mark = self.s_idents.len();
        let first = self.parse_ident()?;
        self.s_idents.push(first);
        let mut items = UseItems::Single;
        while self.eat(Token::PathSep) {
            match self.inp.peek() {
                Some(Token::LBrace) => {
                    let _ = self.inp.next();
                    let gmark = self.s_idents.len();
                    while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
                        let id = self.parse_ident()?;
                        self.s_idents.push(id);
                        if !self.eat(Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::RBrace)?;
                    let gslice = self.inp.state().0.push_idents(&self.s_idents[gmark..]);
                    self.s_idents.truncate(gmark);
                    items = UseItems::Group(gslice);
                    break;
                }
                Some(Token::Star) => {
                    let _ = self.inp.next();
                    items = UseItems::Glob;
                    break;
                }
                _ => {
                    let id = self.parse_ident()?;
                    self.s_idents.push(id);
                }
            }
        }
        if matches!(items, UseItems::Single) && self.eat(Token::As) {
            items = UseItems::Alias(self.parse_ident()?);
        }
        let path = self.inp.state().0.push_idents(&self.s_idents[mark..]);
        self.s_idents.truncate(mark);
        Ok((path, items))
    }

    #[inline]
    fn parse_sort_annot(&mut self) -> Result<Sort, E> {
        if !self.eat(Token::Colon) {
            return Ok(Sort::Type);
        }
        match self.inp.peek() {
            Some(Token::Prop) => {
                let _ = self.inp.next();
                Ok(Sort::Prop)
            }
            Some(Token::Ident("Type")) => {
                let _ = self.inp.next();
                Ok(Sort::Type)
            }
            _ => Err(self.fail_expected(Token::Ident("Type"))),
        }
    }

    fn parse_ty_params(&mut self) -> Result<Slice, E> {
        if !self.eat(Token::Less) {
            return Ok(Slice::EMPTY);
        }
        let mark = self.s_fields.len();
        while !matches!(self.inp.peek(), Some(Token::Greater) | None) {
            let name = self.parse_ident()?;
            let ty = if self.eat(Token::Colon) {
                self.parse_type()?
            } else {
                self.ast().push_expr(Expr::Sort(Sort::Type))
            };
            self.s_fields.push((name, ty));
            if !self.eat(Token::Comma) {
                break;
            }
        }
        self.expect(Token::Greater)?;
        let slice = self.inp.state().0.push_fields(&self.s_fields[mark..]);
        self.s_fields.truncate(mark);
        Ok(slice)
    }

    fn parse_binder_names(&mut self) -> Result<Slice, E> {
        if !self.eat(Token::Less) {
            return Ok(Slice::EMPTY);
        }
        let mark = self.s_idents.len();
        while !matches!(self.inp.peek(), Some(Token::Greater) | None) {
            let id = self.parse_ident()?;
            self.s_idents.push(id);
            if !self.eat(Token::Comma) {
                break;
            }
        }
        self.expect(Token::Greater)?;
        let slice = self.inp.state().0.push_idents(&self.s_idents[mark..]);
        self.s_idents.truncate(mark);
        Ok(slice)
    }

    // Checks without mutating AST whether the upcoming `let` (after `let <name>`) is a rule sugar.
    fn is_rule_after_name(&mut self) -> bool {
        let saved = self.inp.save();
        if self.eat(Token::Less) {
            while !matches!(self.inp.peek(), Some(Token::Greater) | None) {
                if self.inp.peek().and_then(as_ident).is_none() {
                    self.inp.rewind(saved);
                    return false;
                }
                let _ = self.inp.next();
                if !self.eat(Token::Comma) {
                    break;
                }
            }
            if !self.eat(Token::Greater) {
                self.inp.rewind(saved);
                return false;
            }
        }
        let is_rule = self.eat(Token::Colon)
            && matches!(
                self.inp.peek(),
                Some(Token::Ident("Rewrite" | "Refine" | "Rule"))
            )
            && {
                let _ = self.inp.next();
                self.inp.peek() == Some(Token::LParen)
            };
        self.inp.rewind(saved);
        is_rule
    }

    fn parse_type(&mut self) -> Result<ExprId, E> {
        let mut lhs = match self.inp.peek() {
            Some(Token::Prop) => {
                let _ = self.inp.next();
                self.ast().push_expr(Expr::Sort(Sort::Prop))
            }
            Some(Token::Ident("Type")) => {
                let _ = self.inp.next();
                self.ast().push_expr(Expr::Sort(Sort::Type))
            }
            Some(Token::Natural(n)) => {
                let _ = self.inp.next();
                self.ast().push_expr(Expr::Lit(Lit::Int(n)))
            }
            Some(Token::DotDot) => {
                let _ = self.inp.next();
                let inner = self.parse_type()?;
                let ast = self.ast();
                let p = ast.push_ident_one("Variadic");
                let args = ast.push_expr_one(inner);
                ast.push_expr(Expr::Path {
                    path: p,
                    ty_args: args,
                })
            }
            Some(Token::LParen) => {
                let _ = self.inp.next();
                let mark = self.s_exprs.len();
                let mut had_comma = false;
                while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                    let t = self.parse_type()?;
                    self.s_exprs.push(t);
                    if self.eat(Token::Comma) {
                        had_comma = true;
                    } else {
                        break;
                    }
                }
                self.expect(Token::RParen)?;
                if self.eat(Token::Arrow) {
                    let ret = self.parse_type()?;
                    let slice = &self.s_exprs[mark..];
                    let res = if slice.is_empty() {
                        let ast = &mut self.inp.state().0;
                        let up = ast.push_ident_one("Unit");
                        let uty = ast.push_expr(Expr::Path {
                            path: up,
                            ty_args: Slice::EMPTY,
                        });
                        ast.push_expr(Expr::Pi {
                            param: "_",
                            dom: uty,
                            cod: ret,
                        })
                    } else {
                        let ast = &mut self.inp.state().0;
                        let mut acc = ret;
                        for &dom in slice.iter().rev() {
                            acc = ast.push_expr(Expr::Pi {
                                param: "_",
                                dom,
                                cod: acc,
                            });
                        }
                        acc
                    };
                    self.s_exprs.truncate(mark);
                    return Ok(res);
                }
                let slice = &self.s_exprs[mark..];
                let res = match slice {
                    [] => {
                        let ast = &mut self.inp.state().0;
                        let p = ast.push_ident_one("Unit");
                        ast.push_expr(Expr::Path {
                            path: p,
                            ty_args: Slice::EMPTY,
                        })
                    }
                    [single] if !had_comma => *single,
                    many => {
                        let fmark = self.s_fields.len();
                        for (i, &t) in many.iter().enumerate() {
                            let label = TUPLE_FIELDS.get(i).copied().unwrap_or("_n");
                            self.s_fields.push((label, t));
                        }
                        let fs = self.inp.state().0.push_fields(&self.s_fields[fmark..]);
                        self.s_fields.truncate(fmark);
                        self.ast().push_expr(Expr::RecordTy(fs))
                    }
                };
                self.s_exprs.truncate(mark);
                res
            }
            Some(Token::LBrace) => {
                let _ = self.inp.next();
                let mark = self.s_fields.len();
                while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
                    let name = self.parse_ident()?;
                    self.expect(Token::Colon)?;
                    let t = self.parse_type()?;
                    self.s_fields.push((name, t));
                    if !self.eat(Token::Comma) {
                        break;
                    }
                }
                self.expect(Token::RBrace)?;
                let fs = self.inp.state().0.push_fields(&self.s_fields[mark..]);
                self.s_fields.truncate(mark);
                self.ast().push_expr(Expr::RecordTy(fs))
            }
            _ => {
                let path = self.parse_path()?;
                let ty_args = if self.eat(Token::Less) {
                    let mark = self.s_exprs.len();
                    while !matches!(self.inp.peek(), Some(Token::Greater) | None) {
                        let t = self.parse_type()?;
                        self.s_exprs.push(t);
                        if !self.eat(Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::Greater)?;
                    let s = self.inp.state().0.push_exprs(&self.s_exprs[mark..]);
                    self.s_exprs.truncate(mark);
                    s
                } else {
                    Slice::EMPTY
                };
                let node = self.ast().push_expr(Expr::Path { path, ty_args });
                if self.eat(Token::LParen) {
                    let mark = self.s_exprs.len();
                    while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                        let a = self.parse_expr()?;
                        self.s_exprs.push(a);
                        if !self.eat(Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::RParen)?;
                    let args = self.inp.state().0.push_exprs(&self.s_exprs[mark..]);
                    self.s_exprs.truncate(mark);
                    self.ast().push_expr(Expr::App { func: node, args })
                } else {
                    node
                }
            }
        };

        while self.eat(Token::Plus) {
            let rhs = self.parse_type()?;
            lhs = self.ast().push_expr(Expr::Bin {
                op: BinOp::Add,
                lhs,
                rhs,
            });
        }

        if self.eat(Token::Arrow) {
            let rhs = self.parse_type()?;
            Ok(self.ast().push_expr(Expr::Pi {
                param: "_",
                dom: lhs,
                cod: rhs,
            }))
        } else {
            Ok(lhs)
        }
    }

    fn parse_lam_params(&mut self) -> Result<Slice, E> {
        self.expect(Token::Pipe)?;
        let mark = self.s_params.len();
        while !matches!(self.inp.peek(), Some(Token::Pipe) | None) {
            let name = self.parse_ident()?;
            let ty = if self.eat(Token::Colon) {
                Some(self.parse_type()?)
            } else {
                None
            };
            self.s_params.push((name, ty));
            if !self.eat(Token::Comma) {
                break;
            }
        }
        self.expect(Token::Pipe)?;
        let slice = self.inp.state().0.push_opt_params(&self.s_params[mark..]);
        self.s_params.truncate(mark);
        Ok(slice)
    }

    fn parse_rule_side(&mut self) -> Result<Slice, E> {
        if self.eat(Token::LParen) {
            let mark = self.s_exprs.len();
            while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                let e = self.parse_expr()?;
                self.s_exprs.push(e);
                if !self.eat(Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RParen)?;
            let slice = self.inp.state().0.push_exprs(&self.s_exprs[mark..]);
            self.s_exprs.truncate(mark);
            Ok(slice)
        } else {
            let e = self.parse_expr()?;
            Ok(self.ast().push_expr_one(e))
        }
    }

    fn parse_rule_spec(&mut self) -> Result<(&'src str, Slice, Slice), E> {
        let kind = match self.inp.peek() {
            Some(Token::Ident(k @ ("Rewrite" | "Refine" | "Rule"))) => {
                let _ = self.inp.next();
                k
            }
            _ => return Err(self.fail_expected(Token::Ident("Rewrite"))),
        };
        self.expect(Token::LParen)?;
        let lhs = self.parse_rule_side()?;
        if !self.eat(Token::Arrow) && !self.eat(Token::TildeArrow) {
            return Err(self.fail_expected(Token::Arrow));
        }
        let rhs = self.parse_rule_side()?;
        self.expect(Token::RParen)?;
        Ok((kind, lhs, rhs))
    }

    fn parse_port_list(&mut self) -> Result<ExprId, E> {
        self.expect(Token::LParen)?;
        let mark = self.s_exprs.len();
        while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
            let item = if self.eat(Token::DotDot) {
                let name = self.parse_ident()?;
                let ast = self.ast();
                let p = ast.push_idents(&["PortSpec", "Variadic"]);
                let head = ast.push_expr(Expr::Path {
                    path: p,
                    ty_args: Slice::EMPTY,
                });
                let s = ast.push_expr(Expr::Lit(Lit::Str(name)));
                let args = ast.push_expr_one(s);
                ast.push_expr(Expr::App { func: head, args })
            } else {
                let name = self.parse_ident()?;
                let ctor = if self.eat(Token::Question) {
                    "Optional"
                } else {
                    "Fixed"
                };
                let ast = self.ast();
                let p = ast.push_idents(&["PortSpec", ctor]);
                let head = ast.push_expr(Expr::Path {
                    path: p,
                    ty_args: Slice::EMPTY,
                });
                let s = ast.push_expr(Expr::Lit(Lit::Str(name)));
                let args = ast.push_expr_one(s);
                ast.push_expr(Expr::App { func: head, args })
            };
            self.s_exprs.push(item);
            if !self.eat(Token::Comma) {
                break;
            }
        }
        self.expect(Token::RParen)?;
        let args = self.inp.state().0.push_exprs(&self.s_exprs[mark..]);
        self.s_exprs.truncate(mark);
        let ast = self.ast();
        let p = ast.push_ident_one("List");
        let head = ast.push_expr(Expr::Path {
            path: p,
            ty_args: Slice::EMPTY,
        });
        Ok(ast.push_expr(Expr::App { func: head, args }))
    }

    fn is_typed_port_list(&mut self) -> bool {
        let saved = self.inp.save();
        let ok = self.eat(Token::LParen)
            && (matches!(self.inp.peek(), Some(Token::RParen))
                || (self.inp.peek().and_then(as_ident).is_some() && {
                    let _ = self.inp.next();
                    self.inp.peek() == Some(Token::Colon)
                }));
        self.inp.rewind(saved);
        ok
    }

    fn parse_typed_port_list(&mut self) -> Result<ExprId, E> {
        self.expect(Token::LParen)?;
        let mark = self.s_fields.len();
        while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
            let saved = self.inp.save();
            let (name, ty) = if self.inp.peek().and_then(as_ident).is_some() {
                let n = self.parse_ident()?;
                if self.eat(Token::Colon) {
                    (n, self.parse_type()?)
                } else {
                    self.inp.rewind(saved);
                    let idx = self.s_fields.len() - mark;
                    let label = TUPLE_FIELDS.get(idx).copied().unwrap_or("result");
                    (label, self.parse_type()?)
                }
            } else {
                let idx = self.s_fields.len() - mark;
                let label = TUPLE_FIELDS.get(idx).copied().unwrap_or("result");
                (label, self.parse_type()?)
            };
            self.s_fields.push((name, ty));
            if !self.eat(Token::Comma) {
                break;
            }
        }
        self.expect(Token::RParen)?;
        let fs = self.inp.state().0.push_fields(&self.s_fields[mark..]);
        self.s_fields.truncate(mark);
        Ok(self.ast().push_expr(Expr::RecordTy(fs)))
    }

    #[allow(clippy::cognitive_complexity)]
    fn parse_op_sugar(&mut self) -> Result<ExprId, E> {
        self.expect(Token::LBrace)?;
        let mark = self.s_fields.len();
        while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
            let key = self.parse_ident()?;
            self.expect(Token::Colon)?;
            match key {
                "signature" => {
                    let ops = self.parse_typed_port_list()?;
                    self.expect(Token::Arrow)?;
                    let ress = if self.inp.peek() == Some(Token::LParen) {
                        self.parse_typed_port_list()?
                    } else {
                        let rty = self.parse_type()?;
                        let fs = self.ast().push_fields(&[("result", rty)]);
                        self.ast().push_expr(Expr::RecordTy(fs))
                    };
                    self.s_fields.push(("sig_operands", ops));
                    self.s_fields.push(("sig_results", ress));
                }
                "operands" => {
                    let val = if self.is_typed_port_list() {
                        self.parse_typed_port_list()?
                    } else {
                        self.parse_port_list()?
                    };
                    let field_key = if matches!(self.ast().expr(val), Expr::RecordTy(_)) {
                        "sig_operands"
                    } else {
                        "operands"
                    };
                    self.s_fields.push((field_key, val));
                }
                "results" => {
                    let val = if self.is_typed_port_list() {
                        self.parse_typed_port_list()?
                    } else {
                        self.parse_port_list()?
                    };
                    let field_key = if matches!(self.ast().expr(val), Expr::RecordTy(_)) {
                        "sig_results"
                    } else {
                        "results"
                    };
                    self.s_fields.push((field_key, val));
                }
                "regions" => {
                    let val = if self.is_typed_port_list() {
                        self.parse_typed_port_list()?
                    } else {
                        self.parse_port_list()?
                    };
                    let field_key = if matches!(self.ast().expr(val), Expr::RecordTy(_)) {
                        "sig_regions"
                    } else {
                        "regions"
                    };
                    self.s_fields.push((field_key, val));
                }
                "successors" => {
                    let val = if self.is_typed_port_list() {
                        self.parse_typed_port_list()?
                    } else {
                        self.parse_port_list()?
                    };
                    let field_key = if matches!(self.ast().expr(val), Expr::RecordTy(_)) {
                        "sig_successors"
                    } else {
                        "successors"
                    };
                    self.s_fields.push((field_key, val));
                }
                "attributes" => {
                    if self.inp.peek() == Some(Token::LParen) {
                        self.expect(Token::LParen)?;
                        let emark = self.s_exprs.len();
                        while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                            let name = self.parse_ident()?;
                            let id = self.ast().push_expr(Expr::Lit(Lit::Str(name)));
                            self.s_exprs.push(id);
                            if !self.eat(Token::Comma) {
                                break;
                            }
                        }
                        self.expect(Token::RParen)?;
                        let args = self.inp.state().0.push_exprs(&self.s_exprs[emark..]);
                        self.s_exprs.truncate(emark);
                        let ast = self.ast();
                        let p = ast.push_ident_one("List");
                        let head = ast.push_expr(Expr::Path {
                            path: p,
                            ty_args: Slice::EMPTY,
                        });
                        let val = ast.push_expr(Expr::App { func: head, args });
                        self.s_fields.push(("attributes", val));
                    } else {
                        let val = self.parse_type()?;
                        self.s_fields.push(("sig_attributes", val));
                    }
                }
                "assembly" => {
                    let val = self.parse_type()?;
                    self.s_fields.push(("sig_assembly", val));
                }
                "interfaces" => {
                    if self.inp.peek() == Some(Token::LParen) {
                        self.expect(Token::LParen)?;
                        let emark = self.s_exprs.len();
                        while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                            let item = self.parse_expr()?;
                            self.s_exprs.push(item);
                            if !self.eat(Token::Comma) {
                                break;
                            }
                        }
                        self.expect(Token::RParen)?;
                        let args = self.inp.state().0.push_exprs(&self.s_exprs[emark..]);
                        self.s_exprs.truncate(emark);
                        let ast = self.ast();
                        let p = ast.push_ident_one("List");
                        let head = ast.push_expr(Expr::Path {
                            path: p,
                            ty_args: Slice::EMPTY,
                        });
                        let val = ast.push_expr(Expr::App { func: head, args });
                        self.s_fields.push(("interfaces", val));
                    } else {
                        let item = self.parse_expr()?;
                        let args = self.inp.state().0.push_exprs(&[item]);
                        let ast = self.ast();
                        let p = ast.push_ident_one("List");
                        let head = ast.push_expr(Expr::Path {
                            path: p,
                            ty_args: Slice::EMPTY,
                        });
                        let val = ast.push_expr(Expr::App { func: head, args });
                        self.s_fields.push(("interfaces", val));
                    }
                }
                _ => {
                    let val = self.parse_expr()?;
                    self.s_fields.push((key, val));
                }
            }
            if !self.eat(Token::Comma) {
                break;
            }
        }
        self.expect(Token::RBrace)?;
        let fs = self.inp.state().0.push_fields(&self.s_fields[mark..]);
        self.s_fields.truncate(mark);
        Ok(self.ast().push_expr(Expr::OpSugar(fs)))
    }

    fn parse_pattern(&mut self) -> Result<Pattern<'src>, E> {
        match self.inp.peek() {
            Some(Token::Wildcard) => {
                let _ = self.inp.next();
                Ok(Pattern::Wild)
            }
            Some(Token::Natural(n)) => {
                let _ = self.inp.next();
                Ok(Pattern::Lit(Lit::Int(n)))
            }
            Some(Token::Str(s)) => {
                let _ = self.inp.next();
                Ok(Pattern::Lit(Lit::Str(s)))
            }
            Some(Token::True) => {
                let _ = self.inp.next();
                Ok(Pattern::Lit(Lit::Bool(true)))
            }
            Some(Token::False) => {
                let _ = self.inp.next();
                Ok(Pattern::Lit(Lit::Bool(false)))
            }
            Some(Token::LParen) => {
                let _ = self.inp.next();
                self.expect(Token::RParen)?;
                Ok(Pattern::Lit(Lit::Unit))
            }
            _ => {
                let path = self.parse_path()?;
                if self.eat(Token::LParen) {
                    let mark = self.s_idents.len();
                    while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                        let id = self.parse_ident()?;
                        self.s_idents.push(id);
                        if !self.eat(Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::RParen)?;
                    let fields = self.inp.state().0.push_idents(&self.s_idents[mark..]);
                    self.s_idents.truncate(mark);
                    Ok(Pattern::Ctor { path, fields })
                } else {
                    let segs = self.ast().ident_slice(path);
                    let is_ctor = segs.len() > 1
                        || segs
                            .first()
                            .and_then(|s| s.as_bytes().first())
                            .is_some_and(u8::is_ascii_uppercase);
                    if is_ctor {
                        Ok(Pattern::Ctor {
                            path,
                            fields: Slice::EMPTY,
                        })
                    } else {
                        Ok(Pattern::Var(segs[0]))
                    }
                }
            }
        }
    }

    fn parse_block(&mut self) -> Result<ExprId, E> {
        self.expect(Token::LBrace)?;
        let mark = self.s_stmts.len();
        let mut final_expr = None;
        while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
            if self.eat(Token::Let) {
                let name = self.parse_ident()?;
                if self.is_rule_after_name() {
                    let vars = self.parse_binder_names()?;
                    self.expect(Token::Colon)?;
                    let (kind, lhs, rhs) = self.parse_rule_spec()?;
                    self.expect(Token::Semi)?;
                    self.s_stmts.push(Stmt::Rule {
                        name,
                        vars,
                        kind,
                        lhs,
                        rhs,
                    });
                } else {
                    let ty = if self.eat(Token::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    self.expect(Token::Assign)?;
                    let rhs = self.parse_expr()?;
                    self.expect(Token::Semi)?;
                    self.s_stmts.push(Stmt::Let { name, ty, rhs });
                }
            } else {
                let ex = self.parse_expr()?;
                if self.eat(Token::Semi) {
                    self.s_stmts.push(Stmt::Expr(ex));
                } else {
                    final_expr = Some(ex);
                    break;
                }
            }
        }
        self.expect(Token::RBrace)?;
        let stmts = self.inp.state().0.push_stmts(&self.s_stmts[mark..]);
        self.s_stmts.truncate(mark);
        let fe = final_expr.unwrap_or_else(|| self.ast().push_expr(Expr::Lit(Lit::Unit)));
        Ok(self.ast().push_expr(Expr::Block {
            stmts,
            final_expr: fe,
        }))
    }

    fn parse_brace_expr(&mut self) -> Result<ExprId, E> {
        let saved = self.inp.save();
        let _ = self.inp.next(); // consume '{'
        let is_record = match self.inp.peek() {
            Some(Token::RBrace) => true,
            Some(Token::Ident(_) | Token::RawIdent(_)) => {
                let _ = self.inp.next();
                self.inp.peek() == Some(Token::Colon)
            }
            _ => false,
        };
        self.inp.rewind(saved);
        if is_record {
            let _ = self.inp.next(); // '{'
            let mark = self.s_fields.len();
            while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
                let name = match self.inp.peek() {
                    Some(Token::Ident(s) | Token::RawIdent(s)) => {
                        let _ = self.inp.next();
                        s
                    }
                    _ => return Err(self.fail_expected(Token::Ident("field"))),
                };
                self.expect(Token::Colon)?;
                let val = self.parse_expr()?;
                self.s_fields.push((name, val));
                if !self.eat(Token::Comma) {
                    break;
                }
            }
            self.expect(Token::RBrace)?;
            let fs = self.inp.state().0.push_fields(&self.s_fields[mark..]);
            self.s_fields.truncate(mark);
            Ok(self.ast().push_expr(Expr::Record(fs)))
        } else {
            self.parse_block()
        }
    }

    #[allow(clippy::cognitive_complexity)]
    fn parse_atom(&mut self) -> Result<ExprId, E> {
        match self.inp.peek() {
            Some(Token::Ident("Type")) => {
                let _ = self.inp.next();
                Ok(self.ast().push_expr(Expr::Sort(Sort::Type)))
            }
            Some(Token::Ident(_) | Token::RawIdent(_) | Token::Wildcard) => {
                let path = self.parse_path()?;
                let ty_args = if self.inp.peek() == Some(Token::PathSep) {
                    let saved = self.inp.save();
                    let _ = self.inp.next();
                    if self.eat(Token::Less) {
                        let mark = self.s_exprs.len();
                        while !matches!(self.inp.peek(), Some(Token::Greater) | None) {
                            let a = self.parse_type()?;
                            self.s_exprs.push(a);
                            if !self.eat(Token::Comma) {
                                break;
                            }
                        }
                        self.expect(Token::Greater)?;
                        let s = self.inp.state().0.push_exprs(&self.s_exprs[mark..]);
                        self.s_exprs.truncate(mark);
                        s
                    } else {
                        self.inp.rewind(saved);
                        Slice::EMPTY
                    }
                } else {
                    Slice::EMPTY
                };
                Ok(self.ast().push_expr(Expr::Path { path, ty_args }))
            }
            Some(Token::Natural(n)) => {
                let _ = self.inp.next();
                Ok(self.ast().push_expr(Expr::Lit(Lit::Int(n))))
            }
            Some(Token::Str(s)) => {
                let _ = self.inp.next();
                Ok(self.ast().push_expr(Expr::Lit(Lit::Str(s))))
            }
            Some(Token::True) => {
                let _ = self.inp.next();
                Ok(self.ast().push_expr(Expr::Lit(Lit::Bool(true))))
            }
            Some(Token::False) => {
                let _ = self.inp.next();
                Ok(self.ast().push_expr(Expr::Lit(Lit::Bool(false))))
            }
            Some(Token::LParen) => {
                let _ = self.inp.next();
                let mark = self.s_exprs.len();
                while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                    let e = self.parse_expr()?;
                    self.s_exprs.push(e);
                    if !self.eat(Token::Comma) {
                        break;
                    }
                }
                self.expect(Token::RParen)?;
                let items = &self.s_exprs[mark..];
                let res = match items {
                    [] => self.inp.state().0.push_expr(Expr::Lit(Lit::Unit)),
                    [single] => *single,
                    many => {
                        let fmark = self.s_fields.len();
                        for (i, &item) in many.iter().enumerate() {
                            let label = TUPLE_FIELDS.get(i).copied().unwrap_or("_n");
                            self.s_fields.push((label, item));
                        }
                        let fs = self.inp.state().0.push_fields(&self.s_fields[fmark..]);
                        self.s_fields.truncate(fmark);
                        self.inp.state().0.push_expr(Expr::Record(fs))
                    }
                };
                self.s_exprs.truncate(mark);
                Ok(res)
            }
            Some(Token::LBrace) => self.parse_brace_expr(),
            Some(Token::LBracket) => {
                let _ = self.inp.next();
                let mark = self.s_exprs.len();
                while !matches!(self.inp.peek(), Some(Token::RBracket) | None) {
                    let e = self.parse_expr()?;
                    self.s_exprs.push(e);
                    if !self.eat(Token::Comma) {
                        break;
                    }
                }
                self.expect(Token::RBracket)?;
                let args = self.inp.state().0.push_exprs(&self.s_exprs[mark..]);
                self.s_exprs.truncate(mark);
                let ast = self.ast();
                let p = ast.push_ident_one("List");
                let head = ast.push_expr(Expr::Path {
                    path: p,
                    ty_args: Slice::EMPTY,
                });
                Ok(ast.push_expr(Expr::App { func: head, args }))
            }
            Some(Token::Pipe) => {
                let params = self.parse_lam_params()?;
                let ret = if self.eat(Token::Arrow) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let body = self.parse_expr()?;
                Ok(self.ast().push_expr(Expr::Lam { params, ret, body }))
            }
            Some(Token::OrOr) => {
                let _ = self.inp.next();
                let ret = if self.eat(Token::Arrow) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let body = self.parse_expr()?;
                let ast = self.ast();
                let up = ast.push_ident_one("Unit");
                let uty = ast.push_expr(Expr::Path {
                    path: up,
                    ty_args: Slice::EMPTY,
                });
                let params = ast.push_opt_params(&[("_", Some(uty))]);
                Ok(ast.push_expr(Expr::Lam { params, ret, body }))
            }
            Some(Token::Match) => {
                let _ = self.inp.next();
                let scrut = self.parse_expr()?;
                self.expect(Token::LBrace)?;
                let mark = self.s_arms.len();
                while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
                    let pat = self.parse_pattern()?;
                    self.expect(Token::FatArrow)?;
                    let rhs = self.parse_expr()?;
                    self.s_arms.push((pat, rhs));
                    if !self.eat(Token::Comma) {
                        break;
                    }
                }
                self.expect(Token::RBrace)?;
                let arms = self.inp.state().0.push_arms(&self.s_arms[mark..]);
                self.s_arms.truncate(mark);
                Ok(self.ast().push_expr(Expr::Match { scrut, arms }))
            }
            Some(Token::If) => {
                let _ = self.inp.next();
                let cond = self.parse_expr()?;
                let then_expr = self.parse_block()?;
                self.expect(Token::Else)?;
                let else_expr = self.parse_expr()?;
                Ok(self.ast().push_expr(Expr::If {
                    cond,
                    then_expr,
                    else_expr,
                }))
            }
            Some(Token::Prop) => {
                let _ = self.inp.next();
                Ok(self.ast().push_expr(Expr::Sort(Sort::Prop)))
            }
            Some(Token::Memo) => {
                let saved = self.inp.save();
                let _ = self.inp.next();
                match self.inp.peek() {
                    Some(Token::OrOr) => {
                        let _ = self.inp.next();
                        let body = self.parse_expr()?;
                        Ok(self.ast().push_expr(Expr::Memo {
                            params: Slice::EMPTY,
                            body,
                        }))
                    }
                    Some(Token::Pipe) => {
                        let params = self.parse_lam_params()?;
                        let body = self.parse_expr()?;
                        Ok(self.ast().push_expr(Expr::Memo { params, body }))
                    }
                    _ => {
                        self.inp.rewind(saved);
                        let path = self.parse_path()?;
                        Ok(self.ast().push_expr(Expr::Path {
                            path,
                            ty_args: Slice::EMPTY,
                        }))
                    }
                }
            }
            Some(Token::Op) => {
                let saved = self.inp.save();
                let _ = self.inp.next();
                let is_op_sugar = if self.inp.peek() == Some(Token::LBrace) {
                    let _ = self.inp.next();
                    match self.inp.peek() {
                        Some(Token::RBrace) => true,
                        Some(tok) if as_ident(tok).is_some() => {
                            let _ = self.inp.next();
                            self.inp.peek() == Some(Token::Colon)
                        }
                        _ => false,
                    }
                } else {
                    false
                };
                self.inp.rewind(saved);
                if is_op_sugar {
                    let _ = self.inp.next(); // consume `op`
                    self.parse_op_sugar()
                } else {
                    let path = self.parse_path()?;
                    Ok(self.ast().push_expr(Expr::Path {
                        path,
                        ty_args: Slice::EMPTY,
                    }))
                }
            }
            Some(Token::Rec) => {
                let _ = self.inp.next();
                let params = self.parse_lam_params()?;
                let ret = if self.eat(Token::Arrow) {
                    Some(self.parse_type()?)
                } else {
                    None
                };
                let body = self.parse_expr()?;
                Ok(self.ast().push_expr(Expr::RecFix { params, ret, body }))
            }
            _ => Err(self.fail_expected(Token::Ident("expression"))),
        }
    }

    #[inline]
    fn parse_postfix(&mut self) -> Result<ExprId, E> {
        let mut lhs = self.parse_atom()?;
        loop {
            match self.inp.peek() {
                Some(Token::LParen) => {
                    let _ = self.inp.next();
                    let mark = self.s_exprs.len();
                    while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                        let e = self.parse_expr()?;
                        self.s_exprs.push(e);
                        if !self.eat(Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::RParen)?;
                    let args = self.inp.state().0.push_exprs(&self.s_exprs[mark..]);
                    self.s_exprs.truncate(mark);
                    lhs = self.ast().push_expr(Expr::App { func: lhs, args });
                }
                Some(Token::Dot) => {
                    let _ = self.inp.next();
                    let field = match self.inp.peek() {
                        Some(Token::Natural(n)) => {
                            let _ = self.inp.next();
                            n
                        }
                        _ => self.parse_ident()?,
                    };
                    lhs = self.ast().push_expr(Expr::Proj { obj: lhs, field });
                }
                Some(Token::Question) => {
                    let _ = self.inp.next();
                    lhs = self.ast().push_expr(Expr::Try(lhs));
                }
                _ => return Ok(lhs),
            }
        }
    }

    #[inline]
    fn parse_unary(&mut self) -> Result<ExprId, E> {
        match self.inp.peek() {
            Some(Token::Bang) => {
                let _ = self.inp.next();
                let inner = self.parse_unary()?;
                Ok(self.ast().push_expr(Expr::Not(inner)))
            }
            Some(Token::Minus) => {
                let _ = self.inp.next();
                let inner = self.parse_unary()?;
                Ok(self.ast().push_expr(Expr::Neg(inner)))
            }
            _ => self.parse_postfix(),
        }
    }

    #[inline]
    fn parse_expr(&mut self) -> Result<ExprId, E> {
        self.parse_expr_prec(1)
    }

    #[inline]
    fn parse_expr_prec(&mut self, min_prec: u8) -> Result<ExprId, E> {
        let mut lhs = self.parse_unary()?;
        while let Some(tok) = self.inp.peek()
            && let Some(op) = as_binop(tok)
        {
            let prec = op.prec();
            if prec < min_prec {
                break;
            }
            let _ = self.inp.next();
            let rhs = self.parse_expr_prec(prec + 1)?;
            lhs = self.ast().push_expr(Expr::Bin { op, lhs, rhs });
        }
        Ok(lhs)
    }

    #[allow(clippy::cognitive_complexity)]
    fn parse_decl(&mut self) -> Result<DeclId, E> {
        let start = self.inp.cursor();
        let public = self.eat(Token::Pub);
        match self.inp.peek() {
            Some(Token::Mod) => {
                let _ = self.inp.next();
                let name = self.parse_ident()?;
                let body = if self.eat(Token::LBrace) {
                    let mark = self.s_decls.len();
                    while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
                        let d = self.parse_decl()?;
                        self.s_decls.push(d);
                    }
                    self.expect(Token::RBrace)?;
                    let slice = self.inp.state().0.push_decls(&self.s_decls[mark..]);
                    self.s_decls.truncate(mark);
                    Some(slice)
                } else {
                    self.expect(Token::Semi)?;
                    None
                };
                let sp = self.inp.span_since(&start);
                Ok(self.ast().push_decl(
                    Decl::Mod { public, name, body },
                    (sp.start as u32, sp.end as u32),
                ))
            }
            Some(Token::Use) => {
                let _ = self.inp.next();
                let (path, items) = self.parse_use_path_and_items()?;
                self.expect(Token::Semi)?;
                let sp = self.inp.span_since(&start);
                Ok(self.ast().push_decl(
                    Decl::Use {
                        public,
                        path,
                        items,
                    },
                    (sp.start as u32, sp.end as u32),
                ))
            }
            Some(Token::Type) => {
                let _ = self.inp.next();
                let name = self.parse_ident()?;
                let params = self.parse_ty_params()?;
                let sort = self.parse_sort_annot()?;
                self.expect(Token::Semi)?;
                let sp = self.inp.span_since(&start);
                Ok(self.ast().push_decl(
                    Decl::Type {
                        public,
                        name,
                        params,
                        sort,
                    },
                    (sp.start as u32, sp.end as u32),
                ))
            }
            Some(Token::Enum) => {
                let _ = self.inp.next();
                let name = self.parse_ident()?;
                let params = self.parse_ty_params()?;
                let sort = self.parse_sort_annot()?;
                self.expect(Token::LBrace)?;
                let mark = self.s_ctors.len();
                while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
                    let cname = self.parse_ident()?;
                    let fields = if self.eat(Token::LParen) {
                        let fmark = self.s_fields.len();
                        let mut idx = 0usize;
                        while !matches!(self.inp.peek(), Some(Token::RParen) | None) {
                            let ft = self.parse_type()?;
                            let fname = TUPLE_FIELDS.get(idx).copied().unwrap_or("_n");
                            self.s_fields.push((fname, ft));
                            idx += 1;
                            if !self.eat(Token::Comma) {
                                break;
                            }
                        }
                        self.expect(Token::RParen)?;
                        let s = self.inp.state().0.push_fields(&self.s_fields[fmark..]);
                        self.s_fields.truncate(fmark);
                        s
                    } else if self.eat(Token::LBrace) {
                        let fmark = self.s_fields.len();
                        while !matches!(self.inp.peek(), Some(Token::RBrace) | None) {
                            let fname = self.parse_ident()?;
                            self.expect(Token::Colon)?;
                            let ft = self.parse_type()?;
                            self.s_fields.push((fname, ft));
                            if !self.eat(Token::Comma) {
                                break;
                            }
                        }
                        self.expect(Token::RBrace)?;
                        let s = self.inp.state().0.push_fields(&self.s_fields[fmark..]);
                        self.s_fields.truncate(fmark);
                        s
                    } else {
                        Slice::EMPTY
                    };
                    let ret = if self.eat(Token::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    self.s_ctors.push(Ctor {
                        name: cname,
                        fields,
                        ret,
                    });
                    if !self.eat(Token::Comma) {
                        break;
                    }
                }
                self.expect(Token::RBrace)?;
                let ctors = self.inp.state().0.push_ctors(&self.s_ctors[mark..]);
                self.s_ctors.truncate(mark);
                let sp = self.inp.span_since(&start);
                Ok(self.ast().push_decl(
                    Decl::Enum {
                        public,
                        name,
                        params,
                        sort,
                        ctors,
                    },
                    (sp.start as u32, sp.end as u32),
                ))
            }
            Some(Token::Let) => {
                let _ = self.inp.next();
                let name = self.parse_ident()?;
                if self.is_rule_after_name() {
                    let vars = self.parse_binder_names()?;
                    self.expect(Token::Colon)?;
                    let (kind, lhs, rhs) = self.parse_rule_spec()?;
                    self.expect(Token::Semi)?;
                    let sp = self.inp.span_since(&start);
                    Ok(self.ast().push_decl(
                        Decl::Rule {
                            public,
                            name,
                            vars,
                            kind,
                            lhs,
                            rhs,
                        },
                        (sp.start as u32, sp.end as u32),
                    ))
                } else {
                    let tparams = self.parse_ty_params()?;
                    let ty = if self.eat(Token::Colon) {
                        Some(self.parse_type()?)
                    } else {
                        None
                    };
                    let body = if self.eat(Token::Assign) {
                        let is_type_alias = ty.is_some_and(|tid| {
                            matches!(self.ast().expr(tid), Expr::Sort(Sort::Type))
                        });
                        Some(if is_type_alias {
                            self.parse_type()?
                        } else {
                            self.parse_expr()?
                        })
                    } else {
                        None
                    };
                    self.expect(Token::Semi)?;
                    let sp = self.inp.span_since(&start);
                    Ok(self.ast().push_decl(
                        Decl::Let {
                            public,
                            name,
                            tparams,
                            ty,
                            body,
                        },
                        (sp.start as u32, sp.end as u32),
                    ))
                }
            }
            Some(Token::Extract) if !public => {
                let _ = self.inp.next();
                let target = match self.inp.peek() {
                    Some(Token::Ident("rust")) => {
                        let _ = self.inp.next();
                        ExtractTarget::Rust
                    }
                    Some(Token::Ident("egglog")) => {
                        let _ = self.inp.next();
                        ExtractTarget::Egglog
                    }
                    Some(Token::Ident("lean")) => {
                        let _ = self.inp.next();
                        ExtractTarget::Lean
                    }
                    _ => return Err(self.fail_expected(Token::Ident("rust"))),
                };
                let path = self.parse_path()?;
                self.expect(Token::FatArrow)?;
                let rhs = match self.inp.peek() {
                    Some(Token::Str(s)) => {
                        let _ = self.inp.next();
                        s
                    }
                    _ => self.parse_ident()?,
                };
                let ctors = if self.eat(Token::LBracket) {
                    let mark = self.s_ctor_maps.len();
                    while !matches!(self.inp.peek(), Some(Token::RBracket) | None) {
                        let c = self.parse_ident()?;
                        self.expect(Token::FatArrow)?;
                        let t = self.parse_str()?;
                        self.s_ctor_maps.push((c, t));
                        if !self.eat(Token::Comma) {
                            break;
                        }
                    }
                    self.expect(Token::RBracket)?;
                    let s = self.inp.state().0.push_ctor_maps(&self.s_ctor_maps[mark..]);
                    self.s_ctor_maps.truncate(mark);
                    Some(s)
                } else {
                    None
                };
                self.expect(Token::Semi)?;
                let sp = self.inp.span_since(&start);
                Ok(self.ast().push_decl(
                    Decl::Extract {
                        target,
                        path,
                        rhs,
                        ctors,
                    },
                    (sp.start as u32, sp.end as u32),
                ))
            }
            _ => Err(self.fail_expected(Token::Let)),
        }
    }

    fn parse_root(&mut self) -> Result<Slice, E> {
        let mark = self.s_decls.len();
        while self.inp.peek().is_some() {
            let d = self.parse_decl()?;
            self.s_decls.push(d);
        }
        let slice = self.inp.state().0.push_decls(&self.s_decls[mark..]);
        self.s_decls.truncate(mark);
        Ok(slice)
    }
}

fn fast_module<'a, 'src: 'a, E>()
-> impl Parser<'a, &'a [Token<'src>], Slice, FastExtra<'src, E>> + Clone
where
    E: chumsky::error::Error<'a, &'a [Token<'src>]> + 'a,
{
    custom(|inp| FastParser::new(inp).parse_root())
}

/// Lossless CST parser mirroring the exact CDSL v3 grammar for `syntax::parse_source` and `cdsl-format`.
mod lossless {
    use super::*;
    use crate::{
        parser_syntax::{Context, DeclNode, ExprNode, PatNode, wrap},
        syntax::SyntaxKind,
    };

    type LExtra<'a, 'src> = extra::Err<Rich<'a, Token<'src>>>;

    fn l_ident<'a, 'src: 'a>() -> impl Parser<'a, &'a [Token<'src>], (), LExtra<'a, 'src>> + Clone {
        select! {
            Token::Ident(_) => (),
            Token::RawIdent(_) => (),
            Token::Wildcard => (),
            Token::Prop => (),
            Token::Memo => (),
            Token::Op => (),
        }
    }

    fn l_strict_ident<'a, 'src: 'a>()
    -> impl Parser<'a, &'a [Token<'src>], (), LExtra<'a, 'src>> + Clone {
        select! {
            Token::Ident(_) => (),
            Token::RawIdent(_) => (),
        }
    }

    fn l_str<'a, 'src: 'a>() -> impl Parser<'a, &'a [Token<'src>], (), LExtra<'a, 'src>> + Clone {
        select! { Token::Str(_) => () }
    }

    fn l_path<'a, 'src: 'a>() -> impl Parser<'a, &'a [Token<'src>], (), LExtra<'a, 'src>> + Clone {
        l_ident().separated_by(just(Token::PathSep)).at_least(1)
    }

    fn l_sort<'a, 'src: 'a>() -> impl Parser<'a, &'a [Token<'src>], (), LExtra<'a, 'src>> + Clone {
        just(Token::Colon)
            .ignore_then(just(Token::Prop).or(just(Token::Ident("Type"))))
            .ignored()
            .or_not()
            .ignored()
    }

    fn l_ty_params<'a, 'src: 'a>()
    -> impl Parser<'a, &'a [Token<'src>], (), LExtra<'a, 'src>> + Clone {
        l_ident()
            .then(
                just(Token::Colon)
                    .ignore_then(choice((
                        just(Token::Prop).ignored(),
                        just(Token::Ident("Type")).ignored(),
                        l_path(),
                    )))
                    .or_not(),
            )
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .delimited_by(just(Token::Less), just(Token::Greater))
            .or_not()
            .ignored()
    }

    fn l_binder_names<'a, 'src: 'a>()
    -> impl Parser<'a, &'a [Token<'src>], (), LExtra<'a, 'src>> + Clone {
        l_ident()
            .separated_by(just(Token::Comma))
            .allow_trailing()
            .delimited_by(just(Token::Less), just(Token::Greater))
            .or_not()
            .ignored()
    }

    fn l_type<'a, 'src: 'a>(
        ctx: &'a Context<'a>,
    ) -> impl Parser<'a, &'a [Token<'src>], ExprNode, LExtra<'a, 'src>> + Clone {
        recursive(move |ty| {
            let prop = just::<_, &'a [Token<'src>], LExtra<'a, 'src>>(Token::Prop).map_with(
                move |_, e| wrap(ctx, SyntaxKind::Expression, e.span().into_range(), vec![]),
            );
            let type_kw = just::<_, &'a [Token<'src>], LExtra<'a, 'src>>(Token::Ident("Type"))
                .map_with(move |_, e| {
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), vec![])
                });
            let paren = ty
                .clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .then(just(Token::Arrow).ignore_then(ty.clone()).or_not())
                .map_with(
                    move |(mut items, ret),
                          e: &mut chumsky::input::MapExtra<
                        'a,
                        '_,
                        &'a [Token<'src>],
                        LExtra<'a, 'src>,
                    >| {
                        if let Some(r) = ret {
                            items.push(r);
                        }
                        wrap(ctx, SyntaxKind::Expression, e.span().into_range(), items)
                    },
                );
            let rec_ty = l_ident()
                .ignore_then(just(Token::Colon))
                .ignore_then(ty.clone())
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace))
                .map_with(move |items, e| {
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), items)
                });
            let named = l_path()
                .ignore_then(
                    ty.clone()
                        .separated_by(just(Token::Comma))
                        .allow_trailing()
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::Less), just(Token::Greater))
                        .or_not(),
                )
                .then(
                    ty.clone()
                        .separated_by(just(Token::Comma))
                        .allow_trailing()
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::LParen), just(Token::RParen))
                        .or_not(),
                )
                .map_with(move |(args, call_args), e| {
                    let mut ch = args.unwrap_or_default();
                    if let Some(ca) = call_args {
                        ch.extend(ca);
                    }
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });
            let nat_lit = select! { Token::Natural(_) => () }.map_with(
                move |_,
                      e: &mut chumsky::input::MapExtra<
                    'a,
                    '_,
                    &'a [Token<'src>],
                    LExtra<'a, 'src>,
                >| {
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), vec![])
                },
            );
            let variadic = just(Token::DotDot)
                .ignore_then(ty.clone())
                .map_with(move |inner, e| {
                    wrap(
                        ctx,
                        SyntaxKind::Expression,
                        e.span().into_range(),
                        vec![inner],
                    )
                });
            let atom = choice((prop, type_kw, nat_lit, variadic, paren, rec_ty, named));
            let add_ty = atom
                .clone()
                .then(
                    just(Token::Plus)
                        .ignore_then(atom)
                        .repeated()
                        .collect::<Vec<_>>(),
                )
                .map_with(move |(lhs, rest), e| {
                    if rest.is_empty() {
                        lhs
                    } else {
                        let mut ch = Vec::with_capacity(rest.len() + 1);
                        ch.push(lhs);
                        ch.extend(rest);
                        wrap(ctx, SyntaxKind::BinaryExpression, e.span().into_range(), ch)
                    }
                });
            add_ty
                .then(just(Token::Arrow).ignore_then(ty).or_not())
                .map_with(move |(lhs, rhs), e| match rhs {
                    Some(r) => wrap(
                        ctx,
                        SyntaxKind::BinaryExpression,
                        e.span().into_range(),
                        vec![lhs, r],
                    ),
                    None => lhs,
                })
        })
    }

    fn l_rule_spec<'a, 'src: 'a>(
        expr: impl Parser<'a, &'a [Token<'src>], ExprNode, LExtra<'a, 'src>> + Clone + 'a,
    ) -> impl Parser<'a, &'a [Token<'src>], Vec<ExprNode>, LExtra<'a, 'src>> + Clone {
        let tuple_or_one = choice((
            expr.clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
            expr.map(|e| vec![e]),
        ));
        let kind = select! { Token::Ident("Rewrite" | "Refine" | "Rule") => () };
        kind.ignore_then(
            tuple_or_one
                .clone()
                .then_ignore(just(Token::Arrow).or(just(Token::TildeArrow)))
                .then(tuple_or_one)
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map(|(mut a, b)| {
            a.extend(b);
            a
        })
    }

    fn l_expr<'a, 'src: 'a>(
        ctx: &'a Context<'a>,
        ty: impl Parser<'a, &'a [Token<'src>], ExprNode, LExtra<'a, 'src>> + Clone + 'a,
    ) -> impl Parser<'a, &'a [Token<'src>], ExprNode, LExtra<'a, 'src>> + Clone {
        recursive(move |expr| {
            let lam_params = l_ident()
                .ignore_then(just(Token::Colon).ignore_then(ty.clone()).or_not())
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::Pipe), just(Token::Pipe))
                .map(|ps| ps.into_iter().flatten().collect::<Vec<_>>());

            let lit = select! {
                Token::Natural(_) => (),
                Token::Str(_) => (),
                Token::True => (),
                Token::False => (),
                Token::Prop => (),
                Token::Ident("Type") => (),
            }
            .map_with(
                move |_,
                      e: &mut chumsky::input::MapExtra<
                    'a,
                    '_,
                    &'a [Token<'src>],
                    LExtra<'a, 'src>,
                >| {
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), vec![])
                },
            );

            let memo_expr = just(Token::Memo)
                .ignore_then(choice((
                    just(Token::OrOr).map(|_| vec![]),
                    lam_params.clone(),
                )))
                .then(expr.clone())
                .map_with(move |(mut ch, body), e| {
                    ch.push(body);
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let rec_expr = just(Token::Rec)
                .ignore_then(lam_params.clone())
                .then(just(Token::Arrow).ignore_then(ty.clone()).or_not())
                .then(expr.clone())
                .map_with(move |((mut ch, ret), body), e| {
                    if let Some(r) = ret {
                        ch.push(r);
                    }
                    ch.push(body);
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let lam_expr = lam_params
                .then(just(Token::Arrow).ignore_then(ty.clone()).or_not())
                .then(expr.clone())
                .map_with(move |((mut ch, ret), body), e| {
                    if let Some(r) = ret {
                        ch.push(r);
                    }
                    ch.push(body);
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let zero_lam = just(Token::OrOr)
                .ignore_then(just(Token::Arrow).ignore_then(ty.clone()).or_not())
                .then(expr.clone())
                .map_with(move |(ret, body), e| {
                    let mut ch = Vec::new();
                    if let Some(r) = ret {
                        ch.push(r);
                    }
                    ch.push(body);
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let typed_port_item = choice((
                l_ident()
                    .ignore_then(just(Token::Colon))
                    .ignore_then(ty.clone())
                    .map(|t| vec![t]),
                ty.clone().map(|t| vec![t]),
            ));
            let typed_port_list = typed_port_item
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map(|v| v.into_iter().flatten().collect::<Vec<_>>());
            let port_item = just(Token::DotDot)
                .ignore_then(l_ident())
                .or(l_ident().then(just(Token::Question).or_not()).ignored());
            let port_list = port_item
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map(|_| vec![]);
            let ident_list = l_ident()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map(|_| vec![]);
            let expr_list = expr
                .clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen));
            let op_field = choice((
                just(Token::Ident("signature"))
                    .ignore_then(just(Token::Colon))
                    .ignore_then(typed_port_list.clone())
                    .then_ignore(just(Token::Arrow))
                    .then(choice((
                        typed_port_list.clone(),
                        ty.clone().map(|t| vec![t]),
                    )))
                    .map(|(mut a, b)| {
                        a.extend(b);
                        a
                    }),
                select! { Token::Ident("operands" | "results" | "regions" | "successors") => () }
                    .ignore_then(just(Token::Colon))
                    .ignore_then(choice((typed_port_list, port_list))),
                just(Token::Ident("attributes"))
                    .ignore_then(just(Token::Colon))
                    .ignore_then(choice((ident_list, ty.clone().map(|t| vec![t])))),
                just(Token::Ident("assembly"))
                    .ignore_then(just(Token::Colon))
                    .ignore_then(ty.clone().map(|t| vec![t])),
                just(Token::Ident("interfaces"))
                    .ignore_then(just(Token::Colon))
                    .ignore_then(choice((expr_list, expr.clone().map(|ex| vec![ex])))),
                l_ident()
                    .ignore_then(just(Token::Colon))
                    .ignore_then(expr.clone())
                    .map(|ex| vec![ex]),
            ));
            let op_sugar = just(Token::Op)
                .ignore_then(
                    op_field
                        .separated_by(just(Token::Comma))
                        .allow_trailing()
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::LBrace), just(Token::RBrace)),
                )
                .map_with(move |fs, e| {
                    let ch = fs.into_iter().flatten().collect();
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let pat: Boxed<'a, 'a, &'a [Token<'src>], PatNode, LExtra<'a, 'src>> = choice((
                just(Token::Wildcard).ignored(),
                select! {
                    Token::Natural(_) => (),
                    Token::Str(_) => (),
                    Token::True => (),
                    Token::False => (),
                },
                just(Token::LParen).then(just(Token::RParen)).ignored(),
                l_path()
                    .then(
                        l_ident()
                            .separated_by(just(Token::Comma))
                            .allow_trailing()
                            .delimited_by(just(Token::LParen), just(Token::RParen))
                            .or_not(),
                    )
                    .ignored(),
            ))
            .map_with(move |_, e| wrap(ctx, SyntaxKind::Pattern, e.span().into_range(), vec![]))
            .boxed();

            let match_expr = just(Token::Match)
                .ignore_then(expr.clone())
                .then(
                    pat.then_ignore(just(Token::FatArrow))
                        .then(expr.clone())
                        .separated_by(just(Token::Comma))
                        .allow_trailing()
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::LBrace), just(Token::RBrace)),
                )
                .map_with(move |(scrut, arms), e| {
                    let mut ch = vec![scrut];
                    for (p, r) in arms {
                        ch.push(p);
                        ch.push(r);
                    }
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let stmt_rule = just(Token::Let)
                .ignore_then(l_ident())
                .ignore_then(l_binder_names())
                .ignore_then(just(Token::Colon))
                .ignore_then(l_rule_spec(expr.clone()))
                .then_ignore(just(Token::Semi))
                .map_with(move |ch, e| wrap(ctx, SyntaxKind::Binding, e.span().into_range(), ch));

            let stmt_let = just(Token::Let)
                .ignore_then(l_ident())
                .ignore_then(just(Token::Colon).ignore_then(ty.clone()).or_not())
                .then_ignore(just(Token::Assign))
                .then(expr.clone())
                .then_ignore(just(Token::Semi))
                .map_with(move |(t, rhs), e| {
                    let mut ch = Vec::new();
                    if let Some(t) = t {
                        ch.push(t);
                    }
                    ch.push(rhs);
                    wrap(ctx, SyntaxKind::Binding, e.span().into_range(), ch)
                });

            let stmt_expr = expr.clone().then_ignore(just(Token::Semi));

            let block_body = choice((stmt_rule, stmt_let, stmt_expr))
                .repeated()
                .collect::<Vec<_>>()
                .then(expr.clone().or_not())
                .delimited_by(just(Token::LBrace), just(Token::RBrace))
                .map_with(move |(mut ch, fin), e| {
                    if let Some(f) = fin {
                        ch.push(f);
                    }
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let record_lit = l_strict_ident()
                .ignore_then(just(Token::Colon))
                .ignore_then(expr.clone())
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace))
                .map_with(move |ch, e| {
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let if_expr = just(Token::If)
                .ignore_then(expr.clone())
                .then(block_body.clone())
                .then_ignore(just(Token::Else))
                .then(expr.clone())
                .map_with(move |((c, t), el), e| {
                    wrap(
                        ctx,
                        SyntaxKind::Expression,
                        e.span().into_range(),
                        vec![c, t, el],
                    )
                });

            let paren_expr = expr
                .clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .map_with(move |ch, e| {
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let array_lit = expr
                .clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBracket), just(Token::RBracket))
                .map_with(move |ch, e| {
                    wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
                });

            let path_expr = l_path()
                .then(
                    just(Token::PathSep)
                        .ignore_then(
                            ty.clone()
                                .separated_by(just(Token::Comma))
                                .allow_trailing()
                                .collect::<Vec<_>>()
                                .delimited_by(just(Token::Less), just(Token::Greater)),
                        )
                        .or_not(),
                )
                .map_with(move |(_, targs), e| {
                    wrap(
                        ctx,
                        SyntaxKind::Expression,
                        e.span().into_range(),
                        targs.unwrap_or_default(),
                    )
                });

            let atom = choice((
                lit, memo_expr, op_sugar, rec_expr, lam_expr, zero_lam, match_expr, if_expr,
                record_lit, block_body, paren_expr, array_lit, path_expr,
            ));

            enum LPost {
                Call(Vec<ExprNode>),
                Unit,
            }

            let post_op = choice((
                expr.clone()
                    .separated_by(just(Token::Comma))
                    .allow_trailing()
                    .collect::<Vec<_>>()
                    .delimited_by(just(Token::LParen), just(Token::RParen))
                    .map(LPost::Call),
                just(Token::Dot)
                    .ignore_then(l_ident().or(select! { Token::Natural(_) => () }))
                    .map(|_| LPost::Unit),
                just(Token::Question).map(|_| LPost::Unit),
            ));

            let postfix = atom.foldl_with(post_op.repeated(), move |lhs, op, e| {
                let mut ch = vec![lhs];
                if let LPost::Call(args) = op {
                    ch.extend(args);
                }
                wrap(ctx, SyntaxKind::Expression, e.span().into_range(), ch)
            });

            let unary = just(Token::Bang)
                .or(just(Token::Minus))
                .repeated()
                .collect::<Vec<_>>()
                .then(postfix)
                .map_with(move |(ops, val), e| {
                    if ops.is_empty() {
                        val
                    } else {
                        wrap(
                            ctx,
                            SyntaxKind::Expression,
                            e.span().into_range(),
                            vec![val],
                        )
                    }
                })
                .boxed();

            let bin_op = select! {
                Token::Star | Token::Slash | Token::Percent
                | Token::Plus | Token::Minus | Token::PlusPlus
                | Token::EqEq | Token::NotEq | Token::Less | Token::LessEq | Token::Greater | Token::GreaterEq
                | Token::AndAnd | Token::OrOr => ()
            };

            unary
                .clone()
                .foldl_with(bin_op.ignore_then(unary).repeated(), move |lhs, rhs, e| {
                    wrap(
                        ctx,
                        SyntaxKind::BinaryExpression,
                        e.span().into_range(),
                        vec![lhs, rhs],
                    )
                })
        })
    }

    pub(crate) fn module<'a, 'src: 'a>(
        ctx: &'a Context<'a>,
    ) -> impl Parser<'a, &'a [Token<'src>], Vec<DeclNode>, LExtra<'a, 'src>> {
        let ty = l_type(ctx);
        let expr = l_expr(ctx, ty.clone());

        let decl = recursive(move |decl| {
            let is_pub = just(Token::Pub).or_not().ignored();

            let mod_decl = is_pub
                .ignore_then(just(Token::Mod))
                .ignore_then(l_ident())
                .ignore_then(choice((
                    decl.repeated()
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::LBrace), just(Token::RBrace)),
                    just(Token::Semi).map(|_| vec![]),
                )))
                .map_with(move |ch, e| {
                    wrap(ctx, SyntaxKind::Declaration, e.span().into_range(), ch)
                });

            let use_decl = is_pub
                .ignore_then(just(Token::Use))
                .ignore_then(l_path())
                .ignore_then(choice((
                    just(Token::PathSep)
                        .ignore_then(
                            l_ident()
                                .separated_by(just(Token::Comma))
                                .allow_trailing()
                                .delimited_by(just(Token::LBrace), just(Token::RBrace)),
                        )
                        .ignored(),
                    just(Token::PathSep)
                        .ignore_then(just(Token::Star))
                        .ignored(),
                    just(Token::As).ignore_then(l_ident()).ignored(),
                    empty(),
                )))
                .then_ignore(just(Token::Semi))
                .map_with(move |_, e| {
                    wrap(ctx, SyntaxKind::Declaration, e.span().into_range(), vec![])
                });

            let type_decl = is_pub
                .ignore_then(just(Token::Type))
                .ignore_then(l_ident())
                .ignore_then(l_ty_params())
                .ignore_then(l_sort())
                .then_ignore(just(Token::Semi))
                .map_with(move |_, e| {
                    wrap(ctx, SyntaxKind::Declaration, e.span().into_range(), vec![])
                });

            let ctor_tuple = ty
                .clone()
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LParen), just(Token::RParen));
            let ctor_rec = l_ident()
                .ignore_then(just(Token::Colon))
                .ignore_then(ty.clone())
                .separated_by(just(Token::Comma))
                .allow_trailing()
                .collect::<Vec<_>>()
                .delimited_by(just(Token::LBrace), just(Token::RBrace));
            let ctor = l_ident()
                .ignore_then(choice((ctor_tuple, ctor_rec, empty().map(|_| vec![]))))
                .then(just(Token::Colon).ignore_then(ty.clone()).or_not())
                .map(|(mut ch, ret)| {
                    if let Some(r) = ret {
                        ch.push(r);
                    }
                    ch
                });

            let enum_decl = is_pub
                .ignore_then(just(Token::Enum))
                .ignore_then(l_ident())
                .ignore_then(l_ty_params())
                .ignore_then(l_sort())
                .ignore_then(
                    ctor.separated_by(just(Token::Comma))
                        .allow_trailing()
                        .collect::<Vec<_>>()
                        .delimited_by(just(Token::LBrace), just(Token::RBrace)),
                )
                .map_with(move |cs, e| {
                    let ch = cs.into_iter().flatten().collect();
                    wrap(ctx, SyntaxKind::Declaration, e.span().into_range(), ch)
                });

            let rule_decl = is_pub
                .ignore_then(just(Token::Let))
                .ignore_then(l_ident())
                .ignore_then(l_binder_names())
                .ignore_then(just(Token::Colon))
                .ignore_then(l_rule_spec(expr.clone()))
                .then_ignore(just(Token::Semi))
                .map_with(move |ch, e| {
                    wrap(ctx, SyntaxKind::Declaration, e.span().into_range(), ch)
                });

            let let_decl = is_pub
                .ignore_then(just(Token::Let))
                .ignore_then(l_ident())
                .ignore_then(l_ty_params())
                .ignore_then(just(Token::Colon).ignore_then(ty.clone()).or_not())
                .then(just(Token::Assign).ignore_then(expr.clone()).or_not())
                .then_ignore(just(Token::Semi))
                .map_with(move |(t, body), e| {
                    let mut ch = Vec::new();
                    if let Some(t) = t {
                        ch.push(t);
                    }
                    if let Some(b) = body {
                        ch.push(b);
                    }
                    wrap(ctx, SyntaxKind::Declaration, e.span().into_range(), ch)
                });

            let extract_decl = just(Token::Extract)
                .ignore_then(select! { Token::Ident("rust" | "egglog" | "lean") => () })
                .ignore_then(l_path())
                .ignore_then(just(Token::FatArrow))
                .ignore_then(l_str().or(l_ident()))
                .ignore_then(
                    l_ident()
                        .ignore_then(just(Token::FatArrow))
                        .ignore_then(l_str())
                        .separated_by(just(Token::Comma))
                        .allow_trailing()
                        .delimited_by(just(Token::LBracket), just(Token::RBracket))
                        .or_not(),
                )
                .then_ignore(just(Token::Semi))
                .map_with(move |_, e| {
                    wrap(ctx, SyntaxKind::Declaration, e.span().into_range(), vec![])
                });

            choice((
                mod_decl,
                use_decl,
                type_decl,
                enum_decl,
                rule_decl,
                let_decl,
                extract_decl,
            ))
        });

        decl.repeated().collect::<Vec<_>>().then_ignore(end())
    }
}
