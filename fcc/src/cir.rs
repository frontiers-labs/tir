use std::any::Any;
use std::sync::Arc;

use tir::attributes::AttributeValue;
use tir::parse::common::Cursor;
use tir::symbol_table::visibility_of;
use tir::{
    Context, Error, IRFormatter, Operation, Symbol, Terminator, TirType, TokenScope, Type,
    TypeConstraint, TypeId, Value, ValueId, Visibility, dialect, operation, parse::Span,
};

pub mod ops {
    pub use super::{
        BreakOp, ConditionOp, ContinueOp, CopyStructOp, DefineStructOp, DoOp, ForOp, GetMemberOp,
        GlobalOp, GlobalStringOp, GotoOp, IfOp, LabelOp, StringOp, VaArgOp, VaEndOp, VaStartOp,
        WhileOp, YieldOp, ZeroGlobalOp, r#break, condition, r#continue, copy_struct, define_struct,
        r#do, r#for, get_member, global, global_string, r#goto, r#if, label, string, va_arg,
        va_end, va_start, r#while, r#yield, zero_global,
    };
}

dialect! {
    CirDialect {
        name: "cir",
        operations: [
            StringOp,
            GlobalOp,
            GlobalStringOp,
            ZeroGlobalOp,
            DefineStructOp,
            GetMemberOp,
            CopyStructOp,
            VaStartOp,
            VaArgOp,
            VaEndOp,
            ForOp,
            WhileOp,
            DoOp,
            IfOp,
            ConditionOp,
            BreakOp,
            ContinueOp,
            GotoOp,
            LabelOp,
            YieldOp,
        ],
        types: [StructType, VarArgsType, VaListType],
    }
}

operation! {
    GlobalOp {
        name: "global",
        dialect: "cir",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            bytes: "Array",
            relocations: "Array",
            align: "UInt",
        },
    }
}

operation! {
    GlobalStringOp {
        name: "global_string",
        dialect: "cir",
        attributes: A {
            sym_name: "Str",
            value: "Str",
        },
    }
}

/// A named object: it owns its name outright, so it carries no signature and
/// cannot be overloaded.
macro_rules! data_symbol {
    ($op:ty) => {
        impl Symbol for $op {
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
                true
            }
        }
    };
}

data_symbol!(GlobalOp);
data_symbol!(ZeroGlobalOp);

impl GlobalStringOp {
    pub fn sym_name(&self) -> String {
        string_attribute(self, "sym_name")
    }

    pub fn value(&self) -> String {
        string_attribute(self, "value")
    }
}

impl GlobalOp {
    pub fn sym_name(&self) -> String {
        string_attribute(self, "sym_name")
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.attributes()
            .iter()
            .find(|attribute| attribute.name == "bytes")
            .and_then(|attribute| match &attribute.value {
                AttributeValue::Array(bytes) => bytes
                    .iter()
                    .map(|byte| match byte {
                        AttributeValue::UInt(value) => u8::try_from(*value).ok(),
                        _ => None,
                    })
                    .collect(),
                _ => None,
            })
            .unwrap()
    }

    pub fn align(&self) -> u64 {
        uint_attribute(self, "align")
    }

    pub fn relocations(&self) -> Vec<(u64, String, i64, u64)> {
        self.attributes()
            .iter()
            .find(|attribute| attribute.name == "relocations")
            .and_then(|attribute| match &attribute.value {
                AttributeValue::Array(relocations) => Some(
                    relocations
                        .iter()
                        .map(|relocation| {
                            let AttributeValue::Dict(fields) = relocation else {
                                unreachable!("cir.global relocation is a dictionary")
                            };
                            let AttributeValue::UInt(offset) = fields.get("offset").unwrap() else {
                                unreachable!("cir.global relocation has an offset")
                            };
                            let AttributeValue::Str(symbol) = fields.get("symbol").unwrap() else {
                                unreachable!("cir.global relocation has a symbol")
                            };
                            let AttributeValue::Int(addend) = fields.get("addend").unwrap() else {
                                unreachable!("cir.global relocation has an addend")
                            };
                            let AttributeValue::UInt(width) = fields.get("width").unwrap() else {
                                unreachable!("cir.global relocation has a width")
                            };
                            (*offset, symbol.clone(), *addend, *width)
                        })
                        .collect(),
                ),
                _ => None,
            })
            .unwrap()
    }
}

operation! {
    ZeroGlobalOp {
        name: "zero_global",
        dialect: "cir",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            size: "UInt",
            align: "UInt",
        },
    }
}

impl ZeroGlobalOp {
    pub fn sym_name(&self) -> String {
        string_attribute(self, "sym_name")
    }

    pub fn size(&self) -> u64 {
        uint_attribute(self, "size")
    }

    pub fn align(&self) -> u64 {
        uint_attribute(self, "align")
    }
}

fn string_attribute(operation: &impl Operation, name: &str) -> String {
    operation
        .attributes()
        .iter()
        .find(|attribute| attribute.name == name)
        .and_then(|attribute| match &attribute.value {
            AttributeValue::Str(value) => Some(value.clone()),
            _ => None,
        })
        .unwrap()
}

fn uint_attribute(operation: &impl Operation, name: &str) -> u64 {
    operation
        .attributes()
        .iter()
        .find(|attribute| attribute.name == name)
        .and_then(|attribute| match attribute.value {
            AttributeValue::UInt(value) => Some(value),
            _ => None,
        })
        .unwrap()
}

pub struct StructType {
    name: String,
}

impl StructType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context, name: impl Into<String>) -> TypeId {
        context.get_type_id(Arc::new(Self { name: name.into() }))
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl TypeConstraint for StructType {}

impl Type for StructType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "struct"
    }

    fn parse<'src>(
        _mnemonic: &str,
        parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        if !parser.parse_token("<") {
            return Err((parser.span(), Error::ExpectedToken("<")));
        }
        let name = parser
            .parse_string()
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("struct name")))?;
        if !parser.parse_token(">") {
            return Err((parser.span(), Error::ExpectedToken(">")));
        }
        Ok(Self::new(context, name))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write(format!("struct<\"{}\">", self.name))
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any)
            .downcast_ref::<StructType>()
            .is_some_and(|other| other.name == self.name)
    }

    fn hash(&self, state: &mut dyn std::hash::Hasher) {
        state.write(self.name.as_bytes());
    }
}

operation! {
    DefineStructOp {
        name: "define_struct",
        dialect: "cir",
        attributes: A {
            sym_name: "Str",
            fields: "Array",
            size: "UInt",
            align: "UInt",
        },
    }
}

operation! {
    GetMemberOp {
        name: "get_member",
        dialect: "cir",
        operands: O {
            base: "tir::ptr::PtrType",
        },
        attributes: A {
            field: "UInt",
            struct_name: "Str",
        },
        results: R {
            result: "tir::ptr::PtrType",
        },
    }
}

operation! {
    CopyStructOp {
        name: "copy_struct",
        dialect: "cir",
        operands: O {
            destination: "tir::ptr::PtrType",
            source: "tir::ptr::PtrType",
        },
        attributes: A {
            struct_name: "Str",
        },
    }
}

operation! {
    ForOp {
        name: "for",
        dialect: "cir",
        format: "custom",
        regions: R {
            condition_region: Region {
                single_block: true,
            },
            body: Region {},
            step_region: Region {
                single_block: true,
            },
        },
        interfaces: [TokenScope],
    }
}

impl TokenScope for ForOp {
    fn token_scope_regions(&self) -> Vec<tir::RegionId> {
        vec![self.0.regions[1]]
    }
}

impl ForOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let scope = region_scope(&context, self.0.regions[1]);
        fmt.write(format!("cir.for %{} cond", scope.number()))?;
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" body")?;
        tir::region_format::print_op_region(fmt, &context, self, 1)?;
        fmt.write(" step")?;
        tir::region_format::print_op_region(fmt, &context, self, 2)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let scope = parse_scope_token(parser, context)?;
        expect_token(parser, "cond")?;
        let condition_region = parser.parse_region(context)?.id();
        expect_token(parser, "body")?;
        let body = parser
            .parse_region_with_entry_args(context, vec![scope])?
            .id();
        expect_token(parser, "step")?;
        let step_region = parser.parse_region(context)?.id();
        Ok(Box::new(
            ForOpBuilder::new(context)
                .condition_region(condition_region)
                .body(body)
                .step_region(step_region)
                .build(),
        ))
    }
}

operation! {
    WhileOp {
        name: "while",
        dialect: "cir",
        format: "custom",
        regions: R {
            condition_region: Region {
                single_block: true,
            },
            body: Region {},
        },
        interfaces: [TokenScope],
    }
}

impl TokenScope for WhileOp {
    fn token_scope_regions(&self) -> Vec<tir::RegionId> {
        vec![self.0.regions[1]]
    }
}

impl WhileOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let scope = region_scope(&context, self.0.regions[1]);
        fmt.write(format!("cir.while %{} cond", scope.number()))?;
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" body")?;
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let scope = parse_scope_token(parser, context)?;
        expect_token(parser, "cond")?;
        let condition_region = parser.parse_region(context)?.id();
        expect_token(parser, "body")?;
        let body = parser
            .parse_region_with_entry_args(context, vec![scope])?
            .id();
        Ok(Box::new(
            WhileOpBuilder::new(context)
                .condition_region(condition_region)
                .body(body)
                .build(),
        ))
    }
}

operation! {
    DoOp {
        name: "do",
        dialect: "cir",
        format: "custom",
        regions: R {
            body: Region {},
            condition_region: Region {
                single_block: true,
            },
        },
        interfaces: [TokenScope],
    }
}

impl TokenScope for DoOp {
    fn token_scope_regions(&self) -> Vec<tir::RegionId> {
        vec![self.0.regions[0]]
    }
}

impl DoOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        let scope = region_scope(&context, self.0.regions[0]);
        fmt.write(format!("cir.do %{} body", scope.number()))?;
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" cond")?;
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let scope = parse_scope_token(parser, context)?;
        expect_token(parser, "body")?;
        let body = parser
            .parse_region_with_entry_args(context, vec![scope])?
            .id();
        expect_token(parser, "cond")?;
        let condition_region = parser.parse_region(context)?.id();
        Ok(Box::new(
            DoOpBuilder::new(context)
                .body(body)
                .condition_region(condition_region)
                .build(),
        ))
    }
}

operation! {
    IfOp {
        name: "if",
        dialect: "cir",
        format: "custom",
        operands: O {
            condition: "tir::Integer<1>",
        },
        regions: R {
            then_body: Region {},
            else_body: Region {},
        },
    }
}

impl IfOp {
    fn custom_print(&self, fmt: &mut IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
        fmt.write(format!("cir.if %{}", self.operands()[0].number()))?;
        tir::region_format::print_op_region(fmt, &context, self, 0)?;
        fmt.write(" else")?;
        tir::region_format::print_op_region(fmt, &context, self, 1)
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (Span, Error)> {
        let condition = parse_value_id(parser)?;
        let then_body = parser.parse_region(context)?.id();
        expect_token(parser, "else")?;
        let else_body = parser.parse_region(context)?.id();
        Ok(Box::new(
            IfOpBuilder::new(context)
                .condition(condition)
                .then_body(then_body)
                .else_body(else_body)
                .build(),
        ))
    }
}

operation! {
    ConditionOp {
        name: "condition",
        dialect: "cir",
        operands: O {
            condition: "tir::Integer<1>",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ConditionOp {}

operation! {
    BreakOp {
        name: "break",
        dialect: "cir",
        operands: O {
            scope: "tir::builtin::TokenType",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for BreakOp {}

operation! {
    ContinueOp {
        name: "continue",
        dialect: "cir",
        operands: O {
            scope: "tir::builtin::TokenType",
        },
        interfaces: [Terminator],
    }
}

impl Terminator for ContinueOp {}

operation! {
    GotoOp {
        name: "goto",
        dialect: "cir",
        attributes: A {
            label: "Str",
        },
    }
}

operation! {
    LabelOp {
        name: "label",
        dialect: "cir",
        attributes: A {
            label: "Str",
        },
    }
}

operation! {
    YieldOp {
        name: "yield",
        dialect: "cir",
        interfaces: [Terminator],
    }
}

impl Terminator for YieldOp {}

fn region_scope(context: &Context, region: tir::RegionId) -> ValueId {
    context
        .get_region(region)
        .iter(context.clone())
        .next()
        .unwrap()
        .arguments()[0]
        .id()
}

fn parse_value_id(parser: &mut tir::parse::text::Parser) -> Result<ValueId, (Span, Error)> {
    let value_ref = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    parser
        .resolve_value(value_ref)
        .ok_or_else(|| (parser.span(), Error::UnknownValueRef(value_ref.to_string())))
}

fn expect_token(
    parser: &mut tir::parse::text::Parser,
    token: &'static str,
) -> Result<(), (Span, Error)> {
    if parser.parse_token(token) {
        Ok(())
    } else {
        Err((parser.span(), Error::ExpectedToken(token)))
    }
}

fn parse_scope_token(
    parser: &mut tir::parse::text::Parser,
    context: &Context,
) -> Result<Value, (Span, Error)> {
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?
        .to_string();
    let scope = context.create_value(tir::builtin::TokenType::new(context), None);
    parser.define_value(&name, scope.id());
    Ok(scope)
}

#[derive(TirType)]
#[tir_type(dialect = "cir", name = "varargs")]
pub struct VarArgsType;

impl VarArgsType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for VarArgsType {}

impl Type for VarArgsType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "varargs"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("varargs")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<VarArgsType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

#[derive(TirType)]
#[tir_type(dialect = "cir", name = "va_list")]
pub struct VaListType;

impl VaListType {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(context: &Context) -> TypeId {
        context.get_type_id(Arc::new(Self))
    }
}

impl TypeConstraint for VaListType {}

impl Type for VaListType {
    fn dialect(&self) -> &'static str {
        "cir"
    }

    fn parse_key() -> &'static str {
        "va_list"
    }

    fn parse<'src>(
        _mnemonic: &str,
        _parser: &mut tir::parse::text::Parser<'src>,
        context: &Context,
    ) -> Result<TypeId, (Span, Error)> {
        Ok(Self::new(context))
    }

    fn print(&self, fmt: &mut IRFormatter<'_>) -> Result<(), std::fmt::Error> {
        fmt.write("va_list")
    }

    fn eq(&self, other: &dyn Type) -> bool {
        (other as &dyn Any).downcast_ref::<VaListType>().is_some()
    }

    fn hash(&self, _state: &mut dyn std::hash::Hasher) {}
}

operation! {
    StringOp {
        name: "string",
        dialect: "cir",
        attributes: A {
            value: "Str",
        },
        results: R {
            result: "tir::ptr::PtrType",
        },
    }
}

operation! {
    VaStartOp {
        name: "va_start",
        dialect: "cir",
        results: R {
            result: "crate::cir::VaListType",
        },
    }
}

operation! {
    VaArgOp {
        name: "va_arg",
        dialect: "cir",
        operands: O {
            list: "crate::cir::VaListType",
        },
        results: R {
            result: "tir::Any",
        },
    }
}

operation! {
    VaEndOp {
        name: "va_end",
        dialect: "cir",
        operands: O {
            list: "crate::cir::VaListType",
        },
    }
}

pub fn string_op(context: &Context, value: &str, result_type: TypeId) -> StringOp {
    StringOpBuilder::new(context)
        .attr("value", AttributeValue::Str(value.to_string()))
        .result_type(result_type)
        .build()
}
