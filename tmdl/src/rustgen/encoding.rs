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

fn emit_field_runs(fields: &[IntField]) -> proc_macro2::TokenStream {
    let runs = fields.iter().map(|f| {
        let op_lo = proc_macro2::Literal::u16_unsuffixed(f.op_lo);
        let word_lo = proc_macro2::Literal::u16_unsuffixed(f.word_lo);
        let width = proc_macro2::Literal::u16_unsuffixed(f.width);
        quote! {
            tir::backend::binary::FieldRun {
                op_lo: #op_lo,
                word_lo: #word_lo,
                width: #width,
            }
        }
    });
    quote! { &[#(#runs),*] }
}

/// Compile an instruction's encoding arms into an `encode_*_inst` function
/// (and, when the encoding has exactly one immediate operand of known width,
/// a `patch_*_inst` function that re-scatters a resolved fixup value). Both
/// are thin shims over the spec interpreters in `tir::backend::binary`; the
/// instruction-specific part is a static data table.
/// Returns `None` when the instruction has no encoding.
fn emit_instruction_encoder(
    inst: &ast::Instruction,
    encoding_arms: &[ast::EncodingArm],
    ops_map: &HashMap<String, Type>,
    constraints: &HashMap<String, OperandConstraint>,
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
                        op_lo: slc.lo,
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

    // Register operand: encode the allocated physical index; reject anything
    // that did not get one. Immediate operand: fit-check when the field is
    // narrower than 64 bits (immediates written in assembly may be spelled
    // signed or unsigned), then scatter. Symbol names and branch-target blocks
    // are not representable at encode time: their bits stay zero and they are
    // recorded as fixups instead.
    let mut spec_fields: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, fields) in &reg_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let runs = emit_field_runs(fields);
        spec_fields.push(quote! {
            tir::backend::binary::EncodeField {
                attr: #name_lit,
                int_range: None,
                align_mask: 0u128,
                nonzero: false,
                runs: #runs,
                register: true,
            }
        });
    }
    for (name, fields) in &int_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let runs = emit_field_runs(fields);
        let int_range = match ops_map.get(name.as_str()) {
            // Any attribute value fits a full-width field, and the range
            // literals would overflow at 64 bits.
            Some(Type::Bits(n)) if *n < 64 => {
                let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
                let max = proc_macro2::Literal::i64_suffixed(1i64 << n);
                let umax = proc_macro2::Literal::u64_suffixed(1u64 << n);
                quote! { Some((#min, #max, #umax)) }
            }
            _ => quote! { None },
        };
        let constraint = constraints.get(name.as_str()).copied().unwrap_or_default();
        let align_mask = proc_macro2::Literal::u128_suffixed(u128::from(constraint.align - 1));
        let nonzero = constraint.nonzero;
        spec_fields.push(quote! {
            tir::backend::binary::EncodeField {
                attr: #name_lit,
                int_range: #int_range,
                align_mask: #align_mask,
                nonzero: #nonzero,
                runs: #runs,
                register: false,
            }
        });
    }

    let encode_spec_ident = format_ident!("ENCODE_{}", inst.name.to_uppercase());
    let const_word_lit = proc_macro2::Literal::u128_suffixed(const_word);
    let wb_lit = proc_macro2::Literal::usize_unsuffixed(width_bytes as usize);
    let encoder = quote! {
        static #encode_spec_ident: tir::backend::binary::EncodeSpec =
            tir::backend::binary::EncodeSpec {
                const_word: #const_word_lit,
                width_bytes: #wb_lit,
                fields: &[#(#spec_fields),*],
            };
    };

    // A patcher is only meaningful when the encoding has exactly one immediate
    // operand of known width: the value scattered into it is a resolved fixup
    // (e.g. a pc-relative branch delta), which must fit as a signed quantity.
    let patcher = if let [(name, fields)] = &int_fields[..]
        && let Some(Type::Bits(n)) = ops_map.get(name.as_str())
    {
        let patch_spec_ident = format_ident!("PATCH_{}", inst.name.to_uppercase());
        // A full-width field admits any i64 (and the literals would overflow).
        let range = if *n < 64 {
            let min = proc_macro2::Literal::i64_suffixed(-(1i64 << (n - 1)));
            let max = proc_macro2::Literal::i64_suffixed(1i64 << (n - 1));
            quote! { Some((#min, #max)) }
        } else {
            quote! { None }
        };
        // The operand bits the encoding drops are the ones its `#[align]`
        // declares zero (sema admits an encoding that drops bits only with the
        // matching alignment), so a value with any of them set is not
        // representable.
        let align = constraints.get(name.as_str()).copied().unwrap_or_default().align;
        let dropped_mask = proc_macro2::Literal::u128_suffixed(u128::from(align - 1));
        let runs = emit_field_runs(fields);
        Some(quote! {
            static #patch_spec_ident: tir::backend::binary::PatchSpec =
                tir::backend::binary::PatchSpec {
                    range: #range,
                    dropped_mask: #dropped_mask,
                    width_bytes: #wb_lit,
                    runs: #runs,
                };
        })
    } else {
        None
    };

    Ok(Some((encoder, patcher)))
}

/// Compile an instruction's encoding arms into a `DECODE_*` spec — the inverse
/// of [`emit_instruction_encoder`]. Given a 32-bit little-endian instruction
/// word the shared interpreter matches the fixed opcode bits, reconstructs
/// each operand from its (possibly split) bit-fields, builds the corresponding
/// op in the `Context`, and returns its id.
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
    dialect: &str,
    op_name: &str,
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
                        op_lo: slc.lo,
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

    let mut spec_fields: Vec<proc_macro2::TokenStream> = Vec::new();
    for (name, fields) in &reg_fields {
        let class = match ops_map.get(name) {
            Some(Type::Struct(c)) => c,
            _ => return None,
        };
        let name_lit = proc_macro2::Literal::string(name);
        let class_ident = format_ident!("{}", class);
        let runs = emit_field_runs(fields);
        spec_fields.push(quote! {
            tir::backend::binary::DecodeField {
                attr: #name_lit,
                kind: tir::backend::binary::DecodeFieldKind::Register(RegClass::#class_ident.id()),
                runs: #runs,
            }
        });
    }
    for (name, fields) in &int_fields {
        let name_lit = proc_macro2::Literal::string(name);
        let runs = emit_field_runs(fields);
        spec_fields.push(quote! {
            tir::backend::binary::DecodeField {
                attr: #name_lit,
                kind: tir::backend::binary::DecodeFieldKind::Int,
                runs: #runs,
            }
        });
    }

    let spec_ident = format_ident!("DECODE_{}", inst.name.to_uppercase());
    let dialect_lit = proc_macro2::Literal::string(dialect);
    let op_name_lit = proc_macro2::Literal::string(op_name);
    // Sorted so generated code is stable across runs; the interpreter only
    // needs the set.
    let mut attr_names: Vec<&String> = ops_map.keys().collect();
    attr_names.sort();
    let attr_lits: Vec<proc_macro2::Literal> = attr_names
        .iter()
        .map(|n| proc_macro2::Literal::string(n))
        .collect();
    let const_word_lit = proc_macro2::Literal::u32_suffixed(const_word as u32);
    let fixed_mask_lit = proc_macro2::Literal::u32_suffixed(fixed_mask as u32);

    let spec = quote! {
        static #spec_ident: tir::backend::binary::DecodeSpec =
            tir::backend::binary::DecodeSpec {
                op: (#dialect_lit, #op_name_lit),
                fixed_mask: #fixed_mask_lit,
                const_word: #const_word_lit,
                fields: &[#(#spec_fields),*],
                attrs: &[#(#attr_lits),*],
            };
    };

    Some((spec, spec_ident, fixed_mask))
}
