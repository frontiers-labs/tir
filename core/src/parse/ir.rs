use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use crate::block::BlockId;
use crate::value::{Value, ValueId};
use crate::{Block, Context, Error, Operation, Region};

use super::common::{Cursor, Span};
use super::text::Parser as TextParser;

type ParseResult<T> = Result<T, (Span, Error)>;
type BlockLabel = (u32, Vec<(String, crate::TypeId)>);

pub fn parse_ir<T: Operation>(context: &Context, src: &str) -> Result<T, (Span, Error)> {
    let mut parser = TextParser::new(src);

    let op = parse_single_op(&mut parser, context)?;
    let any: Box<dyn Any> = op.into_any();
    any.downcast::<T>()
        .map(|t| *t)
        .map_err(|_| (Span(0), Error::ExpectedOperation(T::dialect(), T::name())))
}

/// Parse a single operation from `src`, returning it detached from any block.
/// Operand value references resolve by numeric id (e.g. `%5` is `ValueId(5)`),
/// so callers can wire operands to existing values. The op is registered in the
/// context; its results receive fresh value ids.
pub fn parse_op(context: &Context, src: &str) -> Result<Box<dyn Operation>, (Span, Error)> {
    let mut parser = TextParser::new(src);
    parse_single_op(&mut parser, context)
}

pub(crate) fn parse_single_op<'src>(
    parser: &mut TextParser<'src>,
    context: &Context,
) -> Result<Box<dyn Operation>, (Span, Error)> {
    parser.skip_trivia();

    // Optional SSA result assignment prefix (e.g. "%2 ="). The builder allocates the
    // concrete ValueId; we bind the textual name to it once the op exists so later
    // operands resolve by name rather than by a literal id.
    let mark = parser.pos();
    let result_name = match parser.parse_value_ref() {
        Some(name) if parser.parse_token("=") => Some(name.to_string()),
        _ => {
            parser.set_pos(mark);
            None
        }
    };

    if let Some(name) = parser.parse_ident() {
        let (dialect, name) = if parser.parse_token(".") {
            if let Some(op_name) = parser.parse_ident() {
                (name, op_name)
            } else {
                return Err((parser.span(), Error::ExpectedOpName));
            }
        } else {
            ("builtin", name)
        };

        parser.skip_trivia();
        let op_parser = context
            .get_parser(dialect, name)
            .map_err(|e| (parser.span(), e))?;

        let op = op_parser(parser, context)?;
        if let Some(name) = result_name
            && let Some(result) = context.get_op(op.id()).results.first()
        {
            parser.define_value(&name, *result);
        }
        Ok(op)
    } else {
        Err((parser.span(), Error::ExpectedOpName))
    }
}

/// Maps value names (e.g. "0", "1", "arg") to ValueIds during parsing.
#[derive(Default, Clone)]
pub struct ValueScope {
    values: HashMap<String, ValueId>,
}

impl ValueScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: String, id: ValueId) {
        self.values.insert(name, id);
    }

    pub fn get(&self, name: &str) -> Option<ValueId> {
        self.values.get(name).copied()
    }
}

impl<'src> TextParser<'src> {
    pub fn parse_region(&mut self, context: &Context) -> Result<Arc<Region>, (Span, Error)> {
        self.parse_region_with_entry_args(context, vec![])
    }

    pub fn parse_region_with_entry_args(
        &mut self,
        context: &Context,
        entry_args: Vec<Value>,
    ) -> Result<Arc<Region>, (Span, Error)> {
        if !self.parse_token("{") {
            return Err((self.span(), Error::ExpectedToken("{")));
        }

        let region = context.create_region();
        let entry = context.create_block(entry_args);
        region.add_block(entry.id());

        let mut current = entry.clone();
        self.region_parse = Some(super::text::RegionParseState {
            region: region.clone(),
            indices: HashMap::from([(0, entry.id())]),
        });

        let result = self.parse_region_body(context, &region, &mut current);
        self.region_parse = None;
        result?;
        Ok(region)
    }

    fn parse_region_body(
        &mut self,
        context: &Context,
        region: &Arc<Region>,
        current: &mut Arc<Block>,
    ) -> ParseResult<()> {
        loop {
            self.skip_trivia();
            if self.parse_token("}") {
                return Ok(());
            }

            if let Some((block_index, block_args)) = self.try_parse_block_label(context)? {
                *current = self.block_at_region_index(context, region, block_index, block_args)?;
                continue;
            }

            let op = parse_single_op(self, context)?;
            current.insert(current.len(), op.id());
        }
    }

    pub(crate) fn resolve_region_block_index(
        &mut self,
        context: &Context,
        index: u32,
        block_arg_types: &[crate::TypeId],
    ) -> Result<BlockId, (Span, Error)> {
        let Some(state) = &mut self.region_parse else {
            return Ok(BlockId::from_number(index));
        };

        if let Some(id) = state.indices.get(&index) {
            let block = context.get_block(*id);
            if !block_arg_types.is_empty() && block.arguments().is_empty() {
                return Err((
                    self.span(),
                    Error::VerificationError(format!(
                        "block ^bb{index} was already referenced without arguments"
                    )),
                ));
            }
            return Ok(*id);
        }

        let len = state.region.iter(context.clone()).len();
        if index as usize != len {
            return Err((
                self.span(),
                Error::VerificationError(format!("block ^bb{index} is not defined in this region")),
            ));
        }

        let block_args = block_arg_types
            .iter()
            .map(|ty| context.create_value(*ty, None))
            .collect();
        let block = context.create_block(block_args);
        state.region.add_block(block.id());
        state.indices.insert(index, block.id());
        Ok(block.id())
    }

    fn try_parse_block_label(&mut self, context: &Context) -> ParseResult<Option<BlockLabel>> {
        let mark = self.pos();
        let Some(block_index) = self.parse_block_index() else {
            return Ok(None);
        };

        let block_args = if self.parse_token("(") {
            self.parse_block_argument_list(context)?
        } else {
            vec![]
        };

        if !self.parse_token(":") {
            self.set_pos(mark);
            return Ok(None);
        }

        Ok(Some((block_index, block_args)))
    }

    fn parse_block_argument_list(
        &mut self,
        context: &Context,
    ) -> Result<Vec<(String, crate::TypeId)>, (Span, Error)> {
        let mut args = vec![];

        loop {
            if self.parse_token(")") {
                return Ok(args);
            }

            let name = self
                .parse_value_ref()
                .ok_or_else(|| (self.span(), Error::ExpectedValueRef))?
                .to_string();

            if !self.parse_token(":") {
                return Err((self.span(), Error::ExpectedToken(":")));
            }

            let ty = self
                .parse_type(context)?
                .ok_or_else(|| (self.span(), Error::ExpectedType))?;
            args.push((name, ty));

            if self.parse_token(")") {
                return Ok(args);
            }
            if !self.parse_token(",") {
                return Err((self.span(), Error::ExpectedToken(",")));
            }
        }
    }

    fn block_at_region_index(
        &mut self,
        context: &Context,
        region: &Arc<Region>,
        index: u32,
        named_args: Vec<(String, crate::TypeId)>,
    ) -> Result<Arc<Block>, (Span, Error)> {
        let existing = self
            .region_parse
            .as_ref()
            .and_then(|s| s.indices.get(&index).copied());

        // A forward branch may have already created the block from the successor's
        // type list; bind the label's names to those existing arguments.
        if let Some(id) = existing {
            let block = context.get_block(id);
            if !named_args.is_empty() && block.arguments().is_empty() {
                return Err((
                    self.span(),
                    Error::VerificationError(format!(
                        "block ^bb{index} was already referenced without arguments"
                    )),
                ));
            }
            for ((name, _), arg) in named_args.iter().zip(block.arguments()) {
                self.define_value(name, arg.id());
            }
            return Ok(block);
        }

        let len = region.iter(context.clone()).len();
        if index as usize != len {
            return Err((
                self.span(),
                Error::VerificationError(format!("block ^bb{index} is not defined in this region")),
            ));
        }

        let block_args: Vec<Value> = named_args
            .iter()
            .map(|(_, ty)| context.create_value(*ty, None))
            .collect();
        for ((name, _), arg) in named_args.iter().zip(&block_args) {
            self.define_value(name, arg.id());
        }
        let block = context.create_block(block_args);
        region.add_block(block.id());
        self.region_parse
            .as_mut()
            .expect("block labels require an active region parse scope")
            .indices
            .insert(index, block.id());
        Ok(block)
    }
}
