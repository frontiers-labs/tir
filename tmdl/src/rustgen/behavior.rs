use super::*;

pub(super) fn emit_as_sem_expr_impl(
    rhs: &ast::Expr,
    name_ident: &proc_macro2::Ident,
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
) -> Option<proc_macro2::TokenStream> {
    let mut dag = tir_symbolic::sem::SemGraph::<()>::new();
    let lowering = rhs.lower_to_sema(&mut dag, numeric_params, isa_param_values)?;
    // The AsSemExpr impl carries no type annotations (the program-graph builder
    // infers them), so pass no widths.
    let root_expr = emit_dag_as_code(&dag, lowering.root, &[]);

    Some(quote! {
        impl tir::sem::AsSemExpr for #name_ident {
            fn convert(
                &self,
                g: &mut tir::sem::SemGraph,
            ) -> tir::NodeId {
                #root_expr
            }
        }
    })
}

pub(super) fn is_store_call(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::Call(ast::Call {
            callee,
            ..
        }) if matches!(callee.as_ref(), ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store))
    )
}

pub(super) fn behavior_has_atomic_ops(expr: &ast::Expr) -> bool {
    let is_atomic = |e: &ast::Expr| {
        matches!(e, ast::Expr::Call(ast::Call { callee, .. }) if matches!(
            callee.as_ref(),
            ast::Expr::BuiltinFunction(
                ast::BuiltinFunction::LoadReserved
                    | ast::BuiltinFunction::StoreConditional
                    | ast::BuiltinFunction::AtomicRmw
                    | ast::BuiltinFunction::Fence
                    | ast::BuiltinFunction::FenceI
            )
        ))
    };
    if is_atomic(expr) {
        return true;
    }
    match expr {
        ast::Expr::Assign(a) => {
            behavior_has_atomic_ops(&a.dest) || behavior_has_atomic_ops(&a.value)
        }
        ast::Expr::Let(l) => behavior_has_atomic_ops(&l.value),
        ast::Expr::Binary(b) => behavior_has_atomic_ops(&b.lhs) || behavior_has_atomic_ops(&b.rhs),
        ast::Expr::Unary(u) => behavior_has_atomic_ops(&u.x),
        ast::Expr::Block(b) => b.stmts.iter().any(behavior_has_atomic_ops),
        ast::Expr::Call(c) => c.arguments.iter().any(behavior_has_atomic_ops),
        ast::Expr::Field(f) => behavior_has_atomic_ops(&f.base),
        ast::Expr::If(i) => {
            behavior_has_atomic_ops(&i.cond)
                || behavior_has_atomic_ops(&i.then)
                || i.else_.as_ref().is_some_and(|e| behavior_has_atomic_ops(e))
        }
        ast::Expr::IndexAccess(i) => behavior_has_atomic_ops(&i.base),
        ast::Expr::Slice(s) => behavior_has_atomic_ops(&s.base),
        ast::Expr::Cast(c) => behavior_has_atomic_ops(&c.x) || behavior_has_atomic_ops(&c.width),
        ast::Expr::Try(t) => {
            behavior_has_atomic_ops(&t.body)
                || t.handlers.iter().any(|h| behavior_has_atomic_ops(&h.body))
        }
        ast::Expr::Lambda(l) => behavior_has_atomic_ops(&l.body),
        ast::Expr::Ident(_)
        | ast::Expr::Lit(_)
        | ast::Expr::Path(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Tuple(_)
        | ast::Expr::Invalid => false,
    }
}

/// The compile-time values a memory access size may be written over: the
/// instruction's own parameters and the ISA parameters of the instantiation
/// being generated.
#[derive(Clone, Copy)]
pub(super) struct ConstSizeParams<'a> {
    pub(super) numeric: &'a HashMap<String, i64>,
    pub(super) isa: &'a HashMap<String, i64>,
}

pub(super) fn behavior_has_dynamic_sized_memory_access(
    expr: &ast::Expr,
    params: &ConstSizeParams<'_>,
) -> bool {
    let recurse = |e: &ast::Expr| behavior_has_dynamic_sized_memory_access(e, params);
    let is_dynamic_sized = |e: &ast::Expr| {
        matches!(e, ast::Expr::Call(ast::Call { callee, arguments, .. }) if matches!(
            callee.as_ref(),
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::Load | ast::BuiltinFunction::Store)
        ) && !arguments.get(1).is_some_and(|size| {
            ast::const_eval_params(size, params.numeric, params.isa).is_some()
        }))
    };
    if is_dynamic_sized(expr) {
        return true;
    }
    match expr {
        ast::Expr::Assign(a) => recurse(&a.dest) || recurse(&a.value),
        ast::Expr::Let(l) => recurse(&l.value),
        ast::Expr::Binary(b) => recurse(&b.lhs) || recurse(&b.rhs),
        ast::Expr::Unary(u) => recurse(&u.x),
        ast::Expr::Block(b) => b.stmts.iter().any(recurse),
        ast::Expr::Call(c) => c.arguments.iter().any(recurse),
        ast::Expr::Field(f) => recurse(&f.base),
        ast::Expr::If(i) => {
            recurse(&i.cond) || recurse(&i.then) || i.else_.as_ref().is_some_and(|e| recurse(e))
        }
        ast::Expr::IndexAccess(i) => recurse(&i.base),
        ast::Expr::Slice(s) => recurse(&s.base),
        ast::Expr::Cast(c) => recurse(&c.x) || recurse(&c.width),
        ast::Expr::Try(t) => recurse(&t.body) || t.handlers.iter().any(|h| recurse(&h.body)),
        ast::Expr::Lambda(l) => recurse(&l.body),
        ast::Expr::Ident(_)
        | ast::Expr::Lit(_)
        | ast::Expr::Path(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Tuple(_)
        | ast::Expr::Invalid => false,
    }
}

pub(super) fn emit_behavior_exec(
    expr: &ast::Expr,
    trap_handler: Option<&ast::TrapHandler>,
    numeric_params: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    ctx: &RustBehaviorCtx<'_>,
) -> Option<proc_macro2::TokenStream> {
    let behavior = sem_expr_state::lower_behavior(
        expr,
        trap_handler,
        numeric_params,
        ctx.isa_param_values,
        register_index_map,
    )?;
    let (max_sym_id, sym_inits) = emit_sym_inits(
        &behavior.variable_symbols,
        &behavior.register_symbols,
        &behavior.regnum_symbols,
        ctx.ops,
        ctx.isa_param_values,
        ctx.reg_kinds,
    );
    let max_sym_id = behavior
        .let_symbols
        .values()
        .copied()
        .max()
        .map_or(max_sym_id, |id| max_sym_id.max(id as usize));
    let sym_count_lit = proc_macro2::Literal::usize_unsuffixed(max_sym_id + 1);
    let effects = emit_behavior_effect(&behavior, behavior.root, ctx)?;
    Some(quote! {
        tir::backend::exec::Program::Effects {
            env: &EXEC_ENV,
            sym_count: #sym_count_lit,
            sources: &[#(#sym_inits),*],
            effects: &[#(#effects),*],
        }
    })
}

pub(super) struct RustBehaviorCtx<'a> {
    pub(super) ops: &'a [(String, Type)],
    pub(super) isa_param_values: &'a HashMap<String, i64>,
    pub(super) reg_kinds: &'a HashMap<String, (bool, u32)>,
}

fn emit_behavior_effect(
    behavior: &sem_expr_state::BehaviorGraph,
    effect: tir_adt::NodeId,
    ctx: &RustBehaviorCtx<'_>,
) -> Option<Vec<proc_macro2::TokenStream>> {
    let children: Vec<_> = behavior.graph.children(effect).collect();
    match behavior.graph.get_node(effect) {
        tir_symbolic::lang::SymKind::StateAssign => match behavior.effect_payload(effect)? {
            sem_expr_state::EffectPayload::Assign { destination } => {
                let offset = emit_behavior_value_offset(behavior, *children.first()?)?;
                let dest = emit_graph_destination(destination, ctx.ops)?;
                Some(vec![quote! {
                    tir::backend::exec::Effect::Assign { offset: #offset, dest: #dest }
                }])
            }
            // The bound term is evaluated here, once, and parked in the symbol
            // table; later statements read the symbol instead of re-evaluating.
            sem_expr_state::EffectPayload::Bind { symbol, .. } => {
                let offset = emit_binding_value_offset(behavior, *children.first()?)?;
                let sym_lit = proc_macro2::Literal::usize_unsuffixed(*symbol as usize);
                Some(vec![quote! {
                    tir::backend::exec::Effect::Bind { offset: #offset, sym: #sym_lit }
                }])
            }
            _ => None,
        },
        tir_symbolic::lang::SymKind::StateStore
        | tir_symbolic::lang::SymKind::StateStoreConditional
        | tir_symbolic::lang::SymKind::StateFence => {
            let offset = emit_behavior_value_offset(behavior, *children.first()?)?;
            Some(vec![quote! {
                tir::backend::exec::Effect::Assign {
                    offset: #offset,
                    dest: tir::backend::exec::Dest::Discard,
                }
            }])
        }
        tir_symbolic::lang::SymKind::StateTrap => {
            let sem_expr_state::EffectPayload::Trap { argument_count, .. } =
                behavior.effect_payload(effect)?
            else {
                return None;
            };
            let cause = *children.get((0..*argument_count).next()?)?;
            let offset = emit_behavior_value_offset(behavior, cause)?;
            Some(vec![quote! {
                tir::backend::exec::Effect::Trap { offset: #offset }
            }])
        }
        // The simulator executes the no-trap path. Handler state is modeled by
        // the SMT printer, while machine exception handling owns trap entry.
        tir_symbolic::lang::SymKind::StateTry => {
            emit_behavior_effect(behavior, *children.first()?, ctx)
        }
        tir_symbolic::lang::SymKind::StateBlock => {
            let mut steps = Vec::new();
            for effect in children {
                steps.extend(emit_behavior_effect(behavior, effect, ctx)?);
            }
            Some(steps)
        }
        tir_symbolic::lang::SymKind::StateIf => {
            let cond = emit_behavior_value_offset(behavior, *children.first()?)?;
            let then_body = emit_behavior_effect(behavior, *children.get(1)?, ctx)?;
            let else_body = match children.get(2) {
                Some(else_effect) => emit_behavior_effect(behavior, *else_effect, ctx)?,
                None => Vec::new(),
            };
            Some(vec![quote! {
                tir::backend::exec::Effect::If {
                    cond: #cond,
                    then: &[#(#then_body),*],
                    els: &[#(#else_body),*],
                }
            }])
        }
        tir_symbolic::lang::SymKind::StateHandler => None,
        _ => None,
    }
}

fn emit_behavior_value_offset(
    behavior: &sem_expr_state::BehaviorGraph,
    root: tir_adt::NodeId,
) -> Option<proc_macro2::Literal> {
    let (values, root) = behavior.bound_value_graph(root)?;
    Some(lowered_value_offset(&values, root))
}

fn emit_binding_value_offset(
    behavior: &sem_expr_state::BehaviorGraph,
    root: tir_adt::NodeId,
) -> Option<proc_macro2::Literal> {
    let (values, root) = behavior.binding_value_graph(root)?;
    Some(lowered_value_offset(&values, root))
}

pub(super) fn lowered_value_offset(
    dag: &tir_symbolic::sem::SemGraph,
    root: tir_adt::NodeId,
) -> proc_macro2::Literal {
    // Behavior value terms carry no type annotations, so no typed nodes.
    let (offset, has_typed_node) = intern_dag(dag, root, &[]);
    assert!(!has_typed_node);
    proc_macro2::Literal::u32_unsuffixed(offset)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_sym_inits(
    variable_symbols: &HashMap<String, u32>,
    register_symbols: &HashMap<(String, u32), u32>,
    regnum_symbols: &HashMap<String, u32>,
    ops: &[(String, Type)],
    isa_param_values: &HashMap<String, i64>,
    reg_kinds: &HashMap<String, (bool, u32)>,
) -> (usize, Vec<proc_macro2::TokenStream>) {
    let max_sym_id = [
        variable_symbols.values().copied().max(),
        register_symbols.values().copied().max(),
        regnum_symbols.values().copied().max(),
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(0) as usize;

    let mut entries: Vec<(u32, proc_macro2::TokenStream)> = Vec::new();
    for (name, &sym_id) in variable_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        if let Some((_, ty)) = ops.iter().find(|(n, _)| n == name) {
            match ty {
                Type::Struct(class_name) => {
                    let (is_float, width) =
                        reg_kinds.get(class_name).copied().unwrap_or((false, 64));
                    if is_float && matches!(width, 16 | 32 | 64) {
                        let (exponent, mantissa) = match width {
                            16 => (5u32, 10u32),
                            32 => (8, 23),
                            _ => (11, 52),
                        };
                        entries.push((
                            sym_id,
                            quote! {
                                (#sym_lit, tir::backend::exec::SymSource::FloatRegisterAttr(
                                    #name_lit, #exponent, #mantissa
                                ))
                            },
                        ));
                        continue;
                    }
                    let variant = if width > 64 {
                        format_ident!("WideRegisterAttr")
                    } else {
                        format_ident!("RegisterAttr")
                    };
                    entries.push((
                        sym_id,
                        quote! {
                            (#sym_lit, tir::backend::exec::SymSource::#variant(#name_lit))
                        },
                    ));
                }
                Type::Integer => entries.push((
                    sym_id,
                    quote! {
                        (#sym_lit, tir::backend::exec::SymSource::IntAttr(#name_lit, 64))
                    },
                )),
                Type::Bits(width) => {
                    let width_lit = proc_macro2::Literal::u32_unsuffixed(*width as u32);
                    entries.push((sym_id, quote! {
                        (#sym_lit, tir::backend::exec::SymSource::IntAttr(#name_lit, #width_lit))
                    }));
                }
                _ => {}
            }
        } else if let Some(&value) = isa_param_values.get(name) {
            // An ISA parameter (e.g. `XLEN`): resolve it from the machine's
            // selected feature set, falling back to the widest TMDL value for
            // contexts that don't configure ISA params.
            let value_lit = proc_macro2::Literal::i64_unsuffixed(value);
            entries.push((
                sym_id,
                quote! {
                    (#sym_lit, tir::backend::exec::SymSource::IsaParam(#name_lit, #value_lit))
                },
            ));
        }
    }
    for ((class, number), &sym_id) in register_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let class_lit = proc_macro2::Literal::string(class);
        let number_lit = proc_macro2::Literal::u16_unsuffixed(*number as u16);
        entries.push((
            sym_id,
            quote! {
                (#sym_lit, tir::backend::exec::SymSource::FixedRegister(#class_lit, #number_lit))
            },
        ));
    }

    // `regnum(op)` binds a symbol to the operand's encoding index. The index is
    // an identity, not an arithmetic value; comparisons coerce by value and
    // ignore width, so a plain 64-bit integer holds it.
    for (name, &sym_id) in regnum_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        entries.push((
            sym_id,
            quote! {
                (#sym_lit, tir::backend::exec::SymSource::RegAttrIndex(#name_lit))
            },
        ));
    }

    // Symbol IDs follow expression traversal, independent of hash order.
    entries.sort_unstable_by_key(|(id, _)| *id);
    (
        max_sym_id,
        entries.into_iter().map(|(_, entry)| entry).collect(),
    )
}

fn emit_graph_destination(
    dest: &sem_expr_state::Destination,
    ops: &[(String, Type)],
) -> Option<proc_macro2::TokenStream> {
    use sem_expr_state::Destination;

    if matches!(dest, Destination::Path { base, members } if base == "PC" && members == &["pc"]) {
        return Some(quote! { tir::backend::exec::Dest::Pc });
    }

    if let Destination::FixedRegister { class, index, .. } = dest {
        let class_lit = proc_macro2::Literal::string(class);
        let index_lit = proc_macro2::Literal::u16_unsuffixed(*index as u16);
        return Some(quote! { tir::backend::exec::Dest::Fixed(#class_lit, #index_lit) });
    }

    let name = match dest {
        Destination::Ident(name) => name,
        Destination::Path { members, .. } if members.len() == 1 => &members[0],
        Destination::FixedRegister { .. }
        | Destination::Path { .. }
        | Destination::Field { .. } => return None,
    };
    if let Some((_, Type::Struct(_))) = ops.iter().find(|(n, _)| n == name) {
        let name_lit = proc_macro2::Literal::string(name);
        return Some(quote! { tir::backend::exec::Dest::Reg(#name_lit) });
    }

    // A local binding: sequencing substituted the bound value into every later
    // read, so the assignment statement itself carries no state effect.
    if matches!(dest, Destination::Ident(_)) {
        return Some(quote! { tir::backend::exec::Dest::Discard });
    }

    None
}

#[allow(clippy::too_many_arguments)]
pub(super) fn intern_dag(
    dag: &tir_symbolic::sem::SemGraph,
    root: tir_adt::NodeId,
    widths: &[Option<u32>],
) -> (u32, bool) {
    let mut ops: Vec<tir_symbolic::sem::SemOp> = Vec::new();
    let mut node_indices: HashMap<usize, u32> = HashMap::new();
    let mut has_typed_node = false;
    let terms = tir_symbolic::term::Terms::new();
    let inferred_types = tir_symbolic::lang::infer_types(&terms, dag, |_| None).ok();
    for (counter, node_id) in dag.postorder(root).enumerate() {
        ops.push(tir_symbolic::sem::SemOp::Node(*dag.get_node(node_id)));

        if let Some(data) = dag.get_leaf_data(node_id) {
            ops.push(tir_symbolic::sem::SemOp::Payload(payload_desc(data)));
        }

        if !matches!(
            inferred_types
                .as_ref()
                .and_then(|types| types.get(node_id.index()))
                .and_then(tir_symbolic::term::TypeArg::as_type)
                .map(|ty| terms.ctor(ty)),
            Some(tir_symbolic::term::ctor::FLOAT)
        ) && !matches!(
            dag.get_node(node_id),
            tir_symbolic::lang::SymKind::FAdd
                | tir_symbolic::lang::SymKind::FSub
                | tir_symbolic::lang::SymKind::FMul
                | tir_symbolic::lang::SymKind::FDiv
                | tir_symbolic::lang::SymKind::FCvt
                | tir_symbolic::lang::SymKind::SIToFP
                | tir_symbolic::lang::SymKind::UIToFP
                | tir_symbolic::lang::SymKind::Bitcast
                | tir_symbolic::lang::SymKind::LoadMemory
                | tir_symbolic::lang::SymKind::LoadReserved
        ) && (dag.get_leaf_data(node_id).is_none()
            || *dag.get_node(node_id) == tir_symbolic::lang::SymKind::Symbol)
            && let Some(Some(width)) = widths.get(node_id.index()).copied()
        {
            ops.push(tir_symbolic::sem::SemOp::Typed(width));
            has_typed_node = true;
        }

        let children: Vec<tir_adt::NodeId> = dag.children(node_id).collect();
        for child_id in children {
            ops.push(tir_symbolic::sem::SemOp::Edge(
                counter as u32,
                node_indices[&child_id.index()],
            ));
        }

        node_indices.insert(node_id.index(), counter as u32);
    }

    (intern_sem_ops(&ops), has_typed_node)
}

fn emit_dag_as_code(
    dag: &tir_symbolic::sem::SemGraph,
    root: tir_adt::NodeId,
    widths: &[Option<u32>],
) -> proc_macro2::TokenStream {
    let (offset, has_typed_node) = intern_dag(dag, root, widths);
    let offset_lit = proc_macro2::Literal::u32_unsuffixed(offset);
    if has_typed_node {
        quote! {
            {
                use tir::sem::ExtendSemBytesTyped as _;
                g.extend_sem_bytes_typed(_context, SEM_KINDS, SEM_BLOB, #offset_lit)
            }
        }
    } else {
        quote! {
            {
                use tir::sem::ExtendSemBytes as _;
                g.extend_sem_bytes(SEM_KINDS, SEM_BLOB, #offset_lit)
            }
        }
    }
}

fn payload_desc(
    payload: &tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>,
) -> tir_symbolic::sem::SemPayloadDesc {
    use tir_symbolic::lang::SymPayload;
    use tir_symbolic::sem::SemPayloadDesc;
    match payload {
        SymPayload::SymbolId(id) => SemPayloadDesc::SymbolId(*id),
        SymPayload::Value(value) => SemPayloadDesc::Value(value.number()),
        SymPayload::Int(v) => SemPayloadDesc::Int {
            width: u32::try_from(v.width()).expect("a TMDL constant fits a 32-bit width"),
            value: if v.is_signed() {
                v.to_i64() as u64
            } else {
                v.to_u64()
            },
            signed: v.is_signed(),
        },
        SymPayload::Float(f) => SemPayloadDesc::Float(f.to_f64()),
    }
}

pub(super) fn emit_expr_kind_ts(kind: &tir_symbolic::lang::SymKind) -> proc_macro2::TokenStream {
    let variant = tir_symbolic::lang::scalar_op(*kind).map_or_else(
        || format_ident!("{kind:?}"),
        |op| format_ident!("{}", op.rust),
    );
    quote! { tir::sem::SymKind::#variant }
}

// ---------------------------------------------------------------------------
// Instruction encoders
