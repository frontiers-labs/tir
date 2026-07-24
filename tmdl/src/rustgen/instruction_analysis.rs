fn analyze_instruction_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<InstructionSemantics> {
    let rhs = resolve_behavior_rhs(inst, operands, defined_register_operands)?;
    let mut pattern = tir::sem::SemGraph::new();
    let lowering = rhs.lower_to_sema_with_isa(
        &mut pattern,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    let fixed_register_by_class = split_fixed_registers(&lowering.register_symbols);

    let guarded_semantics = defined_register_operands.first().and_then(|dst| {
        analyze_guarded_semantics(
            inst,
            dst,
            numeric_params,
            isa_param_values,
            register_index_map,
        )
    });

    Some(InstructionSemantics {
        pattern,
        root: lowering.root,
        variable_symbols: lowering.variable_symbols,
        fixed_register_by_class,
        register_symbols: lowering.register_symbols,
        guarded_semantics,
    })
}

/// The destination's full guarded semantics `If(cond, then, else)` when the
/// behavior is a statement-level `if cond { dst = t } else { dst = e }`. The else
/// arm is lowered first, so its operand symbol ids match the guard-relaxed
/// selection pattern (which lowers the else arm alone) — a prerequisite for the
/// pass-construction relaxation proof to share the pattern's op node.
fn analyze_guarded_semantics(
    inst: &ast::Instruction,
    dst: &str,
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<(tir::sem::SemGraph, tir::graph::NodeId)> {
    use tir::graph::MutDag;
    let (cond, then_value, else_value) = guarded_assignment_shape(&inst.behavior, dst)?;
    // Resolve `self.XLEN` and friends to their concrete per-ISA width (the value
    // `execute()` uses, e.g. 64 for RV32+RV64), so the guarded semantics is a
    // width-concrete graph the relaxation proof can bit-blast — patterns keep it
    // symbolic, but this companion exists only to be proved.
    let mut concrete_params = numeric_params.clone();
    for (name, value) in isa_param_values {
        concrete_params.entry(name.clone()).or_insert(*value);
    }
    let mut graph = tir::sem::SemGraph::new();
    let (roots, _) = ast::Expr::lower_all_to_sema_with_isa(
        &[else_value, cond, then_value],
        &mut graph,
        &concrete_params,
        isa_param_values,
        register_index_map,
    )?;
    let [else_root, cond_root, then_root] = roots.as_slice() else {
        return None;
    };
    let if_node = graph.add_node(tir::sem::SymKind::If);
    graph.add_edge(if_node, *cond_root);
    graph.add_edge(if_node, *then_root);
    graph.add_edge(if_node, *else_root);
    Some((graph, if_node))
}

/// Match `if cond { dst = then } else { dst = else }`, returning the condition and
/// the two arm values. `None` for any other shape (including a single `dst = if …`
/// assignment, whose value is an `If` expression, not a statement guard).
fn guarded_assignment_shape<'a>(
    behavior: &'a ast::Expr,
    dst: &str,
) -> Option<(&'a ast::Expr, &'a ast::Expr, &'a ast::Expr)> {
    let ast::Expr::If(if_expr) = unwrap_single_stmt(behavior) else {
        return None;
    };
    let else_arm = if_expr.else_.as_deref()?;
    let then_value = single_assignment_value(&if_expr.then, dst)?;
    let else_value = single_assignment_value(else_arm, dst)?;
    Some((&if_expr.cond, then_value, else_value))
}

/// Unwrap a block holding a single statement to that statement; otherwise the
/// expression itself.
fn unwrap_single_stmt(expr: &ast::Expr) -> &ast::Expr {
    match expr {
        ast::Expr::Block(b) if b.stmts.len() == 1 => &b.stmts[0],
        other => other,
    }
}

/// The value of a lone `dst = value` assignment inside `expr` (a block arm or a
/// bare assignment).
fn single_assignment_value<'a>(expr: &'a ast::Expr, dst: &str) -> Option<&'a ast::Expr> {
    match unwrap_single_stmt(expr) {
        ast::Expr::Assign(a) if assignment_dest_name(&a.dest).as_deref() == Some(dst) => {
            Some(&a.value)
        }
        _ => None,
    }
}

fn split_fixed_registers(symbols: &HashMap<(String, u32), u32>) -> HashMap<String, Option<u16>> {
    let mut fixed_register_by_class: HashMap<String, Option<u16>> = HashMap::new();

    for (class, number) in symbols.keys() {
        let entry = fixed_register_by_class.entry(class.clone()).or_insert(None);
        if let Ok(number_u16) = u16::try_from(*number) {
            match entry {
                None => *entry = Some(number_u16),
                Some(existing) if *existing == number_u16 => {}
                Some(_) => *entry = None,
            }
        } else {
            *entry = None;
        }
    }

    fixed_register_by_class
}

fn register_operand_names(operands: &[(String, Type)]) -> HashSet<&str> {
    operands
        .iter()
        .filter_map(|(name, ty)| match ty {
            Type::Struct(_) => Some(name.as_str()),
            _ => None,
        })
        .collect()
}

fn assignment_dest_name(dest: &ast::Expr) -> Option<String> {
    match dest {
        ast::Expr::Ident(id) => Some(id.name.clone()),
        ast::Expr::Path(path) if path.remainder.len() == 1 => Some(path.remainder[0].clone()),
        _ => None,
    }
}

/// `(class, register-name)` when an assignment destination is a register path
/// (e.g. `VCSR::vl`), or `None` for a plain identifier (an encoded operand).
fn assignment_dest_register_path(dest: &ast::Expr) -> Option<(String, String)> {
    match dest {
        ast::Expr::Path(path) if path.remainder.len() == 1 => {
            Some((path.base.clone(), path.remainder[0].clone()))
        }
        _ => None,
    }
}

/// The operand names referenced anywhere in `expr`, in first-seen order. Used to
/// find which operand feeds a register a definer instruction writes.
fn referenced_operands(expr: &ast::Expr, operands: &HashSet<&str>) -> Vec<String> {
    let mut out = Vec::new();
    collect_referenced_idents(expr, operands, &mut out);
    out
}

fn collect_referenced_idents(expr: &ast::Expr, operands: &HashSet<&str>, out: &mut Vec<String>) {
    match expr {
        ast::Expr::Ident(id) => {
            if operands.contains(id.name.as_str()) && !out.iter().any(|n| n == &id.name) {
                out.push(id.name.clone());
            }
        }
        ast::Expr::Lit(_)
        | ast::Expr::Path(_)
        | ast::Expr::BuiltinFunction(_)
        | ast::Expr::Invalid => {}
        ast::Expr::Assign(a) => {
            collect_referenced_idents(&a.dest, operands, out);
            collect_referenced_idents(&a.value, operands, out);
        }
        ast::Expr::Binary(b) => {
            collect_referenced_idents(&b.lhs, operands, out);
            collect_referenced_idents(&b.rhs, operands, out);
        }
        ast::Expr::Unary(u) => collect_referenced_idents(&u.x, operands, out),
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_referenced_idents(stmt, operands, out);
            }
        }
        ast::Expr::Call(c) => {
            collect_referenced_idents(&c.callee, operands, out);
            for arg in &c.arguments {
                collect_referenced_idents(arg, operands, out);
            }
        }
        ast::Expr::Field(f) => collect_referenced_idents(&f.base, operands, out),
        ast::Expr::If(i) => {
            collect_referenced_idents(&i.cond, operands, out);
            collect_referenced_idents(&i.then, operands, out);
            if let Some(e) = &i.else_ {
                collect_referenced_idents(e, operands, out);
            }
        }
        ast::Expr::IndexAccess(i) => collect_referenced_idents(&i.base, operands, out),
        ast::Expr::Slice(s) => collect_referenced_idents(&s.base, operands, out),
        ast::Expr::Try(t) => {
            collect_referenced_idents(&t.body, operands, out);
            for h in &t.handlers {
                collect_referenced_idents(&h.body, operands, out);
            }
        }
        ast::Expr::Lambda(l) => collect_referenced_idents(&l.body, operands, out),
    }
}

/// The operands of a value rule's zero-form constant materializer, when one can
/// be derived: the canonical pattern must be `Add(reg, imm)` over two bare
/// operand symbols, with the source register in a class `zeroable_class`
/// accepts (an integer class with a hardwired-zero register) and every other
/// operand accounted for as the `rd_name` destination or the folded immediate.
/// The caller guarantees `rd_name` is the sole defined register operand, unread
/// and untied, with no implicit register reads.
/// Returns `(source register operand name, its class, immediate symbol)`.
fn value_zero_form_operands(
    canon_pattern: &impl tir::graph::Dag<
        Node = tir::sem::SymKind,
        Leaf = tir::sem::SymPayload<tir::ValueId>,
    >,
    canon_root: tir::graph::NodeId,
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
    rd_name: &str,
    zeroable_class: impl Fn(&str) -> bool,
) -> Option<(String, String, u32)> {
    use tir::sem::{SymKind, SymPayload};

    if *canon_pattern.get_node(canon_root) != SymKind::Add {
        return None;
    }
    let children: Vec<tir::graph::NodeId> = canon_pattern.children(canon_root).collect();
    if children.len() != 2 {
        return None;
    }
    let symbol_of = |node: tir::graph::NodeId| {
        (*canon_pattern.get_node(node) == SymKind::Symbol)
            .then(|| match canon_pattern.get_leaf_data(node) {
                Some(SymPayload::SymbolId(s)) => Some(*s),
                _ => None,
            })
            .flatten()
    };

    let mut source = None;
    let mut imm_sym = None;
    for &child in &children {
        let sym = symbol_of(child)?;
        let operand = ops
            .iter()
            .find(|(name, _)| variable_symbols.get(name) == Some(&sym))?;
        match &operand.1 {
            Type::Struct(class) if zeroable_class(class) => {
                source = Some((operand.0.clone(), class.clone()));
            }
            Type::Bits(_) | Type::Integer => imm_sym = Some(sym),
            _ => return None,
        }
    }
    let (source_name, source_class) = source?;
    let imm_sym = imm_sym?;

    // Every operand must be the destination, the zeroed source, or the folded
    // immediate — anything else would go unbound in the derived emitter.
    let accounted = ops.iter().all(|(name, ty)| match ty {
        Type::Struct(_) => name == rd_name || *name == source_name,
        Type::Bits(_) | Type::Integer => variable_symbols.get(name) == Some(&imm_sym),
        Type::String => true,
        _ => false,
    });
    accounted.then_some((source_name, source_class, imm_sym))
}

/// The boundary symbols an instruction is width-sensitive in: the operands'
/// upper register bits reach the result, so a value of a different width must
/// not bind (its bits above the value width are undefined). Comparison
/// operands always qualify — the comparison node's own type is its i1 result
/// and says nothing about operand widths. Right-shift values and
/// division/remainder operands qualify only under an *untyped* node: a typed
/// node (a word form like `sraw`) already pins its operands through width
/// inference. Low-bits-preserving operators (add/and/shl/mul low half) are
/// exempt: a narrower value's upper garbage never reaches its own low bits.
fn width_sensitive_symbols(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    node_widths: &[Option<u32>],
) -> HashSet<u32> {
    use tir::sem::SymKind as K;

    let mut out = HashSet::new();
    for index in 0..dag.len() {
        let node = tir::graph::NodeId::from_index(index);
        let untyped = node_widths.get(index).copied().flatten().is_none();
        let sensitive_children: &[usize] = match dag.get_node(node) {
            K::Eq | K::Ne | K::Lt | K::Le | K::Gt | K::Ge | K::ULt | K::ULe | K::UGt | K::UGe => {
                &[0, 1]
            }
            K::Div | K::UDiv | K::SRem | K::URem if untyped => &[0, 1],
            K::ShiftRightLogic | K::ShiftRightArithmetic if untyped => &[0],
            _ => &[],
        };
        let children: Vec<tir::graph::NodeId> = dag.children(node).collect();
        for &slot in sensitive_children {
            if let Some(child) = children.get(slot)
                && let Some(tir::sem::SymPayload::SymbolId(symbol)) = dag.get_leaf_data(*child)
            {
                out.insert(*symbol);
            }
        }
    }
    out
}

/// Emit each register operand's storage domain and whether its instruction
/// consumes the full architectural width.
fn emit_operand_register_call(
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
    sensitive_symbols: &HashSet<u32>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> proc_macro2::TokenStream {
    let register_steps: Vec<proc_macro2::TokenStream> = ops
        .iter()
        .filter_map(|(op_name, op_ty)| {
            let Type::Struct(class_name) = op_ty else {
                return None;
            };
            let &symbol = variable_symbols.get(op_name)?;
            let class_lit = proc_macro2::Literal::string(class_name);
            let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
            let capability = if polymorphic_classes.contains(class_name) {
                quote! { tir::backend::isel::RegisterCapability::any(*width) }
            } else if float_classes.contains(class_name) {
                quote! { tir::backend::isel::RegisterCapability::float(*width) }
            } else {
                quote! { tir::backend::isel::RegisterCapability::integer(*width) }
            };
            let requirement = if sensitive_symbols.contains(&symbol) {
                quote! { tir::backend::isel::RegisterRequirement::whole(#capability) }
            } else {
                quote! { tir::backend::isel::RegisterRequirement::low_bits(#capability) }
            };
            Some(quote! {
                if let Some((_, width)) =
                    __register_widths.iter().find(|(class, _)| *class == #class_lit)
                {
                    __operand_registers.push((#symbol_lit, #requirement));
                }
            })
        })
        .collect();

    if register_steps.is_empty() {
        return quote! {};
    }
    quote! {
        .with_operand_registers({
            let mut __operand_registers = Vec::new();
            #(#register_steps)*
            __operand_registers
        })
    }
}

fn emit_result_register_call(
    class_name: Option<&str>,
    float_classes: &HashSet<String>,
    polymorphic_classes: &HashSet<String>,
) -> proc_macro2::TokenStream {
    let Some(class_name) = class_name else {
        return quote! {};
    };
    let class_lit = proc_macro2::Literal::string(class_name);
    let capability = if polymorphic_classes.contains(class_name) {
        quote! { tir::backend::isel::RegisterCapability::any(*width) }
    } else if float_classes.contains(class_name) {
        quote! { tir::backend::isel::RegisterCapability::float(*width) }
    } else {
        quote! { tir::backend::isel::RegisterCapability::integer(*width) }
    };
    quote! {
        .with_optional_result_register(
            __register_widths
                .iter()
                .find(|(class, _)| *class == #class_lit)
                .map(|(_, width)| tir::backend::isel::RegisterRequirement::low_bits(#capability))
        )
    }
}

/// The encoding range of each immediate operand: the field's bit width from the
/// operand type, signedness from how the behavior consumes the symbol —
/// `sext(imm, _)` sign-extends, everything else is unsigned — and an
/// `extract(imm, hi, 0)` wrapper (a shift-amount mask) narrows the usable bits.
/// Selection uses these to refuse constants the field cannot represent.
fn immediate_operand_ranges(
    dag: &impl tir::graph::Dag<Node = tir::sem::SymKind, Leaf = tir::sem::SymPayload<tir::ValueId>>,
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
) -> Vec<(u32, u32, bool)> {
    use tir::sem::{SymKind as K, SymPayload};

    let is_symbol_leaf = |node: tir::graph::NodeId, symbol: u32| {
        *dag.get_node(node) == K::Symbol
            && matches!(
                dag.get_leaf_data(node),
                Some(SymPayload::SymbolId(id)) if *id == symbol
            )
    };
    let const_value = |node: tir::graph::NodeId| match dag.get_leaf_data(node) {
        Some(SymPayload::Int(v)) => Some(v.to_u64()),
        _ => None,
    };

    let mut out = Vec::new();
    for (op_name, op_ty) in ops {
        let Type::Bits(bits) = op_ty else { continue };
        let Some(&symbol) = variable_symbols.get(op_name) else {
            continue;
        };
        let mut signed = false;
        let mut width = u32::from(*bits);
        for index in 0..dag.len() {
            let node = tir::graph::NodeId::from_index(index);
            let children: Vec<tir::graph::NodeId> = dag.children(node).collect();
            let uses_symbol = children
                .first()
                .is_some_and(|&child| is_symbol_leaf(child, symbol));
            if !uses_symbol {
                continue;
            }
            match dag.get_node(node) {
                K::SExt => signed = true,
                K::Extract
                    if children.len() == 3
                        && children.get(2).and_then(|&c| const_value(c)) == Some(0) =>
                {
                    if let Some(hi) = children.get(1).and_then(|&c| const_value(c)) {
                        width = width.min(hi as u32 + 1);
                    }
                }
                _ => {}
            }
        }
        out.push((symbol, width, signed));
    }
    out
}

/// Emit the `.with_operand_imm_ranges` builder call for the immediate operands'
/// encoding ranges.
fn emit_operand_imm_range_call(ranges: &[(u32, u32, bool)]) -> proc_macro2::TokenStream {
    if ranges.is_empty() {
        return quote! {};
    }
    let entries: Vec<proc_macro2::TokenStream> = ranges
        .iter()
        .map(|(symbol, width, signed)| {
            let symbol_lit = proc_macro2::Literal::u32_unsuffixed(*symbol);
            let width_lit = proc_macro2::Literal::u32_unsuffixed(*width);
            quote! {
                (#symbol_lit, tir::backend::isel::ImmRange { width: #width_lit, signed: #signed })
            }
        })
        .collect();
    quote! { .with_operand_imm_ranges(vec![#(#entries),*]) }
}

/// The literal architectural width of a register class, when its `WIDTH` param
/// is a compile-time literal (x86 `GPR32`/`GPR16`/`GPR8`). A class sized by an
/// ISA parameter (`self.XLEN`) resolves only under the enabled features and
/// yields `None`.
fn literal_register_class_width(files: &[ast::File], class_name: &str) -> Option<u32> {
    files
        .iter()
        .flat_map(|f| f.register_classes())
        .find(|rc| rc.name == class_name)?
        .parameters
        .get("WIDTH")
        .and_then(|(_ty, value)| match value {
            Some(ast::Expr::Lit(ast::Lit::Int(li))) => Some(parse_literal_value(li) as u32),
            _ => None,
        })
}

/// Operator kinds whose result is meaningfully sized by the destination register
/// width — scalar integer and float computations. Vector, memory, and control
/// kinds carry no scalar width and are never typed from a register class.
fn scalar_root_kind(kind: &tir::sem::SymKind) -> bool {
    use tir::sem::SymKind as K;
    matches!(
        kind,
        K::Add
            | K::Sub
            | K::Mul
            | K::Div
            | K::UDiv
            | K::SRem
            | K::URem
            | K::Neg
            | K::And
            | K::Or
            | K::Xor
            | K::Not
            | K::ShiftLeft
            | K::ShiftRightLogic
            | K::ShiftRightArithmetic
            | K::FAdd
            | K::FSub
            | K::FMul
            | K::FDiv
    )
}

/// Whether `expr` reads or writes a program-counter register (`PC::pc`).
fn behavior_references_pc(expr: &ast::Expr, pc_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Path(path) => pc_classes.contains(&path.base),
        ast::Expr::Ident(_) | ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) => false,
        ast::Expr::Invalid => false,
        ast::Expr::Assign(a) => {
            behavior_references_pc(&a.dest, pc_classes)
                || behavior_references_pc(&a.value, pc_classes)
        }
        ast::Expr::Binary(b) => {
            behavior_references_pc(&b.lhs, pc_classes) || behavior_references_pc(&b.rhs, pc_classes)
        }
        ast::Expr::Unary(u) => behavior_references_pc(&u.x, pc_classes),
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| behavior_references_pc(stmt, pc_classes)),
        ast::Expr::Call(c) => {
            behavior_references_pc(&c.callee, pc_classes)
                || c.arguments
                    .iter()
                    .any(|arg| behavior_references_pc(arg, pc_classes))
        }
        ast::Expr::Field(f) => behavior_references_pc(&f.base, pc_classes),
        ast::Expr::If(i) => {
            behavior_references_pc(&i.cond, pc_classes)
                || behavior_references_pc(&i.then, pc_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| behavior_references_pc(e, pc_classes))
        }
        ast::Expr::IndexAccess(i) => behavior_references_pc(&i.base, pc_classes),
        ast::Expr::Slice(s) => behavior_references_pc(&s.base, pc_classes),
        ast::Expr::Try(t) => {
            behavior_references_pc(&t.body, pc_classes)
                || t.handlers
                    .iter()
                    .any(|h| behavior_references_pc(&h.body, pc_classes))
        }
        ast::Expr::Lambda(l) => behavior_references_pc(&l.body, pc_classes),
    }
}

/// Whether a behavior *reads* a status-flag register (a `flag_classes` register
/// path in a value position). Such readers (`cset`, `csel`) compute from
/// condition-code bits a plain value rule cannot see: lifting the flag reads
/// into free symbolic operands yields a pattern structurally identical to an
/// integer comparison, so it would match `cmpi` and drop the operand bindings.
/// They instead materialize through composed definer+reader rules (see
/// `emit_flag_reader_rules`). A flag-path assignment *destination* is a write,
/// not a read, so definers (`cmp`) are not caught.
fn behavior_reads_flag_register(expr: &ast::Expr, flag_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Path(path) => flag_classes.contains(&path.base),
        ast::Expr::Ident(_) | ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) => false,
        ast::Expr::Invalid => false,
        ast::Expr::Assign(a) => {
            let dest_is_flag_write =
                matches!(&*a.dest, ast::Expr::Path(p) if flag_classes.contains(&p.base));
            behavior_reads_flag_register(&a.value, flag_classes)
                || (!dest_is_flag_write && behavior_reads_flag_register(&a.dest, flag_classes))
        }
        ast::Expr::Binary(b) => {
            behavior_reads_flag_register(&b.lhs, flag_classes)
                || behavior_reads_flag_register(&b.rhs, flag_classes)
        }
        ast::Expr::Unary(u) => behavior_reads_flag_register(&u.x, flag_classes),
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| behavior_reads_flag_register(stmt, flag_classes)),
        ast::Expr::Call(c) => {
            behavior_reads_flag_register(&c.callee, flag_classes)
                || c.arguments
                    .iter()
                    .any(|arg| behavior_reads_flag_register(arg, flag_classes))
        }
        ast::Expr::Field(f) => behavior_reads_flag_register(&f.base, flag_classes),
        ast::Expr::If(i) => {
            behavior_reads_flag_register(&i.cond, flag_classes)
                || behavior_reads_flag_register(&i.then, flag_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| behavior_reads_flag_register(e, flag_classes))
        }
        ast::Expr::IndexAccess(i) => behavior_reads_flag_register(&i.base, flag_classes),
        ast::Expr::Slice(s) => behavior_reads_flag_register(&s.base, flag_classes),
        ast::Expr::Try(t) => {
            behavior_reads_flag_register(&t.body, flag_classes)
                || t.handlers
                    .iter()
                    .any(|h| behavior_reads_flag_register(&h.body, flag_classes))
        }
        ast::Expr::Lambda(l) => behavior_reads_flag_register(&l.body, flag_classes),
    }
}

/// Whether a behavior invokes the `todo()` builtin anywhere: its semantics are
/// unmodeled, so it generates no selection rules and its `execute()` traps.
fn behavior_uses_todo(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::BuiltinFunction(ast::BuiltinFunction::Todo) => true,
        ast::Expr::Ident(_) | ast::Expr::Lit(_) | ast::Expr::BuiltinFunction(_) => false,
        ast::Expr::Path(_) | ast::Expr::Invalid => false,
        ast::Expr::Assign(a) => behavior_uses_todo(&a.dest) || behavior_uses_todo(&a.value),
        ast::Expr::Binary(b) => behavior_uses_todo(&b.lhs) || behavior_uses_todo(&b.rhs),
        ast::Expr::Unary(u) => behavior_uses_todo(&u.x),
        ast::Expr::Block(b) => b.stmts.iter().any(behavior_uses_todo),
        ast::Expr::Call(c) => {
            behavior_uses_todo(&c.callee) || c.arguments.iter().any(behavior_uses_todo)
        }
        ast::Expr::Field(f) => behavior_uses_todo(&f.base),
        ast::Expr::If(i) => {
            behavior_uses_todo(&i.cond)
                || behavior_uses_todo(&i.then)
                || i.else_.as_ref().is_some_and(|e| behavior_uses_todo(e))
        }
        ast::Expr::IndexAccess(i) => behavior_uses_todo(&i.base),
        ast::Expr::Slice(s) => behavior_uses_todo(&s.base),
        ast::Expr::Try(t) => {
            behavior_uses_todo(&t.body) || t.handlers.iter().any(|h| behavior_uses_todo(&h.body))
        }
        ast::Expr::Lambda(l) => behavior_uses_todo(&l.body),
    }
}

fn collect_behavior_assignments<'a>(expr: &'a ast::Expr, out: &mut Vec<(String, &'a ast::Expr)>) {
    match expr {
        ast::Expr::Assign(a) => {
            if let Some(dst) = assignment_dest_name(&a.dest) {
                out.push((dst, a.value.as_ref()));
            }
        }
        ast::Expr::Block(b) => {
            for stmt in &b.stmts {
                collect_behavior_assignments(stmt, out);
            }
        }
        ast::Expr::If(i) => {
            collect_behavior_assignments(i.then.as_ref(), out);
            if let Some(else_expr) = &i.else_ {
                collect_behavior_assignments(else_expr.as_ref(), out);
            }
        }
        // Only the no-trap path defines values; handler writes are trap state.
        ast::Expr::Try(t) => collect_behavior_assignments(&t.body, out),
        _ => {}
    }
}

/// Register operands the behavior *reads*: referenced anywhere outside an
/// assignment-destination position. An operand that is also defined is a tied
/// (two-address) operand, e.g. the x86 `dst = dst + src`.
fn infer_read_register_operands(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
) -> HashSet<String> {
    fn walk(expr: &ast::Expr, operands: &HashSet<&str>, out: &mut Vec<String>) {
        if let ast::Expr::Assign(a) = expr {
            // A plain identifier/path destination is a pure write; any other
            // destination form (e.g. a slice, a partial update) reads its base.
            if assignment_dest_name(&a.dest).is_none() {
                collect_referenced_idents(&a.dest, operands, out);
            }
            walk(&a.value, operands, out);
            return;
        }
        if let ast::Expr::Block(b) = expr {
            for stmt in &b.stmts {
                walk(stmt, operands, out);
            }
            return;
        }
        if let ast::Expr::If(i) = expr {
            collect_referenced_idents(&i.cond, operands, out);
            walk(&i.then, operands, out);
            if let Some(e) = &i.else_ {
                walk(e, operands, out);
            }
            return;
        }
        if let ast::Expr::Try(t) = expr {
            walk(&t.body, operands, out);
            return;
        }
        collect_referenced_idents(expr, operands, out);
    }

    let register_operands = register_operand_names(operands);
    let mut reads = Vec::new();
    walk(behavior, &register_operands, &mut reads);
    reads.into_iter().collect()
}

fn infer_defined_register_operands(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
) -> Vec<String> {
    let register_operands = register_operand_names(operands);

    let mut defs = Vec::new();
    let mut assignments = Vec::new();
    collect_behavior_assignments(behavior, &mut assignments);
    for (dst, _) in assignments {
        if register_operands.contains(dst.as_str()) && !defs.iter().any(|existing| existing == &dst)
        {
            defs.push(dst);
        }
    }
    defs
}

fn resolve_behavior_rhs<'a>(
    inst: &'a ast::Instruction,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
) -> Option<&'a ast::Expr> {
    let register_operands = register_operand_names(operands);

    let mut assignments = Vec::new();
    collect_behavior_assignments(&inst.behavior, &mut assignments);
    for (dst, rhs) in assignments.iter().rev() {
        if defined_register_operands.iter().any(|d| d == dst) {
            return Some(*rhs);
        }
    }
    for (dst, rhs) in assignments.iter().rev() {
        if register_operands.contains(dst.as_str()) {
            return Some(*rhs);
        }
    }
    if let Some(store) = find_store_effect_expr(&inst.behavior) {
        return Some(store);
    }
    match &inst.behavior {
        ast::Expr::Assign(a) => Some(a.value.as_ref()),
        ast::Expr::Block(_) | ast::Expr::If(_) => None,
        other => Some(other),
    }
}

fn find_store_effect_expr(expr: &ast::Expr) -> Option<&ast::Expr> {
    match expr {
        ast::Expr::Call(_) if is_store_call(expr) => Some(expr),
        ast::Expr::Block(b) => b.stmts.iter().find_map(find_store_effect_expr),
        ast::Expr::Try(t) => find_store_effect_expr(&t.body),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Template / asm helpers
// ---------------------------------------------------------------------------

fn resolve_string(expr: &ast::Expr) -> Option<String> {
    match &expr {
        ast::Expr::Lit(ast::Lit::Str(lstr)) => Some(lstr.value().to_owned()),
        ast::Expr::Lit(_) => None,
        ast::Expr::Block(b) => {
            if b.last_expr_return
                && let Some(ast::Expr::Lit(ast::Lit::Str(s))) = b.stmts.last()
            {
                return Some(s.value().to_owned());
            }
            None
        }
        _ => None,
    }
}

fn resolve_asm_template_for_instruction<'a>(
    inst: &'a ast::Instruction,
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Option<String> {
    resolve_effective_asm_for_instruction(inst, item_cache).and_then(resolve_string)
}

