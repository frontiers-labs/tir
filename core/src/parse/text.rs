use std::collections::HashMap;
use std::sync::Arc;

use crate::Region;
use crate::block::BlockId;
use crate::parse::common::{Cursor, Span};

pub(crate) struct RegionParseState {
    pub region: Arc<Region>,
    pub indices: HashMap<u32, BlockId>,
}

pub struct Parser<'src> {
    src: &'src str,
    position: u32,
    pub(crate) region_parse: Option<RegionParseState>,
}

impl<'src> Parser<'src> {
    pub fn new(src: &'src str) -> Self {
        Self {
            src,
            position: 0,
            region_parse: None,
        }
    }

    pub fn peek_char(&self) -> Option<char> {
        self.src.chars().nth(self.position as usize)
    }

    pub fn parse_ident(&mut self) -> Option<&'src str> {
        let start = self.position as usize;

        if self
            .src
            .chars()
            .nth(start)
            .map(|c| c.is_alphabetic())
            .unwrap_or(false)
        {
            let mut last = start + 1;
            while let Some(c) = self.src.chars().nth(last) {
                if !c.is_alphanumeric() && c != '_' {
                    break;
                }
                last += 1;
            }

            self.position = last as u32;
            self.skip_trivia();
            Some(&self.src[start..last])
        } else {
            None
        }
    }

    pub fn parse_token(&mut self, token: &str) -> bool {
        if self
            .src
            .get(self.position as usize..)
            .map(|s| s.starts_with(token))
            .unwrap_or(false)
        {
            self.position += token.len() as u32;
            self.skip_trivia();
            true
        } else {
            false
        }
    }

    pub fn parse_string(&mut self) -> Option<&'src str> {
        if self.src.get(self.position as usize..)?.starts_with('"') {
            let start = self.position as usize + 1;
            let mut i = start;
            while let Some(c) = self.src.chars().nth(i) {
                if c == '"' {
                    break;
                }
                i += 1;
            }
            if self.src.chars().nth(i)? == '"' {
                self.position = (i + 1) as u32;
                self.skip_trivia();
                return Some(&self.src[start..i]);
            }
        }
        None
    }

    pub fn parse_number(&mut self) -> Option<i64> {
        let mut i = self.position as usize;
        let bytes = self.src.as_bytes();
        if i >= bytes.len() {
            return None;
        }
        let mut neg = false;
        if bytes[i] == b'-' {
            neg = true;
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        let s = &self.src[(if neg { start - 1 } else { start })..i];
        let val: i64 = s.parse().ok()?;
        self.position = i as u32;
        self.skip_trivia();
        Some(val)
    }

    pub fn parse_value_ref(&mut self) -> Option<&'src str> {
        if self
            .src
            .get(self.position as usize..)
            .map(|s| s.starts_with('%'))
            .unwrap_or(false)
        {
            let start = self.position as usize + 1;
            let mut last = start;
            while let Some(c) = self.src.chars().nth(last) {
                if !c.is_alphanumeric() && c != '_' {
                    break;
                }
                last += 1;
            }
            if last > start {
                self.position = last as u32;
                let result = &self.src[start..last];
                self.skip_trivia();
                Some(result)
            } else {
                None
            }
        } else {
            None
        }
    }

    pub fn parse_type(
        &mut self,
        context: &crate::Context,
    ) -> Result<Option<crate::TypeId>, (Span, crate::Error)> {
        let mark = self.position;
        if !self.parse_token("!") {
            return Ok(None);
        }

        let dialect_or_name = self
            .parse_ident()
            .ok_or_else(|| (self.span(), crate::Error::ExpectedType))?;

        let (dialect, name) = if self.parse_token(".") {
            let Some(name) = self.parse_ident() else {
                return Err((self.span(), crate::Error::ExpectedType));
            };
            (dialect_or_name, name)
        } else {
            ("builtin", dialect_or_name)
        };

        let type_parser = context
            .get_type_parser(dialect, name)
            .map_err(|err| (self.span(), err))?;

        match type_parser(name, self, context) {
            Ok(ty) => Ok(Some(ty)),
            Err(err) => {
                self.position = mark;
                Err(err)
            }
        }
    }

    /// Parse the region-local index in a `^bb<number>` reference.
    pub fn parse_block_index(&mut self) -> Option<u32> {
        let mark = self.position;
        if !self.parse_token("^bb") {
            return None;
        }
        match self.parse_number() {
            Some(n) if n >= 0 => Some(n as u32),
            _ => {
                self.position = mark;
                None
            }
        }
    }

    /// Parse a `^bb<number>` reference, returning a [`BlockId`](crate::BlockId)
    /// without applying any active region parse scope.
    pub fn parse_block_ref(&mut self) -> Option<BlockId> {
        self.parse_block_index().map(BlockId::from_number)
    }

    pub fn parse_symbol_name(&mut self) -> Option<&'src str> {
        if self
            .src
            .get(self.position as usize..)
            .map(|s| s.starts_with('@'))
            .unwrap_or(false)
        {
            self.position += 1;
            self.parse_ident()
        } else {
            None
        }
    }

    pub fn pos(&self) -> u32 {
        self.position
    }
    pub fn set_pos(&mut self, pos: u32) {
        self.position = pos;
        self.skip_trivia();
    }
}

impl Cursor for Parser<'_> {
    fn span(&self) -> Span {
        Span(self.position)
    }

    fn skip_trivia(&mut self) {
        // `position` is a byte offset (see `parse_token`/`peek_char`), so work in
        // byte offsets throughout to stay correct on non-ASCII input.
        let mut last = self.position as usize;
        loop {
            // Whitespace (including newlines).
            last += self.src[last..]
                .char_indices()
                .find(|(_, c)| !c.is_whitespace())
                .map_or(self.src.len() - last, |(i, _)| i);
            // `//` line comments, so a `.tir` test file can carry lit
            // `RUN:`/`CHECK:` directives without breaking the parser.
            if self.src[last..].starts_with("//") {
                match self.src[last..].find('\n') {
                    Some(i) => last += i + 1,
                    None => last = self.src.len(),
                }
                continue;
            }
            break;
        }

        self.position = last as u32;
    }
}
