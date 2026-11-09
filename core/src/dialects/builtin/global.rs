use crate::attributes::AttributeValue;
use crate::symbol_table::visibility_of;
use crate::{Context, Error, Global, IRFormatter, Symbol, TypeId, Visibility};

use crate as tir;

/// A truncated address difference patched into a global after data layout.
pub struct SymbolDifference {
    pub offset: u64,
    pub symbol: String,
    pub base: String,
    pub width: u64,
}

// A δ node: a global data object, producing the address of its storage.
//
// A definition carries an alignment and either an initializer (`bytes`, plus the
// `relocations` patched into it) or a zero-filled `size`. A declaration of an
// object another module defines carries `external` and neither.
tir::helpers::op_def! {
    file: "defs/converted/dialects_builtin_global.cdsl",
    hosts: {
        global_verifier => crate::builtin::global_verify_impl,
        global_iface_Symbol => (Symbol, &crate::builtin::__TIR_IFACE_GlobalOp_Symbol),
        global_iface_Global => (Global, &crate::builtin::__TIR_IFACE_GlobalOp_Global),
        global_printer => crate::builtin::global_custom_print,
        global_parser => |parser, context, _definition| crate::builtin::global_custom_parse(parser, context),
    },
}

pub(crate) fn global_global_address(operation: &tir::OpHandle) -> crate::ValueId {
    crate::builtin::global_result(operation)
}

pub(crate) fn global_global_initializer(operation: &tir::OpHandle) -> Option<Vec<u8>> {
    crate::builtin::global_bytes_attr(operation)
        .or_else(|| crate::builtin::global_size(operation).map(|size| vec![0; size as usize]))
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_GlobalOp_Global: crate::GlobalCallbacks = crate::GlobalCallbacks {
    address: crate::builtin::global_global_address,
    initializer: crate::builtin::global_global_initializer,
};

pub(crate) fn global_symbol_symbol_name(operation: &tir::OpHandle) -> String {
    crate::builtin::global_sym_name(operation)
}

pub(crate) fn global_symbol_symbol_signature(__operation: &tir::OpHandle) -> Option<Vec<TypeId>> {
    None
}

pub(crate) fn global_symbol_symbol_result_type(__operation: &tir::OpHandle) -> Option<TypeId> {
    None
}

pub(crate) fn global_symbol_symbol_visibility(operation: &tir::OpHandle) -> Visibility {
    visibility_of(operation)
}

pub(crate) fn global_symbol_is_definition(operation: &tir::OpHandle) -> bool {
    !crate::builtin::global_is_external(operation)
}

#[allow(non_upper_case_globals)]
pub(crate) static __TIR_IFACE_GlobalOp_Symbol: crate::SymbolCallbacks = crate::SymbolCallbacks {
    symbol_name: crate::builtin::global_symbol_symbol_name,
    symbol_signature: crate::builtin::global_symbol_symbol_signature,
    symbol_result_type: crate::builtin::global_symbol_symbol_result_type,
    symbol_visibility: crate::builtin::global_symbol_symbol_visibility,
    is_definition: crate::builtin::global_symbol_is_definition,
};

pub(crate) fn global_verify_impl(
    operation: &tir::OpHandle,
    _context: &Context,
) -> Result<(), Error> {
    crate::builtin::global_symbol_differences(operation)?;
    let name = crate::builtin::global_sym_name(operation);
    let defines = crate::builtin::global_bytes_attr(operation).is_some() as u8
        + crate::builtin::global_size(operation).is_some() as u8;
    if crate::builtin::global_is_external(operation) {
        if defines > 0 || crate::builtin::global_align(operation).is_some() {
            return Err(Error::VerificationError(format!(
                "external global '@{name}' declares storage it does not define"
            )));
        }
        return Ok(());
    }
    if defines != 1 {
        return Err(Error::VerificationError(format!(
            "global '@{name}' must carry either an initializer or a zero-filled size"
        )));
    }
    match crate::builtin::global_align(operation) {
        Some(align) if align.is_power_of_two() => Ok(()),
        _ => Err(Error::VerificationError(format!(
            "global '@{name}' must carry a power-of-two alignment"
        ))),
    }
}

/// Symbol differences whose operands must resolve in the same object section.
pub fn global_symbol_differences(
    operation: &tir::OpHandle,
) -> Result<Vec<SymbolDifference>, Error> {
    let invalid = || {
        Error::VerificationError(format!(
            "invalid symbol differences in global '@{}'",
            crate::builtin::global_sym_name(operation)
        ))
    };
    let Some(attribute) = operation.attr("symbol_differences") else {
        return Ok(Vec::new());
    };
    let AttributeValue::Array(entries) = attribute else {
        return Err(invalid());
    };
    let size = crate::builtin::global_bytes_attr(operation)
        .map(|bytes| bytes.len() as u64)
        .or_else(|| crate::builtin::global_size(operation))
        .ok_or_else(invalid)?;
    let number = |attribute: &AttributeValue| match attribute {
        AttributeValue::UInt(value) => Some(*value),
        AttributeValue::Int(value) => u64::try_from(*value).ok(),
        _ => None,
    };
    entries
        .iter()
        .map(|entry| {
            let AttributeValue::Dict(fields) = entry else {
                return Err(invalid());
            };
            let (
                Some(offset),
                Some(AttributeValue::Str(symbol)),
                Some(AttributeValue::Str(base)),
                Some(width),
            ) = (
                fields.get("offset").and_then(number),
                fields.get("symbol"),
                fields.get("base"),
                fields.get("width").and_then(number),
            )
            else {
                return Err(invalid());
            };
            if !matches!(width, 1 | 2 | 4 | 8)
                || offset.checked_add(width).is_none_or(|end| end > size)
            {
                return Err(invalid());
            }
            Ok(SymbolDifference {
                offset,
                symbol: symbol.to_string(),
                base: base.to_string(),
                width,
            })
        })
        .collect()
}

/// The address of this object's storage.
pub fn global_address(operation: &tir::OpHandle) -> crate::ValueId {
    crate::builtin::global_result(operation)
}

/// Whether this only declares an object another module defines.
pub fn global_is_external(operation: &tir::OpHandle) -> bool {
    operation.attr("external") == Some(AttributeValue::Bool(true))
}

pub fn global_align(operation: &tir::OpHandle) -> Option<u64> {
    match operation.attr("align")? {
        AttributeValue::UInt(align) => Some(align),
        _ => None,
    }
}

/// The zero-filled size, for an object with no initializer.
pub fn global_size(operation: &tir::OpHandle) -> Option<u64> {
    match operation.attr("size")? {
        AttributeValue::UInt(size) => Some(size),
        _ => None,
    }
}

/// The initializer image, before relocations are patched in.
pub fn global_bytes_attr(operation: &tir::OpHandle) -> Option<Vec<u8>> {
    let AttributeValue::Array(bytes) = operation.attr("bytes")? else {
        return None;
    };
    bytes
        .iter()
        .map(|byte| match byte {
            AttributeValue::UInt(value) => u8::try_from(*value).ok(),
            AttributeValue::Int(value) => u8::try_from(*value).ok(),
            _ => None,
        })
        .collect()
}

/// The section the object belongs to, when it is not the default for its
/// initializer.
pub fn global_section(operation: &tir::OpHandle) -> Option<String> {
    match operation.attr("section")? {
        AttributeValue::Str(name) => Some(name.to_string()),
        _ => None,
    }
}

/// `(offset, symbol, addend, width)` for each address patched into `bytes`.
pub fn global_relocations(operation: &tir::OpHandle) -> Vec<(u64, String, i64, u64)> {
    let Some(AttributeValue::Array(relocations)) = operation.attr("relocations") else {
        return Vec::new();
    };
    relocations
        .iter()
        .map(|relocation| {
            let AttributeValue::Dict(fields) = relocation else {
                unreachable!("a global relocation is a dictionary")
            };
            let AttributeValue::UInt(offset) = fields.get("offset").unwrap() else {
                unreachable!("a global relocation has an offset")
            };
            let AttributeValue::Str(symbol) = fields.get("symbol").unwrap() else {
                unreachable!("a global relocation has a symbol")
            };
            let AttributeValue::Int(addend) = fields.get("addend").unwrap() else {
                unreachable!("a global relocation has an addend")
            };
            let AttributeValue::UInt(width) = fields.get("width").unwrap() else {
                unreachable!("a global relocation has a width")
            };
            (*offset, symbol.to_string(), *addend, *width)
        })
        .collect()
}

pub(crate) fn global_custom_print(
    operation: &tir::OpHandle,
    fmt: &mut IRFormatter,
) -> Result<(), std::fmt::Error> {
    let context = operation.context.clone();
    fmt.write(format!(
        "%{} = global",
        crate::builtin::global_address(operation).number()
    ))?;
    if crate::builtin::global_symbol_symbol_visibility(operation) == Visibility::Private {
        fmt.write(" private")?;
    }
    if crate::builtin::global_is_external(operation) {
        fmt.write(" external")?;
    }
    fmt.write(format!(" @{}", crate::builtin::global_sym_name(operation)))?;
    if let Some(size) = crate::builtin::global_size(operation) {
        fmt.write(format!(" size {size}"))?;
    }
    if let Some(align) = crate::builtin::global_align(operation) {
        fmt.write(format!(" align {align}"))?;
    }
    if let Some(section) = crate::builtin::global_section(operation) {
        fmt.write(format!(" section \"{section}\""))?;
    }
    if let Some(bytes) = operation.attr("bytes") {
        fmt.write(" bytes ")?;
        bytes.print(fmt, &context)?;
    }
    if let Some(relocations) = operation.attr("relocations") {
        fmt.write(" relocations ")?;
        relocations.print(fmt, &context)?;
    }
    if let Some(differences) = operation.attr("symbol_differences") {
        fmt.write(" symbol_differences ")?;
        differences.print(fmt, &context)?;
    }
    fmt.write("\n")
}

pub(crate) fn global_custom_parse(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<tir::OpHandle, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;

    let is_private = parser.parse_token("private");
    let is_external = parser.parse_token("external");
    let sym_name = parser
        .parse_symbol_name()
        .ok_or_else(|| (parser.span(), Error::ExpectedSymbolName))?
        .to_string();

    let mut parts = tir::NewOpParts::default();
    if is_private {
        parts.attributes.push(context.named_attribute(
            "sym_visibility",
            AttributeValue::Str("private".to_string().into()),
        ));
    }
    if is_external {
        parts
            .attributes
            .push(context.named_attribute("external", AttributeValue::Bool(true)));
    }
    if parser.parse_token("size") {
        parts
            .attributes
            .push(context.named_attribute("size", AttributeValue::UInt(parse_number(parser)?)));
    }
    if parser.parse_token("align") {
        parts
            .attributes
            .push(context.named_attribute("align", AttributeValue::UInt(parse_number(parser)?)));
    }
    if parser.parse_token("section") {
        let section = parser
            .parse_string()
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("section name")))?
            .to_string();
        parts
            .attributes
            .push(context.named_attribute("section", AttributeValue::Str(section.into())));
    }
    for name in ["bytes", "relocations", "symbol_differences"] {
        if !parser.parse_token(name) {
            continue;
        }
        let value = parser
            .parse_attribute_value(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("attribute value")))?;
        parts.attributes.push(context.named_attribute(name, value));
    }

    Ok(r#global(
        context,
        sym_name.as_str(),
        crate::ptr::PtrType::opaque(context),
        parts,
        tir::InsertionPoint::Root,
    ))
}

fn parse_number(parser: &mut tir::parse::text::Parser) -> Result<u64, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    parser
        .parse_number()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("a non-negative number")))
}

/// A δ definition holding `bytes`, aligned to `align`.
pub fn global_bytes(
    context: &Context,
    name: &str,
    bytes: Vec<u8>,
    align: u64,
    mut parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> crate::OpHandle {
    parts.attributes.insert(
        0,
        context.named_attribute(
            "bytes",
            AttributeValue::Array(
                bytes
                    .into_iter()
                    .map(|byte| AttributeValue::UInt(u64::from(byte)))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        ),
    );
    parts.attributes.insert(
        1,
        context.named_attribute("align", AttributeValue::UInt(align)),
    );
    r#global(
        context,
        name,
        crate::ptr::PtrType::opaque(context),
        parts,
        insertion,
    )
}

/// A δ definition reserving `size` zero bytes, aligned to `align`.
pub fn global_zero(
    context: &Context,
    name: &str,
    size: u64,
    align: u64,
    mut parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> crate::OpHandle {
    parts.attributes.insert(
        0,
        context.named_attribute("size", AttributeValue::UInt(size)),
    );
    parts.attributes.insert(
        1,
        context.named_attribute("align", AttributeValue::UInt(align)),
    );
    r#global(
        context,
        name,
        crate::ptr::PtrType::opaque(context),
        parts,
        insertion,
    )
}

/// A δ declaration of an object another module defines.
pub fn global_external(
    context: &Context,
    name: &str,
    mut parts: tir::NewOpParts,
    insertion: tir::InsertionPoint,
) -> crate::OpHandle {
    parts.attributes.insert(
        0,
        context.named_attribute("external", AttributeValue::Bool(true)),
    );
    r#global(
        context,
        name,
        crate::ptr::PtrType::opaque(context),
        parts,
        insertion,
    )
}
