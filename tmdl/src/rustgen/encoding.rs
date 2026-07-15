// ---------------------------------------------------------------------------

/// One contiguous run of an integer operand's bits placed into the encoded
/// word: operand bits `[op_lo, op_lo + width)` land at word bits
/// `[word_lo, word_lo + width)`.
struct IntField {
    op_lo: u16,
    word_lo: u16,
    width: u16,
}

fn encoding_mask(width: u16) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

/// Compile an instruction's encoding arms into an `encode_*_inst` function
/// (and, when the encoding has exactly one immediate operand of known width,
/// a `patch_*_inst` function that re-scatters a resolved fixup value).
/// Returns `None` when the instruction has no encoding.
fn emit_instruction_encoder(
    inst: &ast::Instruction,
    encoding_arms: &[ast::EncodingArm],
    ops_map: &HashMap<String, Type>,
    resolved_params: &HashMap<String, (Type, Option<ast::Expr>)>,
    width_bytes: u64,
) -> Result<Option<(proc_macro2::TokenStream, Option<proc_macro2::TokenStream>)>, TMDLError> {
    if encoding_arms.is_empty() {
        return Ok(None);
    }
    if width_bytes > 16 {
        return Err(TMDLError::Codegen(format!(
            "instruction '{}': encodings wider than 128 bits are not supported",
            inst.name
        )));
    }

    let mut const_word: u128 = 0;
    // Insertion-ordered so generated code is stable across runs.
    let mut reg_fields: Vec<(String, Vec<IntField>)> = Vec::new();
    let mut int_fields: Vec<(String, Vec<IntField>)> = Vec::new();

    let push_field = |dst: &mut Vec<(String, Vec<IntField>)>, name: &str, field: IntField| match dst
        .iter_mut()
        .find(|(n, _)| n == name)
    {
        Some((_, fields)) => fields.push(field),
        None => dst.push((name.to_string(), vec![field])),
    };

    for arm in encoding_arms {
        let word_lo = arm.start;
        let width = arm.end.unwrap_or(arm.start) - arm.start + 1;
        let bad_value = || {
            TMDLError::Codegen(format!(
                "instruction '{}': unsupported encoding value at bits {}..{}",
                inst.name,
                arm.start,
                arm.end.unwrap_or(arm.start)
            ))
        };

        match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => {
                const_word |=
                    (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
            }
            ast::Expr::Ident(id) => match ops_map.get(&id.name) {
                Some(Type::Struct(_)) => push_field(
                    &mut reg_fields,
                    &id.name,
                    IntField {
                        op_lo: 0,
                        word_lo,
                        width,
                    },
                ),
                Some(Type::Integer | Type::Bits(_)) => push_field(
                    &mut int_fields,
                    &id.name,
                    IntField {
                        op_lo: 0,
                        word_lo,
                        width,
                    },
                ),
                Some(_) => return Err(bad_value()),
                None => match resolved_params.get(&id.name) {
                    Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
                        const_word |=
                            (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
                    }
                    _ => {
                        return Err(TMDLError::Codegen(format!(
                            "instruction '{}': encoding parameter '{}' has no literal value",
                            inst.name, id.name
                        )));
                    }
                },
            },
            ast::Expr::Slice(slc) => {
                let ast::Expr::Ident(id) = &*slc.base else {
                    return Err(bad_value());
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return Err(bad_value()),
                };
                push_field(
                    dst,
                    &id.name,
                    IntField {
                        op_lo: slc.start,
                        word_lo,
                        width,
                    },
                );
            }
            ast::Expr::IndexAccess(idx) => {
                let ast::Expr::Ident(id) = &*idx.base else {
                    return Err(bad_value());
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return Err(bad_value()),
                };
                push_field(
                    dst,
                    &id.name,
                    IntField {
                        op_lo: idx.index,
                        word_lo,
                        width: 1,
                    },
                );
            }
            _ => return Err(bad_value()),
        }
    }

    let scatter = |fields: &[IntField]| -> Vec<proc_macro2::TokenStream> {
        fields
            .iter()
            .map(|f| {
                let mask = proc_macro2::Literal::u128_suffixed(encoding_mask(f.width));
                let bits = if f.op_lo > 0 {
                    let op_lo = proc_macro2::Literal::u32_suffixed(f.op_lo as u32);
                    quote! { (value >> #op_lo) & #mask }
                } else {
                    quote! { value & #mask }
                };
                if f.word_lo > 0 {
                    let word_lo = proc_macro2::Literal::u32_suffixed(f.word_lo as u32);
                    quote! { word |= (#bits) << #word_lo; }
                } else {
                    quote! { word |= #bits; }
                }
            })
            .collect()
    };

    let mut steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, fields) in &reg_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let ors = scatter(fields);
        steps.push(quote! {
            {
                let attr = op.attributes.iter().find(|a| a.name == #name_lit)?;
                let value = match &attr.value {
                    tir::attributes::AttributeValue::Register(
                        tir::attributes::RegisterAttr::Physical { index, .. },
                    ) => *index as u128,
                    _ => return None,
                };
                #(#ors)*
            }
        });
    }

    for (name, fields) in &int_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let ors = scatter(fields);
        // Immediates written in assembly may be spelled signed or unsigned
        // (`-1` vs `0xFFF`), so accept either fit within the declared width.
        let (int_check, uint_check) = match ops_map.get(name.as_str()) {
            // Any attribute value fits a full-width field, and the shifts
            // below would overflow at 64 bits.
            Some(Type::Bits(n)) if *n >= 64 => (quote! {}, quote! {}),
            Some(Type::Bits(n)) => {
                let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
                let max = proc_macro2::Literal::i64_suffixed(1i64 << n);
                let umax = proc_macro2::Literal::u64_suffixed(1u64 << n);
                (
                    quote! { if !(#min..#max).contains(&v) { return None; } },
                    quote! { if v >= #umax { return None; } },
                )
            }
            _ => (quote! {}, quote! {}),
        };
        steps.push(quote! {
            {
                let attr = op.attributes.iter().find(|a| a.name == #name_lit)?;
                match &attr.value {
                    tir::attributes::AttributeValue::Int(v) => {
                        let v = *v;
                        #int_check
                        let value = v as u128;
                        #(#ors)*
                    }
                    tir::attributes::AttributeValue::UInt(v) => {
                        let v = *v;
                        #uint_check
                        let value = v as u128;
                        #(#ors)*
                    }
                    tir::attributes::AttributeValue::Str(s) => {
                        fixups.push(tir::backend::binary::InstFixup {
                            operand: #name_lit,
                            target: tir::backend::binary::FixupTarget::Symbol(s.clone()),
                        });
                    }
                    tir::attributes::AttributeValue::Block(b) => {
                        fixups.push(tir::backend::binary::InstFixup {
                            operand: #name_lit,
                            target: tir::backend::binary::FixupTarget::Block(*b),
                        });
                    }
                    _ => return None,
                }
            }
        });
    }

    let encode_fn_ident = format_ident!("encode_{}_inst", inst.name.to_lowercase());
    let const_word_lit = proc_macro2::Literal::u128_suffixed(const_word);
    let wb_lit = proc_macro2::Literal::usize_unsuffixed(width_bytes as usize);
    let word_decl = if reg_fields.is_empty() && int_fields.is_empty() {
        quote! { let word: u128 = #const_word_lit; }
    } else {
        quote! { let mut word: u128 = #const_word_lit; }
    };
    let fixups_decl = if int_fields.is_empty() {
        quote! { let fixups: Vec<tir::backend::binary::InstFixup> = Vec::new(); }
    } else {
        quote! { let mut fixups: Vec<tir::backend::binary::InstFixup> = Vec::new(); }
    };
    // Operand-less instructions (e.g. ecall) encode to a constant word and never
    // consult the op's attributes.
    let op_param = if reg_fields.is_empty() && int_fields.is_empty() {
        quote! { _op }
    } else {
        quote! { op }
    };
    let encoder = quote! {
        fn #encode_fn_ident(
            #op_param: &tir::OpInstance,
        ) -> Option<tir::backend::binary::EncodedInst> {
            #word_decl
            #fixups_decl
            #(#steps)*
            Some(tir::backend::binary::EncodedInst {
                bytes: word.to_le_bytes()[..#wb_lit].to_vec(),
                fixups,
            })
        }
    };

    // A patcher is only meaningful when the encoding has exactly one immediate
    // operand of known width: the value scattered into it is a resolved fixup
    // (e.g. a pc-relative branch delta), which must fit as a signed quantity.
    let patcher = if let [(name, fields)] = &int_fields[..]
        && let Some(Type::Bits(n)) = ops_map.get(name.as_str())
    {
        let patch_fn_ident = format_ident!("patch_{}_inst", inst.name.to_lowercase());
        // A full-width field admits any i64 (and the shifts would overflow).
        let range_check = if *n < 64 {
            let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
            let max = proc_macro2::Literal::i64_suffixed(1i64 << (n - 1));
            quote! {
                if !(#min..#max).contains(&value) {
                    return None;
                }
            }
        } else {
            quote! {}
        };
        let lowest_bit = fields.iter().map(|f| f.op_lo).min().unwrap_or(0);
        // Operand bits below the lowest encoded bit are silently dropped by the
        // scatter (e.g. bit 0 of RISC-V branch offsets); a value with any of
        // them set cannot be represented.
        let dropped_check = if lowest_bit > 0 {
            let dropped_mask = proc_macro2::Literal::u128_suffixed(encoding_mask(lowest_bit));
            quote! { if (value as u128) & #dropped_mask != 0 { return None; } }
        } else {
            quote! {}
        };
        let ors = scatter(fields);
        Some(quote! {
            fn #patch_fn_ident(bytes: &mut [u8], value: i64) -> Option<()> {
                #range_check
                #dropped_check
                if bytes.len() < #wb_lit {
                    return None;
                }
                let mut word: u128 = 0;
                for (i, b) in bytes.iter().enumerate().take(#wb_lit) {
                    word |= (*b as u128) << (8 * i);
                }
                let value = value as u128;
                #(#ors)*
                let out = word.to_le_bytes();
                bytes[..#wb_lit].copy_from_slice(&out[..#wb_lit]);
                Some(())
            }
        })
    } else {
        None
    };

    Ok(Some((encoder, patcher)))
}

/// Compile an instruction's encoding arms into a `decode_*_inst` function — the
/// inverse of [`emit_instruction_encoder`]. Given a 32-bit little-endian
/// instruction word it matches the fixed opcode bits, reconstructs each operand
/// from its (possibly split) bit-fields, builds the corresponding op in the
/// `Context`, and returns its id.
///
/// Best-effort: returns `None` (no decoder emitted) for instructions without an
/// encoding, not exactly 32 bits wide, or using an encoding form this generator
/// cannot invert — so enabling decoding never breaks a backend's build.
fn emit_instruction_decoder(
    inst: &ast::Instruction,
    encoding_arms: &[ast::EncodingArm],
    ops_map: &HashMap<String, Type>,
    resolved_params: &HashMap<String, (Type, Option<ast::Expr>)>,
    width_bytes: u64,
) -> Option<(proc_macro2::TokenStream, proc_macro2::Ident, u128)> {
    if encoding_arms.is_empty() || width_bytes != 4 {
        return None;
    }

    let mut const_word: u128 = 0;
    let mut fixed_mask: u128 = 0;
    let mut reg_fields: Vec<(String, Vec<IntField>)> = Vec::new();
    let mut int_fields: Vec<(String, Vec<IntField>)> = Vec::new();

    let push_field = |dst: &mut Vec<(String, Vec<IntField>)>, name: &str, field: IntField| match dst
        .iter_mut()
        .find(|(n, _)| n == name)
    {
        Some((_, fields)) => fields.push(field),
        None => dst.push((name.to_string(), vec![field])),
    };

    for arm in encoding_arms {
        let word_lo = arm.start;
        let width = arm.end.unwrap_or(arm.start) - arm.start + 1;
        match &arm.value {
            ast::Expr::Lit(ast::Lit::Int(li)) => {
                const_word |=
                    (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
                fixed_mask |= encoding_mask(width) << word_lo;
            }
            ast::Expr::Ident(id) => match ops_map.get(&id.name) {
                Some(Type::Struct(_)) => push_field(
                    &mut reg_fields,
                    &id.name,
                    IntField {
                        op_lo: 0,
                        word_lo,
                        width,
                    },
                ),
                Some(Type::Integer | Type::Bits(_)) => push_field(
                    &mut int_fields,
                    &id.name,
                    IntField {
                        op_lo: 0,
                        word_lo,
                        width,
                    },
                ),
                Some(_) => return None,
                None => match resolved_params.get(&id.name) {
                    Some((_, Some(ast::Expr::Lit(ast::Lit::Int(li))))) => {
                        const_word |=
                            (u128::from(parse_literal_value(li)) & encoding_mask(width)) << word_lo;
                        fixed_mask |= encoding_mask(width) << word_lo;
                    }
                    _ => return None,
                },
            },
            ast::Expr::Slice(slc) => {
                let ast::Expr::Ident(id) = &*slc.base else {
                    return None;
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return None,
                };
                push_field(
                    dst,
                    &id.name,
                    IntField {
                        op_lo: slc.start,
                        word_lo,
                        width,
                    },
                );
            }
            ast::Expr::IndexAccess(idx) => {
                let ast::Expr::Ident(id) = &*idx.base else {
                    return None;
                };
                let dst = match ops_map.get(&id.name) {
                    Some(Type::Struct(_)) => &mut reg_fields,
                    Some(Type::Integer | Type::Bits(_)) => &mut int_fields,
                    _ => return None,
                };
                push_field(
                    dst,
                    &id.name,
                    IntField {
                        op_lo: idx.index,
                        word_lo,
                        width: 1,
                    },
                );
            }
            _ => return None,
        }
    }

    // Reassemble one operand from its pieces: for each (word_lo, op_lo, width)
    // run, place word bits `[word_lo, word_lo+width)` at operand bits `op_lo`.
    let gather = |fields: &[IntField]| -> proc_macro2::TokenStream {
        let pieces: Vec<proc_macro2::TokenStream> = fields
            .iter()
            .map(|f| {
                let mask = proc_macro2::Literal::u64_suffixed(encoding_mask(f.width) as u64);
                let extract = if f.word_lo > 0 {
                    let word_lo = proc_macro2::Literal::u32_suffixed(f.word_lo as u32);
                    quote! { (word >> #word_lo) as u64 & #mask }
                } else {
                    quote! { word as u64 & #mask }
                };
                if f.op_lo > 0 {
                    let op_lo = proc_macro2::Literal::u32_suffixed(f.op_lo as u32);
                    quote! { value |= (#extract) << #op_lo; }
                } else {
                    quote! { value |= #extract; }
                }
            })
            .collect();
        quote! {{ let mut value: u64 = 0; #(#pieces)* value }}
    };

    let mut attr_steps: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, fields) in &reg_fields {
        let class = match ops_map.get(name) {
            Some(Type::Struct(c)) => c,
            _ => return None,
        };
        let name_lit = proc_macro2::Literal::string(name);
        let class_id = reg_class_id(class);
        let g = gather(fields);
        attr_steps.push(quote! {
            .attr(
                #name_lit,
                tir::attributes::AttributeValue::Register(
                    tir::attributes::RegisterAttr::Physical {
                        class: #class_id,
                        index: (#g) as u16,
                    },
                ),
            )
        });
    }
    for (name, fields) in &int_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let g = gather(fields);
        attr_steps.push(quote! {
            .attr(#name_lit, tir::attributes::AttributeValue::Int((#g) as i64))
        });
    }

    let decode_fn_ident = format_ident!("decode_{}_inst", inst.name.to_lowercase());
    let builder_ident = format_ident!("{}OpBuilder", &inst.name);
    let const_word_lit = proc_macro2::Literal::u32_suffixed(const_word as u32);
    // An operand-less instruction fixes every bit, making the mask an identity.
    let guard = if fixed_mask as u32 == u32::MAX {
        quote! { if word != #const_word_lit { return None; } }
    } else {
        let fixed_mask_lit = proc_macro2::Literal::u32_suffixed(fixed_mask as u32);
        quote! { if word & #fixed_mask_lit != #const_word_lit { return None; } }
    };

    let decoder = quote! {
        fn #decode_fn_ident(context: &tir::Context, word: u32) -> Option<tir::OpId> {
            #guard
            let op = #builder_ident::new(context)
                #(#attr_steps)*
                .build();
            Some(op.id())
        }
    };

    Some((decoder, decode_fn_ident, fixed_mask))
}
