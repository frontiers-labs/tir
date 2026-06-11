use std::collections::HashMap;

use tir::{
    IRBuilder,
    builtin::{ModuleEndOpBuilder, ModuleOp, ModuleOpBuilder},
    parse::tokens::Parser,
};

use crate::{
    LiteralOpBuilder, SectionOpBuilder, SymbolEndOpBuilder, SymbolOpBuilder, lex, lexer::Token,
};

pub type AsmInstructionParser =
    for<'src> fn(&tir::Context, &mut IRBuilder, &mut Parser<'src, Token<'src>>) -> Result<(), ()>;

pub struct AsmParser {
    /// Candidate parsers per mnemonic. A single mnemonic (e.g. AArch64 `add`)
    /// can name several instruction forms (register vs. immediate), so each key
    /// maps to a list tried in turn with backtracking.
    instruction_parsers: HashMap<String, Vec<AsmInstructionParser>>,
    /// Mnemonics the target defines but the selected ISA/extension set disables.
    /// These are parse errors, unlike genuinely unknown identifiers which are
    /// still skipped.
    disabled_mnemonics: std::collections::HashSet<String>,
}

impl AsmParser {
    pub fn new(instruction_parsers: HashMap<String, Vec<AsmInstructionParser>>) -> Self {
        AsmParser {
            instruction_parsers,
            disabled_mnemonics: Default::default(),
        }
    }

    pub fn with_disabled_mnemonics(
        mut self,
        disabled_mnemonics: std::collections::HashSet<String>,
    ) -> Self {
        self.disabled_mnemonics = disabled_mnemonics;
        self
    }

    #[allow(clippy::result_unit_err)]
    pub fn parse_asm(&self, context: &tir::Context, src: &str) -> Result<ModuleOp, ()> {
        let module = ModuleOpBuilder::new(context).build();

        let tokens = lex(src)?;

        let mut parser = Parser::new(&tokens);

        let mut builder = IRBuilder::new(module.body());
        let section_op = builder.insert(SectionOpBuilder::new(context).build());
        builder.insert(ModuleEndOpBuilder::new(context).build());
        let section_body = section_op.body();
        builder.set_insertion_point_to_start(section_body.clone());

        while let Some(token) = parser.peek() {
            match token {
                Token::Global => {
                    let _ = parser.bump();
                    let name = parser.bump();
                    match name {
                        Some(Token::Ident(name)) => {
                            builder.set_insertion_point_to_start(section_body.clone());
                            let global_op = builder.insert(
                                SymbolOpBuilder::new(context)
                                    .attr(
                                        "name",
                                        tir::attributes::AttributeValue::Str((*name).to_string()),
                                    )
                                    .build(),
                            );
                            builder.set_insertion_point_to_start(global_op.body());
                            builder.insert(SymbolEndOpBuilder::new(context).build());
                            builder.set_insertion_point_to_start(global_op.body());
                        }
                        _ => return Err(()),
                    }
                }
                Token::Label(_) => {
                    // FIXME just skip for now, use actual block names in future.
                    let _ = parser.bump();
                }
                Token::Text => {
                    // FIXME set insertion point to end of text section
                    let _ = parser.bump();
                }
                Token::Directive(directive) => {
                    let directive = *directive;
                    let _ = parser.bump();
                    let kind = &directive[1..];
                    match kind {
                        "byte" | "half" | "word" | "dword" | "space" => {
                            let value = match parser.bump() {
                                Some(Token::DecNumber(n)) => n.parse::<i64>().map_err(|_| ())?,
                                Some(Token::HexNumber(n)) => parse_hex(n)?,
                                _ => return Err(()),
                            };
                            builder.insert(
                                LiteralOpBuilder::new(context)
                                    .attr(
                                        "kind",
                                        tir::attributes::AttributeValue::Str(kind.to_string()),
                                    )
                                    .attr("value", tir::attributes::AttributeValue::Int(value))
                                    .build(),
                            );
                        }
                        "string" | "ascii" | "asciz" => {
                            let Some(Token::StringLit(value)) = parser.bump() else {
                                return Err(());
                            };
                            builder.insert(
                                LiteralOpBuilder::new(context)
                                    .attr(
                                        "kind",
                                        tir::attributes::AttributeValue::Str(kind.to_string()),
                                    )
                                    .attr(
                                        "value",
                                        tir::attributes::AttributeValue::Str((*value).to_string()),
                                    )
                                    .build(),
                            );
                        }
                        // Layout/section directives (`.rodata`, `.align`, ...)
                        // carry no data; skip them like unknown idents.
                        _ => {}
                    }
                }
                Token::Ident(ident) => {
                    // Try to dispatch to an instruction parser by mnemonic.
                    let key = ident.to_string();
                    if let Some(handlers) = self.instruction_parsers.get(&key) {
                        // consume mnemonic
                        let _ = parser.bump();
                        // A mnemonic may have several forms (e.g. register vs.
                        // immediate `add`); try each, rewinding the token cursor
                        // between failed attempts. The generated parsers only
                        // emit IR on success, so backtracking the cursor is
                        // enough to undo a failed candidate.
                        let start = parser.position();
                        let mut parsed = false;
                        for handler in handlers {
                            parser.reset(start);
                            if handler(context, &mut builder, &mut parser).is_ok() {
                                parsed = true;
                                break;
                            }
                        }
                        if !parsed {
                            return Err(());
                        }
                    } else if self.disabled_mnemonics.contains(&key) {
                        // The instruction exists but the selected ISA/extension
                        // set does not include it.
                        return Err(());
                    } else {
                        // Unknown ident in text section; skip it for now
                        let _ = parser.bump();
                    }
                }
                _ => {
                    let _ = parser.bump();
                }
            }
        }

        Ok(module)
    }
}

fn parse_hex(text: &str) -> Result<i64, ()> {
    let (neg, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let digits = text.trim_start_matches("0x").trim_start_matches("0X");
    let value = i128::from_str_radix(digits, 16).map_err(|_| ())?;
    let value = if neg { -value } else { value };
    i64::try_from(value).map_err(|_| ())
}
