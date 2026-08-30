fn abi_kind(kind: ast::AbiValueKind) -> proc_macro2::TokenStream {
    match kind {
        ast::AbiValueKind::Int => quote! { tir::backend::abi::ValueKind::Int },
        ast::AbiValueKind::Float => quote! { tir::backend::abi::ValueKind::Float },
        ast::AbiValueKind::Vector => quote! { tir::backend::abi::ValueKind::Vector },
    }
}

fn abi_register(
    register: &ast::AbiRegister,
    classes: &HashMap<String, &ast::RegisterClass>,
) -> Result<(proc_macro2::TokenStream, String, u16), TMDLError> {
    let class = classes.get(&register.class).ok_or_else(|| {
        TMDLError::Codegen(format!(
            "unknown ABI register class '{}'",
            register.class
        ))
    })?;
    let index = class
        .resolve_registers()
        .find(|candidate| candidate.name == register.name)
        .and_then(|candidate| candidate.encoding_index())
        .ok_or_else(|| {
            TMDLError::Codegen(format!(
                "unknown ABI register '{}::{}'",
                register.class, register.name
            ))
        })?;
    let class_id = reg_class_id(&register.class);
    Ok((quote! { (#class_id, #index) }, class.register_file(classes).to_string(), index))
}

fn abi_registers(
    sequences: &[ast::AbiRegisterSequence],
    classes: &HashMap<String, &ast::RegisterClass>,
) -> Result<Vec<(proc_macro2::TokenStream, String, u16)>, TMDLError> {
    let mut result = Vec::new();
    for sequence in sequences {
        let (start, file, start_index) = abi_register(&sequence.start, classes)?;
        let Some(end) = &sequence.end else {
            result.push((start, file, start_index));
            continue;
        };
        let (_, _, end_index) = abi_register(end, classes)?;
        let class = classes.get(&sequence.start.class).unwrap();
        let class_id = reg_class_id(&sequence.start.class);
        let mut indices = class
            .resolve_registers()
            .filter_map(|register| register.encoding_index())
            .filter(|index| (start_index..=end_index).contains(index))
            .collect::<Vec<_>>();
        indices.sort_unstable();
        indices.dedup();
        for index in indices {
            result.push((quote! { (#class_id, #index) }, file.clone(), index));
        }
    }
    Ok(result)
}

fn abi_expr_value(
    expr: &ast::Expr,
    abi: &ast::Abi,
    item_cache: &HashMap<&str, &ast::Item>,
) -> Result<u32, TMDLError> {
    let mut params = HashMap::new();
    for isa in &abi.for_isas {
        for (name, value) in crate::utils::isa_param_values(isa, item_cache) {
            params
                .entry(name)
                .and_modify(|current: &mut i64| *current = (*current).max(value))
                .or_insert(value);
        }
    }
    let mut pending = abi.parameters.iter().collect::<Vec<_>>();
    while !pending.is_empty() {
        let mut next = Vec::new();
        let mut progress = false;
        for (name, (_, value)) in pending {
            let Some(value) = value else {
                continue;
            };
            if let Some(value) = crate::utils::eval_bits_width(value, &params) {
                params.insert(name.clone(), i64::from(value));
                progress = true;
            } else {
                next.push((name, &abi.parameters[name]));
            }
        }
        if !progress {
            break;
        }
        pending = next;
    }
    crate::utils::eval_bits_width(expr, &params)
        .map(u32::from)
        .ok_or_else(|| TMDLError::Codegen(format!("ABI '{}' stack expression is not constant", abi.name)))
}

fn emit_abi_info(
    files: &[ast::File],
    item_cache: &HashMap<&str, &ast::Item>,
) -> Result<proc_macro2::TokenStream, TMDLError> {
    let classes: HashMap<String, &ast::RegisterClass> = files
        .iter()
        .flat_map(|file| file.register_classes())
        .map(|class| (class.name.clone(), class))
        .collect();
    let mut entries = Vec::new();

    for abi in files.iter().flat_map(|file| file.abis()) {
        let name = abi.alias.as_deref().unwrap_or(&abi.name);
        let stack = abi.stack.as_ref().ok_or_else(|| {
            TMDLError::Codegen(format!("ABI '{}' does not declare a stack layout", abi.name))
        })?;
        let align = abi_expr_value(stack.align.as_ref().ok_or_else(|| {
            TMDLError::Codegen(format!("ABI '{}' stack has no alignment", abi.name))
        })?, abi, item_cache)?;
        let slot_size = abi_expr_value(stack.slot_size.as_ref().ok_or_else(|| {
            TMDLError::Codegen(format!("ABI '{}' stack has no slot size", abi.name))
        })?, abi, item_cache)?;
        let red_zone = abi_expr_value(stack.red_zone.as_ref().ok_or_else(|| {
            TMDLError::Codegen(format!("ABI '{}' stack has no red zone", abi.name))
        })?, abi, item_cache)?;
        let grows_down = match stack.grows {
            Some(ast::AbiStackGrowth::Down) => true,
            Some(ast::AbiStackGrowth::Up) => false,
            None => {
                return Err(TMDLError::Codegen(format!(
                    "ABI '{}' stack has no growth direction",
                    abi.name
                )));
            }
        };
        let save_style = match stack.save_style.unwrap_or(ast::AbiSaveStyle::FrameSlots) {
            ast::AbiSaveStyle::FrameSlots => quote! { tir::backend::abi::SaveStyle::FrameSlots },
            ast::AbiSaveStyle::PushPop => quote! { tir::backend::abi::SaveStyle::PushPop },
        };

        let role = |name: &str| -> Result<Option<(proc_macro2::TokenStream, String, u16)>, TMDLError> {
            abi.roles
                .iter()
                .find(|role| role.name == name)
                .map(|role| abi_register(&role.register, &classes))
                .transpose()
        };
        let sp = role("sp")?.ok_or_else(|| {
            TMDLError::Codegen(format!("ABI '{}' does not declare sp", abi.name))
        })?;
        let ra = role("ra")?;
        let fp = role("fp")?;

        let pass_entries = |passes: &[ast::AbiPassSequence]| -> Result<Vec<_>, TMDLError> {
            passes
                .iter()
                .map(|pass| {
                    let kind = abi_kind(pass.kind);
                    let regs = abi_registers(&pass.registers, &classes)?
                        .into_iter()
                        .map(|(register, _, _)| register);
                    let overflow = match pass.overflow.unwrap_or(ast::AbiOverflow::Stack) {
                        ast::AbiOverflow::Stack => quote! { tir::backend::abi::Overflow::Stack },
                        ast::AbiOverflow::Kind(kind) => {
                            let kind = abi_kind(kind);
                            quote! { tir::backend::abi::Overflow::Chain(#kind) }
                        }
                    };
                    Ok(quote! {
                        tir::backend::abi::PassSeq {
                            kind: #kind,
                            regs: &[#(#regs),*],
                            overflow: #overflow,
                        }
                    })
                })
                .collect()
        };
        let args = pass_entries(&abi.args)?;
        let rets = pass_entries(&abi.rets)?;
        let callee_saved = abi_registers(
            abi.callee_saved.as_deref().unwrap_or_default(),
            &classes,
        )?;
        let reserved = abi_registers(abi.reserved.as_deref().unwrap_or_default(), &classes)?;

        let mut excluded = HashSet::new();
        excluded.extend(callee_saved.iter().map(|(_, file, index)| (file.clone(), *index)));
        excluded.extend(reserved.iter().map(|(_, file, index)| (file.clone(), *index)));
        excluded.insert((sp.1.clone(), sp.2));
        if let Some((_, file, index)) = &ra {
            excluded.insert((file.clone(), *index));
        }
        if let Some((_, file, index)) = &fp {
            excluded.insert((file.clone(), *index));
        }
        let mut caller_saved = Vec::new();
        let mut seen = HashSet::new();
        for class in files.iter().flat_map(|file| file.register_classes()) {
            let class_id = reg_class_id(&class.name);
            let file = class.register_file(&classes).to_string();
            // A status flag carries no encoding slot and is never allocated, so
            // it is not one of `indexed_registers`; a call destroys it all the
            // same — the flags a compare left behind do not survive the callee,
            // and that is the edge that keeps a call out from between a compare
            // and the branch reading it. Its index is its position in the class,
            // which is how the implicit-register facts name it.
            for (position, register) in class.resolve_registers().enumerate() {
                let index = match register.encoding_index() {
                    Some(index) => index,
                    None if register.traits.contains(&ast::RegisterTrait::StatusFlag) => {
                        position as u16
                    }
                    None => continue,
                };
                let identity = (file.clone(), index);
                if !excluded.contains(&identity) && seen.insert(identity) {
                    caller_saved.push(quote! { (#class_id, #index) });
                }
            }
        }

        let callee_saved = callee_saved.into_iter().map(|(register, _, _)| register);
        let reserved = reserved.into_iter().map(|(register, _, _)| register);
        let sp = sp.0;
        let ra = match ra {
            Some((register, _, _)) => quote! { Some(#register) },
            None => quote! { None },
        };
        let fp = match fp {
            Some((register, _, _)) => quote! { Some(#register) },
            None => quote! { None },
        };
        let classifier = match abi.classifier.as_deref() {
            Some("riscv") => quote! { tir::backend::abi::ClassifierKind::Riscv },
            Some("aapcs64") => quote! { tir::backend::abi::ClassifierKind::Aapcs64 },
            Some("sysv") => quote! { tir::backend::abi::ClassifierKind::Sysv },
            _ => {
                return Err(TMDLError::Codegen(format!(
                    "ABI '{}' has no known classifier",
                    abi.name
                )));
            }
        };

        entries.push(quote! {
            tir::backend::abi::AbiInfo {
                name: #name,
                stack: tir::backend::abi::StackLayout {
                    align: #align,
                    slot_size: #slot_size,
                    red_zone: #red_zone,
                    grows_down: #grows_down,
                    save_style: #save_style,
                },
                sp: #sp,
                ra: #ra,
                fp: #fp,
                indirect_result: None,
                argument_group_alignment: None,
                argument_group_policy: None,
                args: &[#(#args),*],
                rets: &[#(#rets),*],
                callee_saved: &[#(#callee_saved),*],
                caller_saved: &[#(#caller_saved),*],
                reserved: &[#(#reserved),*],
                classifier: #classifier,
            }
        });
    }

    let count = entries.len();
    let default_abi = if count == 0 {
        quote! { panic!("target declares no ABI") }
    } else {
        quote! { &ABIS[0] }
    };
    Ok(quote! {
        static ABIS: [tir::backend::abi::AbiInfo; #count] = [#(#entries),*];

        pub fn abis() -> &'static [tir::backend::abi::AbiInfo] {
            &ABIS
        }

        pub fn default_abi() -> &'static tir::backend::abi::AbiInfo {
            #default_abi
        }

        pub fn abi_by_name(name: &str) -> Option<&'static tir::backend::abi::AbiInfo> {
            ABIS.iter().find(|abi| abi.name.eq_ignore_ascii_case(name))
        }
    })
}
