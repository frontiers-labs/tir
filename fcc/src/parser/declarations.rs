//! Declaration specifiers, declarators and translation-unit items.

use super::expressions::{constant_expr, initializer};
use super::statements::{close_scope, stmt};
use super::{Extra, ParseState, Span, ident};
use crate::ast::*;
use crate::lexer::Token;
use chumsky::input::{MapExtra, ValueInput};
use chumsky::inspector::SimpleState;
use chumsky::prelude::*;
use tir::graph::{Dag, MutDag, NodeId};

pub(super) fn ctype<'src, I>() -> impl Parser<'src, I, CType, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let qualifier = choice((
        just(Token::KwConst),
        just(Token::KwRestrict),
        just(Token::KwVolatile),
    ));
    let builtin_atom = select! {
        tok @ Token::KwInt => tok,
        tok @ Token::KwVoid => tok,
        tok @ Token::KwChar => tok,
        tok @ Token::KwLong => tok,
        tok @ Token::KwShort => tok,
        tok @ Token::KwSigned => tok,
        tok @ Token::KwUnsigned => tok,
        tok @ Token::KwBool => tok,
        tok @ Token::KwUnderscoreBool => tok,
        tok @ Token::KwFloat => tok,
        tok @ Token::KwDouble => tok,
    };
    let builtin = builtin_atom
        .repeated()
        .at_least(1)
        .collect::<Vec<_>>()
        .map(|atoms| builtin_type(&atoms));
    let named = select! { Token::Identifier(name) => name }.try_map_with(
        |name, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            if e.state().0.is_typedef(&name) {
                Ok(CType::Named(name))
            } else {
                Err(Rich::custom(e.span(), "expected type name"))
            }
        },
    );
    let record = choice((
        just(Token::KwStruct).to(RecordKind::Struct),
        just(Token::KwUnion).to(RecordKind::Union),
    ))
    .then(ident())
    .map_with(|(kind, name), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
        let id = e.state().0.record_id(kind, Some(&name), false);
        CType::Record(kind, id, Some(name))
    });
    let enumeration = just(Token::KwEnum)
        .ignore_then(ident())
        .map(|name| CType::Enum(Some(name)));
    let base = choice((builtin, record, enumeration, named));
    let pointer = just(Token::Star).ignore_then(qualifier.clone().repeated().collect::<Vec<_>>());

    qualifier
        .repeated()
        .collect::<Vec<_>>()
        .then(base)
        .then(pointer.repeated().collect::<Vec<_>>())
        .map(|((qualifiers, mut ty), pointers)| {
            ty = apply_qualifiers(ty, &qualifiers);
            for qualifiers in pointers {
                ty = CType::Pointer(Box::new(ty));
                ty = apply_qualifiers(ty, &qualifiers);
            }
            ty
        })
}

fn apply_qualifiers(mut ty: CType, qualifiers: &[Token]) -> CType {
    if qualifiers.iter().any(|token| token == &Token::KwConst) {
        ty = CType::Const(Box::new(ty));
    }
    if qualifiers.iter().any(|token| token == &Token::KwVolatile) {
        ty = CType::Volatile(Box::new(ty));
    }
    if qualifiers.iter().any(|token| token == &Token::KwRestrict) {
        ty = CType::Restrict(Box::new(ty));
    }
    ty
}

pub(super) fn function<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let array_suffix = just(Token::LBracket).then_ignore(just(Token::RBracket));
    let param = ctype()
        .then(ident().or_not())
        .then(array_suffix.repeated().collect::<Vec<_>>())
        .map_with(
            |((mut ty, name), array_suffixes), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
                let tok = e.span().start;
                let st = &mut e.state().0;
                for _ in array_suffixes {
                    ty = CType::Array(Box::new(ty), None);
                }
                let id = st.add(AstKind::Param, tok);
                st.ast.set_leaf_data(
                    id,
                    AstLeaf::Param {
                        name: name.unwrap_or_default(),
                        ty,
                    },
                );
                id
            },
        );
    let varargs =
        just(Token::Ellipsis).map_with(|_, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            e.state().0.add(AstKind::VarArgs, tok)
        });

    let params = choice((param, varargs))
        .separated_by(just(Token::Comma))
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LParen), just(Token::RParen));
    let storage = choice((
        just(Token::KwExtern).to(false),
        just(Token::KwStatic).to(true),
        just(Token::KwInline).to(false),
    ))
    .repeated()
    .collect::<Vec<bool>>()
    .map(|specifiers| specifiers.contains(&true));

    let header = storage.then(ctype().then(ident()).then(params));
    let definition_header = header.clone().then_ignore(just(Token::LBrace)).map_with(
        |header, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let st = &mut e.state().0;
            st.push_scope();
            for &param in &header.1.1 {
                if let Some(AstLeaf::Param { name, .. }) = st.ast.get_leaf_data(param)
                    && !name.is_empty()
                {
                    st.declare_ordinary(name.clone());
                }
            }
            header
        },
    );
    let body = stmt()
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(close_scope());
    let definition = definition_header.then(body).map_with(
        |((is_static, ((ret, name), params)), body), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Function, tok);
            let has_parameter_type_list = !params.is_empty();
            st.ast.set_leaf_data(
                id,
                AstLeaf::Function {
                    name,
                    ret,
                    has_parameter_type_list,
                    is_static,
                },
            );
            let params = params
                .into_iter()
                .filter(|&param| !is_void_param(&st.ast, param))
                .collect::<Vec<_>>();
            for child in params.into_iter().chain(body) {
                st.ast.add_edge(id, child);
            }
            id
        },
    );
    let prototype = header.then_ignore(just(Token::Semicolon)).map_with(
        |(is_static, ((ret, name), params)), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Prototype, tok);
            let has_parameter_type_list = !params.is_empty();
            st.ast.set_leaf_data(
                id,
                AstLeaf::Function {
                    name,
                    ret,
                    has_parameter_type_list,
                    is_static,
                },
            );
            let params = params
                .into_iter()
                .filter(|&param| !is_void_param(&st.ast, param))
                .collect::<Vec<_>>();
            for param in params {
                st.ast.add_edge(id, param);
            }
            id
        },
    );

    choice((definition, prototype))
}

fn is_void_param(ast: &Ast, param: NodeId) -> bool {
    matches!(
        ast.get_leaf_data(param),
        Some(AstLeaf::Param {
            name,
            ty: CType::Void,
        }) if name.is_empty()
    )
}

struct DeclParser<'a> {
    tokens: &'a [Token],
    pos: usize,
    attrs: Vec<String>,
}

struct DeclSpecs {
    ty: CType,
    storage: Vec<Token>,
    type_decl: Option<NodeId>,
}

struct DeclSpecPrefix {
    storage: Vec<Token>,
    qualifiers: Vec<Token>,
    attrs: Vec<String>,
    base: DeclSpecBase,
}

enum DeclSpecBase {
    Complete(CType, Option<NodeId>),
    Record(RecordKind),
}

struct RecordFrame {
    kind: RecordKind,
    id: RecordId,
    name: Option<String>,
    nested_records: Vec<NodeId>,
    fields: Vec<NodeId>,
    pending_field: Option<DeclSpecPrefix>,
}

struct Declarator {
    name: String,
    ty: CType,
}

impl<'a> DeclParser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            attrs: Vec::new(),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, tok: &Token) -> bool {
        if self.peek() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Token) -> Result<(), String> {
        self.eat(tok)
            .then_some(())
            .ok_or_else(|| format!("expected {tok}"))
    }

    fn is_done(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    fn take_initializer(&mut self) -> &'a [Token] {
        let start = self.pos;
        let mut depth = 0;
        while let Some(token) = self.tokens.get(self.pos) {
            match token {
                Token::LBrace | Token::LParen | Token::LBracket => depth += 1,
                Token::RBrace | Token::RParen | Token::RBracket => depth -= 1,
                Token::Comma if depth == 0 => break,
                _ => {}
            }
            self.pos += 1;
        }
        &self.tokens[start..self.pos]
    }

    fn take_enumerator_value(&mut self) -> &'a [Token] {
        let start = self.pos;
        let mut delimiter_depth = 0_i32;
        let mut conditional_depth = 0_u32;
        while let Some(token) = self.tokens.get(self.pos) {
            match token {
                Token::LParen | Token::LBracket => delimiter_depth += 1,
                Token::RParen | Token::RBracket => delimiter_depth -= 1,
                Token::Question if delimiter_depth == 0 => conditional_depth += 1,
                Token::Colon if delimiter_depth == 0 && conditional_depth != 0 => {
                    conditional_depth -= 1;
                }
                Token::Comma | Token::RBrace if delimiter_depth == 0 && conditional_depth == 0 => {
                    break;
                }
                _ => {}
            }
            self.pos += 1;
        }
        &self.tokens[start..self.pos]
    }

    fn parse_specs(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
    ) -> Result<DeclSpecs, String> {
        let prefix = self.parse_spec_prefix(state, tok)?;
        match prefix.base {
            DeclSpecBase::Complete(ref ty, type_decl) => {
                let ty = ty.clone();
                Ok(finish_decl_specs(prefix, ty, type_decl))
            }
            DeclSpecBase::Record(kind) => {
                let (ty, type_decl) = self.parse_record(state, tok, kind)?;
                Ok(finish_decl_specs(prefix, ty, type_decl))
            }
        }
    }

    fn parse_spec_prefix(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
    ) -> Result<DeclSpecPrefix, String> {
        let mut storage = Vec::new();
        let mut qualifiers = Vec::new();
        let mut spec_tokens = Vec::new();
        let mut attrs = std::mem::take(&mut self.attrs);

        let base = loop {
            match self.peek() {
                Some(Token::KwTypedef | Token::KwExtern | Token::KwStatic | Token::KwInline) => {
                    storage.push(self.next().unwrap());
                }
                Some(Token::KwConst | Token::KwVolatile | Token::KwRestrict) => {
                    qualifiers.push(self.next().unwrap());
                }
                Some(Token::KwStruct) => {
                    self.next();
                    break DeclSpecBase::Record(RecordKind::Struct);
                }
                Some(Token::KwUnion) => {
                    self.next();
                    break DeclSpecBase::Record(RecordKind::Union);
                }
                Some(Token::KwEnum) => {
                    self.next();
                    let name = match self.peek() {
                        Some(Token::Identifier(_)) => match self.next().unwrap() {
                            Token::Identifier(name) => Some(name),
                            _ => unreachable!(),
                        },
                        _ => None,
                    };
                    let type_decl = if self.eat(&Token::LBrace) {
                        let mut enumerators = Vec::new();
                        while !self.eat(&Token::RBrace) {
                            let enumerator_tok = tok + self.pos;
                            let enumerator_name = match self.next() {
                                Some(Token::Identifier(name)) => name,
                                Some(token) => {
                                    return Err(format!("expected enumerator name, found {token}"));
                                }
                                None => return Err("unterminated enum declaration".to_string()),
                            };
                            let value = if self.eat(&Token::Assign) {
                                let expression_offset = tok + self.pos;
                                let expression = self.take_enumerator_value();
                                Some(parse_external_constant_expression(
                                    state,
                                    expression_offset,
                                    expression,
                                )?)
                            } else {
                                None
                            };
                            state.0.declare_ordinary(enumerator_name.clone());
                            let enumerator = state.0.add(AstKind::Enumerator, enumerator_tok);
                            state.0.ast.set_leaf_data(
                                enumerator,
                                AstLeaf::Enumerator {
                                    name: enumerator_name,
                                },
                            );
                            if let Some(value) = value {
                                state.0.ast.add_edge(enumerator, value);
                            }
                            enumerators.push(enumerator);
                            if !self.eat(&Token::Comma) {
                                self.expect(&Token::RBrace)?;
                                break;
                            }
                        }
                        let declaration = state.0.add(AstKind::EnumDecl, tok);
                        state
                            .0
                            .ast
                            .set_leaf_data(declaration, AstLeaf::Enum { name: name.clone() });
                        for enumerator in enumerators {
                            state.0.ast.add_edge(declaration, enumerator);
                        }
                        Some(declaration)
                    } else {
                        None
                    };
                    break DeclSpecBase::Complete(CType::Enum(name), type_decl);
                }
                Some(
                    Token::KwVoid
                    | Token::KwBool
                    | Token::KwUnderscoreBool
                    | Token::KwChar
                    | Token::KwShort
                    | Token::KwInt
                    | Token::KwLong
                    | Token::KwSigned
                    | Token::KwUnsigned
                    | Token::KwFloat
                    | Token::KwDouble,
                ) => spec_tokens.push(self.next().unwrap()),
                Some(Token::Identifier(name)) if is_decl_attr_name(name) => {
                    let attr = self.parse_attr()?;
                    attrs.push(attr);
                }
                Some(Token::Identifier(_)) if spec_tokens.is_empty() => {
                    if let Token::Identifier(name) = self.next().unwrap() {
                        break DeclSpecBase::Complete(CType::Named(name), None);
                    }
                    unreachable!();
                }
                _ if !spec_tokens.is_empty() => {
                    break DeclSpecBase::Complete(builtin_type(&spec_tokens), None);
                }
                _ => return Err("expected declaration type".to_string()),
            }
        };

        Ok(DeclSpecPrefix {
            storage,
            qualifiers,
            attrs,
            base,
        })
    }

    fn parse_record(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
        kind: RecordKind,
    ) -> Result<(CType, Option<NodeId>), String> {
        let (ty, frame) = self.begin_record(state, kind);
        let Some(frame) = frame else {
            return Ok((ty, None));
        };
        let mut frames = vec![frame];
        loop {
            if self.eat(&Token::RBrace) {
                let (ty, record) = finish_record(state, tok, frames.pop().unwrap());
                let Some(parent) = frames.last_mut() else {
                    return Ok((ty, Some(record)));
                };
                parent.nested_records.push(record);
                let prefix = parent.pending_field.take().unwrap();
                let specs = finish_decl_specs(prefix, ty, Some(record));
                parent
                    .fields
                    .extend(self.parse_field_declarators(state, tok, specs.ty)?);
                continue;
            }
            if self.is_done() {
                return Err("unterminated record declaration".to_string());
            }

            let prefix = self.parse_spec_prefix(state, tok)?;
            match prefix.base {
                DeclSpecBase::Complete(ref ty, type_decl) => {
                    let ty = ty.clone();
                    let specs = finish_decl_specs(prefix, ty, type_decl);
                    frames
                        .last_mut()
                        .unwrap()
                        .fields
                        .extend(self.parse_field_declarators(state, tok, specs.ty)?);
                }
                DeclSpecBase::Record(kind) => {
                    let (ty, frame) = self.begin_record(state, kind);
                    if let Some(frame) = frame {
                        frames.last_mut().unwrap().pending_field = Some(prefix);
                        frames.push(frame);
                    } else {
                        let specs = finish_decl_specs(prefix, ty, None);
                        frames
                            .last_mut()
                            .unwrap()
                            .fields
                            .extend(self.parse_field_declarators(state, tok, specs.ty)?);
                    }
                }
            }
        }
    }

    fn begin_record(
        &mut self,
        state: &mut SimpleState<ParseState>,
        kind: RecordKind,
    ) -> (CType, Option<RecordFrame>) {
        let name = match self.peek() {
            Some(Token::Identifier(_)) => match self.next().unwrap() {
                Token::Identifier(name) => Some(name),
                _ => unreachable!(),
            },
            _ => None,
        };
        let defining = self.peek() == Some(&Token::LBrace);
        let record_id = state.0.record_id(kind, name.as_deref(), defining);
        let ty = CType::Record(kind, record_id, name.clone());
        let frame = self.eat(&Token::LBrace).then_some(RecordFrame {
            kind,
            id: record_id,
            name,
            nested_records: Vec::new(),
            fields: Vec::new(),
            pending_field: None,
        });
        (ty, frame)
    }

    fn parse_field_declarators(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
        ty: CType,
    ) -> Result<Vec<NodeId>, String> {
        let mut fields = Vec::new();
        loop {
            self.consume_attrs()?;
            let mut decl = self.parse_declarator(state, tok, ty.clone())?;
            self.consume_attrs()?;
            decl.ty = self.take_attrs(decl.ty);
            self.consume_bitfield()?;
            let id = state.0.add(AstKind::Field, tok);
            state.0.ast.set_leaf_data(
                id,
                AstLeaf::Field {
                    name: decl.name,
                    ty: decl.ty,
                },
            );
            fields.push(id);
            if self.eat(&Token::Comma) {
                continue;
            }
            self.expect(&Token::Semicolon)?;
            break;
        }
        Ok(fields)
    }

    fn parse_declarator(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
        mut base: CType,
    ) -> Result<Declarator, String> {
        while self.eat(&Token::Star) {
            let attrs = self.consume_pointer_attrs()?;
            base = CType::Pointer(Box::new(base));
            if !attrs.is_empty() {
                base = CType::Attributed(Box::new(base), attrs);
            }
        }

        let mut decl = if self.eat(&Token::LParen) {
            if self.eat(&Token::Star) {
                let attrs = self.consume_pointer_attrs()?;
                let name = self.parse_name()?;
                self.expect(&Token::RParen)?;
                let ty = if self.eat(&Token::LParen) {
                    let (params, varargs, has_parameter_type_list) =
                        self.parse_param_list(state, tok)?;
                    CType::Pointer(Box::new(CType::Function {
                        ret: Box::new(base),
                        params,
                        varargs,
                        has_parameter_type_list,
                    }))
                } else {
                    CType::Pointer(Box::new(base))
                };
                let ty = if attrs.is_empty() {
                    ty
                } else {
                    CType::Attributed(Box::new(ty), attrs)
                };
                Declarator { name, ty }
            } else {
                let decl = self.parse_declarator(state, tok, base)?;
                self.expect(&Token::RParen)?;
                decl
            }
        } else {
            Declarator {
                name: self.parse_name()?,
                ty: base,
            }
        };

        loop {
            if self.eat(&Token::LBracket) {
                let mut dimensions = Vec::new();
                loop {
                    let length_tok = tok + self.pos;
                    let len = self.collect_until_matching(Token::LBracket, Token::RBracket)?;
                    let length = if len.is_empty() {
                        None
                    } else {
                        let spelling = tokens_text(&len);
                        Some(parse_external_array_length(
                            state, length_tok, &len, spelling,
                        )?)
                    };
                    dimensions.push(length);
                    if !self.eat(&Token::LBracket) {
                        break;
                    }
                }
                for length in dimensions.into_iter().rev() {
                    decl.ty = CType::Array(Box::new(decl.ty), length);
                }
            } else if self.eat(&Token::LParen) {
                let (params, varargs, has_parameter_type_list) =
                    self.parse_param_list(state, tok)?;
                decl.ty = CType::Function {
                    ret: Box::new(decl.ty),
                    params,
                    varargs,
                    has_parameter_type_list,
                };
            } else {
                break;
            }
        }

        Ok(decl)
    }

    fn parse_name(&mut self) -> Result<String, String> {
        self.consume_attrs()?;
        match self.next() {
            Some(Token::Identifier(name)) => Ok(name),
            Some(tok) => Err(format!("expected declarator name, found {tok}")),
            None => Err("expected declarator name".to_string()),
        }
    }

    fn parse_param_list(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
    ) -> Result<(Vec<CParam>, bool, bool), String> {
        let mut params = Vec::new();
        let mut varargs = false;
        if self.eat(&Token::RParen) {
            return Ok((params, varargs, false));
        }
        loop {
            if self.eat(&Token::Ellipsis) {
                varargs = true;
            } else {
                let specs = self.parse_specs_for_param(state, tok)?;
                let param = if matches!(self.peek(), Some(Token::Comma | Token::RParen)) {
                    CParam {
                        name: String::new(),
                        ty: specs,
                    }
                } else {
                    let pos = self.pos;
                    let attrs = self.attrs.clone();
                    let decl = match self.parse_declarator(state, tok, specs.clone()) {
                        Ok(decl) => decl,
                        Err(_) => {
                            self.pos = pos;
                            self.attrs = attrs;
                            self.parse_abstract_declarator(state, tok, specs)?
                        }
                    };
                    CParam {
                        name: decl.name,
                        ty: decl.ty,
                    }
                };
                if !matches!(param.ty, CType::Void) || !param.name.is_empty() {
                    params.push(param);
                }
            }
            if self.eat(&Token::Comma) {
                continue;
            }
            self.expect(&Token::RParen)?;
            break;
        }
        Ok((params, varargs, true))
    }

    fn parse_abstract_declarator(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
        mut base: CType,
    ) -> Result<Declarator, String> {
        while self.eat(&Token::Star) {
            let attrs = self.consume_pointer_attrs()?;
            base = CType::Pointer(Box::new(base));
            if !attrs.is_empty() {
                base = CType::Attributed(Box::new(base), attrs);
            }
        }
        self.consume_attrs()?;

        while self.eat(&Token::LBracket) {
            let length_tok = tok + self.pos;
            let len = self.collect_until_matching(Token::LBracket, Token::RBracket)?;
            let length = if len.is_empty() {
                None
            } else {
                let spelling = tokens_text(&len);
                Some(parse_external_array_length(
                    state, length_tok, &len, spelling,
                )?)
            };
            base = CType::Array(Box::new(base), length);
        }

        if !matches!(self.peek(), Some(Token::Comma | Token::RParen)) {
            return Err("expected abstract declarator".to_string());
        }
        Ok(Declarator {
            name: String::new(),
            ty: base,
        })
    }

    fn parse_specs_for_param(
        &mut self,
        state: &mut SimpleState<ParseState>,
        tok: usize,
    ) -> Result<CType, String> {
        self.parse_specs(state, tok).map(|specs| specs.ty)
    }

    fn consume_bitfield(&mut self) -> Result<(), String> {
        if self.eat(&Token::Colon) {
            while !matches!(self.peek(), Some(Token::Comma | Token::Semicolon) | None) {
                self.next();
            }
        }
        Ok(())
    }

    fn consume_pointer_attrs(&mut self) -> Result<Vec<String>, String> {
        let mut attrs = Vec::new();
        loop {
            match self.peek() {
                Some(Token::KwConst | Token::KwVolatile | Token::KwRestrict) => {
                    attrs.push(self.next().unwrap().to_string());
                }
                Some(Token::Identifier(name)) if is_decl_attr_name(name) => {
                    attrs.push(self.parse_attr()?);
                }
                _ => break,
            }
        }
        Ok(attrs)
    }

    fn consume_attrs(&mut self) -> Result<(), String> {
        while matches!(self.peek(), Some(Token::Identifier(name)) if is_decl_attr_name(name)) {
            let attr = self.parse_attr()?;
            self.attrs.push(attr);
        }
        Ok(())
    }

    fn take_attrs(&mut self, ty: CType) -> CType {
        if self.attrs.is_empty() {
            ty
        } else {
            CType::Attributed(Box::new(ty), std::mem::take(&mut self.attrs))
        }
    }

    fn parse_attr(&mut self) -> Result<String, String> {
        let name = match self.next() {
            Some(Token::Identifier(name)) => name,
            _ => unreachable!(),
        };
        let name = match name.as_str() {
            "__restrict" | "__restrict__" => "restrict".to_string(),
            _ => name,
        };
        if self.eat(&Token::LParen) {
            let args = self.collect_until_matching(Token::LParen, Token::RParen)?;
            Ok(format!("{name}({})", tokens_text(&args)))
        } else {
            Ok(name)
        }
    }

    fn collect_until_matching(&mut self, open: Token, close: Token) -> Result<Vec<Token>, String> {
        let mut depth = 1usize;
        let mut out = Vec::new();
        while let Some(tok) = self.next() {
            if tok == open {
                depth += 1;
                out.push(tok);
            } else if tok == close {
                depth -= 1;
                if depth == 0 {
                    return Ok(out);
                }
                out.push(tok);
            } else {
                out.push(tok);
            }
        }
        Err(format!("expected {close}"))
    }
}

fn finish_decl_specs(
    prefix: DeclSpecPrefix,
    mut ty: CType,
    type_decl: Option<NodeId>,
) -> DeclSpecs {
    for qualifier in prefix.qualifiers.into_iter().rev() {
        ty = match qualifier {
            Token::KwConst => CType::Const(Box::new(ty)),
            Token::KwVolatile => CType::Volatile(Box::new(ty)),
            Token::KwRestrict => CType::Restrict(Box::new(ty)),
            _ => ty,
        };
    }
    if !prefix.attrs.is_empty() {
        ty = CType::Attributed(Box::new(ty), prefix.attrs);
    }
    DeclSpecs {
        ty,
        storage: prefix.storage,
        type_decl,
    }
}

fn finish_record(
    state: &mut SimpleState<ParseState>,
    tok: usize,
    frame: RecordFrame,
) -> (CType, NodeId) {
    let record = state.0.add(AstKind::RecordDecl, tok);
    state.0.ast.set_leaf_data(
        record,
        AstLeaf::Record {
            id: frame.id,
            kind: frame.kind,
            name: frame.name.clone(),
        },
    );
    // Nested definitions come before the fields: sema lays a record out in child
    // order, and a field of a nested type needs that type's size already known.
    for nested in frame.nested_records {
        state.0.ast.add_edge(record, nested);
    }
    for field in frame.fields {
        state.0.ast.add_edge(record, field);
    }
    (CType::Record(frame.kind, frame.id, frame.name), record)
}

fn builtin_type(tokens: &[Token]) -> CType {
    SpecCounts::of(tokens)
        .ctype()
        .unwrap_or_else(|| CType::Invalid(tokens_text(tokens)))
}

/// How often each builtin type specifier keyword occurs in a declaration's specifier list.
#[derive(Default)]
struct SpecCounts {
    void: usize,
    boolean: usize,
    float: usize,
    double: usize,
    char_: usize,
    short: usize,
    long: usize,
    int: usize,
    signed: usize,
    unsigned: usize,
    other: usize,
}

impl SpecCounts {
    fn of(tokens: &[Token]) -> Self {
        let mut counts = SpecCounts::default();
        for token in tokens {
            let slot = match token {
                Token::KwVoid => &mut counts.void,
                Token::KwBool | Token::KwUnderscoreBool => &mut counts.boolean,
                Token::KwFloat => &mut counts.float,
                Token::KwDouble => &mut counts.double,
                Token::KwChar => &mut counts.char_,
                Token::KwShort => &mut counts.short,
                Token::KwLong => &mut counts.long,
                Token::KwInt => &mut counts.int,
                Token::KwSigned => &mut counts.signed,
                Token::KwUnsigned => &mut counts.unsigned,
                _ => &mut counts.other,
            };
            *slot += 1;
        }
        counts
    }

    /// The type these specifiers name, or `None` for a combination C does not allow
    /// (`long short`, `unsigned float`, a repeated keyword, ...).
    fn ctype(&self) -> Option<CType> {
        if self.other > 0 || self.int > 1 {
            return None;
        }
        let sign = (self.signed, self.unsigned);
        Some(
            match (
                self.void,
                self.boolean,
                self.float,
                self.double,
                self.char_,
                self.short,
                self.long,
                self.int,
                sign,
            ) {
                (1, 0, 0, 0, 0, 0, 0, 0, (0, 0)) => CType::Void,
                (0, 1, 0, 0, 0, 0, 0, 0, (0, 0)) => CType::Bool,
                (0, 0, 1, 0, 0, 0, 0, 0, (0, 0)) => CType::Float,
                (0, 0, 0, 1, 0, 0, 0, 0, (0, 0)) => CType::Double,
                (0, 0, 0, 1, 0, 0, 1, 0, (0, 0)) => CType::LongDouble,
                (0, 0, 0, 0, 1, 0, 0, 0, (0, 0)) => CType::Char,
                (0, 0, 0, 0, 1, 0, 0, 0, (1, 0)) => CType::SignedChar,
                (0, 0, 0, 0, 1, 0, 0, 0, (0, 1)) => CType::UnsignedChar,
                (0, 0, 0, 0, 0, 1, 0, _, (0, 0) | (1, 0)) => CType::Short,
                (0, 0, 0, 0, 0, 1, 0, _, (0, 1)) => CType::UnsignedShort,
                (0, 0, 0, 0, 0, 0, 2, _, (0, 0) | (1, 0)) => CType::LongLong,
                (0, 0, 0, 0, 0, 0, 2, _, (0, 1)) => CType::UnsignedLongLong,
                (0, 0, 0, 0, 0, 0, 1, _, (0, 0) | (1, 0)) => CType::Long,
                (0, 0, 0, 0, 0, 0, 1, _, (0, 1)) => CType::UnsignedLong,
                (0, 0, 0, 0, 0, 0, 0, _, (0, 1)) => CType::UnsignedInt,
                (0, 0, 0, 0, 0, 0, 0, 1, (0, 0)) | (0, 0, 0, 0, 0, 0, 0, _, (1, 0)) => CType::Int,
                _ => return None,
            },
        )
    }
}

fn is_decl_attr_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(
        name,
        "_Nullable"
            | "_Nonnull"
            | "_Null_unspecified"
            | "__restrict"
            | "__restrict__"
            | "__THROW"
            | "__THROWNL"
            | "__wur"
            | "__nonnull"
            | "__attribute_malloc__"
            | "__attr_dealloc"
            | "__COLD"
            | "__fortified_attr_access"
            | "__attribute__"
            | "__asm"
            | "__asm__"
            | "__swift_nonisolated_unsafe"
            | "__swift_unavailable"
            | "__ptr"
    ) || name.starts_with("__")
        && (lower.contains("like")
            || lower.contains("alias")
            || lower.contains("availability")
            || lower.contains("deprecated"))
        || name.starts_with("_LIBC_")
}

fn tokens_text(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

fn add_param_node(st: &mut ParseState, tok: usize, mut param: CParam) -> NodeId {
    let id = st.add(AstKind::Param, tok);
    param.name.clear();
    st.ast.set_leaf_data(
        id,
        AstLeaf::Param {
            name: param.name,
            ty: param.ty,
        },
    );
    id
}

fn add_varargs_node(st: &mut ParseState, tok: usize) -> NodeId {
    st.add(AstKind::VarArgs, tok)
}

fn split_function_type(ty: CType) -> Result<(CType, Vec<CParam>, bool, bool), CType> {
    match ty {
        CType::Function {
            ret,
            params,
            varargs,
            has_parameter_type_list,
        } => Ok((*ret, params, varargs, has_parameter_type_list)),
        CType::Attributed(inner, attrs) => match *inner {
            CType::Function {
                ret,
                params,
                varargs,
                has_parameter_type_list,
            } => Ok((
                CType::Attributed(ret, attrs),
                params,
                varargs,
                has_parameter_type_list,
            )),
            other => Err(CType::Attributed(Box::new(other), attrs)),
        },
        other => Err(other),
    }
}

pub(super) fn top_level_attr<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    let group = recursive(|group| {
        let atom = any()
            .filter(|tok: &Token| !matches!(tok, Token::LParen | Token::RParen))
            .map(|tok| vec![tok]);
        choice((
            group
                .repeated()
                .collect::<Vec<Vec<Token>>>()
                .map(|parts| {
                    let mut toks = vec![Token::LParen];
                    toks.extend(parts.into_iter().flatten());
                    toks.push(Token::RParen);
                    toks
                })
                .delimited_by(just(Token::LParen), just(Token::RParen)),
            atom,
        ))
    });

    select! { Token::Identifier(name) => name }
        .try_map(|name, span| {
            is_decl_attr_name(&name)
                .then_some(name)
                .ok_or_else(|| Rich::custom(span, "expected top-level attribute"))
        })
        .then(
            group
                .repeated()
                .collect::<Vec<Vec<Token>>>()
                .delimited_by(just(Token::LParen), just(Token::RParen)),
        )
        .map_with(|(name, args), e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let st = &mut e.state().0;
            let id = st.add(AstKind::Attribute, tok);
            let args = args.into_iter().flatten().collect::<Vec<_>>();
            st.ast.set_leaf_data(
                id,
                AstLeaf::Attribute(format!("{name}({})", tokens_text(&args))),
            );
            id
        })
}

pub(super) fn top_level_marker<'src, I>() -> impl Parser<'src, I, (), Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    select! { Token::Identifier(name) => name }
        .try_map(|name, span| {
            is_top_level_marker(&name)
                .then_some(())
                .ok_or_else(|| Rich::custom(span, "expected top-level marker"))
        })
        .then_ignore(just(Token::Semicolon).or_not())
}

fn parse_external_tokens(
    state: &mut SimpleState<ParseState>,
    tok: usize,
    tokens: &[Token],
) -> Result<NodeId, String> {
    let mut parser = DeclParser::new(tokens);
    let specs = parser.parse_specs(state, tok)?;
    let is_typedef = specs
        .storage
        .iter()
        .any(|tok| matches!(tok, Token::KwTypedef));
    let is_extern = specs
        .storage
        .iter()
        .any(|tok| matches!(tok, Token::KwExtern));
    let is_static = specs
        .storage
        .iter()
        .any(|tok| matches!(tok, Token::KwStatic));
    let mut nodes = Vec::new();
    if let Some(type_decl) = specs.type_decl {
        nodes.push(type_decl);
    }

    if parser.is_done() {
        if nodes.len() == 1 {
            return Ok(nodes[0]);
        }
        if let CType::Record(kind, record_id, name) = specs.ty {
            let id = state.0.add(AstKind::RecordDecl, tok);
            state.0.ast.set_leaf_data(
                id,
                AstLeaf::Record {
                    id: record_id,
                    kind,
                    name,
                },
            );
            return Ok(id);
        }
        return Err("record declaration has no declarator".to_string());
    }

    loop {
        parser.consume_attrs()?;
        let mut decl = parser.parse_declarator(state, tok, specs.ty.clone())?;
        parser.consume_attrs()?;
        decl.ty = parser.take_attrs(decl.ty);
        let ty = decl.ty;
        if !is_typedef
            && let Ok((ret, params, varargs, has_parameter_type_list)) =
                split_function_type(ty.clone())
        {
            let params = params
                .into_iter()
                .map(|param| add_param_node(&mut state.0, tok, param))
                .collect::<Vec<_>>();
            let varargs = varargs.then(|| add_varargs_node(&mut state.0, tok));
            let id = state.0.add(AstKind::Prototype, tok);
            state.0.ast.set_leaf_data(
                id,
                AstLeaf::Function {
                    name: decl.name,
                    ret,
                    has_parameter_type_list,
                    is_static,
                },
            );
            for param in params {
                state.0.ast.add_edge(id, param);
            }
            if let Some(varargs) = varargs {
                state.0.ast.add_edge(id, varargs);
            }
            nodes.push(id);
        } else {
            match ty {
                ty if is_typedef => {
                    state.0.declare_typedef(decl.name.clone());
                    let id = state.0.add(AstKind::Typedef, tok);
                    state.0.ast.set_leaf_data(
                        id,
                        AstLeaf::Typedef {
                            name: decl.name,
                            ty,
                        },
                    );
                    nodes.push(id);
                }
                ty => {
                    state.0.declare_ordinary(decl.name.clone());
                    let initializer = if parser.eat(&Token::Assign) {
                        let initializer_tok = tok + parser.pos;
                        let initializer_tokens = parser.take_initializer();
                        Some(parse_external_initializer(
                            state,
                            initializer_tok,
                            initializer_tokens,
                        )?)
                    } else {
                        None
                    };
                    let id = state.0.add(AstKind::Global, tok);
                    state.0.ast.set_leaf_data(
                        id,
                        AstLeaf::Global {
                            name: decl.name,
                            ty,
                            is_extern,
                            is_static,
                        },
                    );
                    if let Some(initializer) = initializer {
                        state.0.ast.add_edge(id, initializer);
                    }
                    nodes.push(id);
                }
            }
        }

        if parser.eat(&Token::Comma) {
            continue;
        }
        if parser.is_done() {
            break;
        }
        return Err(format!(
            "unexpected token in declaration: {}",
            parser.peek().unwrap()
        ));
    }

    if nodes.len() == 1 {
        Ok(nodes[0])
    } else {
        let group = state.0.add(AstKind::DeclGroup, tok);
        for node in nodes {
            state.0.ast.add_edge(group, node);
        }
        Ok(group)
    }
}

fn parse_local_decl_tokens(
    state: &mut SimpleState<ParseState>,
    tok: usize,
    tokens: &[Token],
) -> Result<NodeId, String> {
    let mut parser = DeclParser::new(tokens);
    let specs = parser.parse_specs(state, tok)?;
    let mut nodes = specs.type_decl.into_iter().collect::<Vec<_>>();

    if parser.is_done() {
        return nodes
            .pop()
            .ok_or_else(|| "enum declaration has no declarator".to_string());
    }

    loop {
        parser.consume_attrs()?;
        let mut declarator = parser.parse_declarator(state, tok, specs.ty.clone())?;
        parser.consume_attrs()?;
        declarator.ty = parser.take_attrs(declarator.ty);
        state.0.declare_ordinary(declarator.name.clone());
        let initializer = if parser.eat(&Token::Assign) {
            let initializer_tok = tok + parser.pos;
            let initializer_tokens = parser.take_initializer();
            Some(parse_external_initializer(
                state,
                initializer_tok,
                initializer_tokens,
            )?)
        } else {
            None
        };
        let declaration = state.0.add(AstKind::Decl, tok);
        state.0.ast.set_leaf_data(
            declaration,
            AstLeaf::Decl {
                name: declarator.name,
                ty: declarator.ty,
            },
        );
        if let Some(initializer) = initializer {
            state.0.ast.add_edge(declaration, initializer);
        }
        nodes.push(declaration);

        if parser.eat(&Token::Comma) {
            continue;
        }
        if parser.is_done() {
            break;
        }
        return Err(format!(
            "unexpected token in declaration: {}",
            parser.peek().unwrap()
        ));
    }

    if nodes.len() == 1 {
        Ok(nodes[0])
    } else {
        let group = state.0.add(AstKind::DeclGroup, tok);
        for node in nodes {
            state.0.ast.add_edge(group, node);
        }
        Ok(group)
    }
}

fn parse_external_initializer(
    state: &mut SimpleState<ParseState>,
    token_offset: usize,
    tokens: &[Token],
) -> Result<NodeId, String> {
    if tokens.is_empty() {
        return Err("expected initializer".to_string());
    }
    let previous_offset = state.0.token_offset;
    state.0.token_offset = token_offset;
    let (initializer, errors) = initializer()
        .then_ignore(end())
        .parse_with_state(tokens, state)
        .into_output_errors();
    state.0.token_offset = previous_offset;
    if let Some(error) = errors.first() {
        return Err(error.to_string());
    }
    initializer.ok_or_else(|| "expected initializer".to_string())
}

fn parse_external_constant_expression(
    state: &mut SimpleState<ParseState>,
    token_offset: usize,
    tokens: &[Token],
) -> Result<NodeId, String> {
    if tokens.is_empty() {
        return Err("expected enumerator value".to_string());
    }
    let previous_offset = state.0.token_offset;
    state.0.token_offset = token_offset;
    let (expression, errors) = constant_expr()
        .then_ignore(end())
        .parse_with_state(tokens, state)
        .into_output_errors();
    state.0.token_offset = previous_offset;
    if let Some(error) = errors.first() {
        return Err(error.to_string());
    }
    expression.ok_or_else(|| "expected enumerator value".to_string())
}

fn parse_external_array_length(
    state: &mut SimpleState<ParseState>,
    token_offset: usize,
    tokens: &[Token],
    spelling: String,
) -> Result<ArrayLength, String> {
    let mut scratch = SimpleState(ParseState {
        ast: Ast::new(),
        spans: Vec::new(),
        token_offset: 0,
        name_scopes: state.0.name_scopes.clone(),
        next_record: state.0.next_record,
    });
    let (expression, errors) = constant_expr()
        .then_ignore(end())
        .parse_with_state(tokens, &mut scratch)
        .into_output_errors();
    if expression.is_none() || !errors.is_empty() {
        return Ok(ArrayLength::new(spelling, None));
    }
    parse_external_constant_expression(state, token_offset, tokens)
        .map(|expression| ArrayLength::new(spelling, Some(expression)))
}

fn declaration_tokens<'src, I>() -> impl Parser<'src, I, Vec<Token>, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    custom(|input| {
        let mut tokens = Vec::new();
        let mut closing_delimiters = Vec::new();
        loop {
            let before = input.cursor();
            let Some(token) = input.next() else {
                return Err(Rich::custom(
                    input.span_since(&before),
                    "unterminated declaration",
                ));
            };
            match &token {
                Token::LBrace => closing_delimiters.push(Token::RBrace),
                Token::LParen => closing_delimiters.push(Token::RParen),
                Token::LBracket => closing_delimiters.push(Token::RBracket),
                Token::RBrace | Token::RParen | Token::RBracket => {
                    let Some(expected) = closing_delimiters.pop() else {
                        return Err(Rich::custom(
                            input.span_since(&before),
                            format!("unexpected closing delimiter {token}"),
                        ));
                    };
                    if token != expected {
                        return Err(Rich::custom(
                            input.span_since(&before),
                            format!("expected {expected}, found {token}"),
                        ));
                    }
                }
                Token::Semicolon if closing_delimiters.is_empty() => return Ok(tokens),
                _ => {}
            }
            tokens.push(token);
        }
    })
}

pub(super) fn local_decl<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    ctype()
        .rewind()
        .ignore_then(declaration_tokens())
        .try_map_with(|tokens, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
            let tok = e.span().start;
            let span = e.span();
            parse_local_decl_tokens(e.state(), tok, &tokens)
                .map_err(|message| Rich::custom(span, message))
        })
}

pub(super) fn external_decl<'src, I>() -> impl Parser<'src, I, NodeId, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token, Span = Span>,
{
    declaration_tokens().try_map_with(|tokens, e: &mut MapExtra<'src, '_, I, Extra<'src>>| {
        let tok = e.span().start;
        let span = e.span();
        parse_external_tokens(e.state(), tok, &tokens).map_err(|msg| Rich::custom(span, msg))
    })
}

fn is_top_level_marker(name: &str) -> bool {
    matches!(name, "__BEGIN_DECLS" | "__END_DECLS")
}
