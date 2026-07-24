// ---------------------------------------------------------------------------
// Instruction analysis helpers
// ---------------------------------------------------------------------------

struct InstructionSemantics {
    pattern: tir::sem::SemGraph,
    root: tir::graph::NodeId,
    variable_symbols: HashMap<String, u32>,
    fixed_register_by_class: HashMap<String, Option<u16>>,
    /// `(register class, index) -> pattern symbol` for every register the behavior
    /// reads by path (e.g. `VCSR::vl`). These are implicit reads — registers not
    /// among the encoded operands — and become the rule's `implicit_uses`.
    register_symbols: HashMap<(String, u32), u32>,
    /// The destination's full guarded semantics (`If(cond, then, else)`) when the
    /// behavior assigns the result under a statement-level `if`/`else`, e.g. riscv
    /// `div`. The selection pattern is the guard-relaxed else arm; this lets pass
    /// construction prove the relaxation sound. `None` for unguarded behaviors.
    guarded_semantics: Option<(tir::sem::SemGraph, tir::graph::NodeId)>,
}

/// The selectable semantics of a conditional-branch instruction: the branch
/// condition as a pattern, plus the operand carrying the taken target.
struct BranchSemantics {
    /// The condition expression (`rs1 == rs2`, …) as a pattern graph.
    pattern: tir::sem::SemGraph,
    root: tir::graph::NodeId,
    variable_symbols: HashMap<String, u32>,
    /// The immediate operand encoding the taken target (`imm`), and the fresh
    /// pattern symbol the emitter reads it from as a block binding.
    target_operand: String,
    target_symbol: u32,
}

/// Recognize the guarded-PC-write shape `if COND { PC::pc = PC::pc + …imm… }`
/// and derive a conditional-branch rule from it: the pattern is `COND` over the
/// instruction's register operands, and `imm` becomes the taken-target block
/// operand. Anything else (fallthrough writes, extra state, PC in the
/// condition) is rejected.
/// Recognize the guarded-PC-write shape `if COND { PC::pc = …imm… }` and return
/// the guard condition together with the single immediate operand the PC write
/// references (the taken target). Anything else (an `else` arm, fallthrough
/// writes, a non-immediate target) is rejected.
fn guarded_pc_write_shape<'a>(
    inst: &'a ast::Instruction,
    operands: &[(String, Type)],
    pc_classes: &HashSet<String>,
) -> Option<(&'a ast::Expr, String)> {
    // Behavior must be exactly one guarded write: `if cond { PC::pc = … }`.
    let mut body = &inst.behavior;
    while let ast::Expr::Block(block) = body {
        let [stmt] = block.stmts.as_slice() else {
            return None;
        };
        body = stmt;
    }
    let ast::Expr::If(guarded) = body else {
        return None;
    };
    if guarded.else_.is_some() {
        return None;
    }

    let mut taken = guarded.then.as_ref();
    while let ast::Expr::Block(block) = taken {
        let [stmt] = block.stmts.as_slice() else {
            return None;
        };
        taken = stmt;
    }
    let ast::Expr::Assign(assign) = taken else {
        return None;
    };
    let (dest_class, _) = assignment_dest_register_path(&assign.dest)?;
    if !pc_classes.contains(&dest_class) {
        return None;
    }

    // The taken target: the single immediate operand the PC write references.
    let operand_names: HashSet<&str> = operands.iter().map(|(name, _)| name.as_str()).collect();
    let target_refs = referenced_operands(&assign.value, &operand_names);
    let [target_operand] = target_refs.as_slice() else {
        return None;
    };
    let target_is_immediate = operands
        .iter()
        .any(|(name, ty)| name == target_operand && matches!(ty, Type::Bits(_) | Type::Integer));
    if !target_is_immediate {
        return None;
    }

    Some((&guarded.cond, target_operand.clone()))
}

fn analyze_branch_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    pc_classes: &HashSet<String>,
) -> Option<BranchSemantics> {
    let (cond, target_operand) = guarded_pc_write_shape(inst, operands, pc_classes)?;

    // The condition must be expressible over the encoded operands alone.
    if behavior_references_pc(cond, pc_classes) {
        return None;
    }
    let mut pattern = tir::sem::SemGraph::new();
    let lowering = cond.lower_to_sema_with_isa(
        &mut pattern,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    if !lowering.register_symbols.is_empty() {
        return None;
    }

    let target_symbol = lowering
        .variable_symbols
        .values()
        .max()
        .map_or(0, |max| max + 1);

    Some(BranchSemantics {
        pattern,
        root: lowering.root,
        variable_symbols: lowering.variable_symbols,
        target_operand,
        target_symbol,
    })
}

/// A flag-definer instruction (`cmp`, `test`): every behavior statement assigns
/// a status-flag register of one class. `flag_roots` maps each written flag's
/// register index to its value expression, lowered over the encoded operands
/// into `graph` through one shared symbol table.
struct FlagDefinerSemantics {
    class: String,
    graph: tir::sem::SemGraph,
    flag_roots: HashMap<u32, tir::graph::NodeId>,
    variable_symbols: HashMap<String, u32>,
}

/// A flag-guarded branch (`b.lt`, `jl`): a guarded PC write whose condition
/// reads only status-flag registers of one class.
struct FlagBranchSemantics {
    class: String,
    graph: tir::sem::SemGraph,
    root: tir::graph::NodeId,
    /// Guard symbol id -> the flag register index it reads.
    flag_symbols: HashMap<u32, u32>,
    target_operand: String,
}

/// A flag-reading value materializer (`cset`, `setcc`): defines one register as
/// `if <flags> { c1 } else { c0 }` over one class's status flags, with constant
/// arms. Composed with a flag definer it yields a boolean materializer value
/// rule (see `emit_flag_reader_rules`).
struct FlagReaderSemantics {
    class: String,
    graph: tir::sem::SemGraph,
    /// The `if`'s condition, then, and else subgraphs.
    cond_root: tir::graph::NodeId,
    then_root: tir::graph::NodeId,
    else_root: tir::graph::NodeId,
    /// Condition symbol id -> the flag register index it reads.
    flag_symbols: HashMap<u32, u32>,
    dest_operand: String,
}

/// The statement list of a behavior body (peeling wrapper blocks).
fn behavior_statements(behavior: &ast::Expr) -> Vec<&ast::Expr> {
    let mut body = behavior;
    while let ast::Expr::Block(block) = body {
        if let [stmt] = block.stmts.as_slice() {
            body = stmt;
        } else {
            return block.stmts.iter().collect();
        }
    }
    vec![body]
}

/// Recognize a flag definer: every behavior statement assigns a distinct
/// status-flag register of one class, each flag's value a pure function of the
/// encoded register operands. ISA parameters (`self.XLEN`) resolve to their
/// concrete values here — the composed condition is proved against a canonical
/// comparison, so no width expression survives into the emitted pattern.
fn analyze_flag_definer_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Option<FlagDefinerSemantics> {
    if flag_classes.is_empty() {
        return None;
    }
    let stmts = behavior_statements(&inst.behavior);
    if stmts.is_empty() {
        return None;
    }

    let mut class: Option<String> = None;
    let mut flag_exprs: Vec<(u32, &ast::Expr)> = Vec::new();
    for stmt in stmts {
        let ast::Expr::Assign(assign) = stmt else {
            return None;
        };
        let (dest_class, dest_reg) = assignment_dest_register_path(&assign.dest)?;
        if !flag_classes.contains(&dest_class) {
            return None;
        }
        match &class {
            Some(existing) if *existing != dest_class => return None,
            None => class = Some(dest_class.clone()),
            _ => {}
        }
        let index = *register_index_map.get(&(dest_class, dest_reg))?;
        if flag_exprs.iter().any(|(existing, _)| *existing == index) {
            return None;
        }
        if behavior_references_pc(&assign.value, pc_classes) {
            return None;
        }
        flag_exprs.push((index, &assign.value));
    }

    // Composition binds each register operand to a pattern symbol the emitted
    // pair reads back as a register, and at most one immediate operand to a
    // constant symbol feeding the composed comparison. Any other operand shape
    // is not derived.
    let immediate_operands = operands
        .iter()
        .filter(|(_, ty)| matches!(ty, Type::Bits(_) | Type::Integer))
        .count();
    if immediate_operands > 1
        || operands.iter().any(|(_, ty)| {
            !matches!(
                ty,
                Type::Struct(_) | Type::String | Type::Bits(_) | Type::Integer
            )
        })
    {
        return None;
    }

    let mut params = numeric_params.clone();
    params.extend(isa_param_values.iter().map(|(k, v)| (k.clone(), *v)));
    let mut graph = tir::sem::SemGraph::new();
    let exprs: Vec<&ast::Expr> = flag_exprs.iter().map(|(_, expr)| *expr).collect();
    let (roots, lowering) = ast::Expr::lower_all_to_sema_with_isa(
        &exprs,
        &mut graph,
        &params,
        isa_param_values,
        register_index_map,
    )?;
    // The flags must be functions of the encoded operands alone (no implicit
    // register reads), and every register operand must feed some flag, or the
    // emitted definer could not bind it.
    if !lowering.register_symbols.is_empty() {
        return None;
    }
    if operands.iter().any(|(name, ty)| {
        matches!(ty, Type::Struct(_)) && !lowering.variable_symbols.contains_key(name)
    }) {
        return None;
    }

    Some(FlagDefinerSemantics {
        class: class?,
        graph,
        flag_roots: flag_exprs
            .iter()
            .map(|(index, _)| *index)
            .zip(roots)
            .collect(),
        variable_symbols: lowering.variable_symbols,
    })
}

/// Recognize a flag-guarded branch: the guarded-PC-write shape whose condition
/// reads only status-flag registers of one class and whose sole encodable
/// operand is the taken target.
fn analyze_flag_branch_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Option<FlagBranchSemantics> {
    if flag_classes.is_empty() {
        return None;
    }
    let (cond, target_operand) = guarded_pc_write_shape(inst, operands, pc_classes)?;
    if operands
        .iter()
        .any(|(name, ty)| *name != target_operand && !matches!(ty, Type::String))
    {
        return None;
    }
    if behavior_references_pc(cond, pc_classes) {
        return None;
    }

    let mut params = numeric_params.clone();
    params.extend(isa_param_values.iter().map(|(k, v)| (k.clone(), *v)));
    let mut graph = tir::sem::SemGraph::new();
    let lowering =
        cond.lower_to_sema_with_isa(&mut graph, &params, isa_param_values, register_index_map)?;
    if !lowering.variable_symbols.is_empty() || lowering.register_symbols.is_empty() {
        return None;
    }

    let mut class: Option<String> = None;
    let mut flag_symbols = HashMap::new();
    for ((reg_class, index), symbol) in &lowering.register_symbols {
        if !flag_classes.contains(reg_class) {
            return None;
        }
        match &class {
            Some(existing) if existing != reg_class => return None,
            None => class = Some(reg_class.clone()),
            _ => {}
        }
        flag_symbols.insert(*symbol, *index);
    }

    Some(FlagBranchSemantics {
        class: class?,
        graph,
        root: lowering.root,
        flag_symbols,
        target_operand,
    })
}

/// Recognize a flag-reading value materializer: one register defined as `if
/// <cond> { c1 } else { c0 }` whose condition reads only status-flag registers
/// of one class and whose arms are functions of those flags alone. Selects
/// (`csel`, arms reading encoded operands) are rejected by the operand-read
/// check.
///
/// The value is lowered exactly as a plain value rule would be — `self.XLEN`
/// kept as a width symbol rather than const-folded — so the emitted arms are
/// the width-polymorphic `slt`-style form the bool-materialize bridge matches.
fn analyze_flag_reader_semantics(
    inst: &ast::Instruction,
    operands: &[(String, Type)],
    numeric_params: &HashMap<String, i64>,
    isa_param_values: &HashMap<String, i64>,
    register_index_map: &HashMap<(String, String), u32>,
    flag_classes: &HashSet<String>,
    pc_classes: &HashSet<String>,
) -> Option<FlagReaderSemantics> {
    use tir::graph::Dag;
    if flag_classes.is_empty() {
        return None;
    }
    let defined_register_operands = infer_defined_register_operands(&inst.behavior, operands);
    let [dest] = defined_register_operands.as_slice() else {
        return None;
    };
    let stmts = behavior_statements(&inst.behavior);
    let [stmt] = stmts.as_slice() else {
        return None;
    };
    let ast::Expr::Assign(assign) = stmt else {
        return None;
    };
    if assignment_dest_name(&assign.dest).as_deref() != Some(dest.as_str()) {
        return None;
    }
    let ast::Expr::If(if_expr) = &*assign.value else {
        return None;
    };
    // The arms carry the materialized value: they must not themselves read flags
    // (the composition only substitutes the condition's reads).
    if if_expr.else_.is_none()
        || behavior_references_pc(&assign.value, pc_classes)
        || behavior_reads_flag_register(&if_expr.then, flag_classes)
        || if_expr
            .else_
            .as_ref()
            .is_some_and(|e| behavior_reads_flag_register(e, flag_classes))
    {
        return None;
    }

    let mut graph = tir::sem::SemGraph::new();
    let lowering = assign.value.lower_to_sema_with_isa(
        &mut graph,
        numeric_params,
        isa_param_values,
        register_index_map,
    )?;
    // No encoded operand feeds the value (that would be a select, not a boolean
    // materializer); `self.XLEN` is an ISA param, not an operand, so it may still
    // appear as a width symbol. It must actually read a flag.
    if operands
        .iter()
        .any(|(name, _)| lowering.variable_symbols.contains_key(name))
        || lowering.register_symbols.is_empty()
    {
        return None;
    }

    let root = lowering.root;
    if *graph.get_node(root) != tir::sem::SymKind::If {
        return None;
    }
    let children: Vec<tir::graph::NodeId> = graph.children(root).collect();
    let [cond_root, then_root, else_root] = children.as_slice() else {
        return None;
    };

    let mut class: Option<String> = None;
    let mut flag_symbols = HashMap::new();
    for ((reg_class, index), symbol) in &lowering.register_symbols {
        if !flag_classes.contains(reg_class) {
            return None;
        }
        match &class {
            Some(existing) if existing != reg_class => return None,
            None => class = Some(reg_class.clone()),
            _ => {}
        }
        flag_symbols.insert(*symbol, *index);
    }

    Some(FlagReaderSemantics {
        class: class?,
        graph,
        cond_root: *cond_root,
        then_root: *then_root,
        else_root: *else_root,
        flag_symbols,
        dest_operand: dest.clone(),
    })
}

/// Copy `node`'s subgraph from `src` into `dst`, preserving payloads. Children
/// are copied first, keeping `dst` in post order.
fn copy_subgraph(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = src.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| copy_subgraph(dst, src, child, memo))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(data) = src.get_leaf_data(node) {
        dst.set_leaf_data(copied, data.clone());
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    memo.insert(node.index(), copied);
    copied
}

/// Copy `node`'s subgraph, renumbering each distinct symbol id through `remap`
/// to a fresh id from `next`. Used to lift a reader's arm symbols (its `XLEN`
/// width var) above the two comparison-operand symbols they are spliced beside,
/// so the two symbol spaces do not collide.
fn copy_subgraph_remap_symbols(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    memo: &mut HashMap<usize, tir::graph::NodeId>,
    remap: &mut HashMap<u32, u32>,
    next: &mut u32,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = memo.get(&node.index()) {
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = src.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| copy_subgraph_remap_symbols(dst, src, child, memo, remap, next))
        .collect();
    let copied = dst.add_node(*src.get_node(node));
    if let Some(data) = src.get_leaf_data(node) {
        let data = if let tir::sem::SymPayload::SymbolId(id) = data {
            let new_id = *remap.entry(*id).or_insert_with(|| {
                let assigned = *next;
                *next += 1;
                assigned
            });
            tir::sem::SymPayload::SymbolId(new_id)
        } else {
            data.clone()
        };
        dst.set_leaf_data(copied, data);
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    memo.insert(node.index(), copied);
    copied
}

/// Copy a boolean value materializer's arm (`zext(0/1, W)`), replacing the
/// widen-to width with a fresh capture symbol so the pattern matches the boolean
/// regardless of the destination register width. Without this an 8-bit `setcc`
/// arm (`zext(1, 8)`) fails to match the width-1 boolean the bridge produces,
/// while an `XLEN`-symbol arm (arm64 `cset`) already generalizes — the value is a
/// boolean 0/1, the register width is not part of what selects it.
fn copy_reader_arm(
    dst: &mut tir::sem::SemGraph,
    src: &tir::sem::SemGraph,
    arm_root: tir::graph::NodeId,
    remap: &mut HashMap<u32, u32>,
    next: &mut u32,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    let kind = *src.get_node(arm_root);
    if matches!(kind, tir::sem::SymKind::ZExt | tir::sem::SymKind::SExt) {
        let children: Vec<tir::graph::NodeId> = src.children(arm_root).collect();
        if children.len() == 2 {
            let value = copy_subgraph_remap_symbols(
                dst,
                src,
                children[0],
                &mut HashMap::new(),
                remap,
                next,
            );
            let width = dst.add_node(tir::sem::SymKind::Symbol);
            dst.set_leaf_data(width, tir::sem::SymPayload::SymbolId(*next));
            *next += 1;
            let widened = dst.add_node(kind);
            dst.add_edge(widened, value);
            dst.add_edge(widened, width);
            return widened;
        }
    }
    copy_subgraph_remap_symbols(dst, src, arm_root, &mut HashMap::new(), remap, next)
}

/// Copy the branch guard from `guard` into `dst`, replacing each status-flag
/// read (a symbol in `substitute`) with a copy of the definer's expression for
/// that flag. The definer's operand symbols survive verbatim, so the composed
/// condition is a function of the definer's encoded operands alone.
fn compose_guard_with_definer(
    dst: &mut tir::sem::SemGraph,
    guard: &tir::sem::SemGraph,
    node: tir::graph::NodeId,
    substitute: &HashMap<u32, tir::graph::NodeId>,
    definer: &tir::sem::SemGraph,
    guard_memo: &mut HashMap<usize, tir::graph::NodeId>,
    definer_memo: &mut HashMap<usize, tir::graph::NodeId>,
) -> tir::graph::NodeId {
    use tir::graph::{Dag, MutDag};
    if let Some(&copied) = guard_memo.get(&node.index()) {
        return copied;
    }
    if let Some(tir::sem::SymPayload::SymbolId(symbol)) = guard.get_leaf_data(node)
        && let Some(&flag_root) = substitute.get(symbol)
    {
        let copied = copy_subgraph(dst, definer, flag_root, definer_memo);
        guard_memo.insert(node.index(), copied);
        return copied;
    }
    let children: Vec<tir::graph::NodeId> = guard.children(node).collect();
    let copied_children: Vec<tir::graph::NodeId> = children
        .into_iter()
        .map(|child| {
            compose_guard_with_definer(
                dst,
                guard,
                child,
                substitute,
                definer,
                guard_memo,
                definer_memo,
            )
        })
        .collect();
    let copied = dst.add_node(*guard.get_node(node));
    if let Some(data) = guard.get_leaf_data(node) {
        dst.set_leaf_data(copied, data.clone());
    }
    for child in copied_children {
        dst.add_edge(copied, child);
    }
    guard_memo.insert(node.index(), copied);
    copied
}

/// Operator kinds the constant folder may evaluate: pure scalar computations
/// with a defined interpreter semantics.
fn foldable_kind(kind: &tir::sem::SymKind) -> bool {
    use tir::sem::SymKind as K;
    matches!(
        kind,
        K::Add
            | K::Sub
            | K::Mul
            | K::Neg
            | K::And
            | K::Or
            | K::Xor
            | K::Not
            | K::ShiftLeft
            | K::ShiftRightLogic
            | K::ShiftRightArithmetic
            | K::ZExt
            | K::SExt
            | K::Extract
            | K::Log2Ceil
            | K::Concat
    )
}

/// Fold maximal constant subtrees into constant leaves. Width expressions like
/// `self.XLEN - 1` lower (with the concrete ISA parameter) to `Sub(64, 1)`;
/// the SMT oracle's bit-blaster needs them as literal extract bounds and
/// extension widths, so they are evaluated here with the reference interpreter.
fn fold_constant_subtrees(
    src: &tir::sem::SemGraph,
    root: tir::graph::NodeId,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    use tir::graph::{Dag, MutDag};

    // Whether every leaf under `node` is a constant and every operator foldable.
    fn all_constant(
        src: &tir::sem::SemGraph,
        node: tir::graph::NodeId,
        memo: &mut HashMap<usize, bool>,
    ) -> bool {
        if let Some(&known) = memo.get(&node.index()) {
            return known;
        }
        let result = match src.get_leaf_data(node) {
            Some(tir::sem::SymPayload::Int(_)) => true,
            Some(_) => false,
            None => {
                foldable_kind(src.get_node(node))
                    && src
                        .children(node)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .all(|child| all_constant(src, child, memo))
            }
        };
        memo.insert(node.index(), result);
        result
    }

    fn walk(
        dst: &mut tir::sem::SemGraph,
        src: &tir::sem::SemGraph,
        node: tir::graph::NodeId,
        const_memo: &mut HashMap<usize, bool>,
        copy_memo: &mut HashMap<usize, tir::graph::NodeId>,
    ) -> tir::graph::NodeId {
        if let Some(&copied) = copy_memo.get(&node.index()) {
            return copied;
        }
        let copied = if src.get_leaf_data(node).is_none() && all_constant(src, node, const_memo) {
            let mut sub = tir::sem::SemGraph::new();
            copy_subgraph(&mut sub, src, node, &mut HashMap::new());
            let tir::sem::Value::Int(value) = tir::sem::execute(&sub, &[]) else {
                // Not evaluable after all: copy verbatim.
                return copy_subgraph(dst, src, node, copy_memo);
            };
            let leaf = dst.add_node(tir::sem::SymKind::Constant);
            dst.set_leaf_data(leaf, tir::sem::SymPayload::Int(value));
            leaf
        } else {
            let children: Vec<tir::graph::NodeId> = src.children(node).collect();
            let copied_children: Vec<tir::graph::NodeId> = children
                .into_iter()
                .map(|child| walk(dst, src, child, const_memo, copy_memo))
                .collect();
            let copied = dst.add_node(*src.get_node(node));
            if let Some(data) = src.get_leaf_data(node) {
                dst.set_leaf_data(copied, data.clone());
            }
            for child in copied_children {
                dst.add_edge(copied, child);
            }
            copied
        };
        copy_memo.insert(node.index(), copied);
        copied
    }

    let mut dst = tir::sem::SemGraph::new();
    let folded_root = walk(
        &mut dst,
        src,
        root,
        &mut HashMap::new(),
        &mut HashMap::new(),
    );
    (dst, folded_root)
}

/// `kind(s0, s1)` (or swapped) — a candidate canonical comparison over the
/// definer's two operand symbols.
fn comparison_candidate(
    kind: tir::sem::SymKind,
    swap: bool,
) -> (tir::sem::SemGraph, tir::graph::NodeId) {
    use tir::graph::MutDag;
    let mut g = tir::sem::SemGraph::new();
    let a = g.add_node(tir::sem::SymKind::Symbol);
    g.set_leaf_data(a, tir::sem::SymPayload::SymbolId(0));
    let b = g.add_node(tir::sem::SymKind::Symbol);
    g.set_leaf_data(b, tir::sem::SymPayload::SymbolId(1));
    let (lhs, rhs) = if swap { (b, a) } else { (a, b) };
    let root = g.add_node(kind);
    g.add_edge(root, lhs);
    g.add_edge(root, rhs);
    (g, root)
}

/// The comparison the composed flag condition is provably equivalent to, if
/// any: the six canonical predicates `cmpi` lowers to, in both operand orders.
/// A fuzz filter picks the candidate cheaply; the SMT oracle then proves it
/// (bit-blasted equivalence at the operands' architectural widths), so a wrong
/// flag formula derives no rule instead of a miscompiling one.
fn find_equivalent_comparison(
    composed: &tir::sem::SemGraph,
    symbol_widths: &[u32],
) -> Option<(tir::sem::SemGraph, tir::graph::NodeId)> {
    use tir::sem::{EquivalenceOracle, FuzzOracle, SmtOracle, SymKind};
    const CANDIDATES: &[(SymKind, bool)] = &[
        (SymKind::Eq, false),
        (SymKind::Ne, false),
        (SymKind::Lt, false),
        (SymKind::Lt, true),
        (SymKind::Ge, false),
        (SymKind::Ge, true),
        (SymKind::ULt, false),
        (SymKind::ULt, true),
        (SymKind::UGe, false),
        (SymKind::UGe, true),
    ];
    let fuzz = FuzzOracle::default();
    for (kind, swap) in CANDIDATES {
        let (candidate, root) = comparison_candidate(*kind, *swap);
        if fuzz.equivalent(composed, &candidate, symbol_widths)
            && SmtOracle.equivalent(composed, &candidate, symbol_widths)
        {
            return Some((candidate, root));
        }
    }
    None
}

/// A register class's architectural width: a literal `WIDTH`, or `WIDTH =
/// self.PARAM` resolved through the instruction's ISA parameter view.
fn register_class_width_with_isa(
    files: &[ast::File],
    class_name: &str,
    isa_param_values: &HashMap<String, i64>,
) -> Option<u32> {
    let rc = files
        .iter()
        .flat_map(|f| f.register_classes())
        .find(|rc| rc.name == class_name)?;
    match rc.parameters.get("WIDTH") {
        Some((_ty, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
            Some(parse_literal_value(li) as u32)
        }
        Some((_ty, Some(ast::Expr::Field(field)))) if matches!(&*field.base, ast::Expr::Ident(id) if id.name == "self") => {
            isa_param_values
                .get(field.member.as_str())
                .map(|v| *v as u32)
        }
        _ => None,
    }
}

/// The architectural bit-width of each of a definer's comparison-operand
/// symbols. A register operand's width comes from its class; the immediate
/// operand shares it — comparison operands are the same architectural width, so
/// the composed condition proves against a canonical comparison over full-width
/// symbols. `None` if a register width is unresolved or a symbol is untyped.
fn definer_symbol_widths(
    files: &[ast::File],
    d: &FlagInst<'_>,
    d_sem: &FlagDefinerSemantics,
) -> Option<Vec<u32>> {
    let mut widths = vec![0u32; d_sem.variable_symbols.len()];
    let mut imm_symbol: Option<u32> = None;
    let mut register_width: Option<u32> = None;
    for (op_name, op_ty) in &d.ops {
        let Some(&symbol) = d_sem.variable_symbols.get(op_name) else {
            continue;
        };
        match op_ty {
            Type::Struct(class_name) => {
                let width = register_class_width_with_isa(files, class_name, &d.isa_param_values)?;
                widths[symbol as usize] = width;
                register_width = Some(width);
            }
            Type::Bits(_) | Type::Integer => imm_symbol = Some(symbol),
            _ => {}
        }
    }
    if let Some(symbol) = imm_symbol {
        widths[symbol as usize] = register_width?;
    }
    if widths.contains(&0) {
        return None;
    }
    Some(widths)
}

/// The pattern symbols bound to a definer's immediate operands (there is at most
/// one), for canonicalization and immediate-range enforcement.
fn definer_immediate_symbols(d: &FlagInst<'_>, d_sem: &FlagDefinerSemantics) -> HashSet<u32> {
    d.ops
        .iter()
        .filter(|(_, ty)| matches!(ty, Type::Bits(_) | Type::Integer))
        .filter_map(|(name, _)| d_sem.variable_symbols.get(name).copied())
        .collect()
}

/// A flag-mediated instruction's resolved shape, shared by definer, branch and
/// reader analysis.
struct FlagInst<'a> {
    inst: &'a ast::Instruction,
    ops: Vec<(String, Type)>,
    mnemonic: String,
    isa_param_values: HashMap<String, i64>,
}

/// Emit an flag-definer prelude function (materializing the flag-setting
/// instruction ahead of its consumer) once per definer, and return its ident
/// plus the definer's operand register constraints. Shared by branch and reader
/// pair emission, deduping through `emitted_preludes`.
fn emit_flag_definer_prelude(
    d: &FlagInst<'_>,
    d_sem: &FlagDefinerSemantics,
    emitted_preludes: &mut HashSet<String>,
    isel_rule_emitters: &mut Vec<proc_macro2::TokenStream>,
) -> (proc_macro2::Ident, Vec<proc_macro2::TokenStream>) {
    let prelude_fn_ident = format_ident!("emit_isel_flag_definer_{}", d.inst.name.to_lowercase());
    let d_builder_ident = format_ident!("{}OpBuilder", &d.inst.name);

    let mut operand_constraint_entries: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut prelude_attr_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (op_name, op_ty) in &d.ops {
        let Some(&symbol) = d_sem.variable_symbols.get(op_name) else {
            continue;
        };
        let op_name_lit = proc_macro2::Literal::string(op_name);
        let symbol_lit = proc_macro2::Literal::u32_unsuffixed(symbol);
        match op_ty {
            Type::Struct(class_name) => {
                let class_id = reg_class_id(class_name);
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Register) });
                prelude_attr_steps.push(quote! {
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
            Type::Bits(_) | Type::Integer => {
                operand_constraint_entries
                    .push(quote! { (#symbol_lit, tir::graph::OperandConstraint::Immediate) });
                prelude_attr_steps.push(quote! {
                    let v = m
                        .int_binding(#symbol_lit)
                        .ok_or(tir::PassError::RewriteFailed(req.op_id()))?;
                    builder = builder.attr(
                        #op_name_lit,
                        tir::attributes::AttributeValue::Int(v),
                    );
                });
            }
            _ => continue,
        }
    }

    if emitted_preludes.insert(d.inst.name.clone()) {
        isel_rule_emitters.push(quote! {
            fn #prelude_fn_ident(
                context: &tir::Context,
                req: &tir::backend::isel::EmitRequest,
                m: &tir::backend::isel::RuleMatch,
            ) -> Result<Box<dyn tir::Operation>, tir::PassError> {
                let _ = (req, m);
                let mut builder = #d_builder_ident::new(context);
                #(#prelude_attr_steps)*
                Ok(Box::new(builder.build()))
            }
        });
    }

    (prelude_fn_ident, operand_constraint_entries)
}

