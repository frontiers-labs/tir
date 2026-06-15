use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use logos::{Lexer, Logos};

use crate::diagnostics::{
    Diagnostic, FileId, PreprocError, PreprocWarning, Span, file_source, intern_file,
};
use crate::lexer::Token;

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
fn eval_if_expr(expr: &str, defines: &HashMap<String, Token>) -> i64 {
    let toks: Vec<PreprocToken> = PreprocToken::lexer(expr).filter_map(|r| r.ok()).collect();
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
    defines: &'a HashMap<String, Token>,
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
                        Some(Token::IntegerLiteral(n)) => n.to_i64(),
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
}

pub struct TokenStream {
    /// Stack of source frames.  Top = active frame.
    frames: Vec<Frame>,
    /// Maps macro names to their single-token replacement value.
    ///
    /// `Token::Hash` is used as a sentinel meaning "defined but no replacement
    /// text" (e.g. `#define FLAG`).  Such macros participate in `#ifdef` /
    /// `#ifndef` but expand to nothing in code.
    defines: HashMap<String, Token>,
    include_paths: Vec<PathBuf>,
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
        let new_offset = match remainder.find('\n') {
            Some(i) => source_len - remainder.len() + i + 1,
            None => source_len,
        };
        if let Some(frame) = self.frames.last_mut() {
            frame.offset = new_offset;
        }
    }

    /// The currently active file, for spanning directives.
    fn current_file(&self) -> FileId {
        self.frames.last().unwrap().file
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
                let body_end = remainder.find('\n').unwrap_or(remainder.len());
                // Lex the replacement body to get its token value.
                // Token::Hash is the sentinel for "no replacement text".
                let token = Token::lexer(remainder[..body_end].trim())
                    .next()
                    .and_then(|r| r.ok())
                    .unwrap_or(Token::Hash);
                self.defines.insert(name, token);
                self.skip_line(source.len(), remainder);
            }

            Some(Ok(PreprocToken::Undef)) if !skipping => {
                if let Some(Ok(PreprocToken::Identifier(n))) = pp.next() {
                    self.defines.remove(&n);
                }
                self.skip_line(source.len(), pp.remainder());
            }

            Some(Ok(PreprocToken::Include)) if !skipping => {
                let path = match pp.next() {
                    Some(Ok(PreprocToken::QuotedPath)) => {
                        let s = pp.slice();
                        s[1..s.len() - 1].to_string()
                    }
                    Some(Ok(PreprocToken::AnglePath)) => {
                        let s = pp.slice();
                        s[1..s.len() - 1].to_string()
                    }
                    _ => {
                        self.skip_line(source.len(), pp.remainder());
                        return;
                    }
                };
                self.skip_line(source.len(), pp.remainder());
                let content = self
                    .include_paths
                    .iter()
                    .find_map(|dir| std::fs::read_to_string(dir.join(&path)).ok());
                if let Some(content) = content {
                    let file = intern_file(&path, &content);
                    self.frames.push(Frame {
                        source: file_source(file),
                        offset: 0,
                        file,
                    });
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
                let result = !skipping && eval_if_expr(&remainder[..line_end], &self.defines) != 0;
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
                if let Some(top) = self.cond_stack.last_mut() {
                    *top = match *top {
                        CondState::Inactive => {
                            if eval_if_expr(expr_str, &self.defines) != 0 {
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
}

impl Iterator for TokenStream {
    type Item = (Token, Span);

    fn next(&mut self) -> Option<(Token, Span)> {
        loop {
            // Drop exhausted frames.
            while self
                .frames
                .last()
                .is_some_and(|f| f.offset >= f.source.len())
            {
                self.frames.pop();
            }

            // Clone Arc (cheap) + copy offset/file so we release the shared
            // borrow on frames before taking &mut self below.
            let (source_rc, offset, file) = {
                let top = self.frames.last()?;
                (Arc::clone(&top.source), top.offset, top.file)
            };

            // Every emitted token is spanned at its start position in its file.
            let span = Span::new(file, offset);

            let mut lexer = Token::lexer(&source_rc[offset..]);
            let tok = lexer.next();

            match tok {
                None => {
                    self.frames.pop();
                }

                Some(Err(_)) => {
                    // Unrecognised character — skip it.
                    let new = source_rc.len() - lexer.remainder().len();
                    self.frames.last_mut().unwrap().offset = new;
                }

                Some(Ok(Token::Hash)) => {
                    // morph hands the same source position to the directive lexer.
                    let pp = lexer.morph::<PreprocToken>();
                    self.process_directive(&source_rc, offset, pp);
                    // process_directive always calls skip_line, which sets the offset.
                }

                Some(Ok(Token::Identifier(name))) => {
                    let new = source_rc.len() - lexer.remainder().len();
                    self.frames.last_mut().unwrap().offset = new;
                    if !is_skipping(&self.cond_stack) {
                        match self.defines.get(&name).cloned() {
                            Some(Token::Hash) => {
                                // Empty define — expands to nothing; continue.
                            }
                            Some(tok) => return Some((tok, span)),
                            None => return Some((Token::Identifier(name), span)),
                        }
                    }
                }

                Some(Ok(c_tok)) => {
                    let new = source_rc.len() - lexer.remainder().len();
                    self.frames.last_mut().unwrap().offset = new;
                    if !is_skipping(&self.cond_stack) {
                        return Some((c_tok, span));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

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
    include_paths: &[PathBuf],
) -> TokenStream {
    let file = intern_file(name, source);
    TokenStream {
        frames: vec![Frame {
            source: file_source(file),
            offset: 0,
            file,
        }],
        defines,
        include_paths: include_paths.to_vec(),
        cond_stack: Vec::new(),
        diagnostics: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::preprocessed;
    use crate::diagnostics::Code;
    use std::collections::HashMap;

    fn diagnostics(source: &str) -> Vec<Code> {
        let mut stream = preprocessed("<pp-test>", source, HashMap::new(), &[]);
        stream.collect_tokens();
        stream.diagnostics().iter().map(|d| d.code()).collect()
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
