use crate::BlockHandle;
use crate::RegionHandle;
use std::any::Any;
use std::collections::{HashMap, HashSet};

use crate::attributes::{AttributeValue, NamedAttribute};
use crate::block::BlockId;
use crate::value::{Value, ValueId};
use crate::{Context, Error, OpId, Operation};

use super::common::{Cursor, Span};
use super::text::Parser as TextParser;

type ParseResult<T> = Result<T, (Span, Error)>;
/// An unordered body: its operations, its results, and how many trailing
/// results are dependencies.
type NodesBody = (Vec<OpId>, Vec<ValueId>, usize);
type BlockLabel = (String, BlockArguments, Vec<NamedAttribute>);
/// The `(%a: !ty | %d)` argument list of a block label: the value arguments
/// with their types, and the names of the dependency arguments.
type BlockArguments = (Vec<(String, crate::TypeId)>, Vec<String>);

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

    // Optional result binding prefix: `%2 =`, `%2, %3 =`, `%2 | %4 =` or `| %4 =`,
    // the names after the `|` binding dependencies. The builder allocates the
    // concrete ValueIds; the names are bound once the op exists so later
    // operands resolve by name rather than by a literal id.
    let (result_names, dep_names) = parse_result_prefix(parser)?;

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
        coerce_predicates(parser, context, dialect, name, op.id())?;
        let handle = context.get_op(op.id());
        for (name, result) in result_names.iter().zip(handle.value_results()) {
            parser.define_value(name, result);
        }
        // A dependency the op's own parser did not produce is one the text
        // says it does: the ports memory order threads through an op are its
        // own to carry, at whatever count the binding names.
        let deps = handle.dep_results();
        for (index, name) in dep_names.iter().enumerate() {
            let result = deps
                .get(index)
                .copied()
                .unwrap_or_else(|| context.append_dep_result(op.id()));
            parser.define_value(name, result);
        }
        Ok(op)
    } else {
        Err((parser.span(), Error::ExpectedOpName))
    }
}

/// The names a `%a, %b | %c =` prefix binds, values then dependencies; both
/// empty where the line binds nothing, with the cursor left where it was.
fn parse_result_prefix(parser: &mut TextParser<'_>) -> ParseResult<(Vec<String>, Vec<String>)> {
    let mark = parser.pos();
    let mut result_names = Vec::new();
    while let Some(name) = parser.parse_value_ref() {
        result_names.push(name.to_string());
        if !parser.parse_token(",") {
            break;
        }
    }
    let dep_names = crate::dependency::parse_dep_names(parser).unwrap_or_default();
    if (result_names.is_empty() && dep_names.is_empty()) || !parser.parse_token("=") {
        parser.set_pos(mark);
        return Ok((Vec::new(), Vec::new()));
    }
    Ok((result_names, dep_names))
}

/// Retype the `Str` attributes an op declares as `Predicate`: the attribute
/// parser has no op in scope, so it produces a string and the op's schema
/// decides what it means.
fn coerce_predicates(
    parser: &TextParser<'_>,
    context: &Context,
    dialect: &str,
    name: &str,
    op: crate::OpId,
) -> Result<(), (Span, Error)> {
    let Some(schema) = crate::OP_SCHEMAS
        .iter()
        .find(|schema| schema.dialect == dialect && schema.name == name)
    else {
        return Ok(());
    };
    if !schema.attributes.iter().any(|attr| attr.ty == "Predicate") {
        return Ok(());
    }

    let mut attributes = context.get_op(op).attributes();
    for attr in schema.attributes.iter().filter(|a| a.ty == "Predicate") {
        let Some(sym) = context.sym(attr.name) else {
            continue;
        };
        for stored in attributes.iter_mut().filter(|stored| stored.name == sym) {
            let crate::attributes::AttributeValue::Str(text) = &stored.value else {
                continue;
            };
            let predicate = crate::attributes::Predicate::parse(text).ok_or_else(|| {
                (
                    parser.span(),
                    Error::InvalidPredicate(format!("{dialect}.{name}"), text.to_string()),
                )
            })?;
            stored.value = crate::attributes::AttributeValue::Predicate(predicate);
        }
    }
    context.set_op_attributes(op, attributes);
    Ok(())
}

impl<'src> TextParser<'src> {
    pub fn parse_region(&mut self, context: &Context) -> Result<RegionHandle, (Span, Error)> {
        self.parse_region_with_entry_args(context, vec![])
    }

    /// Parse a region body, ordered or unordered.
    ///
    /// Which one it is, the text says: a `^label:` makes it a control-flow
    /// graph, a trailing `-> %v, ...` line makes it a dependence graph, and a
    /// body with neither is an ordered region of one implicit block. So the
    /// entry block is created only once something needs it, and `entry_args`
    /// become that block's arguments or, in an unordered body, the region's own
    /// ports.
    pub fn parse_region_with_entry_args(
        &mut self,
        context: &Context,
        entry_args: Vec<Value>,
    ) -> Result<RegionHandle, (Span, Error)> {
        self.parse_region_with_entry_args_and_deps(context, entry_args, vec![])
    }

    /// [`Self::parse_region_with_entry_args`] where the region is also entered
    /// on the dependencies `dep_args`, trailing its value arguments.
    pub fn parse_region_with_entry_args_and_deps(
        &mut self,
        context: &Context,
        entry_args: Vec<Value>,
        dep_args: Vec<Value>,
    ) -> Result<RegionHandle, (Span, Error)> {
        if !self.parse_token("{") {
            return Err((self.span(), Error::ExpectedToken("{")));
        }

        let region = context.create_region();
        let enclosing = self.region_parse.take();
        self.region_parse = Some(super::text::RegionParseState {
            region: region.id(),
            labels: HashMap::new(),
            defined: HashSet::new(),
            entry: None,
            arguments: entry_args,
            dep_arguments: dep_args,
        });

        let result = self.parse_region_body(context, &region);
        let state = self.region_parse.take();
        self.region_parse = enclosing;
        let results = result?;

        let mut state = state.expect("the region parse scope survives its region");
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
        match results {
            Some((ops, results, dep_results)) => {
                let (ports, dep_ports) = state.take_arguments();
                context.set_region_nodes(region.id(), ports, dep_ports, ops, results, dep_results);
            }
            None => {
                // A body with no statements at all still owns the block its
                // arguments belong to.
                if state.entry.is_none() {
                    let (arguments, deps) = state.take_arguments();
                    let block = context.create_block_with_dependencies(arguments, deps);
                    region.add_block(block.id());
                }
            }
        }
        Ok(region)
    }

    /// The statements between the braces. Answers with the operations and
    /// results of an unordered body, or `None` when the body was ordered.
    fn parse_region_body(
        &mut self,
        context: &Context,
        region: &RegionHandle,
    ) -> ParseResult<Option<NodesBody>> {
        let mut current: Option<BlockHandle> = None;
        let mut loose: Vec<OpId> = vec![];
        loop {
            self.skip_trivia();
            if self.parse_token("}") {
                self.flush_loose(context, region, &mut current, &loose);
                return Ok(None);
            }

            if let Some((results, dep_results)) = self.try_parse_region_results(context)? {
                if current.is_some() || !region.block_ids().is_empty() {
                    return Err((
                        self.span(),
                        Error::VerificationError(
                            "an unordered region names its results, so it has no blocks"
                                .to_string(),
                        ),
                    ));
                }
                if !self.parse_token("}") {
                    return Err((self.span(), Error::ExpectedToken("}")));
                }
                return Ok(Some((loose, results, dep_results)));
            }

            if let Some((label, block_args, attrs)) = self.try_parse_block_label(context)? {
                self.flush_loose(context, region, &mut current, &loose);
                loose.clear();
                let entry = self.entry_block(context, region);
                let state = self
                    .region_parse
                    .as_mut()
                    .expect("block labels require an active region parse scope");
                if current.is_none() && !state.labels.contains_key(&label) {
                    state.labels.insert(label.clone(), entry);
                    state.defined.insert(label.clone());
                }
                let block = self.block_at_label(context, region, &label, block_args)?;
                for attr in attrs {
                    block.set_attr(&context.resolve(attr.name), attr.value);
                }
                current = Some(block);
                continue;
            }

            let op = parse_single_op(self, context)?;
            match &current {
                Some(block) => block.append(op.id()),
                None => loose.push(op.id()),
            }
        }
    }

    /// Give the operations parsed before any block label to the entry block,
    /// which an ordered body turns out to have after all.
    fn flush_loose(
        &mut self,
        context: &Context,
        region: &RegionHandle,
        current: &mut Option<BlockHandle>,
        loose: &[OpId],
    ) {
        if current.is_some() || loose.is_empty() {
            return;
        }
        let entry = self.entry_block(context, region);
        let block = context.get_block(entry);
        for op in loose {
            block.append(*op);
        }
        *current = Some(block);
    }

    /// The region's entry block, creating it — with the region's arguments, and
    /// under the name `bb0` — the first time it is asked for.
    fn entry_block(&mut self, context: &Context, region: &RegionHandle) -> BlockId {
        let state = self
            .region_parse
            .as_mut()
            .expect("a region body parses inside its own scope");
        if let Some(entry) = state.entry {
            return entry;
        }
        let (arguments, deps) = state.take_arguments();
        let block = context.create_block_with_dependencies(arguments, deps);
        region.add_block(block.id());
        let state = self.region_parse.as_mut().expect("scope checked above");
        state.entry = Some(block.id());
        state.labels.insert("bb0".to_string(), block.id());
        state.defined.insert("bb0".to_string());
        block.id()
    }

    /// The `-> %a, %b | %c` line closing an unordered region, if that is what
    /// comes next: the values it produces, then the dependencies it hands on.
    /// Answers every result with how many trailing ones are dependencies; an
    /// empty result list is written as a bare `->`.
    fn try_parse_region_results(
        &mut self,
        context: &Context,
    ) -> ParseResult<Option<(Vec<ValueId>, usize)>> {
        if !self.parse_token("->") {
            return Ok(None);
        }
        let mut results = vec![];
        while let Some(name) = self.parse_value_ref().map(str::to_string) {
            results.push(self.resolve_value(context, &name));
            if !self.parse_token(",") {
                break;
            }
        }
        let deps = crate::dependency::parse_dep_operands(self, context)?;
        let dep_count = deps.len();
        results.extend(deps);
        Ok(Some((results, dep_count)))
    }

    pub(crate) fn resolve_region_block_label(
        &mut self,
        context: &Context,
        name: &str,
        block_arg_types: &[crate::TypeId],
        dep_arguments: usize,
    ) -> Result<BlockId, (Span, Error)> {
        // The entry block is implicit and printed as `^bb0`, so a branch back to
        // it is what brings it into being when nothing else has.
        if name == "bb0"
            && let Some(region) = self.region_parse.as_ref().map(|state| state.region)
        {
            let region = context.get_region(region);
            self.entry_block(context, &region);
        }
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
            if (!block_arg_types.is_empty() || dep_arguments > 0) && block.arguments().is_empty() {
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
            .chain(
                (0..dep_arguments).map(|_| context.create_value(crate::TypeId::DEPENDENCY, None)),
            )
            .collect();
        let block = context.create_block_with_dependencies(block_args, dep_arguments);
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
            (vec![], vec![])
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

    /// The `%a: !ty, %b: !ty | %c, %d)` list after a label's opening paren.
    fn parse_block_argument_list(
        &mut self,
        context: &Context,
    ) -> Result<BlockArguments, (Span, Error)> {
        let mut args = vec![];

        loop {
            if self.parse_token(")") {
                return Ok((args, vec![]));
            }
            if self.peek_char() == Some('|') {
                let deps = crate::dependency::parse_dep_names(self)?;
                if !self.parse_token(")") {
                    return Err((self.span(), Error::ExpectedToken(")")));
                }
                return Ok((args, deps));
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
                return Ok((args, vec![]));
            }
            if !self.parse_token(",") && self.peek_char() != Some('|') {
                return Err((self.span(), Error::ExpectedToken(",")));
            }
        }
    }

    fn block_at_label(
        &mut self,
        context: &Context,
        region: &RegionHandle,
        label: &str,
        (named_args, dep_names): BlockArguments,
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
            if (!named_args.is_empty() || !dep_names.is_empty()) && block.arguments().is_empty() {
                return Err((
                    self.span(),
                    Error::VerificationError(format!(
                        "block ^{label} was already referenced without arguments"
                    )),
                ));
            }
            for ((name, _), arg) in named_args.iter().zip(block.value_arguments()) {
                self.define_value(name, arg.id());
            }
            for (name, arg) in dep_names.iter().zip(block.dep_arguments()) {
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
            .chain(
                dep_names
                    .iter()
                    .map(|_| context.create_value(crate::TypeId::DEPENDENCY, None)),
            )
            .collect();
        for (name, arg) in named_args
            .iter()
            .map(|(name, _)| name)
            .chain(&dep_names)
            .zip(&block_args)
        {
            self.define_value(name, arg.id());
        }
        let block = context.create_block_with_dependencies(block_args, dep_names.len());
        region.add_block(block.id());
        let state = self.region_parse.as_mut().expect("scope checked above");
        state.labels.insert(label.to_string(), block.id());
        state.defined.insert(label.to_string());
        Ok(block)
    }
}
