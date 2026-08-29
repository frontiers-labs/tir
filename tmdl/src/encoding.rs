//! Encoding field lists: resolving each field's width and laying the fields
//! out into instruction-word bit ranges.
//!
//! An `encoding` block lists fields the way the ISA's manual draws them: high
//! bit first within an *encoding unit*, units in emission order. The unit is
//! the ISA's `ENCODING_UNIT` parameter — 32 for a fixed-width word ISA, 8 for a
//! byte-stream ISA like x86, where the manual draws one byte at a time. With no
//! `ENCODING_UNIT` declared the whole encoding is a single unit, which is what
//! a fixed-width ISA wants.
//!
//! A field wider than one unit spans whole units and is little-endian across
//! them, matching how a multi-byte displacement or immediate reaches memory.

use std::collections::HashMap;

use chumsky::error::Rich;

use crate::Span;
use crate::ast;
use crate::expander::Diag;
use crate::types::Type;
use crate::utils::parse_literal_value;

/// Resolve every encoding field's width from the declared type of the operand,
/// parameter, register class or literal it names. Fields whose width cannot be
/// determined are reported; the rest of the pipeline can then assume a width.
pub fn resolve_encoding_widths(files: &mut [ast::File]) -> Vec<Diag> {
    let classes = register_class_widths(files);
    let mut diags = Vec::new();

    for file in files.iter_mut() {
        let file_name = file.file_name.clone();
        for item in &mut file.items {
            let (name, operands, params, encoding) = match item {
                ast::Item::Template(t) => (&t.name, &t.operands, &t.params, &mut t.encoding),
                ast::Item::Instruction(i) => (&i.name, &i.operands, &i.params, &mut i.encoding),
                _ => continue,
            };
            let operands: HashMap<String, Type> = operands.iter().cloned().collect();
            let params: HashMap<String, Type> = params
                .iter()
                .map(|(name, (ty, _))| (name.clone(), ty.clone()))
                .collect();
            let ctx = FieldContext {
                owner: name,
                operands: &operands,
                params: &params,
                classes: &classes,
            };
            for field in encoding.iter_mut() {
                match ctx.width_of(&field.value) {
                    Ok(width) => field.width = width,
                    Err(message) => {
                        diags.push((file_name.clone(), Rich::custom(field.span, message)))
                    }
                }
            }
        }
    }
    diags
}

/// `ENCODING_LEN` of every register class, i.e. the width of an operand of that
/// class in an encoding.
pub fn register_class_widths(files: &[ast::File]) -> HashMap<String, u16> {
    files
        .iter()
        .flat_map(|f| &f.items)
        .filter_map(|item| match item {
            ast::Item::RegisterClass(class) => {
                let width = class
                    .parameters
                    .get("ENCODING_LEN")
                    .and_then(|(_, value)| value.as_ref())
                    .and_then(|value| match value {
                        ast::Expr::Lit(ast::Lit::Int(li)) => Some(parse_literal_value(li) as u16),
                        _ => None,
                    })?;
                Some((class.name.clone(), width))
            }
            _ => None,
        })
        .collect()
}

struct FieldContext<'a> {
    owner: &'a str,
    operands: &'a HashMap<String, Type>,
    params: &'a HashMap<String, Type>,
    classes: &'a HashMap<String, u16>,
}

impl FieldContext<'_> {
    fn width_of(&self, value: &ast::Expr) -> Result<u16, String> {
        match value {
            ast::Expr::Lit(ast::Lit::Int(li)) => literal_width(li.value()),
            ast::Expr::Ident(id) => self.named_width(&id.name),
            ast::Expr::Slice(slc) => Ok(slc.hi - slc.lo + 1),
            ast::Expr::IndexAccess(_) => Ok(1),
            _ => Err(format!(
                "encoding field in '{}' must be a literal, parameter, operand or bit slice",
                self.owner
            )),
        }
    }

    /// The width a bare `name` contributes. A register operand contributes its
    /// class's `ENCODING_LEN`; anything whose width is an ISA-parameter
    /// expression has no single width and must be sliced explicitly.
    fn named_width(&self, name: &str) -> Result<u16, String> {
        let ty = self
            .operands
            .get(name)
            .or_else(|| self.params.get(name))
            .ok_or_else(|| {
                format!(
                    "Unknown '{name}' in encoding of instruction '{}': \
                     not a parameter or operand",
                    self.owner
                )
            })?;
        match ty {
            Type::Bits(width) => Ok(*width),
            Type::Struct(class) => self.classes.get(class).copied().ok_or_else(|| {
                format!(
                    "register class '{class}' of operand '{name}' in '{}' \
                     declares no ENCODING_LEN",
                    self.owner
                )
            }),
            Type::BitsExpr(_) => Err(format!(
                "width of '{name}' in '{}' depends on an ISA parameter; \
                 give the encoding field an explicit bit range",
                self.owner
            )),
            _ => Err(format!(
                "'{name}' in encoding of '{}' has no bit width",
                self.owner
            )),
        }
    }
}

/// Bits a literal spells: one per binary digit, four per hex digit. A decimal
/// literal spells no width, so it cannot stand in an encoding.
pub(crate) fn literal_width(spelling: &str) -> Result<u16, String> {
    if let Some(digits) = spelling.strip_prefix("0b").or(spelling.strip_prefix("0B")) {
        Ok(digits.len() as u16)
    } else if let Some(digits) = spelling.strip_prefix("0x").or(spelling.strip_prefix("0X")) {
        Ok(digits.len() as u16 * 4)
    } else {
        Err(format!(
            "decimal literal '{spelling}' has no width; write it in binary or hex"
        ))
    }
}

/// Lay `fields` out into instruction-word bit ranges, `unit` bits to an
/// encoding unit (the whole encoding when the ISA declares none). Returns
/// `None` when the fields do not fill a whole number of units, which
/// [`check_encoding_units`] reports.
pub fn encoding_arms(
    fields: &[ast::EncodingField],
    unit: Option<u16>,
) -> Option<Vec<ast::EncodingArm>> {
    let total: u16 = fields.iter().map(|f| f.width).sum();
    let unit = effective_unit(total, unit)?;

    let mut arms = Vec::with_capacity(fields.len());
    let mut pos = 0u16;
    for field in fields {
        let unit_index = pos / unit;
        let offset = pos % unit;
        // A field spanning whole units is little-endian across them, so it is
        // one run starting at the first of those units.
        let start = if field.width > unit - offset {
            if offset != 0 || !field.width.is_multiple_of(unit) {
                return None;
            }
            unit_index * unit
        } else {
            unit_index * unit + unit - offset - field.width
        };
        arms.push(ast::EncodingArm {
            start,
            end: Some(start + field.width - 1),
            value: field.value.clone(),
            span: field.span,
        });
        pos += field.width;
    }
    // Ascending by bit position: the order every consumer reads an encoding in.
    arms.sort_by_key(|arm| arm.start);
    Some(arms)
}

/// The unit `total` bits are actually divided into: an encoding shorter than
/// one unit is a single short unit, which is how a 16-bit compressed
/// instruction sits in a 32-bit-word ISA.
fn effective_unit(total: u16, unit: Option<u16>) -> Option<u16> {
    match unit {
        _ if total == 0 => None,
        None => Some(total),
        Some(unit) if total < unit => Some(total),
        Some(unit) if total.is_multiple_of(unit) => Some(unit),
        Some(_) => None,
    }
}

/// Whether `fields` fill a whole number of `unit`-bit encoding units, and every
/// field either fits inside one unit or spans whole ones.
pub fn check_encoding_units(
    fields: &[ast::EncodingField],
    unit: Option<u16>,
    owner: &str,
    span: Span,
    file_name: &str,
) -> Option<Diag> {
    if encoding_arms(fields, unit).is_some() {
        return None;
    }
    // With no declared unit the whole encoding is one, which always lays out.
    let total: u16 = fields.iter().map(|f| f.width).sum();
    let unit = unit.unwrap_or(total);
    let message = if effective_unit(total, Some(unit)).is_none() {
        format!(
            "encoding of instruction '{owner}' is {total} bits, \
             which is not a whole number of {unit}-bit encoding units"
        )
    } else {
        format!(
            "encoding of instruction '{owner}' has a field crossing an \
             {unit}-bit encoding unit boundary without filling whole units"
        )
    };
    Some((file_name.to_string(), Rich::custom(span, message)))
}

/// Bits per encoding unit for an instruction whose ISAs resolve to
/// `isa_params`, or `None` when they declare no `ENCODING_UNIT`.
pub fn encoding_unit(isa_params: &HashMap<String, i64>) -> Option<u16> {
    isa_params.get("ENCODING_UNIT").map(|unit| *unit as u16)
}
