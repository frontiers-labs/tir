use std::collections::HashMap;

use tir::{
    IRBuilder,
    builtin::{ModuleEndOpBuilder, ModuleOp, ModuleOpBuilder},
    parse::tokens::Parser,
};

use crate::{SectionOpBuilder, SymbolEndOpBuilder, SymbolOpBuilder, lex, lexer::Token};

pub type AsmInstructionParser =
    for<'src> fn(&tir::Context, &mut IRBuilder, &mut Parser<'src, Token<'src>>) -> Result<(), ()>;

pub struct AsmParser {
    /// Candidate parsers per mnemonic. A single mnemonic (e.g. AArch64 `add`)
    /// can name several instruction forms (register vs. immediate), so each key
    /// maps to a list tried in turn with backtracking.
    instruction_parsers: HashMap<String, Vec<AsmInstructionParser>>,
    preprocessor: Option<fn(&str) -> String>,
}

impl AsmParser {
    pub fn new(instruction_parsers: HashMap<String, Vec<AsmInstructionParser>>) -> Self {
        AsmParser {
            instruction_parsers,
            preprocessor: None,
        }
    }

    pub fn with_preprocessor(
        instruction_parsers: HashMap<String, Vec<AsmInstructionParser>>,
        preprocessor: fn(&str) -> String,
    ) -> Self {
        AsmParser {
            instruction_parsers,
            preprocessor: Some(preprocessor),
        }
    }

    #[allow(clippy::result_unit_err)]
    pub fn parse_asm(&self, context: &tir::Context, src: &str) -> Result<ModuleOp, ()> {
        let module = ModuleOpBuilder::new(context).build();

        let preprocessed;
        let src = if let Some(preprocessor) = self.preprocessor {
            preprocessed = preprocessor(src);
            preprocessed.as_str()
        } else {
            src
        };
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
