use crate::Terminator;
use crate::operation;
use crate::symbol_table::SymbolTable;
use crate::{Context, Error, Operation};

use crate as tir;

operation! {
    ModuleOp {
        name: "module",
        dialect: "builtin",
        verifier: "true",
        regions: R {
            body: Region {
                single_block: true,
            }
        }
    }
}

impl tir::Verifiable for ModuleOp {
    fn verify_impl(&self, context: &Context) -> Result<(), Error> {
        SymbolTable::build(context, self.id()).verify(context)
    }
}

operation! {
    ModuleEndOp {
        name: "module_end",
        dialect: "builtin",
        interfaces: [Terminator],
    }
}

impl Terminator for ModuleEndOp {}

#[cfg(test)]
mod tests {
    use crate::{
        Context, IRFormatter, Operation,
        builtin::{ModuleOp, ops},
        parse::ir::parse_ir,
    };

    #[test]
    fn module_parses_labeled_func_blocks() {
        use crate::builtin::FuncOp;

        let context = Context::with_default_dialects();
        let src = r#"module {
  func @jump() -> !i32 {
    cfg.br ^bb1
  ^bb1:
    %0 = constant {value = 42} : !i32
    return %0
  }
  module_end
}"#;
        let module = parse_ir::<ModuleOp>(&context, src).expect("parse module");
        let func = module
            .body()
            .iter(context.clone())
            .next()
            .unwrap()
            .as_op::<FuncOp>()
            .expect("func op");
        let region = func.regions().next().unwrap();
        assert_eq!(region.iter(context.clone()).len(), 2);
        assert!(module.verify(&context).is_ok());
    }

    #[test]
    fn module_creation() {
        let context = Context::with_default_dialects();
        let m = ops::module(&context, None).build();

        m.body().append_op(ops::module_end(&context).build());

        assert_eq!(m.regions().len(), 1);
        assert_eq!(m.body().iter(context.clone()).len(), 1);

        let mut buf = String::new();
        let mut f = IRFormatter::new(&mut buf);
        m.print(&mut f).expect("ok");
        assert!(!buf.is_empty());

        let new_op = parse_ir::<ModuleOp>(&context, &buf).expect("Failed to parse constructed op");
        let mut new_buf = String::new();
        let mut f = IRFormatter::new(&mut new_buf);
        new_op.print(&mut f).expect("ok");
        assert_eq!(buf, new_buf);
    }
}
