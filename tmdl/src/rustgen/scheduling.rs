fn machine_model_ts(
    machine: &ast::Machine,
    machine_id: usize,
    instructions: &[(String, String, String)],
) -> Result<(proc_macro2::Ident, proc_macro2::TokenStream), TMDLError> {
    let id_lit = proc_macro2::Literal::usize_unsuffixed(machine_id);

    let pipeline_lits = machine.pipeline.iter().map(|p| {
        let name_lit = proc_macro2::Literal::string(&p.name);
        let prot_ts = protection_ts(p.protection);
        quote! {
            tir::backend::sched::PipelinePhase { name: #name_lit, protection: #prot_ts }
        }
    });

    let forward_lits = machine.forwards.iter().map(|f| {
        let from_lit = proc_macro2::Literal::string(&f.from);
        let to_lit = proc_macro2::Literal::string(&f.to);
        let lat_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(f.latency));
        quote! {
            tir::backend::sched::Forward { from: #from_lit, to: #to_lit, latency: #lat_lit }
        }
    });

    let resource_lits = machine.resources.iter().map(|r| {
        let name_lit = proc_macro2::Literal::string(&r.name);
        let units_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(r.units));
        quote! { tir::backend::sched::ProcUnit { name: #name_lit, units: #units_lit } }
    });

    let buffer_lits = machine.buffers.iter().map(|(name, size)| {
        let name_lit = proc_macro2::Literal::string(name);
        let size_lit = proc_macro2::Literal::u32_unsuffixed(clamp_u32(*size));
        quote! { tir::backend::sched::BufferSize { name: #name_lit, size: #size_lit } }
    });

    let reg_file_lits = machine.reg_files.iter().map(|(name, count)| {
        let name_lit = proc_macro2::Literal::string(name);
        let count_lit = proc_macro2::Literal::u16_unsuffixed(clamp_u16(*count));
        quote! { tir::backend::sched::RegFile { name: #name_lit, count: #count_lit } }
    });

    let name_lit = proc_macro2::Literal::string(&machine.name);
    let issue_width_lit =
        proc_macro2::Literal::u16_unsuffixed(clamp_u16(machine.issue_width.unwrap_or(1).max(1)));
    let fn_ident = format_ident!("{}_model", to_snake_case(&machine.name));
    let frontend = frontend_ts(machine.frontend.as_ref());

    let fusion_lits: Vec<_> = machine
        .fusions
        .iter()
        .map(|fusion| -> Result<_, TMDLError> {
            let name = proc_macro2::Literal::string(&fusion.name);
            let steps: Vec<_> = fusion
                .steps
                .iter()
                .map(|step| {
                    let ops: Vec<_> = instructions
                        .iter()
                        .filter(|(instruction, _, mnemonic)| {
                            step.instruction_names.contains(instruction)
                                || step.mnemonics.contains(mnemonic)
                        })
                        .map(|(_, op, _)| proc_macro2::Literal::string(op))
                        .collect();
                    quote! { tir::backend::sched::FusionStep { ops: &[#(#ops),*] } }
                })
                .collect();
            let guard = match &fusion.condition {
                Some(condition) => fusion_guard_ts(condition, fusion)?,
                None => quote! { tir::backend::sched::FusionExpr::True },
            };
            let schedule = fusion_schedule_ts(&fusion.schedule, fusion, machine)?;
            Ok(quote! {
                tir::backend::sched::FusionPattern {
                    name: #name,
                    steps: &[#(#steps),*],
                    guard: #guard,
                    schedule: #schedule,
                }
            })
        })
        .collect::<Result<_, _>>()?;

    let screaming = to_snake_case(&machine.name).to_uppercase();
    let model_ident = format_ident!("{screaming}_MODEL");
    let model = quote! {
        static #model_ident: tir::backend::sched::MachineModel =
            tir::backend::sched::MachineModel {
                name: #name_lit,
                id: #id_lit,
                issue_width: #issue_width_lit,
                frontend: #frontend,
                resources: &[#(#resource_lits),*],
                buffers: &[#(#buffer_lits),*],
                pipeline: &[#(#pipeline_lits),*],
                forwards: &[#(#forward_lits),*],
                reg_files: &[#(#reg_file_lits),*],
                fusions: &[#(#fusion_lits),*],
            };

        pub fn #fn_ident() -> tir::backend::sched::MachineModel {
            #model_ident
        }
    };
    Ok((fn_ident, model))
}

fn fusion_step_index(fusion: &ast::FusionDecl, name: &str) -> Result<usize, TMDLError> {
    fusion
        .steps
        .iter()
        .position(|step| step.name == name)
        .ok_or_else(|| {
            TMDLError::Codegen(format!(
                "fusion '{}' references unknown step '{name}'",
                fusion.name
            ))
        })
}

fn fusion_int(expr: &ast::Expr) -> Result<i128, TMDLError> {
    let ast::Expr::Lit(ast::Lit::Int(value)) = expr else {
        return Err(TMDLError::Codegen(
            "fusion guard requires an integer literal".to_string(),
        ));
    };
    let spelling = value.value();
    let (radix, digits) = if let Some(digits) = spelling
        .strip_prefix("0x")
        .or_else(|| spelling.strip_prefix("0X"))
    {
        (16, digits)
    } else if let Some(digits) = spelling
        .strip_prefix("0b")
        .or_else(|| spelling.strip_prefix("0B"))
    {
        (2, digits)
    } else {
        (10, spelling)
    };
    i128::from_str_radix(digits, radix)
        .map_err(|_| TMDLError::Codegen(format!("invalid fusion integer '{spelling}'")))
}

fn fusion_step_arg(expr: &ast::Expr, fusion: &ast::FusionDecl) -> Result<usize, TMDLError> {
    let ast::Expr::Ident(step) = expr else {
        return Err(TMDLError::Codegen(
            "fusion guard requires a named step".to_string(),
        ));
    };
    fusion_step_index(fusion, &step.name)
}

fn fusion_operand_ref_ts(
    reference: &ast::FusionOperandRef,
    fusion: &ast::FusionDecl,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let step = proc_macro2::Literal::usize_unsuffixed(fusion_step_index(fusion, &reference.step)?);
    let name = proc_macro2::Literal::string(&reference.operand);
    Ok(quote! { tir::backend::sched::FusionOperandRef { step: #step, name: #name } })
}

fn fusion_value_ts(
    expr: &ast::Expr,
    fusion: &ast::FusionDecl,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    match expr {
        ast::Expr::Block(block) if block.last_expr_return && block.stmts.len() == 1 => {
            fusion_value_ts(&block.stmts[0], fusion)
        }
        ast::Expr::Lit(ast::Lit::Int(_)) => {
            let value = proc_macro2::Literal::i128_unsuffixed(fusion_int(expr)?);
            Ok(quote! { tir::backend::sched::FusionValue::Integer(#value) })
        }
        ast::Expr::Field(field) => {
            let ast::Expr::Ident(step) = field.base.as_ref() else {
                return Err(TMDLError::Codegen(
                    "fusion operand must be step.operand".to_string(),
                ));
            };
            let reference = fusion_operand_ref_ts(
                &ast::FusionOperandRef {
                    step: step.name.clone(),
                    operand: field.member.clone(),
                    span: field.span,
                },
                fusion,
            )?;
            Ok(quote! { tir::backend::sched::FusionValue::Operand(#reference) })
        }
        ast::Expr::Call(call) => {
            let name = match call.callee.as_ref() {
                ast::Expr::Ident(id) => id.name.as_str(),
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Width) => "width",
                ast::Expr::BuiltinFunction(ast::BuiltinFunction::Regnum) => "regnum",
                _ => {
                    return Err(TMDLError::Codegen(
                        "unsupported fusion value call".to_string(),
                    ));
                }
            };
            match (name, call.arguments.as_slice()) {
                ("pc" | "width", [step]) => {
                    let index =
                        proc_macro2::Literal::usize_unsuffixed(fusion_step_arg(step, fusion)?);
                    if name == "pc" {
                        Ok(quote! { tir::backend::sched::FusionValue::Pc(#index) })
                    } else {
                        Ok(quote! { tir::backend::sched::FusionValue::Width(#index) })
                    }
                }
                ("regnum" | "operand_width", [operand]) => {
                    let ast::Expr::Field(field) = operand else {
                        return Err(TMDLError::Codegen(format!(
                            "fusion {name} requires step.operand"
                        )));
                    };
                    let ast::Expr::Ident(step) = field.base.as_ref() else {
                        return Err(TMDLError::Codegen(format!(
                            "fusion {name} requires step.operand"
                        )));
                    };
                    let reference = fusion_operand_ref_ts(
                        &ast::FusionOperandRef {
                            step: step.name.clone(),
                            operand: field.member.clone(),
                            span: field.span,
                        },
                        fusion,
                    )?;
                    if name == "regnum" {
                        Ok(quote! { tir::backend::sched::FusionValue::Regnum(#reference) })
                    } else {
                        Ok(quote! { tir::backend::sched::FusionValue::OperandWidth(#reference) })
                    }
                }
                ("encoded_byte", [step, index]) => {
                    let step =
                        proc_macro2::Literal::usize_unsuffixed(fusion_step_arg(step, fusion)?);
                    let index = proc_macro2::Literal::usize_unsuffixed(
                        usize::try_from(fusion_int(index)?).map_err(|_| {
                            TMDLError::Codegen("fusion encoded_byte index out of range".to_string())
                        })?,
                    );
                    Ok(
                        quote! { tir::backend::sched::FusionValue::EncodedByte { step: #step, index: #index } },
                    )
                }
                _ => Err(TMDLError::Codegen(format!(
                    "unsupported fusion value call '{name}'"
                ))),
            }
        }
        ast::Expr::Binary(binary) => {
            let variant = match binary.op {
                ast::BinOp::Add => quote! { Add },
                ast::BinOp::Sub => quote! { Sub },
                ast::BinOp::Mul => quote! { Mul },
                ast::BinOp::Div => quote! { Div },
                _ => {
                    return Err(TMDLError::Codegen(
                        "unsupported fusion value operator".to_string(),
                    ));
                }
            };
            let lhs = fusion_value_ts(&binary.lhs, fusion)?;
            let rhs = fusion_value_ts(&binary.rhs, fusion)?;
            Ok(quote! { tir::backend::sched::FusionValue::#variant(&#lhs, &#rhs) })
        }
        _ => Err(TMDLError::Codegen(
            "unsupported fusion value expression".to_string(),
        )),
    }
}

fn fusion_guard_ts(
    expr: &ast::Expr,
    fusion: &ast::FusionDecl,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    match expr {
        ast::Expr::Block(block) if block.last_expr_return && block.stmts.len() == 1 => {
            fusion_guard_ts(&block.stmts[0], fusion)
        }
        ast::Expr::Ident(id) if id.name == "true" => {
            Ok(quote! { tir::backend::sched::FusionExpr::True })
        }
        ast::Expr::Ident(id) if id.name == "false" => Ok(
            quote! { tir::backend::sched::FusionExpr::Not(&tir::backend::sched::FusionExpr::True) },
        ),
        ast::Expr::Unary(unary) if unary.op == ast::UnOp::BitwiseNot => {
            let x = fusion_guard_ts(&unary.x, fusion)?;
            Ok(quote! { tir::backend::sched::FusionExpr::Not(&#x) })
        }
        ast::Expr::Binary(binary) => {
            let variant = match binary.op {
                ast::BinOp::BitwiseAnd => quote! { And },
                ast::BinOp::BitwiseOr => quote! { Or },
                ast::BinOp::Equal => quote! { Eq },
                ast::BinOp::NotEqual => quote! { Ne },
                ast::BinOp::LessThan => quote! { Lt },
                ast::BinOp::LessThenEqual => quote! { Le },
                ast::BinOp::GreaterThan => quote! { Gt },
                ast::BinOp::GreaterThanEqual => quote! { Ge },
                _ => {
                    return Err(TMDLError::Codegen(
                        "unsupported fusion guard operator".to_string(),
                    ));
                }
            };
            if matches!(binary.op, ast::BinOp::BitwiseAnd | ast::BinOp::BitwiseOr) {
                let lhs = fusion_guard_ts(&binary.lhs, fusion)?;
                let rhs = fusion_guard_ts(&binary.rhs, fusion)?;
                Ok(quote! { tir::backend::sched::FusionExpr::#variant(&[#lhs, #rhs]) })
            } else {
                let lhs = fusion_value_ts(&binary.lhs, fusion)?;
                let rhs = fusion_value_ts(&binary.rhs, fusion)?;
                Ok(quote! { tir::backend::sched::FusionExpr::#variant(#lhs, #rhs) })
            }
        }
        ast::Expr::Call(call) => {
            let ast::Expr::Ident(callee) = call.callee.as_ref() else {
                return Err(TMDLError::Codegen(
                    "unsupported fusion guard call".to_string(),
                ));
            };
            match (callee.name.as_str(), call.arguments.as_slice()) {
                ("same_register", [a, b]) | ("overlap_register", [a, b]) => {
                    let a = fusion_value_ts(a, fusion)?;
                    let b = fusion_value_ts(b, fusion)?;
                    if callee.name == "same_register" {
                        Ok(quote! { tir::backend::sched::FusionExpr::SameRegister(#a, #b) })
                    } else {
                        Ok(quote! { tir::backend::sched::FusionExpr::OverlapRegister(#a, #b) })
                    }
                }
                ("same_block", [a, b, bytes]) => {
                    let first = proc_macro2::Literal::usize_unsuffixed(fusion_step_arg(a, fusion)?);
                    let second =
                        proc_macro2::Literal::usize_unsuffixed(fusion_step_arg(b, fusion)?);
                    let bytes = proc_macro2::Literal::u64_unsuffixed(
                        u64::try_from(fusion_int(bytes)?).map_err(|_| {
                            TMDLError::Codegen("fusion block size out of range".to_string())
                        })?,
                    );
                    Ok(
                        quote! { tir::backend::sched::FusionExpr::SameBlock { first: #first, second: #second, bytes: #bytes } },
                    )
                }
                ("aligned", [step, bytes]) => {
                    let step =
                        proc_macro2::Literal::usize_unsuffixed(fusion_step_arg(step, fusion)?);
                    let bytes = proc_macro2::Literal::u64_unsuffixed(
                        u64::try_from(fusion_int(bytes)?).map_err(|_| {
                            TMDLError::Codegen("fusion alignment out of range".to_string())
                        })?,
                    );
                    Ok(
                        quote! { tir::backend::sched::FusionExpr::Aligned { step: #step, bytes: #bytes } },
                    )
                }
                _ => Err(TMDLError::Codegen(format!(
                    "unsupported fusion guard call '{}'",
                    callee.name
                ))),
            }
        }
        _ => Err(TMDLError::Codegen(
            "unsupported fusion guard expression".to_string(),
        )),
    }
}

fn fusion_selector_ts(
    selector: &ast::FusionOperandSelector,
    fusion: &ast::FusionDecl,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    Ok(match selector {
        ast::FusionOperandSelector::Operand(reference) => {
            let reference = fusion_operand_ref_ts(reference, fusion)?;
            quote! { tir::backend::sched::FusionOperandSelector::Operand(#reference) }
        }
        ast::FusionOperandSelector::AllInputs(step) => {
            let step = proc_macro2::Literal::usize_unsuffixed(fusion_step_index(fusion, step)?);
            quote! { tir::backend::sched::FusionOperandSelector::AllInputs(#step) }
        }
        ast::FusionOperandSelector::AllOutputs(step) => {
            let step = proc_macro2::Literal::usize_unsuffixed(fusion_step_index(fusion, step)?);
            quote! { tir::backend::sched::FusionOperandSelector::AllOutputs(#step) }
        }
    })
}

fn fusion_groups_ts(
    groups: &[ast::FusionStageGroup],
    fusion: &ast::FusionDecl,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let groups: Vec<_> = groups.iter().map(|group| -> Result<_, TMDLError> {
        let steps: Vec<_> = group.steps.iter().map(|step| {
            fusion_step_index(fusion, step).map(proc_macro2::Literal::usize_unsuffixed)
        }).collect::<Result<_, _>>()?;
        let slots = proc_macro2::Literal::u16_unsuffixed(clamp_u16(group.slots));
        Ok(quote! { tir::backend::sched::FusionStageGroup { steps: &[#(#steps),*], slots: #slots } })
    }).collect::<Result<_, _>>()?;
    Ok(quote! { &[#(#groups),*] })
}

fn fusion_schedule_ts(
    schedule: &ast::FusionSchedule,
    fusion: &ast::FusionDecl,
    machine: &ast::Machine,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let decode_uops = proc_macro2::Literal::u16_unsuffixed(clamp_u16(schedule.decode_uops));
    let decoded_cache_uops = proc_macro2::Literal::u16_unsuffixed(clamp_u16(
        schedule.decoded_cache_uops.unwrap_or(schedule.decode_uops),
    ));
    let decoder = match &schedule.decoder {
        Some(name) => {
            let name = proc_macro2::Literal::string(name);
            quote! { Some(#name) }
        }
        None => quote! { None },
    };
    let decode_cycles = proc_macro2::Literal::u16_unsuffixed(clamp_u16(schedule.decode_cycles));
    let rename_slots = proc_macro2::Literal::u16_unsuffixed(clamp_u16(schedule.rename_slots));
    let rob_slots = proc_macro2::Literal::u16_unsuffixed(clamp_u16(schedule.rob_entries));
    let retire_slots = proc_macro2::Literal::u16_unsuffixed(clamp_u16(schedule.retire_slots));
    let decode_groups = fusion_groups_ts(&schedule.decode_groups, fusion)?;
    let rename_groups = fusion_groups_ts(&schedule.rename_groups, fusion)?;
    let rob_groups = fusion_groups_ts(&schedule.rob_groups, fusion)?;
    let retire_groups = fusion_groups_ts(&schedule.retire_groups, fusion)?;
    let resource_groups: HashMap<&str, &ast::ResourceExpr> = machine
        .resource_groups
        .iter()
        .map(|group| (group.name.as_str(), &group.resources))
        .collect();
    let uops: Vec<_> = schedule.uops.iter().map(|uop| -> Result<_, TMDLError> {
        let name = proc_macro2::Literal::string(&uop.name);
        let routes: Vec<_> = uop.resources.as_ref().map(|resources| resolve_resource_expr(resources, &resource_groups, None))
            .unwrap_or_default().iter().map(|route| {
                let resources = route.iter().map(|use_| {
                    let resource = proc_macro2::Literal::string(&use_.resource);
                    let cycles = proc_macro2::Literal::u16_unsuffixed(use_.cycles);
                    quote! { tir::backend::sched::ResourceUse { resource: #resource, cycles: #cycles } }
                });
                quote! { tir::backend::sched::ResourceRoute { resources: &[#(#resources),*] } }
            }).collect();
        let inherit_routes = match &uop.inherit_routes {
            Some(step) => {
                let index = proc_macro2::Literal::usize_unsuffixed(fusion_step_index(fusion, step)?);
                quote! { Some(#index) }
            }
            None => quote! { None },
        };
        let inputs: Vec<_> = uop.inputs.iter().map(|selector| fusion_selector_ts(selector, fusion)).collect::<Result<_, _>>()?;
        let outputs: Vec<_> = uop.outputs.iter().map(|selector| fusion_selector_ts(selector, fusion)).collect::<Result<_, _>>()?;
        let depends_on: Vec<_> = uop.depends_on.iter().map(|name| proc_macro2::Literal::string(name)).collect();
        let read_cycle = proc_macro2::Literal::u16_unsuffixed(clamp_u16(uop.read_cycle));
        let write_cycle = proc_macro2::Literal::u16_unsuffixed(clamp_u16(uop.write_cycle));
        let memory: Vec<_> = uop.memory.iter().map(|reference| -> Result<_, TMDLError> {
            let step = proc_macro2::Literal::usize_unsuffixed(fusion_step_index(fusion, &reference.step)?);
            let index = match reference.index {
                Some(index) => {
                    let index = proc_macro2::Literal::usize_unsuffixed(usize::try_from(index).map_err(|_| TMDLError::Codegen("negative fusion memory index".to_string()))?);
                    quote! { Some(#index) }
                }
                None => quote! { None },
            };
            Ok(quote! { tir::backend::sched::FusionMemoryRef { step: #step, index: #index } })
        }).collect::<Result<_, _>>()?;
        let control_steps: Vec<_> = uop.control_steps.iter().map(|step| {
            fusion_step_index(fusion, step).map(proc_macro2::Literal::usize_unsuffixed)
        }).collect::<Result<_, _>>()?;
        Ok(quote! {
            tir::backend::sched::FusionMicroOp {
                name: #name,
                routes: &[#(#routes),*],
                inherit_routes: #inherit_routes,
                inputs: &[#(#inputs),*],
                outputs: &[#(#outputs),*],
                depends_on: &[#(#depends_on),*],
                read_cycle: #read_cycle,
                write_cycle: #write_cycle,
                memory: &[#(#memory),*],
                control_steps: &[#(#control_steps),*],
            }
        })
    }).collect::<Result<_, _>>()?;
    Ok(quote! {
        tir::backend::sched::FusionSchedule {
            decode_uops: #decode_uops,
            decoded_cache_uops: #decoded_cache_uops,
            decoder: #decoder,
            decode_cycles: #decode_cycles,
            rename_slots: #rename_slots,
            rob_slots: #rob_slots,
            retire_slots: #retire_slots,
            decode_groups: #decode_groups,
            rename_groups: #rename_groups,
            rob_groups: #rob_groups,
            retire_groups: #retire_groups,
            uops: &[#(#uops),*],
        }
    })
}

/// Emit one `static <MACHINE>_MODEL` (plus the `fn <machine>_model()` accessor that
/// returns it) per TMDL `machine` block, and resolve every instruction's `unit`
/// membership against each machine's `bind`s into a concrete scheduling class.
/// Machines are numbered, so an instruction reaches its class by indexing rather
/// than by looking its own name up in a per-machine table. These classes
/// feed static scheduling and provide the simulator fallback; captured
/// conditional latencies override that fallback during timing replay.
fn emit_machine_models<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Result<(proc_macro2::TokenStream, SchedTables), TMDLError> {
    let unit_defaults = collect_unit_defaults(files);
    let scheduled = collect_scheduled(files, item_cache);
    let all_instructions = collect_all_instruction_ops(files, item_cache);

    let mut model_fns = Vec::new();
    let mut lookup_arms = Vec::new();
    let mut machine_names = Vec::new();
    let mut class_pool = SchedClassPool::default();
    let machine_count = files.iter().flat_map(|file| file.machines()).count();
    let mut latency_cases = HashMap::new();
    // Per scheduled instruction, its class on each machine, in machine-id order.
    let mut per_instruction: Vec<Vec<proc_macro2::Ident>> = vec![Vec::new(); scheduled.len()];
    for (machine_id, machine) in files.iter().flat_map(|f| f.machines()).enumerate() {
        let resource_groups: HashMap<&str, &ast::ResourceExpr> = machine
            .resource_groups
            .iter()
            .map(|group| (group.name.as_str(), &group.resources))
            .collect();
        for override_ in &machine.overrides {
            if !override_.latency_cases.is_empty() {
                let cases = latency_cases
                    .entry(override_.instruction.clone())
                    .or_insert_with(|| vec![Vec::new(); machine_count]);
                cases[machine_id] = override_
                    .latency_cases
                    .iter()
                    .map(|case| ResolvedLatencyCase {
                        condition: case.condition.clone(),
                        latency: case.latency,
                        uops: (!case.uops.is_empty()).then(|| {
                            case.uops
                                .iter()
                                .flat_map(|uop| {
                                    (0..uop.count.max(0)).map(|_| ResolvedMicroOp {
                                        routes: resolve_resource_expr(
                                            &uop.resources,
                                            &resource_groups,
                                            None,
                                        ),
                                    })
                                })
                                .collect()
                        }),
                    })
                    .collect();
            }
        }
        let binds: HashMap<&str, &ast::UnitBind> =
            machine.binds.iter().map(|b| (b.unit.as_str(), b)).collect();
        let overrides: HashMap<&str, &ast::MachineOverride> = machine
            .overrides
            .iter()
            .map(|o| (o.instruction.as_str(), o))
            .collect();
        // Resolve each scheduled instruction to a concrete class on this machine. A
        // per-instruction `override` supersedes the `unit`-based resolution.
        let entries: Vec<ResolvedClass> = scheduled
            .iter()
            .map(
                |(name, _operation, _mnemonic, units)| match overrides.get(name.as_str()) {
                    Some(ov) => resolve_spec(
                        ExplicitTimingSpec {
                            reads: ov.reads.as_deref(),
                            writes: ov.writes.as_deref(),
                            latency: ov.latency,
                            throughput: ov.throughput,
                            uses: &ov.uses,
                            uops: &ov.uops,
                            decode_uops: ov.decode_uops,
                            decoder: ov.decoder.as_deref(),
                            decode_cycles: ov.decode_cycles,
                            eliminated: ov.eliminated,
                            zero_idiom: ov.zero_idiom,
                        },
                        &resource_groups,
                        &machine.pipeline,
                    ),
                    None => resolve_sched_class(
                        units,
                        &binds,
                        &unit_defaults,
                        &resource_groups,
                        &machine.pipeline,
                    ),
                },
            )
            .collect();
        for (index, class) in entries.iter().enumerate() {
            per_instruction[index].push(sched_class_ident(class, &mut class_pool));
        }
        let (fn_ident, model) = machine_model_ts(machine, machine_id, &all_instructions)?;
        model_fns.push(model);

        // Select by the machine name, and by its alias when one is declared, so
        // the tool-facing name lives in TMDL next to the machine.
        let mut keys = vec![machine.name.clone()];
        if let Some(alias) = &machine.alias {
            keys.push(alias.clone());
        }
        let machine_features = feature_slice(&machine.for_isas);
        let key_lits = keys.iter().map(|k| proc_macro2::Literal::string(k));
        let tool_name = proc_macro2::Literal::string(keys.last().unwrap());
        machine_names.push(quote! {
            if features_enabled(features, #machine_features) {
                names.push(#tool_name);
            }
        });
        lookup_arms.push(quote! {
            #(#key_lits)|* => features_enabled(features, #machine_features).then(#fn_ident)
        });
    }

    let tables = SchedTables {
        latency_cases,
        costs: instruction_costs(files, item_cache),
        classes: scheduled
            .iter()
            .map(|(name, ..)| name.clone())
            .zip(per_instruction)
            .collect(),
    };

    // A target with no `machine` models (e.g. a text-only pseudo-ISA) gets
    // trivial accessors so `features` and `names` are not spuriously unused.
    if machine_names.is_empty() {
        return Ok((
            quote! {
                /// No machine models are declared for this target.
                pub fn machine_model(_name: &str, _features: &[Feature]) -> Option<tir::backend::sched::MachineModel> {
                    None
                }

                /// No machine models are declared for this target.
                pub fn machines(_features: &[Feature]) -> Vec<&'static str> {
                    Vec::new()
                }
            },
            tables,
        ));
    }

    let class_defs = class_pool.classes.iter().enumerate().map(|(i, class)| {
        let ident = format_ident!("SCHED_C{i}");
        let body = sched_class_ts(class);
        quote! { const #ident: tir::backend::sched::InstrSchedClass = #body; }
    });

    Ok((
        quote! {
            #(#class_defs)*

            #(#model_fns)*

            /// Resolve a machine by its TMDL name or alias. `None` when the name is
            /// unknown or the machine's `for [...]` clause is disjoint from `features`.
            pub fn machine_model(name: &str, features: &[Feature]) -> Option<tir::backend::sched::MachineModel> {
                match name {
                    #(#lookup_arms,)*
                    _ => None,
                }
            }

            /// Tool-facing names (alias preferred) of the machines compatible with `features`.
            pub fn machines(features: &[Feature]) -> Vec<&'static str> {
                let mut names = Vec::new();
                #(#machine_names)*
                names
            }
        },
        tables,
    ))
}

/// Distinct scheduling classes across all machines, each emitted once as a
/// `SCHED_C<n>` constant. Instruction tables reference these instead of repeating
/// the struct literal: the overwhelming majority of an ISA's instructions resolve
/// to one of a few dozen classes, so this keeps the generated file (and rustc's
/// work on it) proportional to the number of distinct classes, not instructions.
#[derive(Default)]
struct SchedClassPool {
    /// First-use order, which drives constant numbering and so must stay a `Vec`
    /// for reproducible output.
    classes: Vec<ResolvedClass>,
    index: HashMap<ResolvedClass, usize>,
}

fn sched_class_ident(class: &ResolvedClass, pool: &mut SchedClassPool) -> proc_macro2::Ident {
    let next = pool.classes.len();
    let index = *pool.index.entry(class.clone()).or_insert_with(|| {
        pool.classes.push(class.clone());
        next
    });
    format_ident!("SCHED_C{index}")
}

fn sched_class_ts(c: &ResolvedClass) -> proc_macro2::TokenStream {
    let lat_lit = proc_macro2::Literal::u16_unsuffixed(c.latency);
    let read_lit = proc_macro2::Literal::u16_unsuffixed(c.read_cycle);
    let rthr_lit = proc_macro2::Literal::u16_unsuffixed(c.rthroughput);
    let res_lits = c.resources.iter().map(|r| proc_macro2::Literal::string(r));
    let uop_lits = emit_uops(&c.uops);
    let decode_uops = proc_macro2::Literal::u16_unsuffixed(c.decode_uops);
    let decoder = match &c.decoder {
        Some(decoder) => {
            let decoder = proc_macro2::Literal::string(decoder);
            quote! { Some(#decoder) }
        }
        None => quote! { None },
    };
    let decode_cycles = proc_macro2::Literal::u16_unsuffixed(c.decode_cycles);
    let eliminated = c.eliminated;
    let zero_idiom = c.zero_idiom;
    quote! {
        tir::backend::sched::InstrSchedClass {
            latency: #lat_lit,
            read_cycle: #read_lit,
            rthroughput: #rthr_lit,
            resources: &[#(#res_lits),*],
            uops: &[#(#uop_lits),*],
            decode_uops: #decode_uops,
            decoder: #decoder,
            decode_cycles: #decode_cycles,
            eliminated: #eliminated,
            zero_idiom: #zero_idiom,
        }
    }
}

fn emit_uops(uops: &[ResolvedMicroOp]) -> Vec<proc_macro2::TokenStream> {
    uops.iter()
        .map(|uop| {
            let route_lits = uop.routes.iter().map(|route| {
                let use_lits = route.iter().map(|use_| {
                    let resource = proc_macro2::Literal::string(&use_.resource);
                    let cycles = proc_macro2::Literal::u16_unsuffixed(use_.cycles);
                    quote! {
                        tir::backend::sched::ResourceUse {
                            resource: #resource,
                            cycles: #cycles,
                        }
                    }
                });
                quote! {
                    tir::backend::sched::ResourceRoute {
                        resources: &[#(#use_lits),*],
                    }
                }
            });
            quote! {
                tir::backend::sched::MicroOp {
                    routes: &[#(#route_lits),*],
                }
            }
        })
        .collect()
}

/// Resource-agnostic `unit` defaults, keyed by name. Used both when a machine
/// does not bind a unit and to drive the machine-independent [`instruction_costs`].
fn collect_unit_defaults(files: &[ast::File]) -> HashMap<&str, &ast::SchedClassDecl> {
    files
        .iter()
        .flat_map(|f| f.count())
        .map(|u| (u.name.as_str(), u))
        .collect()
}

/// `(instruction name, operation identity, mnemonic, units)` for every
/// instruction carrying a `schedule` block. The declaration name keys
/// per-instruction machine `override`s; operation identity keys the runtime
/// scheduling table; mnemonic remains the machine-independent cost key.
fn collect_all_instruction_ops<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, String, String)> {
    files
        .iter()
        .flat_map(|file| file.instructions())
        .filter_map(|inst| {
            let params = resolve_params_for_instruction(inst, item_cache);
            let operation = params
                .get("OPNAME")
                .and_then(|(_, value)| value.as_ref())
                .and_then(resolve_string)
                .or_else(|| {
                    params
                        .get("MNEMONIC")
                        .and_then(|(_, value)| value.as_ref())
                        .and_then(resolve_string)
                })?;
            let mnemonic = params
                .get("MNEMONIC")
                .and_then(|(_, value)| value.as_ref())
                .and_then(resolve_string)
                .unwrap_or_else(|| operation.clone());
            Some((inst.name.clone(), operation, mnemonic))
        })
        .collect()
}

fn collect_scheduled<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> Vec<(String, String, String, Vec<String>)> {
    let mut scheduled = Vec::new();
    for inst in files.iter().flat_map(|f| f.instructions()) {
        let schedule = crate::utils::resolve_effective_schedule_for_instruction(inst, item_cache);
        if schedule.is_none()
            && !files
                .iter()
                .flat_map(|file| file.machines())
                .any(|machine| {
                    machine
                        .overrides
                        .iter()
                        .any(|override_| override_.instruction == inst.name)
                })
        {
            continue;
        }
        let resolved_params = resolve_params_for_instruction(inst, item_cache);
        let operation = resolved_params
            .get("OPNAME")
            .and_then(|(_, v)| v.as_ref())
            .and_then(resolve_string)
            .or_else(|| {
                resolved_params
                    .get("MNEMONIC")
                    .and_then(|(_, v)| v.as_ref())
                    .and_then(resolve_string)
            });
        let Some(operation) = operation else {
            continue;
        };
        let mnemonic = resolved_params
            .get("MNEMONIC")
            .and_then(|(_, v)| v.as_ref())
            .and_then(resolve_string)
            .unwrap_or_else(|| operation.clone());
        scheduled.push((
            inst.name.clone(),
            operation,
            mnemonic,
            schedule
                .map(|schedule| schedule.classes.clone())
                .unwrap_or_default(),
        ));
    }
    scheduled
}

/// Per-instruction scheduling facts resolved once from the TMDL `machine` blocks,
/// for the `InstrInfo` records [`emit_instructions`] emits. Keyed by instruction
/// declaration name.
struct SchedTables {
    latency_cases: HashMap<String, Vec<Vec<ResolvedLatencyCase>>>,
    costs: HashMap<String, u32>,
    /// The `SCHED_C*` class of each instruction on each machine, in machine-id
    /// order. Absent for an instruction no machine describes.
    classes: HashMap<String, Vec<proc_macro2::Ident>>,
}

#[derive(Clone)]
struct ResolvedLatencyCase {
    condition: ast::Expr,
    latency: i64,
    uops: Option<Vec<ResolvedMicroOp>>,
}

impl SchedTables {
    fn cost(&self, inst: &str) -> u32 {
        self.costs.get(inst).copied().unwrap_or(1)
    }

    /// The `InstrInfo::sched` initializer for one instruction: its class on every
    /// machine. `None` when no machine describes it, leaving the empty table
    /// `InstrInfo::BASE` carries.
    fn sched(&self, inst: &str) -> Option<proc_macro2::TokenStream> {
        let classes = self.classes.get(inst)?;
        Some(quote! { &[#(#classes),*] })
    }
}

/// Machine-independent cost per instruction declaration, derived from its `unit`
/// defaults (latency, falling back to 1). Instruction selection uses this cost
/// independently of machine overrides and conditional simulator latencies.
fn instruction_costs<'a>(
    files: &'a [ast::File],
    item_cache: &HashMap<&'a str, &'a ast::Item>,
) -> HashMap<String, u32> {
    let unit_defaults = collect_unit_defaults(files);
    let empty_binds: HashMap<&str, &ast::UnitBind> = HashMap::new();
    let empty_groups: HashMap<&str, &ast::ResourceExpr> = HashMap::new();
    collect_scheduled(files, item_cache)
        .into_iter()
        .map(|(name, _, _, units)| {
            // Machine-independent: no machine binds and no pipeline, so this
            // resolves through the unit defaults to a scalar latency.
            let resolved =
                resolve_sched_class(&units, &empty_binds, &unit_defaults, &empty_groups, &[]);
            (name, u32::from(resolved.latency))
        })
        .collect()
}

fn phase_cycle(pipeline: &[ast::PipelinePhase], name: &str) -> Option<u16> {
    pipeline
        .iter()
        .position(|p| p.name == name)
        .map(|i| i as u16)
}

/// The resolved scheduling cost of an instruction on one machine.
#[derive(Clone, PartialEq, Eq, Hash)]
struct ResolvedClass {
    latency: u16,
    read_cycle: u16,
    rthroughput: u16,
    resources: Vec<String>,
    uops: Vec<ResolvedMicroOp>,
    decode_uops: u16,
    decoder: Option<String>,
    decode_cycles: u16,
    eliminated: bool,
    zero_idiom: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ResolvedMicroOp {
    routes: Vec<Vec<ResolvedResourceUse>>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ResolvedResourceUse {
    resource: String,
    cycles: u16,
}

struct ExplicitTimingSpec<'a> {
    reads: Option<&'a str>,
    writes: Option<&'a str>,
    latency: Option<i64>,
    throughput: Option<i64>,
    uses: &'a [String],
    uops: &'a [ast::MicroOp],
    decode_uops: Option<i64>,
    decoder: Option<&'a str>,
    decode_cycles: Option<i64>,
    eliminated: Option<bool>,
    zero_idiom: Option<bool>,
}

/// Resolve one explicit timing spec (a `bind` or an `override`) to a class. Timing
/// is phase-based when it names `reads`/`writes` phases (cycles from the machine's
/// pipeline), else scalar (`latency = N` ≡ read at cycle 0, write at cycle N).
fn resolve_spec(
    spec: ExplicitTimingSpec<'_>,
    resource_groups: &HashMap<&str, &ast::ResourceExpr>,
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let (rc, wc) = if spec.reads.is_some() || spec.writes.is_some() {
        let rc = spec
            .reads
            .and_then(|p| phase_cycle(pipeline, p))
            .unwrap_or(0);
        let wc = spec
            .writes
            .and_then(|p| phase_cycle(pipeline, p))
            .unwrap_or_else(|| rc.saturating_add(clamp_u16(spec.latency.unwrap_or(1))));
        (rc, wc.max(rc))
    } else {
        (0, clamp_u16(spec.latency.unwrap_or(1)))
    };
    let resolved_uops: Vec<_> = spec
        .uops
        .iter()
        .flat_map(|uop| {
            (0..uop.count.max(0)).map(|_| ResolvedMicroOp {
                routes: resolve_resource_expr(&uop.resources, resource_groups, None),
            })
        })
        .collect();
    ResolvedClass {
        latency: wc.saturating_sub(rc).max(1),
        read_cycle: rc,
        rthroughput: clamp_u16(spec.throughput.unwrap_or(1)).max(1),
        resources: spec.uses.to_vec(),
        decode_uops: clamp_u16(
            spec.decode_uops
                .unwrap_or(resolved_uops.len().max(1) as i64),
        )
        .max(1),
        decoder: spec.decoder.map(str::to_string),
        decode_cycles: clamp_u16(spec.decode_cycles.unwrap_or(1)).max(1),
        eliminated: spec.eliminated.unwrap_or(false),
        zero_idiom: spec.zero_idiom.unwrap_or(false),
        uops: resolved_uops,
    }
}

/// Resolve an instruction's `unit` membership to a concrete class on one machine.
/// Precedence per unit: the machine's `bind` → the unit's resource-agnostic default
/// → the built-in `(latency 1, read 0)`. Across multiple units the result aggregates
/// conservatively: the highest-latency unit sets the latency/read-cycle, throughput
/// is the max, resources are unioned.
fn resolve_sched_class(
    units: &[String],
    binds: &HashMap<&str, &ast::UnitBind>,
    unit_defaults: &HashMap<&str, &ast::SchedClassDecl>,
    resource_groups: &HashMap<&str, &ast::ResourceExpr>,
    pipeline: &[ast::PipelinePhase],
) -> ResolvedClass {
    let mut latency: u16 = 0;
    let mut read_cycle: u16 = 0;
    let mut rthroughput: u16 = 0;
    let mut resources: Vec<String> = Vec::new();
    let mut uops = Vec::new();
    let mut decode_uops = 0u16;
    let mut decoder = None;
    let mut decode_cycles = 0u16;
    let mut eliminated = false;
    let mut zero_idiom = false;
    let mut chosen = false;

    for unit in units {
        let class = if let Some(b) = binds.get(unit.as_str()) {
            resolve_spec(
                ExplicitTimingSpec {
                    reads: b.reads.as_deref(),
                    writes: b.writes.as_deref(),
                    latency: b.latency,
                    throughput: b.throughput,
                    uses: &b.uses,
                    uops: &b.uops,
                    decode_uops: b.decode_uops,
                    decoder: b.decoder.as_deref(),
                    decode_cycles: b.decode_cycles,
                    eliminated: b.eliminated,
                    zero_idiom: b.zero_idiom,
                },
                resource_groups,
                pipeline,
            )
        } else if let Some(d) = unit_defaults.get(unit.as_str()) {
            ResolvedClass {
                latency: clamp_u16(d.default_latency.unwrap_or(1)).max(1),
                read_cycle: 0,
                rthroughput: clamp_u16(d.default_throughput.unwrap_or(1)).max(1),
                resources: Vec::new(),
                uops: Vec::new(),
                decode_uops: 1,
                decoder: None,
                decode_cycles: 1,
                eliminated: false,
                zero_idiom: false,
            }
        } else {
            ResolvedClass {
                latency: 1,
                read_cycle: 0,
                rthroughput: 1,
                resources: Vec::new(),
                uops: Vec::new(),
                decode_uops: 1,
                decoder: None,
                decode_cycles: 1,
                eliminated: false,
                zero_idiom: false,
            }
        };

        for r in &class.resources {
            if !resources.iter().any(|e| e == r) {
                resources.push(r.clone());
            }
        }
        uops.extend(class.uops);
        decode_uops = decode_uops.saturating_add(class.decode_uops);
        if decoder.is_none() {
            decoder = class.decoder;
        }
        decode_cycles = decode_cycles.max(class.decode_cycles);
        eliminated |= class.eliminated;
        zero_idiom |= class.zero_idiom;
        if !chosen || class.latency > latency {
            latency = class.latency;
            read_cycle = class.read_cycle;
            chosen = true;
        }
        rthroughput = rthroughput.max(class.rthroughput);
    }

    ResolvedClass {
        latency: latency.max(1),
        read_cycle,
        rthroughput: rthroughput.max(1),
        resources,
        uops,
        decode_uops: decode_uops.max(1),
        decoder,
        decode_cycles: decode_cycles.max(1),
        eliminated,
        zero_idiom,
    }
}

fn frontend_ts(frontend: Option<&ast::Frontend>) -> proc_macro2::TokenStream {
    let Some(frontend) = frontend else {
        return quote! { None };
    };
    let bytes_per_cycle =
        proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.fetch.bytes_per_cycle));
    let window_bytes = proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.fetch.window_bytes));
    let alignment = proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.fetch.alignment));
    let queue_bytes = proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.fetch.queue_bytes));
    let slots = frontend
        .decode
        .slots
        .iter()
        .map(|slot| proc_macro2::Literal::string(slot));
    let uops_per_cycle =
        proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.decode.uops_per_cycle));
    let queue_uops = proc_macro2::Literal::u16_unsuffixed(clamp_u16(frontend.decode.queue_uops));
    let decoders = frontend.decode.decoders.iter().map(|decoder| {
        let name = proc_macro2::Literal::string(&decoder.name);
        let max_uops =
            proc_macro2::Literal::u16_unsuffixed(clamp_u16(decoder.max_uops_per_instruction));
        quote! {
            tir::backend::sched::Decoder {
                name: #name,
                max_uops_per_instruction: #max_uops,
            }
        }
    });
    let decoded_cache = match &frontend.decoded_cache {
        Some(cache) => {
            let sets = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.sets));
            let ways = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.ways));
            let line_bytes = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.line_bytes));
            let line_uops = proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.line_uops));
            let deliver =
                proc_macro2::Literal::u16_unsuffixed(clamp_u16(cache.deliver_uops_per_cycle));
            quote! {
                Some(tir::backend::sched::DecodedCache {
                    sets: #sets,
                    ways: #ways,
                    line_bytes: #line_bytes,
                    line_uops: #line_uops,
                    deliver_uops_per_cycle: #deliver,
                })
            }
        }
        None => quote! { None },
    };
    quote! {
        Some(tir::backend::sched::Frontend {
            fetch: tir::backend::sched::FrontendFetch {
                bytes_per_cycle: #bytes_per_cycle,
                window_bytes: #window_bytes,
                alignment: #alignment,
                queue_bytes: #queue_bytes,
            },
            decode: tir::backend::sched::FrontendDecode {
                slots: &[#(#slots),*],
                uops_per_cycle: #uops_per_cycle,
                queue_uops: #queue_uops,
                decoders: &[#(#decoders),*],
            },
            decoded_cache: #decoded_cache,
        })
    }
}

fn resolve_resource_expr(
    expr: &ast::ResourceExpr,
    groups: &HashMap<&str, &ast::ResourceExpr>,
    occupancy: Option<u16>,
) -> Vec<Vec<ResolvedResourceUse>> {
    match expr {
        ast::ResourceExpr::Resource(name) => match groups.get(name.as_str()) {
            Some(group) => resolve_resource_expr(group, groups, occupancy),
            None => vec![vec![ResolvedResourceUse {
                resource: name.clone(),
                cycles: occupancy.unwrap_or(1).max(1),
            }]],
        },
        ast::ResourceExpr::Any(resources) => resources
            .iter()
            .flat_map(|resource| resolve_resource_expr(resource, groups, occupancy))
            .collect(),
        ast::ResourceExpr::All(resources) => {
            resources.iter().fold(vec![Vec::new()], |routes, resource| {
                let rhs = resolve_resource_expr(resource, groups, occupancy);
                routes
                    .into_iter()
                    .flat_map(|lhs| {
                        rhs.iter().map(move |right| {
                            let mut route = lhs.clone();
                            route.extend(right.iter().cloned());
                            route
                        })
                    })
                    .collect()
            })
        }
        ast::ResourceExpr::Occupied { resource, cycles } => resolve_resource_expr(
            resource,
            groups,
            occupancy.or(Some(clamp_u16(*cycles).max(1))),
        ),
    }
}

/// The `tir::backend::sched::Protection` variant for an AST protection mode.
fn protection_ts(p: ast::Protection) -> proc_macro2::TokenStream {
    match p {
        ast::Protection::Protected => quote! { tir::backend::sched::Protection::Protected },
        ast::Protection::Unprotected => quote! { tir::backend::sched::Protection::Unprotected },
        ast::Protection::Hard => quote! { tir::backend::sched::Protection::Hard },
    }
}

fn clamp_u16(v: i64) -> u16 {
    v.clamp(0, u16::MAX as i64) as u16
}

fn clamp_u32(v: i64) -> u32 {
    v.clamp(0, u32::MAX as i64) as u32
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i != 0 && !out.ends_with('_') {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}
