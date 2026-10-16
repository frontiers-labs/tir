use std::collections::HashMap;
use std::sync::Arc;

pub use tir_symbolic::term::{
    CtorId, Kind, Name, Param, Terms, TypeArg, TypeCtor, TypeId, TypeTerm,
};

use crate::{
    Context, Error, IRFormatter,
    parse::{Span, text::Parser as IRParser},
};

pub type TypeParser =
    for<'src> fn(&str, &mut IRParser<'src>, &Context) -> Result<TypeId, (Span, Error)>;

/// Writes a term of one constructor after its `!`, the dialect included unless
/// it is `builtin`. Children print through the [`Spellings`] it is handed.
pub type TypePrinter = fn(&Spellings, TypeId, &mut IRFormatter<'_>) -> std::fmt::Result;

/// The short spelling of one constructor's terms. A constructor without one
/// is written `!dialect.name<args>`.
#[derive(Clone, Copy)]
pub struct TypeSpelling {
    pub ctor: CtorId,
    /// The mnemonics after `!` and the dialect the parser dispatches on. A
    /// mnemonic followed by digits (`i32`, `bf16`) is found by its letters.
    pub keys: &'static [&'static str],
    pub parse: TypeParser,
    pub print: TypePrinter,
    /// The type stands for "any number of further arguments" when it ends a
    /// function signature.
    pub variadic_tail: bool,
}

/// A constructor a dialect defines. Each store numbers it by when it was first
/// registered there, so its id is looked up by name in the store at hand.
pub struct Ctor {
    info: TypeCtor,
}

impl Ctor {
    pub const fn new(info: TypeCtor) -> Self {
        Self { info }
    }

    /// The id of this constructor in `terms`, registered on first use.
    pub fn id(&'static self, terms: &impl AsRef<Terms>) -> CtorId {
        terms.as_ref().register(&self.info)
    }

    /// What registers `ctor` with no short spelling: its terms are written
    /// `!dialect.name<args>`.
    pub fn spelling(context: &Context, ctor: &'static Ctor) -> TypeSpelling {
        TypeSpelling {
            ctor: ctor.id(context),
            keys: &[],
            parse: |_, parser, _| {
                Err((
                    crate::parse::common::Cursor::span(parser),
                    Error::ExpectedType,
                ))
            },
            print: Spellings::print_generic,
            variadic_tail: false,
        }
    }

    /// Whether `ty` is a term of this constructor.
    pub fn is(&'static self, terms: &impl AsRef<Terms>, ty: TypeId) -> bool {
        let terms = terms.as_ref();
        let info = terms.ctor_info(terms.ctor(ty));
        std::ptr::eq(info, &self.info)
            || (info.dialect == self.info.dialect && info.name == self.info.name)
    }
}

/// The spellings a context parses and prints types with.
pub struct Spellings {
    terms: Arc<Terms>,
    printers: HashMap<CtorId, TypePrinter>,
    variadic_tails: Vec<CtorId>,
    parsers: HashMap<&'static str, HashMap<&'static str, TypeParser>>,
}

impl AsRef<Terms> for Spellings {
    fn as_ref(&self) -> &Terms {
        &self.terms
    }
}

impl Spellings {
    pub(crate) fn new(terms: Arc<Terms>) -> Self {
        Self {
            terms,
            printers: HashMap::new(),
            variadic_tails: Vec::new(),
            parsers: HashMap::new(),
        }
    }

    /// The store the spelled types are interned in.
    pub fn terms(&self) -> &Terms {
        &self.terms
    }

    pub(crate) fn register(&mut self, dialect: &'static str, spelling: &TypeSpelling) {
        self.printers.insert(spelling.ctor, spelling.print);
        if spelling.variadic_tail && !self.variadic_tails.contains(&spelling.ctor) {
            self.variadic_tails.push(spelling.ctor);
        }
        for key in spelling.keys {
            self.register_parser(dialect, key, spelling.parse);
        }
    }

    pub(crate) fn register_parser(
        &mut self,
        dialect: &'static str,
        key: &'static str,
        parser: TypeParser,
    ) {
        self.parsers.entry(dialect).or_default().insert(key, parser);
    }

    pub(crate) fn parser(&self, dialect: &str, name: &str) -> Option<TypeParser> {
        let keys = self.parsers.get(dialect)?;
        if let Some(parser) = keys.get(name) {
            return Some(*parser);
        }
        let prefix = name
            .find(|c: char| !c.is_ascii_alphabetic() && c != '_')
            .map(|end| &name[..end])?;
        keys.get(prefix).copied()
    }

    pub(crate) fn is_variadic_tail(&self, ty: TypeId) -> bool {
        self.variadic_tails.contains(&self.terms.ctor(ty))
    }

    /// `ty` as the IR spells it, `!` first.
    pub fn print(&self, ty: TypeId, fmt: &mut IRFormatter<'_>) -> std::fmt::Result {
        fmt.write("!")?;
        self.print_body(ty, fmt)
    }

    /// `ty` as the IR spells it after its `!`.
    pub fn print_body(&self, ty: TypeId, fmt: &mut IRFormatter<'_>) -> std::fmt::Result {
        match self.printers.get(&self.terms.ctor(ty)) {
            Some(print) => print(self, ty, fmt),
            None => self.print_generic(ty, fmt),
        }
    }

    /// `ty` as `dialect.name<args>`, children in their own spellings.
    pub fn print_generic(&self, ty: TypeId, fmt: &mut IRFormatter<'_>) -> std::fmt::Result {
        let term = self.terms.term(ty);
        let info = self.terms.ctor_info(term.ctor());
        fmt.write(format!("{}.{}", info.dialect, info.name))?;
        if term.args().is_empty() {
            return Ok(());
        }
        fmt.write("<")?;
        self.print_args(term.args(), fmt)?;
        fmt.write(">")
    }

    fn print_args(&self, args: &[TypeArg], fmt: &mut IRFormatter<'_>) -> std::fmt::Result {
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                fmt.write(", ")?;
            }
            self.print_arg(arg, fmt)?;
        }
        Ok(())
    }

    /// One argument of a term: a child in its own spelling, a bound variable
    /// by the name its `type.def` gives it.
    pub fn print_arg(&self, arg: &TypeArg, fmt: &mut IRFormatter<'_>) -> std::fmt::Result {
        match arg {
            TypeArg::Type(child) => self.print(*child, fmt),
            TypeArg::List(items) => {
                fmt.write("[")?;
                self.print_args(items, fmt)?;
                fmt.write("]")
            }
            TypeArg::Var(index) => {
                let name = fmt.type_param(*index).map(str::to_string);
                fmt.write(name.unwrap_or_else(|| self.terms.display_arg(arg)))
            }
            other => fmt.write(self.terms.display_arg(other)),
        }
    }
}

pub(crate) fn parse_generic(
    ctor: CtorId,
    parser: &mut IRParser<'_>,
    context: &Context,
) -> Result<TypeId, (Span, Error)> {
    use crate::parse::common::Cursor;
    let args = parser
        .parse_delimited("<", ">", |parser| parse_generic_arg(parser, context))?
        .unwrap_or_default();
    context
        .terms()
        .try_intern(ctor, args.into())
        .map_err(|message| (parser.span(), Error::VerificationError(message)))
}

fn parse_generic_arg(
    parser: &mut IRParser<'_>,
    context: &Context,
) -> Result<TypeArg, (Span, Error)> {
    use crate::parse::common::Cursor;
    if let Some(arg) = parser.parse_type_arg(context)? {
        return Ok(arg);
    }
    if let Some(items) =
        parser.parse_delimited("[", "]", |parser| parse_generic_arg(parser, context))?
    {
        return Ok(TypeArg::List(items.into()));
    }
    let mut name = parser
        .parse_ident()
        .ok_or_else(|| (parser.span(), Error::ExpectedType))?
        .to_string();
    while parser.parse_token(".") {
        let part = parser
            .parse_ident()
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        name = format!("{name}.{part}");
    }
    Ok(TypeArg::Sym(context.terms().name(&name)))
}
