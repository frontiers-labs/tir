use super::*;

pub(super) struct TransferTables {
    classes: HashMap<String, ast::RegisterClass>,
    parameters: std::collections::BTreeMap<String, Vec<i64>>,
}

impl TransferTables {
    pub(super) fn new(files: &[ast::File]) -> Self {
        let classes = files
            .iter()
            .flat_map(|file| file.register_classes())
            .map(|class| (class.name.clone(), class.clone()))
            .collect();
        let mut parameters = std::collections::BTreeMap::<String, Vec<i64>>::new();
        for isa in files.iter().flat_map(|file| file.isas()) {
            for (name, (_, value)) in &isa.parameters {
                if let Some(ast::Expr::Lit(ast::Lit::Int(value))) = value {
                    parameters
                        .entry(name.clone())
                        .or_default()
                        .push(parse_literal_value(value) as i64);
                }
            }
        }
        for values in parameters.values_mut() {
            values.sort_unstable();
            values.dedup();
        }
        Self {
            classes,
            parameters,
        }
    }

    pub(super) fn width(&self, class: &str, params: &HashMap<String, i64>) -> Option<u32> {
        let expr = self
            .classes
            .get(class)?
            .parameters
            .get("WIDTH")?
            .1
            .as_ref()?;
        let width = ast::const_eval_params(expr, params, &HashMap::new())?;
        u32::try_from(width).ok().filter(|width| *width > 0)
    }

    fn specializations(
        &self,
        ctx: &InstrEmitCtx<'_>,
        numeric: &HashMap<String, i64>,
    ) -> Vec<HashMap<String, i64>> {
        let mut used = BTreeSet::new();
        let mut collect = |expr: &ast::Expr| {
            crate::utils::visit_exprs(expr, &mut |expr| {
                if let ast::Expr::Field(field) = expr
                    && matches!(field.base.as_ref(), ast::Expr::Ident(id) if id.name == "self")
                    && !numeric.contains_key(&field.member)
                {
                    used.insert(field.member.clone());
                }
            });
        };
        collect(&ctx.inst.behavior);
        for (_, ty) in ctx.ops {
            if let Type::Struct(class) = ty
                && let Some((_, Some(expr))) = self
                    .classes
                    .get(class)
                    .and_then(|class| class.parameters.get("WIDTH"))
            {
                collect(expr);
            }
        }
        let mut variants = vec![HashMap::new()];
        for name in used {
            let Some(values) = self.parameters.get(&name) else {
                return vec![];
            };
            variants = variants
                .into_iter()
                .flat_map(|variant| {
                    let name = name.clone();
                    values.iter().map(move |value| {
                        let mut variant = variant.clone();
                        variant.insert(name.clone(), *value);
                        variant
                    })
                })
                .collect();
        }
        variants
    }
}

pub(super) struct TransferAddress<'a> {
    pub(super) base: &'a str,
    pub(super) bits: u32,
    pub(super) offset: Option<(&'a str, u16, bool, i64)>,
}

pub(super) struct TransferCandidate<'a> {
    pub(super) kind: &'static str,
    pub(super) width: u32,
    pub(super) destination: Option<&'a str>,
    pub(super) source: Vec<&'a str>,
    pub(super) address: Option<TransferAddress<'a>>,
    pub(super) alignment: u32,
}

fn transfer_pure(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Ident(_) | ast::Expr::Lit(ast::Lit::Int(_)) => true,
        ast::Expr::Field(field) => matches!(
            field.base.as_ref(), ast::Expr::Ident(id) if id.name == "self"
        ),
        ast::Expr::Binary(binary) => {
            matches!(
                binary.op,
                ast::BinOp::Add
                    | ast::BinOp::Sub
                    | ast::BinOp::BitwiseAnd
                    | ast::BinOp::BitwiseOr
                    | ast::BinOp::BitwiseXor
                    | ast::BinOp::ShiftLeftLogical
                    | ast::BinOp::ShiftRightLogical
                    | ast::BinOp::ShiftRightArithmetic
            ) && transfer_pure(&binary.lhs)
                && transfer_pure(&binary.rhs)
        }
        ast::Expr::Unary(unary) => transfer_pure(&unary.x),
        ast::Expr::Cast(cast) => transfer_pure(&cast.x) && transfer_pure(&cast.width),
        ast::Expr::Slice(slice) => transfer_pure(&slice.base),
        ast::Expr::Call(call) => {
            matches!(
                call.callee.as_ref(),
                ast::Expr::BuiltinFunction(
                    ast::BuiltinFunction::SExt
                        | ast::BuiltinFunction::ZExt
                        | ast::BuiltinFunction::Extract
                        | ast::BuiltinFunction::Bitcast
                )
            ) && call.arguments.iter().all(transfer_pure)
        }
        _ => false,
    }
}

fn transfer_statement(expr: &ast::Expr) -> Option<(&ast::Expr, Option<&str>)> {
    match expr {
        ast::Expr::Block(block) => match block.stmts.as_slice() {
            [expr] => transfer_statement(expr),
            _ => None,
        },
        ast::Expr::Try(try_) => {
            let [handler] = try_.handlers.as_slice() else {
                return None;
            };
            if !matches!(
                handler.kind.as_str(),
                "misaligned_load" | "misaligned_store"
            ) {
                return None;
            }
            let (body, caught) = transfer_statement(&try_.body)?;
            caught
                .is_none()
                .then_some((body, Some(handler.kind.as_str())))
        }
        ast::Expr::Assign(_) | ast::Expr::Call(_) => Some((expr, None)),
        _ => None,
    }
}

fn transfer_register_class<'a>(ctx: &'a InstrEmitCtx<'_>, name: &str) -> Option<&'a str> {
    ctx.ops.iter().find_map(|(operand, ty)| match ty {
        Type::Struct(class) if operand == name => Some(class.as_str()),
        _ => None,
    })
}

fn transfer_address<'a>(
    expr: &'a ast::Expr,
    tables: &TransferTables,
    ctx: &InstrEmitCtx<'_>,
    params: &HashMap<String, i64>,
    numeric: &HashMap<String, i64>,
) -> Option<TransferAddress<'a>> {
    let mut constants = params.clone();
    constants.extend(numeric.clone());
    if let ast::Expr::Ident(id) = expr {
        return Some(TransferAddress {
            base: &id.name,
            bits: tables.width(transfer_register_class(ctx, &id.name)?, params)?,
            offset: None,
        });
    }
    let ast::Expr::Binary(binary) = expr else {
        return None;
    };
    if !matches!(binary.op, ast::BinOp::Add | ast::BinOp::Sub) {
        return None;
    }
    for (position, (base, offset)) in [(&binary.lhs, &binary.rhs), (&binary.rhs, &binary.lhs)]
        .into_iter()
        .enumerate()
    {
        if binary.op == ast::BinOp::Sub && position != 0 {
            continue;
        }
        let (ast::Expr::Ident(base), ast::Expr::Call(offset)) = (base.as_ref(), offset.as_ref())
        else {
            continue;
        };
        let signed = match offset.callee.as_ref() {
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::SExt) => true,
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::ZExt) => false,
            _ => continue,
        };
        let [ast::Expr::Ident(imm), width] = offset.arguments.as_slice() else {
            continue;
        };
        let Some(Type::Bits(bits)) = ctx.ops_map.get(&imm.name) else {
            continue;
        };
        let Some(class) = transfer_register_class(ctx, &base.name) else {
            continue;
        };
        let address_bits = tables.width(class, params)?;
        if ast::const_eval_params(width, &constants, &HashMap::new())
            != Some(i64::from(address_bits))
            || u32::from(*bits) > address_bits
        {
            continue;
        }
        return Some(TransferAddress {
            base: &base.name,
            bits: address_bits,
            offset: Some((
                &imm.name,
                *bits,
                signed,
                if binary.op == ast::BinOp::Sub { -1 } else { 1 },
            )),
        });
    }
    None
}

fn transfer_memory_call(expr: &ast::Expr, load: bool) -> Option<&ast::Call> {
    let mut found = None;
    let mut count = 0;
    crate::utils::visit_exprs(expr, &mut |expr| {
        if let ast::Expr::Call(call) = expr
            && matches!(
                call.callee.as_ref(),
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Load) if load
            )
        {
            count += 1;
            found = Some(call);
        }
    });
    (count == 1).then_some(found).flatten()
}

fn transfer_memory_bytes(expr: &ast::Expr, params: &HashMap<String, i64>) -> Option<u32> {
    let value = if let ast::Expr::Call(call) = expr {
        if !matches!(
            call.callee.as_ref(),
            ast::Expr::BuiltinFunction(ast::BuiltinFunction::ZExt)
        ) {
            return None;
        }
        let [value, width] = call.arguments.as_slice() else {
            return None;
        };
        let value = ast::const_eval_params(value, params, &HashMap::new())?;
        let width = ast::const_eval_params(width, params, &HashMap::new())?;
        if value < 0 || width <= 0 || (width < 63 && value >= 1_i64.checked_shl(width as u32)?) {
            return None;
        }
        value
    } else {
        ast::const_eval_params(expr, params, &HashMap::new())?
    };
    u32::try_from(value)
        .ok()
        .filter(|bytes| bytes.is_power_of_two())
}

fn transfer_copy<'a>(
    destination: &'a str,
    ctx: &'a InstrEmitCtx<'_>,
    tables: &TransferTables,
    params: &HashMap<String, i64>,
) -> Option<TransferCandidate<'a>> {
    let width = tables.width(transfer_register_class(ctx, destination)?, params)?;
    let mut source = Vec::new();
    for (name, ty) in ctx.ops {
        match ty {
            Type::Struct(class) if ctx.read_register_operands.contains(name) => {
                if tables.width(class, params)? != width || name == destination {
                    return None;
                }
                source.push(name.as_str());
            }
            Type::Bits(_) | Type::Integer => {
                if ctx
                    .operand_constraints
                    .get(name)
                    .is_some_and(|constraint| constraint.nonzero)
                {
                    return None;
                }
            }
            Type::Struct(_) => {}
            _ => return None,
        }
    }
    if source.is_empty() {
        return None;
    }
    Some(TransferCandidate {
        kind: "Copy",
        width,
        destination: Some(destination),
        source,
        address: None,
        alignment: 1,
    })
}

fn transfer_memory<'a>(
    stmt: &'a ast::Expr,
    caught: Option<&str>,
    ctx: &'a InstrEmitCtx<'_>,
    tables: &TransferTables,
    params: &HashMap<String, i64>,
    numeric: &HashMap<String, i64>,
) -> Option<TransferCandidate<'a>> {
    let (call, destination) = match stmt {
        ast::Expr::Assign(assign) => {
            let ast::Expr::Ident(destination) = assign.dest.as_ref() else {
                return None;
            };
            (
                transfer_memory_call(&assign.value, true)?,
                Some(destination.name.as_str()),
            )
        }
        ast::Expr::Call(call)
            if matches!(
                call.callee.as_ref(),
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Store)
            ) =>
        {
            (call, None)
        }
        _ => return None,
    };
    if call.arguments.len() != 3 {
        return None;
    }
    let mut constants = params.clone();
    constants.extend(numeric.clone());
    let bytes = transfer_memory_bytes(&call.arguments[1], &constants)?;
    let width = bytes.checked_mul(8)?;
    let address = transfer_address(&call.arguments[0], tables, ctx, params, numeric)?;
    if destination == Some(address.base) {
        return None;
    }
    let (register, source) = if let Some(destination) = destination {
        if !matches!(
            ast::const_eval_params(&call.arguments[2], numeric, params),
            Some(0 | 1)
        ) {
            return None;
        }
        (destination, vec![])
    } else {
        let candidates: Vec<_> = ctx
            .ops
            .iter()
            .filter_map(|(name, ty)| matches!(ty, Type::Struct(_)).then_some(name.as_str()))
            .filter(|name| *name != address.base)
            .collect();
        let [source] = candidates.as_slice() else {
            return None;
        };
        (*source, vec![*source])
    };
    if tables.width(transfer_register_class(ctx, register)?, params)? != width {
        return None;
    }
    let kind = if destination.is_some() {
        "Reload"
    } else {
        "Spill"
    };
    if caught.is_some_and(|caught| {
        caught
            != if destination.is_some() {
                "misaligned_load"
            } else {
                "misaligned_store"
            }
    }) {
        return None;
    }
    Some(TransferCandidate {
        kind,
        width,
        destination,
        source,
        address: Some(address),
        alignment: if caught.is_some() { bytes } else { 1 },
    })
}

fn emit_transfer_candidate(
    candidate: &TransferCandidate<'_>,
    tables: &TargetTables,
    ctx: &InstrEmitCtx<'_>,
    parameters: &HashMap<String, i64>,
    opcode: &str,
    recipe: usize,
) -> Option<Vec<allocation::Fact>> {
    let ports = instruction_ports(tables, ctx);
    let mut facts = Vec::new();
    for (name, ty) in ctx.ops {
        let Type::Struct(class) = ty else {
            let kind = if candidate
                .address
                .as_ref()
                .and_then(|a| a.offset)
                .is_some_and(|(offset, _, _, _)| offset == name)
            {
                "offset"
            } else if candidate.kind == "Copy" && matches!(ty, Type::Bits(_) | Type::Integer) {
                "constant"
            } else {
                return None;
            };
            facts.push(allocation::fact(
                "TMDLRecipeAttribute",
                vec![
                    opcode.into(),
                    recipe.into(),
                    name.into(),
                    kind.into(),
                    0.into(),
                ],
            ));
            continue;
        };
        let role = if candidate.destination == Some(name.as_str()) {
            "destination"
        } else if candidate.source.contains(&name.as_str()) {
            "source"
        } else if candidate.address.as_ref().is_some_and(|a| a.base == name) {
            "base"
        } else {
            return None;
        };
        let position = |def| {
            ports
                .iter()
                .filter(|(_, d)| *d == def)
                .position(|(port, _)| port == name)
                .and_then(|n| u16::try_from(n).ok())
                .map_or(-1, i64::from)
        };
        facts.push(allocation::fact(
            "TMDLRecipeRegister",
            vec![
                opcode.into(),
                recipe.into(),
                name.into(),
                class.into(),
                position(false).into(),
                position(true).into(),
                role.into(),
            ],
        ));
        if candidate.destination == Some(name.as_str()) {
            for index in tables
                .transfer
                .classes
                .get(class)?
                .resolve_registers()
                .filter(|r| r.traits.contains(&ast::RegisterTrait::HardwiredZero))
                .filter_map(|r| r.encoding_index())
            {
                facts.push(allocation::fact(
                    "TMDLRecipeExcluded",
                    vec![opcode.into(), recipe.into(), name.into(), index.into()],
                ));
            }
        }
    }
    if ports
        .iter()
        .any(|(port, _)| !ctx.ops_map.contains_key(port))
    {
        return None;
    }
    if let Some((name, bits, signed, factor)) = candidate.address.as_ref().and_then(|a| a.offset) {
        let c = ctx
            .operand_constraints
            .get(name)
            .copied()
            .unwrap_or_default();
        facts.push(allocation::fact(
            "TMDLRecipeOffsetFactor",
            vec![opcode.into(), recipe.into(), factor.into()],
        ));
        facts.push(allocation::fact(
            "TMDLRecipeOffset",
            vec![
                opcode.into(),
                recipe.into(),
                name.into(),
                bits.into(),
                signed.into(),
                c.align.into(),
                c.nonzero.into(),
            ],
        ));
    } else {
        facts.push(allocation::fact(
            "TMDLRecipeZeroOffset",
            vec![opcode.into(), recipe.into()],
        ));
    }
    let kind = match candidate.kind {
        "Copy" => "copy",
        "Spill" => "spill",
        "Reload" => "reload",
        "AddressAdd" => "address-add",
        _ => unreachable!(),
    };
    facts.push(allocation::fact(
        "TMDLRecipe",
        vec![
            opcode.into(),
            recipe.into(),
            kind.into(),
            candidate.width.into(),
            candidate.address.as_ref().map_or(0, |a| a.bits).into(),
            candidate.alignment.into(),
            ctx.inst.for_isas.len().into(),
            parameters.len().into(),
        ],
    ));
    for feature in &ctx.inst.for_isas {
        facts.push(allocation::fact(
            "TMDLRecipeFeature",
            vec![opcode.into(), recipe.into(), feature.into()],
        ));
    }
    let mut parameters: Vec<_> = parameters.iter().collect();
    parameters.sort_by_key(|(name, _)| *name);
    for (name, value) in parameters {
        facts.push(allocation::fact(
            "TMDLRecipeParameter",
            vec![opcode.into(), recipe.into(), name.into(), (*value).into()],
        ));
    }
    Some(facts)
}

pub(super) fn instruction_transfer_recipes(
    tables: &TargetTables,
    ctx: &InstrEmitCtx<'_>,
    numeric: &HashMap<String, i64>,
    defaults: &HashMap<String, i64>,
    opcode: &str,
) -> Vec<Vec<allocation::Fact>> {
    // Inlining an unused effectful let would erase an instruction effect.
    let mut unsafe_let = false;
    crate::utils::visit_exprs(&ctx.inst.behavior, &mut |expr| {
        if let ast::Expr::Let(let_) = expr {
            unsafe_let |= !transfer_pure(&let_.value);
        }
    });
    if unsafe_let || ctx.defined_register_operands.len() > 1 {
        return vec![];
    }
    let behavior = inline_let_bindings(&ctx.inst.behavior);
    let Some((stmt, caught)) = transfer_statement(&behavior) else {
        return vec![];
    };
    let Some(actual) = sem_expr_state::lower_behavior(
        &ctx.inst.behavior,
        None,
        numeric,
        defaults,
        &tables.register_index_map,
    ) else {
        return vec![];
    };
    let mut recipes = Vec::new();
    for specialization in tables.transfer.specializations(ctx, numeric) {
        let params = &specialization;
        if let Some(candidate) =
            transfer_memory(stmt, caught, ctx, &tables.transfer, params, numeric)
            && transfer_candidate_proves(&candidate, tables, ctx, &actual, params)
            && let Some(recipe) = emit_transfer_candidate(
                &candidate,
                tables,
                ctx,
                &specialization,
                opcode,
                recipes.len(),
            )
        {
            recipes.push(recipe);
        }
        if caught.is_some() {
            continue;
        }
        let ast::Expr::Assign(assign) = stmt else {
            continue;
        };
        let ast::Expr::Ident(destination) = assign.dest.as_ref() else {
            continue;
        };
        if let Some(candidate) = transfer_copy(&destination.name, ctx, &tables.transfer, params)
            && transfer_candidate_proves(&candidate, tables, ctx, &actual, params)
            && let Some(recipe) = emit_transfer_candidate(
                &candidate,
                tables,
                ctx,
                &specialization,
                opcode,
                recipes.len(),
            )
        {
            recipes.push(recipe);
        }
        if let Some(address) =
            transfer_address(&assign.value, &tables.transfer, ctx, params, numeric)
            && address.offset.is_some()
            && address.base != destination.name
            && transfer_register_class(ctx, &destination.name)
                .and_then(|class| tables.transfer.width(class, params))
                == Some(address.bits)
        {
            let candidate = TransferCandidate {
                kind: "AddressAdd",
                width: address.bits,
                destination: Some(&destination.name),
                source: vec![],
                address: Some(address),
                alignment: 1,
            };
            if transfer_candidate_proves(&candidate, tables, ctx, &actual, params)
                && let Some(recipe) = emit_transfer_candidate(
                    &candidate,
                    tables,
                    ctx,
                    &specialization,
                    opcode,
                    recipes.len(),
                )
            {
                recipes.push(recipe);
            }
        }
    }
    recipes
}
