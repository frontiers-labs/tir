use tir::helpers::operation;
use tir::{Any, Context, Error, Operation, Terminator, ValueId};

use tir;

operation! {
    ModuleOp {
        name: "module",
        dialect: "spirv",
        attributes: A {
            version: "Str",
            addressing_model: "Str",
            memory_model: "Str",
        },
        regions: R { body: Region { single_block: true, } }
    }
}

operation! {
    ModuleEndOp {
        name: "module_end",
        dialect: "spirv",
        interfaces: [Terminator],
    }
}

impl Terminator for ModuleEndOp {}

operation! {
    CapabilityOp {
        name: "Capability",
        dialect: "spirv",
        attributes: A { name: "Str", }
    }
}

operation! {
    GlobalVariableOp {
        name: "GlobalVariable",
        dialect: "spirv",
        attributes: A {
            sym_name: "Str",
            storage_class: "Str",
            decorations: "Dict",
        },
        results: R { result: "Any", }
    }
}

operation! {
    EntryPointOp {
        name: "EntryPoint",
        dialect: "spirv",
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
        attributes: A { value: "any", },
        results: R { result: "Any", }
    }
}

operation! {
    LoadOp {
        name: "Load",
        dialect: "spirv",
        operands: O { pointer: "Any", },
        results: R { result: "Any", }
    }
}

operation! {
    StoreOp {
        name: "Store",
        dialect: "spirv",
        operands: O { pointer: "Any", value: "Any", }
    }
}

operation! {
    ControlBarrierOp {
        name: "ControlBarrier",
        dialect: "spirv",
        operands: O { execution_scope: "Any", memory_scope: "Any", memory_semantics: "Any", }
    }
}

operation! {
    MemoryBarrierOp {
        name: "MemoryBarrier",
        dialect: "spirv",
        operands: O { memory_scope: "Any", memory_semantics: "Any", }
    }
}

operation! {
    CompositeExtractOp {
        name: "CompositeExtract",
        dialect: "spirv",
        attributes: A { indices: "Array", },
        operands: O { composite: "Any", },
        results: R { result: "Any", }
    }
}

operation! {
    AccessChainOp {
        name: "AccessChain",
        dialect: "spirv",
        format: "custom",
        operands: O { base: "Any", indices: "*Any", },
        results: R { result: "Any", }
    }
}

impl AccessChainOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        let context = self.0.context.upgrade();
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
        let base = parse_value(parser)?;
        let mut indices = Vec::new();
        while parser.parse_token(",") {
            indices.push(parse_value(parser)?);
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

    pub fn indices(&self) -> &[ValueId] {
        &self.operands()[1..]
    }
}

operation! {
    ReturnOp {
        name: "Return",
        dialect: "spirv",
        operands: O { value: "?Any", },
        interfaces: [Terminator],
    }
}

impl Terminator for ReturnOp {}

fn parse_value(
    parser: &mut tir::parse::text::Parser<'_>,
) -> Result<ValueId, (tir::parse::Span, Error)> {
    use tir::parse::common::Cursor;
    let name = parser
        .parse_value_ref()
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))?;
    parser
        .resolve_value(name)
        .ok_or_else(|| (parser.span(), Error::ExpectedValueRef))
}
