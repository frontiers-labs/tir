/// The register classes holding floating-point registers, and those holding
/// polymorphic ones, by name.
fn register_class_kinds(files: &[ast::File]) -> (HashSet<String>, HashSet<String>) {
    let names = |keep: fn(&ast::RegisterClass) -> bool| {
        files
            .iter()
            .flat_map(|file| file.register_classes())
            .filter(|class| keep(class))
            .map(|class| class.name.clone())
            .collect()
    };
    (
        names(|class| class.has_float_registers()),
        names(|class| class.has_polymorphic_registers()),
    )
}

/// The `MNEMONIC` an instruction resolves to and its `OPNAME`, which defaults
/// to the mnemonic.
fn instruction_names(
    params: &HashMap<String, (Type, Option<ast::Expr>)>,
) -> Option<(String, String)> {
    let string = |name: &str| {
        params
            .get(name)
            .and_then(|(_, value)| value.as_ref())
            .and_then(resolve_string)
    };
    let mnemonic = string("MNEMONIC")?;
    let op_name = string("OPNAME").unwrap_or_else(|| mnemonic.clone());
    Some((mnemonic, op_name))
}

fn analyze_instruction_semantics(
    behavior: &ast::Expr,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<InstructionSemantics> {
    let rhs = resolve_behavior_rhs(behavior, operands, defined_register_operands)?;
    let mut pattern = tir_symbolic::sem::SemGraph::new();
    let lowering = rhs.lower_to_sema_with_isa(
        &mut pattern,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    let fixed_register_by_class = split_fixed_registers(&lowering.register_symbols);

    let guarded_semantics = defined_register_operands.first().and_then(|dst| {
        analyze_guarded_semantics(
            behavior,
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
    behavior: &ast::Expr,
    dst: &str,
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
) -> Option<(tir_symbolic::sem::SemGraph, tir_graph::NodeId)> {
    use tir_graph::MutDag;
    let (cond, then_value, else_value) = guarded_assignment_shape(behavior, dst)?;
    // Resolve `self.XLEN` and friends to their concrete per-ISA width (the value
    // `execute()` uses, e.g. 64 for RV32+RV64), so the guarded semantics is a
    // width-concrete graph the relaxation proof can bit-blast — patterns keep it
    // symbolic, but this companion exists only to be proved.
    let mut concrete_params = numeric_params.clone();
    for (name, value) in isa_param_values {
        concrete_params.entry(name.clone()).or_insert(*value);
    }
    let mut graph = tir_symbolic::sem::SemGraph::new();
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
    let if_node = graph.add_node(tir_symbolic::lang::SymKind::If);
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
        | ast::Expr::Tuple(_)
        | ast::Expr::Invalid => {}
        ast::Expr::Assign(a) => {
            collect_referenced_idents(&a.dest, operands, out);
            collect_referenced_idents(&a.value, operands, out);
        }
        ast::Expr::Let(l) => collect_referenced_idents(&l.value, operands, out),
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
            // `regnum(op)` reads the operand's encoding index, not the value in
            // its register: an instruction that only asks which register an
            // operand names does not read that register.
            if matches!(
                c.callee.as_ref(),
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Regnum)
            ) {
                return;
            }
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
        ast::Expr::Cast(c) => {
            collect_referenced_idents(&c.x, operands, out);
            collect_referenced_idents(&c.width, operands, out);
        }
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
    canon_pattern: &impl tir_graph::Dag<
        Node = tir_symbolic::lang::SymKind,
        Leaf = tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>,
    >,
    canon_root: tir_graph::NodeId,
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
    rd_name: &str,
    zeroable_class: impl Fn(&str) -> bool,
) -> Option<(String, String, u32)> {
    use tir_symbolic::lang::{SymKind, SymPayload};

    if *canon_pattern.get_node(canon_root) != SymKind::Add {
        return None;
    }
    let children: Vec<tir_graph::NodeId> = canon_pattern.children(canon_root).collect();
    if children.len() != 2 {
        return None;
    }
    let symbol_of = |node: tir_graph::NodeId| {
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
/// and says nothing about operand widths. Extension operands always qualify —
/// `sext`/`zext` read the operand up to its *own* width (the sign bit moves
/// with it), so the result type never pins the operand. Right-shift values and
/// division/remainder operands qualify only under an *untyped* node: a typed
/// node (a word form like `sraw`) already pins its operands through width
/// inference.
///
/// Sensitivity reaches *through* low-bits-preserving operators rather than
/// stopping at them: `and`'s own result keeps a narrow operand's garbage out of
/// its low bits, but `(dst & src) == 0` (x86 `test` + `jcc`) still compares
/// every bit of that garbage, so both operands are sensitive. It stops at
/// operators that cap which operand bits the consumer can see: an `extract`
/// reads a fixed bit range regardless of the bound value's width, and a
/// memory read yields fresh bits unrelated to its address operands.
fn width_sensitive_symbols(
    dag: &impl tir_graph::Dag<Node = tir_symbolic::lang::SymKind, Leaf = tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>>,
    node_widths: &[Option<u32>],
) -> HashSet<u32> {
    use tir_symbolic::lang::SymKind as K;

    let mut out = HashSet::new();
    for index in 0..dag.len() {
        let node = tir_graph::NodeId::from_index(index);
        let untyped = node_widths.get(index).copied().flatten().is_none();
        let sensitive_children: &[usize] = match dag.get_node(node) {
            K::Eq | K::Ne | K::Lt | K::Le | K::Gt | K::Ge | K::ULt | K::ULe | K::UGt | K::UGe => {
                &[0, 1]
            }
            K::Div | K::UDiv | K::SRem | K::URem if untyped => &[0, 1],
            K::ShiftRightLogic | K::ShiftRightArithmetic if untyped => &[0],
            K::SExt | K::ZExt => &[0],
            _ => &[],
        };
        let children: Vec<tir_graph::NodeId> = dag.children(node).collect();
        for &slot in sensitive_children {
            if let Some(child) = children.get(slot) {
                collect_symbols(dag, *child, &mut out);
            }
        }
    }
    out
}

/// Every operand symbol whose upper bits reach `node`'s value. Stops at
/// operators that cap the visible bit range (`extract`) or yield fresh bits
/// (memory reads) — symbols below them are not width-sensitive through this
/// path (see [`width_sensitive_symbols`]).
fn collect_symbols(
    dag: &impl tir_graph::Dag<Node = tir_symbolic::lang::SymKind, Leaf = tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>>,
    node: tir_graph::NodeId,
    out: &mut HashSet<u32>,
) {
    use tir_symbolic::lang::SymKind as K;

    if let Some(tir_symbolic::lang::SymPayload::SymbolId(symbol)) = dag.get_leaf_data(node) {
        out.insert(*symbol);
        return;
    }
    if matches!(
        dag.get_node(node),
        K::Extract | K::LoadMemory | K::LoadReserved | K::AtomicRmw
    ) {
        return;
    }
    for child in dag.children(node) {
        collect_symbols(dag, child, out);
    }
}

/// An immediate operand's selection range: the pattern symbol it binds, the
/// width and signedness its behavior gives the encoded field, and the
/// constraints the operand declares.
#[derive(Clone, Copy)]
struct ImmediateRange {
    symbol: u32,
    width: u32,
    signed: bool,
    constraint: OperandConstraint,
}

/// The encoding range of each immediate operand: the field's bit width from the
/// operand type, signedness from how the behavior consumes the symbol —
/// `sext(imm, _)` sign-extends, everything else is unsigned — and an
/// `extract(imm, hi, 0)` wrapper (a shift-amount mask) narrows the usable bits.
/// Selection uses these to refuse constants the field cannot represent.
fn immediate_operand_ranges(
    dag: &impl tir_graph::Dag<Node = tir_symbolic::lang::SymKind, Leaf = tir_symbolic::lang::SymPayload<tir_symbolic::sem::ValueId>>,
    ops: &[(String, Type)],
    variable_symbols: &HashMap<String, u32>,
    constraints: &HashMap<String, OperandConstraint>,
) -> Vec<ImmediateRange> {
    use tir_symbolic::lang::{SymKind as K, SymPayload};

    let is_symbol_leaf = |node: tir_graph::NodeId, symbol: u32| {
        *dag.get_node(node) == K::Symbol
            && matches!(
                dag.get_leaf_data(node),
                Some(SymPayload::SymbolId(id)) if *id == symbol
            )
    };
    let const_value = |node: tir_graph::NodeId| match dag.get_leaf_data(node) {
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
            let node = tir_graph::NodeId::from_index(index);
            let children: Vec<tir_graph::NodeId> = dag.children(node).collect();
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
        out.push(ImmediateRange {
            symbol,
            width,
            signed,
            constraint: constraints.get(op_name).copied().unwrap_or_default(),
        });
    }
    out
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
fn scalar_root_kind(kind: &tir_symbolic::lang::SymKind) -> bool {
    use tir_symbolic::lang::SymKind as K;
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
            | K::Bitcast
            | K::FAdd
            | K::FSub
            | K::FMul
            | K::FDiv
            | K::SIToFP
            | K::UIToFP
            | K::FPToSI
            | K::FPToUI
    )
}

/// Whether `expr` reads or writes a program-counter register (`PC::pc`).
fn behavior_references_pc(expr: &ast::Expr, pc_classes: &HashSet<String>) -> bool {
    let mut found = false;
    crate::utils::visit_exprs(expr, &mut |e| {
        if let ast::Expr::Path(path) = e {
            found |= pc_classes.contains(&path.base);
        }
    });
    found
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
    let is_flag = |e: &ast::Expr| matches!(e, ast::Expr::Path(p) if flag_classes.contains(&p.base));
    let (mut mentions, mut writes) = (0usize, 0usize);
    crate::utils::visit_exprs(expr, &mut |e: &ast::Expr| match e {
        ast::Expr::Assign(a) if is_flag(&a.dest) => writes += 1,
        _ if is_flag(e) => mentions += 1,
        _ => {}
    });
    mentions > writes
}

/// Whether the *value* a behavior defines reads a status-flag register. Only
/// this portion may veto a value rule: the right-hand sides feeding non-flag
/// destinations, plus any statement-level guard around such an assignment. A
/// flag read confined to a flag output's own right-hand side (x86 rotate's
/// count-zero carry preservation) leaves the value expression flag-free, so the
/// rule still roots. Call with the let-inlined behavior, so a binding shared
/// between the value and a flag output is seen on the value side too.
fn value_reads_flag_register(expr: &ast::Expr, flag_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Assign(a) => {
            !assignment_dest_register_path(&a.dest)
                .is_some_and(|(class, _)| flag_classes.contains(&class))
                && behavior_reads_flag_register(expr, flag_classes)
        }
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| value_reads_flag_register(stmt, flag_classes)),
        ast::Expr::If(i) => {
            value_reads_flag_register(&i.then, flag_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| value_reads_flag_register(e, flag_classes))
                || (defines_value(&i.then, flag_classes)
                    || i.else_
                        .as_ref()
                        .is_some_and(|e| defines_value(e, flag_classes)))
                    && behavior_reads_flag_register(&i.cond, flag_classes)
        }
        ast::Expr::Try(t) => value_reads_flag_register(&t.body, flag_classes),
        other => behavior_reads_flag_register(other, flag_classes),
    }
}

/// Whether a statement defines anything but a status flag — the guarded arms
/// whose condition therefore feeds the value (see [`value_reads_flag_register`]).
fn defines_value(expr: &ast::Expr, flag_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Assign(a) => !assignment_dest_register_path(&a.dest)
            .is_some_and(|(class, _)| flag_classes.contains(&class)),
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| defines_value(stmt, flag_classes)),
        ast::Expr::If(i) => {
            defines_value(&i.then, flag_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| defines_value(e, flag_classes))
        }
        ast::Expr::Try(t) => defines_value(&t.body, flag_classes),
        ast::Expr::Lit(_) | ast::Expr::Ident(_) | ast::Expr::Path(_) => false,
        _ => true,
    }
}

/// Whether the behavior assigns a fixed register path (`GPR::rsp = …`) outside
/// the flag classes. A single-value tile claims only its operand write; a
/// sibling fixed-register write would be dropped from the claim while the
/// fixed read binds as a free pattern variable — a `pop` rule matching any
/// load. Flag-path writes stay legal: the flag machinery composes them.
fn behavior_writes_fixed_register(expr: &ast::Expr, flag_classes: &HashSet<String>) -> bool {
    match expr {
        ast::Expr::Assign(a) => assignment_dest_register_path(&a.dest)
            .is_some_and(|(class, _)| !flag_classes.contains(&class)),
        ast::Expr::Block(b) => b
            .stmts
            .iter()
            .any(|stmt| behavior_writes_fixed_register(stmt, flag_classes)),
        ast::Expr::If(i) => {
            behavior_writes_fixed_register(&i.then, flag_classes)
                || i.else_
                    .as_ref()
                    .is_some_and(|e| behavior_writes_fixed_register(e, flag_classes))
        }
        ast::Expr::Try(t) => behavior_writes_fixed_register(&t.body, flag_classes),
        _ => false,
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
    behavior: &'a ast::Expr,
    operands: &[(String, Type)],
    defined_register_operands: &[String],
) -> Option<&'a ast::Expr> {
    let register_operands = register_operand_names(operands);

    let mut assignments = Vec::new();
    collect_behavior_assignments(behavior, &mut assignments);
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
    if let Some(store) = find_store_effect_expr(behavior) {
        return Some(store);
    }
    match behavior {
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

pub(crate) fn resolve_string(expr: &ast::Expr) -> Option<String> {
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
