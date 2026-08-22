use crate::attributes::AttributeValue;
use crate::symbol_table::visibility_of;
use crate::{Context, Error, IRFormatter, Operation, Symbol, TypeId, Visibility, operation};

use crate as tir;

// A δ node: a global data object, producing the address of its storage.
//
// A definition carries an alignment and either an initializer (`bytes`, plus the
// `relocations` patched into it) or a zero-filled `size`. A declaration of an
// object another module defines carries `external` and neither.
operation! {
    GlobalOp {
        name: "global",
        dialect: "builtin",
        format: "custom",
        verifier: "true",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
        },
        results: R {
            result: "crate::ptr::PtrType",
        },
    }
}

impl Symbol for GlobalOp {
    fn symbol_name(&self) -> String {
        self.sym_name()
    }

    fn symbol_signature(&self) -> Option<Vec<TypeId>> {
        None
    }

    fn symbol_result_type(&self) -> Option<TypeId> {
        None
    }

    fn symbol_visibility(&self) -> Visibility {
        visibility_of(self)
    }

    fn is_definition(&self) -> bool {
        !self.is_external()
    }
}

impl tir::Verifiable for GlobalOp {
    fn verify_impl(&self, _context: &Context) -> Result<(), Error> {
        let name = self.sym_name();
        let defines = self.bytes().is_some() as u8 + self.size().is_some() as u8;
        if self.is_external() {
            if defines > 0 || self.align().is_some() {
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
        match self.align() {
            Some(align) if align.is_power_of_two() => Ok(()),
            _ => Err(Error::VerificationError(format!(
                "global '@{name}' must carry a power-of-two alignment"
            ))),
        }
    }
}

impl GlobalOp {
    /// The address of this object's storage.
    pub fn address(&self) -> crate::ValueId {
        self.result()
    }

    pub fn sym_name(&self) -> String {
        match self.attr("sym_name") {
            Some(AttributeValue::Str(name)) => name.to_string(),
            _ => panic!("global must carry sym_name"),
        }
    }

    /// Whether this only declares an object another module defines.
    pub fn is_external(&self) -> bool {
        self.attr("external") == Some(AttributeValue::Bool(true))
    }

    pub fn align(&self) -> Option<u64> {
        match self.attr("align")? {
            AttributeValue::UInt(align) => Some(align),
            _ => None,
        }
    }

    /// The zero-filled size, for an object with no initializer.
    pub fn size(&self) -> Option<u64> {
        match self.attr("size")? {
            AttributeValue::UInt(size) => Some(size),
            _ => None,
        }
    }

    /// The initializer image, before relocations are patched in.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        let AttributeValue::Array(bytes) = self.attr("bytes")? else {
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
    pub fn section(&self) -> Option<String> {
        match self.attr("section")? {
            AttributeValue::Str(name) => Some(name.to_string()),
            _ => None,
        }
    }

    /// `(offset, symbol, addend, width)` for each address patched into `bytes`.
    pub fn relocations(&self) -> Vec<(u64, String, i64, u64)> {
        let Some(AttributeValue::Array(relocations)) = self.attr("relocations") else {
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

    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        fmt.write(format!("%{} = global", self.address().number()))?;
        if self.symbol_visibility() == Visibility::Private {
            fmt.write(" private")?;
        }
        if self.is_external() {
            fmt.write(" external")?;
        }
        fmt.write(format!(" @{}", self.sym_name()))?;
        if let Some(size) = self.size() {
            fmt.write(format!(" size {size}"))?;
        }
        if let Some(align) = self.align() {
            fmt.write(format!(" align {align}"))?;
        }
        if let Some(section) = self.section() {
            fmt.write(format!(" section \"{section}\""))?;
        }
        if let Some(bytes) = self.attr("bytes") {
            fmt.write(" bytes ")?;
            bytes.print(fmt, &context)?;
        }
        if let Some(relocations) = self.attr("relocations") {
            fmt.write(" relocations ")?;
            relocations.print(fmt, &context)?;
        }
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        use tir::parse::common::Cursor;

        let is_private = parser.parse_token("private");
        let is_external = parser.parse_token("external");
        let sym_name = parser
            .parse_symbol_name()
            .ok_or_else(|| (parser.span(), Error::ExpectedSymbolName))?
            .to_string();

        let mut builder = GlobalOpBuilder::new(context)
            .attr("sym_name", AttributeValue::Str(sym_name.into()))
            .result_type(crate::ptr::PtrType::opaque(context));
        if is_private {
            builder = builder.attr(
                "sym_visibility",
                AttributeValue::Str("private".to_string().into()),
            );
        }
        if is_external {
            builder = builder.attr("external", AttributeValue::Bool(true));
        }
        if parser.parse_token("size") {
            builder = builder.attr("size", AttributeValue::UInt(parse_number(parser)?));
        }
        if parser.parse_token("align") {
            builder = builder.attr("align", AttributeValue::UInt(parse_number(parser)?));
        }
        if parser.parse_token("section") {
            let section = parser
                .parse_string()
                .ok_or_else(|| (parser.span(), Error::ExpectedToken("section name")))?
                .to_string();
            builder = builder.attr("section", AttributeValue::Str(section.into()));
        }
        for name in ["bytes", "relocations"] {
            if !parser.parse_token(name) {
                continue;
            }
            let value = parser
                .parse_attribute_value(context)?
                .ok_or_else(|| (parser.span(), Error::ExpectedToken("attribute value")))?;
            builder = builder.attr(name, value);
        }

        Ok(Box::new(builder.build()))
    }
}

fn parse_number(parser: &mut tir::parse::text::Parser) -> Result<u64, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    parser
        .parse_number()
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| (parser.span(), Error::ExpectedToken("a non-negative number")))
}

/// A δ definition holding `bytes`, aligned to `align`.
pub fn global_bytes(context: &Context, name: &str, bytes: Vec<u8>, align: u64) -> GlobalOpBuilder {
    GlobalOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string().into()))
        .attr(
            "bytes",
            AttributeValue::Array(
                bytes
                    .into_iter()
                    .map(|byte| AttributeValue::UInt(u64::from(byte)))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
        .attr("align", AttributeValue::UInt(align))
        .result_type(crate::ptr::PtrType::opaque(context))
}

/// A δ definition reserving `size` zero bytes, aligned to `align`.
pub fn global_zero(context: &Context, name: &str, size: u64, align: u64) -> GlobalOpBuilder {
    GlobalOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string().into()))
        .attr("size", AttributeValue::UInt(size))
        .attr("align", AttributeValue::UInt(align))
        .result_type(crate::ptr::PtrType::opaque(context))
}

/// A δ declaration of an object another module defines.
pub fn global_external(context: &Context, name: &str) -> GlobalOpBuilder {
    GlobalOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string().into()))
        .attr("external", AttributeValue::Bool(true))
        .result_type(crate::ptr::PtrType::opaque(context))
}

// The address of a symbol, materialized inside a function. Module-level λ and δ
// values are not reachable from machine code, so the backend rewrites the uses
// of one into this op ahead of instruction selection.
operation! {
    SymAddrOp {
        name: "sym_addr",
        dialect: "builtin",
        attributes: A {
            sym_name: "Str",
        },
        results: R {
            result: "crate::ptr::PtrType",
        },
        interfaces: [crate::Pure],
    }
}

impl crate::Pure for SymAddrOp {}

impl SymAddrOp {
    pub fn sym_name(&self) -> String {
        match self.attr("sym_name") {
            Some(AttributeValue::Str(name)) => name.to_string(),
            _ => panic!("sym_addr must carry sym_name"),
        }
    }
}

/// The address of `name`, as an opaque pointer.
pub fn symbol_address(context: &Context, name: &str) -> SymAddrOp {
    SymAddrOpBuilder::new(context)
        .attr("sym_name", AttributeValue::Str(name.to_string().into()))
        .result_type(crate::ptr::PtrType::opaque(context))
        .build()
}
