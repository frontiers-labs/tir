use crate::Any;
use crate::builtin::UnitType;
use crate::operation;
use crate::symbol_table::{symbol_name_of, visibility_of};

use crate as tir;
use crate::{Context, Error, Operation, RegionExit, Symbol, Terminator, Visibility};

operation! {
    FuncOp {
        name: "func",
        dialect: "func",
        format: "custom",
        verifier: "true",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            ret_type: "Type",
        },
        results: R {
            result: "crate::builtin::FnType",
        },
        regions: R {
            body: Region {
                kind: Any,
            }
        }
    }
}

/// A λ definition named `name`, its `!fn` type read off the entry block of
/// `body` so the signature can never disagree with the parameters.
pub fn lambda(
    context: &Context,
    name: &str,
    ret_type: tir::TypeId,
    body: &tir::RegionHandle,
) -> FuncOpBuilder {
    let parameters: Vec<_> = body.ports().iter().map(tir::Value::ty).collect();
    FuncOpBuilder::new(context)
        .sym_name(name)
        .ret_type(ret_type)
        .result_type(tir::builtin::FnType::new(context, &parameters, ret_type))
        .body(body.id())
}

impl FuncOpBuilder {
    pub fn sym_name(self, name: &str) -> Self {
        self.attr(
            "sym_name",
            tir::attributes::AttributeValue::Str(name.to_string().into()),
        )
    }

    pub fn ret_type(self, ty: tir::TypeId) -> Self {
        self.attr("ret_type", tir::attributes::AttributeValue::Type(ty))
    }

    pub fn result_address(self) -> Self {
        self.attr(
            "result_address",
            tir::attributes::AttributeValue::Bool(true),
        )
    }

    pub fn noalias(self, arguments: &[usize]) -> Self {
        self.attr(
            "noalias",
            tir::attributes::AttributeValue::Array(
                arguments
                    .iter()
                    .map(|&argument| tir::attributes::AttributeValue::UInt(argument as u64))
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
    }

    pub fn argument_alignments(self, alignments: &[u64]) -> Self {
        self.attr(
            "argument_alignments",
            tir::attributes::AttributeValue::Array(
                alignments
                    .iter()
                    .copied()
                    .map(tir::attributes::AttributeValue::UInt)
                    .collect::<Vec<_>>()
                    .into(),
            ),
        )
    }
}

impl FuncOp {
    /// The region holding the body, whichever kind it is. [`FuncOp::body`]
    /// answers with the entry block, which only an ordered body has.
    pub fn body_region(&self) -> tir::RegionHandle {
        use tir::Operation;
        self.regions().next().expect("a function owns its body")
    }

    /// The function's parameters: the body region's arguments.
    pub fn parameters(&self) -> Vec<tir::Value> {
        self.body_region().ports()
    }

    /// The λ value this definition produces: what a call to it takes as callee.
    pub fn fn_value(&self) -> tir::ValueId {
        self.result()
    }

    pub fn has_result_address(&self) -> bool {
        self.attr("result_address") == Some(tir::attributes::AttributeValue::Bool(true))
    }

    pub fn ret_type(&self) -> tir::TypeId {
        match self.attr("ret_type") {
            Some(tir::attributes::AttributeValue::Type(ty)) => ty,
            _ => panic!("func must carry ret_type"),
        }
    }

    pub fn argument_alignments(&self) -> Vec<u64> {
        super::argument_alignments(self)
    }

    /// The parameters the caller guarantees name memory nothing else the
    /// function reaches names: a `restrict`-qualified pointer, by index.
    pub fn noalias_arguments(&self) -> Vec<usize> {
        super::noalias_arguments(self)
    }
}

impl Symbol for FuncOp {
    fn symbol_name(&self) -> String {
        symbol_name_of(self)
    }

    fn symbol_signature(&self) -> Option<Vec<tir::TypeId>> {
        Some(self.parameters().iter().map(tir::Value::ty).collect())
    }

    fn symbol_result_type(&self) -> Option<tir::TypeId> {
        Some(self.ret_type())
    }

    fn symbol_visibility(&self) -> Visibility {
        visibility_of(self)
    }

    fn is_definition(&self) -> bool {
        true
    }
}

impl FuncOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        use tir::Operation;

        // %2 = func.func @name(%0: i32, %1: i32) -> i32 {
        fmt.write(format!("%{} = func.func", self.fn_value().number()))?;
        if self.symbol_visibility() == Visibility::Private {
            fmt.write(" private")?;
        }

        // Print symbol name
        let sym_name = match self.attr("sym_name") {
            Some(tir::attributes::AttributeValue::Str(s)) => s.to_string(),
            Some(_) => panic!("sym_name must be a string"),
            None => "unknown".to_string(),
        };

        fmt.write(format!(" @{}", sym_name))?;

        // Print parameters from entry block arguments
        let context = self.0.context.upgrade();
        let args = self.parameters();

        fmt.write("(")?;
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                fmt.write(", ")?;
            }
            fmt.write(format!("%{}: ", arg.id().number()))?;
            context.print_type(arg.ty(), fmt)?;
        }
        fmt.write(")")?;

        // Print return type
        let ret_type = self.ret_type();

        if ret_type != UnitType::new(&context) {
            fmt.write(" -> ")?;
            context.print_type(ret_type, fmt)?;
        }
        if self.has_result_address() {
            fmt.write(" result_address")?;
        }
        super::print_argument_alignments(fmt, &self.argument_alignments())?;
        super::print_noalias_arguments(fmt, &self.noalias_arguments())?;

        tir::region_format::print_op_region(fmt, &context, self, 0)?;

        Ok(())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        use tir::parse::common::Cursor;

        let is_private = parser.parse_token("private");

        // Parse @name
        let sym_name = parser
            .parse_symbol_name()
            .ok_or_else(|| (parser.span(), tir::Error::ExpectedSymbolName))?
            .to_string();

        // Parse parameter list: (%0: type, %1: type)
        if !parser.parse_token("(") {
            return Err((parser.span(), tir::Error::ExpectedToken("(")));
        }

        let mut block_args = vec![];

        if !parser.parse_token(")") {
            loop {
                let val_name = parser
                    .parse_value_ref()
                    .ok_or_else(|| (parser.span(), tir::Error::ExpectedValueRef))?
                    .to_string();

                if !parser.parse_token(":") {
                    return Err((parser.span(), tir::Error::ExpectedToken(":")));
                }

                let ty = parser
                    .parse_type(context)?
                    .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?;

                // Create a value in context with the parsed type
                let value = context.create_value(ty, None);
                parser.define_value(&val_name, value.id());
                block_args.push(value);

                if parser.parse_token(")") {
                    break;
                }
                if !parser.parse_token(",") {
                    return Err((parser.span(), tir::Error::ExpectedToken(",")));
                }
            }
        }

        // Parse optional -> return_type
        let ret_type = if parser.parse_token("->") {
            parser
                .parse_type(context)?
                .ok_or_else(|| (parser.span(), tir::Error::ExpectedType))?
        } else {
            UnitType::new(context)
        };
        let result_address = parser.parse_token("result_address");
        let argument_alignments = super::parse_argument_alignments(parser, context)?;
        let noalias = super::parse_noalias_arguments(parser, context)?;

        // Parse body region { ... }
        let block_arg_types: Vec<tir::TypeId> = block_args.iter().map(tir::Value::ty).collect();
        let body_region = parser.parse_region_with_entry_args(context, block_args)?;

        let parameters: Vec<_> = block_arg_types;
        let mut builder = FuncOpBuilder::new(context)
            .sym_name(&sym_name)
            .ret_type(ret_type)
            .result_type(tir::builtin::FnType::new(context, &parameters, ret_type))
            .body(body_region.id());
        if result_address {
            builder = builder.result_address();
        }
        if let Some(argument_alignments) = argument_alignments {
            builder = builder.attr("argument_alignments", argument_alignments);
        }
        if let Some(noalias) = noalias {
            builder = builder.attr("noalias", noalias);
        }
        if is_private {
            builder = builder.attr(
                "sym_visibility",
                tir::attributes::AttributeValue::Str("private".to_string().into()),
            );
        }

        Ok(Box::new(builder.build()))
    }
}

impl FuncOp {
    /// An unordered body names the values it produces, so what it names is what
    /// the signature says the function returns. An ordered body says the same
    /// thing through its [`RegionExit`] operations.
    fn verify_nodes_results(&self, context: &Context) -> Result<(), Error> {
        let body = self.body_region();
        if !body.is_nodes() {
            return Ok(());
        }
        let state = tir::builtin::StateType::new(context);
        let produced: Vec<tir::TypeId> = body
            .results()
            .iter()
            .map(|result| context.get_value(*result).ty())
            .filter(|ty| *ty != state)
            .collect();
        let declared = match self.ret_type() {
            unit if unit == UnitType::new(context) => vec![],
            ty => vec![ty],
        };
        if produced == declared {
            return Ok(());
        }
        let spell = |types: &[tir::TypeId]| match types {
            [one] => context.type_to_string(*one),
            many => format!(
                "({})",
                many.iter()
                    .map(|ty| context.type_to_string(*ty))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        Err(Error::VerificationError(format!(
            "function '@{}' produces {}, but its signature returns {}",
            self.symbol_name(),
            spell(&produced),
            spell(&declared)
        )))
    }
}

impl tir::Verifiable for FuncOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        super::verify_argument_alignments(self, self.parameters().len(), "function")?;
        let parameters: Vec<_> = self.parameters().iter().map(tir::Value::ty).collect();
        super::verify_noalias_arguments(self, context, &parameters)?;
        let expected = tir::builtin::FnType::new(context, &parameters, self.ret_type());
        if context.get_value(self.fn_value()).ty() != expected {
            return Err(Error::VerificationError(format!(
                "function '@{}' produces {}, but its signature is {}",
                self.symbol_name(),
                context.type_to_string(context.get_value(self.fn_value()).ty()),
                context.type_to_string(expected)
            )));
        }
        self.verify_nodes_results(context)?;
        if !self.has_result_address() {
            return Ok(());
        }
        let Some(argument) = self.parameters().first().cloned() else {
            return Err(Error::VerificationError(
                "result-address function requires a destination argument".to_string(),
            ));
        };
        let ty = context.get_type_data(argument.ty());
        if (ty.as_ref() as &dyn std::any::Any)
            .downcast_ref::<crate::ptr::PtrType>()
            .is_none()
        {
            return Err(Error::VerificationError(
                "result-address function destination must have pointer type".to_string(),
            ));
        }
        Ok(())
    }
}

operation! {
    ReturnOp {
        name: "return",
        dialect: "func",
        operands: O {
            value: "?Any",
        },
        interfaces: [Terminator, RegionExit],
        state: "in",
    }
}

impl ReturnOp {
    /// The value the function returns, or `None` for a void return. The trailing
    /// `!state` operand names the memory handed back to the caller, not a value.
    pub fn returned_value(&self) -> Option<crate::ValueId> {
        let operands = self.operands();
        operands[..operands.len() - self.state_operand().is_some() as usize]
            .first()
            .copied()
    }
}

impl Terminator for ReturnOp {}

impl RegionExit for ReturnOp {
    /// Everything the return carries, memory state included: what the region
    /// hands back is the whole tuple, so an unordered body naming the same
    /// values in its `->` line binds exactly the same thing.
    fn exit_values(&self) -> Vec<tir::ValueId> {
        self.operands().to_vec()
    }
}
