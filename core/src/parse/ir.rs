use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use crate::block::BlockId;
use crate::value::{Value, ValueId};
use crate::{Block, Context, Error, Operation, Region};

use super::common::{Cursor, Span};
use super::text::Parser as TextParser;

type ParseResult<T> = Result<T, (Span, Error)>;
type BlockLabel = (u32, Vec<Value>);

pub fn parse_ir<T: Operation>(context: &Context, src: &str) -> Result<T, (Span, Error)> {
    let mut parser = TextParser::new(src);

    let op = parse_single_op(&mut parser, context)?;
    let any: Box<dyn Any> = op.into_any();
    any.downcast::<T>()
        .map(|t| *t)
        .map_err(|_| (Span(0), Error::ExpectedOperation(T::dialect(), T::name())))
}

pub(crate) fn parse_single_op<'src>(
    parser: &mut TextParser<'src>,
    context: &Context,
) -> Result<Box<dyn Operation>, (Span, Error)> {
    parser.skip_trivia();

    // Optional SSA result assignment prefix (e.g. "%2 =").
    // The concrete ValueId is currently allocated by builders from context state.
    let mark = parser.pos();
    if parser.parse_value_ref().is_some() && !parser.parse_token("=") {
        parser.set_pos(mark);
    }

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

        op_parser(parser, context)
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
    pub fn parse_single_block_region(
        &mut self,
        context: &Context,
    ) -> Result<Arc<Region>, (Span, Error)> {
        self.parse_single_block_region_with_args(context, vec![])
    }

    pub fn parse_single_block_region_with_args(
        &mut self,
        context: &Context,
        block_args: Vec<Value>,
    ) -> Result<Arc<Region>, (Span, Error)> {
        self.parse_block_region_with_entry_args(context, block_args)
    }

    pub fn parse_block_region_with_entry_args(
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
        let mut labeled = HashMap::new();

        loop {
            self.skip_trivia();
            if self.parse_token("}") {
                break;
            }

            if let Some((block_num, block_args)) = self.try_parse_block_label(context)? {
                current = self.get_or_create_labeled_block(
                    context,
                    &region,
                    block_num,
                    block_args,
                    &mut labeled,
                )?;
                continue;
            }

            let op = parse_single_op(self, context)?;
            current.insert(current.len(), op.id());
        }

        Ok(region)
    }

    fn try_parse_block_label(
        &mut self,
        context: &Context,
    ) -> ParseResult<Option<BlockLabel>> {
        let mark = self.pos();
        let Some(block) = self.parse_block_ref() else {
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

        Ok(Some((block.number(), block_args)))
    }

    fn parse_block_argument_list(
        &mut self,
        context: &Context,
    ) -> Result<Vec<Value>, (Span, Error)> {
        let mut args = vec![];

        loop {
            if self.parse_token(")") {
                return Ok(args);
            }

            let _val_name = self
                .parse_value_ref()
                .ok_or_else(|| (self.span(), Error::ExpectedValueRef))?;

            if !self.parse_token(":") {
                return Err((self.span(), Error::ExpectedToken(":")));
            }

            let ty = self
                .parse_type(context)?
                .ok_or_else(|| (self.span(), Error::ExpectedType))?;
            args.push(context.create_value(ty, None));

            if self.parse_token(")") {
                return Ok(args);
            }
            if !self.parse_token(",") {
                return Err((self.span(), Error::ExpectedToken(",")));
            }
        }
    }

    fn get_or_create_labeled_block(
        &mut self,
        context: &Context,
        region: &Arc<Region>,
        block_num: u32,
        block_args: Vec<Value>,
        labeled: &mut HashMap<u32, BlockId>,
    ) -> Result<Arc<Block>, (Span, Error)> {
        if let Some(id) = labeled.get(&block_num) {
            let block = context.get_block(*id);
            if !block_args.is_empty() && block.arguments().is_empty() {
                return Err((
                    self.span(),
                    Error::VerificationError(format!(
                        "block ^bb{block_num} was already referenced without arguments"
                    )),
                ));
            }
            return Ok(block);
        }

        let id = BlockId::from_number(block_num);
        let block = if context.has_block(id) {
            let existing = context.get_block(id);
            if !block_args.is_empty() && existing.arguments().is_empty() {
                return Err((
                    self.span(),
                    Error::VerificationError(format!(
                        "block ^bb{block_num} was already created without arguments"
                    )),
                ));
            }
            existing
        } else {
            context.create_block_at(id, block_args)
        };
        if !region.iter(context.clone()).any(|b| b.id() == block.id()) {
            region.add_block(block.id());
        }
        labeled.insert(block_num, block.id());
        Ok(block)
    }
}
