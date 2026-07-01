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
    "nneg",
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
    "writeonly",
    "fastcc",
    "coldcc",
    "tailcc",
];

pub fn parse_module(src: &str) -> Result<Module, Error> {
    let tokens = lex(src);
    let eoi = Span::from(src.len()..src.len());
    let input = tokens.as_slice().map(eoi, |(t, s)| (t, s));

    let (out, errors) = module().parse(input).into_output_errors();
    match out {
        Some(module) if errors.is_empty() => Ok(module),
        _ => Err(Error::Parse(
            errors
                .iter()
                .map(|e| format!("{e:?}"))
                .collect::<Vec<_>>()
                .join("; "),
        )),
    }
}

type Extra<'src> = extra::Err<Rich<'src, Token<'src>, Span>>;

fn module<'src, I>() -> impl Parser<'src, I, Module, Extra<'src>>
where
    I: ValueInput<'src, Token = Token<'src>, Span = Span>,
{
    // Skip attribute/linkage keywords wherever they decorate an operand or type.
    let skip = any()
        .filter(|t: &Token| matches!(t, Token::Ident(s) if SKIP.contains(s)))
        .repeated();

    let ty = {
        let base = select! {
            Token::IntTy(w) => Type::Int(w),
            Token::Ident("void") => Type::Void,
            Token::Ident("ptr") => Type::Ptr(None),
        };
        base.then(just(Token::Star).repeated().collect::<Vec<_>>())
            .map(|(base, stars)| {
                let mut ty = base;
                for _ in 0..stars.len() {
                    ty = Type::Ptr(Some(Box::new(ty)));
                }
                ty
            })
    };

    let operand = select! {
        Token::Local(n) => Operand::Ref(n.to_string()),
        Token::Int(v) => Operand::ConstInt(v),
        Token::Ident("true") => Operand::ConstInt(1),
        Token::Ident("false") => Operand::ConstInt(0),
    };

    let local = select! { Token::Local(n) => n.to_string() };
    // A branch target, `%name`; matches the stripped label defined at a block.
    let target = select! { Token::Local(n) => n.to_string() };
    // A block-label definition: a bare identifier or number followed by `:`.
    let block_label = select! {
        Token::Ident(s) => s.to_string(),
        Token::Int(n) => n.to_string(),
    };
    let binding = local.then_ignore(just(Token::Eq));

    let binop = select! {
        Token::Ident("add") => BinOp::Add,
        Token::Ident("sub") => BinOp::Sub,
        Token::Ident("mul") => BinOp::Mul,
        Token::Ident("and") => BinOp::And,
        Token::Ident("or") => BinOp::Or,
        Token::Ident("xor") => BinOp::Xor,
        Token::Ident("shl") => BinOp::Shl,
        Token::Ident("lshr") => BinOp::LShr,
        Token::Ident("ashr") => BinOp::AShr,
    };
    let binary = binding
        .clone()
        .then(binop)
        .then_ignore(skip)
        .then(ty.clone())
        .then(operand)
        .then_ignore(just(Token::Comma))
        .then(operand)
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
        .then(select! { Token::Ident(p) => p.to_string() })
        .then(ty.clone())
        .then(operand)
        .then_ignore(just(Token::Comma))
        .then(operand)
        .map(|((((result, pred), ty), lhs), rhs)| Inst::ICmp {
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
    };
    let cast = binding
        .clone()
        .then(castop)
        .then(ty.clone())
        .then(operand)
        .then_ignore(just(Token::Ident("to")))
        .then(ty.clone())
        .map(|((((result, op), from), value), to)| Inst::Cast {
            result,
            op,
            from,
            value,
            to,
        });

    let alloca = binding
        .clone()
        .then_ignore(just(Token::Ident("alloca")))
        .then_ignore(skip)
        .then(ty.clone())
        .map(|(result, ty)| Inst::Alloca { result, ty });

    let load = binding
        .clone()
        .then_ignore(just(Token::Ident("load")))
        .then_ignore(skip)
        .then(ty.clone())
        .then_ignore(just(Token::Comma))
        .then_ignore(ty.clone())
        .then(operand)
        .map(|((result, ty), ptr)| Inst::Load { result, ty, ptr });

    let store = just(Token::Ident("store"))
        .ignore_then(skip)
        .ignore_then(ty.clone())
        .then(operand)
        .then_ignore(just(Token::Comma))
        .then_ignore(ty.clone())
        .then(operand)
        .map(|((ty, value), ptr)| Inst::Store { ty, value, ptr });

    let br = {
        let dest = just(Token::Ident("label")).ignore_then(target);
        let uncond = dest.clone().map(|dest| Inst::Br { dest });
        let cond = ty
            .clone()
            .ignore_then(operand)
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
        let val = ty.clone().then(operand).map(|(ty, op)| Inst::Ret {
            value: Some((ty, op)),
        });
        just(Token::Ident("ret")).ignore_then(void.or(val))
    };

    let call = {
        let arg = ty.clone().then_ignore(skip).then(operand);
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
            .then(select! { Token::Global(n) => n.to_string() })
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
        cast,
        alloca,
        load,
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
        .or(inst.map(Item::Inst))
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
            functions: items.into_iter().flatten().collect(),
        })
}

#[derive(Clone)]
enum Item {
    Label(String),
    Inst(Inst),
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
            Item::Inst(inst) => blocks.last_mut().unwrap().insts.push(inst),
        }
    }
    Function {
        name,
        ret,
        params,
        blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_function() {
        let src = "define i32 @add(i32 %a, i32 %b) {\n  %s = add i32 %a, %b\n  ret i32 %s\n}\n";
        let module = parse_module(src).unwrap();
        assert_eq!(module.functions.len(), 1);
        let f = &module.functions[0];
        assert_eq!(f.name, "add");
        assert_eq!(f.ret, Type::Int(32));
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.blocks.len(), 1);
        assert_eq!(
            f.blocks[0].insts[0],
            Inst::Binary {
                result: "s".into(),
                op: BinOp::Add,
                ty: Type::Int(32),
                lhs: Operand::Ref("a".into()),
                rhs: Operand::Ref("b".into()),
            }
        );
    }

    #[test]
    fn splits_labelled_blocks() {
        let src = "define void @f(i1 %c) {\nentry:\n  br i1 %c, label %t, label %e\nt:\n  ret void\ne:\n  ret void\n}\n";
        let f = &parse_module(src).unwrap().functions[0];
        assert_eq!(f.blocks.len(), 3);
        assert_eq!(f.blocks[0].label.as_deref(), Some("entry"));
        assert!(matches!(f.blocks[0].insts[0], Inst::CondBr { .. }));
    }

    #[test]
    fn unknown_opcode_becomes_unsupported() {
        let src = "define i32 @f(i32 %x) {\n  %y = freeze i32 %x\n  ret i32 %y\n}\n";
        let f = &parse_module(src).unwrap().functions[0];
        assert_eq!(f.blocks[0].insts[0], Inst::Unsupported("freeze".into()));
    }

    #[test]
    fn skips_declarations_metadata_and_trailing_attrs() {
        let src = "target datalayout = \"e\"\n\
                   declare i32 @ext(i32)\n\
                   @g = global i32 0\n\
                   define i32 @f() {\n  %p = alloca i32, align 4\n  ret i32 0\n}\n\
                   !0 = !{}\n";
        let module = parse_module(src).unwrap();
        assert_eq!(module.functions.len(), 1);
        assert!(matches!(
            module.functions[0].blocks[0].insts[0],
            Inst::Alloca { .. }
        ));
    }
}
