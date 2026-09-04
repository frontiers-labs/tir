use crate::BlockHandle;
use crate::RegionHandle;
use std::any::Any;
use std::collections::{HashMap, HashSet};

use crate::attributes::{AttributeValue, NamedAttribute};
use crate::block::BlockId;
use crate::value::Value;
use crate::{Context, Error, Operation};

use super::common::{Cursor, Span};
use super::text::Parser as TextParser;

type ParseResult<T> = Result<T, (Span, Error)>;
type BlockLabel = (String, Vec<(String, crate::TypeId)>, Vec<NamedAttribute>);

pub fn parse_ir<T: Operation>(context: &Context, src: &str) -> Result<T, (Span, Error)> {
    let mut parser = TextParser::new(src);

    parse_attribute_aliases(&mut parser, context)?;
    let op = parse_single_op(&mut parser, context)?;
    bind_forward_references(&mut parser, context)?;
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
    parser.forbid_forward_references();
    parse_attribute_aliases(&mut parser, context)?;
    let op = parse_single_op(&mut parser, context)?;
    bind_forward_references(&mut parser, context)?;
    Ok(op)
}

/// Point every use of a placeholder at the value its name turned out to name.
fn bind_forward_references(parser: &mut TextParser<'_>, context: &Context) -> ParseResult<()> {
    let bindings = parser
        .forward_bindings()
        .map_err(|error| (parser.span(), error))?;
    if bindings.is_empty() {
        return Ok(());
    }
    for (old, new) in bindings {
        context.replace_value_uses(old, new);
    }
    Ok(())
}

/// Consume the file preamble of `#name = value` attribute aliases, binding each
/// for the rest of the parse. Aliases are file-scoped: they are defined before
/// the top-level operation and referenced from any attribute inside it.
fn parse_attribute_aliases<'src>(
    parser: &mut TextParser<'src>,
    context: &Context,
) -> ParseResult<()> {
    parser.skip_trivia();
    while parser.peek_char() == Some('#') {
        parser.parse_token("#");
        let name = parser
            .parse_ident()
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("alias name")))?
            .to_string();
        if !parser.parse_token("=") {
            return Err((parser.span(), Error::ExpectedToken("=")));
        }
        let value = parser
            .parse_attribute_value(context)?
            .ok_or_else(|| (parser.span(), Error::ExpectedToken("attribute value")))?;
        parser.define_alias(&name, value);
    }
    Ok(())
}

pub(crate) fn parse_single_op<'src>(
    parser: &mut TextParser<'src>,
    context: &Context,
) -> Result<Box<dyn Operation>, (Span, Error)> {
    parser.skip_trivia();

    // Optional SSA result assignment prefix (e.g. "%2 =" or "%2, %3 ="). The builder
    // allocates the concrete ValueIds; we bind the textual names to them once the op
    // exists so later operands resolve by name rather than by a literal id.
    let mark = parser.pos();
    let mut result_names = Vec::new();
    loop {
        match parser.parse_value_ref() {
            Some(name) => result_names.push(name.to_string()),
            None => {
                result_names.clear();
                break;
            }
        }
        if parser.parse_token(",") {
            continue;
        }
        if !parser.parse_token("=") {
            result_names.clear();
        }
        break;
    }
    if result_names.is_empty() {
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

        let op = op_parser(parser, context)?;
        let results = context.get_op(op.id()).results().to_vec();
        for (name, result) in result_names.iter().zip(results) {
            parser.define_value(name, result);
        }
        Ok(op)
    } else {
        Err((parser.span(), Error::ExpectedOpName))
    }
}

impl<'src> TextParser<'src> {
    pub fn parse_region(&mut self, context: &Context) -> Result<RegionHandle, (Span, Error)> {
        self.parse_region_with_entry_args(context, vec![])
    }

    pub fn parse_region_with_entry_args(
        &mut self,
        context: &Context,
        entry_args: Vec<Value>,
    ) -> Result<RegionHandle, (Span, Error)> {
        if !self.parse_token("{") {
            return Err((self.span(), Error::ExpectedToken("{")));
        }

        let region = context.create_region();
        let entry = context.create_block(entry_args);
        region.add_block(entry.id());

        let mut current = entry.clone();
        let enclosing = self.region_parse.take();
        self.region_parse = Some(super::text::RegionParseState {
            labels: HashMap::from([("bb0".to_string(), entry.id())]),
            defined: HashSet::from(["bb0".to_string()]),
        });

        let result = self.parse_region_body(context, &region, &mut current);
        let state = self.region_parse.take();
        self.region_parse = enclosing;
        result?;

        let state = state.expect("the region parse scope survives its region");
        if let Some(name) = state
            .labels
            .keys()
            .find(|name| !state.defined.contains(*name))
        {
            return Err((
                self.span(),
                Error::VerificationError(format!("block ^{name} is referenced but never defined")),
            ));
        }
        Ok(region)
    }

    fn parse_region_body(
        &mut self,
        context: &Context,
        region: &RegionHandle,
        current: &mut BlockHandle,
    ) -> ParseResult<()> {
        let entry = current.clone();
        let mut entry_open = true;
        loop {
            self.skip_trivia();
            if self.parse_token("}") {
                return Ok(());
            }

            if let Some((label, block_args, attrs)) = self.try_parse_block_label(context)? {
                let state = self
                    .region_parse
                    .as_mut()
                    .expect("block labels require an active region parse scope");
                if entry_open && !state.labels.contains_key(&label) {
                    state.labels.insert(label.clone(), entry.id());
                    state.defined.insert(label.clone());
                }
                entry_open = false;
                *current = self.block_at_label(context, region, &label, block_args)?;
                for attr in attrs {
                    current.set_attr(&context.resolve(attr.name), attr.value);
                }
                continue;
            }

            entry_open = false;
            let op = parse_single_op(self, context)?;
            current.append(op.id());
        }
    }

    pub(crate) fn resolve_region_block_label(
        &mut self,
        context: &Context,
        name: &str,
        block_arg_types: &[crate::TypeId],
    ) -> Result<BlockId, (Span, Error)> {
        let Some(state) = &mut self.region_parse else {
            // Detached parses (no region scope) address blocks by raw id.
            return name
                .strip_prefix("bb")
                .and_then(|n| n.parse::<u32>().ok())
                .map(BlockId::from_number)
                .ok_or_else(|| {
                    (
                        self.span(),
                        Error::VerificationError(format!(
                            "block ^{name} is not defined in this region"
                        )),
                    )
                });
        };

        if let Some(id) = state.labels.get(name) {
            let block = context.get_block(*id);
            if !block_arg_types.is_empty() && block.arguments().is_empty() {
                return Err((
                    self.span(),
                    Error::VerificationError(format!(
                        "block ^{name} was already referenced without arguments"
                    )),
                ));
            }
            return Ok(*id);
        }

        let block_args = block_arg_types
            .iter()
            .map(|ty| context.create_value(*ty, None))
            .collect();
        let block = context.create_block(block_args);
        state.labels.insert(name.to_string(), block.id());
        Ok(block.id())
    }

    fn try_parse_block_label(&mut self, context: &Context) -> ParseResult<Option<BlockLabel>> {
        let mark = self.pos();
        let Some(label) = self.parse_block_label().map(str::to_string) else {
            return Ok(None);
        };

        let block_args = if self.parse_token("(") {
            self.parse_block_argument_list(context)?
        } else {
            vec![]
        };

        let attrs = if self.parse_token("{") {
            self.parse_block_attribute_list(context)?
        } else {
            vec![]
        };

        if !self.parse_token(":") {
            self.set_pos(mark);
            return Ok(None);
        }

        Ok(Some((label, block_args, attrs)))
    }

    /// Parse the block-attribute entries of `{name = value, ...}` after the
    /// opening brace: string, float, integer or boolean values.
    fn parse_block_attribute_list(
        &mut self,
        context: &Context,
    ) -> ParseResult<Vec<NamedAttribute>> {
        let mut attrs = vec![];
        loop {
            if self.parse_token("}") {
                return Ok(attrs);
            }

            let name = self
                .parse_ident()
                .ok_or_else(|| (self.span(), Error::ExpectedToken("attribute name")))?
                .to_string();
            if !self.parse_token("=") {
                return Err((self.span(), Error::ExpectedToken("=")));
            }

            let value = if let Some(s) = self.parse_string() {
                AttributeValue::Str(s.to_string().into())
            } else if let Some(f) = self.parse_float() {
                AttributeValue::F64(f)
            } else if let Some(n) = self.parse_number() {
                AttributeValue::Int(n)
            } else {
                match self.parse_ident() {
                    Some("true") => AttributeValue::Bool(true),
                    Some("false") => AttributeValue::Bool(false),
                    _ => return Err((self.span(), Error::ExpectedToken("attribute value"))),
                }
            };
            attrs.push(NamedAttribute::new(context.intern(&name), value));

            if self.parse_token("}") {
                return Ok(attrs);
            }
            if !self.parse_token(",") {
                return Err((self.span(), Error::ExpectedToken(",")));
            }
        }
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

    fn block_at_label(
        &mut self,
        context: &Context,
        region: &RegionHandle,
        label: &str,
        named_args: Vec<(String, crate::TypeId)>,
    ) -> Result<BlockHandle, (Span, Error)> {
        let state = self
            .region_parse
            .as_ref()
            .expect("block labels require an active region parse scope");

        // A forward branch may have already created the block from the successor's
        // type list; bind the label's names to those existing arguments and let
        // the block join the region here, in definition order.
        if let Some(id) = state.labels.get(label).copied() {
            let block = context.get_block(id);
            if !named_args.is_empty() && block.arguments().is_empty() {
                return Err((
                    self.span(),
                    Error::VerificationError(format!(
                        "block ^{label} was already referenced without arguments"
                    )),
                ));
            }
            for ((name, _), arg) in named_args.iter().zip(block.arguments()) {
                self.define_value(name, arg.id());
            }
            let state = self.region_parse.as_mut().expect("scope checked above");
            if state.defined.insert(label.to_string()) {
                region.add_block(id);
            }
            return Ok(block);
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
        let state = self.region_parse.as_mut().expect("scope checked above");
        state.labels.insert(label.to_string(), block.id());
        state.defined.insert(label.to_string());
        Ok(block)
    }
}
