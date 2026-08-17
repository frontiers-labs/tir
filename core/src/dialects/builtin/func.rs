use crate::Any;
use crate::builtin::UnitType;
use crate::operation;
use crate::symbol_table::{symbol_name_of, visibility_of};

use crate as tir;
use crate::{Context, Error, Operation, Symbol, Terminator, Visibility};

operation! {
    FuncOp {
        name: "func",
        dialect: "builtin",
        format: "custom",
        verifier: "true",
        interfaces: [Symbol],
        attributes: A {
            sym_name: "Str",
            ret_type: "Type",
        },
        regions: R {
            body: Region {
                single_block: true,
            }
        }
    }
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
}

impl Symbol for FuncOp {
    fn symbol_name(&self) -> String {
        symbol_name_of(self)
    }

    fn symbol_signature(&self) -> Option<Vec<tir::TypeId>> {
        Some(self.body().arguments().iter().map(tir::Value::ty).collect())
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

        // func @name(%0: i32, %1: i32) -> i32 {
        fmt.write("func")?;
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
        let block = self.body();
        let args = block.arguments();

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

        // Parse body region { ... }
        let body_region = parser.parse_region_with_entry_args(context, block_args)?;

        let mut builder = FuncOpBuilder::new(context)
            .sym_name(&sym_name)
            .ret_type(ret_type)
            .body(body_region.id());
        if result_address {
            builder = builder.result_address();
        }
        if let Some(argument_alignments) = argument_alignments {
            builder = builder.attr("argument_alignments", argument_alignments);
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

impl tir::Verifiable for FuncOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        super::verify_argument_alignments(self, self.body().arguments().len(), "function")?;
        if !self.has_result_address() {
            return Ok(());
        }
        let Some(argument) = self.body().arguments().first().cloned() else {
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
        dialect: "builtin",
        operands: O {
            value: "?Any",
        },
        interfaces: [Terminator],
        state: "in",
    }
}

impl Terminator for ReturnOp {}

#[cfg(test)]
mod tests {
    use crate::{
        Context, IRFormatter, Operation,
        builtin::{FuncOp, IntegerType, ops},
        parse::ir::parse_ir,
    };

    #[test]
    fn func_construction() {
        let context = Context::with_default_dialects();

        // Create function parameters
        let param0 = context.create_value(IntegerType::new(&context, 32), None);
        let param1 = context.create_value(IntegerType::new(&context, 32), None);
        let param0_id = param0.id();

        // Create the body region with block arguments
        let region = context.create_region();
        let block = context.create_block(vec![param0, param1]);
        region.add_block(block.id());

        // Build function op
        let func = ops::func(
            &context,
            "add",
            IntegerType::new(&context, 32),
            Some(region.id()),
        )
        .build();

        // Insert return op into body
        func.body()
            .append_op(ops::r#return(&context, param0_id).build());

        assert_eq!(func.regions().len(), 1);
        assert_eq!(func.body().arguments().len(), 2);
        assert_eq!(func.body().iter(context.clone()).len(), 1);
    }

    // Func roundtrip coverage (single-block, multi-block, block args, void)
    // lives in the FileCheck suite under core/checks/IRRoundtrip.

    #[test]
    fn parse_non_contiguous_and_named_values() {
        let context = Context::with_default_dialects();
        // Names are neither contiguous nor numeric; they must still resolve to the
        // actual allocated value ids rather than being read as literal ids.
        let src = r#"func @nc(%100: !i32) -> !i32 {
    %sum = addi %100, %100 : !i32
    return %sum
  }"#;
        let func = parse_ir::<FuncOp>(&context, src).expect("parse");
        assert!(func.verify(&context).is_ok());

        let arg_id = func.body().arguments()[0].id();
        let ops: Vec<_> = func.body().iter(context.clone()).collect();
        assert_eq!(ops[0].operands().as_slice(), vec![arg_id, arg_id]);
        let sum_id = ops[0].results()[0];
        assert_eq!(ops[1].operands().as_slice(), vec![sum_id]);
    }

    #[test]
    fn parse_text_labeled_blocks() {
        let context = Context::with_default_dialects();
        let src = r#"  func @jump() -> !i32 {
    cfg.br ^bb1
  ^bb1:
    %0 = constant {value = 42} : !i32
    return %0
  }"#;
        let func = parse_ir::<FuncOp>(&context, src).expect("parse labeled blocks");
        let region = func.regions().next().unwrap();
        assert_eq!(region.iter(context.clone()).len(), 2);
        assert!(func.verify(&context).is_ok());

        let mut buf = String::new();
        let mut f = IRFormatter::new(&mut buf);
        func.print(&mut f).expect("print ok");
        assert!(buf.contains("^bb1:") || buf.contains("^bb"), "{buf}");
    }
}
