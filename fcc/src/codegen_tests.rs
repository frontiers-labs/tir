#[cfg(test)]
mod tests {
    use crate::codegen::codegen;
    use crate::diagnostics::{Span, intern_file};
    use crate::lexer::Token;
    use crate::parser::parse;
    use crate::sema::analyze;
    use logos::Logos;

    fn fcc_context() -> tir::Context {
        let context = tir::Context::with_default_dialects();
        context.register_dialect::<crate::cir::CirDialect>();
        context
    }

    fn lower(src: &str) -> (tir::Context, tir::builtin::ModuleOp) {
        let file = intern_file("<test>", src);
        let tokens: Vec<_> = Token::lexer(src)
            .spanned()
            .map(|(r, span)| (r.unwrap(), Span::new(file, span.start)))
            .collect();
        let options = Default::default();
        let unit = parse(&tokens, options).expect("parse");
        let unit = analyze(unit, options).expect("sema");
        let context = fcc_context();
        let module = codegen(&context, &unit).expect("codegen");
        (context, module)
    }

    fn compile(src: &str) -> String {
        let (_context, module) = lower(src);
        let mut out = String::new();
        let mut fmt = tir::IRFormatter::new(&mut out);
        tir::Operation::print(&module, &mut fmt).expect("print");
        out
    }

    fn compile_structs_lowered(src: &str) -> String {
        let (context, module) = lower(src);
        let mut passes = tir::PassManager::new();
        passes.add_pass(crate::passes::LowerCirStructsPass::new());
        passes
            .run(&context, context.get_op(tir::Operation::id(&module)))
            .expect("lower CIR structs");
        let mut out = String::new();
        let mut formatter = tir::IRFormatter::new(&mut out);
        tir::Operation::print(&module, &mut formatter).unwrap();
        out
    }

    /// Codegen behaviour is checked by the LIT tests under `fcc/checks/Codegen`.
    /// This Rust test covers the round-trip invariant, which is a property of
    /// the emitted IR rather than a textual match and so does not fit a
    /// FileCheck test.
    #[test]
    fn ir_roundtrips_through_parser() {
        // The emitted IR must parse back as a module and print identically.
        let ir = compile("int sum(int a, int b) { return a + b; }");

        let context = fcc_context();
        let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, &ir)
            .expect("emitted IR should parse back");

        let mut buf = String::new();
        let mut fmt = tir::IRFormatter::new(&mut buf);
        tir::Operation::print(&module, &mut fmt).expect("print");
        assert_eq!(ir, buf);
    }

    #[test]
    fn cir_variadic_ir_roundtrips_through_parser() {
        let ir = compile(
            r#"int printf(const char *restrict format, ...);
int main(void) { printf("hello"); return 0; }"#,
        );

        let context = fcc_context();
        let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, &ir)
            .expect("emitted CIR should parse back");

        let mut buf = String::new();
        let mut fmt = tir::IRFormatter::new(&mut buf);
        tir::Operation::print(&module, &mut fmt).expect("print");
        assert_eq!(ir, buf);
    }

    #[test]
    fn emits_struct_definition_and_type() {
        let ir = compile(
            "struct Pair { char tag; int value; }; int main(void) { struct Pair pair; return 0; }",
        );

        assert!(ir.contains("cir.define_struct"), "{ir}");
        assert!(ir.contains("!cir.struct<\"Pair\">"), "{ir}");
    }

    #[test]
    fn struct_ir_roundtrips_through_parser() {
        let ir = compile(
            "struct Pair { char tag; int value; }; int main(void) { struct Pair source; struct Pair destination; source.value = 1; destination = source; return destination.value; }",
        );
        let context = fcc_context();
        let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, &ir)
            .expect("emitted struct CIR should parse");
        let mut parsed = String::new();
        let mut formatter = tir::IRFormatter::new(&mut parsed);
        tir::Operation::print(&module, &mut formatter).unwrap();

        assert_eq!(ir, parsed);
    }

    #[test]
    fn emits_member_address_and_scalar_load() {
        let ir = compile(
            "struct Pair { char tag; int value; }; int read(void) { struct Pair pair; return pair.value; }",
        );

        assert!(ir.contains("cir.get_member"), "{ir}");
        assert!(ir.contains("ptr.load"), "{ir}");
    }

    #[test]
    fn emits_scalar_member_store() {
        let ir = compile(
            "struct Pair { int value; }; int write(void) { struct Pair pair; pair.value = 7; return pair.value; }",
        );

        assert!(ir.matches("cir.get_member").count() >= 2, "{ir}");
        assert!(ir.contains("ptr.store"), "{ir}");
    }

    #[test]
    fn emits_whole_struct_copy() {
        let ir = compile(
            "struct Pair { int value; }; int copy(void) { struct Pair source; struct Pair destination; destination = source; return 0; }",
        );

        assert!(ir.contains("cir.copy_struct"), "{ir}");
    }

    #[test]
    fn lowers_member_access_to_pointer_addition() {
        let ir = compile_structs_lowered(
            "struct Pair { char tag; int value; }; int read(void) { struct Pair pair; return pair.value; }",
        );

        assert!(ir.contains("ptr.ptradd"), "{ir}");
        assert!(!ir.contains("cir.get_member"), "{ir}");
        assert!(!ir.contains("cir.define_struct"), "{ir}");
    }

    #[test]
    fn lowers_struct_copy_to_scalar_memory_operations() {
        let ir = compile_structs_lowered(
            "struct Pair { char tag; int value; }; int copy(void) { struct Pair source; struct Pair destination; destination = source; return 0; }",
        );

        assert!(!ir.contains("cir.copy_struct"), "{ir}");
        assert!(ir.matches("ptr.load").count() >= 2, "{ir}");
        assert!(ir.matches("ptr.store").count() >= 2, "{ir}");
    }

    #[test]
    fn cir_loop_ir_roundtrips_through_parser() {
        let ir = compile("int f(void) { int i = 0; while (i < 3) { i = i + 1; } return i; }");

        let context = fcc_context();
        let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, &ir)
            .expect("emitted CIR loop should parse back");

        let mut buf = String::new();
        let mut fmt = tir::IRFormatter::new(&mut buf);
        tir::Operation::print(&module, &mut fmt).expect("print");
        assert_eq!(ir, buf);
    }

    fn lower_cir_control_flow(src: &str) -> String {
        let (context, module) = lower(src);
        let mut passes = tir::PassManager::new();
        let function_pipeline = passes.nest::<tir::builtin::FuncOp>();
        function_pipeline.add_pass(crate::passes::LowerCirControlFlowPass::new());
        function_pipeline.add_pass(tir::passes::Mem2RegPass::new());
        passes
            .run(&context, context.get_op(tir::Operation::id(&module)))
            .expect("lower CIR control flow");

        let mut lowered = String::new();
        let mut fmt = tir::IRFormatter::new(&mut lowered);
        tir::Operation::print(&module, &mut fmt).expect("print");
        lowered
    }

    #[test]
    fn invariant_cir_while_lowers_to_canonical_scf() {
        let lowered = lower_cir_control_flow("int f(void) { while (1) {} return 0; }");

        assert!(lowered.contains("scf.while"), "{lowered}");
        assert!(!lowered.contains("cir.while"));
        assert!(!lowered.contains("scf.while") || !lowered.contains(" cond {"));
    }

    #[test]
    fn single_block_changing_cir_while_lowers_to_scf() {
        let lowered = lower_cir_control_flow(
            "int f(void) { int i = 0; while (i < 3) { i = i + 1; } return i; }",
        );

        assert!(!lowered.contains("cir.while"));
        assert!(lowered.contains("scf.while"));
        assert!(lowered.contains("scf.condition"));
        assert!(!lowered.contains("cond_br"));
    }

    #[test]
    fn structured_break_preserves_token_scope_in_scf() {
        let lowered =
            lower_cir_control_flow("int f(int stop) { while (1) { if (stop) break; } return 0; }");

        assert!(!lowered.contains("cir.while"));
        assert!(lowered.contains("scf.while"), "{lowered}");
        assert!(lowered.contains("scf.break"), "{lowered}");
        assert!(!lowered.contains("cond_br"), "{lowered}");
    }

    #[test]
    fn multiblock_cir_while_lowers_directly_to_cfg() {
        let context = fcc_context();
        let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(
            &context,
            r#"module {
  func @f(%0: !i1) {
    cir.while %1 cond {
      cir.condition %0
    } body {
      br ^bb1
      ^bb1:
      cir.yield
    }
    return
  }
  module_end
}
"#,
        )
        .expect("parse multiblock CIR");
        let mut passes = tir::PassManager::new();
        passes
            .nest::<tir::builtin::FuncOp>()
            .add_pass(crate::passes::LowerCirControlFlowPass::new());
        passes
            .run(&context, context.get_op(tir::Operation::id(&module)))
            .expect("lower multiblock CIR");

        let mut lowered = String::new();
        let mut fmt = tir::IRFormatter::new(&mut lowered);
        tir::Operation::print(&module, &mut fmt).expect("print");
        assert!(!lowered.contains("cir.while"));
        assert!(!lowered.contains("scf.while"));
        assert!(lowered.contains("cond_br"));
        let roundtrip_context = fcc_context();
        tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&roundtrip_context, &lowered)
            .expect("lowered multiblock CFG should parse");
    }
}
