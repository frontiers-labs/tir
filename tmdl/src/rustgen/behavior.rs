fn emit_as_sem_expr_impl(
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
                g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
            ) -> tir::graph::NodeId {
                #root_expr
            }
        }
    })
}

fn is_store_call(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::Call(ast::Call {
            callee,
            ..
        }) if matches!(callee.as_ref(), ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store))
    )
}

/// Whether the behavior contains any atomic/fence builtin. Such behaviors are
/// excluded from instruction selection and op-sem pattern generation.
fn behavior_has_atomic_ops(expr: &ast::Expr) -> bool {
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
        | ast::Expr::Invalid => false,
    }
}

/// The compile-time values a memory access size may be written over: the
/// instruction's own parameters and the ISA parameters of the instantiation
/// being generated.
#[derive(Clone, Copy)]
struct ConstSizeParams<'a> {
    numeric: &'a HashMap<String, i64>,
    isa: &'a HashMap<String, i64>,
}

/// Whether the behavior loads or stores with a non-constant size (e.g. RVV
/// unit-stride forms sized by `vl`). A static selection pattern cannot
/// express a dynamic access size, and leaving such rules in would let plain
/// scalar loads/stores match them by binding the size, so they are excluded
/// from instruction selection and op-sem pattern generation. A size that
/// const-evaluates under the ISA parameters (`self.XLEN / 8`) is concrete for
/// this ISA instantiation and stays in.
fn behavior_has_dynamic_sized_memory_access(
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
            recurse(&i.cond)
                || recurse(&i.then)
                || i.else_.as_ref().is_some_and(|e| recurse(e))
        }
        ast::Expr::IndexAccess(i) => recurse(&i.base),
        ast::Expr::Slice(s) => recurse(&s.base),
        ast::Expr::Cast(c) => recurse(&c.x) || recurse(&c.width),
        ast::Expr::Try(t) => {
            recurse(&t.body) || t.handlers.iter().any(|h| recurse(&h.body))
        }
        ast::Expr::Lambda(l) => recurse(&l.body),
        ast::Expr::Ident(_)
        | ast::Expr::Lit(_)
        | ast::Expr::Path(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Invalid => false,
    }
}

fn emit_behavior_exec(
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

struct RustBehaviorCtx<'a> {
    ops: &'a [(String, Type)],
    isa_param_values: &'a HashMap<String, i64>,
    reg_kinds: &'a HashMap<String, (bool, u32)>,
}

fn emit_behavior_effect(
    behavior: &sem_expr_state::BehaviorGraph,
    effect: tir_graph::NodeId,
    ctx: &RustBehaviorCtx<'_>,
) -> Option<Vec<proc_macro2::TokenStream>> {
    use tir_graph::Dag as _;

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
        tir_symbolic::lang::SymKind::StateTry => emit_behavior_effect(behavior, *children.first()?, ctx),
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
    root: tir_graph::NodeId,
) -> Option<proc_macro2::Literal> {
    let (values, root) = behavior.bound_value_graph(root)?;
    Some(lowered_value_offset(&values, root))
}

fn emit_binding_value_offset(
    behavior: &sem_expr_state::BehaviorGraph,
    root: tir_graph::NodeId,
) -> Option<proc_macro2::Literal> {
    let (values, root) = behavior.binding_value_graph(root)?;
    Some(lowered_value_offset(&values, root))
}

fn lowered_value_offset(
    dag: &impl tir_graph::Dag<Node = tir_symbolic::lang::SymKind, Leaf = tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>>,
    root: tir_graph::NodeId,
) -> proc_macro2::Literal {
    // Behavior value terms carry no type annotations, so no typed nodes.
    let (offset, has_typed_node) = intern_dag(dag, root, &[]);
    assert!(!has_typed_node);
    proc_macro2::Literal::u32_unsuffixed(offset)
}

/// Emit the [`tir::backend::exec::SymSource`] table entries binding an
/// instruction's entry symbols: register operands and fixed/status registers
/// are read from the machine; integer operands and ISA parameters are bound to
/// constants. Returns the highest symbol id (to size the table) and the entries.
#[allow(clippy::too_many_arguments)]
fn emit_sym_inits(
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

    let mut entries: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, &sym_id) in variable_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        if let Some((_, ty)) = ops.iter().find(|(n, _)| n == name) {
            match ty {
                Type::Struct(class_name) => {
                    let (_is_float, width) =
                        reg_kinds.get(class_name).copied().unwrap_or((false, 64));
                    // A vector operand (wider than a word) is read as raw byte
                    // lanes; the behavior splits it into lanes and interprets each
                    // as int or float. Scalar operands — integer and float alike —
                    // read as an `APInt` bit pattern: float operations reinterpret
                    // those bits via the node's float type, so a float value is
                    // never forced whole through the wrong representation, and a
                    // bit move (`fmov Xd,Dn`) reads the pattern directly.
                    let variant = if width > 64 {
                        format_ident!("WideRegisterAttr")
                    } else {
                        format_ident!("RegisterAttr")
                    };
                    entries.push(quote! {
                        (#sym_lit, tir::backend::exec::SymSource::#variant(#name_lit))
                    });
                }
                Type::Integer => entries.push(quote! {
                    (#sym_lit, tir::backend::exec::SymSource::IntAttr(#name_lit, 64))
                }),
                Type::Bits(width) => {
                    let width_lit = proc_macro2::Literal::u32_unsuffixed(*width as u32);
                    entries.push(quote! {
                        (#sym_lit, tir::backend::exec::SymSource::IntAttr(#name_lit, #width_lit))
                    });
                }
                _ => {}
            }
        } else if let Some(&value) = isa_param_values.get(name) {
            // An ISA parameter (e.g. `XLEN`): resolve it from the machine's
            // selected feature set, falling back to the widest TMDL value for
            // contexts that don't configure ISA params.
            let value_lit = proc_macro2::Literal::i64_unsuffixed(value);
            entries.push(quote! {
                (#sym_lit, tir::backend::exec::SymSource::IsaParam(#name_lit, #value_lit))
            });
        }
    }
    for ((class, number), &sym_id) in register_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let class_lit = proc_macro2::Literal::string(class);
        let number_lit = proc_macro2::Literal::u16_unsuffixed(*number as u16);
        entries.push(quote! {
            (#sym_lit, tir::backend::exec::SymSource::FixedRegister(#class_lit, #number_lit))
        });
    }

    // `regnum(op)` binds a symbol to the operand's encoding index. The index is
    // an identity, not an arithmetic value; comparisons coerce by value and
    // ignore width, so a plain 64-bit integer holds it.
    for (name, &sym_id) in regnum_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        entries.push(quote! {
            (#sym_lit, tir::backend::exec::SymSource::RegAttrIndex(#name_lit))
        });
    }

    (max_sym_id, entries)
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

/// Emit the pattern function, emit function, and rule registration for one
/// conditional-branch rule. Operands named in `zero_slots` are wired to a fixed
/// physical register (a class's hardwired-zero register) instead of bound from
/// the match — the mechanism behind the zero-form branch variants; every other
/// register/immediate operand binds from the match as usual.
#[allow(clippy::too_many_arguments)]
fn emit_cond_branch_rule(
    rule_name: &str,
    dialect: &str,
    op_name: &str,
    op_ty_ident: &proc_macro2::Ident,
    inst_name: &str,
    for_isas: &[String],
    ops: &[(String, Type)],
    pattern: &tir_symbolic::sem::SemGraph,
    root: tir_graph::NodeId,
    variable_symbols: &HashMap<String, u32>,
    target_operand: &str,
    target_symbol: u32,
    zero_slots: &HashMap<String, (String, u16)>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> (proc_macro2::TokenStream, proc_macro2::Ident) {
    let mut operand_constraint_entries: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut emit_attrs: Vec<proc_macro2::TokenStream> = Vec::new();
    for (op_name, op_ty) in ops {
        if op_name == target_operand {
            emit_attrs.push(emit_attr_block(op_name, target_symbol));
            continue;
        }
        if let Some((class_name, index)) = zero_slots.get(op_name) {
            emit_attrs.push(emit_attr_physical(op_name, &reg_class_id(class_name), *index));
            continue;
        }
        let Some(&symbol) = variable_symbols.get(op_name) else {
            continue;
        };
        match op_ty {
            Type::Struct(_) => {
                operand_constraint_entries.push(constraint_entry(
                    symbol,
                    quote! { tir::graph::OperandConstraint::Register },
                ));
                emit_attrs.push(emit_attr_value(op_name, symbol));
            }
            Type::Integer | Type::Bits(_) => {
                operand_constraint_entries.push(constraint_entry(
                    symbol,
                    quote! { tir::graph::OperandConstraint::Immediate },
                ));
                emit_attrs.push(emit_attr_int(op_name, symbol));
            }
            _ => {}
        }
    }

    let immediate_symbols: HashSet<u32> = ops
        .iter()
        .filter(|(_, op_ty)| matches!(op_ty, Type::Bits(_) | Type::Integer))
        .filter_map(|(op_name, _)| variable_symbols.get(op_name).copied())
        .collect();
    let (canon_pattern, canon_root, forced_widths) =
        tir_symbolic::lang::canonicalize_for_selection(pattern, root, &immediate_symbols);
    let mut pattern_widths = tir_symbolic::lang::infer_widths(&canon_pattern, |_| None);
    for (index, forced) in forced_widths.iter().enumerate() {
        if forced.is_some() {
            pattern_widths[index] = *forced;
        }
    }
    let (offset, typed) = intern_dag(&canon_pattern, canon_root, &pattern_widths);
    let pattern_spec = SpecPattern {
        offset,
        typed,
        float_width: None,
    };
    let operand_register_specs = operand_register_specs_for_ops(
        ops,
        variable_symbols,
        &width_sensitive_symbols(&canon_pattern, &pattern_widths),
        float_classes,
        polymorphic_classes,
    );
    let imm_range_entries =
        imm_range_spec_entries(&immediate_operand_ranges(pattern, ops, variable_symbols));

    let (emitter_ts, emit_shim) =
        emit_emitter_spec(rule_name, dialect, op_name, op_ty_ident, &emit_attrs, inst_name);
    let target_symbol_lit = proc_macro2::Literal::u32_unsuffixed(target_symbol);
    let (rule_ts, rule_ident) = emit_rule_spec(
        rule_name,
        rule_name,
        for_isas,
        &pattern_spec,
        &[inst_name],
        quote! {
            tir::backend::isel::RuleKind::CondBranch {
                target_symbol: #target_symbol_lit,
            }
        },
        None,
        &emit_shim,
        &operand_constraint_entries,
        &operand_register_specs,
        None,
        &imm_range_entries,
        None,
    );
    (
        quote! {
            #emitter_ts
            #rule_ts
        },
        rule_ident,
    )
}

/// Clone `pattern` with the register-operand symbol `reg_symbol` replaced by the
/// `zext(0b0, W)` zero shape. `width_symbol` is the fresh wildcard the extension
/// width binds to — matched but never read by the emitter.
fn branch_pattern_with_zero(
    pattern: &tir_symbolic::sem::SemGraph,
    root: tir_graph::NodeId,
    reg_symbol: u32,
    width_symbol: u32,
) -> (tir_symbolic::sem::SemGraph, tir_graph::NodeId) {
    let mut out = tir_symbolic::sem::SemGraph::new();
    let mut memo: HashMap<usize, tir_graph::NodeId> = HashMap::new();
    let new_root =
        clone_pattern_with_zero(pattern, root, reg_symbol, width_symbol, &mut out, &mut memo);
    (out, new_root)
}

fn clone_pattern_with_zero(
    pattern: &tir_symbolic::sem::SemGraph,
    node: tir_graph::NodeId,
    reg_symbol: u32,
    width_symbol: u32,
    out: &mut tir_symbolic::sem::SemGraph,
    memo: &mut HashMap<usize, tir_graph::NodeId>,
) -> tir_graph::NodeId {
    use tir_graph::{Dag, MutDag};
    if let Some(&existing) = memo.get(&node.index()) {
        return existing;
    }
    if *pattern.get_node(node) == tir_symbolic::lang::SymKind::Symbol
        && matches!(
            pattern.get_leaf_data(node),
            Some(tir_symbolic::lang::SymPayload::SymbolId(s)) if *s == reg_symbol
        )
    {
        let zero = out.add_node(tir_symbolic::lang::SymKind::Constant);
        out.set_leaf_data(zero, tir_symbolic::sem::int_payload(1, 0, false));
        let width = out.add_node(tir_symbolic::lang::SymKind::Symbol);
        out.set_leaf_data(width, tir_symbolic::lang::SymPayload::SymbolId(width_symbol));
        let zext = out.add_node(tir_symbolic::lang::SymKind::ZExt);
        out.add_edge(zext, zero);
        out.add_edge(zext, width);
        memo.insert(node.index(), zext);
        return zext;
    }
    // Children first: the store keeps strict post-order (a child's index must
    // precede its parent's).
    let kind = *pattern.get_node(node);
    let new_children: Vec<tir_graph::NodeId> = pattern
        .children(node)
        .collect::<Vec<_>>()
        .into_iter()
        .map(|child| clone_pattern_with_zero(pattern, child, reg_symbol, width_symbol, out, memo))
        .collect();
    let new_node = out.add_node(kind);
    if let Some(data) = pattern.get_leaf_data(node) {
        out.set_leaf_data(new_node, data.clone());
    }
    for new_child in new_children {
        out.add_edge(new_node, new_child);
    }
    memo.insert(node.index(), new_node);
    new_node
}

/// Serializes `dag` into the sem blob, returning its offset and whether any
/// node carries a type annotation (requiring the typed loader at use site).
fn intern_dag(
    dag: &impl tir_graph::Dag<Node = tir_symbolic::lang::SymKind, Leaf = tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>>,
    root: tir_graph::NodeId,
    widths: &[Option<u32>],
) -> (u32, bool) {
    let mut ops: Vec<tir_symbolic::sem::SemOp> = Vec::new();
    let mut node_indices: HashMap<usize, u32> = HashMap::new();
    let mut has_typed_node = false;
    for (counter, node_id) in dag.postorder(root).enumerate() {
        ops.push(tir_symbolic::sem::SemOp::Node(*dag.get_node(node_id)));

        if let Some(data) = dag.get_leaf_data(node_id) {
            ops.push(tir_symbolic::sem::SemOp::Payload(payload_desc(data)));
        }

        if !matches!(
            dag.get_node(node_id),
            tir_symbolic::lang::SymKind::FAdd
                | tir_symbolic::lang::SymKind::FSub
                | tir_symbolic::lang::SymKind::FMul
                | tir_symbolic::lang::SymKind::FDiv
                | tir_symbolic::lang::SymKind::SIToFP
                | tir_symbolic::lang::SymKind::UIToFP
                | tir_symbolic::lang::SymKind::Bitcast
                | tir_symbolic::lang::SymKind::LoadMemory
                | tir_symbolic::lang::SymKind::LoadReserved
        ) && dag.get_leaf_data(node_id).is_none()
            && let Some(Some(width)) = widths.get(node_id.index()).copied()
        {
            ops.push(tir_symbolic::sem::SemOp::Typed(width));
            has_typed_node = true;
        }

        let children: Vec<tir_graph::NodeId> = dag.children(node_id).collect();
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
    dag: &impl tir_graph::Dag<Node = tir_symbolic::lang::SymKind, Leaf = tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>>,
    root: tir_graph::NodeId,
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

fn payload_desc(payload: &tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>) -> tir_symbolic::sem::SemPayloadDesc {
    use tir_symbolic::sem::SemPayloadDesc;
    use tir_symbolic::lang::SymPayload;
    match payload {
        SymPayload::SymbolId(id) => SemPayloadDesc::SymbolId(*id),
        SymPayload::Value(value) => SemPayloadDesc::Value(value.number()),
        SymPayload::Int(v) => SemPayloadDesc::Int {
            width: v.width(),
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

fn emit_expr_kind_ts(kind: &tir_symbolic::lang::SymKind) -> proc_macro2::TokenStream {
    let variant = tir_symbolic::lang::scalar_op(*kind).map_or_else(
        || format_ident!("{kind:?}"),
        |op| format_ident!("{}", op.rust),
    );
    quote! { tir::sem::SymKind::#variant }
}


// ---------------------------------------------------------------------------
// Instruction encoders
