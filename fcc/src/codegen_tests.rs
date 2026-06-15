#[cfg(test)]
mod tests {
    use crate::codegen::codegen;
    use crate::diagnostics::{Span, intern_file};
    use crate::parser::parse;
    use logos::Logos;

    use crate::lexer::Token;

    fn compile(src: &str) -> String {
        let file = intern_file("<test>", src);
        let tokens: Vec<_> = Token::lexer(src)
            .spanned()
            .map(|(r, span)| (r.unwrap(), Span::new(file, span.start)))
            .collect();
        let unit = parse(&tokens).expect("parse");
        let context = tir::Context::with_default_dialects();
        crate::cir::register(&context);
        let module = codegen(&context, &unit).expect("codegen");
        let mut out = String::new();
        let mut fmt = tir::IRFormatter::new(&mut out);
        tir::Operation::print(&module, &mut fmt).expect("print");
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

        let context = tir::Context::with_default_dialects();
        let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, &ir)
            .expect("emitted IR should parse back");

        let mut buf = String::new();
        let mut fmt = tir::IRFormatter::new(&mut buf);
        tir::Operation::print(&module, &mut fmt).expect("print");
        assert_eq!(ir, buf);
    }

    /// The loop ops carry their bodies as regions, so this checks the structured
    /// control flow survives the parser too: every loop form, an `if`, and a
    /// `break`/`continue` round-trip and re-print identically.
    fn loop_roundtrips(src: &str) {
        let ir = compile(src);

        let context = tir::Context::with_default_dialects();
        crate::cir::register(&context);
        let module = tir::parse::ir::parse_ir::<tir::builtin::ModuleOp>(&context, &ir)
            .expect("emitted loop IR should parse back");
        assert!(
            tir::Operation::verify(&module, &context).is_ok(),
            "reparsed loop IR should verify"
        );

        let mut buf = String::new();
        let mut fmt = tir::IRFormatter::new(&mut buf);
        tir::Operation::print(&module, &mut fmt).expect("print");
        assert_eq!(ir, buf);
    }

    #[test]
    fn while_loop_roundtrips() {
        loop_roundtrips("int f(int n) { int i = 0; while (i < n) { i = i + 1; } return i; }");
    }

    #[test]
    fn do_while_continue_roundtrips() {
        loop_roundtrips(
            "int f(int n) { int i = 0; do { i = i + 1; if (i == 3) continue; } while (i < n); return i; }",
        );
    }

    #[test]
    fn for_break_roundtrips() {
        loop_roundtrips(
            "int f(int n) { int t = 0; int i; for (i = 0; i < n; i = i + 1) { t = t + i; if (t > 100) break; } return t; }",
        );
    }
}
