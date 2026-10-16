//! Synthesis of the selection rules, run once when the pass is created.
//!
//! The dialects registered in the binary say which operations a function can
//! hold and at which types. Each such operation is instantiated at every
//! assignment of types its ports admit and its semantic term read off it.

use tir::{
    Context, NewOp, NodeId, OpId, TypeArg, TypeId,
    attributes::AttributeValue,
    builtin::{FloatType, IntegerType},
    schema::{OP_SCHEMAS, OpSchema},
    sem::{SemGraph, SymKind, SymPayload, egraph::minimal_unsigned_apint, infer_types},
};

use tir_symbolic::scheme::{Arity, Slot};

use super::value_type;

pub(super) struct Instance {
    pub(super) schema: &'static OpSchema,
    pub(super) op: OpId,
    pub(super) semantics: SemGraph,
    pub(super) root: NodeId,
    pub(super) types: Option<Vec<TypeArg>>,
    pub(super) plain: bool,
    pub(super) lanes: bool,
}

// The floating-point semantics an operation is instantiated with besides its
// default: each fixed rounding with each handling of exceptions, and the NaN
// policies at the default rounding.
fn meanings(schema: &OpSchema) -> Vec<tir::fp::Semantics> {
    use tir::fp::{
        ComparisonBehavior, ComparisonSemantics, Exceptions, IntegerConversionSemantics, NaNPolicy,
        Rounding, RoundingMode, Semantics,
    };
    if !schema
        .attributes
        .iter()
        .any(|attribute| attribute.ty == "FpSemantics")
    {
        return Vec::new();
    }
    let default = Semantics::default_for_operation(schema.name);
    let modes = [
        RoundingMode::TiesToEven,
        RoundingMode::TowardZero,
        RoundingMode::TowardNegative,
        RoundingMode::TowardPositive,
        RoundingMode::TiesToAway,
    ];
    // A dynamic rounding reads the environment: its term is the operation's
    // resource semantics.
    let roundings = modes.into_iter().map(Rounding::Fixed);
    let roundings = roundings.chain([Rounding::Dynamic]);
    let exceptions = [
        Exceptions::Ignore,
        Exceptions::Flags,
        Exceptions::FlagsAndTraps,
    ];
    let mut found = Vec::new();
    match default {
        Semantics::Arithmetic(base) => {
            for rounding in roundings {
                for exceptions in exceptions {
                    let policies: &[NaNPolicy] = match rounding == base.rounding {
                        true => &[
                            NaNPolicy::AnyQuiet,
                            NaNPolicy::PreservePayload,
                            NaNPolicy::Canonical,
                        ],
                        false => &[NaNPolicy::AnyQuiet],
                    };
                    for &nan in policies {
                        found.push(Semantics::Arithmetic(tir::fp::ArithmeticSemantics {
                            rounding,
                            exceptions,
                            nan,
                            ..base
                        }));
                    }
                }
            }
        }
        Semantics::Comparison(base) => {
            for behavior in [ComparisonBehavior::Quiet, ComparisonBehavior::Signaling] {
                for exceptions in exceptions {
                    found.push(Semantics::Comparison(ComparisonSemantics {
                        behavior,
                        exceptions,
                        ..base
                    }));
                }
            }
        }
        Semantics::IntegerConversion(base) => {
            for rounding in roundings {
                for exceptions in exceptions {
                    found.push(Semantics::IntegerConversion(IntegerConversionSemantics {
                        rounding,
                        exceptions,
                        ..base
                    }));
                }
            }
        }
    }
    found.retain(|meaning| *meaning != default);
    found
}

pub(super) const INTEGER_WIDTHS: [u32; 6] = [1, 5, 8, 16, 32, 64];

// The type standing for an optional port an instance goes without.
fn absent() -> TypeId {
    TypeId::from_number(u32::MAX)
}

pub(super) fn vectors(context: &Context, rules: &[super::Rule], widths: &[u32]) -> Vec<TypeId> {
    let mut named = std::collections::BTreeSet::new();
    let mut any = false;
    let patterns = rules
        .iter()
        .flat_map(|rule| std::iter::once(&rule.pattern).chain(&rule.secondary));
    for pattern in patterns {
        let symbol = |node: NodeId| *pattern.get_node(node) == SymKind::Symbol;
        for node in (0..pattern.len()).map(NodeId::from_index) {
            if *pattern.get_node(node) != SymKind::Split {
                continue;
            }
            let mut read = pattern.children(node).skip(1);
            let (Some(lanes), Some(width)) = (read.next(), read.next()) else {
                continue;
            };
            if !symbol(lanes) {
                continue;
            }
            match pattern.get_leaf_data(width) {
                Some(SymPayload::Int(width)) => {
                    named.insert(width.to_u64());
                }
                _ => any |= symbol(width),
            }
        }
    }
    let elements = widths.iter().filter(|&&width| match any {
        true => width % 8 == 0,
        false => named.contains(&u64::from(width)),
    });
    elements
        .map(|&width| {
            tir::vector::VectorType::dynamic(context, IntegerType::new(context, u64::from(width)))
        })
        .collect()
}

// The types a port of `slot` is instantiated at: `types` holds the integer
// widths and the vector types the rules compute in.
fn candidates(context: &Context, types: (&[u32], &[TypeId]), slot: &Slot) -> Option<Vec<TypeId>> {
    let mut found = pattern_candidates(context, types, &slot.pattern)?;
    // A port an operation may go without is also instantiated without it.
    if slot.arity == Arity::Opt {
        found.push(absent());
    }
    Some(found)
}

fn pattern_candidates(
    context: &Context,
    types: (&[u32], &[TypeId]),
    pattern: &TypeArg,
) -> Option<Vec<TypeId>> {
    let (widths, vectors) = types;
    let ty = match pattern {
        // A port that takes any value: every scalar an instance is built of.
        TypeArg::Var(_) => {
            let ints = widths
                .iter()
                .map(|&width| IntegerType::new(context, u64::from(width)));
            let floats = [FloatType::f32(context), FloatType::f64(context)];
            let pointer = tir::ptr::PtrType::opaque(context);
            return Some(ints.chain(floats).chain([pointer]).collect());
        }
        TypeArg::Type(ty) => *ty,
        _ => return None,
    };
    if let Some(width) = context.terms().int_width(ty) {
        return Some(vec![IntegerType::new(context, width)]);
    }
    if context.terms().ctor(ty) == tir_symbolic::term::ctor::INT {
        return Some(
            widths
                .iter()
                .map(|&width| IntegerType::new(context, u64::from(width)))
                .collect(),
        );
    }
    if context.terms().ctor(ty) == tir_symbolic::term::ctor::FLOAT {
        return Some(vec![FloatType::f32(context), FloatType::f64(context)]);
    }
    let is =
        |dialect, name| context.terms().ctor_named(dialect, name) == Some(context.terms().ctor(ty));
    if is("vector", "v") {
        // No instance where the target has no vector.
        return (!vectors.is_empty()).then(|| vectors.to_vec());
    }
    if is("ptr", "p") {
        return Some(vec![tir::ptr::PtrType::opaque(context)]);
    }
    let ground = matches!(pattern, TypeArg::Type(_)) && !has_vars(context.terms(), pattern);
    let scalar = |ty| {
        tir::fp::EnvironmentType::view(context, ty).is_some()
            || tir::fp::RoundingType::view(context, ty).is_some()
            || tir::builtin::IndexType::view(context, ty).is_some()
    };
    (ground && scalar(ty)).then(|| vec![ty])
}

fn has_vars(terms: &tir::Terms, arg: &TypeArg) -> bool {
    match arg {
        TypeArg::Var(_) => true,
        TypeArg::Type(ty) => terms
            .term(*ty)
            .args()
            .iter()
            .any(|arg| has_vars(terms, arg)),
        TypeArg::List(items) => items.iter().any(|arg| has_vars(terms, arg)),
        _ => false,
    }
}

// The registry is in link order, which changes from one build to the next:
// the instances are numbered in an order of their own.
fn schemas() -> Vec<&'static OpSchema> {
    let mut all: Vec<&'static OpSchema> = OP_SCHEMAS.iter().collect();
    all.sort_by_key(|schema| (schema.dialect, schema.name));
    all
}

pub(super) fn instances(
    scratch: &Context,
    types: (&[u32], &[TypeId]),
    layout: Option<&AttributeValue>,
    skipped: &mut Vec<(&'static str, &'static str, &'static str)>,
) -> Vec<Instance> {
    let mut found = Vec::new();
    for schema in schemas() {
        let mut skip = |why| skipped.push((schema.dialect, schema.name, why));
        // The registry lists every op linked in; the context holds the
        // dialects this compile uses.
        if scratch.get_parser(schema.dialect, schema.name).is_err() {
            skip("unregistered");
            continue;
        }
        // The state ports an operation may go without are left out here;
        // a floating-point meaning that needs them gets them below, and the
        // accesses to memory are built apart.
        let fixed = |port: &&tir::schema::FieldSchema| !port.variadic;
        let all = schema.operands.iter().chain(schema.results);
        if all
            .clone()
            .any(|port| port.variadic && port.name != "state")
        {
            skip("variadic");
            continue;
        }
        if ["MemoryRead", "MemoryWrite"]
            .iter()
            .any(|access| schema.interfaces.contains(access))
        {
            skip("access");
            continue;
        }
        let ports: Vec<_> = all.filter(fixed).collect();
        if schema.results.is_empty() {
            skip("no result");
            continue;
        }
        // Instantiating an op of no semantics builds nothing to select by,
        // and its wildcard ports would be built at every type.
        if !schema.semantics && !schema.interfaces.contains(&"HasResourceSemantics") {
            skip("no semantics");
            continue;
        }
        let Ok(scheme) = tir::scheme_of(scratch, schema) else {
            skip("scheme");
            continue;
        };
        let Some(choices) = scheme
            .slots()
            .filter(|slot| slot.arity != Arity::Many)
            .map(|slot| candidates(scratch, types, slot))
            .collect::<Option<Vec<_>>>()
        else {
            skip("port type");
            continue;
        };
        // Floating-point semantics are no predicate: the instances of each
        // meaning are built below.
        let attributes: Vec<_> = schema
            .attributes
            .iter()
            .filter(|attribute| attribute.ty != "FpSemantics")
            // A flag only makes the result poison where it fails: selection
            // forgets it, so the instance has none.
            .filter(|attribute| attribute.ty != "?Flag")
            .collect();
        let predicates: Vec<Option<(&'static str, tir::attributes::Predicate)>> =
            match attributes.as_slice() {
                [] => vec![None],
                [attribute] if !attribute.vocabulary.is_empty() => attribute
                    .vocabulary
                    .iter()
                    .map(|&predicate| Some((attribute.name, predicate)))
                    .collect(),
                _ => {
                    skip("attribute");
                    continue;
                }
            };
        let uniform = schema.interfaces.contains(&"SameOperandAndResultType");
        let others = meanings(schema);
        let states = [
            tir::builtin::StateType::fp_env(scratch),
            tir::builtin::StateType::memory(scratch),
        ];
        // The vector ports of an operation are one type, as it verifies: an
        // assignment mixing them is not built to be refused.
        let (_, vectors) = types;
        let mixed = |types: &[TypeId]| {
            let mut held = types.iter().filter(|ty| vectors.contains(ty));
            held.next()
                .is_some_and(|first| held.any(|other| other != first))
        };
        let mut pick = vec![0usize; ports.len()];
        'assignments: loop {
            let types: Vec<TypeId> = pick
                .iter()
                .zip(&choices)
                .map(|(&index, choice)| choice[index])
                .collect();
            let uniform = !uniform || types.windows(2).all(|pair| pair[0] == pair[1]);
            if uniform && !mixed(&types) {
                for predicate in &predicates {
                    // An operation on the environment has its state whatever
                    // its meaning.
                    let shapes: [&[TypeId]; 2] = [&[], &states[..1]];
                    let mut built = shapes.iter().map(|states| {
                        instantiate(scratch, layout, schema, &types, *predicate, None, states)
                    });
                    match built.next().expect("two shapes") {
                        Ok(instance) => found.push(instance),
                        Err(why) => match built.next().and_then(Result::ok) {
                            Some(instance) => found.push(instance),
                            None => skip(why),
                        },
                    }
                    // A meaning that touches the floating-point environment
                    // or may trap has the states it does so through.
                    for meaning in &others {
                        let shapes: [&[TypeId]; 3] = [&[], &states[..1], &states];
                        let built = shapes.iter().find_map(|states| {
                            let meaning = Some(*meaning);
                            instantiate(
                                scratch, layout, schema, &types, *predicate, meaning, states,
                            )
                            .ok()
                        });
                        found.extend(built);
                    }
                }
            }
            for (slot, choice) in pick.iter_mut().zip(&choices) {
                *slot += 1;
                if *slot < choice.len() {
                    continue 'assignments;
                }
                *slot = 0;
            }
            break;
        }
    }
    found
}

fn instantiate(
    scratch: &Context,
    layout: Option<&AttributeValue>,
    schema: &'static OpSchema,
    types: &[TypeId],
    predicate: Option<(&'static str, tir::attributes::Predicate)>,
    meaning: Option<tir::fp::Semantics>,
    states: &[TypeId],
) -> Result<Instance, &'static str> {
    let read = schema.operands.iter().filter(|port| !port.variadic).count();
    let (operands, results) = types.split_at(read);
    // An operand has to be defined somewhere to verify: a block argument is.
    // The states come after the values, observed and published alike.
    let block = scratch.create_block(Vec::new());
    let operands = operands
        .iter()
        .filter(|&&ty| ty != absent())
        .chain(states)
        .map(|&ty| scratch.append_block_argument(block.id, ty).id())
        .collect();
    let results = results
        .iter()
        .chain(states)
        .map(|&ty| scratch.create_value(ty, None).id())
        .collect();
    let attributes = predicate
        .map(|(name, predicate)| {
            scratch.named_attribute(name, AttributeValue::Predicate(predicate))
        })
        .into_iter()
        // A detached op reads the data layout it carries itself.
        .chain(layout.map(|spec| scratch.named_attribute(tir::LAYOUT_SPEC, spec.clone())))
        // Floating-point semantics are the operation's default ones unless
        // the instance is of another meaning.
        .chain(
            schema
                .attributes
                .iter()
                .filter(|attribute| attribute.ty == "FpSemantics")
                .map(|attribute| {
                    let default = tir::fp::Semantics::default_for_operation(schema.name);
                    let held = std::sync::Arc::new(meaning.unwrap_or(default));
                    let value = AttributeValue::FpSemantics(held);
                    scratch.named_attribute(attribute.name, value)
                }),
        )
        // An operation with a port it may go without says how many operands
        // each port has.
        .chain(schema.operands.iter().any(|port| port.variadic).then(|| {
            let sizes = schema.operands.iter();
            let sizes = sizes.map(|port| {
                AttributeValue::UInt(if port.variadic {
                    states.len() as u64
                } else {
                    1
                })
            });
            scratch.named_attribute(
                "operand_segment_sizes",
                AttributeValue::Array(sizes.collect()),
            )
        }))
        .collect();
    let op = scratch.add_operation(NewOp::new_dynamic(
        (schema.dialect, schema.name),
        scratch.clone(),
        operands,
        results,
        Vec::new(),
        attributes,
    ));
    if let Err(error) = op.clone().as_dyn_op().verify(scratch) {
        if std::env::var_os("TIR_ISEL_SYNTH").is_some() {
            eprintln!(
                "isel-synth: {}.{} does not verify: {error:?}",
                schema.dialect, schema.name
            );
        }
        return Err("does not verify");
    }
    let mut semantics = SemGraph::new();
    let mut root = op.clone().as_dyn_op().semantic_expr(&mut semantics);
    // An operation on the floating-point environment has no value term of
    // its own: what it computes is what it does to the environment.
    if root.is_none()
        && let Some(effects) = op.clone().as_interface::<dyn tir::HasResourceSemantics>()
    {
        let resource = effects.resource_semantics();
        semantics = SemGraph::new();
        // The term is the whole of the operation unless it both computes a
        // value and changes the environment beside it: then the change is
        // only what its flags are checked for.
        let changes = op
            .clone()
            .as_interface::<dyn tir::ResourceEffects>()
            .map(|effects| effects.resource_effects())
            .is_some_and(|effects| {
                let changed = |effect: &tir::ResourceEffect| {
                    effect.resource == tir::builtin::StateResource::FpEnv
                        && effect.access == tir::ResourceAccess::Change
                };
                effects.iter().any(changed)
            });
        let whole =
            resource.value_results.is_empty() || resource.raised_flags.is_some() || !changes;
        if whole {
            let mut copied = std::collections::HashMap::new();
            root = spelled(
                &resource.graph,
                resource.root,
                &op,
                &mut semantics,
                &mut copied,
            );
        }
    }
    let Some(root) = root else {
        return Err("no semantics");
    };
    // An assignment the port constraints admit can still be no operation:
    // a comparison of two widths, an extension that narrows.
    let Some(types) = typed(scratch, &op, &semantics) else {
        return Err("ill-typed");
    };
    Ok(Instance {
        schema,
        op: op.id,
        semantics,
        root,
        types: Some(types),
        plain: meaning.is_none() && states.is_empty(),
        lanes: op.results().first().is_some_and(|&result| {
            tir::vector::VectorType::view(scratch, scratch.get_value(result).ty()).is_some()
        }),
    })
}

fn typed(scratch: &Context, op: &tir::OpHandle, semantics: &SemGraph) -> Option<Vec<TypeArg>> {
    let terms = scratch.terms();
    let types = infer_types(terms, semantics, |node| {
        semantics
            .get_annotation(node)
            .and_then(|meta| meta.actual_type)
            .and_then(|ty| value_type(terms, ty))
            .or_else(|| match semantics.get_leaf_data(node) {
                Some(SymPayload::SymbolId(id)) => op
                    .operands()
                    .get(*id as usize)
                    .and_then(|&operand| value_type(terms, scratch.get_value(operand).ty())),
                _ => None,
            })
    })
    .ok()?;
    Some(types)
}

// Copy the term at `node` of the resource semantics of `op` into `target`,
// each value it names spelled as the operand of `op` it is.
fn spelled(
    source: &SemGraph,
    node: NodeId,
    op: &tir::OpHandle,
    target: &mut SemGraph,
    copied: &mut std::collections::HashMap<NodeId, NodeId>,
) -> Option<NodeId> {
    if let Some(&known) = copied.get(&node) {
        return Some(known);
    }
    let children = source
        .children(node)
        .map(|child| spelled(source, child, op, target, copied))
        .collect::<Option<Vec<_>>>()?;
    // A graph takes edges only from its newest node.
    let made = target.add_node(*source.get_node(node));
    if let Some(payload) = source.get_leaf_data(node) {
        let payload = match payload {
            SymPayload::Value(value) => {
                let operands = op.operands();
                let operand = operands.iter().position(|operand| operand == value)?;
                SymPayload::SymbolId(operand as u32)
            }
            other => other.clone(),
        };
        target.set_leaf_data(made, payload);
    }
    if let Some(ty) = source
        .get_annotation(node)
        .and_then(|meta| meta.actual_type)
    {
        target.annotation_mut(made).actual_type = Some(ty);
    }
    for child in children {
        target.add_edge(made, child);
    }
    copied.insert(node, made);
    Some(made)
}

fn leaf(graph: &mut SemGraph, payload: SymPayload<tir::ValueId>) -> NodeId {
    let kind = match payload {
        SymPayload::Int(_) => SymKind::Constant,
        _ => SymKind::Symbol,
    };
    let node = graph.add_node(kind);
    graph.set_leaf_data(node, payload);
    node
}

// A memory access has no semantic string: it is the access term over its
// address, the bytes it moves and the state it observes, as the IR's memory
// interfaces spell it.
pub(super) fn accesses(
    scratch: &Context,
    address: u32,
    skipped: &mut Vec<(&'static str, &'static str, &'static str)>,
    found: &mut Vec<Instance>,
) {
    let state = tir::builtin::StateType::memory(scratch);
    let pointer = tir::ptr::PtrType::opaque(scratch);
    let mut moved: Vec<(TypeId, u32)> = INTEGER_WIDTHS
        .iter()
        .filter(|&&width| width % 8 == 0)
        .map(|&width| (IntegerType::new(scratch, u64::from(width)), width))
        .collect();
    moved.push((FloatType::new(scratch, 8, 23), 32));
    moved.push((FloatType::new(scratch, 11, 52), 64));
    moved.push((pointer, address));
    // Two machine words moved as one value: a wide register's load and
    // store.
    moved.push((IntegerType::new(scratch, 128), 128));
    for schema in schemas() {
        let reads = schema.interfaces.contains(&"MemoryRead");
        let writes = schema.interfaces.contains(&"MemoryWrite");
        if !reads && !writes || scratch.get_parser(schema.dialect, schema.name).is_err() {
            continue;
        }
        let fixed =
            |ports: &[tir::schema::FieldSchema]| ports.iter().filter(|port| !port.variadic).count();
        let shape = (fixed(schema.operands), fixed(schema.results));
        if shape != if reads { (1, 1) } else { (2, 0) } {
            skipped.push((schema.dialect, schema.name, "access shape"));
            continue;
        }
        for &(ty, width) in &moved {
            let block = scratch.create_block(Vec::new());
            let port = |ty| scratch.append_block_argument(block.id, ty).id();
            let (operands, results) = if reads {
                (vec![port(pointer), port(state)], vec![ty, state])
            } else {
                (vec![port(ty), port(pointer), port(state)], vec![state])
            };
            let sizes = vec![AttributeValue::UInt(1); schema.operands.len()];
            let attributes = vec![
                scratch
                    .named_attribute("operand_segment_sizes", AttributeValue::Array(sizes.into())),
            ];
            let op = scratch.add_operation(NewOp::new_dynamic(
                (schema.dialect, schema.name),
                scratch.clone(),
                operands,
                results
                    .iter()
                    .map(|&ty| scratch.create_value(ty, None).id())
                    .collect(),
                Vec::new(),
                attributes,
            ));
            if op.clone().as_dyn_op().verify(scratch).is_err() {
                skipped.push((schema.dialect, schema.name, "does not verify"));
                continue;
            }
            let mut semantics = SemGraph::new();
            let bytes = SymPayload::Int(minimal_unsigned_apint(u64::from(width / 8)));
            let space = SymPayload::Int(minimal_unsigned_apint(0));
            let symbol = |index| SymPayload::SymbolId(index);
            let (kind, children) = if reads {
                (
                    SymKind::LoadMemory,
                    vec![symbol(0), bytes, space, symbol(1)],
                )
            } else {
                (
                    SymKind::StoreMemory,
                    vec![symbol(1), bytes, symbol(0), space, symbol(2)],
                )
            };
            // A graph takes edges only from its newest node.
            let children: Vec<NodeId> = children
                .into_iter()
                .map(|child| leaf(&mut semantics, child))
                .collect();
            let root = semantics.add_node(kind);
            for child in children {
                semantics.add_edge(root, child);
            }
            semantics.annotation_mut(root).actual_type = Some(if reads { ty } else { state });
            debug_assert!(
                scratch
                    .terms()
                    .bit_width(ty)
                    .is_none_or(|bits| bits == u64::from(width))
            );
            found.push(Instance {
                schema,
                op: op.id,
                types: typed(scratch, &op, &semantics),
                semantics,
                root,
                plain: true,
                lanes: false,
            });
        }
    }
}
