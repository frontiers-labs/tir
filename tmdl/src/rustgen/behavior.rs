fn emit_as_sem_expr_impl(
    rhs: &ast::Expr,
    name_ident: &proc_macro2::Ident,
    numeric_params: &HashMap<String, i64>,
) -> Option<proc_macro2::TokenStream> {
    let mut dag = tir::sem::SemGraph::new();
    let lowering = rhs.lower_to_sema(&mut dag, numeric_params)?;
    // The AsSemExpr impl carries no type annotations (the program-graph builder
    // infers them), so pass no widths.
    let (stmts, root_var) = emit_dag_as_code(&dag, lowering.root, &[]);

    Some(quote! {
        impl tir::sem::AsSemExpr for #name_ident {
            fn convert(
                &self,
                g: &mut impl tir::graph::MutDag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
            ) -> tir::graph::NodeId {
                #(#stmts)*
                #root_var
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
        ctx.mnemonic,
        ctx.reg_kinds,
    );
    let sym_count_lit = proc_macro2::Literal::usize_unsuffixed(max_sym_id + 1);
    let body = emit_behavior_effect(&behavior, behavior.root, ctx)?;
    Some(quote! {
        {
            let __tmdl_entry_syms: Vec<tir::sem::Value> = {
                let mut __syms: Vec<Option<tir::sem::Value>> = vec![None; #sym_count_lit];
                #(#sym_inits)*
                __syms.into_iter()
                    .map(|value| value.unwrap_or_else(|| tir::sem::int_value(64, 0)))
                    .collect()
            };
            #body
        }
    })
}

struct RustBehaviorCtx<'a> {
    ops: &'a [(String, Type)],
    isa_param_values: &'a HashMap<String, i64>,
    mnemonic: &'a proc_macro2::Literal,
    reg_kinds: &'a HashMap<String, (bool, u32)>,
}

fn emit_behavior_effect(
    behavior: &sem_expr_state::BehaviorGraph,
    effect: tir::graph::NodeId,
    ctx: &RustBehaviorCtx<'_>,
) -> Option<proc_macro2::TokenStream> {
    use tir::graph::Dag as _;

    let children: Vec<_> = behavior.graph.children(effect).collect();
    match behavior.graph.get_node(effect) {
        tir::sem::SymKind::StateAssign => {
            let sem_expr_state::EffectPayload::Assign { destination } =
                behavior.effect_payload(effect)?
            else {
                return None;
            };
            let eval = emit_behavior_value_eval(behavior, *children.first()?, ctx.mnemonic)?;
            let write = emit_graph_destination_write(destination, ctx.ops, ctx.mnemonic)?;
            Some(quote! {{ #eval #write }})
        }
        tir::sem::SymKind::StateStore
        | tir::sem::SymKind::StateStoreConditional
        | tir::sem::SymKind::StateFence => {
            let eval = emit_behavior_value_eval(behavior, *children.first()?, ctx.mnemonic)?;
            Some(quote! {{ #eval let _ = value; }})
        }
        tir::sem::SymKind::StateTrap => {
            let sem_expr_state::EffectPayload::Trap { argument_count, .. } =
                behavior.effect_payload(effect)?
            else {
                return None;
            };
            let cause = *children.get((0..*argument_count).next()?)?;
            let eval = emit_behavior_value_eval(behavior, cause, ctx.mnemonic)?;
            Some(quote! { #eval machine.raise_exception(value.to_u64())?; })
        }
        // The simulator executes the no-trap path. Handler state is modeled by
        // the SMT printer, while machine exception handling owns trap entry.
        tir::sem::SymKind::StateTry => emit_behavior_effect(behavior, *children.first()?, ctx),
        tir::sem::SymKind::StateBlock => {
            let mut steps = Vec::new();
            for effect in children {
                steps.push(emit_behavior_effect(behavior, effect, ctx)?);
            }
            Some(quote! { #(#steps)* })
        }
        tir::sem::SymKind::StateIf => {
            let cond_eval = emit_behavior_value_eval(behavior, *children.first()?, ctx.mnemonic)?;
            let then_body = emit_behavior_effect(behavior, *children.get(1)?, ctx)?;
            // Omit the `else` arm for a guard with no else clause (e.g. a
            // guarded CSR write), so codegen emits no empty `else {}`.
            let else_arm = match children.get(2) {
                Some(else_effect) => {
                    let else_body = emit_behavior_effect(behavior, *else_effect, ctx)?;
                    quote! { else { #else_body } }
                }
                None => quote! {},
            };
            Some(quote! {
                {
                    #cond_eval
                    if value.to_u64() != 0 {
                        #then_body
                    } #else_arm
                }
            })
        }
        tir::sem::SymKind::StateHandler => None,
        _ => None,
    }
}

fn emit_behavior_value_eval(
    behavior: &sem_expr_state::BehaviorGraph,
    root: tir::graph::NodeId,
    mnemonic_lit: &proc_macro2::Literal,
) -> Option<proc_macro2::TokenStream> {
    let (values, root) = behavior.value_graph(root)?;
    emit_lowered_value_eval(&values, root, mnemonic_lit)
}

fn emit_lowered_value_eval(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    root: tir::graph::NodeId,
    mnemonic_lit: &proc_macro2::Literal,
) -> Option<proc_macro2::TokenStream> {
    // Build the semantic graph inline (no type annotations, so no `_context`).
    let (dag_stmts, _root) = emit_dag_as_code(dag, root, &[]);

    Some(quote! {
        let value = {
            let mut __g = tir::sem::SemGraph::new();
            {
                use tir::graph::MutDag as _;
                let g = &mut __g;
                #(#dag_stmts)*
            }
            let __syms = __tmdl_entry_syms.clone();
            struct __TmdlMachineMemory<'a>(&'a mut dyn tir::backend::MachineContext);
            impl tir::sem::Memory for __TmdlMachineMemory<'_> {
                type Error = tir::backend::SimTrap;

                fn read_memory(&mut self, address: u64, size: usize) -> Result<u64, Self::Error> {
                    self.0.read_memory(address, size)
                }

                fn write_memory(
                    &mut self,
                    address: u64,
                    size: usize,
                    value: u64,
                ) -> Result<(), Self::Error> {
                    self.0.write_memory(address, size, value)
                }

                fn load_reserved(
                    &mut self,
                    address: u64,
                    size: usize,
                    ord: tir::sem::MemOrdering,
                ) -> Result<u64, Self::Error> {
                    self.0.load_reserved(address, size, ord)
                }

                fn store_conditional(
                    &mut self,
                    address: u64,
                    size: usize,
                    value: u64,
                    ord: tir::sem::MemOrdering,
                ) -> Result<bool, Self::Error> {
                    self.0.store_conditional(address, size, value, ord)
                }

                fn atomic_rmw(
                    &mut self,
                    op: tir::sem::AtomicRmwOp,
                    address: u64,
                    size: usize,
                    value: u64,
                    ord: tir::sem::MemOrdering,
                ) -> Result<u64, Self::Error> {
                    self.0.atomic_rmw(op, address, size, value, ord)
                }

                fn fence(&mut self, pred: u32, succ: u32, kind: u32) -> Result<(), Self::Error> {
                    self.0.fence(pred, succ, kind)
                }
            }
            let mut __memory = __TmdlMachineMemory(machine);
            match tir::sem::execute_with_memory(&__g, &__syms, &mut __memory)? {
                tir::sem::Value::Int(i) => tir::backend::RegisterValue::Int(i),
                // A float result (e.g. `fadd`) and a lane concatenation (a vector
                // destination) are written back as raw bytes; the destination
                // register's storage keeps the bit pattern.
                tir::sem::Value::Float(f) => {
                    tir::backend::RegisterValue::Bits(tir::utils::RawBits::from_apfloat(&f))
                }
                tir::sem::Value::RawBits(b) => tir::backend::RegisterValue::Bits(b),
                tir::sem::Value::Iterator(_) => {
                    return Err(tir::backend::SimTrap::InvalidInstruction {
                        op: #mnemonic_lit,
                        reason: "instruction semantic expression did not evaluate to a register value".to_string(),
                    });
                }
            }
        };
    })
}

/// Emit the steps that fill `__syms` for a lowered behavior: register operands and
/// fixed/status registers are read from the machine; integer operands and ISA
/// parameters are bound to constants. Returns the highest symbol id (to size the
/// table) and the steps.
fn emit_sym_inits(
    variable_symbols: &HashMap<String, u32>,
    register_symbols: &HashMap<(String, u32), u32>,
    regnum_symbols: &HashMap<String, u32>,
    ops: &[(String, Type)],
    isa_param_values: &HashMap<String, i64>,
    mnemonic_lit: &proc_macro2::Literal,
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

    let mut steps: Vec<proc_macro2::TokenStream> = Vec::new();
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
                    let read = if width > 64 {
                        quote! {
                            tir::sem::value_from_raw_bits(machine.read_register_bits(class.name(), index)?)
                        }
                    } else {
                        quote! {
                            tir::sem::value_from_register(machine.read_register(class.name(), index)?)
                        }
                    };
                    steps.push(quote! {
                        {
                            let (class, index) = tir::backend::register_attr(self.attributes(), #name_lit)
                                .ok_or(tir::backend::SimTrap::MissingAttribute {
                                    op: #mnemonic_lit,
                                    attribute: #name_lit,
                                })?;
                            __syms[#sym_lit] = Some(#read);
                        }
                    });
                }
                Type::Integer => steps.push(quote! {
                    {
                        let value = tir::backend::int_attr(self.attributes(), #name_lit)
                            .ok_or(tir::backend::SimTrap::MissingAttribute {
                                op: #mnemonic_lit,
                                attribute: #name_lit,
                            })?;
                        __syms[#sym_lit] = Some(tir::sem::int_value_signed(64, value));
                    }
                }),
                Type::Bits(width) => {
                    let width_lit = proc_macro2::Literal::u32_unsuffixed(*width as u32);
                    steps.push(quote! {
                        {
                            let value = tir::backend::int_attr(self.attributes(), #name_lit)
                                .ok_or(tir::backend::SimTrap::MissingAttribute {
                                    op: #mnemonic_lit,
                                    attribute: #name_lit,
                                })?;
                            __syms[#sym_lit] = Some(tir::sem::int_value_signed(#width_lit, value));
                        }
                    });
                }
                _ => {}
            }
        } else if let Some(&value) = isa_param_values.get(name) {
            // An ISA parameter (e.g. `XLEN`): resolve it from the machine's
            // selected feature set, falling back to the widest TMDL value for
            // contexts that don't configure ISA params.
            let value_lit = proc_macro2::Literal::i64_unsuffixed(value);
            steps.push(quote! {
                __syms[#sym_lit] = Some(tir::sem::int_value_signed(64, machine.isa_param(#name_lit).unwrap_or(#value_lit)));
            });
        }
    }
    for ((class, number), &sym_id) in register_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let class_lit = proc_macro2::Literal::string(class);
        let number_lit = proc_macro2::Literal::u16_unsuffixed(*number as u16);
        steps.push(quote! {
            __syms[#sym_lit] = Some(tir::sem::value_from_register(machine.read_register(#class_lit, #number_lit)?));
        });
    }

    // `regnum(op)` binds a symbol to the operand's encoding index. The index is
    // an identity, not an arithmetic value; comparisons coerce by value and
    // ignore width, so a plain 64-bit integer holds it.
    for (name, &sym_id) in regnum_symbols {
        let sym_lit = proc_macro2::Literal::usize_unsuffixed(sym_id as usize);
        let name_lit = proc_macro2::Literal::string(name);
        steps.push(quote! {
            {
                let (_, index) = tir::backend::register_attr(self.attributes(), #name_lit)
                    .ok_or(tir::backend::SimTrap::MissingAttribute {
                        op: #mnemonic_lit,
                        attribute: #name_lit,
                    })?;
                __syms[#sym_lit] = Some(tir::sem::int_value(64, index as u64));
            }
        });
    }

    (max_sym_id, steps)
}

fn emit_graph_destination_write(
    dest: &sem_expr_state::Destination,
    ops: &[(String, Type)],
    mnemonic_lit: &proc_macro2::Literal,
) -> Option<proc_macro2::TokenStream> {
    use sem_expr_state::Destination;

    if matches!(dest, Destination::Path { base, members } if base == "PC" && members == &["pc"]) {
        return Some(quote! { machine.write_pc(value.to_u64()); });
    }

    if let Destination::FixedRegister { class, index, .. } = dest {
        let class_lit = proc_macro2::Literal::string(class);
        let index_lit = proc_macro2::Literal::u16_unsuffixed(*index as u16);
        return Some(quote! {
            if !register_has_trait_hardwired_zero(#class_lit, #index_lit) {
                machine.write_register_value(#class_lit, #index_lit, value)?;
            }
        });
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
        return Some(quote! {
            let (dst_class, dst_idx) = tir::backend::register_attr(self.attributes(), #name_lit).ok_or(
                tir::backend::SimTrap::MissingAttribute {
                    op: #mnemonic_lit,
                    attribute: #name_lit,
                },
            )?;
            if !register_has_trait_hardwired_zero(dst_class.name(), dst_idx) {
                machine.write_register_value(dst_class.name(), dst_idx, value)?;
            }
        });
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
    builder_ident: &proc_macro2::Ident,
    mnemonic_name: &str,
    inst_features: &proc_macro2::TokenStream,
    ops: &[(String, Type)],
    pattern: &tir::sem::SemGraph,
    root: tir::graph::NodeId,
    variable_symbols: &HashMap<String, u32>,
    target_operand: &str,
    target_symbol: u32,
    zero_slots: &HashMap<String, (String, u16)>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let emit_fn_ident = format_ident!("emit_isel_{}", rule_name);
    let pattern_fn_ident = format_ident!("isel_pattern_{}", rule_name);
    let rule_name_lit = proc_macro2::Literal::string(rule_name);
    let target_symbol_lit = proc_macro2::Literal::u32_unsuffixed(target_symbol);

    let mut operand_constraint_entries: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut emit_attr_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (op_name, op_ty) in ops {
        let op_name_lit = proc_macro2::Literal::string(op_name);
        if op_name == target_operand {
            emit_attr_steps.push(quote! {
                let dest = m
                    .block_binding(#target_symbol_lit)
                    .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                builder = builder.attr(
                    #op_name_lit,
                    tir::attributes::AttributeValue::Block(dest),
                );
            });
            continue;
        }
        if let Some((class_name, index)) = zero_slots.get(op_name) {
            let class_id = reg_class_id(class_name);
            let index_lit = proc_macro2::Literal::u16_unsuffixed(*index);
            emit_attr_steps.push(quote! {
                builder = builder.attr(
                    #op_name_lit,
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::Physical {
                            class: #class_id,
                            index: #index_lit,
                        },
                    ),
                );
            });
            continue;
        }
        let Some(&symbol) = variable_symbols.get(op_name) else {
            continue;
        };
        let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
        match op_ty {
            Type::Struct(class_name) => {
                let class_id = reg_class_id(class_name);
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Register) });
                emit_attr_steps.push(quote! {
                    let src = m
                        .value_binding(#symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                    builder = builder.attr(
                        #op_name_lit,
                        tir::attributes::AttributeValue::Register(
                            tir::attributes::RegisterAttr::Virtual {
                                id: src.number(),
                                class: Some(#class_id),
                            },
                        ),
                    );
                });
            }
            Type::Integer | Type::Bits(_) => {
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Immediate) });
                emit_attr_steps.push(quote! {
                    let v = m
                        .int_binding(#symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                    builder = builder.attr(
                        #op_name_lit,
                        tir::attributes::AttributeValue::Int(v),
                    );
                });
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
        tir::sem::canonicalize_for_selection(pattern, root, &immediate_symbols);
    let mut pattern_widths = tir::sem::infer_widths(&canon_pattern, |_| None);
    for (index, forced) in forced_widths.iter().enumerate() {
        if forced.is_some() {
            pattern_widths[index] = *forced;
        }
    }
    let (pattern_stmts, _root_var) = emit_dag_as_code(&canon_pattern, canon_root, &pattern_widths);
    let operand_register_call = emit_operand_register_call(
        ops,
        variable_symbols,
        &width_sensitive_symbols(&canon_pattern, &pattern_widths),
        float_classes,
        polymorphic_classes,
    );
    let operand_imm_range_call =
        emit_operand_imm_range_call(&immediate_operand_ranges(pattern, ops, variable_symbols));
    let base_cost = {
        use tir::graph::Dag;
        (canon_pattern.len() as u32).max(1)
    };
    let base_cost_lit = proc_macro2::Literal::u32_unsuffixed(base_cost);
    let mnemonic_cost_lit = proc_macro2::Literal::string(mnemonic_name);

    let emitter = quote! {
        fn #pattern_fn_ident(_context: &tir::Context) -> tir::sem::SemGraph {
            use tir::graph::MutDag;
            let mut g = tir::sem::SemGraph::new();
            #(#pattern_stmts)*
            g
        }

        fn #emit_fn_ident(
            context: &tir::Context,
            req: &tir::backend::isel::EmitRequest,
            m: &tir::backend::isel::RuleMatch,
        ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
            let _ = (req, m);
            let mut builder = #builder_ident::new(context);
            #(#emit_attr_steps)*
            Ok(Box::new(builder.build()))
        }
    };

    let init = quote! {
        if features_enabled(features, #inst_features) {
            rules.push(
                tir::backend::isel::Rule::new(
                    #rule_name_lit,
                    #pattern_fn_ident(context),
                    (#base_cost_lit).max(instruction_cost(#mnemonic_cost_lit)),
                    #emit_fn_ident,
                )
                .with_kind(tir::backend::isel::RuleKind::CondBranch {
                    target_symbol: #target_symbol_lit,
                })
                .with_operand_constraints(vec![#(#operand_constraint_entries),*])
                #operand_register_call
                #operand_imm_range_call
                ,
            );
        }
    };
    (emitter, init)
}

/// Clone `pattern` with the register-operand symbol `reg_symbol` replaced by the
/// `zext(0b0, W)` zero shape. `width_symbol` is the fresh wildcard the extension
/// width binds to — matched but never read by the emitter.
fn branch_pattern_with_zero(
    pattern: &tir::sem::SemGraph,
    root: tir::graph::NodeId,
    reg_symbol: u32,
    width_symbol: u32,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    let mut out = tir::sem::SemGraph::new();
    let mut memo: HashMap<usize, tir::graph::NodeId> = HashMap::new();
    let new_root =
        clone_pattern_with_zero(pattern, root, reg_symbol, width_symbol, &mut out, &mut memo);
    (out, new_root)
}

fn clone_pattern_with_zero(
    pattern: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    reg_symbol: u32,
    width_symbol: u32,
    out: &mut tir::sem::SemGraph,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&existing) = memo.get(&node.index()) {
        return existing;
    }
    if *pattern.get_node(node) == tir::sem::SymKind::Symbol
        && matches!(
            pattern.get_leaf_data(node),
            Some(tir::sem::SymPayload::SymbolId(s)) if *s == reg_symbol
        )
    {
        let zero = out.add_node(tir::sem::SymKind::Constant);
        out.set_leaf_data(zero, tir::sem::int_payload(1, 0, false));
        let width = out.add_node(tir::sem::SymKind::Symbol);
        out.set_leaf_data(width, tir::sem::SymPayload::SymbolId(width_symbol));
        let zext = out.add_node(tir::sem::SymKind::ZExt);
        out.add_edge(zext, zero);
        out.add_edge(zext, width);
        memo.insert(node.index(), zext);
        return zext;
    }
    // Children first: the store keeps strict post-order (a child's index must
    // precede its parent's).
    let kind = *pattern.get_node(node);
    let new_children: Vec<tir::graph::NodeId> = pattern
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

fn emit_dag_as_code(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    root: tir::graph::NodeId,
    widths: &[Option<u32>],
) -> (Vec<proc_macro2::TokenStream>, proc_macro2::Ident) {
    let mut stmts: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut node_vars: HashMap<usize, proc_macro2::Ident> = HashMap::new();
    let mut has_typed_node = false;
    for (counter, node_id) in dag.postorder(root).enumerate() {
        let var = format_ident!("__sem_{}", counter);

        let kind_ts = emit_expr_kind_ts(dag.get_node(node_id));
        stmts.push(quote! { let #var = g.add_node(#kind_ts); });

        if let Some(data) = dag.get_leaf_data(node_id) {
            let data_ts = emit_expr_payload_ts(data);
            stmts.push(quote! { g.set_leaf_data(#var, #data_ts); });
        }

        if !matches!(
            dag.get_node(node_id),
            tir::sem::SymKind::FAdd
                | tir::sem::SymKind::FSub
                | tir::sem::SymKind::FMul
                | tir::sem::SymKind::FDiv
                | tir::sem::SymKind::SIToFP
                | tir::sem::SymKind::Bitcast
                | tir::sem::SymKind::LoadMemory
                | tir::sem::SymKind::LoadReserved
        ) && dag.get_leaf_data(node_id).is_none()
            && let Some(Some(width)) = widths.get(node_id.index()).copied()
        {
            let width_lit = proc_macro2::Literal::u32_unsuffixed(width);
            stmts.push(quote! {
                g.set_actual_type(#var, tir::builtin::IntegerType::new(_context, #width_lit));
            });
            has_typed_node = true;
        }

        let children: Vec<tir::graph::NodeId> = dag.children(node_id).collect();
        for child_id in children {
            let child_var = node_vars[&child_id.index()].clone();
            stmts.push(quote! { g.add_edge(#var, #child_var); });
        }

        node_vars.insert(node_id.index(), var);
    }

    if has_typed_node {
        stmts.insert(0, quote! { use tir::graph::MetaMutDag as _; });
    }

    let root_var = node_vars[&root.index()].clone();
    (stmts, root_var)
}

fn emit_expr_kind_ts(kind: &tir::sem::SymKind) -> proc_macro2::TokenStream {
    let variant = tir::sem::scalar_op(*kind).map_or_else(
        || format_ident!("{kind:?}"),
        |op| format_ident!("{}", op.rust),
    );
    quote! { tir::sem::SymKind::#variant }
}

fn emit_expr_payload_ts(payload: &tir::sem::SymPayload<tir::ValueId>) -> proc_macro2::TokenStream {
    use tir::sem::SymPayload;
    match payload {
        SymPayload::SymbolId(id) => {
            let id_lit = proc_macro2::Literal::u32_unsuffixed(*id);
            quote! { tir::sem::SymPayload::SymbolId(#id_lit) }
        }
        SymPayload::Value(value) => {
            let value_lit = proc_macro2::Literal::u32_unsuffixed(value.number());
            quote! { tir::sem::SymPayload::Value(tir::ValueId::from_number(#value_lit)) }
        }
        SymPayload::Int(v) => {
            let width = proc_macro2::Literal::u32_unsuffixed(v.width());
            if v.is_signed() {
                let val = proc_macro2::Literal::u64_unsuffixed(v.to_i64() as u64);
                quote! { tir::sem::int_payload(#width, #val, true) }
            } else {
                let val = proc_macro2::Literal::u64_unsuffixed(v.to_u64());
                quote! { tir::sem::int_payload(#width, #val, false) }
            }
        }
        SymPayload::Float(f) => {
            let val = proc_macro2::Literal::f64_unsuffixed(f.to_f64());
            quote! { tir::sem::float_payload(#val) }
        }
    }
}

// ---------------------------------------------------------------------------
// Instruction encoders
