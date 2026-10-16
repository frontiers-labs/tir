//! Signature schemes: the types an op takes and gives, as one polymorphic
//! term, `forall W: nat. (int<W>, int<W>) -> int<W>`.
//!
//! Binders are the scheme's variables and each `_` is a variable of its own,
//! fresh for every value of its slot. A binder bound by one value constrains
//! every other value it appears in; there is no subtyping and no implicit
//! conversion. The IR's type check joins the patterns with the types of an
//! op's values on the relational engine (`tir::check_types`).
//!
//! ```text
//! scheme  := ["forall" binder ("," binder)* "."] slots "->" (slot | slots)
//! slots   := "(" [slot ("," slot)*] ")"
//! binder  := NAME [":" ("nat" | "data" | "sym" | pattern ("|" pattern)*)]
//! slot    := pattern ["*" | "?"]
//! pattern := "_" | NAME | NUMBER | CTOR ["<" pattern ("," pattern)* ">"]
//! ```
//!
//! `CTOR` names a constructor of the term store: `dialect.name`, a bare name
//! of the `builtin` dialect, or `int`/`float` for `builtin.i`/`builtin.f`.
//! A binder bounded by patterns is a type that has the shape of one of them:
//! `forall C: index | int<_>.` binds `C` to an index or an integer.

use std::fmt;

use crate::term::{Kind, Param, Terms, TypeArg};

/// How many values fill a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arity {
    One,
    /// `?`: none or one.
    Opt,
    /// `*`: any number.
    Many,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Pattern {
    Wild,
    Binder(String),
    Nat(u64),
    Ctor(String, Vec<Pattern>),
}

/// One operand or result position of a scheme as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotSyntax {
    pattern: Pattern,
    pub arity: Arity,
}

/// A scheme as written: checked for syntax, its constructors not yet looked
/// up. The `operation!` macro reads the slot arities from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemeSyntax {
    binders: Vec<(String, Kind)>,
    // Per binder, the shapes it may take; empty for any.
    bounds: Vec<Vec<Pattern>>,
    pub operands: Vec<SlotSyntax>,
    pub results: Vec<SlotSyntax>,
}

impl SchemeSyntax {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut parser = Parser { text, at: 0 };
        let (binders, bounds) = parser.binders()?;
        let names: Vec<&str> = binders.iter().map(|(name, _)| name.as_str()).collect();
        let operands = parser.slots(&names)?;
        parser.expect("->")?;
        let results = if parser.peek("(") {
            parser.slots(&names)?
        } else {
            vec![parser.slot(&names)?]
        };
        parser.skip_space();
        if parser.at != text.len() {
            return Err(parser.error("end of scheme"));
        }
        Ok(Self {
            binders,
            bounds,
            operands,
            results,
        })
    }

    /// A slot of `pattern` written as in a scheme, for the ports an op
    /// appends to the ones it declares.
    pub fn slot(pattern: &str, arity: Arity) -> Result<SlotSyntax, String> {
        let mut parser = Parser {
            text: pattern,
            at: 0,
        };
        let pattern = parser.pattern(&[])?;
        Ok(SlotSyntax { pattern, arity })
    }

    /// Look the constructors up in the term store.
    pub fn resolve(&self, terms: &Terms) -> Result<Scheme, String> {
        let mut names: Vec<String> = self.binders.iter().map(|(name, _)| name.clone()).collect();
        let mut resolve_slot = |slot: &SlotSyntax| -> Result<Slot, String> {
            let pattern = self.resolve_pattern(terms, &slot.pattern, Kind::Data, &mut names)?;
            Ok(Slot {
                pattern,
                arity: slot.arity,
            })
        };
        let operands = self
            .operands
            .iter()
            .map(&mut resolve_slot)
            .collect::<Result<_, _>>()?;
        let results = self
            .results
            .iter()
            .map(&mut resolve_slot)
            .collect::<Result<_, _>>()?;
        let bounds = self
            .bounds
            .iter()
            .map(|shapes| {
                shapes
                    .iter()
                    .map(|shape| self.resolve_pattern(terms, shape, Kind::Data, &mut names))
                    .collect::<Result<_, _>>()
            })
            .collect::<Result<_, _>>()?;
        Ok(Scheme {
            names,
            bounds,
            operands,
            results,
        })
    }

    fn resolve_pattern(
        &self,
        terms: &Terms,
        pattern: &Pattern,
        kind: Kind,
        names: &mut Vec<String>,
    ) -> Result<TypeArg, String> {
        match pattern {
            Pattern::Wild => {
                names.push("_".into());
                Ok(TypeArg::Var(names.len() as u32 - 1))
            }
            Pattern::Binder(name) => {
                let index = self
                    .binders
                    .iter()
                    .position(|(binder, _)| binder == name)
                    .expect("a parsed binder");
                let declared = self.binders[index].1;
                let fits = declared == kind || (declared == Kind::Data && kind == Kind::Ghost);
                if !fits {
                    return Err(format!(
                        "binder `{name}` is {declared:?} where {kind:?} is expected"
                    ));
                }
                Ok(TypeArg::Var(index as u32))
            }
            Pattern::Nat(n) => Ok(TypeArg::Nat(*n)),
            Pattern::Ctor(spelled, args) => {
                let (dialect, name) = match spelled.as_str() {
                    "int" => ("builtin", "i"),
                    "float" => ("builtin", "f"),
                    other => other.split_once('.').unwrap_or(("builtin", other)),
                };
                let ctor = terms
                    .ctor_named(dialect, name)
                    .ok_or_else(|| format!("unknown type constructor `{spelled}`"))?;
                let params = terms.ctor_info(ctor).params;
                let args = args
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| {
                        let kind = match params.get(index).map(|(_, param)| *param) {
                            Some(Param::One(kind) | Param::Opt(kind)) => kind,
                            _ => Kind::Data,
                        };
                        self.resolve_pattern(terms, arg, kind, names)
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(TypeArg::Type(terms.intern(ctor, args)?))
            }
        }
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pattern::Wild => f.write_str("_"),
            Pattern::Binder(name) => f.write_str(name),
            Pattern::Nat(n) => write!(f, "{n}"),
            Pattern::Ctor(name, args) if args.is_empty() => f.write_str(name),
            Pattern::Ctor(name, args) => {
                write!(f, "{name}<")?;
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                f.write_str(">")
            }
        }
    }
}

impl SlotSyntax {
    /// The slot's type as written, without its arity marker.
    pub fn pattern(&self) -> String {
        self.pattern.to_string()
    }
}

impl fmt::Display for SlotSyntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let marker = match self.arity {
            Arity::One => "",
            Arity::Opt => "?",
            Arity::Many => "*",
        };
        write!(f, "{}{marker}", self.pattern)
    }
}

impl fmt::Display for SchemeSyntax {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |f: &mut fmt::Formatter<'_>, slots: &[SlotSyntax]| {
            f.write_str("(")?;
            for (index, slot) in slots.iter().enumerate() {
                if index > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "{slot}")?;
            }
            f.write_str(")")
        };
        if !self.binders.is_empty() {
            f.write_str("forall ")?;
            for (index, ((name, kind), shapes)) in self.binders.iter().zip(&self.bounds).enumerate()
            {
                if index > 0 {
                    f.write_str(", ")?;
                }
                match kind {
                    Kind::Nat => write!(f, "{name}: nat")?,
                    Kind::Sym => write!(f, "{name}: sym")?,
                    _ => f.write_str(name)?,
                }
                for (index, shape) in shapes.iter().enumerate() {
                    f.write_str(if index == 0 { ": " } else { " | " })?;
                    write!(f, "{shape}")?;
                }
            }
            f.write_str(". ")?;
        }
        list(f, &self.operands)?;
        f.write_str(" -> ")?;
        list(f, &self.results)
    }
}

// The binders with their kinds, and per binder the shapes bounding it.
type Binders = (Vec<(String, Kind)>, Vec<Vec<Pattern>>);

struct Parser<'a> {
    text: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.text[self.at..]
    }

    fn skip_space(&mut self) {
        let trimmed = self.rest().trim_start();
        self.at = self.text.len() - trimmed.len();
    }

    fn peek(&mut self, token: &str) -> bool {
        self.skip_space();
        self.rest().starts_with(token)
    }

    fn eat(&mut self, token: &str) -> bool {
        let found = self.peek(token);
        if found {
            self.at += token.len();
        }
        found
    }

    fn expect(&mut self, token: &str) -> Result<(), String> {
        if self.eat(token) {
            Ok(())
        } else {
            Err(self.error(&format!("`{token}`")))
        }
    }

    fn error(&self, expected: &str) -> String {
        format!(
            "scheme `{}`: expected {expected} at `{}`",
            self.text,
            self.rest()
        )
    }

    fn word(&mut self, dots: bool) -> Option<String> {
        self.skip_space();
        let rest = self.rest();
        let len = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || (dots && c == '.')))
            .unwrap_or(rest.len());
        if len == 0 {
            return None;
        }
        let start = self.at;
        self.at += len;
        Some(self.text[start..self.at].to_string())
    }

    fn binders(&mut self) -> Result<Binders, String> {
        let (mut binders, mut bounds) = (Vec::new(), Vec::new());
        if !self.peek("forall") {
            return Ok((binders, bounds));
        }
        self.at += "forall".len();
        loop {
            let Some(name) = self.word(false) else {
                return Err(self.error("a binder"));
            };
            let mut shapes = Vec::new();
            let kind = if self.eat(":") {
                let mark = self.at;
                match self.word(false).as_deref() {
                    Some("nat") => Kind::Nat,
                    Some("data") => Kind::Data,
                    Some("sym") => Kind::Sym,
                    _ => {
                        self.at = mark;
                        shapes.push(self.pattern(&[])?);
                        while self.eat("|") {
                            shapes.push(self.pattern(&[])?);
                        }
                        Kind::Data
                    }
                }
            } else {
                Kind::Data
            };
            binders.push((name, kind));
            bounds.push(shapes);
            if !self.eat(",") {
                break;
            }
        }
        self.expect(".")?;
        Ok((binders, bounds))
    }

    fn slots(&mut self, binders: &[&str]) -> Result<Vec<SlotSyntax>, String> {
        self.expect("(")?;
        let mut slots = Vec::new();
        if self.eat(")") {
            return Ok(slots);
        }
        loop {
            slots.push(self.slot(binders)?);
            if self.eat(")") {
                return Ok(slots);
            }
            self.expect(",")?;
        }
    }

    fn slot(&mut self, binders: &[&str]) -> Result<SlotSyntax, String> {
        let pattern = self.pattern(binders)?;
        let arity = if self.eat("*") {
            Arity::Many
        } else if self.eat("?") {
            Arity::Opt
        } else {
            Arity::One
        };
        Ok(SlotSyntax { pattern, arity })
    }

    fn pattern(&mut self, binders: &[&str]) -> Result<Pattern, String> {
        let Some(word) = self.word(true) else {
            return Err(self.error("a type"));
        };
        if word == "_" {
            return Ok(Pattern::Wild);
        }
        if let Ok(n) = word.parse::<u64>() {
            return Ok(Pattern::Nat(n));
        }
        if binders.contains(&word.as_str()) {
            return Ok(Pattern::Binder(word));
        }
        let mut args = Vec::new();
        if self.eat("<") {
            loop {
                args.push(self.pattern(binders)?);
                if self.eat(">") {
                    break;
                }
                self.expect(",")?;
            }
        }
        Ok(Pattern::Ctor(word, args))
    }
}

/// One operand or result position, its constructors looked up.
#[derive(Clone, Debug)]
pub struct Slot {
    /// The type of every value in the slot; binders and `_`s are
    /// [`TypeArg::Var`]s.
    pub pattern: TypeArg,
    pub arity: Arity,
}

/// A scheme with its constructors looked up.
#[derive(Clone, Debug)]
pub struct Scheme {
    // The binders, then a `_` for each wildcard.
    names: Vec<String>,
    // Per binder, the shapes it may take; empty for any.
    bounds: Vec<Vec<TypeArg>>,
    pub operands: Vec<Slot>,
    pub results: Vec<Slot>,
}

impl Scheme {
    /// The variables' names, indexed by [`TypeArg::Var`].
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The shapes binder `var` may take, one of which its type has; empty
    /// for any type.
    pub fn bound(&self, var: u32) -> &[TypeArg] {
        self.bounds.get(var as usize).map_or(&[], Vec::as_slice)
    }

    /// The operand slots, then the result slots.
    pub fn slots(&self) -> impl Iterator<Item = &Slot> {
        self.operands.iter().chain(&self.results)
    }
}
