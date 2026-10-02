//! Convert architectural immediate aliases into TMDL's encoded operands.
use crate::*;
use std::collections::{HashMap, HashSet};
use tir::attributes::{AttributeValue as Attr, NamedAttribute};
use tir::backend::{AsmCursor, AsmInstructionParser, Token, phys_attr};
use tir::parse::tokens::Parser;
use tir::{Context, NewOp};
type Tokens<'a> = Parser<'a, Token<'a>>;

pub(super) fn get_instruction_parsers(
    features: &[Feature],
) -> (HashMap<String, Vec<AsmInstructionParser>>, HashSet<String>) {
    let (mut parsers, disabled) = generated::parsers(features);
    for (mnemonic, feature, parser) in [
        ("mov", Feature::ARMv8A64, mov as AsmInstructionParser),
        ("lsl", Feature::ARMv8A64, lsl),
        ("orr", Feature::ARMv8A64, orr),
        ("tst", Feature::ARMv8A64, tst),
        ("ldp", Feature::FP, pair::<true>),
        ("stp", Feature::FP, pair::<false>),
        ("subs", Feature::ARMv8A64, subs),
    ] {
        if features.contains(&feature) {
            parsers
                .entry(mnemonic.into())
                .or_default()
                .insert(0, parser);
        }
    }
    (parsers, disabled)
}
fn comma(p: &mut Tokens<'_>) -> Result<(), ()> {
    matches!(p.bump(), Some(Token::Comma))
        .then_some(())
        .ok_or(())
}
fn integer(p: &mut Tokens<'_>) -> Result<i64, ()> {
    match p.bump() {
        Some(Token::DecNumber(n)) => n.parse().map_err(|_| ()),
        Some(Token::HexNumber(n)) => {
            let negative = n.starts_with('-');
            let value = u64::from_str_radix(&n.trim_start_matches('-')[2..], 16).map_err(|_| ())?;
            if negative {
                i64::try_from(-i128::from(value)).map_err(|_| ())
            } else {
                Ok(value as i64)
            }
        }
        _ => Err(()),
    }
}
fn word(p: &mut Tokens<'_>, stack: bool) -> Result<Attr, ()> {
    let (index, class) = if stack {
        (parse_gpr32sp(p), RegClass::GPR32sp)
    } else {
        (parse_gpr32(p), RegClass::GPR32)
    };
    Ok(phys_attr((class.id(), index.ok_or(())?)))
}
fn emit(
    c: &Context,
    b: &mut AsmCursor,
    p: &Tokens<'_>,
    name: &'static str,
    attrs: Vec<(&str, Attr)>,
) -> Result<(), ()> {
    if matches!(
        p.peek(),
        Some(Token::Comma | Token::LBracket | Token::RBracket)
    ) {
        return Err(());
    }
    let attrs = attrs
        .into_iter()
        .map(|(name, value)| NamedAttribute::new(c.intern(name), value))
        .collect();
    let op = c.add_operation(NewOp::new_dynamic(
        ("arm64", name),
        c.clone(),
        vec![],
        vec![],
        vec![],
        attrs,
    ));
    b.insert_id(op.id);
    Ok(())
}
fn mov(c: &Context, b: &mut AsmCursor, p: &mut Tokens<'_>) -> Result<(), ()> {
    let rd = word(p, false)?;
    comma(p)?;
    let value = integer(p)?;
    if !(-0x8000_0000..=0xffff_ffff).contains(&value) {
        return Err(());
    }
    let value = value as u32;
    let (name, mut attrs) = if value <= 65535 {
        ("movz_w", vec![("imm", Attr::Int(i64::from(value)))])
    } else if !value <= 65535 {
        (
            "movn_w",
            vec![("imm", Attr::Int(i64::from(!value))), ("hw", Attr::Int(0))],
        )
    } else {
        if rd == phys_attr((RegClass::GPR32.id(), 31)) {
            return Err(());
        }
        return logical(
            c,
            b,
            p,
            Some(rd),
            phys_attr((RegClass::GPR32.id(), 31)),
            32,
            u64::from(value),
        );
    };
    attrs.push(("rd", rd));
    emit(c, b, p, name, attrs)
}
fn lsl(c: &Context, b: &mut AsmCursor, p: &mut Tokens<'_>) -> Result<(), ()> {
    let rd = word(p, false)?;
    comma(p)?;
    let rn = word(p, false)?;
    comma(p)?;
    let shift = integer(p)?;
    if !(0..32).contains(&shift) {
        return Err(());
    }
    emit(
        c,
        b,
        p,
        "ubfm_w",
        vec![
            ("rd", rd),
            ("rn", rn),
            ("immr", Attr::Int((32 - shift) % 32)),
            ("imms", Attr::Int(31 - shift)),
        ],
    )
}
fn mask_fields(value: u64, width: u32) -> Option<(i64, i64, i64)> {
    let mask = u64::MAX >> (64 - width);
    if value == 0 || value == mask || value & !mask != 0 {
        return None;
    }
    for log in 1..=width.ilog2() {
        let size = 1_u32 << log;
        let element_mask = u64::MAX >> (64 - size);
        for ones in 1..size {
            let run = u64::MAX >> (64 - ones);
            for rotation in 0..size {
                let element =
                    ((run >> rotation) | (run << ((size - rotation) % size))) & element_mask;
                let repeated = element * (mask / element_mask);
                if repeated == value {
                    return Some((
                        i64::from(size == 64),
                        i64::from(rotation),
                        i64::from(((!(size * 2 - 1)) & 63) | (ones - 1)),
                    ));
                }
            }
        }
    }
    None
}
fn logical(
    c: &Context,
    b: &mut AsmCursor,
    p: &Tokens<'_>,
    rd: Option<Attr>,
    rn: Attr,
    width: u32,
    value: u64,
) -> Result<(), ()> {
    let (n, r, s) = mask_fields(value, width).ok_or(())?;
    let mut attrs = vec![
        ("rn", rn),
        ("n", Attr::Int(n)),
        ("immr", Attr::Int(r)),
        ("imms", Attr::Int(s)),
    ];
    let name = if let Some(rd) = rd {
        attrs.push(("rd", rd));
        "orr_imm_w"
    } else {
        "tst_imm"
    };
    emit(c, b, p, name, attrs)
}
fn orr(c: &Context, b: &mut AsmCursor, p: &mut Tokens<'_>) -> Result<(), ()> {
    let rd = word(p, true)?;
    comma(p)?;
    let rn = word(p, false)?;
    comma(p)?;
    let value = integer(p)? as u64;
    logical(c, b, p, Some(rd), rn, 32, value)
}
fn tst(c: &Context, b: &mut AsmCursor, p: &mut Tokens<'_>) -> Result<(), ()> {
    let rn = phys_attr((RegClass::GPR.id(), parse_gpr(p).ok_or(())?));
    comma(p)?;
    let value = integer(p)? as u64;
    logical(c, b, p, None, rn, 64, value)
}
fn expand_fmov(imm: u32) -> f32 {
    let bit = (imm >> 6) & 1;
    f32::from_bits(
        ((imm >> 7) << 31)
            | ((1 - bit) << 30)
            | ((0_u32.wrapping_sub(bit) & 31) << 25)
            | (((imm >> 4) & 3) << 23)
            | ((imm & 15) << 19),
    )
}
pub(super) fn parse_fmov_immediate(p: &mut Tokens<'_>) -> Result<i64, ()> {
    let value = match p.bump() {
        Some(Token::FloatNumber(n) | Token::DecNumber(n)) => n.parse::<f32>().map_err(|_| ())?,
        _ => return Err(()),
    };
    (0..256)
        .find(|&imm| expand_fmov(imm) == value)
        .map(i64::from)
        .ok_or(())
}
pub(super) fn print_fmov_immediate(imm: i64) -> Option<String> {
    (0..256)
        .contains(&imm)
        .then(|| expand_fmov(imm as u32).to_string())
}
pub(super) fn print_movi_immediate(imm: i64) -> Option<String> {
    let mask = (0..8).fold(0_u64, |mask, i| {
        mask | ((((imm as u64 >> i) & 1) * 255) << (8 * i))
    });
    (0..256).contains(&imm).then(|| format!("{mask:#x}"))
}
pub(super) fn parse_movi_immediate(p: &mut Tokens<'_>) -> Result<i64, ()> {
    let value = integer(p)? as u64;
    let mut imm = 0;
    for i in 0..8 {
        match (value >> (i * 8)) & 255 {
            0 => {}
            255 => imm |= 1 << i,
            _ => return Err(()),
        }
    }
    Ok(imm)
}
fn pair<const LOAD: bool>(c: &Context, b: &mut AsmCursor, p: &mut Tokens<'_>) -> Result<(), ()> {
    let rt = parse_qpr(p).ok_or(())?;
    comma(p)?;
    let rt2 = parse_qpr(p).ok_or(())?;
    comma(p)?;
    if !matches!(p.bump(), Some(Token::LBracket)) {
        return Err(());
    }
    let rn = parse_gprsp(p).ok_or(())?;
    if !matches!(p.bump(), Some(Token::RBracket)) {
        return Err(());
    }
    emit(
        c,
        b,
        p,
        if LOAD { "ldp_q" } else { "stp_q" },
        vec![
            ("rt", phys_attr((RegClass::QPR.id(), rt))),
            ("rt2", phys_attr((RegClass::QPR.id(), rt2))),
            ("rn", phys_attr((RegClass::GPRsp.id(), rn))),
            ("imm", Attr::Int(0)),
        ],
    )
}
pub(super) fn parse_condition(p: &mut Tokens<'_>) -> Result<i64, ()> {
    if !matches!(p.peek(), Some(Token::Ident(_))) {
        return integer(p);
    }
    let name = match p.parse_ident().ok_or(())? {
        "cs" => "hs",
        "cc" => "lo",
        name => name,
    };
    [
        "eq", "ne", "hs", "lo", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al",
        "nv",
    ]
    .iter()
    .position(|&v| v == name)
    .map(|v| v as i64)
    .ok_or(())
}
pub(super) fn parse_inverted_condition(p: &mut Tokens<'_>) -> Result<i64, ()> {
    if !matches!(p.peek(), Some(Token::Ident(_))) {
        return integer(p);
    }
    let cond = parse_condition(p)?;
    if cond >= 14 { Err(()) } else { Ok(cond ^ 1) }
}
fn subs(c: &Context, b: &mut AsmCursor, p: &mut Tokens<'_>) -> Result<(), ()> {
    let rd = phys_attr((RegClass::GPR.id(), parse_gpr(p).ok_or(())?));
    comma(p)?;
    let rn = phys_attr((RegClass::GPR.id(), parse_gpr(p).ok_or(())?));
    comma(p)?;
    let rm = phys_attr((RegClass::GPR.id(), parse_gpr(p).ok_or(())?));
    emit(
        c,
        b,
        p,
        "subs_lsl",
        vec![("rd", rd), ("rn", rn), ("rm", rm), ("shift", Attr::Int(0))],
    )
}
