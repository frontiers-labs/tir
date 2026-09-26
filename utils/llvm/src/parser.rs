//! Parser from an LLVM textual-IR token stream into [`ast`], built with
//! `chumsky`. The parser reads definitions, declarations, globals, and named
//! types from one token stream. Unrecognised top-level lines are skipped; each
//! recognised instruction is parsed into a typed node, and anything else on an
//! instruction line becomes [`ast::Inst::Unsupported`] so conversion can report
//! precisely what it cannot lower.

use chumsky::input::ValueInput;
use chumsky::prelude::*;

use crate::ast::*;
use crate::error::Error;
use crate::lexer::{Span, Token, lex};

/// Attribute, linkage and flag keywords that decorate instructions and
/// signatures. Individual instruction parsers retain the flags they support;
/// the rest are skipped. None collide with a type, opcode or operand keyword.
const SKIP: &[&str] = &[
    "nsw",
    "nuw",
    "exact",
    "disjoint",
    "fast",
    "volatile",
    "inbounds",
    "dso_local",
    "dso_preemptable",
    "local_unnamed_addr",
    "unnamed_addr",
    "internal",
    "external",
    "private",
    "weak",
    "weak_odr",
    "linkonce",
    "linkonce_odr",
    "hidden",
    "protected",
    "noundef",
    "signext",
    "zeroext",
    "inreg",
    "nonnull",
    "noalias",
    "nocapture",
    "readonly",
    "readnone",
    "returned",
    "writeonly",
    "dead_on_unwind",
    "writable",
    "fastcc",
    "coldcc",
    "tailcc",
];

#[derive(Clone)]
enum ParsedAbiAttr {
    SRet(Type),
    ByVal(Type),
    Align(u64),
    Other,
}

fn abi_attrs(attrs: Vec<ParsedAbiAttr>) -> AbiAttrs {
    let mut result = AbiAttrs::default();
    for attr in attrs {
        match attr {
            ParsedAbiAttr::SRet(ty) => result.sret = Some(ty),
            ParsedAbiAttr::ByVal(ty) => result.byval = Some(ty),
            ParsedAbiAttr::Align(align) => result.align = Some(align),
            ParsedAbiAttr::Other => {}
        }
    }
    result
}

pub fn parse_module(src: &str) -> Result<Module, Error> {
    let normalized = normalize_constant_geps(&normalize_switches(&normalize_attributes(src))?);
    let tokens = lex(&normalized);
    let eoi = Span::from(normalized.len()..normalized.len());
    let input = tokens.as_slice().map(eoi, |(t, s)| (t, s));

    let (out, errors) = module().parse(input).into_output_errors();
    match out {
        Some(Ok(module)) if errors.is_empty() => Ok(module),
        Some(Err(error)) if errors.is_empty() => Err(error),
        _ => Err(Error::Parse(
            errors
                .iter()
                .map(|e| format!("{e:?}"))
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}

fn normalize_attributes(src: &str) -> String {
    const NAMES: &[&str] = &[
        "range",
        "captures",
        "initializes",
        "dereferenceable",
        "dereferenceable_or_null",
    ];
    let mut text = src.to_string();
    loop {
        let found = NAMES
            .iter()
            .filter_map(|name| {
                find_unquoted(&text, &format!("{name}(")).map(|index| (index, *name))
            })
            .min_by_key(|(index, _)| *index);
        let Some((start, name)) = found else {
            break;
        };
        let open = start + name.len();
        let Some(close) = matching_paren(&text, open) else {
            break;
        };
        text.replace_range(start..=close, "");
    }
    text
}

fn find_unquoted(text: &str, needle: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && character == '\\' {
            escaped = true;
            continue;
        }
        if character == '"' {
            quoted = !quoted;
        } else if !quoted && text[index..].starts_with(needle) {
            return Some(index);
        }
    }
    None
}

fn normalize_switches(src: &str) -> Result<String, Error> {
    let lines = src.lines().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    let mut next_switch = 0;
    let mut current_label = "0".to_string();
    let mut signature = String::new();
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if !trimmed.starts_with("switch ") {
            if trimmed.starts_with("define ") {
                signature.clear();
            }
            if trimmed.starts_with("define ") || !signature.is_empty() {
                signature.push_str(trimmed);
                if let Some(label) = implicit_entry_label(&signature) {
                    current_label = label;
                    signature.clear();
                }
            }
            if let Some((label, _)) = trimmed
                .split_once(':')
                .filter(|(label, _)| !label.contains(' '))
            {
                current_label = label.to_string();
            }
            output.push_str(lines[index]);
            output.push('\n');
            index += 1;
            continue;
        }
        let header = trimmed
            .strip_prefix("switch ")
            .and_then(|line| line.strip_suffix('['))
            .ok_or_else(|| Error::Parse(format!("invalid switch: {trimmed}")))?
            .trim();
        let (condition, default) = header
            .split_once(", label %")
            .ok_or_else(|| Error::Parse(format!("invalid switch: {trimmed}")))?;
        let (ty, value) = condition
            .split_once(' ')
            .ok_or_else(|| Error::Parse(format!("invalid switch condition: {condition}")))?;
        index += 1;
        let mut cases = Vec::new();
        while index < lines.len() && lines[index].trim() != "]" {
            let case = lines[index].trim();
            let (constant, destination) = case
                .strip_prefix(ty)
                .map(str::trim_start)
                .and_then(|case| case.split_once(", label %"))
                .ok_or_else(|| Error::Parse(format!("invalid switch case: {case}")))?;
            cases.push((constant.to_string(), destination.to_string()));
            index += 1;
        }
        index += 1;
        for (case_index, (constant, destination)) in cases.iter().enumerate() {
            let compare = format!("%llvm.switch.cmp.{next_switch}.{case_index}");
            let next = format!("llvm.switch.next.{current_label}.{next_switch}.{case_index}");
            output.push_str(&format!("  {compare} = icmp eq {ty} {value}, {constant}\n"));
            let false_dest = if case_index + 1 == cases.len() {
                default
            } else {
                &next
            };
            output.push_str(&format!(
                "  br i1 {compare}, label %{destination}, label %{false_dest}\n"
            ));
            if case_index + 1 != cases.len() {
                output.push_str(&format!("{next}:\n"));
            }
        }
        if cases.is_empty() {
            output.push_str(&format!("  br label %{default}\n"));
        }
        next_switch += 1;
    }
    Ok(output)
}

fn implicit_entry_label(signature: &str) -> Option<String> {
    let params = signature
        .split_once('@')
        .and_then(|(_, rest)| rest.split_once('('))?;
    let mut parens = 1;
    let mut braces = 0;
    let mut brackets = 0;
    let mut angles = 0;
    let mut start = 0;
    let mut numbered = Vec::new();
    for (index, byte) in params.1.bytes().enumerate() {
        match byte {
            b'(' => parens += 1,
            b')' if parens == 1 => {
                numbered.push(&params.1[start..index]);
                return Some(
                    numbered
                        .into_iter()
                        .filter_map(|param| param.rsplit_once('%'))
                        .filter_map(|(_, name)| {
                            let digits = name.bytes().take_while(u8::is_ascii_digit).count();
                            name[..digits].parse::<u64>().ok()
                        })
                        .max()
                        .map_or(0, |value| value + 1)
                        .to_string(),
                );
            }
            b')' => parens -= 1,
            b'{' => braces += 1,
            b'}' => braces -= 1,
            b'[' => brackets += 1,
            b']' => brackets -= 1,
            b'<' => angles += 1,
            b'>' => angles -= 1,
            b',' if parens == 1 && braces == 0 && brackets == 0 && angles == 0 => {
                numbered.push(&params.1[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    None
}

fn normalize_constant_geps(src: &str) -> String {
    let mut next = 0;
    let mut output = String::new();
    for original in src.lines() {
        let mut line = original.to_string();
        let mut definitions = Vec::new();
        while let Some((start, end)) = innermost_constant_gep(&line) {
            let name = format!("%llvm.gep.{next}");
            next += 1;
            let expression = &line[start..end];
            let arguments = expression
                .find('(')
                .map(|open| &expression[open + 1..expression.len() - 1])
                .unwrap();
            definitions.push(format!("  {name} = getelementptr {arguments}"));
            line.replace_range(start..end, &name);
        }
        for definition in definitions {
            output.push_str(&definition);
            output.push('\n');
        }
        output.push_str(&line);
        output.push('\n');
    }
    output
}

fn innermost_constant_gep(line: &str) -> Option<(usize, usize)> {
    let mut search = 0;
    let mut found = None;
    while let Some(relative) = find_unquoted(&line[search..], "getelementptr") {
        let start = search + relative;
        let open = line[start..].find('(').map(|index| start + index)?;
        let mut depth = 0;
        for (relative, byte) in line[open..].bytes().enumerate() {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        found = Some((start, open + relative + 1));
                        break;
                    }
                }
                _ => {}
            }
        }
        search = open + 1;
    }
    found
}

type Extra<'src> = extra::Err<Rich<'src, Token<'src>, Span>>;

fn type_parser<'src, I>() -> impl Parser<'src, I, Type, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    recursive(|ty| {
        let array = select! { Token::Int(n) if n >= 0 => n as u64 }
            .then_ignore(just(Token::Ident("x")))
            .then(ty.clone())
            .delimited_by(just(Token::LBracket), just(Token::RBracket))
            .map(|(count, elem)| Type::Array(count, Box::new(elem)));
        let vector = select! { Token::Int(n) if n > 0 && n <= u32::MAX as i64 => n as u32 }
            .then_ignore(just(Token::Ident("x")))
            .then(ty.clone())
            .delimited_by(just(Token::LAngle), just(Token::RAngle))
            .map(|(count, elem)| Type::Vector(count, Box::new(elem)));
        let structure = ty
            .clone()
            .separated_by(just(Token::Comma))
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LBrace), just(Token::RBrace))
            .map(Type::Struct);
        choice((
            select! { Token::IntTy(w) => Type::Int(w) },
            select! { Token::Local(n) => Type::Named(n.to_string()) },
            select! {
                Token::Ident("void") => Type::Void,
                Token::Ident("ptr") => Type::Ptr(None),
                Token::Ident("half") => Type::Float(16),
                Token::Ident("float") => Type::Float(32),
                Token::Ident("double") => Type::Float(64),
            },
            array,
            vector,
            structure,
        ))
        .then(just(Token::Star).repeated().collect::<Vec<_>>())
        .map(|(base, stars)| {
            stars
                .into_iter()
                .fold(base, |ty, _| Type::Ptr(Some(Box::new(ty))))
        })
    })
}

fn operand_parser<'src, I>() -> impl Parser<'src, I, Operand, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    select! {
        Token::Local(n) => Operand::Ref(n.to_string()),
        Token::Int(v) => Operand::ConstInt(v),
        Token::Float(v) => Operand::ConstFloat(v),
        Token::HexFloatBits(bits) => Operand::ConstFloatBits(bits),
        Token::Ident("true") => Operand::ConstInt(1),
        Token::Ident("false") => Operand::ConstInt(0),
        Token::Global(n) => Operand::Global(n.to_string()),
        Token::Ident("null") => Operand::Null,
        Token::Ident("undef") => Operand::Undef,
        Token::Ident("poison") => Operand::Poison,
    }
}

fn binop_parser<'src, I>() -> impl Parser<'src, I, BinOp, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    select! {
        Token::Ident("add") => BinOp::Add,
        Token::Ident("sub") => BinOp::Sub,
        Token::Ident("mul") => BinOp::Mul,
        Token::Ident("and") => BinOp::And,
        Token::Ident("or") => BinOp::Or,
        Token::Ident("xor") => BinOp::Xor,
        Token::Ident("shl") => BinOp::Shl,
        Token::Ident("lshr") => BinOp::LShr,
        Token::Ident("ashr") => BinOp::AShr,
        Token::Ident("sdiv") => BinOp::SDiv,
        Token::Ident("udiv") => BinOp::UDiv,
        Token::Ident("srem") => BinOp::SRem,
        Token::Ident("urem") => BinOp::URem,
        Token::Ident("fadd") => BinOp::FAdd,
        Token::Ident("fsub") => BinOp::FSub,
        Token::Ident("fmul") => BinOp::FMul,
        Token::Ident("fdiv") => BinOp::FDiv,
    }
}

fn abi_attr_parser<'src, I>() -> impl Parser<'src, I, ParsedAbiAttr, Extra<'src>> + Clone
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let ty = type_parser();
    choice((
        just(Token::Ident("sret"))
            .ignore_then(
                ty.clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(ParsedAbiAttr::SRet),
        just(Token::Ident("byval"))
            .ignore_then(
                ty.clone()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .map(ParsedAbiAttr::ByVal),
        just(Token::Ident("align"))
            .ignore_then(select! { Token::Int(value) if value > 0 => value as u64 })
            .map(ParsedAbiAttr::Align),
        any()
            .filter(|t: &Token| matches!(t, Token::Ident(s) if SKIP.contains(s)))
            .to(ParsedAbiAttr::Other),
        just(Token::Ident("dereferenceable"))
            .ignore_then(
                select! { Token::Int(_) => () }
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .to(ParsedAbiAttr::Other),
        just(Token::Ident("captures"))
            .ignore_then(
                any()
                    .and_is(just(Token::RParen).not())
                    .repeated()
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            )
            .to(ParsedAbiAttr::Other),
    ))
}

#[allow(clippy::too_many_lines)]
fn module<'src, I>() -> impl Parser<'src, I, Result<Module, Error>, Extra<'src>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    // Skip attribute/linkage keywords wherever they decorate an operand or type.
    let skip = any()
        .filter(|t: &Token| matches!(t, Token::Ident(s) if SKIP.contains(s)))
        .repeated();

    let ty = type_parser();
    let operand = operand_parser();
    let abi_attrs = abi_attr_parser()
        .repeated()
        .collect::<Vec<_>>()
        .map(abi_attrs);

    let local = select! { Token::Local(n) => n.to_string() };
    // A branch target, `%name`; matches the stripped label defined at a block.
    let target = select! { Token::Local(n) => n.to_string() };
    // A block-label definition: a bare identifier or number followed by `:`.
    let block_label = select! {
        Token::Ident(s) => s.to_string(),
        Token::Int(n) => n.to_string(),
    };
    let binding = local.then_ignore(just(Token::Eq));

    let binop = binop_parser();
    let binary = binding
        .clone()
        .then(binop)
        .then(skip.collect::<Vec<_>>())
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(operand.clone())
        .map(|(((((result, op), flags), ty), lhs), rhs)| Inst::Binary {
            result,
            // Disjoint inputs have no carries; overlapping bits produce LLVM poison.
            op: if matches!(op, BinOp::Or) && flags.contains(&Token::Ident("disjoint")) {
                BinOp::Add
            } else {
                op
            },
            no_signed_wrap: flags.contains(&Token::Ident("nsw")),
            no_unsigned_wrap: flags.contains(&Token::Ident("nuw")),
            ty,
            lhs,
            rhs,
        });
    let fneg = binding
        .clone()
        .then_ignore(just(Token::Ident("fneg")))
        .then_ignore(skip.collect::<Vec<_>>())
        .then(ty.clone())
        .then(operand.clone())
        .map(|((result, ty), value)| Inst::FNeg { result, ty, value });

    let icmp = binding
        .clone()
        .then_ignore(just(Token::Ident("icmp")))
        .then_ignore(just(Token::Ident("samesign")).or_not())
        .then(select! { Token::Ident(p) => p.to_string() })
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(operand.clone())
        .map(|((((result, pred), ty), lhs), rhs)| Inst::ICmp {
            result,
            pred,
            ty,
            lhs,
            rhs,
        });
    let fcmp = binding
        .clone()
        .then_ignore(just(Token::Ident("fcmp")))
        .then(select! { Token::Ident(p) => p.to_string() })
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(operand.clone())
        .map(|((((result, pred), ty), lhs), rhs)| Inst::FCmp {
            result,
            pred,
            ty,
            lhs,
            rhs,
        });

    let extractelement = binding
        .clone()
        .then_ignore(just(Token::Ident("extractelement")))
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(ty.clone())
        .then(operand.clone())
        .map(
            |((((result, vector), value), _), index)| Inst::ExtractElement {
                result,
                vector,
                value,
                index,
            },
        );
    let insertelement = binding
        .clone()
        .then_ignore(just(Token::Ident("insertelement")))
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(ty.clone())
        .then(operand.clone())
        .map(
            |((((((result, vector), value), _), element), _), index)| Inst::InsertElement {
                result,
                vector,
                value,
                element,
                index,
            },
        );

    let castop = select! {
        Token::Ident("sext") => CastOp::SExt,
        Token::Ident("zext") => CastOp::ZExt,
        Token::Ident("trunc") => CastOp::Trunc,
        Token::Ident("ptrtoint") => CastOp::PtrToInt,
        Token::Ident("inttoptr") => CastOp::IntToPtr,
        Token::Ident("sitofp") => CastOp::SIToFP,
        Token::Ident("uitofp") => CastOp::UIToFP,
        Token::Ident("fptosi") => CastOp::FPToSI,
        Token::Ident("fptoui") => CastOp::FPToUI,
        Token::Ident("fpext") => CastOp::FPExt,
        Token::Ident("fptrunc") => CastOp::FPTrunc,
    };
    let cast = binding
        .clone()
        .then(castop)
        .then(just(Token::Ident("nneg")).or_not())
        .then_ignore(skip)
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Ident("to")))
        .then(ty.clone())
        .map(
            |(((((result, op), non_negative), from), value), to)| Inst::Cast {
                result,
                op,
                non_negative: non_negative.is_some(),
                from,
                value,
                to,
            },
        );
    let freeze = binding
        .clone()
        .then_ignore(just(Token::Ident("freeze")))
        .then(ty.clone())
        .then(operand.clone())
        .map(|((result, ty), value)| Inst::Freeze { result, ty, value });

    let alloca = binding
        .clone()
        .then_ignore(just(Token::Ident("alloca")))
        .then_ignore(skip)
        .then(ty.clone())
        .then(
            just(Token::Comma)
                .ignore_then(just(Token::Ident("align")))
                .ignore_then(select! { Token::Int(value) if value >= 0 => value as u64 })
                .or_not(),
        )
        .map(|((result, ty), align)| Inst::Alloca { result, ty, align });

    let gep_args = ty
        .clone()
        .then_ignore(just(Token::Comma))
        .then_ignore(ty.clone())
        .then(operand.clone())
        .then(
            just(Token::Comma)
                .ignore_then(ty.clone().then(operand.clone()))
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map(|((source, base), indices)| Operand::GetElementPtr {
            source,
            base: Box::new(base),
            indices,
        });
    let gep_value = just(Token::Ident("getelementptr"))
        .ignore_then(skip)
        .ignore_then(
            gep_args
                .clone()
                .delimited_by(just(Token::LParen), just(Token::RParen))
                .or(gep_args),
        );

    let address = gep_value.or(operand.clone());
    let load = binding
        .clone()
        .then_ignore(just(Token::Ident("load")))
        .then_ignore(skip)
        .then(ty.clone())
        .then_ignore(just(Token::Comma))
        .then_ignore(ty.clone())
        .then(address.clone())
        .map(|((result, ty), ptr)| Inst::Load { result, ty, ptr });
    let extractvalue = binding
        .clone()
        .then_ignore(just(Token::Ident("extractvalue")))
        .then(ty.clone())
        .then(operand.clone())
        .then(
            just(Token::Comma)
                .ignore_then(
                    select! { Token::Int(index) if index >= 0 && index <= u32::MAX as i64 => index as u32 },
                )
                .repeated()
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|(((result, aggregate), value), indices)| Inst::ExtractValue {
            result,
            aggregate,
            value,
            indices,
        });
    let insertvalue = binding
        .clone()
        .then_ignore(just(Token::Ident("insertvalue")))
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(
            select! { Token::Int(index) if index >= 0 && index <= u32::MAX as i64 => index as u32 },
        )
        .map(
            |(((((result, aggregate), value), element_type), element), index)| Inst::InsertValue {
                result,
                aggregate,
                value,
                element_type,
                element,
                index,
            },
        );

    let store = just(Token::Ident("store"))
        .ignore_then(skip)
        .ignore_then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then_ignore(ty.clone())
        .then(address)
        .map(|((ty, value), ptr)| Inst::Store { ty, value, ptr });

    let gep = binding
        .clone()
        .then_ignore(just(Token::Ident("getelementptr")))
        .then_ignore(skip)
        .then(ty.clone())
        .then_ignore(just(Token::Comma))
        .then_ignore(ty.clone())
        .then(operand.clone())
        .then(
            just(Token::Comma)
                .ignore_then(ty.clone().then(operand.clone()))
                .repeated()
                .collect::<Vec<_>>(),
        )
        .map(|(((result, source), base), indices)| Inst::GetElementPtr {
            result,
            source,
            base,
            indices,
        });

    let incoming = operand
        .clone()
        .then_ignore(just(Token::Comma))
        .then(target)
        .delimited_by(just(Token::LBracket), just(Token::RBracket));
    let phi = binding
        .clone()
        .then_ignore(just(Token::Ident("phi")))
        .then(ty.clone())
        .then(
            incoming
                .separated_by(just(Token::Comma))
                .at_least(1)
                .collect::<Vec<_>>(),
        )
        .map(|((result, ty), incoming)| Inst::Phi {
            result,
            ty,
            incoming,
        });

    let select = binding
        .clone()
        .then_ignore(just(Token::Ident("select")))
        .then_ignore(skip)
        .then_ignore(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then_ignore(ty.clone())
        .then(operand.clone())
        .map(|((((result, cond), ty), if_true), if_false)| Inst::Select {
            result,
            cond,
            ty,
            if_true,
            if_false,
        });

    let br = {
        let dest = just(Token::Ident("label")).ignore_then(target);
        let uncond = dest.clone().map(|dest| Inst::Br { dest });
        let cond = ty
            .clone()
            .ignore_then(operand.clone())
            .then_ignore(just(Token::Comma))
            .then(dest.clone())
            .then_ignore(just(Token::Comma))
            .then(dest.clone())
            .map(|((cond, if_true), if_false)| Inst::CondBr {
                cond,
                if_true,
                if_false,
            });
        just(Token::Ident("br")).ignore_then(uncond.or(cond))
    };

    let ret = {
        let void = just(Token::Ident("void")).to(Inst::Ret { value: None });
        let val = ty.clone().then(operand.clone()).map(|(ty, op)| Inst::Ret {
            value: Some((ty, op)),
        });
        just(Token::Ident("ret")).ignore_then(void.or(val))
    };
    let unreachable = just(Token::Ident("unreachable")).to(Inst::Unreachable);

    let call = {
        let arg = ty
            .clone()
            .then(abi_attrs.clone())
            .then(operand.clone())
            .map(|((ty, abi), value)| CallArg { ty, value, abi });
        let args = arg
            .separated_by(just(Token::Comma))
            .collect::<Vec<_>>()
            .delimited_by(just(Token::LParen), just(Token::RParen));
        binding
            .clone()
            .or_not()
            .then_ignore(just(Token::Ident("tail")).or_not())
            .then_ignore(just(Token::Ident("musttail")).or_not())
            .then_ignore(just(Token::Ident("call")))
            .then_ignore(skip)
            .then(ty.clone())
            .then_ignore(
                any()
                    .and_is(select! { Token::Global(_) | Token::Local(_) => () }.not())
                    .repeated(),
            )
            .then(select! {
                Token::Global(n) => Operand::Global(n.to_string()),
                Token::Local(n) => Operand::Ref(n.to_string()),
            })
            .then(args)
            .map(|(((result, ret), callee), args)| Inst::Call {
                result,
                ret,
                callee,
                args,
            })
    };

    // Any other instruction: capture its opcode (after an optional result
    // binding and `tail` marker) so lowering can report it as unsupported.
    let unsupported = binding
        .clone()
        .or_not()
        .then_ignore(just(Token::Ident("tail")).or_not())
        .ignore_then(select! { Token::Ident(s) => s.to_string() })
        .map(Inst::Unsupported);

    let inst = choice((
        freeze,
        fneg,
        binary,
        icmp,
        fcmp,
        extractelement,
        insertelement,
        cast,
        alloca,
        load,
        extractvalue,
        insertvalue,
        gep,
        phi,
        select,
        call,
        store,
        br,
        ret,
        unreachable,
        unsupported,
    ));

    // Trailing tokens on an instruction line (`, align 4`, `!tbaa !3`, ...) are
    // irrelevant here; drop everything up to the newline.
    let rest_of_line = any().and_is(just(Token::Newline).not()).repeated();

    let label_line = block_label.then_ignore(just(Token::Colon)).map(Item::Label);
    let stmt = label_line
        .or(inst.map(|inst| Item::Inst(Box::new(inst))))
        .then_ignore(rest_of_line.clone());

    let body = choice((
        just(Token::Newline).to(Option::<Item>::None),
        stmt.map(Some),
    ))
    .repeated()
    .collect::<Vec<_>>();

    let param = ty
        .clone()
        .then(abi_attrs)
        .then(local)
        .map(|((ty, abi), name)| Param { name, ty, abi });
    let params = param
        .separated_by(just(Token::Comma))
        .collect::<Vec<_>>()
        .then(
            just(Token::Comma)
                .or_not()
                .ignore_then(just(Token::Ident("...")))
                .or_not(),
        )
        .map(|(params, variadic)| (params, variadic.is_some()))
        .delimited_by(just(Token::LParen), just(Token::RParen));

    let function = just(Token::Ident("define"))
        .ignore_then(skip.collect::<Vec<_>>())
        .then(ty.clone())
        .then(select! { Token::Global(n) => n.to_string() })
        .then(params)
        // Skip anything between the signature and the opening brace: attribute
        // groups, `unnamed_addr`, `personality`, alignment, ...
        .then_ignore(any().and_is(just(Token::LBrace).not()).repeated())
        .then_ignore(just(Token::LBrace))
        .then(body)
        .then_ignore(just(Token::RBrace))
        .map(|((((linkage, ret), name), (params, variadic)), items)| {
            let internal = linkage
                .iter()
                .any(|token| matches!(token, Token::Ident("internal" | "private")));
            build_function(name, internal, ret, params, variadic, items)
        });

    choice((
        function.map(|function| Ok(Some(TopItem::Function(function)))),
        non_function_top_level(),
    ))
    .repeated()
    .collect::<Vec<_>>()
    .then_ignore(end())
    .map(build_module)
}

fn non_function_top_level<'src, I>()
-> impl Parser<'src, I, Result<Option<TopItem>, Error>, Extra<'src>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    let named_type = select! { Token::Local(name) => name.to_string() }
        .then_ignore(just(Token::Eq))
        .then_ignore(just(Token::Ident("type")))
        .then(type_parser())
        .map(|(name, ty)| TopItem::NamedType(name, ty));
    let top_line = any()
        .and_is(just(Token::Newline).not())
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(just(Token::Newline).or_not());
    let global = select! { Token::Global(name) => name.to_string() }
        .then_ignore(just(Token::Eq))
        .then(top_line.clone())
        .map(|(name, tokens)| parse_global_tokens(name, &tokens).map(TopItem::Global));
    let declaration = just(Token::Ident("declare"))
        .ignore_then(top_line)
        .map(|tokens| parse_declaration_tokens(&tokens).map(TopItem::Declaration));
    let skip_line = choice((
        just(Token::Newline).ignored(),
        any()
            .and_is(just(Token::Newline).not())
            .and_is(just(Token::Ident("define")).not())
            .repeated()
            .at_least(1)
            .collect::<Vec<_>>()
            .then_ignore(just(Token::Newline).or_not())
            .ignored(),
    ));
    choice((
        named_type.map(|item| Ok(Some(item))),
        global.map(|item| item.map(Some)),
        declaration.map(|item| item.map(Some)),
        skip_line.map(|()| Ok(None)),
    ))
}

fn build_module(items: Vec<Result<Option<TopItem>, Error>>) -> Result<Module, Error> {
    let mut module = Module {
        named_types: Vec::new(),
        globals: Vec::new(),
        declarations: Vec::new(),
        functions: Vec::new(),
    };
    for item in items {
        match item? {
            Some(TopItem::NamedType(name, ty)) => module.named_types.push((name, ty)),
            Some(TopItem::Global(global)) => module.globals.push(global),
            Some(TopItem::Declaration(declaration)) => module.declarations.push(declaration),
            Some(TopItem::Function(function)) => module.functions.push(function),
            None => {}
        }
    }
    Ok(module)
}

enum TopItem {
    NamedType(String, Type),
    Global(Global),
    Declaration(Declaration),
    Function(Function),
}

#[derive(Clone)]
enum Item {
    Label(String),
    Inst(Box<Inst>),
}

/// Fold the flat statement list into blocks. An unlabelled entry block is
/// implicit; the first label seen names it if it is still empty, otherwise it
/// opens a new block.
fn build_function(
    name: String,
    internal: bool,
    ret: Type,
    params: Vec<Param>,
    variadic: bool,
    items: Vec<Option<Item>>,
) -> Function {
    let mut blocks = vec![Block {
        label: None,
        insts: Vec::new(),
    }];
    for item in items.into_iter().flatten() {
        match item {
            Item::Label(label) => {
                let entry_empty =
                    blocks.len() == 1 && blocks[0].label.is_none() && blocks[0].insts.is_empty();
                if entry_empty {
                    blocks[0].label = Some(label);
                } else {
                    blocks.push(Block {
                        label: Some(label),
                        insts: Vec::new(),
                    });
                }
            }
            Item::Inst(inst) => blocks.last_mut().unwrap().insts.push(*inst),
        }
    }
    Function {
        name,
        internal,
        ret,
        params,
        variadic,
        blocks,
    }
}

fn parse_type_tokens(tokens: &[Token<'_>]) -> Result<Type, Error> {
    let (ty, consumed) = type_prefix(tokens)?;
    if consumed == tokens.len() {
        Ok(ty)
    } else {
        Err(Error::Parse("invalid type".into()))
    }
}

fn type_prefix(tokens: &[Token<'_>]) -> Result<(Type, usize), Error> {
    let spanned = tokens
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, token)| (token, Span::from(index..index + 1)))
        .collect::<Vec<_>>();
    let eoi = Span::from(tokens.len()..tokens.len());
    let input = spanned.as_slice().map(eoi, |(token, span)| (token, span));
    type_parser()
        .then(any().repeated().collect::<Vec<_>>())
        .then_ignore(end())
        .parse(input)
        .into_result()
        .map(|(ty, remainder)| (ty, tokens.len() - remainder.len()))
        .map_err(|errors| Error::Parse(format!("invalid type: {errors:?}")))
}

fn parse_declaration_tokens(tokens: &[Token<'_>]) -> Result<Declaration, Error> {
    let symbol = tokens
        .iter()
        .position(|token| matches!(token, Token::Global(_)))
        .ok_or_else(|| Error::Parse("invalid declaration".into()))?;
    let (name, after_symbol) = match &tokens[symbol..] {
        [Token::Global(name), Token::LParen, rest @ ..] => ((*name).to_string(), rest),
        _ => return Err(Error::Parse("invalid declaration".into())),
    };
    let ret = (0..symbol)
        .rev()
        .find_map(|start| parse_type_tokens(&tokens[start..symbol]).ok())
        .ok_or_else(|| Error::Parse("invalid declaration return type".into()))?;
    let mut depth = 0;
    let close = after_symbol
        .iter()
        .position(|token| match token {
            Token::LParen | Token::LBracket | Token::LBrace => {
                depth += 1;
                false
            }
            Token::RParen if depth == 0 => true,
            Token::RParen | Token::RBracket | Token::RBrace => {
                depth -= 1;
                false
            }
            _ => false,
        })
        .ok_or_else(|| Error::Parse("unterminated declaration".into()))?;
    let mut variadic = false;
    let mut params = Vec::new();
    for parameter in split_token_commas(&after_symbol[..close]) {
        if matches!(parameter, [Token::Ident("...")]) {
            variadic = true;
        } else if !parameter.is_empty() {
            let (ty, _) = type_prefix(parameter)?;
            params.push(ty);
        }
    }
    Ok(Declaration {
        name,
        ret,
        params,
        variadic,
    })
}

fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (relative, byte) in text[open..].bytes().enumerate() {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + relative);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_global_tokens(name: String, tokens: &[Token<'_>]) -> Result<Global, Error> {
    let marker = tokens
        .iter()
        .position(|token| matches!(token, Token::Ident("global" | "constant")))
        .ok_or_else(|| Error::Parse(format!("unsupported global: @{name}")))?;
    let private = tokens[..marker]
        .iter()
        .any(|token| matches!(token, Token::Ident("private" | "internal")));
    let constant = matches!(tokens[marker], Token::Ident("constant"));
    let external = tokens[..marker]
        .iter()
        .any(|token| matches!(token, Token::Ident("external")));
    let definition = &tokens[marker + 1..];
    let (ty, type_len) = type_prefix(definition)
        .map_err(|_| Error::Parse(format!("invalid global type: @{name}")))?;
    if has_wide_integer(&ty) {
        return Err(Error::Parse(format!(
            "unsupported integer global type: {ty:?}"
        )));
    }
    let align = tokens
        .windows(2)
        .rev()
        .find_map(|pair| match pair {
            [Token::Ident("align"), Token::Int(value)] if *value >= 0 => Some(*value as u64),
            _ => None,
        })
        .unwrap_or(1);
    let initializer_tokens = &definition[type_len..];
    let initializer_tokens = &initializer_tokens
        [..first_top_level_comma(initializer_tokens).unwrap_or(initializer_tokens.len())];
    let initializer = parse_global_initializer(initializer_tokens, &ty, external)?;
    Ok(Global {
        name,
        ty,
        initializer,
        align,
        private,
        constant,
    })
}

fn parse_global_initializer(
    tokens: &[Token<'_>],
    ty: &Type,
    external: bool,
) -> Result<GlobalInitializer, Error> {
    if external {
        return Ok(GlobalInitializer::External);
    }
    match tokens {
        [Token::Ident("zeroinitializer")] => Ok(GlobalInitializer::Zero),
        [Token::Ident("null")] => Ok(GlobalInitializer::Null),
        [Token::CString(text)] => Ok(GlobalInitializer::CString(decode_c_string(text)?)),
        [Token::Int(value)] => Ok(GlobalInitializer::Integer(*value)),
        [Token::Float(value)] if *value == 0.0 && !value.is_sign_negative() => {
            Ok(GlobalInitializer::Zero)
        }
        _ if tokens.iter().any(|token| matches!(token, Token::Global(_))) => {
            parse_symbol_initializer(tokens, ty)
        }
        [Token::LBracket, inner @ .., Token::RBracket] => Ok(GlobalInitializer::Bytes(
            parse_integer_array_tokens(inner, ty)?,
        )),
        _ => Err(Error::Parse("unsupported global initializer".into())),
    }
}

fn has_wide_integer(ty: &Type) -> bool {
    match ty {
        Type::Int(width) => *width > 64,
        Type::Array(_, element) => has_wide_integer(element),
        Type::Struct(fields) => fields.iter().any(has_wide_integer),
        _ => false,
    }
}

fn parse_symbol_initializer(tokens: &[Token<'_>], ty: &Type) -> Result<GlobalInitializer, Error> {
    let entries = match tokens {
        [Token::LBracket, inner @ .., Token::RBracket] => split_token_commas(inner),
        _ => vec![tokens],
    };
    let relative = entries.iter().any(|entry| {
        entry
            .iter()
            .any(|token| matches!(token, Token::Ident("trunc")))
    });
    if relative {
        if !matches!(ty, Type::Array(_, element) if **element == Type::Int(32)) {
            return Err(Error::Parse(
                "symbol differences require an i32 array".into(),
            ));
        }
        let differences = entries
            .into_iter()
            .map(|entry| {
                if let [
                    Token::IntTy(32),
                    Token::Ident("trunc"),
                    Token::LParen,
                    Token::IntTy(64),
                    Token::Ident("sub"),
                    Token::LParen,
                    Token::IntTy(64),
                    Token::Ident("ptrtoint"),
                    Token::LParen,
                    Token::Ident("ptr"),
                    Token::Global(symbol),
                    Token::Ident("to"),
                    Token::IntTy(64),
                    Token::RParen,
                    Token::Comma,
                    Token::IntTy(64),
                    Token::Ident("ptrtoint"),
                    Token::LParen,
                    Token::Ident("ptr"),
                    Token::Global(base),
                    Token::Ident("to"),
                    Token::IntTy(64),
                    Token::RParen,
                    Token::RParen,
                    Token::Ident("to"),
                    Token::IntTy(32),
                    Token::RParen,
                ] = entry
                {
                    Ok(SymbolDifference {
                        symbol: (*symbol).to_string(),
                        base: (*base).to_string(),
                    })
                } else {
                    Err(Error::Parse(
                        "unsupported symbolic global initializer".into(),
                    ))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(GlobalInitializer::SymbolDifferences(differences));
    }
    entries
        .into_iter()
        .map(|entry| match entry {
            [Token::Ident("ptr"), Token::Global(symbol)] => Ok((*symbol).to_string()),
            _ => Err(Error::Parse(
                "unsupported symbolic global initializer".into(),
            )),
        })
        .collect::<Result<Vec<_>, _>>()
        .map(GlobalInitializer::Symbols)
}

fn parse_integer_array_tokens(tokens: &[Token<'_>], ty: &Type) -> Result<Vec<u8>, Error> {
    let Type::Array(_, element) = ty else {
        return Err(Error::Parse("array initializer has non-array type".into()));
    };
    let Type::Int(width) = element.as_ref() else {
        return Err(Error::Parse(
            "unsupported aggregate global initializer".into(),
        ));
    };
    let bytes_per_element = width.div_ceil(8) as usize;
    let mut bytes = Vec::new();
    for item in split_token_commas(tokens) {
        let value = item
            .iter()
            .find_map(|token| match token {
                Token::Int(value) => Some(*value),
                _ => None,
            })
            .ok_or_else(|| Error::Parse("invalid integer initializer".into()))?;
        bytes.extend_from_slice(&value.to_le_bytes()[..bytes_per_element]);
    }
    Ok(bytes)
}

fn split_token_commas<'a, 'src>(tokens: &'a [Token<'src>]) -> Vec<&'a [Token<'src>]> {
    let mut depth = 0i32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::LBracket | Token::LBrace | Token::LParen => depth += 1,
            Token::RBracket | Token::RBrace | Token::RParen => depth -= 1,
            Token::Comma if depth == 0 => {
                parts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&tokens[start..]);
    parts
}

fn first_top_level_comma(tokens: &[Token<'_>]) -> Option<usize> {
    let mut depth = 0i32;
    tokens.iter().position(|token| match token {
        Token::LBracket | Token::LBrace | Token::LParen => {
            depth += 1;
            false
        }
        Token::RBracket | Token::RBrace | Token::RParen => {
            depth -= 1;
            false
        }
        Token::Comma => depth == 0,
        _ => false,
    })
}

fn decode_c_string(text: &str) -> Result<Vec<u8>, Error> {
    let mut bytes = Vec::new();
    let mut chars = text.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        let hi = chars
            .next()
            .ok_or_else(|| Error::Parse("truncated string escape".into()))?;
        let lo = chars
            .next()
            .ok_or_else(|| Error::Parse("truncated string escape".into()))?;
        let digits = [hi, lo];
        let value = std::str::from_utf8(&digits)
            .ok()
            .and_then(|digits| u8::from_str_radix(digits, 16).ok())
            .ok_or_else(|| Error::Parse("invalid string escape".into()))?;
        bytes.push(value);
    }
    Ok(bytes)
}
