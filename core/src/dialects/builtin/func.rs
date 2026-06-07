use crate::Any;
use crate::builtin::UnitType;
use crate::operation;

use crate as tir;
use crate::Terminator;

operation! {
    FuncOp {
        name: "func",
        dialect: "builtin",
        format: "custom",
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
            tir::attributes::AttributeValue::Str(name.to_string()),
        )
    }

    pub fn ret_type(self, ty: tir::TypeId) -> Self {
        self.attr("ret_type", tir::attributes::AttributeValue::Type(ty))
    }
}

impl FuncOp {
    fn custom_print(&self, fmt: &mut tir::IRFormatter) -> Result<(), std::fmt::Error> {
        use tir::Operation;

        // func @name(%0: i32, %1: i32) -> i32 {
        fmt.write("func")?;

        // Print symbol name
        let sym_name = self
            .attributes()
            .iter()
            .find(|a| a.name == "sym_name")
            .map(|a| match &a.value {
                tir::attributes::AttributeValue::Str(s) => s.clone(),
                _ => panic!("sym_name must be a string"),
            })
            .unwrap_or_else(|| "unknown".to_string());

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
        let ret_type = self
            .attributes()
            .iter()
            .find(|a| a.name == "ret_type")
            .map(|a| match &a.value {
                tir::attributes::AttributeValue::Type(ty) => *ty,
                _ => panic!("ret_type must be a type"),
            })
            .unwrap_or_else(|| UnitType::new(&context));

        if ret_type != UnitType::new(&context) {
            fmt.write(" -> ")?;
            context.print_type(ret_type, fmt)?;
        }

        tir::region_format::print_op_region(fmt, &context, self, 0)?;

        Ok(())
    }

    fn custom_parse(
        parser: &mut tir::parse::text::Parser,
        context: &tir::Context,
    ) -> Result<Box<dyn tir::Operation>, (tir::parse::Span, tir::Error)> {
        use tir::parse::common::Cursor;

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
                let _val_name = parser
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

        // Parse body region { ... }
        let body_region = parser.parse_region_with_entry_args(context, block_args)?;

        let builder = FuncOpBuilder::new(context)
            .sym_name(&sym_name)
            .ret_type(ret_type)
            .body(body_region.id());

        Ok(Box::new(builder.build()))
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
    }
}

impl Terminator for ReturnOp {}

#[cfg(test)]
mod tests {
    use crate::{
        Context, IRBuilder, IRFormatter, Operation,
        builtin::{FuncOp, IntegerType, UnitType, ops},
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
        let mut builder = IRBuilder::new(func.body());
        builder.insert(ops::r#return(&context, param0_id).build());

        assert_eq!(func.regions().len(), 1);
        assert_eq!(func.body().arguments().len(), 2);
        assert_eq!(func.body().iter(context.clone()).len(), 1);
    }

    #[test]
    fn func_roundtrip() {
        let context = Context::with_default_dialects();

        // Build function
        let param0 = context.create_value(IntegerType::new(&context, 32), None);
        let param1 = context.create_value(IntegerType::new(&context, 32), None);
        let param0_id = param0.id();

        let region = context.create_region();
        let block = context.create_block(vec![param0, param1]);
        region.add_block(block.id());

        let func = ops::func(
            &context,
            "add",
            IntegerType::new(&context, 32),
            Some(region.id()),
        )
        .build();

        let mut builder = IRBuilder::new(func.body());
        builder.insert(ops::r#return(&context, param0_id).build());

        // Print
        let mut buf = String::new();
        let mut f = IRFormatter::new(&mut buf);
        func.print(&mut f).expect("print ok");
        assert!(!buf.is_empty());

        // Parse back
        let new_context = Context::with_default_dialects();
        let new_func = parse_ir::<FuncOp>(&new_context, &buf).expect("Failed to parse func");

        // Print again
        let mut new_buf = String::new();
        let mut f = IRFormatter::new(&mut new_buf);
        new_func.print(&mut f).expect("print ok");

        assert_eq!(buf, new_buf);
    }

    #[test]
    fn parse_text_labeled_blocks() {
        let context = Context::with_default_dialects();
        let src = r#"  func @jump() -> !i32 {
    br ^bb1
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

    #[test]
    fn multi_block_func_roundtrip() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);

        let region = context.create_region();
        let entry = context.create_block(vec![]);
        let target = context.create_block(vec![]);
        region.add_block(entry.id());
        region.add_block(target.id());

        let func = ops::func(&context, "jump", i32_ty, Some(region.id())).build();

        let mut entry_b = IRBuilder::new(entry.clone());
        entry_b.insert(ops::br(&context, vec![], target.id()).build());

        let mut target_b = IRBuilder::new(target.clone());
        let c = ops::constant(&context, 42, i32_ty).build();
        let c_result = c.result();
        target_b.insert(c);
        target_b.insert(ops::r#return(&context, c_result).build());

        let mut buf = String::new();
        let mut f = IRFormatter::new(&mut buf);
        func.print(&mut f).expect("print ok");
        assert!(buf.contains("^bb"));
        assert!(buf.contains("br ^bb"));

        let new_context = Context::with_default_dialects();
        let new_func = parse_ir::<FuncOp>(&new_context, &buf).expect("parse multi-block func");
        assert!(new_func.verify(&new_context).is_ok());

        let mut new_buf = String::new();
        let mut f = IRFormatter::new(&mut new_buf);
        new_func.print(&mut f).expect("print ok");
        assert_eq!(buf, new_buf);
    }

    #[test]
    fn multi_block_func_with_block_args_roundtrip() {
        let context = Context::with_default_dialects();
        let i32_ty = IntegerType::new(&context, 32);
        let param = context.create_value(i32_ty, None);
        let param_id = param.id();
        let arg = context.create_value(i32_ty, None);
        let arg_id = arg.id();

        let region = context.create_region();
        let entry = context.create_block(vec![param]);
        let target = context.create_block(vec![arg]);
        region.add_block(entry.id());
        region.add_block(target.id());

        let func = ops::func(&context, "fwd", i32_ty, Some(region.id())).build();

        let mut entry_b = IRBuilder::new(entry.clone());
        entry_b.insert(ops::br(&context, vec![param_id], target.id()).build());

        let mut target_b = IRBuilder::new(target.clone());
        target_b.insert(ops::r#return(&context, arg_id).build());

        let mut buf = String::new();
        let mut f = IRFormatter::new(&mut buf);
        func.print(&mut f).expect("print ok");
        assert!(buf.contains("br ^bb"));
        assert!(buf.contains("):"));

        let new_context = Context::with_default_dialects();
        let new_func = parse_ir::<FuncOp>(&new_context, &buf).expect("parse func with block args");
        assert!(new_func.verify(&new_context).is_ok());

        let mut new_buf = String::new();
        let mut f = IRFormatter::new(&mut new_buf);
        new_func.print(&mut f).expect("print ok");
        assert_eq!(buf, new_buf);
    }

    #[test]
    fn void_func() {
        let context = Context::with_default_dialects();

        // Build void function with no parameters and no return value
        let func = ops::func(&context, "nop", UnitType::new(&context), None).build();

        // Insert return with no operand
        let mut builder = IRBuilder::new(func.body());
        builder.insert(ops::r#return(&context, crate::operand::Operand::none()).build());

        // Print
        let mut buf = String::new();
        let mut f = IRFormatter::new(&mut buf);
        func.print(&mut f).expect("print ok");

        // Parse back
        let new_context = Context::with_default_dialects();
        let new_func = parse_ir::<FuncOp>(&new_context, &buf).expect("Failed to parse void func");

        let mut new_buf = String::new();
        let mut f = IRFormatter::new(&mut new_buf);
        new_func.print(&mut f).expect("print ok");

        assert_eq!(buf, new_buf);
    }
}
