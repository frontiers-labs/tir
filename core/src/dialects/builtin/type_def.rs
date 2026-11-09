//! Nominal types. `type.def !Name = !body` names a type in a module, and
//! `!Name` is then the term `type.named<Name, []>`: its identity is the name,
//! never the body. The body is read only where a layout is computed.
//!
//! The constructors a body is built from, `!struct`, `!union` and `!variant`,
//! hold `(name, type)` fields. Their layouts are regions of their core
//! definitions (`dialects/layout/types.rs`, see [`crate::layout`]).

use std::collections::BTreeMap;

use tir_symbolic::term::{ctor, no_carrier, valid};

use crate::attributes::AttributeValue;
use crate::parse::common::Cursor;
use crate::ty::{Ctor, Terms, TypeArg, TypeSpelling};
use crate::{Context, Error, IRFormatter, Symbol, TypeId, Visibility, dialect};

use crate as tir;

dialect! {
    TypeDialect {
        name: "type",
        operations: [DEF],
        types: [],
    }
}

static STRUCT: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_builtin_type_def.cdsl", binding: constructors::STRUCT, hosts: { STRUCT_validate => valid, STRUCT_carrier => no_carrier } },
);

static UNION: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_builtin_type_def.cdsl", binding: constructors::UNION, hosts: { UNION_validate => valid, UNION_carrier => no_carrier } },
);

static VARIANT: Ctor = Ctor::new(
    ::tir::helpers::type_ctor! { file: "defs/types/core_src_dialects_builtin_type_def.cdsl", binding: constructors::VARIANT, hosts: { VARIANT_validate => valid, VARIANT_carrier => no_carrier } },
);

/// `!Name` or `!Name<args>`: the type a `type.def` of the enclosing module
/// declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedType {
    name: String,
    args: Vec<TypeArg>,
}

impl NamedType {
    /// The ground type `!name`.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, name: &str) -> TypeId {
        Self::get(context, name, vec![]).expect("a named type")
    }

    /// `!name<args>`.
    pub fn get(
        terms: &impl AsRef<Terms>,
        name: &str,
        args: Vec<TypeArg>,
    ) -> Result<TypeId, String> {
        let terms = terms.as_ref();
        let name = TypeArg::Sym(terms.name(name));
        terms.intern(ctor::NAMED, vec![name, TypeArg::List(args.into())])
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        if terms.ctor(ty) != ctor::NAMED {
            return None;
        }
        let term = terms.term(ty);
        let [TypeArg::Sym(name), TypeArg::List(args)] = term.args() else {
            return None;
        };
        Some(Self {
            name: terms.name_text(*name),
            args: args.to_vec(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn args(&self) -> &[TypeArg] {
        &self.args
    }

    pub fn spelling(_context: &Context) -> TypeSpelling {
        {
            const __TIR_TYPE_PARSE: ::tir::TypeParser =
                |_, parser, _| Err((parser.span(), Error::ExpectedType));
            const __TIR_TYPE_PRINT: ::tir::TypePrinter = |spellings, ty, fmt| {
                let named = NamedType::view(spellings, ty).expect("a named type");
                fmt.write(&named.name)?;
                if named.args.is_empty() {
                    return Ok(());
                }
                fmt.write("<")?;
                for (index, arg) in named.args.iter().enumerate() {
                    if index > 0 {
                        fmt.write(", ")?;
                    }
                    spellings.print_arg(arg, fmt)?;
                }
                fmt.write(">")
            };
            const __TIR_TYPE_CTOR_ID: fn(&::tir::Context) -> ::tir::CtorId = |_context| ctor::NAMED;
            ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_builtin_type_def.cdsl", binding: spellings::NamedType, context: _context, hosts: { NamedType_parse => __TIR_TYPE_PARSE, NamedType_print => __TIR_TYPE_PRINT, NamedType_ctor_id => __TIR_TYPE_CTOR_ID } }
        }
    }
}

/// Which aggregate constructor a term is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregateKind {
    Struct,
    Union,
    Variant,
}

impl AggregateKind {
    fn ctor(self) -> &'static Ctor {
        match self {
            Self::Struct => &STRUCT,
            Self::Union => &UNION,
            Self::Variant => &VARIANT,
        }
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Struct => &["struct"],
            Self::Union => &["union"],
            Self::Variant => &["variant"],
        }
    }
}

/// `!struct<{a: !i32, b: !f64}>`, `!union<{..}>` or `!variant<{..}>`, the
/// variant optionally with its tag type: `!variant<{..}, !i8>`, a struct
/// with every field at byte alignment `!struct<{..}, true>`. A field's type
/// is a bound variable inside a generic definition. An argument equal to the
/// default the core type definitions give it is left out of the term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregateType {
    kind: AggregateKind,
    fields: Vec<(String, TypeArg)>,
}

impl AggregateType {
    pub fn get(
        terms: &impl AsRef<Terms>,
        kind: AggregateKind,
        fields: &[(&str, TypeArg)],
    ) -> Result<TypeId, String> {
        let terms = terms.as_ref();
        let fields = fields
            .iter()
            .map(|(name, ty)| TypeArg::List(Box::new([TypeArg::Sym(terms.name(name)), ty.clone()])))
            .collect();
        terms.intern(kind.ctor().id(terms), vec![TypeArg::List(fields)])
    }

    pub fn view(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<Self> {
        let terms = terms.as_ref();
        let kind = [
            AggregateKind::Struct,
            AggregateKind::Union,
            AggregateKind::Variant,
        ]
        .into_iter()
        .find(|kind| kind.ctor().is(terms, ty))?;
        let term = terms.term(ty);
        let TypeArg::List(fields) = term.args().first()? else {
            return None;
        };
        let fields = fields
            .iter()
            .map(|field| match field {
                TypeArg::List(pair) => match &**pair {
                    [TypeArg::Sym(name), ty] => Some((terms.name_text(*name), ty.clone())),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Option<_>>()?;
        Some(Self { kind, fields })
    }

    /// The struct `ty` with every field at byte alignment.
    pub fn packed(terms: &impl AsRef<Terms>, ty: TypeId) -> Option<TypeId> {
        let terms = terms.as_ref();
        if !STRUCT.is(terms, ty) {
            return None;
        }
        let fields = terms.term(ty).args().first()?.clone();
        terms
            .intern(STRUCT.id(terms), vec![fields, TypeArg::Nat(1)])
            .ok()
    }

    pub fn kind(&self) -> AggregateKind {
        self.kind
    }

    pub fn fields(&self) -> &[(String, TypeArg)] {
        &self.fields
    }

    /// The position of the field called `name`.
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|(field, _)| field == name)
    }

    pub fn spelling(context: &Context, kind: AggregateKind) -> TypeSpelling {
        {
            const __TIR_TYPE_PARSE: ::tir::TypeParser = |mnemonic, parser, context| {
                let kind = match mnemonic {
                    "struct" => AggregateKind::Struct,
                    "union" => AggregateKind::Union,
                    _ => AggregateKind::Variant,
                };
                if !parser.parse_token("<") {
                    return Err((parser.span(), Error::ExpectedToken("<")));
                }
                // A generic definition's parameter may stand for the whole list.
                let fields = match parser.parse_type_arg(context)? {
                    Some(fields @ TypeArg::Var(_)) => fields,
                    _ => TypeArg::List(
                        parser
                            .parse_delimited("{", "}", |parser| {
                                let name = parser
                                    .parse_ident()
                                    .or_else(|| parser.parse_string())
                                    .ok_or_else(|| {
                                    (parser.span(), Error::ExpectedToken("field name"))
                                })?;
                                let name = TypeArg::Sym(context.terms().name(name));
                                if !parser.parse_token(":") {
                                    return Err((parser.span(), Error::ExpectedToken(":")));
                                }
                                let ty = parser
                                    .parse_type_arg(context)?
                                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                                Ok(TypeArg::List(Box::new([name, ty])))
                            })?
                            .ok_or_else(|| (parser.span(), Error::ExpectedToken("{")))?
                            .into(),
                    ),
                };
                let mut args = vec![fields];
                if kind != AggregateKind::Union && parser.parse_token(",") {
                    let flag = [("false", 0), ("true", 1)]
                        .into_iter()
                        .find(|(word, _)| parser.parse_token(word));
                    let arg = match flag {
                        Some((_, bit)) => TypeArg::Nat(bit),
                        None => parser
                            .parse_type_arg(context)?
                            .ok_or_else(|| (parser.span(), Error::ExpectedType))?,
                    };
                    args.push(arg);
                }
                if !parser.parse_token(">") {
                    return Err((parser.span(), Error::ExpectedToken(">")));
                }
                context
                    .terms()
                    .intern(kind.ctor().id(context), args)
                    .map_err(|message| (parser.span(), Error::VerificationError(message)))
            };
            const __TIR_TYPE_PRINT: ::tir::TypePrinter = |spellings, ty, fmt| {
                let Some(aggregate) = AggregateType::view(spellings, ty) else {
                    return spellings.print_generic(ty, fmt);
                };
                fmt.write(format!("{}<{{", aggregate.kind.keys()[0]))?;
                for (index, (name, field)) in aggregate.fields.iter().enumerate() {
                    if index > 0 {
                        fmt.write(", ")?;
                    }
                    // A name that is no identifier, as C's `_x`, is quoted.
                    match is_identifier(name) {
                        true => fmt.write(format!("{name}: "))?,
                        false => fmt.write(format!("\"{name}\": "))?,
                    }
                    spellings.print_arg(field, fmt)?;
                }
                fmt.write("}")?;
                for arg in &spellings.terms().term(ty).args()[1..] {
                    fmt.write(", ")?;
                    match (aggregate.kind, arg) {
                        (AggregateKind::Struct, TypeArg::Nat(bit)) => fmt.write(match bit {
                            0 => "false",
                            _ => "true",
                        })?,
                        _ => spellings.print_arg(arg, fmt)?,
                    }
                }
                fmt.write(">")
            };
            const __TIR_TYPE_CTOR_ID_STRUCT: fn(&::tir::Context) -> ::tir::CtorId =
                |context| STRUCT.id(context);
            const __TIR_TYPE_CTOR_ID_UNION: fn(&::tir::Context) -> ::tir::CtorId =
                |context| UNION.id(context);
            const __TIR_TYPE_CTOR_ID_VARIANT: fn(&::tir::Context) -> ::tir::CtorId =
                |context| VARIANT.id(context);
            match kind {
                AggregateKind::Struct => {
                    ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_builtin_type_def.cdsl", binding: spellings::AggregateType_Struct, context: context, hosts: { AggregateType_parse => __TIR_TYPE_PARSE, AggregateType_print => __TIR_TYPE_PRINT, AggregateType_Struct_ctor_id => __TIR_TYPE_CTOR_ID_STRUCT } }
                }
                AggregateKind::Union => {
                    ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_builtin_type_def.cdsl", binding: spellings::AggregateType_Union, context: context, hosts: { AggregateType_parse => __TIR_TYPE_PARSE, AggregateType_print => __TIR_TYPE_PRINT, AggregateType_Union_ctor_id => __TIR_TYPE_CTOR_ID_UNION } }
                }
                AggregateKind::Variant => {
                    ::tir::helpers::type_spelling! { file: "defs/types/core_src_dialects_builtin_type_def.cdsl", binding: spellings::AggregateType_Variant, context: context, hosts: { AggregateType_parse => __TIR_TYPE_PARSE, AggregateType_print => __TIR_TYPE_PRINT, AggregateType_Variant_ctor_id => __TIR_TYPE_CTOR_ID_VARIANT } }
                }
            }
        }
    }
}

fn is_identifier(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_alphabetic)
        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

// Every type `ty` holds by value, `ty` first, until `visit` says stop: an
// aggregate's fields, a tuple's or a vector's elements, and the body `expand`
// gives a named type. A pointer holds its pointee by reference, so the walk
// does not enter it.
pub(crate) fn contained_by_value(
    terms: &Terms,
    ty: TypeId,
    expand: impl Fn(TypeId) -> Option<TypeId>,
    mut visit: impl FnMut(TypeId) -> bool,
) {
    let mut stack = vec![ty];
    let mut seen = std::collections::HashSet::new();
    while let Some(ty) = stack.pop() {
        if !seen.insert(ty) || !visit(ty) || crate::ptr::PtrType::view(terms, ty).is_some() {
            continue;
        }
        if terms.ctor(ty) == ctor::NAMED {
            stack.extend(expand(ty));
            continue;
        }
        let term = terms.term(ty);
        let mut args: Vec<&TypeArg> = term.args().iter().collect();
        while let Some(arg) = args.pop() {
            match arg {
                TypeArg::Type(child) => stack.push(*child),
                TypeArg::List(items) => args.extend(items.iter()),
                TypeArg::Lam(_, body) => args.push(body),
                _ => {}
            }
        }
    }
}

// A parameter default as the term argument it stands for.
pub(crate) fn type_arg_of(value: &AttributeValue) -> Option<TypeArg> {
    match value {
        AttributeValue::Type(ty) => Some(TypeArg::Type(*ty)),
        AttributeValue::Bool(flag) => Some(TypeArg::Nat(u64::from(*flag))),
        value => Some(TypeArg::Nat(u64::try_from(value.as_int()?).ok()?)),
    }
}

/// Replace each bound variable `i` of `arg` by `values[i]`.
pub fn substitute(
    terms: &impl AsRef<Terms>,
    arg: &TypeArg,
    values: &[TypeArg],
) -> Result<TypeArg, String> {
    let terms = terms.as_ref();
    Ok(match arg {
        TypeArg::Var(index) => values
            .get(*index as usize)
            .cloned()
            .ok_or_else(|| format!("type argument {index} is missing"))?,
        TypeArg::Type(id) => {
            let term = terms.term(*id);
            let args = term
                .args()
                .iter()
                .map(|arg| substitute(terms, arg, values))
                .collect::<Result<Vec<_>, _>>()?;
            if args == term.args() {
                return Ok(arg.clone());
            }
            TypeArg::Type(terms.intern(term.ctor(), args)?)
        }
        TypeArg::List(items) => TypeArg::List(
            items
                .iter()
                .map(|item| substitute(terms, item, values))
                .collect::<Result<_, _>>()?,
        ),
        other => other.clone(),
    })
}

// What a layout region takes for each kind of parameter: a field list as
// the field types, a type, a number, a flag.
const PARAM_KINDS: [&str; 4] = ["fields", "data", "nat", "bool"];

// `type.def !Name<params> layout {packed} = !body layout (%spec: ..) {..}`.
//
// A definition with no body is incomplete: a pointer can point at it, nothing
// can hold it. The core type definitions, named after the
// aggregate constructors, give their layout regions.
tir::helpers::op_def! {
    file: "defs/converted/dialects_builtin_type_def.cdsl",
    hosts: {
        def_verifier => crate::builtin::def_verify_impl,
        def_iface_Symbol => (Symbol, &crate::builtin::__TIR_IFACE_TypeDefOp_Symbol),
        def_printer => crate::builtin::def_custom_print,
        def_parser => |parser, context, _definition| crate::builtin::def_custom_parse(parser, context),
    },
}

/// A `type.def` of `name` with `body`, or an incomplete one.
pub fn type_def(
    context: &Context,
    name: &str,
    body: Option<TypeId>,
    mut parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> crate::OpHandle {
    if let Some(body) = body {
        define(context, name, body);
        parts.attributes.insert(
            0,
            context.named_attribute("body", AttributeValue::Type(body)),
        );
    }
    r#def(context, name, vec![], parts, insertion)
}

pub(crate) fn define(context: &Context, name: &str, body: TypeId) {
    let terms = context.terms();
    terms.define(terms.name(name), terms.kind(body));
}

pub(crate) fn def_symbol_symbol_name(operation: &tir::OpHandle) -> String {
    format!("!{}", crate::builtin::def_sym_name(operation))
}

pub(crate) fn def_symbol_symbol_signature(___operation: &tir::OpHandle) -> Option<Vec<TypeId>> {
    None
}

pub(crate) fn def_symbol_symbol_result_type(___operation: &tir::OpHandle) -> Option<TypeId> {
    None
}

pub(crate) fn def_symbol_symbol_visibility(___operation: &tir::OpHandle) -> Visibility {
    Visibility::Public
}

pub(crate) fn def_symbol_is_definition(operation: &tir::OpHandle) -> bool {
    crate::builtin::def_body(operation).is_some()
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_TypeDefOp_Symbol: crate::SymbolCallbacks = crate::SymbolCallbacks {
    symbol_name: crate::builtin::def_symbol_symbol_name,
    symbol_signature: crate::builtin::def_symbol_symbol_signature,
    symbol_result_type: crate::builtin::def_symbol_symbol_result_type,
    symbol_visibility: crate::builtin::def_symbol_symbol_visibility,
    is_definition: crate::builtin::def_symbol_is_definition,
};

/// The generic parameters as `(name, kind)`.
pub fn def_params(operation: &tir::OpHandle) -> Vec<(String, String)> {
    let Some(AttributeValue::Array(params)) = operation.attr("params") else {
        return vec![];
    };
    params
        .iter()
        .filter_map(|param| match param {
            AttributeValue::Array(param) => match &**param {
                [AttributeValue::Str(name), AttributeValue::Str(kind), ..] => {
                    Some((name.to_string(), kind.to_string()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The default of parameter `index`, for a constructor's optional one.
pub fn def_param_default(operation: &tir::OpHandle, index: usize) -> Option<AttributeValue> {
    let Some(AttributeValue::Array(params)) = operation.attr("params") else {
        return None;
    };
    match params.get(index)? {
        AttributeValue::Array(param) => param.get(2).cloned(),
        _ => None,
    }
}

/// The `layout {..}` options.
pub fn def_layout_options(operation: &tir::OpHandle) -> BTreeMap<String, AttributeValue> {
    match operation.attr("layout") {
        Some(AttributeValue::Dict(options)) => *options,
        _ => BTreeMap::new(),
    }
}

pub fn def_packed(operation: &tir::OpHandle) -> bool {
    crate::builtin::def_layout_options(operation).get("packed") == Some(&AttributeValue::Bool(true))
}

/// The alignment the options raise the definition to, in bits.
pub fn def_align(operation: &tir::OpHandle) -> Option<u64> {
    crate::builtin::def_layout_options(operation)
        .get("align")?
        .as_int()?
        .try_into()
        .ok()
}

/// The region computing the layout, when the definition has its own.
pub fn def_layout_region(operation: &tir::OpHandle) -> Option<tir::RegionId> {
    operation.regions().first().copied()
}

/// The body with `args` for the parameters: what `!Name<args>` stands for.
pub fn def_expand(operation: &tir::OpHandle, args: &[TypeArg]) -> Option<TypeId> {
    let body = crate::builtin::def_body(operation)?;
    if args.is_empty() {
        return Some(body);
    }
    substitute(&operation.context, &TypeArg::Type(body), args)
        .ok()?
        .as_type()
}

pub(crate) fn def_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    fmt.write(format!(
        "type.def !{}",
        crate::builtin::def_sym_name(operation)
    ))?;
    let params = crate::builtin::def_params(operation);
    if !params.is_empty() {
        fmt.write("<")?;
        for (index, (name, kind)) in params.iter().enumerate() {
            if index > 0 {
                fmt.write(", ")?;
            }
            fmt.write(format!("{name}: {kind}"))?;
            if let Some(default) = crate::builtin::def_param_default(operation, index) {
                fmt.write(" = ")?;
                default.print(fmt, &context)?;
            }
        }
        fmt.write(">")?;
    }
    let options = crate::builtin::def_layout_options(operation);
    if !options.is_empty() {
        fmt.write(" layout {")?;
        for (index, (key, value)) in options.iter().enumerate() {
            if index > 0 {
                fmt.write(", ")?;
            }
            fmt.write(key)?;
            if *value != AttributeValue::Bool(true) {
                fmt.write(" = ")?;
                value.print(fmt, &context)?;
            }
        }
        fmt.write("}")?;
    }
    fmt.set_type_params(params.into_iter().map(|(name, _)| name).collect());
    if let Some(body) = crate::builtin::def_body(operation) {
        fmt.write(" = ")?;
        context.print_type(body, fmt)?;
    }
    fmt.set_type_params(vec![]);
    let Some(region) = crate::builtin::def_layout_region(operation) else {
        return fmt.write("\n");
    };
    fmt.write(" layout (")?;
    let arguments = context.get_region(region).value_arguments();
    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            fmt.write(", ")?;
        }
        fmt.write(format!("%{}: ", argument.id().number()))?;
        context.print_type(argument.ty(), fmt)?;
    }
    fmt.write(")")?;
    tir::region_format::print_op_region(fmt, &context, operation, 0)
}

pub(crate) fn def_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    if !parser.parse_token("!") {
        return Err((parser.span(), Error::ExpectedToken("!")));
    }
    let name = parser
        .parse_ident()
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("type name")))?
        .to_string();
    let name = parser.parse_renamed(&name).unwrap_or(name);
    let params = parser
        .parse_delimited("<", ">", |parser| {
            let name = parser
                .parse_ident()
                .ok_or_else(|| (parser.span(), Error::ExpectedToken("parameter name")))?
                .to_string();
            if !parser.parse_token(":") {
                return Err((parser.span(), Error::ExpectedToken(":")));
            }
            let kind = parser
                .parse_ident()
                .ok_or_else(|| (parser.span(), Error::ExpectedToken("parameter kind")))?
                .to_string();
            let default = match parser.parse_token("=") {
                true => Some(
                    parser
                        .parse_attribute_value(context)?
                        .ok_or_else(|| (parser.span(), Error::ExpectedToken("a default")))?,
                ),
                false => None,
            };
            Ok((name, kind, default))
        })?
        .unwrap_or_default();
    parser.type_defs.insert(name.clone());
    let mut parts = tir::NewOpParts::default();
    let mut regions = vec![];
    if !params.is_empty() {
        let params: Vec<AttributeValue> = params
            .iter()
            .map(|(name, kind, default)| {
                let mut param = vec![
                    AttributeValue::from(name.as_str()),
                    AttributeValue::from(kind.as_str()),
                ];
                param.extend(default.clone());
                AttributeValue::Array(param.into())
            })
            .collect();
        parts
            .attributes
            .push(context.named_attribute("params", AttributeValue::Array(params.into())));
    }

    let mark = parser.pos();
    if parser.parse_token("layout")
        && let Some(options) = parser.parse_delimited("{", "}", |parser| {
            let key = parser
                .parse_ident()
                .ok_or_else(|| (parser.span(), Error::ExpectedToken("layout option")))?
                .to_string();
            let value = match parser.parse_token("=") {
                true => parser
                    .parse_attribute_value(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedToken("attribute")))?,
                false => AttributeValue::Bool(true),
            };
            Ok((key, value))
        })?
    {
        parts.attributes.push(context.named_attribute(
            "layout",
            AttributeValue::Dict(Box::new(options.into_iter().collect())),
        ));
    } else {
        parser.set_pos(mark);
    }

    let enclosing = std::mem::replace(
        &mut parser.type_params,
        params.into_iter().map(|(name, ..)| name).collect(),
    );
    let body = match parser.parse_token("=") {
        true => Some(
            parser
                .parse_type(context)
                .and_then(|body| body.ok_or_else(|| (parser.span(), Error::ExpectedType))),
        ),
        false => None,
    };
    parser.type_params = enclosing;
    if let Some(body) = body {
        let body = body?;
        define(context, &name, body);
        parts
            .attributes
            .push(context.named_attribute("body", AttributeValue::Type(body)));
    }

    if parser.parse_token("layout") {
        let arguments = parser
            .parse_delimited("(", ")", |parser| {
                let name = parser
                    .parse_value_ref()
                    .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
                    .to_string();
                if !parser.parse_token(":") {
                    return Err((parser.span(), Error::ExpectedToken(":")));
                }
                let ty = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
                let value = context.create_value(ty, None);
                parser.define_value(&name, value.id());
                Ok(value)
            })?
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("(")))?;
        let region = parser.parse_region_with_entry_args(context, arguments)?;
        regions.push(region.id());
    }
    Ok(r#def(
        context,
        name.as_str(),
        regions,
        parts,
        tir::InsertionPoint::Root,
    ))
}

/// Whether `name` spells a type the core defines, which no other
/// `type.def` may take.
pub fn def_is_core_type_name(context: &Context, name: &str) -> bool {
    let terms = context.terms();
    context.get_type_parser("builtin", name).is_ok()
        || terms.ctor_named("builtin", name).is_some()
        || terms.ctor_named("type", name).is_some()
        || context
            .core_type_names()
            .is_some_and(|names| names.contains(&name))
}

pub(crate) fn def_verify_impl(operation: &tir::OpHandle, context: &Context) -> Result<(), Error> {
    let name = crate::builtin::def_sym_name(operation);
    let holder = context.parent_op(operation.id);
    if let Some(holder) = holder
        && !context.get_op(holder).is_operation(&crate::builtin::MODULE)
    {
        let inside = enclosing_symbol(context, holder).map_or_else(
            || "a nested region".to_string(),
            |name| format!("function `@{name}`"),
        );
        return Err(Error::VerificationError(format!(
            "type definition `!{name}` inside {inside}; move it to module scope"
        )));
    }
    // The core type definitions define the constructors, so only there is a
    // definition spelled like one.
    if crate::builtin::def_is_core_type_name(context, &name) && !context.defines_types() {
        return Err(Error::VerificationError(format!(
            "type name `!{name}` is spelled like a builtin type"
        )));
    }
    if let Some((param, kind)) = crate::builtin::def_params(operation)
        .into_iter()
        .find(|(_, kind)| !PARAM_KINDS.contains(&kind.as_str()))
    {
        return Err(Error::VerificationError(format!(
            "parameter `{param}` of `!{name}` has unknown kind `{kind}` (expected one of {})",
            PARAM_KINDS.join(", ")
        )));
    }
    Ok(())
}

fn enclosing_symbol(context: &Context, op: tir::OpId) -> Option<String> {
    let mut current = Some(op);
    while let Some(id) = current {
        if let Some(symbol) = context.get_op(id).as_interface::<dyn Symbol>() {
            return Some(symbol.symbol_name());
        }
        current = context.parent_op(id);
    }
    None
}
