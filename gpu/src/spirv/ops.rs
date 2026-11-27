#![allow(non_snake_case)]

use tir::{Context, Error, Operation, Terminator, ValueId};

include!(concat!(env!("OUT_DIR"), "/spirv_manual.rs"));

__cdsl_spirv_ModuleOp!();

__cdsl_spirv_ModuleEndOp!();

impl Terminator for ModuleEndOp {}

__cdsl_spirv_CapabilityOp!();

__cdsl_spirv_GlobalVariableOp!();

__cdsl_spirv_EntryPointOp!();

__cdsl_spirv_ExecutionModeOp!();

__cdsl_spirv_ConstantOp!();

__cdsl_spirv_LoadOp!();

__cdsl_spirv_StoreOp!();

__cdsl_spirv_ControlBarrierOp!();

__cdsl_spirv_MemoryBarrierOp!();

__cdsl_spirv_CompositeExtractOp!();

__cdsl_spirv_AccessChainOp!();

impl AccessChainOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.clone();
        fmt.write(format!(
            "%{} = spirv.AccessChain %{}",
            self.result().number(),
            self.base().number()
        ))?;
        for index in self.indices() {
            fmt.write(format!(", %{}", index.number()))?;
        }
        fmt.write(" : ")?;
        context.print_type(context.get_value(self.result()).ty(), fmt)?;
        fmt.write("\n")
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &Context,
    ) -> Result<Box<dyn Operation>, (tir::parse::Span, Error)> {
        use tir::parse::common::Cursor;
        let base = parse_value(parser, context)?;
        let mut indices = Vec::new();
        while parser.parse_token(",") {
            indices.push(parse_value(parser, context)?);
        }
        if !parser.parse_token(":") {
            return Err((parser.span(), Error::ExpectedToken(":")));
        }
        let result_type = parser
            .parse_type(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedType))?;
        Ok(Box::new(
            AccessChainOpBuilder::new(context)
                .base(base)
                .indices(indices)
                .result_type(result_type)
                .build(),
        ))
    }

    pub fn base(&self) -> ValueId {
        self.operands()[0]
    }

    pub fn indices(&self) -> tir::ValueIds {
        self.operands()[1..].into()
    }
}

__cdsl_spirv_ReturnOp!();

impl Terminator for ReturnOp {}

fn parse_value(
    parser: &mut tir::parse::text::Parser<'_>,
    context: &Context,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    Ok(parser.resolve_value(context, name))
}
