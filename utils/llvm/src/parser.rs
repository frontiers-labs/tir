//! Parser from an LLVM textual-IR token stream into [`ast`], built with
//! `chumsky`. Top-level lines other than `define` (target triples, globals,
//! metadata, attribute groups, `declare`) are skipped; each recognised
//! instruction is parsed into a typed node, and anything else on an instruction
//! line becomes [`ast::Inst::Unsupported`] so the module still parses and
//! conversion can report precisely what it cannot lower.

use chumsky::input::ValueInput;
use chumsky::prelude::*;

use crate::ast::*;
use crate::error::Error;
use crate::lexer::{Span, Token, lex};

/// Attribute, linkage and flag keywords that decorate instructions and
/// signatures but carry no meaning for this importer; skipped wherever they may
/// appear. None of these collide with a type, opcode or operand keyword.
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
    "fastcc",
    "coldcc",
    "tailcc",
];

pub fn parse_module(src: &str) -> Result<Module, Error> {
    let (without_attributes, source_attributes) = normalize_parameterized_attributes(src);
    let normalized = normalize_constant_geps(&normalize_switches(&without_attributes)?);
    let tokens = lex(&normalized);
    let eoi = Span::from(normalized.len()..normalized.len());
    let input = tokens.as_slice().map(eoi, |(t, s)| (t, s));

    let (out, errors) = module().parse(input).into_output_errors();
    match out {
        Some(mut module) if errors.is_empty() => {
            let (named_types, globals, declarations) = parse_top_level(src)?;
            module.named_types = named_types;
            module.globals = globals;
            module.declarations = declarations;
            module.source_attributes = source_attributes;
            Ok(module)
        }
        _ => Err(Error::Parse(
            errors
                .iter()
                .map(|e| format!("{e:?}"))
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}

fn normalize_parameterized_attributes(src: &str) -> (String, Vec<String>) {
    const NAMES: &[&str] = &[
        "range",
        "captures",
        "initializes",
        "dereferenceable",
        "dereferenceable_or_null",
    ];
    let mut text = src.to_string();
    let mut attributes = src
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|word| {
            matches!(
                *word,
                "nsw"
                    | "nuw"
                    | "exact"
                    | "disjoint"
                    | "nneg"
                    | "inbounds"
                    | "musttail"
                    | "volatile"
            )
        })
        .map(|word| word.to_string())
        .collect::<Vec<_>>();
    loop {
        let found = NAMES
            .iter()
            .filter_map(|name| text.find(&format!("{name}(")).map(|index| (index, *name)))
            .min_by_key(|(index, _)| *index);
        let Some((start, name)) = found else {
            break;
        };
        let open = start + name.len();
        let Some(close) = matching_paren(&text, open) else {
            break;
        };
        attributes.push(text[start..=close].to_string());
        text.replace_range(start..=close, "");
    }
    attributes.sort();
    attributes.dedup();
    (text, attributes)
}

fn normalize_switches(src: &str) -> Result<String, Error> {
    let lines = src.lines().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    let mut next_switch = 0;
    let mut current_label = "0".to_string();
    while index < lines.len() {
        let trimmed = lines[index].trim();
        if !trimmed.starts_with("switch ") {
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
    while let Some(relative) = line[search..].find("getelementptr") {
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
        Token::Ident("true") => Operand::ConstInt(1),
        Token::Ident("false") => Operand::ConstInt(0),
        Token::Global(n) => Operand::Global(n.to_string()),
        Token::Ident("null") => Operand::Null,
        Token::Ident("undef") => Operand::Undef,
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

fn module<'src, I>() -> impl Parser<'src, I, Module, Extra<'src>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    // Skip attribute/linkage keywords wherever they decorate an operand or type.
    let skip = any()
        .filter(|t: &Token| matches!(t, Token::Ident(s) if SKIP.contains(s)))
        .repeated();

    let ty = type_parser();
    let operand = operand_parser();

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
        .then_ignore(skip)
        .then(ty.clone())
        .then(operand.clone())
        .then_ignore(just(Token::Comma))
        .then(operand.clone())
        .map(|((((result, op), ty), lhs), rhs)| Inst::Binary {
            result,
            op,
            ty,
            lhs,
            rhs,
        });

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

    let call = {
        let arg_attr = choice((
            any()
                .filter(|t: &Token| matches!(t, Token::Ident(s) if SKIP.contains(s)))
                .ignored(),
            just(Token::Ident("align")).ignore_then(select! { Token::Int(_) => () }),
            just(Token::Ident("dereferenceable")).ignore_then(
                select! { Token::Int(_) => () }
                    .delimited_by(just(Token::LParen), just(Token::RParen)),
            ),
            just(Token::Ident("captures")).ignore_then(
                any()
                    .and_is(just(Token::RParen).not())
                    .repeated()
                    .delimited_by(just(Token::LParen), just(Token::RParen))
                    .ignored(),
            ),
        ))
        .repeated();
        let arg = ty.clone().then_ignore(arg_attr).then(operand.clone());
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
        binary,
        icmp,
        fcmp,
        cast,
        alloca,
        load,
        gep,
        phi,
        select,
        call,
        store,
        br,
        ret,
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
        .then_ignore(skip)
        .then(local)
        .map(|(ty, name)| Param { name, ty });
    let params = param
        .separated_by(just(Token::Comma))
        .collect::<Vec<_>>()
        .delimited_by(just(Token::LParen), just(Token::RParen));

    let function = just(Token::Ident("define"))
        .ignore_then(skip)
        .ignore_then(ty.clone())
        .then(select! { Token::Global(n) => n.to_string() })
        .then(params)
        // Skip anything between the signature and the opening brace: attribute
        // groups, `unnamed_addr`, `personality`, alignment, ...
        .then_ignore(any().and_is(just(Token::LBrace).not()).repeated())
        .then_ignore(just(Token::LBrace))
        .then(body)
        .then_ignore(just(Token::RBrace))
        .map(|(((ret, name), params), items)| build_function(name, ret, params, items));

    // A skipped top-level line: at least one token or a bare newline, always
    // making progress so the outer `repeated` terminates.
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

    choice((function.map(Some), skip_line.map(|()| None)))
        .repeated()
        .collect::<Vec<_>>()
        .then_ignore(end())
        .map(|items| Module {
            named_types: Vec::new(),
            globals: Vec::new(),
            declarations: Vec::new(),
            source_attributes: Vec::new(),
            functions: items.into_iter().flatten().collect(),
        })
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
    ret: Type,
    params: Vec<Param>,
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
        ret,
        params,
        blocks,
    }
}

type TopLevel = (Vec<(String, Type)>, Vec<Global>, Vec<Declaration>);

fn parse_top_level(src: &str) -> Result<TopLevel, Error> {
    let mut named_types = Vec::new();
    let mut globals = Vec::new();
    let mut declarations = Vec::new();
    for line in src.lines().map(str::trim) {
        if line.starts_with('%') && line.contains(" = type ") {
            let (name, body) = line.split_once(" = type ").unwrap();
            named_types.push((name[1..].to_string(), parse_type_text(body)?));
        } else if line.starts_with('@') && line.contains(" = ") {
            globals.push(parse_global(line)?);
        } else if line.starts_with("declare ") {
            declarations.push(parse_declaration(line)?);
        }
    }
    Ok((named_types, globals, declarations))
}

fn parse_declaration(line: &str) -> Result<Declaration, Error> {
    let at = line
        .find('@')
        .ok_or_else(|| Error::Parse(format!("invalid declaration: {line}")))?;
    let prefix = line["declare ".len()..at].trim();
    let ret = prefix
        .split_whitespace()
        .rev()
        .find_map(|word| parse_type_text(word).ok())
        .ok_or_else(|| Error::Parse(format!("invalid declaration return type: {line}")))?;
    let open = line[at..].find('(').map(|index| at + index).unwrap();
    let close = matching_paren(line, open)
        .ok_or_else(|| Error::Parse(format!("unterminated declaration: {line}")))?;
    let name = line[at + 1..open].to_string();
    let mut variadic = false;
    let mut params = Vec::new();
    for parameter in split_commas(&line[open + 1..close]) {
        if parameter.is_empty() {
            continue;
        }
        if parameter == "..." {
            variadic = true;
            continue;
        }
        let ty = parameter
            .split_whitespace()
            .scan(String::new(), |prefix, word| {
                if !prefix.is_empty() {
                    prefix.push(' ');
                }
                prefix.push_str(word);
                Some(prefix.clone())
            })
            .find_map(|prefix| parse_type_text(&prefix).ok())
            .ok_or_else(|| Error::Parse(format!("invalid declaration parameter: {parameter}")))?;
        params.push(ty);
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

fn parse_global(line: &str) -> Result<Global, Error> {
    let (name, rest) = line
        .split_once(" = ")
        .ok_or_else(|| Error::Parse(format!("invalid global: {line}")))?;
    let private = rest
        .split_whitespace()
        .any(|word| word == "private" || word == "internal");
    let constant = rest.split_whitespace().any(|word| word == "constant");
    let marker = if constant { "constant " } else { "global " };
    let definition = rest
        .find(marker)
        .map(|index| &rest[index + marker.len()..])
        .ok_or_else(|| Error::Parse(format!("unsupported global: {line}")))?;
    let align = definition
        .rsplit_once(", align ")
        .and_then(|(_, value)| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let definition = definition.split(", align ").next().unwrap().trim();
    let (ty, initializer) = if rest.split_whitespace().any(|word| word == "external") {
        (parse_type_text(definition)?, GlobalInitializer::External)
    } else {
        let (ty, initializer) = split_type_value(definition)?;
        if has_wide_integer(&ty) {
            return Err(Error::Parse(format!(
                "unsupported integer global type: {ty:?}"
            )));
        }
        let initializer = if initializer == "zeroinitializer" {
            GlobalInitializer::Zero
        } else if initializer == "null" {
            GlobalInitializer::Null
        } else if let Some(text) = initializer
            .strip_prefix("c\"")
            .and_then(|s| s.strip_suffix('"'))
        {
            GlobalInitializer::CString(decode_c_string(text)?)
        } else if let Ok(value) = initializer.parse() {
            GlobalInitializer::Integer(value)
        } else if initializer
            .parse::<f64>()
            .is_ok_and(|value| value == 0.0 && !value.is_sign_negative())
        {
            GlobalInitializer::Zero
        } else if initializer.contains("trunc (i64 sub (") {
            GlobalInitializer::SymbolDifferences(parse_symbol_differences(initializer, &ty)?)
        } else if initializer.contains('@') {
            GlobalInitializer::Symbols(parse_absolute_symbols(initializer)?)
        } else if initializer.starts_with('[') {
            GlobalInitializer::Bytes(parse_integer_array(initializer, &ty)?)
        } else {
            return Err(Error::Parse(format!(
                "unsupported global initializer: {initializer}"
            )));
        };
        (ty, initializer)
    };
    Ok(Global {
        name: name[1..].to_string(),
        ty,
        initializer,
        align,
        private,
        constant,
    })
}

fn has_wide_integer(ty: &Type) -> bool {
    match ty {
        Type::Int(width) => *width > 64,
        Type::Array(_, element) => has_wide_integer(element),
        Type::Struct(fields) => fields.iter().any(has_wide_integer),
        _ => false,
    }
}

fn parse_symbols(initializer: &str) -> Vec<String> {
    initializer
        .split('@')
        .skip(1)
        .map(|part| {
            part.split(|c: char| !(c.is_ascii_alphanumeric() || "._$".contains(c)))
                .next()
                .unwrap()
                .to_string()
        })
        .collect()
}

fn parse_absolute_symbols(initializer: &str) -> Result<Vec<String>, Error> {
    let entries = initializer
        .strip_prefix('[')
        .and_then(|text| text.strip_suffix(']'))
        .map(split_commas)
        .unwrap_or_else(|| vec![initializer]);
    entries
        .into_iter()
        .map(|entry| {
            let entry = entry.trim();
            let symbols = parse_symbols(entry);
            if symbols.len() == 1 && entry.starts_with("ptr @") {
                Ok(symbols[0].clone())
            } else {
                Err(Error::Parse(format!(
                    "unsupported symbolic global initializer: {entry}"
                )))
            }
        })
        .collect()
}

fn parse_symbol_differences(initializer: &str, ty: &Type) -> Result<Vec<SymbolDifference>, Error> {
    if !matches!(ty, Type::Array(_, element) if **element == Type::Int(32)) {
        return Err(Error::Parse(
            "symbol differences require an i32 array".into(),
        ));
    }
    let entries = initializer
        .strip_prefix('[')
        .and_then(|text| text.strip_suffix(']'))
        .ok_or_else(|| Error::Parse("symbol differences require an array".into()))?;
    split_commas(entries)
        .into_iter()
        .map(|entry| {
            let symbols = parse_symbols(entry);
            if entry.trim().starts_with("i32 trunc (i64 sub (")
                && entry.trim().ends_with("to i32)")
                && symbols.len() == 2
            {
                Ok(SymbolDifference {
                    symbol: symbols[0].clone(),
                    base: symbols[1].clone(),
                })
            } else {
                Err(Error::Parse(format!(
                    "unsupported symbolic global initializer: {entry}"
                )))
            }
        })
        .collect()
}

fn split_type_value(text: &str) -> Result<(Type, &str), Error> {
    for index in 1..text.len() {
        if !text.is_char_boundary(index) || !text.as_bytes()[index].is_ascii_whitespace() {
            continue;
        }
        if let Ok(ty) = parse_type_text(&text[..index]) {
            return Ok((ty, text[index..].trim()));
        }
    }
    Err(Error::Parse(format!("expected type and value: {text}")))
}

fn parse_type_text(text: &str) -> Result<Type, Error> {
    let text = text.trim();
    if let Some(width) = text.strip_prefix('i').and_then(|v| v.parse().ok()) {
        return Ok(Type::Int(width));
    }
    match text {
        "void" => return Ok(Type::Void),
        "ptr" => return Ok(Type::Ptr(None)),
        "half" => return Ok(Type::Float(16)),
        "float" => return Ok(Type::Float(32)),
        "double" => return Ok(Type::Float(64)),
        _ => {}
    }
    if let Some(name) = text.strip_prefix('%') {
        return Ok(Type::Named(name.to_string()));
    }
    if let Some(inner) = text.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
        let (count, elem) = inner
            .split_once(" x ")
            .ok_or_else(|| Error::Parse(format!("invalid array type: {text}")))?;
        return Ok(Type::Array(
            count
                .parse()
                .map_err(|_| Error::Parse(format!("invalid array size: {count}")))?,
            Box::new(parse_type_text(elem)?),
        ));
    }
    if let Some(inner) = text.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
        return split_commas(inner)
            .into_iter()
            .map(parse_type_text)
            .collect::<Result<Vec<_>, _>>()
            .map(Type::Struct);
    }
    Err(Error::Parse(format!("unsupported type: {text}")))
}

fn split_commas(text: &str) -> Vec<&str> {
    let mut depth = 0i32;
    let mut start = 0;
    let mut parts = Vec::new();
    for (index, byte) in text.bytes().enumerate() {
        match byte {
            b'[' | b'{' | b'(' => depth += 1,
            b']' | b'}' | b')' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(text[start..index].trim());
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(text[start..].trim());
    parts
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

fn parse_integer_array(text: &str, ty: &Type) -> Result<Vec<u8>, Error> {
    let Type::Array(_, elem) = ty else {
        return Err(Error::Parse("array initializer has non-array type".into()));
    };
    let Type::Int(width) = elem.as_ref() else {
        return Err(Error::Parse(
            "unsupported aggregate global initializer".into(),
        ));
    };
    let bytes_per_element = width.div_ceil(8) as usize;
    let inner = text
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| Error::Parse("invalid array initializer".into()))?;
    let mut bytes = Vec::new();
    for item in split_commas(inner) {
        let value: i64 = item
            .split_whitespace()
            .last()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| Error::Parse(format!("invalid integer initializer: {item}")))?;
        bytes.extend_from_slice(&value.to_le_bytes()[..bytes_per_element]);
    }
    Ok(bytes)
}
