use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use logos::{Lexer, Logos};

use crate::diagnostics::{
    Diagnostic, FileId, MissingInclude, PreprocError, PreprocWarning, Span, file_source,
    intern_file,
};
use crate::lexer::Token;

/// Directories searched for `#include` files. `user` holds `-I` directories in
/// command-line order; `system` holds the toolchain's default directories.
/// Quoted includes search the including file's directory, then `user`, then
/// `system`; angle includes search `user` then `system`.
#[derive(Debug, Clone, Default)]
pub struct IncludePaths {
    pub user: Vec<PathBuf>,
    pub system: Vec<PathBuf>,
}

// ---------------------------------------------------------------------------
// Preprocessor-directive token type
// ---------------------------------------------------------------------------

#[derive(Logos, Debug, PartialEq)]
#[logos(skip r"[ \t\r]+")]
enum PreprocToken {
    // Directives (must come before Identifier so they win on longest-match).
    #[token("define")]
    Define,
    #[token("undef")]
    Undef,
    #[token("include")]
    Include,
    #[token("elifdef")]
    Elifdef,
    #[token("elifndef")]
    Elifndef,
    #[token("ifdef")]
    Ifdef,
    #[token("ifndef")]
    Ifndef,
    #[token("elif")]
    Elif,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("endif")]
    Endif,
    #[token("line")]
    Line,
    #[token("error")]
    Error,
    #[token("warning")]
    Warning,
    #[token("pragma")]
    Pragma,
    #[token("embed")]
    Embed,

    // `defined` is both a directive keyword and usable in #if expressions.
    #[token("defined")]
    Defined,

    // General identifier (after all keywords so keywords take priority).
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", |lex| lex.slice().to_string())]
    Identifier(String),

    // Paths for #include.
    #[regex(r#""[^"]*""#)]
    QuotedPath,
    #[regex(r"<[^>]*>")]
    AnglePath,

    // Integer literals for #if expression evaluation.
    #[regex(r"0[xX][0-9a-fA-F][0-9a-fA-F_]*|[0-9][0-9_]*", |lex| {
        let s = lex.slice().replace('_', "");
        if s.starts_with("0x") || s.starts_with("0X") {
            i64::from_str_radix(&s[2..], 16).ok()
        } else {
            s.parse::<i64>().ok()
        }
    })]
    Integer(i64),

    // Operators (longer tokens before shorter ones to ensure correct greedy match).
    #[token("##")]
    HashHash,
    #[token("&&")]
    And,
    #[token("||")]
    Or,
    #[token("==")]
    Eq,
    #[token("!=")]
    Ne,
    #[token("<=")]
    Le,
    #[token(">=")]
    Ge,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,
    #[token("<")]
    Lt,
    #[token(">")]
    Gt,
    #[token("!")]
    Bang,
    #[token("~")]
    Tilde,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("&")]
    BitAnd,
    #[token("|")]
    BitOr,
    #[token("^")]
    BitXor,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("?")]
    Question,
    #[token(":")]
    Colon,
}

// ---------------------------------------------------------------------------
// #if / #elif expression evaluator
// ---------------------------------------------------------------------------

/// Evaluate a C preprocessor constant expression; returns the integer value.
/// Undefined identifiers and non-integer macros evaluate to 0.
fn eval_if_expr(expr: &str, defines: &HashMap<String, MacroDefinition>, span: Span) -> i64 {
    let expanded = expand_if_expr(expr, defines, span);
    let toks: Vec<PreprocToken> = PreprocToken::lexer(&expanded)
        .filter_map(Result::ok)
        .collect();
    IfExpr {
        toks: &toks,
        pos: 0,
        defines,
    }
    .eval()
}

struct IfExpr<'a> {
    toks: &'a [PreprocToken],
    pos: usize,
    defines: &'a HashMap<String, MacroDefinition>,
}

impl<'a> IfExpr<'a> {
    // Returning `&'a` (lifetime of toks slice) rather than `&'_ self` lets us
    // borrow `self.defines` in the same expression without a conflict.
    fn peek(&self) -> Option<&'a PreprocToken> {
        self.toks.get(self.pos)
    }

    fn bump(&mut self) -> Option<&'a PreprocToken> {
        let t = self.toks.get(self.pos)?;
        self.pos += 1;
        Some(t)
    }

    fn eval(&mut self) -> i64 {
        self.ternary()
    }

    fn ternary(&mut self) -> i64 {
        let val = self.or();
        if matches!(self.peek(), Some(PreprocToken::Question)) {
            self.bump();
            let then = self.ternary();
            if matches!(self.peek(), Some(PreprocToken::Colon)) {
                self.bump();
            }
            let else_ = self.ternary();
            if val != 0 { then } else { else_ }
        } else {
            val
        }
    }

    fn or(&mut self) -> i64 {
        let mut val = self.and();
        while matches!(self.peek(), Some(PreprocToken::Or)) {
            self.bump();
            let rhs = self.and();
            val = ((val != 0) || (rhs != 0)) as i64;
        }
        val
    }

    fn and(&mut self) -> i64 {
        let mut val = self.bit_or();
        while matches!(self.peek(), Some(PreprocToken::And)) {
            self.bump();
            let rhs = self.bit_or();
            val = ((val != 0) && (rhs != 0)) as i64;
        }
        val
    }

    fn bit_or(&mut self) -> i64 {
        let mut val = self.bit_xor();
        while matches!(self.peek(), Some(PreprocToken::BitOr)) {
            self.bump();
            val |= self.bit_xor();
        }
        val
    }

    fn bit_xor(&mut self) -> i64 {
        let mut val = self.bit_and();
        while matches!(self.peek(), Some(PreprocToken::BitXor)) {
            self.bump();
            val ^= self.bit_and();
        }
        val
    }

    fn bit_and(&mut self) -> i64 {
        let mut val = self.equality();
        while matches!(self.peek(), Some(PreprocToken::BitAnd)) {
            self.bump();
            val &= self.equality();
        }
        val
    }

    fn equality(&mut self) -> i64 {
        let mut val = self.comparison();
        loop {
            match self.peek() {
                Some(PreprocToken::Eq) => {
                    self.bump();
                    val = (val == self.comparison()) as i64;
                }
                Some(PreprocToken::Ne) => {
                    self.bump();
                    val = (val != self.comparison()) as i64;
                }
                _ => break,
            }
        }
        val
    }

    fn comparison(&mut self) -> i64 {
        let mut val = self.shift();
        loop {
            match self.peek() {
                Some(PreprocToken::Lt) => {
                    self.bump();
                    val = (val < self.shift()) as i64;
                }
                Some(PreprocToken::Le) => {
                    self.bump();
                    val = (val <= self.shift()) as i64;
                }
                Some(PreprocToken::Gt) => {
                    self.bump();
                    val = (val > self.shift()) as i64;
                }
                Some(PreprocToken::Ge) => {
                    self.bump();
                    val = (val >= self.shift()) as i64;
                }
                _ => break,
            }
        }
        val
    }

    fn shift(&mut self) -> i64 {
        let mut val = self.additive();
        loop {
            match self.peek() {
                Some(PreprocToken::Shl) => {
                    self.bump();
                    val <<= self.additive();
                }
                Some(PreprocToken::Shr) => {
                    self.bump();
                    val >>= self.additive();
                }
                _ => break,
            }
        }
        val
    }

    fn additive(&mut self) -> i64 {
        let mut val = self.multiplicative();
        loop {
            match self.peek() {
                Some(PreprocToken::Plus) => {
                    self.bump();
                    val += self.multiplicative();
                }
                Some(PreprocToken::Minus) => {
                    self.bump();
                    val -= self.multiplicative();
                }
                _ => break,
            }
        }
        val
    }

    fn multiplicative(&mut self) -> i64 {
        let mut val = self.unary();
        loop {
            match self.peek() {
                Some(PreprocToken::Star) => {
                    self.bump();
                    val *= self.unary();
                }
                Some(PreprocToken::Slash) => {
                    self.bump();
                    let r = self.unary();
                    val = if r != 0 { val / r } else { 0 };
                }
                Some(PreprocToken::Percent) => {
                    self.bump();
                    let r = self.unary();
                    val = if r != 0 { val % r } else { 0 };
                }
                _ => break,
            }
        }
        val
    }

    fn unary(&mut self) -> i64 {
        match self.peek() {
            Some(PreprocToken::Bang) => {
                self.bump();
                (self.unary() == 0) as i64
            }
            Some(PreprocToken::Tilde) => {
                self.bump();
                !self.unary()
            }
            Some(PreprocToken::Minus) => {
                self.bump();
                -self.unary()
            }
            Some(PreprocToken::Plus) => {
                self.bump();
                self.unary()
            }
            Some(PreprocToken::Defined) => {
                self.bump();
                let paren = matches!(self.peek(), Some(PreprocToken::LParen));
                if paren {
                    self.bump();
                }
                let is_def = match self.peek() {
                    Some(PreprocToken::Identifier(name)) => {
                        let result = self.defines.contains_key(name.as_str()) as i64;
                        self.pos += 1;
                        result
                    }
                    _ => 0,
                };
                if paren && matches!(self.peek(), Some(PreprocToken::RParen)) {
                    self.bump();
                }
                is_def
            }
            _ => self.primary(),
        }
    }

    fn primary(&mut self) -> i64 {
        match self.peek() {
            Some(PreprocToken::Integer(_)) => {
                if let Some(PreprocToken::Integer(n)) = self.bump() {
                    *n
                } else {
                    0
                }
            }
            Some(PreprocToken::Identifier(_)) => {
                if let Some(PreprocToken::Identifier(name)) = self.bump() {
                    match self.defines.get(name.as_str()) {
                        Some(MacroDefinition::Object(body))
                            if matches!(body.as_slice(), [Token::IntegerLiteral(_)]) =>
                        {
                            let Token::IntegerLiteral(n) = &body[0] else {
                                unreachable!()
                            };
                            n.value.to_i64()
                        }
                        _ => 0, // undefined or non-integer macro
                    }
                } else {
                    0
                }
            }
            Some(PreprocToken::LParen) => {
                self.bump();
                let val = self.eval();
                if matches!(self.peek(), Some(PreprocToken::RParen)) {
                    self.bump();
                }
                val
            }
            _ => {
                self.bump();
                0
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Conditional-compilation state machine
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum CondState {
    /// An outer level is already skipping — ignore everything at this level.
    OuterSkip,
    /// This branch is active: emit tokens.
    Active,
    /// No active branch seen yet at this level: skip tokens.
    Inactive,
    /// An active branch was already emitted: skip all remaining branches.
    Done,
}

fn is_skipping(stack: &[CondState]) -> bool {
    stack.iter().any(|s| !matches!(s, CondState::Active))
}

// ---------------------------------------------------------------------------
// TokenStream
// ---------------------------------------------------------------------------

/// Lazy preprocessed token stream.
///
/// Internally keeps a stack of `(source, byte_offset)` frames.  The top frame
/// is the one currently being lexed.  New frames are pushed for `#include`
/// files so that processing is interleaved rather than collected up front.
///
/// Because `logos::Lexer<'s, T>` borrows `&'s str`, storing one in a struct
/// causes a self-referential lifetime problem.  We sidestep this by keeping
/// only `Rc<str>` + `usize` offset and reconstructing a short-lived lexer on
/// each call to `next()`.  Lexer initialisation is O(1) (pointer + state
/// setup), so this is negligible.
/// One source being lexed. Included files push new frames; the bottom frame is
/// the primary translation unit. Each frame knows its interned [`FileId`], so a
/// token's span points into the file it actually came from.
struct Frame {
    source: Arc<str>,
    offset: usize,
    file: FileId,
    /// Resolved path of this frame's file. Its parent directory is the search
    /// base for quoted includes appearing in this file.
    path: PathBuf,
}

#[derive(Clone)]
enum MacroDefinition {
    Object(Vec<Token>),
    Function {
        parameters: Vec<String>,
        replacement: Vec<Token>,
    },
}

#[derive(Clone)]
struct ExpansionToken {
    token: Token,
    span: Span,
    hideset: HashSet<String>,
}

pub struct TokenStream {
    /// Stack of source frames.  Top = active frame.
    frames: Vec<Frame>,
    defines: HashMap<String, MacroDefinition>,
    pending: VecDeque<ExpansionToken>,
    include_paths: IncludePaths,
    cond_stack: Vec<CondState>,
    diagnostics: Vec<Diagnostic>,
}

impl TokenStream {
    /// Set the top frame's offset to right after the next `\n` in `remainder`.
    ///
    /// `remainder` must be a suffix of the top frame's source.  The formula
    /// `source_len - remainder.len()` recovers the absolute position even
    /// though `remainder` was originally sliced from `source[offset..]`.
    fn skip_line(&mut self, source_len: usize, remainder: &str) {
        let mut consumed = 0;
        loop {
            let rest = &remainder[consumed..];
            let Some(i) = rest.find('\n') else {
                consumed = remainder.len();
                break;
            };
            let line_end = consumed + i;
            consumed = line_end + 1;
            if !remainder[..line_end].ends_with('\\') {
                break;
            }
        }
        let new_offset = source_len - remainder.len() + consumed;
        if let Some(frame) = self.frames.last_mut() {
            frame.offset = new_offset;
        }
    }

    /// The currently active file, for spanning directives.
    fn current_file(&self) -> FileId {
        self.frames.last().unwrap().file
    }

    /// Resolve an `#include` to its `(path, contents)`, or `None` if not found.
    /// Quoted includes search the including file's directory first; both forms
    /// then search `user` (`-I` order) and `system` directories.
    fn resolve_include(&self, path: &str, quoted: bool) -> Option<(PathBuf, String)> {
        let includer_dir = if quoted {
            self.frames
                .last()
                .and_then(|f| f.path.parent())
                .map(Path::to_path_buf)
        } else {
            None
        };
        includer_dir
            .iter()
            .chain(&self.include_paths.user)
            .chain(&self.include_paths.system)
            .find_map(|dir| {
                let candidate = dir.join(path);
                std::fs::read_to_string(&candidate)
                    .ok()
                    .map(|content| (candidate, content))
            })
    }

    fn process_directive(
        &mut self,
        source: &str,
        directive_start: usize,
        mut pp: Lexer<'_, PreprocToken>,
    ) {
        let skipping = is_skipping(&self.cond_stack);

        match pp.next() {
            Some(Ok(PreprocToken::Define)) if !skipping => {
                let name = match pp.next() {
                    Some(Ok(PreprocToken::Identifier(n))) => n,
                    _ => {
                        self.skip_line(source.len(), pp.remainder());
                        return;
                    }
                };
                let remainder = pp.remainder();
                let definition_text = logical_line(remainder);
                let definition = if definition_text.starts_with('(') {
                    let Some(parameters_end) = definition_text.find(')') else {
                        self.skip_line(source.len(), remainder);
                        return;
                    };
                    let parameters = &definition_text[1..parameters_end];
                    let parameters = if parameters.trim().is_empty() {
                        Vec::new()
                    } else {
                        parameters
                            .split(',')
                            .map(|parameter| parameter.trim().to_string())
                            .collect()
                    };
                    MacroDefinition::Function {
                        parameters,
                        replacement: lex_replacement(&definition_text[parameters_end + 1..]),
                    }
                } else {
                    MacroDefinition::Object(lex_replacement(&definition_text))
                };
                self.defines.insert(name, definition);
                self.skip_line(source.len(), remainder);
            }

            Some(Ok(PreprocToken::Undef)) if !skipping => {
                if let Some(Ok(PreprocToken::Identifier(n))) = pp.next() {
                    self.defines.remove(&n);
                }
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(PreprocToken::Include)) if !skipping => {
                let (path, quoted) = match pp.next() {
                    Some(Ok(PreprocToken::QuotedPath)) => (unquote(pp.slice()), true),
                    Some(Ok(PreprocToken::AnglePath)) => (unquote(pp.slice()), false),
                    _ => {
                        self.skip_line(source.len(), pp.remainder());
                        return;
                    }
                };
                self.skip_line(source.len(), pp.remainder());
                match self.resolve_include(&path, quoted) {
                    Some((resolved, content)) => {
                        let file = intern_file(&resolved.to_string_lossy(), &content);
                        self.frames.push(Frame {
                            source: file_source(file),
                            offset: 0,
                            file,
                            path: resolved,
                        });
                    }
                    None => {
                        let span = Span::new(self.current_file(), directive_start);
                        self.diagnostics
                            .push(MissingInclude::new(span, path).into());
                    }
                }
            }

            Some(Ok(PreprocToken::Ifdef)) => {
                let name = match pp.next() {
                    Some(Ok(PreprocToken::Identifier(n))) => n,
                    _ => String::new(),
                };
                let state = if skipping {
                    CondState::OuterSkip
                } else if self.defines.contains_key(&name) {
                    CondState::Active
                } else {
                    CondState::Inactive
                };
                self.cond_stack.push(state);
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(PreprocToken::Ifndef)) => {
                let name = match pp.next() {
                    Some(Ok(PreprocToken::Identifier(n))) => n,
                    _ => String::new(),
                };
                let state = if skipping {
                    CondState::OuterSkip
                } else if !self.defines.contains_key(&name) {
                    CondState::Active
                } else {
                    CondState::Inactive
                };
                self.cond_stack.push(state);
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(PreprocToken::If)) => {
                let remainder = pp.remainder();
                let line_end = remainder.find('\n').unwrap_or(remainder.len());
                let result = !skipping
                    && eval_if_expr(
                        &remainder[..line_end],
                        &self.defines,
                        Span::new(self.current_file(), directive_start),
                    ) != 0;
                let state = if skipping {
                    CondState::OuterSkip
                } else if result {
                    CondState::Active
                } else {
                    CondState::Inactive
                };
                self.cond_stack.push(state);
                self.skip_line(source.len(), remainder);
            }

            Some(Ok(PreprocToken::Elif)) => {
                let remainder = pp.remainder();
                let line_end = remainder.find('\n').unwrap_or(remainder.len());
                let expr_str = &remainder[..line_end];
                let span = Span::new(self.current_file(), directive_start);
                if let Some(top) = self.cond_stack.last_mut() {
                    *top = match *top {
                        CondState::Inactive => {
                            if eval_if_expr(expr_str, &self.defines, span) != 0 {
                                CondState::Active
                            } else {
                                CondState::Inactive
                            }
                        }
                        CondState::Active => CondState::Done,
                        other => other,
                    };
                }
                self.skip_line(source.len(), remainder);
            }

            Some(Ok(PreprocToken::Elifdef)) => {
                let name = match pp.next() {
                    Some(Ok(PreprocToken::Identifier(n))) => n,
                    _ => String::new(),
                };
                if let Some(top) = self.cond_stack.last_mut() {
                    *top = match *top {
                        CondState::Inactive => {
                            if self.defines.contains_key(&name) {
                                CondState::Active
                            } else {
                                CondState::Inactive
                            }
                        }
                        CondState::Active => CondState::Done,
                        other => other,
                    };
                }
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(PreprocToken::Elifndef)) => {
                let name = match pp.next() {
                    Some(Ok(PreprocToken::Identifier(n))) => n,
                    _ => String::new(),
                };
                if let Some(top) = self.cond_stack.last_mut() {
                    *top = match *top {
                        CondState::Inactive => {
                            if !self.defines.contains_key(&name) {
                                CondState::Active
                            } else {
                                CondState::Inactive
                            }
                        }
                        CondState::Active => CondState::Done,
                        other => other,
                    };
                }
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(PreprocToken::Else)) => {
                if let Some(top) = self.cond_stack.last_mut() {
                    *top = match *top {
                        CondState::Inactive => CondState::Active,
                        CondState::Active => CondState::Done,
                        other => other,
                    };
                }
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(PreprocToken::Endif)) => {
                self.cond_stack.pop();
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(directive @ (PreprocToken::Error | PreprocToken::Warning))) if !skipping => {
                let remainder = pp.remainder();
                let line_end = remainder.find('\n').unwrap_or(remainder.len());
                let text = remainder[..line_end].trim().to_string();
                let span = Span::new(self.current_file(), directive_start);
                let diag: Diagnostic = if directive == PreprocToken::Error {
                    PreprocError::new(span, text).into()
                } else {
                    PreprocWarning::new(span, text).into()
                };
                self.diagnostics.push(diag);
                self.skip_line(source.len(), remainder);
            }

            _ => self.skip_line(source.len(), pp.remainder()),
        }
    }

    fn next_unexpanded(&mut self) -> Option<ExpansionToken> {
        loop {
            while self
                .frames
                .last()
                .is_some_and(|frame| frame.offset >= frame.source.len())
            {
                self.frames.pop();
            }

            let (source, offset, file) = {
                let frame = self.frames.last()?;
                (Arc::clone(&frame.source), frame.offset, frame.file)
            };
            let span = Span::new(file, offset);
            let mut lexer = Token::lexer(&source[offset..]);

            match lexer.next() {
                None => {
                    self.frames.pop();
                }
                Some(Err(_)) => {
                    self.frames.last_mut().unwrap().offset = source.len() - lexer.remainder().len();
                }
                Some(Ok(Token::Hash)) => {
                    let pp = lexer.morph::<PreprocToken>();
                    self.process_directive(&source, offset, pp);
                }
                Some(Ok(token)) => {
                    self.frames.last_mut().unwrap().offset = source.len() - lexer.remainder().len();
                    if !is_skipping(&self.cond_stack) {
                        return Some(ExpansionToken {
                            token,
                            span,
                            hideset: HashSet::new(),
                        });
                    }
                }
            }
        }
    }

    fn take_input(&mut self) -> Option<ExpansionToken> {
        self.pending.pop_front().or_else(|| self.next_unexpanded())
    }

    fn prepend(&mut self, tokens: impl IntoIterator<Item = ExpansionToken>) {
        let tokens = tokens.into_iter().collect::<Vec<_>>();
        for token in tokens.into_iter().rev() {
            self.pending.push_front(token);
        }
    }
}

impl Iterator for TokenStream {
    type Item = (Token, Span);

    fn next(&mut self) -> Option<(Token, Span)> {
        let token = next_expanded(self)?;
        Some((token.token, token.span))
    }
}

trait ExpansionSource {
    fn take(&mut self) -> Option<ExpansionToken>;
    fn prepend(&mut self, tokens: Vec<ExpansionToken>);
    fn defines(&self) -> &HashMap<String, MacroDefinition>;
}

impl ExpansionSource for TokenStream {
    fn take(&mut self) -> Option<ExpansionToken> {
        self.take_input()
    }

    fn prepend(&mut self, tokens: Vec<ExpansionToken>) {
        TokenStream::prepend(self, tokens);
    }

    fn defines(&self) -> &HashMap<String, MacroDefinition> {
        &self.defines
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

fn expand_if_expr(source: &str, defines: &HashMap<String, MacroDefinition>, span: Span) -> String {
    let mut tokens = Token::lexer(source)
        .filter_map(Result::ok)
        .map(|token| ExpansionToken {
            token,
            span,
            hideset: HashSet::new(),
        })
        .collect::<Vec<_>>();
    for defined_index in 0..tokens.len() {
        if !matches!(&tokens[defined_index].token, Token::Identifier(name) if name == "defined") {
            continue;
        }
        tokens[defined_index].hideset.insert("defined".to_string());
        let mut operand_index = defined_index + 1;
        while tokens
            .get(operand_index)
            .is_some_and(|token| is_whitespace(&token.token))
        {
            operand_index += 1;
        }
        if tokens
            .get(operand_index)
            .is_some_and(|token| matches!(token.token, Token::LParen))
        {
            operand_index += 1;
            while tokens
                .get(operand_index)
                .is_some_and(|token| is_whitespace(&token.token))
            {
                operand_index += 1;
            }
        }
        let operand = tokens
            .get(operand_index)
            .and_then(|token| match &token.token {
                Token::Identifier(name) => Some(name.clone()),
                _ => None,
            });
        if let Some(operand) = operand {
            tokens[operand_index].hideset.insert(operand);
        }
    }
    expand_tokens(tokens, defines)
        .into_iter()
        .map(|token| token.token.to_string())
        .collect()
}

fn lex_replacement(source: &str) -> Vec<Token> {
    Token::lexer(source.trim()).filter_map(Result::ok).collect()
}

fn logical_line(source: &str) -> String {
    let mut line = String::new();
    let mut remainder = source;
    loop {
        let Some(line_end) = remainder.find('\n') else {
            line.push_str(remainder);
            break;
        };
        let segment = &remainder[..line_end];
        if let Some(continued) = segment.strip_suffix('\\') {
            line.push_str(continued);
            remainder = &remainder[line_end + 1..];
        } else {
            line.push_str(segment);
            break;
        }
    }
    line
}

fn is_whitespace(token: &Token) -> bool {
    matches!(token, Token::Whitespace(_) | Token::Comment(_))
}

fn trim_argument(argument: &[ExpansionToken]) -> Vec<ExpansionToken> {
    let start = argument
        .iter()
        .position(|token| !is_whitespace(&token.token))
        .unwrap_or(argument.len());
    let end = argument
        .iter()
        .rposition(|token| !is_whitespace(&token.token))
        .map_or(start, |index| index + 1);
    argument[start..end].to_vec()
}

fn substitute_arguments(
    replacement: Vec<Token>,
    parameters: &[String],
    arguments: Vec<Vec<ExpansionToken>>,
    defines: &HashMap<String, MacroDefinition>,
    span: Span,
) -> Vec<ExpansionToken> {
    let arguments = arguments
        .into_iter()
        .map(|argument| expand_tokens(trim_argument(&argument), defines))
        .collect::<Vec<_>>();
    let mut substituted = Vec::new();
    for token in replacement {
        let parameter = match &token {
            Token::Identifier(name) => parameters.iter().position(|parameter| parameter == name),
            _ => None,
        };
        if let Some(index) = parameter {
            substituted.extend(arguments[index].clone());
        } else {
            substituted.push(ExpansionToken {
                token,
                span,
                hideset: HashSet::new(),
            });
        }
    }
    substituted
}

fn expand_tokens(
    tokens: Vec<ExpansionToken>,
    defines: &HashMap<String, MacroDefinition>,
) -> Vec<ExpansionToken> {
    let mut source = QueuedExpansion {
        input: VecDeque::from(tokens),
        defines,
    };
    let mut output = Vec::new();
    while let Some(token) = next_expanded(&mut source) {
        output.push(token);
    }
    output
}

fn next_expanded(source: &mut impl ExpansionSource) -> Option<ExpansionToken> {
    loop {
        let token = source.take()?;
        let Token::Identifier(name) = &token.token else {
            return Some(token);
        };
        let Some(definition) = source.defines().get(name).cloned() else {
            return Some(token);
        };
        if token.hideset.contains(name) {
            return Some(token);
        }

        let replacement = match definition {
            MacroDefinition::Object(replacement) => replacement
                .into_iter()
                .map(|replacement| ExpansionToken {
                    token: replacement,
                    span: token.span,
                    hideset: token.hideset.clone(),
                })
                .collect::<Vec<_>>(),
            MacroDefinition::Function {
                parameters,
                replacement,
            } => {
                let Some(arguments) = take_invocation(source, &parameters) else {
                    return Some(token);
                };
                substitute_arguments(
                    replacement,
                    &parameters,
                    arguments,
                    source.defines(),
                    token.span,
                )
            }
        };
        let mut hideset = token.hideset;
        hideset.insert(name.clone());
        source.prepend(
            replacement
                .into_iter()
                .map(|mut replacement| {
                    replacement.hideset.extend(hideset.iter().cloned());
                    replacement
                })
                .collect(),
        );
    }
}

fn take_invocation(
    source: &mut impl ExpansionSource,
    parameters: &[String],
) -> Option<Vec<Vec<ExpansionToken>>> {
    let mut consumed = Vec::new();
    let opening = loop {
        let Some(token) = source.take() else {
            source.prepend(consumed);
            return None;
        };
        if is_whitespace(&token.token) {
            consumed.push(token);
        } else {
            break token;
        }
    };
    consumed.push(opening);
    if !matches!(consumed.last().unwrap().token, Token::LParen) {
        source.prepend(consumed);
        return None;
    }

    let mut arguments = vec![Vec::new()];
    let mut depth = 0;
    loop {
        let Some(token) = source.take() else {
            source.prepend(consumed);
            return None;
        };
        consumed.push(token.clone());
        match token.token {
            Token::LParen => {
                depth += 1;
                arguments.last_mut().unwrap().push(token);
            }
            Token::RParen if depth == 0 => break,
            Token::RParen => {
                depth -= 1;
                arguments.last_mut().unwrap().push(token);
            }
            Token::Comma if depth == 0 => arguments.push(Vec::new()),
            _ => arguments.last_mut().unwrap().push(token),
        }
    }
    if parameters.is_empty() && arguments.len() == 1 && trim_argument(&arguments[0]).is_empty() {
        arguments.clear();
    }
    if arguments.len() != parameters.len() {
        source.prepend(consumed);
        return None;
    }
    Some(arguments)
}

struct QueuedExpansion<'a> {
    input: VecDeque<ExpansionToken>,
    defines: &'a HashMap<String, MacroDefinition>,
}

impl ExpansionSource for QueuedExpansion<'_> {
    fn take(&mut self) -> Option<ExpansionToken> {
        self.input.pop_front()
    }

    fn prepend(&mut self, tokens: Vec<ExpansionToken>) {
        for token in tokens.into_iter().rev() {
            self.input.push_front(token);
        }
    }

    fn defines(&self) -> &HashMap<String, MacroDefinition> {
        self.defines
    }
}

impl TokenStream {
    /// Drain the stream into the full token list. Diagnostics raised during
    /// preprocessing (`#error`, `#warning`) are available via [`Self::diagnostics`]
    /// afterwards.
    pub fn collect_tokens(&mut self) -> Vec<(Token, Span)> {
        self.by_ref().collect()
    }

    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }
}

/// Strip the surrounding `"..."` or `<...>` delimiters from an include path.
fn unquote(spelling: &str) -> String {
    spelling[1..spelling.len() - 1].to_string()
}

/// Build a lazy preprocessed token stream over a source file.
///
/// * `name`          — file name shown in diagnostics (e.g. a path or `<stdin>`)
/// * `source`        — primary translation unit text
/// * `defines`       — predefined macros (name → single-token replacement)
/// * `include_paths` — directories searched for `#include` files
pub fn preprocessed(
    name: &str,
    source: &str,
    defines: HashMap<String, Token>,
    include_paths: &IncludePaths,
) -> TokenStream {
    let file = intern_file(name, source);
    let defines = defines
        .into_iter()
        .map(|(name, token)| {
            let replacement = if token == Token::Hash {
                Vec::new()
            } else {
                vec![token]
            };
            (name, MacroDefinition::Object(replacement))
        })
        .collect();
    TokenStream {
        frames: vec![Frame {
            source: file_source(file),
            offset: 0,
            file,
            path: PathBuf::from(name),
        }],
        defines,
        pending: VecDeque::new(),
        include_paths: include_paths.clone(),
        cond_stack: Vec::new(),
        diagnostics: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{IncludePaths, preprocessed};
    use crate::diagnostics::Code;
    use std::collections::HashMap;

    fn diagnostics(source: &str) -> Vec<Code> {
        let mut stream = preprocessed(
            "<pp-test>",
            source,
            HashMap::new(),
            &IncludePaths::default(),
        );
        stream.collect_tokens();
        stream.diagnostics().iter().map(|d| d.code()).collect()
    }

    #[test]
    fn missing_include_reports_the_path() {
        let mut stream = preprocessed(
            "<pp-test>",
            "#include \"nope.h\"\n",
            HashMap::new(),
            &IncludePaths::default(),
        );
        stream.collect_tokens();
        let diags = stream.diagnostics();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code(), Code::MissingInclude);
        let mut buf = Vec::new();
        diags[0].write(&mut buf, false).unwrap();
        let rendered = String::from_utf8(buf).unwrap();
        assert!(rendered.contains("'nope.h' file not found"), "{rendered}");
    }

    #[test]
    fn error_directive_raises_an_error() {
        let codes = diagnostics("#error broken\nint main(void){return 0;}\n");
        assert_eq!(codes, vec![Code::PreprocError]);
    }

    #[test]
    fn warning_directive_raises_a_warning() {
        let codes = diagnostics("#warning heads up\nint main(void){return 0;}\n");
        assert_eq!(codes, vec![Code::PreprocWarning]);
    }

    #[test]
    fn skipped_error_directive_is_silent() {
        let codes = diagnostics("#if 0\n#error never\n#endif\n");
        assert!(codes.is_empty());
    }
}
