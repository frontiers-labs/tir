use tir::helpers::operation;
use tir::{Context, Error, Operation, Terminator, ValueId};

use tir;

operation! {
    ModuleOp {
        name: "module",
        dialect: "spirv",
        scheme: "() -> ()",
        attributes: A {
            version: "Str",
            addressing_model: "Str",
            memory_model: "Str",
        },
        regions: R { body: Region { kind: Blocks, } }
    }
}

operation! {
    ModuleEndOp {
        name: "module_end",
        dialect: "spirv",
        scheme: "() -> ()",
        interfaces: [Terminator],
    }
}

impl Terminator for ModuleEndOp {}

operation! {
    CapabilityOp {
        name: "Capability",
        dialect: "spirv",
        scheme: "() -> ()",
        attributes: A { name: "Str", }
    }
}

operation! {
    GlobalVariableOp {
        name: "GlobalVariable",
        dialect: "spirv",
        scheme: "() -> _",
        attributes: A {
            sym_name: "Str",
            storage_class: "Str",
            decorations: "Dict",
        },
        results: R { result }
    }
}

operation! {
    EntryPointOp {
        name: "EntryPoint",
        dialect: "spirv",
        scheme: "() -> ()",
        attributes: A {
            execution_model: "Str",
            function: "Str",
            interfaces: "Array",
        }
    }
}

operation! {
    ExecutionModeOp {
        name: "ExecutionMode",
        dialect: "spirv",
        scheme: "() -> ()",
        attributes: A {
            function: "Str",
            mode: "Str",
            values: "Array",
        }
    }
}

operation! {
    ConstantOp {
        name: "Constant",
        dialect: "spirv",
        scheme: "() -> _",
        attributes: A { value: "any", },
        results: R { result }
    }
}

operation! {
    LoadOp {
        name: "Load",
        dialect: "spirv",
        scheme: "(_) -> _",
        operands: O { pointer },
        results: R { result }
    }
}

operation! {
    StoreOp {
        name: "Store",
        dialect: "spirv",
        scheme: "(_, _) -> ()",
        operands: O { pointer, value }
    }
}

operation! {
    ControlBarrierOp {
        name: "ControlBarrier",
        dialect: "spirv",
        scheme: "(_, _, _) -> ()",
        operands: O { execution_scope, memory_scope, memory_semantics }
    }
}

operation! {
    MemoryBarrierOp {
        name: "MemoryBarrier",
        dialect: "spirv",
        scheme: "(_, _) -> ()",
        operands: O { memory_scope, memory_semantics }
    }
}

operation! {
    CompositeExtractOp {
        name: "CompositeExtract",
        dialect: "spirv",
        scheme: "(_) -> _",
        attributes: A { indices: "Array", },
        operands: O { composite },
        results: R { result }
    }
}

operation! {
    AccessChainOp {
        name: "AccessChain",
        dialect: "spirv",
        scheme: "(_, _*) -> _",
        format: "custom",
        operands: O { base, indices },
        results: R { result }
    }
}

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

operation! {
    ReturnOp {
        name: "Return",
        dialect: "spirv",
        scheme: "(_?) -> ()",
        operands: O { value },
        interfaces: [Terminator],
    }
}

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
