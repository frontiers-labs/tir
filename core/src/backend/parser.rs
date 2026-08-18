use std::collections::HashMap;
use tir::BlockHandle;
use tir::RegionHandle;

use tir::attributes::AttributeValue;
use tir::{
    BlockId, Operation,
    builtin::{ModuleEndOpBuilder, ModuleOp, ModuleOpBuilder},
    parse::tokens::Parser,
};

use crate::backend::{
    LiteralOpBuilder, MachineInstruction, SectionOpBuilder, SymbolEndOpBuilder, SymbolOpBuilder,
    lex, lexer::Token,
};

pub type AsmInstructionParser =
    for<'src> fn(&tir::Context, &mut AsmCursor, &mut Parser<'src, Token<'src>>) -> Result<(), ()>;

/// Where the assembly parser writes: a block and the position the next operation
/// takes in it. A symbol body already holds its terminator when parsing starts,
/// so its instructions land ahead of everything the block holds rather than at
/// its end.
pub struct AsmCursor {
    block: BlockHandle,
    position: usize,
}

impl AsmCursor {
    pub fn at_start(block: BlockHandle) -> Self {
        Self { block, position: 0 }
    }

    pub fn insert<T: Operation>(&mut self, op: T) -> T {
        self.block.insert(self.position, op.id());
        self.position += 1;
        op
    }
}

pub struct AsmParser {
    /// Candidate parsers per mnemonic. A single mnemonic (e.g. AArch64 `add`)
    /// can name several instruction forms (register vs. immediate), so each key
    /// maps to a list tried in turn with backtracking.
    instruction_parsers: HashMap<String, Vec<AsmInstructionParser>>,
    /// Mnemonics the target defines but the selected ISA/extension set disables.
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

        let module_body = module.body();
        let section_op = module_body.append_op(SectionOpBuilder::new(context).build());
        module_body.append_op(ModuleEndOpBuilder::new(context).build());
        let section_body = section_op.body();
        let mut builder = AsmCursor::at_start(section_body.clone());

        // Local labels seen so far, mapped to the block they name. After parsing
        // we rewrite branch immediates that reference one of these.
        let mut labels: HashMap<String, BlockId> = HashMap::new();
        let mut cur_region: Option<RegionHandle> = None;
        let mut cur_entry: Option<BlockHandle> = None;
        // Whether the insertion point is still the symbol's entry block, and
        // whether that block already received content or an entry label.
        let mut at_entry = false;
        let mut block_has_content = false;
        let mut entry_named = false;

        while let Some(token) = parser.peek() {
            match token {
                Token::Global => {
                    let _ = parser.bump();
                    let name = parser.bump();
                    match name {
                        Some(Token::Ident(name)) => {
                            builder = AsmCursor::at_start(section_body.clone());
                            let global_op = builder.insert(
                                SymbolOpBuilder::new(context)
                                    .attr(
                                        "name",
                                        tir::attributes::AttributeValue::Str(
                                            (*name).to_string().into(),
                                        ),
                                    )
                                    .build(),
                            );
                            builder = AsmCursor::at_start(global_op.body());
                            builder.insert(SymbolEndOpBuilder::new(context).build());
                            builder = AsmCursor::at_start(global_op.body());

                            cur_region = global_op.regions().next();
                            cur_entry = Some(global_op.body());
                            at_entry = true;
                            block_has_content = false;
                            entry_named = false;
                        }
                        _ => return Err(()),
                    }
                }
                Token::Label(name) => {
                    let name = (*name).to_string();
                    let _ = parser.bump();
                    if let (Some(region), Some(entry)) = (&cur_region, &cur_entry) {
                        if at_entry && !block_has_content && !entry_named {
                            // First label of a symbol with an empty entry block
                            // (typical `func:` after `.global func`): map it to the
                            // existing entry block rather than starting a new one.
                            // The entry needs no `name` attribute: the symbol label
                            // already prints it, and naming it would add a header to
                            // the otherwise headerless single-block IR dump.
                            labels.insert(name, entry.id());
                            entry_named = true;
                        } else {
                            let block = context.create_block(vec![]);
                            region.add_block(block.id());
                            block.set_attr("name", AttributeValue::Str(name.clone().into()));
                            labels.insert(name, block.id());
                            builder = AsmCursor::at_start(block.clone());
                            at_entry = false;
                            block_has_content = false;
                        }
                    }
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
                                        tir::attributes::AttributeValue::Str(
                                            kind.to_string().into(),
                                        ),
                                    )
                                    .attr("value", tir::attributes::AttributeValue::Int(value))
                                    .build(),
                            );
                            block_has_content = true;
                        }
                        "string" | "ascii" | "asciz" => {
                            let Some(Token::StringLit(value)) = parser.bump() else {
                                return Err(());
                            };
                            builder.insert(
                                LiteralOpBuilder::new(context)
                                    .attr(
                                        "kind",
                                        tir::attributes::AttributeValue::Str(
                                            kind.to_string().into(),
                                        ),
                                    )
                                    .attr(
                                        "value",
                                        tir::attributes::AttributeValue::Str(
                                            (*value).to_string().into(),
                                        ),
                                    )
                                    .build(),
                            );
                            block_has_content = true;
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
                        block_has_content = true;
                    } else if self.disabled_mnemonics.contains(&key) {
                        // The instruction exists but the selected ISA/extension
                        // set does not include it.
                        return Err(());
                    } else {
                        return Err(());
                    }
                }
                _ => {
                    let _ = parser.bump();
                }
            }
        }

        if !labels.is_empty() {
            resolve_labels(context, &module.body(), &labels);
        }

        Ok(module)
    }
}

/// Rewrite branch immediates that name a local label into a block reference, so
/// the encoder emits a pc-relative fixup instead of a symbol relocation.
/// Unknown identifiers stay as `Str` and become external symbol references.
fn resolve_labels(context: &tir::Context, block: &BlockHandle, labels: &HashMap<String, BlockId>) {
    for op_id in block.op_ids() {
        let op = context.get_op(op_id);
        if op
            .clone()
            .as_interface::<dyn MachineInstruction>()
            .is_some()
        {
            let mut attrs = op.attributes().to_vec();
            let imm = context.sym("imm");
            let mut changed = false;
            for attr in &mut attrs {
                if Some(attr.name) != imm {
                    continue;
                }
                if let AttributeValue::Str(symbol) = &attr.value
                    && let Some(target) = labels.get(&**symbol)
                {
                    attr.value = AttributeValue::Block(*target);
                    changed = true;
                }
            }
            if changed {
                context.set_op_attributes(op_id, attrs);
            }
        }
        for region_id in op.regions() {
            let region = context.get_region(region_id);
            for child in region.iter(context.clone()) {
                resolve_labels(context, &child, labels);
            }
        }
    }
}

pub(crate) fn parse_hex(text: &str) -> Result<i64, ()> {
    let (neg, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let digits = text.trim_start_matches("0x").trim_start_matches("0X");
    let value = i128::from_str_radix(digits, 16).map_err(|_| ())?;
    let value = if neg { -value } else { value };
    i64::try_from(value).map_err(|_| ())
}
