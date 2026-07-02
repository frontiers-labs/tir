use std::collections::HashMap;
use std::sync::Arc;

use crate::Region;
use crate::block::BlockId;
use crate::parse::common::{Cursor, Span};
use crate::value::ValueId;

pub(crate) struct RegionParseState {
    pub region: Arc<Region>,
    pub indices: HashMap<u32, BlockId>,
}

pub struct Parser<'src> {
    src: &'src str,
    position: u32,
    pub(crate) region_parse: Option<RegionParseState>,
    /// Maps textual SSA names (the text after `%`) to the value ids actually
    /// allocated for them, so names need not match internal ids and need not be
    /// contiguous. Stays flat across nested regions: the printer emits globally
    /// unique value numbers, so a single namespace is enough.
    value_names: HashMap<String, ValueId>,
}

impl<'src> Parser<'src> {
    pub fn new(src: &'src str) -> Self {
        Self {
            src,
            position: 0,
            region_parse: None,
            value_names: HashMap::new(),
        }
    }

    /// Bind a textual SSA name to the value id allocated for it.
    pub fn define_value(&mut self, name: &str, id: ValueId) {
        self.value_names.insert(name.to_string(), id);
    }

    /// Resolve a textual SSA name to its value id. Names bound during this parse
    /// win; a purely numeric name otherwise falls back to its literal id, so
    /// single-op parses can still wire operands to pre-existing values.
    pub fn resolve_value(&self, name: &str) -> Option<ValueId> {
        self.value_names
            .get(name)
            .copied()
            .or_else(|| name.parse::<u32>().ok().map(ValueId::from_number))
    }

    // `position` is a byte offset; every scan below works on the byte-indexed
    // remainder of the source (`&src[position..]`), never on `chars().nth`,
    // which counts characters and drifts after any multi-byte character.
    pub fn peek_char(&self) -> Option<char> {
        self.src.get(self.position as usize..)?.chars().next()
    }

    pub fn parse_ident(&mut self) -> Option<&'src str> {
        let start = self.position as usize;
        let rest = self.src.get(start..)?;
        if !rest.chars().next()?.is_alphabetic() {
            return None;
        }
        let len = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        self.position = (start + len) as u32;
        self.skip_trivia();
        Some(&self.src[start..start + len])
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
        if !self.src.get(self.position as usize..)?.starts_with('"') {
            return None;
        }
        let start = self.position as usize + 1;
        let len = self.src[start..].find('"')?;
        self.position = (start + len + 1) as u32;
        self.skip_trivia();
        Some(&self.src[start..start + len])
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

    /// Parse a float literal in Rust `{:?}` notation: a decimal point is
    /// required (`3.0`, `-2.5e-3`), so plain integers are left for
    /// [`Self::parse_number`]; `inf`/`-inf`/`NaN` cover the specials.
    pub fn parse_float(&mut self) -> Option<f64> {
        for (text, value) in [
            ("-inf", f64::NEG_INFINITY),
            ("inf", f64::INFINITY),
            ("NaN", f64::NAN),
        ] {
            if self.parse_token(text) {
                return Some(value);
            }
        }

        let start = self.position as usize;
        let bytes = self.src.as_bytes();
        let mut i = start;
        if i < bytes.len() && bytes[i] == b'-' {
            i += 1;
        }
        let int_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == int_start || i >= bytes.len() || bytes[i] != b'.' {
            return None;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            let mut j = i + 1;
            if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                j += 1;
            }
            let exp_start = j;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > exp_start {
                i = j;
            }
        }
        let val: f64 = self.src[start..i].parse().ok()?;
        self.position = i as u32;
        self.skip_trivia();
        Some(val)
    }

    pub fn parse_value_ref(&mut self) -> Option<&'src str> {
        if !self.src.get(self.position as usize..)?.starts_with('%') {
            return None;
        }
        let start = self.position as usize + 1;
        let rest = &self.src[start..];
        let len = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        if len == 0 {
            return None;
        }
        self.position = (start + len) as u32;
        let result = &self.src[start..start + len];
        self.skip_trivia();
        Some(result)
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
