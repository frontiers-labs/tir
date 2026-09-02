use crate::parse::common::{Cursor, Span};

pub trait TokenLike<'src> {
    fn as_ident(&self) -> Option<&'src str>;
    fn is_symbol(&self, _sym: Symbol) -> bool {
        false
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Symbol {
    Comma,
}

pub struct Parser<'src, Tok> {
    tokens: &'src [Tok],
    position: u32,
}

impl<'src, Tok> Parser<'src, Tok> {
    pub fn new(tokens: &'src [Tok]) -> Self {
        Self {
            tokens,
            position: 0,
        }
    }

    pub fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.position as usize)
    }

    pub fn bump(&mut self) -> Option<&Tok> {
        let tok = self.tokens.get(self.position as usize);
        if tok.is_some() {
            self.position += 1;
        }
        tok
    }

    /// Current cursor position, for use as a backtracking checkpoint with
    /// [`Parser::reset`].
    pub fn position(&self) -> u32 {
        self.position
    }

    /// Rewind the cursor to a position previously returned by
    /// [`Parser::position`]. Used to retry an alternative when a speculative
    /// parse fails.
    pub fn reset(&mut self, position: u32) {
        self.position = position;
    }
}

impl<'src, Tok: TokenLike<'src>> Parser<'src, Tok> {
    pub fn parse_ident(&mut self) -> Option<&'src str> {
        let res = self.peek().and_then(|t| t.as_ident());
        if res.is_some() {
            self.bump();
        }
        res
    }
}

impl<Tok> Cursor for Parser<'_, Tok> {
    fn span(&self) -> Span {
        Span(self.position)
    }

    fn skip_trivia(&mut self) {
        // Typically a no-op for token streams that have already skipped trivia.
    }
}
